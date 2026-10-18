//! Port of `reader/osm/OsmReader.java`: constructs [`SourceFeature`]s from the raw nodes, ways, and relations
//! contained in an OSM block source (usually an [`OsmInputFile`](crate::reader::osm::osm_input_file::OsmInputFile)).
//!
//! # Usage
//!
//! ```ignore
//! let reader = OsmReader::new("osm", osm_input_file.get()?, node_location_db, multipolygon_geometries, profile, stats);
//! reader.pass1(&config)?;            // node locations, profile preprocessing, relation info, way splitting
//! reader.pass2(&feature_group, &config)?; // emit features into the feature group
//! reader.close();                    // release node locations / relation info / multipolygon way geometries
//! ```
//!
//! # Design
//!
//! - Pass 1 decodes blocks in parallel. When the node location map supports parallel writes, each worker thread
//!   decodes and processes its own blocks; otherwise worker threads decode blocks (and pre-compute encoded node
//!   locations) and hand them off in order to a single processing thread, like Java.
//! - Relation info, way-to-relation and relation-to-parent memberships, the role table, and the set of ways in
//!   multipolygons are built under a lock in pass 1 and frozen (sorted) on first read in pass 2, so pass 2 lookups
//!   are lock-free.
//! - Pass 2 features ([`OsmSourceFeature`]) own the decoded element (no tag clones) and compute geometries lazily
//!   from the node location map on first access. Each worker thread gets its own [`NodeLocations`] handle and its own
//!   copy of the source name so the per-feature reference counts do not bounce between cores.

use std::any::Any;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use geo_types::{Coord, Geometry, LineString, Point, Polygon};
use rayon::slice::ParallelSliceMut;
use roaring::RoaringTreemap;

use crate::collection::feature_group::{self, FeatureGroup, FeatureSink, RenderedFeatureEncoder};
use crate::collection::long_long_map::{LongLongMap, LongLongMapType, LongLongMapWriter, MISSING_VALUE, SealOnRead};
use crate::collection::long_long_multimap::{self, Replaceable};
use crate::collection::storage::Storage;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::expression::expression::ExpressionInput;
use crate::feature_collector;
use crate::geo::geo_utils;
use crate::geo::geometry_exception::{GeometryException, GeometryResult};
use crate::geo::with_geometry::{GeometryCache, WithGeometry, cached};
use crate::profile::Profile;
use crate::reader::osm::osm_block_source::OsmBlockSource;
pub use crate::reader::osm::osm_element::{Node, OsmElement, OsmElementType, Relation, Way};
pub use crate::reader::osm::osm_multipolygon::NodeLocationProvider;
use crate::reader::osm::osm_multipolygon::{self};
use crate::reader::osm::osm_phaser::{OsmPhaser, Phase};
pub use crate::reader::osm::osm_relation_info::{OsmRelationInfo, RelationMember};
use crate::reader::osm::osm_way_splitter::{self, OsmWaySplitter, PerThreadWriter, RoaringBitmapSplitter};
use crate::reader::source_feature::{SourceFeature, SourceFeatureBase};
use crate::reader::with_geometry_type::WithGeometryType;
use crate::reader::with_tags::WithTags;
use crate::render::feature_renderer::FeatureRenderer;
use crate::stats::counter::Counter;
use crate::stats::progress_loggers::ProgressLoggers;
use crate::stats::stats::StatsRef;
use crate::util::format;
use crate::util::memory_estimator::HasEstimate;
use crate::util::resource_usage::{ResourceUsage, StorageKind};
use crate::value::Tags;
use crate::worker::Done;
use crate::worker::distributor::Distributor;
use crate::worker::work_queue::WorkQueue;
use crate::worker::worker_pipeline::WorkerPipeline;

type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;

const ROLE_BITS: u32 = 16;
const MAX_ROLES: usize = (1 << ROLE_BITS) - 10;
const ROLE_SHIFT: u32 = 64 - ROLE_BITS;
const ROLE_MASK: u64 = (1 << ROLE_BITS) - 1;
const NOT_ROLE_MASK: i64 = (1i64 << ROLE_SHIFT) - 1;

/// Returns `true` if a way can be interpreted as a line.
pub fn can_be_line(closed: bool, area: Option<&str>, points: usize) -> bool {
    (!closed || area != Some("yes")) && points >= 2
}

/// Returns `true` if a way can be interpreted as a polygon.
pub fn can_be_polygon(closed: bool, area: Option<&str>, points: usize) -> bool {
    (closed && area != Some("no")) && points >= 4
}

fn is_multipolygon(relation: &Relation) -> bool {
    relation.has_tag_any("type", &["multipolygon", "boundary", "land_area"])
        && relation.members.iter().any(|m| m.member_type == OsmElementType::Way)
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_string()
    }
}

/// The `(key, value)` pairs for one key of a list sorted by key, iterated in the order Java's
/// `SparseUnorderedBinarySearchMultimap.get` returns them: from the entry `Arrays.binarySearch` lands on forwards,
/// then backwards from the entry before it. This matters because it decides the order of a way's relations (e.g. the
/// `route_N_*` attributes OpenMapTiles assigns).
#[derive(Clone, Copy)]
struct Matches<'a> {
    run: &'a [(i64, i64)],
    split: usize,
}

impl<'a> Matches<'a> {
    fn is_empty(&self) -> bool {
        self.run.is_empty()
    }

    fn len(&self) -> usize {
        self.run.len()
    }

    fn iter(&self) -> impl Iterator<Item = &'a (i64, i64)> + 'a {
        self.run[self.split..].iter().chain(self.run[..self.split].iter().rev())
    }
}

/// Returns the pairs with `key` from a list sorted by key (see [`Matches`]).
#[inline]
fn lookup(pairs: &[(i64, i64)], key: i64) -> Matches<'_> {
    // Java's Arrays.binarySearch
    let (mut low, mut high) = (0isize, pairs.len() as isize - 1);
    let mut mid = None;
    while low <= high {
        let m = ((low + high) as usize) >> 1;
        let v = pairs[m].0;
        if v < key {
            low = m as isize + 1;
        } else if v > key {
            high = m as isize - 1;
        } else {
            mid = Some(m);
            break;
        }
    }
    let Some(mid) = mid else { return Matches { run: &[], split: 0 } };
    let start = pairs[..mid].partition_point(|p| p.0 < key);
    let end = mid + pairs[mid..].partition_point(|p| p.0 == key);
    Matches { run: &pairs[start..end], split: mid - start }
}

/// Relation data collected in pass 1 (Java `relationInfo`, `wayToRelations`, `relationToParentRelations`, `roleIds`).
#[derive(Default)]
struct RelationState {
    relation_info: HashMap<i64, Arc<dyn OsmRelationInfo>>,
    role_ids: HashMap<Arc<str>, usize>,
    /// role strings by role ID (index 0 unused)
    roles: Vec<Arc<str>>,
    /// way ID -> encoded relation membership, sorted by way ID when frozen
    way_to_relations: Vec<(i64, i64)>,
    /// relation ID -> encoded parent relation membership, sorted by relation ID when frozen
    relation_to_parent_relations: Vec<(i64, i64)>,
}

impl RelationState {
    fn freeze(&mut self) {
        // stable sorts keep memberships for each key in insertion order
        self.way_to_relations.par_sort_by_key(|p| p.0);
        self.way_to_relations.shrink_to_fit();
        self.relation_to_parent_relations.par_sort_by_key(|p| p.0);
        self.relation_to_parent_relations.shrink_to_fit();
    }

    /// Packs a string role and relation into a compact long for storage.
    fn encode_relation_membership(&mut self, role: &Arc<str>, relation_id: i64, role_sizes: &AtomicU64) -> i64 {
        let role_id = match self.role_ids.get(role) {
            Some(&id) => id,
            None => {
                role_sizes.fetch_add(24 + role.len() as u64, Ordering::Relaxed);
                if self.roles.is_empty() {
                    self.roles.push(Arc::from(""));
                }
                let id = self.role_ids.len() + 1;
                self.role_ids.insert(role.clone(), id);
                self.roles.push(role.clone());
                if id > MAX_ROLES {
                    panic!("Too many roles to encode: {role}");
                }
                id
            }
        };
        relation_id | ((role_id as i64) << ROLE_SHIFT)
    }

    /// Returns the role and relation ID packed into a long.
    #[inline]
    fn decode_relation_membership(&self, encoded: i64) -> (&Arc<str>, i64) {
        let role = (((encoded as u64) >> ROLE_SHIFT) & ROLE_MASK) as usize;
        (&self.roles[role], encoded & NOT_ROLE_MASK)
    }

    fn parent_relations(&self, relation_id: i64) -> Matches<'_> {
        lookup(&self.relation_to_parent_relations, relation_id)
    }

    fn get_relation_memberships(&self, memberships: Matches<'_>) -> Vec<RelationMember<dyn OsmRelationInfo>> {
        let mut rels = Vec::new();
        if memberships.is_empty() {
            return rels;
        }
        rels.reserve(memberships.len());
        for &(_, encoded) in memberships.iter() {
            // encoded ID uses the upper few bits of the long to encode the role
            let (role, relation_id) = self.decode_relation_membership(encoded);
            if let Some(rel) = self.relation_info.get(&relation_id) {
                rels.push(RelationMember::new(role.clone(), rel.clone()));
            }
            let parents = self.parent_relations(relation_id);
            if parents.is_empty() {
                continue;
            }
            let mut visited = HashSet::new();
            visited.insert(relation_id);
            for &(_, parent) in parents.iter() {
                self.get_relation_infos_for_relation_id(parent, &mut visited, &[relation_id], &mut rels);
            }
        }
        rels
    }

    fn get_relation_infos_for_relation_id(
        &self,
        relation_id_and_role: i64,
        visited: &mut HashSet<i64>,
        parent_relation_path: &[i64],
        out: &mut Vec<RelationMember<dyn OsmRelationInfo>>,
    ) {
        let (role, relation_id) = self.decode_relation_membership(relation_id_and_role);
        if !visited.insert(relation_id) {
            return;
        }
        if let Some(relation) = self.relation_info.get(&relation_id) {
            out.push(RelationMember::with_path(role.clone(), relation.clone(), parent_relation_path.to_vec()));
        }
        let parents = self.parent_relations(relation_id);
        if !parents.is_empty() {
            let mut path = parent_relation_path.to_vec();
            path.push(relation_id);
            for &(_, parent) in parents.iter() {
                self.get_relation_infos_for_relation_id(parent, visited, &path, out);
            }
        }
    }

    fn estimate_memory_usage_bytes(&self) -> u64 {
        (self.way_to_relations.capacity() * 16
            + self.relation_to_parent_relations.capacity() * 16
            + self.relation_info.capacity() * 32
            + self.role_ids.capacity() * 32
            + self.roles.capacity() * 16) as u64
    }
}

/// Shared state of an [`OsmReader`].
struct Inner {
    name: Arc<str>,
    block_source: Arc<dyn OsmBlockSource>,
    stats: StatsRef,
    node_location_db: Arc<dyn LongLongMap>,
    profile: Arc<dyn Profile>,
    pass1_blocks: Counter,
    relation_info_sizes: AtomicU64,
    role_sizes: AtomicU64,
    relations: SealOnRead<RelationState>,
    // for multipolygons need to store way info (20m ways, 800m nodes) to use when processing relations (4.5m)
    ways_in_multipolygon: SealOnRead<RoaringTreemap>,
    multipolygon_way_geometries: Box<dyn Replaceable>,
    pass1_phaser: OsmPhaser,
    way_splitter: RoaringBitmapSplitter,
    max_way_id: AtomicI64,
}

impl HasEstimate for Inner {
    fn estimate_memory_usage_bytes(&self) -> u64 {
        let mut size = 0;
        size += self.ways_in_multipolygon.peek(|w| w.serialized_size() as u64).unwrap_or(0);
        // multipolygon way geometries are reported separately
        size += self.relations.peek(RelationState::estimate_memory_usage_bytes).unwrap_or(0);
        size += self.role_sizes.load(Ordering::Relaxed);
        size += self.relation_info_sizes.load(Ordering::Relaxed);
        size += self.way_splitter.estimate_memory_usage_bytes();
        size
    }
}

/// A precomputed node location (`Some`) when blocks are decoded on a different thread than they are processed.
type PrecomputedLocation = Option<u64>;

/// Per-thread state for pass 1 (Java `processPass1Blocks` locals).
struct Pass1Worker<'a> {
    inner: &'a Inner,
    node_writer: Option<Box<dyn LongLongMapWriter + 'a>>,
    way_split_writer: Box<dyn PerThreadWriter + 'a>,
    phases: crate::reader::osm::osm_phaser::ForWorker<'a>,
    max_way_id: i64,
}

impl<'a> Pass1Worker<'a> {
    fn new(inner: &'a Inner) -> Pass1Worker<'a> {
        Pass1Worker {
            inner,
            node_writer: Some(inner.node_location_db.new_writer()),
            way_split_writer: inner.way_splitter.writer_for_thread(),
            phases: inner.pass1_phaser.for_worker(),
            max_way_id: i64::MIN,
        }
    }

    #[inline]
    fn process(&mut self, element: OsmElement, location: PrecomputedLocation) -> anyhow::Result<()> {
        if element.id() < 0 {
            anyhow::bail!("Negative OSM element IDs not supported: {element:?}");
        }
        let profile = &*self.inner.profile;
        match element {
            OsmElement::Node(node) => {
                self.phases.arrive_and_wait_for_others(Phase::Nodes)?;
                if let Err(e) = catch_unwind(AssertUnwindSafe(|| profile.preprocess_osm_node(&node))) {
                    tracing::error!("Error preprocessing OSM node {}: {}", node.id, panic_message(&*e));
                }
                // TODO allow limiting node storage to only ones that profile cares about
                let location = location.unwrap_or_else(|| node.encoded_location());
                match self.node_writer.as_mut() {
                    Some(writer) => writer.put(node.id, location as i64)?,
                    None => anyhow::bail!("Node {} encountered after ways", node.id),
                }
            }
            OsmElement::Way(way) => {
                self.finish_nodes();
                self.phases.arrive_and_wait_for_others(Phase::Ways)?;
                let split_writer = &mut self.way_split_writer;
                let result = catch_unwind(AssertUnwindSafe(|| {
                    profile.preprocess_osm_way(&way);
                    if profile.split_osm_way_at_intersections(&way) {
                        split_writer.add_way(&way.nodes);
                    }
                }));
                if let Err(e) = result {
                    tracing::error!("Error preprocessing OSM way {}: {}", way.id, panic_message(&*e));
                }
                self.max_way_id = self.max_way_id.max(way.id);
            }
            OsmElement::Relation(relation) => {
                self.finish_nodes();
                self.phases.arrive(Phase::Relations)?;
                let inner = self.inner;
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let infos = profile.preprocess_osm_relation(&relation);
                    if let Some(infos) = infos.filter(|i| !i.is_empty()) {
                        let mut guard = inner.relations.lock_blocking();
                        let Some(state) = guard.as_mut() else {
                            panic!("Cannot add relation info after pass 2 has started");
                        };
                        for info in infos {
                            inner.relation_info_sizes.fetch_add(info.estimate_memory_usage_bytes(), Ordering::Relaxed);
                            state.relation_info.insert(relation.id, info);
                        }
                        for member in &relation.members {
                            // TODO handle nodes in relations
                            match member.member_type {
                                OsmElementType::Way => {
                                    let encoded =
                                        state.encode_relation_membership(&member.role, relation.id, &inner.role_sizes);
                                    state.way_to_relations.push((member.ref_id, encoded));
                                }
                                OsmElementType::Relation => {
                                    let encoded =
                                        state.encode_relation_membership(&member.role, relation.id, &inner.role_sizes);
                                    state.relation_to_parent_relations.push((member.ref_id, encoded));
                                }
                                _ => {}
                            }
                        }
                    }
                }));
                if let Err(e) = result {
                    tracing::error!("Error preprocessing OSM relation {}: {}", relation.id, panic_message(&*e));
                }
                // TODO allow limiting multipolygon storage to only ones that profile cares about
                if is_multipolygon(&relation) {
                    let mut guard = inner.ways_in_multipolygon.lock_blocking();
                    let Some(ways) = guard.as_mut() else {
                        anyhow::bail!("Cannot add multipolygon ways after pass 2 has started");
                    };
                    for member in &relation.members {
                        if member.member_type == OsmElementType::Way {
                            ways.insert(member.ref_id as u64);
                        }
                    }
                }
            }
            OsmElement::Other(_) => {}
        }
        Ok(())
    }

    /// Stores locations of nodes that were already preprocessed on a parse thread.
    fn process_preprocessed_nodes(&mut self, nodes: &[(i64, u64)]) -> anyhow::Result<()> {
        self.phases.arrive_and_wait_for_others_many(Phase::Nodes, nodes.len())?;
        if !nodes.is_empty() {
            let Some(writer) = self.node_writer.as_mut() else {
                anyhow::bail!("Node {} encountered after ways", nodes[0].0);
            };
            for &(id, location) in nodes {
                writer.put(id, location as i64)?;
            }
        }
        Ok(())
    }

    /// Flushes node locations once this worker is done with nodes (Java `whenWorkerFinishes(NODES, nodeWriter::close)`).
    #[inline]
    fn finish_nodes(&mut self) {
        if self.node_writer.is_some() {
            self.node_writer = None;
        }
    }

    fn finish(mut self) -> anyhow::Result<()> {
        self.finish_nodes();
        // closing can block waiting for other threads to catch up, so only close after finishing successfully
        self.phases.close()?;
        self.inner.max_way_id.fetch_max(self.max_way_id, Ordering::Relaxed);
        Ok(())
    }
}

/// A block decoded on a parse thread for the single-threaded pass 1 writer.
///
/// Nodes at the start of the block were already preprocessed by the profile on the parse thread and reduced to
/// `(id, encoded location)` pairs; the remaining elements (normally a whole block of ways or relations) are handed
/// to the processing thread as-is.
struct ParsedBlock {
    nodes: Vec<(i64, u64)>,
    others: Vec<OsmElement>,
}

/// Collects rendered features for one worker thread and writes them to the feature sink.
struct Emitter<'a> {
    encoder: RenderedFeatureEncoder,
    sink: &'a mut dyn FeatureSink,
    error: Option<anyhow::Error>,
}

impl Emitter<'_> {
    #[inline]
    fn write(&mut self, rendered: &crate::render::rendered_feature::RenderedFeature) {
        if self.error.is_none()
            && let Err(e) = self.sink.write(&mut self.encoder, rendered)
        {
            self.error = Some(e);
        }
    }

    fn check(&mut self) -> anyhow::Result<()> {
        match self.error.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// Utility that constructs [`SourceFeature`]s from the raw nodes, ways, and relations of an OSM block source.
///
/// See the [module docs](self) for the lifecycle.
pub struct OsmReader {
    inner: Arc<Inner>,
}

impl OsmReader {
    /// Constructs a new `OsmReader` from `block_source` that will use `node_location_db` as a temporary store for
    /// node locations and `multipolygon_geometries` to store multipolygon way geometries between passes.
    ///
    /// - `name`: ID for this reader to use in stats and logs (and the source name of every feature)
    /// - `profile`: logic that defines what map features to emit for each source feature
    pub fn new(
        name: &str,
        block_source: impl OsmBlockSource + 'static,
        node_location_db: Arc<dyn LongLongMap>,
        multipolygon_geometries: Box<dyn Replaceable>,
        profile: Arc<dyn Profile>,
        stats: StatsRef,
    ) -> OsmReader {
        let inner = Arc::new(Inner {
            name: Arc::from(name),
            block_source: Arc::new(block_source),
            stats: stats.clone(),
            node_location_db,
            profile,
            pass1_blocks: Counter::new_single_thread_counter(),
            relation_info_sizes: AtomicU64::new(0),
            role_sizes: AtomicU64::new(0),
            relations: SealOnRead::new(RelationState::default()),
            ways_in_multipolygon: SealOnRead::new(RoaringTreemap::new()),
            multipolygon_way_geometries: multipolygon_geometries,
            pass1_phaser: OsmPhaser::new(0),
            way_splitter: osm_way_splitter::roaring_bitmap_splitter(),
            max_way_id: AtomicI64::new(i64::MIN),
        });
        // hold weak references so stats don't keep the reader's memory alive after close()
        let weak = Arc::downgrade(&inner);
        stats.monitor_in_memory_object(
            "osm_relations",
            Arc::new(move || weak.upgrade().map_or(0, |i| i.estimate_memory_usage_bytes())),
        );
        let weak: Weak<Inner> = Arc::downgrade(&inner);
        stats.labeled_counter(
            "osm_pass1_elements_processed",
            "type",
            Arc::new(move || {
                let mut result = std::collections::BTreeMap::new();
                if let Some(i) = weak.upgrade() {
                    result.insert("blocks".to_string(), i.pass1_blocks.get());
                    result.insert("nodes".to_string(), i.pass1_phaser.nodes());
                    result.insert("ways".to_string(), i.pass1_phaser.ways());
                    result.insert("relations".to_string(), i.pass1_phaser.relations());
                }
                result
            }),
        );
        OsmReader { inner }
    }

    /// Alias for [`OsmReader::new`] that stores multipolygon way geometries in a default in-memory multimap.
    pub fn new_in_memory_multipolygons(
        name: &str,
        block_source: impl OsmBlockSource + 'static,
        node_location_db: Arc<dyn LongLongMap>,
        profile: Arc<dyn Profile>,
        stats: StatsRef,
    ) -> OsmReader {
        let multipolygons = Box::new(long_long_multimap::new_in_memory_replaceable_multimap());
        OsmReader::new(name, block_source, node_location_db, multipolygons, profile, stats)
    }

    /// The name of this reader / source.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Pre-processes all OSM elements before [`OsmReader::pass2`] is used to emit map features.
    ///
    /// Stores node locations for pass 2 to use to reconstruct way geometries, and the result of
    /// [`Profile::preprocess_osm_relation`] so that pass 2 can know the relevant relations that a way belongs to.
    pub fn pass1(&self, config: &PlanetilerConfig) -> anyhow::Result<()> {
        let inner = &self.inner;
        let stats = &inner.stats;
        let timer = stats.start_stage("osm_pass1");
        let threads = config.threads;

        let counter = |f: fn(&Inner) -> i64| {
            let inner = inner.clone();
            move || f(&inner)
        };
        let db = inner.node_location_db.clone();
        let mut loggers = ProgressLoggers::create()
            .add_rate_counter_colored("nodes", counter(|i| i.pass1_phaser.nodes()), true)
            .add(move || {
                let bytes = db.disk_usage_bytes() + db.estimate_memory_usage_bytes();
                format!(" {}", format::pad_right(&format::storage(bytes, false), 5))
            })
            .add_rate_counter_colored("ways", counter(|i| i.pass1_phaser.ways()), true)
            .add_rate_counter_colored("rels", counter(|i| i.pass1_phaser.relations()), true)
            .add_rate_counter("blocks", counter(|i| i.pass1_blocks.get()))
            .new_line()
            .add_process_stats()
            .add_in_memory_object("hppc", inner.clone())
            .new_line();

        let done: Done = if inner.node_location_db.supports_parallel_writes() {
            // If the node location writer supports parallel writes, then parse, process, and write node locations
            // from worker threads
            let parse_threads = 1.max(if threads < 8 { threads } else { threads - 1 });
            inner.pass1_phaser.register_workers(parse_threads);
            let source = inner.block_source.clone();
            let worker_inner = inner.clone();
            let pipeline = WorkerPipeline::start("osm_pass1", stats.clone())
                .from_generator("read", move |next| {
                    source.for_each_block(&mut |block| {
                        next.accept(block);
                        Ok(())
                    })
                })
                .add_buffer("pbf_blocks", parse_threads * 2)
                .sink_to("process", parse_threads, move |blocks| {
                    worker_inner.process_pass1_blocks(blocks.map(|block| block.into_elements()))
                });
            loggers = loggers.add_pipeline_stats(&pipeline);
            pipeline.done()
        } else {
            // If the node location writer requires sequential writes, then the reader hands off the block to workers
            // and a handle that the result will go on to the single-threaded writer, and the writer emits new nodes
            // when they are ready
            let parse_threads = 1.max(if threads < 8 { threads } else { threads - 2 });
            let pending_blocks = parse_threads * 2;
            type Pending = crossbeam_channel::Receiver<anyhow::Result<ParsedBlock>>;
            type ToParse =
                (crate::reader::osm::osm_block_source::Block, crossbeam_channel::Sender<anyhow::Result<ParsedBlock>>);
            inner.pass1_phaser.register_workers(1);
            let parsed_batches: WorkQueue<Pending> = WorkQueue::new("elements", pending_blocks, 1, stats);
            let source = inner.block_source.clone();
            let parse_inner = inner.clone();
            let batches = parsed_batches.clone();
            let read_branch = WorkerPipeline::start("osm_pass1", stats.clone())
                .from_generator("read", move |next: &mut crate::worker::work_queue::WorkQueueWriter<ToParse>| {
                    // close the queue of parsed batches even if reading fails so the process thread finishes
                    struct CloseOnDrop<'q>(&'q WorkQueue<Pending>);
                    impl Drop for CloseOnDrop<'_> {
                        fn drop(&mut self) {
                            self.0.close();
                        }
                    }
                    let mut enqueuer = batches.thread_local_writer();
                    let _close = CloseOnDrop(&batches);
                    source.for_each_block(&mut |block| {
                        let (tx, rx) = crossbeam_channel::bounded(1);
                        enqueuer.accept(rx);
                        next.accept((block, tx));
                        Ok(())
                    })?;
                    enqueuer.flush();
                    Ok(())
                })
                .add_buffer("pbf_blocks", pending_blocks)
                .sink_to("parse", parse_threads, move |blocks| {
                    for (block, result) in blocks {
                        let parsed = block.into_elements().and_then(|elements| parse_inner.parse_pass1_block(elements));
                        // the receiver is gone if processing failed
                        let _ = result.send(parsed);
                    }
                    Ok(())
                });
            let worker_inner = inner.clone();
            let process_branch = WorkerPipeline::start("osm_pass1", stats.clone())
                .read_from_queue(parsed_batches)
                .sink_to("process", 1, move |pending| {
                    worker_inner.process_pass1_parsed_blocks(pending.map(|rx| {
                        rx.recv().map_err(|_| anyhow::anyhow!("block parser failed")).and_then(|result| result)
                    }))
                });
            loggers = loggers.add_pipeline_stats(&read_branch).add_pipeline_stats(&process_branch);
            Done::join_all([read_branch.done(), process_branch.done()])
        };

        loggers.await_and_log(&done, config.log_interval)?;

        tracing::debug!("Processed {} blocks:", format::integer(inner.pass1_blocks.get()));
        inner.pass1_phaser.print_summary();
        timer.stop();

        inner.way_splitter.finish();
        Ok(())
    }

    /// Processes pass 1 for `blocks` of elements in the current thread (Java `processPass1Blocks`). May be called
    /// by multiple threads at once if the node location map supports parallel writes.
    pub fn process_pass1_blocks<I, E>(&self, blocks: I) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = anyhow::Result<E>>,
        E: IntoIterator<Item = OsmElement>,
    {
        self.inner.process_pass1_blocks(blocks)
    }

    /// Processes pass 1 for a single block of elements (test helper).
    pub fn process_pass1_block(&self, elements: impl IntoIterator<Item = OsmElement>) -> anyhow::Result<()> {
        self.inner.process_pass1_blocks([Ok(elements)])
    }

    /// Constructs geometries from OSM elements and emits map features as defined by the [`Profile`] into
    /// `feature_group`.
    pub fn pass2(&self, feature_group: &Arc<FeatureGroup>, config: &Arc<PlanetilerConfig>) -> anyhow::Result<()> {
        let inner = &self.inner;
        let stats = &inner.stats;
        let timer = stats.start_stage("osm_pass2");
        let write_threads = config.feature_write_threads;
        let process_threads = config.feature_process_threads;
        let split_way_multiplier = get_split_way_multiplier(inner.max_way_id.load(Ordering::Relaxed));
        let blocks_processed = Counter::new_multi_thread_counter();
        // track relation count separately because they get enqueued onto the distributor near the end
        let relations_processed = Counter::new_multi_thread_counter();
        let pass2_phaser = Arc::new(OsmPhaser::new(process_threads));
        {
            let (blocks, rels, phaser) = (blocks_processed.clone(), relations_processed.clone(), pass2_phaser.clone());
            stats.labeled_counter(
                "osm_pass2_elements_processed",
                "type",
                Arc::new(move || {
                    [
                        ("blocks".to_string(), blocks.get()),
                        ("nodes".to_string(), phaser.nodes()),
                        ("ways".to_string(), phaser.ways()),
                        ("relations".to_string(), rels.get()),
                    ]
                    .into_iter()
                    .collect()
                }),
            );
        }

        // Use a Distributor to keep all worker threads busy when processing the final blocks of relations by
        // offloading items to threads that are done reading blocks
        let relation_distributor = Distributor::<Relation>::create_with_capacity(1_000);

        let source = inner.block_source.clone();
        let builder = WorkerPipeline::start("osm_pass2", stats.clone())
            .from_generator("read", move |next| {
                source.for_each_block(&mut |block| {
                    next.accept(block);
                    Ok(())
                })
            })
            .add_buffer("pbf_blocks", 10.max(process_threads / 2));
        let handler = {
            let inner = inner.clone();
            let config = config.clone();
            let group = feature_group.clone();
            let (blocks_processed, relations_processed) = (blocks_processed.clone(), relations_processed.clone());
            let pass2_phaser = pass2_phaser.clone();
            move |prev: &mut crate::worker::work_queue::WorkQueueReader<crate::reader::osm::osm_block_source::Block>,
                  next: &mut dyn FeatureSink| {
                // avoid contention trying to get the thread-local counters by getting them once when thread starts
                let blocks = blocks_processed.counter_for_thread();
                let rels = relations_processed.counter_for_thread();
                let factory = feature_collector::Factory::new(config.clone(), inner.stats.clone());
                let renderer = FeatureRenderer::new(config.clone(), inner.stats.clone());
                let node_locations = OsmReader::new_node_location_provider_for(&inner);
                let emitter =
                    RefCell::new(Emitter { encoder: group.new_rendered_feature_encoder(), sink: next, error: None });
                let mut phaser = pass2_phaser.for_worker();
                let mut relation_handler = relation_distributor.for_thread(|relation: Relation| {
                    let id = relation.id;
                    if let Some(feature) = inner.process_relation_pass2(relation, &node_locations) {
                        inner.render(&factory, &renderer, &feature, &emitter, "Relation", id);
                    }
                    rels.inc();
                });
                for block in prev {
                    for element in block.into_elements()? {
                        match element {
                            OsmElement::Node(node) => {
                                phaser.arrive(Phase::Nodes)?;
                                let id = node.id;
                                let feature = node_feature(node, node_locations.name.clone());
                                inner.render(&factory, &renderer, &feature, &emitter, "Node", id);
                            }
                            OsmElement::Way(way) => {
                                phaser.arrive(Phase::Ways)?;
                                let id = way.id;
                                let feature = inner.process_way_pass2(way, &node_locations)?;
                                inner.split_way_if_necessary(feature, split_way_multiplier, &mut |feature| {
                                    inner.render(&factory, &renderer, &feature, &emitter, "Way", id);
                                });
                            }
                            OsmElement::Relation(relation) => {
                                phaser.arrive_and_wait_for_others(Phase::Relations)?;
                                relation_handler.accept(relation);
                            }
                            OsmElement::Other(_) => {}
                        }
                    }
                    blocks.inc();
                    emitter.borrow_mut().check()?;
                }
                phaser.close()?;
                // do work for other threads that are still processing blocks of relations
                relation_handler.close();
                drop(relation_handler);
                emitter.borrow_mut().check()
            }
        };
        let pipeline = feature_group::process_and_write(
            builder,
            config.parallel_temp_io,
            process_threads,
            write_threads,
            feature_group.clone(),
            handler,
        );

        let (db, mp, group, group2) =
            (inner.node_location_db.clone(), inner.clone(), feature_group.clone(), feature_group.clone());
        let (p_nodes, p_ways) = (pass2_phaser.clone(), pass2_phaser.clone());
        let (rels, blocks) = (relations_processed.clone(), blocks_processed.clone());
        let logger = ProgressLoggers::create()
            .add_rate_percent_counter("nodes", inner.pass1_phaser.nodes(), move || p_nodes.nodes(), true)
            .add(move || {
                let bytes = db.disk_usage_bytes() + db.estimate_memory_usage_bytes();
                format!(" {}", format::pad_right(&format::storage(bytes, false), 5))
            })
            .add_rate_percent_counter("ways", inner.pass1_phaser.ways(), move || p_ways.ways(), true)
            .add_rate_percent_counter("rels", inner.pass1_phaser.relations(), move || rels.get(), true)
            .add_rate_counter("features", move || group.num_features_written() as i64)
            .add(move || format!(" {}", format::pad_right(&format::storage(group2.disk_usage_bytes(), false), 5)))
            .add_rate_percent_counter("blocks", inner.pass1_blocks.get(), move || blocks.get(), false)
            .new_line()
            .add_process_stats()
            .add_in_memory_object("relInfo", inner.clone())
            .add(move || {
                let m = &mp.multipolygon_way_geometries;
                let bytes = m.disk_usage_bytes() + m.estimate_memory_usage_bytes();
                format!(" mpGeoms: {}", format::pad_right(&format::storage(bytes, false), 5))
            })
            .new_line()
            .add_pipeline_stats(&pipeline);

        pipeline.await_and_log(&logger, config.log_interval)?;
        drop(pipeline);

        tracing::debug!("Processed {} blocks:", format::integer(blocks_processed.get()));
        pass2_phaser.print_summary();
        timer.stop();

        if let Err(e) = inner.finish_profile(feature_group, config) {
            tracing::error!("Error calling profile.finish: {e:#}");
        }
        Ok(())
    }

    /// Returns a handle that looks up node locations stored in pass 1 (Java `newNodeLocationProvider`). Create one
    /// per thread.
    pub fn new_node_location_provider(&self) -> Arc<NodeLocations> {
        OsmReader::new_node_location_provider_for(&self.inner)
    }

    fn new_node_location_provider_for(inner: &Arc<Inner>) -> Arc<NodeLocations> {
        // copy the name so each thread increments its own reference count
        Arc::new(NodeLocations { inner: inner.clone(), name: Arc::from(&*inner.name) })
    }

    /// Creates the source feature for a node in pass 2 (Java `processNodePass2`).
    pub fn process_node_pass2(&self, node: Node) -> OsmSourceFeature {
        node_feature(node, self.inner.name.clone())
    }

    /// Creates the source feature for a way in pass 2 (Java `processWayPass2`), storing its node IDs if it is part of
    /// a multipolygon.
    pub fn process_way_pass2(&self, way: Way, node_locations: &Arc<NodeLocations>) -> anyhow::Result<OsmSourceFeature> {
        self.inner.process_way_pass2(way, node_locations)
    }

    /// Creates the source feature for a multipolygon relation in pass 2, or `None` for other relations (Java
    /// `processRelationPass2`).
    pub fn process_relation_pass2(
        &self,
        relation: Relation,
        node_locations: &Arc<NodeLocations>,
    ) -> Option<OsmSourceFeature> {
        self.inner.process_relation_pass2(relation, node_locations)
    }

    /// Returns `feature` alone, or when the profile splits this way at intersections, the full way followed by each
    /// segment (Java `splitWayIfNecessary`).
    pub fn split_way_if_necessary(&self, feature: OsmSourceFeature, multiplier: i64) -> Vec<OsmSourceFeature> {
        let mut result = Vec::new();
        self.inner.split_way_if_necessary(feature, multiplier, &mut |f| result.push(f));
        result
    }

    /// Estimates the resource requirements for a nodemap but parses the type/storage from strings.
    pub fn estimate_node_location_usage_str(
        map_type: &str,
        storage: &str,
        osm_file_size: u64,
        path: &Path,
    ) -> anyhow::Result<ResourceUsage> {
        Ok(Self::estimate_node_location_usage(
            LongLongMapType::from_id(map_type)?,
            Storage::from_id(storage)?,
            osm_file_size,
            path,
        ))
    }

    /// Estimates the resource requirements for a nodemap for a given OSM input file.
    pub fn estimate_node_location_usage(
        map_type: LongLongMapType,
        storage: Storage,
        osm_file_size: u64,
        path: &Path,
    ) -> ResourceUsage {
        let nodes = estimate_num_nodes(osm_file_size);
        let max_node_id = estimate_max_node_id(osm_file_size);
        let check = ResourceUsage::new("nodemap");
        let kind = storage_kind(storage);
        match map_type {
            LongLongMapType::Noop => {}
            LongLongMapType::SparseArray => {
                check.add_memory(300_000_000, "sparsearray node location in-memory index").add_storage(
                    path,
                    kind,
                    9 * nodes,
                    "sparsearray node location cache",
                );
            }
            LongLongMapType::SortedTable => {
                check.add_memory(300_000_000, "sortedtable node location in-memory index").add_storage(
                    path,
                    kind,
                    12 * nodes,
                    "sortedtable node location cache",
                );
            }
            LongLongMapType::Array => {
                check.add_storage(
                    path,
                    kind,
                    8 * max_node_id,
                    "array node location cache (switch to sparsearray to reduce size)",
                );
            }
        }
        check
    }

    /// Estimates the resource requirements for a multipolygon geometry multimap but parses the storage from a string.
    pub fn estimate_multipolygon_geometry_usage_str(
        storage: &str,
        osm_file_size: u64,
        path: &Path,
    ) -> anyhow::Result<ResourceUsage> {
        Ok(Self::estimate_multipolygon_geometry_usage(Storage::from_id(storage)?, osm_file_size, path))
    }

    /// Estimates the resource requirements for a multipolygon geometry multimap for a given OSM input file.
    pub fn estimate_multipolygon_geometry_usage(storage: Storage, osm_file_size: u64, path: &Path) -> ResourceUsage {
        // Planet extract (62G) requires about 9.5G for way geometries
        let estimated_size = (9.5 * osm_file_size as f64 / 62.0) as u64;
        let check = ResourceUsage::new("way geometry multipolygon");
        check.add_storage(path, storage_kind(storage), estimated_size, "multipolygon way geometries");
        check
    }

    /// Releases node locations, relation info and multipolygon way geometries (Java `close()`).
    pub fn close(self) {
        drop(self);
    }
}

impl HasEstimate for OsmReader {
    fn estimate_memory_usage_bytes(&self) -> u64 {
        self.inner.estimate_memory_usage_bytes()
    }
}

fn storage_kind(storage: Storage) -> StorageKind {
    match storage {
        Storage::Ram => StorageKind::Ram,
        Storage::Mmap => StorageKind::Mmap,
        Storage::Direct => StorageKind::Direct,
    }
}

fn estimate_num_nodes(osm_file_size: u64) -> u64 {
    // On 2/14/2022, planet.pbf was 66691979646 bytes with ~7.5b nodes, so scale from there
    (7_500_000_000f64 * (osm_file_size as f64 / 66_691_979_646f64)).round() as u64
}

fn estimate_max_node_id(osm_file_size: u64) -> u64 {
    // On 2/14/2022, planet.pbf was 66691979646 bytes and max node ID was ~9.5b, so scale from there but don't go
    // less than 9.5b in case it's an extract
    (9_500_000_000f64 * 1f64.max(osm_file_size as f64 / 66_691_979_646f64)).round() as u64
}

fn get_split_way_multiplier(max_way_id: i64) -> i64 {
    let mut split_way_multiplier = 1i64;
    while split_way_multiplier < max_way_id {
        split_way_multiplier *= 10;
    }
    split_way_multiplier
}

fn node_feature(node: Node, name: Arc<str>) -> OsmSourceFeature {
    OsmSourceFeature::new(OsmElement::Node(node), OsmFeatureKind::Node, true, false, false, name, Vec::new(), None)
}

impl Inner {
    fn process_pass1_blocks<I, E>(&self, blocks: I) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = anyhow::Result<E>>,
        E: IntoIterator<Item = OsmElement>,
    {
        // may be called by multiple threads so need to synchronize access to any shared data structures
        let mut worker = Pass1Worker::new(self);
        for block in blocks {
            for element in block? {
                worker.process(element, None)?;
            }
            self.pass1_blocks.inc();
        }
        worker.finish()
    }

    /// Decodes a block on a parse thread for the single-threaded pass 1 writer: preprocesses leading nodes and
    /// pre-computes their encoded locations since that is fairly expensive and should be done in parallel.
    ///
    /// All node blocks come before way blocks, and the writer waits for each block in order, so every
    /// `preprocess_osm_node` call still finishes before the first `preprocess_osm_way` call.
    fn parse_pass1_block(&self, elements: impl Iterator<Item = OsmElement>) -> anyhow::Result<ParsedBlock> {
        let mut nodes = Vec::new();
        let mut others = Vec::new();
        let profile = &*self.profile;
        for element in elements {
            match element {
                OsmElement::Node(node) if others.is_empty() => {
                    if node.id < 0 {
                        anyhow::bail!("Negative OSM element IDs not supported: {node:?}");
                    }
                    if let Err(e) = catch_unwind(AssertUnwindSafe(|| profile.preprocess_osm_node(&node))) {
                        tracing::error!("Error preprocessing OSM node {}: {}", node.id, panic_message(&*e));
                    }
                    nodes.push((node.id, node.encoded_location()));
                }
                other => others.push(other),
            }
        }
        Ok(ParsedBlock { nodes, others })
    }

    fn process_pass1_parsed_blocks(
        &self,
        blocks: impl Iterator<Item = anyhow::Result<ParsedBlock>>,
    ) -> anyhow::Result<()> {
        let mut worker = Pass1Worker::new(self);
        for block in blocks {
            let ParsedBlock { nodes, others } = block?;
            worker.process_preprocessed_nodes(&nodes)?;
            for element in others {
                worker.process(element, None)?;
            }
            self.pass1_blocks.inc();
        }
        worker.finish()
    }

    fn render(
        &self,
        factory: &feature_collector::Factory,
        renderer: &FeatureRenderer,
        feature: &OsmSourceFeature,
        emitter: &RefCell<Emitter<'_>>,
        element_type: &str,
        id: i64,
    ) {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut features = factory.get(feature);
            self.profile.process_feature(feature, &mut features);
            if !features.is_empty() {
                let mut emitter = emitter.borrow_mut();
                for renderable in features.iter() {
                    renderer.accept(renderable, &mut |rendered| emitter.write(&rendered));
                }
            }
        }));
        if let Err(e) = result {
            tracing::error!("Error processing OSM {element_type} {id}: {}", panic_message(&*e));
        }
    }

    fn finish_profile(&self, group: &Arc<FeatureGroup>, config: &Arc<PlanetilerConfig>) -> anyhow::Result<()> {
        let mut writer = group.writer_for_thread()?;
        let mut encoder = group.new_rendered_feature_encoder();
        let renderer = FeatureRenderer::new(config.clone(), self.stats.clone());
        let factory = feature_collector::Factory::new(config.clone(), self.stats.clone());
        let mut error = None;
        let result = catch_unwind(AssertUnwindSafe(|| {
            self.profile.finish(&self.name, &factory, &mut |feature| {
                renderer.accept(feature, &mut |rendered| {
                    if error.is_none()
                        && let Err(e) = writer.write(&mut encoder, &rendered)
                    {
                        error = Some(e);
                    }
                });
            });
        }));
        if let Err(e) = result {
            anyhow::bail!("{}", panic_message(&*e));
        }
        if let Some(e) = error {
            return Err(e);
        }
        writer.close()
    }

    fn process_way_pass2(&self, way: Way, node_locations: &Arc<NodeLocations>) -> anyhow::Result<OsmSourceFeature> {
        // ways contain an ordered list of node IDs, so we need to join that with node locations from pass1 to
        // reconstruct the geometry.
        if self.ways_in_multipolygon.read().contains(way.id as u64) {
            // if this is part of a multipolygon, store the node IDs for this way ID so that when we get to the
            // multipolygon we can go from way IDs -> node IDs -> node locations.
            self.multipolygon_way_geometries.replace_values(way.id, &way.nodes)?;
        }
        let nodes = &way.nodes;
        let closed = nodes.len() > 1 && nodes[0] == nodes[nodes.len() - 1];
        // area tag used to differentiate between whether a closed way should be treated as a polygon or linestring
        let area = way.tags.get("area").map(|v| v.to_string_lossy());
        let area = area.as_deref();
        let line = can_be_line(closed, area, nodes.len());
        let polygon = can_be_polygon(closed, area, nodes.len());
        let state = self.relations.read_with(RelationState::freeze);
        let rels = state.get_relation_memberships(lookup(&state.way_to_relations, way.id));
        Ok(OsmSourceFeature::new(
            OsmElement::Way(way),
            OsmFeatureKind::Way,
            false,
            line,
            polygon,
            node_locations.name.clone(),
            rels,
            Some(node_locations.clone()),
        ))
    }

    fn process_relation_pass2(
        &self,
        relation: Relation,
        node_locations: &Arc<NodeLocations>,
    ) -> Option<OsmSourceFeature> {
        // Relation info gets used during way processing, except multipolygons which we have to process after we've
        // stored all the node IDs for each way.
        if is_multipolygon(&relation) {
            let state = self.relations.read_with(RelationState::freeze);
            let parent_relations = state.get_relation_memberships(state.parent_relations(relation.id));
            Some(OsmSourceFeature::new(
                OsmElement::Relation(relation),
                OsmFeatureKind::Multipolygon,
                false,
                false,
                true,
                node_locations.name.clone(),
                parent_relations,
                Some(node_locations.clone()),
            ))
        } else {
            None
        }
    }

    fn split_way_if_necessary(
        &self,
        mut feature: OsmSourceFeature,
        multiplier: i64,
        consumer: &mut dyn FnMut(OsmSourceFeature),
    ) {
        let OsmElement::Way(way) = &feature.element else {
            consumer(feature);
            return;
        };
        if feature.line && self.profile.split_osm_way_at_intersections(way) {
            let splits = self.way_splitter.get_split_indices(&way.nodes);
            if !splits.is_empty() {
                let segments = feature.as_split_lines(&splits, multiplier);
                feature.kind = OsmFeatureKind::FullWay;
                consumer(feature);
                segments.into_iter().for_each(consumer);
                return;
            }
        }
        consumer(feature);
    }
}

/// Looks up node locations stored in pass 1 (Java `NodeDbLocationProvider`). Features hold a reference to this to
/// compute their geometries lazily; create one per thread with [`OsmReader::new_node_location_provider`].
pub struct NodeLocations {
    inner: Arc<Inner>,
    /// per-thread copy of the source name
    name: Arc<str>,
}

impl NodeLocations {
    #[inline]
    fn get(&self, id: i64) -> anyhow::Result<Coord<f64>> {
        let encoded = self.inner.node_location_db.get(id);
        if encoded == MISSING_VALUE {
            anyhow::bail!("Missing location for node: {id}");
        }
        let encoded = encoded as u64;
        Ok(Coord { x: geo_utils::decode_world_x(encoded), y: geo_utils::decode_world_y(encoded) })
    }
}

impl NodeLocationProvider for NodeLocations {
    fn get_coordinate(&self, id: i64) -> anyhow::Result<Coord<f64>> {
        self.get(id)
    }

    fn get_way_geometry(&self, node_ids: &[i64]) -> anyhow::Result<Vec<Coord<f64>>> {
        let mut result = Vec::with_capacity(node_ids.len());
        for &id in node_ids {
            result.push(self.get(id)?);
        }
        Ok(result)
    }
}

/// What kind of OSM source feature this is (Java's `NodeSourceFeature`, `WaySourceFeature`, `FullWaySourceFeature`,
/// `SplitWaySourceFeature`, `MultipolygonSourceFeature` subclasses).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsmFeatureKind {
    /// A point created from an OSM node.
    Node,
    /// A line string or polygon created from an OSM way.
    Way,
    /// The full version of a way that also got split at intersections (Java `FullWay`).
    FullWay,
    /// One segment of a way split at intersections (Java `SplitWay`).
    SplitWay { unique_id: i64 },
    /// A multipolygon created from an OSM relation where `type=multipolygon` (or `boundary`/`land_area`).
    Multipolygon,
}

/// A source feature generated from an OSM element (Java `OsmReader.OsmFeature` and subclasses, implementing
/// `OsmSourceFeature`). Inferring the geometry can be expensive, so the geometry is constructed lazily on read.
///
/// Unclosed ways are always interpreted as line strings. Closed ways can be interpreted as either a polygon or a line
/// string unless the `area=yes` tag prevents them from being a line string or `area=no` prevents them from being a
/// polygon.
pub struct OsmSourceFeature {
    // tags live on `element` (the base's tags stay empty to avoid copying them)
    base: SourceFeatureBase,
    element: OsmElement,
    kind: OsmFeatureKind,
    point: bool,
    line: bool,
    polygon: bool,
    node_locations: Option<Arc<NodeLocations>>,
    world_geometry: OnceLock<GeometryResult<Geometry<f64>>>,
    lat_lon_geometry: OnceLock<GeometryResult<Geometry<f64>>>,
}

impl OsmSourceFeature {
    #[allow(clippy::too_many_arguments)]
    fn new(
        element: OsmElement,
        kind: OsmFeatureKind,
        point: bool,
        line: bool,
        polygon: bool,
        name: Arc<str>,
        relation_infos: Vec<RelationMember<dyn OsmRelationInfo>>,
        node_locations: Option<Arc<NodeLocations>>,
    ) -> OsmSourceFeature {
        let id = element.id();
        OsmSourceFeature {
            // tags live on the element
            base: SourceFeatureBase::new(Tags::default(), Some(name), None, relation_infos, id),
            element,
            kind,
            point,
            line,
            polygon,
            node_locations,
            world_geometry: OnceLock::new(),
            lat_lon_geometry: OnceLock::new(),
        }
    }

    /// The kind of OSM feature.
    pub fn kind(&self) -> OsmFeatureKind {
        self.kind
    }

    /// For segments of ways split at intersections (Java `SplitWay.uniqueId()`), the unique ID of this segment.
    pub fn unique_id(&self) -> Option<i64> {
        match self.kind {
            OsmFeatureKind::SplitWay { unique_id } => Some(unique_id),
            _ => None,
        }
    }

    fn way(&self) -> Option<&Way> {
        match &self.element {
            OsmElement::Way(way) => Some(way),
            _ => None,
        }
    }

    fn as_split_lines(&self, splits: &[usize], id_multiplier: i64) -> Vec<OsmSourceFeature> {
        let way = self.way().expect("way");
        let node_locations = self.node_locations.clone();
        let name = self.base.source.clone().unwrap_or_else(|| Arc::from(""));
        let new_segment = |from: usize, to: usize, segment: i64| {
            OsmSourceFeature::new(
                OsmElement::Way(way.split(from, to)),
                OsmFeatureKind::SplitWay { unique_id: way.id + segment * id_multiplier },
                false,
                self.line,
                false,
                name.clone(),
                self.base.relation_infos.clone(),
                node_locations.clone(),
            )
        };
        let mut result = Vec::with_capacity(splits.len() + 1);
        let mut start = 0;
        let mut segment_index = 0;
        for &end in splits {
            result.push(new_segment(start, end + 1, segment_index));
            segment_index += 1;
            start = end;
        }
        result.push(new_segment(start, way.nodes.len(), segment_index));
        result
    }

    fn way_coordinates(&self, stat: &str, kind: &str) -> GeometryResult<Vec<Coord<f64>>> {
        let way = self.way().expect("way");
        let nodes = self.node_locations.as_ref().expect("node locations");
        nodes
            .get_way_geometry(&way.nodes)
            .map_err(|e| GeometryException::new(stat, format!("Error building {kind} for way {}: {e}", way.id)))
    }

    fn compute_way_line(&self) -> GeometryResult<Geometry<f64>> {
        let coords = self.way_coordinates("osm_invalid_line", "line")?;
        if coords.len() < 2 {
            return Err(GeometryException::new(
                "osm_invalid_line",
                format!("Error building line for way {}: Invalid number of points in LineString", self.id()),
            ));
        }
        Ok(Geometry::LineString(LineString(coords)))
    }

    fn compute_multipolygon(&self, relation: &Relation) -> GeometryResult<Geometry<f64>> {
        let node_locations = self.node_locations.as_ref().expect("node locations");
        let inner = &node_locations.inner;
        let mut rings = Vec::with_capacity(relation.members.len());
        let mut added = HashSet::new();
        let relation_type = || relation.get_tag("type").map_or(Cow::Borrowed("null"), |t| t.to_string_lossy());
        for member in &relation.members {
            if member.member_type == OsmElementType::Way {
                if !added.insert(member.ref_id) {
                    // ignore duplicate relation members
                    inner.stats.data_error(&format!("osm_{}_duplicate_member", relation_type()));
                } else {
                    let poly = inner.multipolygon_way_geometries.get(member.ref_id);
                    if !poly.is_empty() {
                        rings.push(poly);
                    } else {
                        // boundary and land_area relations might not be complete for extracts, but multipolygons
                        // should be
                        inner.stats.data_error(&format!("osm_{}_missing_way", relation_type()));
                        tracing::trace!(
                            "Missing {} OsmWay[{}] for {} {:?}",
                            member.role,
                            member.ref_id,
                            relation_type(),
                            self
                        );
                    }
                }
            }
        }
        osm_multipolygon::build(&rings, &**node_locations, relation.id)
    }
}

impl fmt::Debug for OsmSourceFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            OsmFeatureKind::Node => "OsmNode",
            OsmFeatureKind::Multipolygon => "OsmRelation",
            _ => "OsmWay",
        };
        write!(f, "{kind}[{}]", self.base.id)
    }
}

impl WithGeometryType for OsmSourceFeature {
    fn is_point(&self) -> bool {
        self.point
    }

    fn can_be_polygon(&self) -> bool {
        self.polygon
    }

    fn can_be_line(&self) -> bool {
        self.line
    }
}

impl WithGeometry for OsmSourceFeature {
    fn world_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        match &self.element {
            OsmElement::Node(node) => cached(&self.world_geometry, || {
                let encoded = geo_utils::encode_flat_location(node.lon, node.lat);
                Ok(Geometry::Point(Point::new(geo_utils::decode_world_x(encoded), geo_utils::decode_world_y(encoded))))
            }),
            // line() / polygon() memoize the geometry in the geometry cache, reuse it as the world geometry
            OsmElement::Way(_) => {
                if self.polygon {
                    self.polygon()
                } else {
                    self.line()
                }
            }
            OsmElement::Relation(relation) => cached(&self.world_geometry, || self.compute_multipolygon(relation)),
            OsmElement::Other(_) => cached(&self.world_geometry, || Ok(geo_utils::empty_geometry())),
        }
    }

    fn lat_lon_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        cached(&self.lat_lon_geometry, || Ok(geo_utils::world_to_lat_lon_coords(self.world_geometry()?)))
    }

    fn geometry_cache(&self) -> &GeometryCache {
        &self.base.geometry_cache
    }

    fn compute_line(&self) -> GeometryResult<Option<Geometry<f64>>> {
        match &self.element {
            OsmElement::Way(_) => {
                // closed ways that are also polygons share one node location lookup
                if self.polygon
                    && let Ok(Geometry::Polygon(polygon)) = self.polygon()
                {
                    return Ok(Some(Geometry::LineString(polygon.exterior().clone())));
                }
                self.compute_way_line().map(Some)
            }
            _ => {
                let world = self.world_geometry()?;
                match world {
                    Geometry::LineString(_) | Geometry::MultiLineString(_) | Geometry::Line(_) => Ok(None),
                    other => geo_utils::polygon_to_line_string(other).map(Some),
                }
            }
        }
    }

    fn compute_polygon(&self) -> GeometryResult<Option<Geometry<f64>>> {
        match &self.element {
            OsmElement::Way(way) => {
                let coords = self.way_coordinates("osm_invalid_polygon", "polygon")?;
                if coords.len() < 4 || coords[0] != coords[coords.len() - 1] {
                    return Err(GeometryException::new(
                        "osm_invalid_polygon",
                        format!(
                            "Error building polygon for way {}: Points of LinearRing do not form a closed linestring",
                            way.id
                        ),
                    ));
                }
                Ok(Some(Geometry::Polygon(Polygon::new(LineString(coords), vec![]))))
            }
            _ => Ok(None),
        }
    }
}

impl SourceFeature for OsmSourceFeature {
    fn base(&self) -> &SourceFeatureBase {
        &self.base
    }

    fn vector_tile_feature_id(&self, multiplier: i64, renumber: bool) -> i64 {
        let id = match self.kind {
            OsmFeatureKind::SplitWay { unique_id } if renumber => unique_id,
            _ => self.base.id,
        };
        let offset = match self.element.element_type() {
            OsmElementType::Other => 0,
            OsmElementType::Node => 1,
            OsmElementType::Way => 2,
            OsmElementType::Relation => 3,
        };
        id * multiplier + offset
    }

    fn original_element(&self) -> Option<&OsmElement> {
        Some(&self.element)
    }

    fn is_full_way(&self) -> bool {
        self.kind == OsmFeatureKind::FullWay
    }

    fn is_split_way(&self) -> bool {
        matches!(self.kind, OsmFeatureKind::SplitWay { .. })
    }

    fn as_any_feature(&self) -> &dyn Any {
        self
    }
}

impl WithTags for OsmSourceFeature {
    #[inline]
    fn tags(&self) -> &Tags {
        self.element.tags()
    }
}

impl ExpressionInput for OsmSourceFeature {
    fn as_with_geometry_type(&self) -> Option<&dyn WithGeometryType> {
        Some(self)
    }

    fn source(&self) -> Option<&str> {
        self.base.source.as_deref()
    }

    fn source_layer(&self) -> Option<&str> {
        None
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use geo_types::Geometry;

    use super::*;
    use crate::collection::long_long_map;
    use crate::feature_collector::FeatureCollector;
    use crate::geo::test_utils::*;
    use crate::reader::osm::osm_block_source::ElementsBlockSource;
    use crate::reader::osm::osm_element::Member;
    use crate::reader::source_feature::SourceFeatureExt;
    use crate::stats::stats::InMemoryStats;
    use crate::tags;

    #[test]
    fn lookup_matches_java_multimap_order() {
        // Java's SparseUnorderedBinarySearchMultimap.get: forwards from Arrays.binarySearch's hit, then backwards
        let pairs = [(1, 10), (1, 11), (1, 12), (2, 20), (3, 30), (3, 31)];
        let get = |key| lookup(&pairs, key).iter().map(|p| p.1).collect::<Vec<_>>();
        assert_eq!(vec![12, 11, 10], get(1));
        assert_eq!(vec![20], get(2));
        assert_eq!(vec![30, 31], get(3));
        assert!(get(4).is_empty());
        assert!(lookup(&[], 1).is_empty());
    }

    type RelationFn = dyn Fn(&Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> + Send + Sync;

    #[derive(Default)]
    struct TestProfile {
        relation: Option<Box<RelationFn>>,
        split: bool,
    }

    impl Profile for TestProfile {
        fn process_feature(&self, _source_feature: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {}

        fn preprocess_osm_relation(&self, relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
            self.relation.as_ref().and_then(|f| f(relation))
        }

        fn split_osm_way_at_intersections(&self, _way: &Way) -> bool {
            self.split
        }
    }

    #[derive(Debug, PartialEq)]
    struct TestRelInfo {
        id: i64,
        name: &'static str,
    }

    impl OsmRelationInfo for TestRelInfo {
        fn id(&self) -> i64 {
            self.id
        }
    }

    #[derive(Debug, PartialEq)]
    struct OtherRelInfo(i64);

    impl OsmRelationInfo for OtherRelInfo {
        fn id(&self) -> i64 {
            self.0
        }
    }

    fn reader_with_stats(profile: TestProfile, stats: StatsRef) -> OsmReader {
        OsmReader::new(
            "osm",
            ElementsBlockSource::default(),
            Arc::from(long_long_map::new_in_memory_sorted_table()),
            Box::new(long_long_multimap::new_in_memory_replaceable_multimap()),
            Arc::new(profile),
            stats,
        )
    }

    fn reader_with(profile: TestProfile) -> OsmReader {
        reader_with_stats(profile, InMemoryStats::arc())
    }

    fn new_osm_reader() -> OsmReader {
        reader_with(TestProfile::default())
    }

    fn rel_info_profile(
        f: impl Fn(&Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> + Send + Sync + 'static,
    ) -> TestProfile {
        TestProfile { relation: Some(Box::new(f)), split: false }
    }

    fn split_profile() -> TestProfile {
        TestProfile { relation: None, split: true }
    }

    fn node(id: i64, x: f64, y: f64) -> Node {
        Node::new(id, geo_utils::get_world_lat(y), geo_utils::get_world_lon(x))
    }

    fn way(id: i64, nodes: &[i64]) -> Way {
        Way::with_tags(id, Tags::default(), nodes.to_vec())
    }

    fn pass1(reader: &OsmReader, elements: &[OsmElement]) {
        reader.process_pass1_block(elements.iter().cloned()).unwrap();
    }

    fn g(geom: impl Into<Geometry<f64>>) -> Geometry<f64> {
        geom.into()
    }

    fn to_world(geom: &Geometry<f64>) -> Geometry<f64> {
        geo_utils::lat_lon_to_world_coords(geom)
    }

    #[track_caller]
    fn assert_same(expected: &Geometry<f64>, actual: &[&Geometry<f64>]) {
        for a in actual {
            assert_same_normalized_feature(expected, a);
        }
    }

    #[track_caller]
    fn assert_pos(feature: &OsmSourceFeature) {
        assert_point_on_surface(feature.world_geometry().unwrap(), feature.point_on_surface().unwrap());
    }

    /// Runs pass 1, node and way pass 2 over `elements` then returns the pass 2 feature for `relation`.
    fn process_relation(reader: &OsmReader, elements: &[OsmElement], relation: &Relation) -> Option<OsmSourceFeature> {
        pass1(reader, elements);
        let node_cache = reader.new_node_location_provider();
        for element in elements {
            match element {
                OsmElement::Node(n) => drop(reader.process_node_pass2(n.clone())),
                OsmElement::Way(w) => drop(reader.process_way_pass2(w.clone(), &node_cache).unwrap()),
                _ => {}
            }
        }
        reader.process_relation_pass2(relation.clone(), &node_cache)
    }

    #[test]
    fn test_point() {
        let reader = new_osm_reader();
        let node = Node::with_tags(1, tags! {"key" => "value"}, 0.0, 0.0);
        pass1(&reader, &[node.clone().into()]);
        let feature = reader.process_node_pass2(node);
        assert!(feature.is_point());
        assert!(!feature.can_be_polygon());
        assert!(!feature.can_be_line());
        let expected = g(new_point(0.5, 0.5));
        assert_same(
            &expected,
            &[
                feature.world_geometry().unwrap(),
                feature.centroid().unwrap(),
                feature.point_on_surface().unwrap(),
                &to_world(feature.lat_lon_geometry().unwrap()),
            ],
        );
        assert_eq!(0.0, feature.area().unwrap());
        assert_eq!(0.0, feature.length().unwrap());
        assert!(feature.line().is_err());
        assert!(feature.polygon().is_err());
        assert_eq!(&tags! {"key" => "value"}, feature.tags());
        assert_eq!(Some("osm"), feature.source());
        assert_eq!(11, feature.vector_tile_feature_id(10, false));
        assert_eq!("OsmNode[1]", format!("{feature:?}"));
    }

    #[test]
    fn test_line() {
        let reader = new_osm_reader();
        let node_cache = reader.new_node_location_provider();
        let node1 = Node::new(1, 0.0, 0.0);
        let node2 = node(2, 0.75, 0.75);
        let way = Way::with_tags(3, tags! {"key" => "value"}, vec![1, 2]);
        pass1(&reader, &[node1.into(), node2.into(), way.clone().into()]);

        let feature = reader.process_way_pass2(way, &node_cache).unwrap();
        assert!(feature.can_be_line());
        assert!(!feature.is_point());
        assert!(!feature.can_be_polygon());
        let expected = g(new_line_string(&[0.5, 0.5, 0.75, 0.75]));
        assert_same(
            &expected,
            &[
                feature.world_geometry().unwrap(),
                feature.line().unwrap(),
                &to_world(feature.lat_lon_geometry().unwrap()),
            ],
        );
        assert!(feature.polygon().is_err());
        assert_eq!(&g(new_point(0.625, 0.625)), feature.centroid().unwrap());
        assert_pos(&feature);
        assert_eq!(0.0, feature.area().unwrap());
        assert!(((2.0f64 * 0.25 * 0.25).sqrt() - feature.length().unwrap()).abs() < 1e-5);
        assert_eq!(&tags! {"key" => "value"}, feature.tags());
        assert_eq!(32, feature.vector_tile_feature_id(10, false));
        assert_eq!("OsmWay[3]", format!("{feature:?}"));
        assert!(matches!(feature.original_element(), Some(OsmElement::Way(w)) if w.nodes == vec![1, 2]));
    }

    fn square_nodes() -> Vec<OsmElement> {
        vec![node(1, 0.5, 0.5).into(), node(2, 0.5, 0.75).into(), node(3, 0.75, 0.75).into(), node(4, 0.75, 0.5).into()]
    }

    #[test]
    fn test_polygon_area_not_specified() {
        let reader = new_osm_reader();
        let node_cache = reader.new_node_location_provider();
        let way = Way::with_tags(3, tags! {"key" => "value"}, vec![1, 2, 3, 4, 1]);
        let mut elements = square_nodes();
        elements.push(way.clone().into());
        pass1(&reader, &elements);

        let feature = reader.process_way_pass2(way, &node_cache).unwrap();
        assert!(feature.can_be_line());
        assert!(!feature.is_point());
        assert!(feature.can_be_polygon());
        let expected = g(rectangle(0.5, 0.75));
        assert_same(
            &expected,
            &[
                feature.world_geometry().unwrap(),
                feature.polygon().unwrap(),
                &to_world(feature.lat_lon_geometry().unwrap()),
            ],
        );
        assert_same(&g(rectangle(0.5, 0.75).exterior().clone()), &[feature.line().unwrap()]);
        assert_eq!(&g(new_point(0.625, 0.625)), feature.centroid().unwrap());
        assert_pos(&feature);
        assert_eq!(0.25 * 0.25, feature.area().unwrap());
        assert_eq!(1.0, feature.length().unwrap());
    }

    #[test]
    fn test_polygon_area_yes() {
        let reader = new_osm_reader();
        let node_cache = reader.new_node_location_provider();
        let way = Way::with_tags(3, tags! {"area" => "yes"}, vec![1, 2, 3, 4, 1]);
        let mut elements = square_nodes();
        elements.push(way.clone().into());
        pass1(&reader, &elements);

        let feature = reader.process_way_pass2(way, &node_cache).unwrap();
        assert!(!feature.can_be_line());
        assert!(!feature.is_point());
        assert!(feature.can_be_polygon());
        let expected = g(rectangle(0.5, 0.75));
        assert_same(
            &expected,
            &[
                feature.world_geometry().unwrap(),
                feature.polygon().unwrap(),
                &to_world(feature.lat_lon_geometry().unwrap()),
            ],
        );
        assert!(feature.line().is_err());
        assert_eq!(&g(new_point(0.625, 0.625)), feature.centroid().unwrap());
        assert_pos(&feature);
        assert_eq!(0.25 * 0.25, feature.area().unwrap());
        assert_eq!(1.0, feature.length().unwrap());
    }

    #[test]
    fn test_polygon_area_no() {
        let reader = new_osm_reader();
        let node_cache = reader.new_node_location_provider();
        let way = Way::with_tags(5, tags! {"area" => "no"}, vec![1, 2, 3, 4, 1]);
        let mut elements = square_nodes();
        elements.push(way.clone().into());
        pass1(&reader, &elements);

        let feature = reader.process_way_pass2(way, &node_cache).unwrap();
        assert!(feature.can_be_line());
        assert!(!feature.is_point());
        assert!(!feature.can_be_polygon());
        let expected = g(rectangle(0.5, 0.75).exterior().clone());
        assert_same(
            &expected,
            &[
                feature.world_geometry().unwrap(),
                feature.line().unwrap(),
                &to_world(feature.lat_lon_geometry().unwrap()),
            ],
        );
        assert!(feature.polygon().is_err());
        assert_eq!(&g(new_point(0.625, 0.625)), feature.centroid().unwrap());
        assert_pos(&feature);
        assert_eq!(0.0, feature.area().unwrap());
        assert_eq!(1.0, feature.length().unwrap());
    }

    #[test]
    fn test_line_with_too_few_points() {
        let reader = new_osm_reader();
        let way = way(3, &[1]);
        pass1(&reader, &[node(1, 0.5, 0.5).into(), way.clone().into()]);

        let feature = reader.process_way_pass2(way, &reader.new_node_location_provider()).unwrap();
        assert!(!feature.can_be_line());
        assert!(!feature.is_point());
        assert!(!feature.can_be_polygon());
        assert!(feature.world_geometry().is_err());
        assert!(feature.lat_lon_geometry().is_err());
        assert!(feature.line().is_err());
        assert!(feature.centroid().is_err());
        assert!(feature.point_on_surface().is_err());
        assert_eq!(0.0, feature.area().unwrap());
        assert_eq!(0.0, feature.length().unwrap());
    }

    #[test]
    fn test_polygon_with_too_few_points() {
        let reader = new_osm_reader();
        let way = way(3, &[1, 2, 1]);
        pass1(&reader, &[node(1, 0.5, 0.5).into(), node(2, 0.5, 0.75).into(), way.clone().into()]);

        let feature = reader.process_way_pass2(way, &reader.new_node_location_provider()).unwrap();
        assert!(feature.can_be_line());
        assert!(!feature.is_point());
        assert!(!feature.can_be_polygon());
        let expected = g(new_line_string(&[0.5, 0.5, 0.5, 0.75, 0.5, 0.5]));
        assert_same(
            &expected,
            &[
                feature.world_geometry().unwrap(),
                feature.line().unwrap(),
                &to_world(feature.lat_lon_geometry().unwrap()),
            ],
        );
        assert_same(&g(new_point(0.5, 0.625)), &[feature.centroid().unwrap()]);
        assert_pos(&feature);
        assert_eq!(0.0, feature.area().unwrap());
        assert_eq!(0.5, feature.length().unwrap());
    }

    #[test]
    fn test_invalid_polygon() {
        let reader = new_osm_reader();
        pass1(
            &reader,
            &[
                node(1, 0.5, 0.5).into(),
                node(2, 0.75, 0.5).into(),
                node(3, 0.5, 0.75).into(),
                node(4, 0.75, 0.75).into(),
            ],
        );
        let way = Way::with_tags(6, tags! {"area" => "yes"}, vec![1, 2, 3, 4, 1]);
        pass1(&reader, &[way.clone().into()]);

        let feature = reader.process_way_pass2(way, &reader.new_node_location_provider()).unwrap();
        assert!(!feature.can_be_line());
        assert!(!feature.is_point());
        assert!(feature.can_be_polygon());
        let expected = g(new_polygon(&[0.5, 0.5, 0.75, 0.5, 0.5, 0.75, 0.75, 0.75, 0.5, 0.5]));
        assert_same(&expected, &[feature.world_geometry().unwrap(), &to_world(feature.lat_lon_geometry().unwrap())]);
        assert!(feature.line().is_err());
        assert_same(&g(new_point(0.625, 0.625)), &[feature.centroid().unwrap()]);
        assert_pos(&feature);
        assert_eq!(0.0, feature.area().unwrap());
        assert!((1.207 - feature.length().unwrap()).abs() < 1e-2);
    }

    #[test]
    fn test_line_referencing_nonexistent_node() {
        let reader = new_osm_reader();
        let way = way(321, &[123, 2222, 333, 444, 123]);
        pass1(&reader, &[way.clone().into()]);

        let feature = reader.process_way_pass2(way, &reader.new_node_location_provider()).unwrap();
        assert!(feature.can_be_line());
        assert!(!feature.is_point());
        assert!(feature.can_be_polygon());

        let exception = feature.line().unwrap_err();
        assert!(
            exception.message().contains("321") && exception.message().contains("123"),
            "Exception message did not contain way and missing node ID: {}",
            exception.message()
        );
        assert!(feature.world_geometry().is_err());
        assert!(feature.centroid().is_err());
        assert!(feature.polygon().is_err());
        assert!(feature.point_on_surface().is_err());
        assert!(feature.area().is_err());
        assert!(feature.length().is_err());
    }

    fn multipolygon(id: i64, relation_type: &str, members: &[(i64, &str)]) -> Relation {
        Relation::with_tags(
            id,
            tags! {"type" => relation_type},
            members.iter().map(|&(id, role)| Member::new(OsmElementType::Way, id, role)).collect(),
        )
    }

    #[test]
    fn test_multi_polygon() {
        for relation_type in ["multipolygon", "boundary", "land_area"] {
            let reader = new_osm_reader();
            let relation = multipolygon(11, relation_type, &[(9, "outer"), (10, "inner")]);
            let elements: Vec<OsmElement> = vec![
                node(1, 0.1, 0.1).into(),
                node(2, 0.9, 0.1).into(),
                node(3, 0.9, 0.9).into(),
                node(4, 0.1, 0.9).into(),
                node(5, 0.2, 0.2).into(),
                node(6, 0.8, 0.2).into(),
                node(7, 0.8, 0.8).into(),
                node(8, 0.2, 0.8).into(),
                way(9, &[1, 2, 3, 4, 1]).into(),
                way(10, &[5, 6, 7, 8, 5]).into(),
                relation.clone().into(),
            ];
            let feature = process_relation(&reader, &elements, &relation).unwrap();
            assert!(!feature.can_be_line());
            assert!(!feature.is_point());
            assert!(feature.can_be_polygon());
            let expected =
                g(new_polygon_with_holes(rectangle_coord_list(0.1, 0.9), vec![rectangle_coord_list(0.2, 0.8)]));
            assert_same(
                &expected,
                &[
                    &round(feature.world_geometry().unwrap()),
                    &round(feature.polygon().unwrap()),
                    &round(feature.validated_polygon().unwrap()),
                    &round(&to_world(feature.lat_lon_geometry().unwrap())),
                ],
            );
            assert!(feature.line().is_err());
            assert_same(&g(new_point(0.5, 0.5)), &[&round(feature.centroid().unwrap())]);
            assert_pos(&feature);
            assert!((0.28 - feature.area().unwrap()).abs() < 1e-5);
            assert!((5.6 - feature.length().unwrap()).abs() < 1e-2);
            assert_eq!(113, feature.vector_tile_feature_id(10, false));
            assert_eq!("OsmRelation[11]", format!("{feature:?}"));
        }
    }

    #[test]
    fn test_multipolygon_infers_correct_parent() {
        let reader = new_osm_reader();
        let relation = multipolygon(16, "multipolygon", &[(13, "outer"), (14, "inner"), (15, "inner")]);
        let elements: Vec<OsmElement> = vec![
            node(1, 0.1, 0.1).into(),
            node(2, 0.9, 0.1).into(),
            node(3, 0.9, 0.9).into(),
            node(4, 0.1, 0.9).into(),
            node(5, 0.2, 0.2).into(),
            node(6, 0.8, 0.2).into(),
            node(7, 0.8, 0.8).into(),
            node(8, 0.2, 0.8).into(),
            node(9, 0.3, 0.3).into(),
            node(10, 0.7, 0.3).into(),
            node(11, 0.7, 0.7).into(),
            node(12, 0.3, 0.7).into(),
            way(13, &[1, 2, 3, 4, 1]).into(),
            way(14, &[5, 6, 7, 8, 5]).into(),
            // nested hole marked as inner, but should actually be outer
            way(15, &[9, 10, 11, 12, 9]).into(),
            relation.clone().into(),
        ];
        let feature = process_relation(&reader, &elements, &relation).unwrap();
        assert!(!feature.can_be_line());
        assert!(!feature.is_point());
        assert!(feature.can_be_polygon());
        let expected = g(new_multi_polygon(vec![
            new_polygon_with_holes(rectangle_coord_list(0.1, 0.9), vec![rectangle_coord_list(0.2, 0.8)]),
            rectangle(0.3, 0.7),
        ]));
        assert_same(
            &expected,
            &[
                &round(feature.world_geometry().unwrap()),
                &round(feature.polygon().unwrap()),
                &round(feature.validated_polygon().unwrap()),
                &round(&to_world(feature.lat_lon_geometry().unwrap())),
            ],
        );
    }

    #[test]
    fn test_invalid_multipolygon() {
        let reader = new_osm_reader();
        let relation = multipolygon(16, "multipolygon", &[(13, "outer"), (14, "inner"), (15, "inner")]);
        let elements: Vec<OsmElement> = vec![
            node(1, 0.1, 0.1).into(),
            node(2, 0.9, 0.1).into(),
            node(3, 0.9, 0.9).into(),
            node(4, 0.1, 0.9).into(),
            node(5, 0.2, 0.3).into(),
            node(6, 0.8, 0.3).into(),
            node(7, 0.8, 0.8).into(),
            node(8, 0.2, 0.8).into(),
            node(9, 0.2, 0.2).into(),
            node(10, 0.8, 0.2).into(),
            node(11, 0.8, 0.7).into(),
            node(12, 0.2, 0.7).into(),
            way(13, &[1, 2, 3, 4, 1]).into(),
            way(14, &[5, 6, 7, 8, 5]).into(),
            way(15, &[9, 10, 11, 12, 9]).into(),
            relation.clone().into(),
        ];
        let feature = process_relation(&reader, &elements, &relation).unwrap();
        assert!(!feature.can_be_line());
        assert!(!feature.is_point());
        assert!(feature.can_be_polygon());
        assert_topologically_equivalent_feature(
            &g(new_polygon_with_holes(rectangle_coord_list(0.1, 0.9), vec![rectangle_coord_list(0.2, 0.8)])),
            &round(feature.validated_polygon().unwrap()),
        );
        assert_same_normalized_feature(
            &g(new_polygon_with_holes(
                rectangle_coord_list(0.1, 0.9),
                vec![rectangle_coord_list_xy(0.2, 0.3, 0.8, 0.8), rectangle_coord_list_xy(0.2, 0.2, 0.8, 0.7)],
            )),
            &round(feature.polygon().unwrap()),
        );
    }

    #[test]
    fn test_multipolygon_invalid_members() {
        let reader = new_osm_reader();
        let child_relation = Relation::new(17);
        let child_node = Node::new(17, 0.0, 0.0);
        let relation = Relation::with_tags(
            16,
            tags! {"type" => "multipolygon"},
            vec![
                Member::new(OsmElementType::Relation, child_relation.id, "outer"),
                Member::new(OsmElementType::Node, child_node.id, "inner"),
            ],
        );
        let elements: Vec<OsmElement> = vec![
            node(1, 0.1, 0.1).into(),
            node(2, 0.9, 0.1).into(),
            node(3, 0.9, 0.9).into(),
            node(4, 0.1, 0.9).into(),
            node(5, 0.2, 0.3).into(),
            node(6, 0.8, 0.3).into(),
            node(7, 0.8, 0.8).into(),
            node(8, 0.2, 0.8).into(),
            node(9, 0.2, 0.2).into(),
            node(10, 0.8, 0.2).into(),
            node(11, 0.8, 0.7).into(),
            node(12, 0.2, 0.7).into(),
            child_node.into(),
            child_relation.into(),
            relation.clone().into(),
        ];
        assert!(process_relation(&reader, &elements, &relation).is_none());
    }

    #[test]
    fn test_multi_polygon_refers_to_nonexistent_node() {
        let reader = new_osm_reader();
        let relation = multipolygon(6, "multipolygon", &[(5, "outer")]);
        let elements: Vec<OsmElement> = vec![
            node(1, 0.1, 0.1).into(),
            // node(2, 0.9, 0.1), MISSING!
            node(3, 0.9, 0.9).into(),
            node(4, 0.1, 0.9).into(),
            way(5, &[1, 2, 3, 4, 1]).into(),
            relation.clone().into(),
        ];
        let feature = process_relation(&reader, &elements, &relation).unwrap();
        assert!(feature.world_geometry().is_err());
        assert!(feature.polygon().is_err());
        assert!(feature.validated_polygon().is_err());
    }

    #[test]
    fn test_multi_polygon_refers_to_nonexistent_way() {
        let stats = InMemoryStats::arc();
        let reader = reader_with_stats(TestProfile::default(), stats.clone());
        let relation = multipolygon(6, "multipolygon", &[(5, "outer")]);
        let elements: Vec<OsmElement> = vec![
            node(1, 0.1, 0.1).into(),
            node(2, 0.9, 0.1).into(),
            node(3, 0.9, 0.9).into(),
            node(4, 0.1, 0.9).into(),
            // outerway, // missing!
            relation.clone().into(),
        ];
        let feature = process_relation(&reader, &elements, &relation).unwrap();
        assert!(feature.world_geometry().is_err());
        assert!(feature.polygon().is_err());
        assert!(feature.validated_polygon().is_err());
        assert_eq!(Some(&1), stats.data_errors().get("osm_multipolygon_missing_way"));
    }

    fn test_info_profile() -> TestProfile {
        rel_info_profile(|_| Some(vec![Arc::new(TestRelInfo { id: 1, name: "name" }) as Arc<dyn OsmRelationInfo>]))
    }

    #[test]
    fn test_way_in_relation() {
        let reader = reader_with(test_info_profile());
        let node_cache = reader.new_node_location_provider();
        let way = Way::with_tags(3, tags! {"key" => "value"}, vec![1, 2]);
        let relation = Relation::with_tags(4, Tags::default(), vec![Member::new(OsmElementType::Way, 3, "rolename")]);
        pass1(
            &reader,
            &[Node::new(1, 0.0, 0.0).into(), node(2, 0.75, 0.75).into(), way.clone().into(), relation.into()],
        );

        let feature = reader.process_way_pass2(way, &node_cache).unwrap();
        assert!(feature.relation_info::<OtherRelInfo>().is_empty());
        assert_eq!(
            vec![RelationMember::new("rolename", Arc::new(TestRelInfo { id: 1, name: "name" }))],
            feature.relation_info::<TestRelInfo>()
        );
    }

    #[test]
    fn test_node_or_way_relation_in_relation_doesnt_trigger_way() {
        let reader = reader_with(test_info_profile());
        let node_cache = reader.new_node_location_provider();
        let way = Way::with_tags(3, tags! {"key" => "value"}, vec![1, 2]);
        let relation = Relation::with_tags(
            4,
            Tags::default(),
            vec![
                Member::new(OsmElementType::Relation, 3, "rolename"),
                Member::new(OsmElementType::Node, 3, "rolename"),
            ],
        );
        pass1(
            &reader,
            &[Node::new(1, 0.0, 0.0).into(), node(2, 0.75, 0.75).into(), way.clone().into(), relation.into()],
        );

        let feature = reader.process_way_pass2(way, &node_cache).unwrap();
        assert!(feature.relation_info::<TestRelInfo>().is_empty());
    }

    fn same_info_reader() -> (OsmReader, Arc<TestRelInfo>) {
        let info = Arc::new(TestRelInfo { id: 1, name: "name" });
        let i = info.clone();
        (reader_with(rel_info_profile(move |_| Some(vec![i.clone() as Arc<dyn OsmRelationInfo>]))), info)
    }

    fn rel(id: i64, members: &[(OsmElementType, i64, &str)]) -> Relation {
        Relation::with_tags(id, Tags::default(), members.iter().map(|&(t, r, role)| Member::new(t, r, role)).collect())
    }

    fn super_relation_elements() -> (Vec<OsmElement>, Way, Way) {
        let way1 = Way::with_tags(5, tags! {"key" => "value"}, vec![1, 2]);
        let way2 = Way::with_tags(6, tags! {"key" => "value"}, vec![3, 4]);
        let relation1 = rel(7, &[(OsmElementType::Way, 5, "leafway")]);
        let relation2 = rel(8, &[(OsmElementType::Relation, 7, "midrel"), (OsmElementType::Way, 6, "rolename")]);
        let relation3 = rel(9, &[(OsmElementType::Relation, 7, "topmosrel")]);
        let elements = vec![
            node(1, 0.0, 0.0).into(),
            node(2, 1.0, 1.0).into(),
            node(3, 2.0, 2.0).into(),
            node(4, 3.0, 3.0).into(),
            way1.clone().into(),
            way2.clone().into(),
            relation1.into(),
            relation2.into(),
            relation3.into(),
        ];
        (elements, way1, way2)
    }

    #[test]
    fn test_super_relation_not_opted_in() {
        let (reader, info) = same_info_reader();
        let node_cache = reader.new_node_location_provider();
        let (elements, way1, way2) = super_relation_elements();
        pass1(&reader, &elements);
        let feature1 = reader.process_way_pass2(way1, &node_cache).unwrap();
        let feature2 = reader.process_way_pass2(way2, &node_cache).unwrap();
        assert_eq!(vec![RelationMember::new("leafway", info.clone())], feature1.relation_info::<TestRelInfo>());
        assert_eq!(vec![RelationMember::new("rolename", info.clone())], feature2.relation_info::<TestRelInfo>());
    }

    #[test]
    fn test_super_relation_ordered_easily() {
        let (reader, info) = same_info_reader();
        let node_cache = reader.new_node_location_provider();
        let (elements, way1, way2) = super_relation_elements();
        pass1(&reader, &elements);
        let feature1 = reader.process_way_pass2(way1, &node_cache).unwrap();
        let feature2 = reader.process_way_pass2(way2, &node_cache).unwrap();
        assert_eq!(
            vec![
                RelationMember::new("leafway", info.clone()),
                RelationMember::with_path("midrel", info.clone(), vec![7]),
                RelationMember::with_path("topmosrel", info.clone(), vec![7]),
            ],
            feature1.relation_info_with_super::<TestRelInfo>(true)
        );
        assert_eq!(
            vec![RelationMember::new("rolename", info.clone())],
            feature2.relation_info_with_super::<TestRelInfo>(true)
        );
    }

    #[test]
    fn test_super_relation_ordered_badly() {
        let (reader, info) = same_info_reader();
        let node_cache = reader.new_node_location_provider();
        let way1 = Way::with_tags(7, tags! {"key" => "value"}, vec![1, 2]);
        let way2 = Way::with_tags(8, tags! {"key" => "value"}, vec![3, 4]);
        let way3 = Way::with_tags(9, tags! {"key" => "value"}, vec![5, 6]);
        // way1 -> rel1
        // way2 -> rel2 -> rel1
        // way3 -> rel3 -> rel2 -> rel1
        let relation1 = rel(10, &[(OsmElementType::Relation, 11, "rel2"), (OsmElementType::Way, 7, "way1")]);
        let relation2 = rel(11, &[(OsmElementType::Relation, 12, "rel3"), (OsmElementType::Way, 8, "way2")]);
        let relation3 = rel(12, &[(OsmElementType::Way, 9, "way3")]);
        let mut elements: Vec<OsmElement> = (1..=6).map(|i| node(i, (i - 1) as f64, (i - 1) as f64).into()).collect();
        elements.extend([
            way1.clone().into(),
            way2.clone().into(),
            way3.clone().into(),
            relation1.into(),
            relation2.into(),
            relation3.into(),
        ]);
        pass1(&reader, &elements);
        let feature1 = reader.process_way_pass2(way1, &node_cache).unwrap();
        let feature2 = reader.process_way_pass2(way2, &node_cache).unwrap();
        let feature3 = reader.process_way_pass2(way3, &node_cache).unwrap();
        assert_eq!(
            vec![RelationMember::new("way1", info.clone())],
            feature1.relation_info_with_super::<TestRelInfo>(true)
        );
        assert_eq!(
            vec![RelationMember::new("way2", info.clone()), RelationMember::with_path("rel2", info.clone(), vec![11])],
            feature2.relation_info_with_super::<TestRelInfo>(true)
        );
        assert_eq!(
            vec![
                RelationMember::new("way3", info.clone()),
                RelationMember::with_path("rel3", info.clone(), vec![12]),
                RelationMember::with_path("rel2", info.clone(), vec![12, 11]),
            ],
            feature3.relation_info_with_super::<TestRelInfo>(true)
        );
    }

    #[test]
    fn test_cyclic_super_relation() {
        let (reader, info) = same_info_reader();
        let node_cache = reader.new_node_location_provider();
        let way1 = way(3, &[]);
        let relation1 = rel(4, &[(OsmElementType::Relation, 5, "rolename1"), (OsmElementType::Way, 3, "rolename2")]);
        let relation2 = rel(5, &[(OsmElementType::Relation, 4, "rolename3")]);
        pass1(
            &reader,
            &[
                node(1, 0.0, 0.0).into(),
                node(2, 1.0, 1.0).into(),
                way1.clone().into(),
                relation1.into(),
                relation2.into(),
            ],
        );
        let feature1 = reader.process_way_pass2(way1, &node_cache).unwrap();
        assert_eq!(
            vec![
                RelationMember::new("rolename2", info.clone()),
                RelationMember::with_path("rolename3", info.clone(), vec![4])
            ],
            feature1.relation_info_with_super::<TestRelInfo>(true)
        );
    }

    #[derive(Debug, PartialEq)]
    struct BuildingRelationInfo(i64);

    impl OsmRelationInfo for BuildingRelationInfo {
        fn id(&self) -> i64 {
            self.0
        }
    }

    fn building_profile() -> TestProfile {
        rel_info_profile(|relation| {
            relation
                .has_tag_str("type", "building")
                .then(|| vec![Arc::new(BuildingRelationInfo(relation.id)) as Arc<dyn OsmRelationInfo>])
        })
    }

    #[test]
    fn test_multipolygon_in_relation() {
        let reader = reader_with(building_profile());
        let multipolygon = multipolygon(6, "multipolygon", &[(5, "outer")]);
        let building = Relation::with_tags(
            7,
            tags! {"type" => "building"},
            vec![Member::new(OsmElementType::Relation, multipolygon.id, "outline")],
        );
        let elements: Vec<OsmElement> = vec![
            node(1, 0.1, 0.1).into(),
            node(2, 0.9, 0.1).into(),
            node(3, 0.9, 0.9).into(),
            way(5, &[1, 2, 3, 1]).into(),
            multipolygon.clone().into(),
            building.into(),
        ];
        let feature = process_relation(&reader, &elements, &multipolygon).unwrap();
        assert_eq!(
            vec![RelationMember::new("outline", Arc::new(BuildingRelationInfo(7)))],
            feature.relation_info::<BuildingRelationInfo>()
        );
    }

    #[test]
    fn test_multipolygon_does_not_get_relations_of_way_with_same_id() {
        let reader = reader_with(building_profile());
        let multipolygon = multipolygon(6, "multipolygon", &[(6, "outer")]);
        let building =
            Relation::with_tags(7, tags! {"type" => "building"}, vec![Member::new(OsmElementType::Way, 6, "outline")]);
        let elements: Vec<OsmElement> = vec![
            node(1, 0.1, 0.1).into(),
            node(2, 0.9, 0.1).into(),
            node(3, 0.9, 0.9).into(),
            way(6, &[1, 2, 3, 1]).into(),
            multipolygon.clone().into(),
            building.into(),
        ];
        let feature = process_relation(&reader, &elements, &multipolygon).unwrap();
        assert!(feature.relation_info::<BuildingRelationInfo>().is_empty());
    }

    fn split_elements(way1_nodes: &[i64]) -> (Vec<OsmElement>, Way, Way) {
        let way1 = way(1, way1_nodes);
        let way2 = way(2, &[2, 4]);
        let elements = vec![
            node(1, 0.0, 0.0).into(),
            node(2, 0.5, 0.5).into(),
            node(3, 0.75, 0.75).into(),
            node(4, 0.75, 0.5).into(),
            way1.clone().into(),
            way2.clone().into(),
        ];
        (elements, way1, way2)
    }

    #[track_caller]
    fn assert_line_geometries(expected: &Geometry<f64>, feature: &OsmSourceFeature) {
        assert_same(
            expected,
            &[
                &round(feature.world_geometry().unwrap()),
                &round(feature.line().unwrap()),
                &round(&to_world(feature.lat_lon_geometry().unwrap())),
            ],
        );
    }

    #[test]
    fn test_split_line() {
        let reader = reader_with(split_profile());
        let node_cache = reader.new_node_location_provider();
        let (elements, way1, way2) = split_elements(&[1, 2, 3]);
        pass1(&reader, &elements);

        let features = reader.split_way_if_necessary(reader.process_way_pass2(way1, &node_cache).unwrap(), 100);
        assert_eq!(3, features.len());
        let (f1, f2, f3) = (&features[0], &features[1], &features[2]);

        assert!(f1.is_full_way());
        assert!(f1.can_be_line());
        assert!(!f1.is_point());
        assert!(!f1.can_be_polygon());
        assert_line_geometries(&g(new_line_string(&[0.0, 0.0, 0.5, 0.5, 0.75, 0.75])), f1);

        assert!(f2.is_split_way());
        assert!(f2.can_be_line());
        assert!(!f2.is_point());
        assert!(!f2.can_be_polygon());
        assert_line_geometries(&g(new_line_string(&[0.0, 0.0, 0.5, 0.5])), f2);
        assert_eq!(Some(1), f2.unique_id());
        assert_eq!(12, f2.vector_tile_feature_id(10, true));

        assert!(f3.is_split_way());
        assert!(f3.can_be_line());
        assert!(!f3.is_point());
        assert!(!f3.can_be_polygon());
        assert_line_geometries(&g(new_line_string(&[0.5, 0.5, 0.75, 0.75])), f3);
        assert_eq!(Some(101), f3.unique_id());
        assert_eq!(1012, f3.vector_tile_feature_id(10, true));
        assert_eq!(12, f3.vector_tile_feature_id(10, false));
        assert!(matches!(f3.original_element(), Some(OsmElement::Way(w)) if w.nodes == vec![2, 3]));

        let features = reader.split_way_if_necessary(reader.process_way_pass2(way2, &node_cache).unwrap(), 100);
        assert_eq!(1, features.len());
        let f4 = &features[0];
        assert!(!f4.is_split_way());
        assert!(!f4.is_full_way());
        assert!(f4.can_be_line());
        assert!(!f4.is_point());
        assert!(!f4.can_be_polygon());
        assert_line_geometries(&g(new_line_string(&[0.5, 0.5, 0.75, 0.5])), f4);
    }

    #[test]
    fn test_split_line_loop() {
        let reader = reader_with(split_profile());
        let node_cache = reader.new_node_location_provider();
        let (elements, way1, _) = split_elements(&[1, 2, 3, 1]);
        pass1(&reader, &elements);

        let features = reader.split_way_if_necessary(reader.process_way_pass2(way1, &node_cache).unwrap(), 100);
        assert_eq!(3, features.len());

        let f = &features[0];
        assert!(f.is_full_way());
        assert!(f.can_be_line());
        assert!(!f.is_point());
        assert!(f.can_be_polygon());
        let expected = g(new_polygon(&[0.0, 0.0, 0.5, 0.5, 0.75, 0.75, 0.0, 0.0]));
        assert_same(
            &expected,
            &[
                &round(f.world_geometry().unwrap()),
                &round(f.polygon().unwrap()),
                &round(&to_world(f.lat_lon_geometry().unwrap())),
            ],
        );
        assert_same(&g(new_line_string(&[0.0, 0.0, 0.5, 0.5, 0.75, 0.75, 0.0, 0.0])), &[&round(f.line().unwrap())]);

        let f = &features[1];
        assert!(f.is_split_way());
        assert!(f.can_be_line());
        assert!(!f.is_point());
        assert!(!f.can_be_polygon());
        assert_line_geometries(&g(new_line_string(&[0.0, 0.0, 0.5, 0.5])), f);

        let f = &features[2];
        assert!(f.is_split_way());
        assert!(f.can_be_line());
        assert!(!f.is_point());
        assert!(!f.can_be_polygon());
        assert_line_geometries(&g(new_line_string(&[0.5, 0.5, 0.75, 0.75, 0.0, 0.0])), f);
    }

    #[test]
    fn test_dont_split_line_with_area_yes() {
        let reader = reader_with(split_profile());
        let node_cache = reader.new_node_location_provider();
        let way1 = Way::with_tags(1, tags! {"area" => "yes"}, vec![1, 2, 3, 1]);
        let way2 = way(2, &[2, 4]);
        pass1(
            &reader,
            &[
                node(1, 0.0, 0.0).into(),
                node(2, 0.5, 0.5).into(),
                node(3, 0.75, 0.75).into(),
                node(4, 0.75, 0.5).into(),
                way1.clone().into(),
                way2.into(),
            ],
        );
        let features = reader.split_way_if_necessary(reader.process_way_pass2(way1, &node_cache).unwrap(), 100);
        assert_eq!(1, features.len());
        assert!(!features[0].is_split_way());
        assert!(!features[0].is_full_way());
    }

    #[test]
    fn test_negative_ids_and_out_of_order_elements_fail() {
        let reader = new_osm_reader();
        assert!(reader.process_pass1_block([Node::new(-1, 0.0, 0.0).into()]).is_err());
        let reader = new_osm_reader();
        assert!(reader.process_pass1_block([way(1, &[]).into(), Node::new(1, 0.0, 0.0).into()]).is_err());
    }

    #[test]
    fn test_can_be_line_polygon() {
        assert!(can_be_line(false, None, 2));
        assert!(!can_be_line(false, None, 1));
        assert!(!can_be_line(true, Some("yes"), 5));
        assert!(can_be_line(true, Some("no"), 5));
        assert!(can_be_polygon(true, None, 4));
        assert!(!can_be_polygon(true, None, 3));
        assert!(!can_be_polygon(true, Some("no"), 5));
        assert!(!can_be_polygon(false, Some("yes"), 5));
        assert_eq!(1, get_split_way_multiplier(i64::MIN));
        assert_eq!(1000, get_split_way_multiplier(999));
        assert_eq!(1000, get_split_way_multiplier(1000));
    }

    /// Emits one z14 feature per tagged element (same as `BenchmarkOsmReader`'s `emit` profile).
    struct EmitProfile;

    impl Profile for EmitProfile {
        fn process_feature(&self, source: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
            if source.tags().is_empty() {
                return;
            }
            if source.is_point() {
                features.point("point").set_min_zoom(14);
            } else if source.can_be_polygon() {
                features.polygon("polygon").set_min_zoom(14);
            } else if source.can_be_line() {
                features.line("line").set_min_zoom(14);
            }
        }
    }

    /// End-to-end pass 1 + pass 2 over monaco; expected values come from running Java's OsmReader with the same
    /// profile (`bench/java/BenchmarkOsmReader.java --profile=emit`).
    fn run_monaco(map_type: LongLongMapType) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources/monaco-latest.osm.pbf");
        let input = crate::reader::osm::osm_input_file::OsmInputFile::new(path, false);
        let stats = InMemoryStats::arc();
        let config = Arc::new(PlanetilerConfig::defaults());
        let profile: Arc<dyn Profile> = Arc::new(EmitProfile);
        let map = long_long_map::from(map_type, Storage::Ram, &Default::default()).unwrap();
        let reader = OsmReader::new_in_memory_multipolygons(
            "osm",
            input.get().unwrap(),
            Arc::from(map),
            profile.clone(),
            stats.clone(),
        );
        let group = Arc::new(FeatureGroup::new_in_memory(Default::default(), profile, config.clone(), stats.clone()));
        reader.pass1(&config).unwrap();
        assert_eq!(25_423, reader.inner.pass1_phaser.nodes());
        assert_eq!(4_106, reader.inner.pass1_phaser.ways());
        assert_eq!(243, reader.inner.pass1_phaser.relations());
        reader.pass2(&group, &config).unwrap();
        reader.close();
        assert_eq!(7002, group.num_features_written());
        let errors = stats.data_errors();
        assert_eq!(Some(&11948), errors.get("osm_boundary_missing_way"));
        assert_eq!(Some(&29), errors.get("feature_polygon_osm_invalid_multipolygon_empty_after_fix"));
        assert_eq!(Some(&1), errors.get("feature_polygon_osm_invalid_multipolygon_empty"));
    }

    #[test]
    fn test_monaco_sequential_node_map() {
        run_monaco(LongLongMapType::SparseArray);
    }

    #[test]
    fn test_monaco_parallel_node_map() {
        run_monaco(LongLongMapType::Array);
    }
}
