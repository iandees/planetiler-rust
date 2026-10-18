//! Port of `collection/FeatureGroup.java`: accepts rendered map features in any order and groups them by tile for a
//! reader to iterate through.
//!
//! # Writing
//!
//! Each writing thread creates its own [`RenderedFeatureEncoder`] ([`FeatureGroup::new_rendered_feature_encoder`]) to
//! serialize [`RenderedFeature`]s into a sort key and msgpack-encoded value, and its own sort writer
//! ([`FeatureGroup::writer_for_thread`]). [`FeatureGroup::feature_writer`] bundles both into a [`FeatureWriter`], and
//! [`process_and_write`] is the port of Java's `WorkerPipeline.processAndWrite` that wires a processing step into
//! per-thread writers (inline or through a write queue).
//!
//! # Reading
//!
//! After all writers are dropped, [`FeatureGroup::prepare`] sorts the features once (it takes `&mut self` because
//! sorting needs exclusive access to the sorter; when the group is shared in an `Arc` across a worker pipeline, drop
//! the pipeline and use `Arc::get_mut` / `Arc::into_inner`). Then [`FeatureGroup::iter`] (or
//! [`FeatureGroup::parallel_iterator`]) yields one [`TileFeatures`] per tile in tile order. `TileFeatures` own their
//! encoded entries plus an `Arc` to the shared string tables and profile, so they are `Send + 'static` and can be
//! handed to encoder threads that call [`TileFeatures::get_vector_tile`].
//!
//! Limitation: layer names get compressed into a single byte, so only 256 unique layers are supported, and attribute
//! keys into 100,000 values (see [`CommonStringEncoder`]).

use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::Context;
use hashbrown::HashMap;
use tracing::error;

use crate::collection::external_merge_sort::ExternalMergeSort;
use crate::collection::feature_sort::{self, FeatureIter, FeatureSort, FeatureSortWriter};
use crate::collection::sortable_feature::SortableFeature;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::geo::geometry_exception::GeometryException;
use crate::geo::geometry_type::GeometryType;
use crate::geo::tile_coord::TileCoord;
use crate::geo::tile_order::TileOrder;
use crate::profile::{LayerFeatures, Profile};
use crate::render::rendered_feature::{Group, RenderedFeature};
use crate::stats::stats::StatsRef;
use crate::util::common_string_encoder::{AsByte, CommonStringEncoder};
use crate::util::layer_attr_stats;
use crate::value::{Tags, Value};
use crate::vector_tile::{self, VectorGeometry, VectorTile};
use crate::worker::work_queue::{WorkQueueReader, WorkQueueWriter};
use crate::worker::worker_pipeline::{Builder, WorkerPipeline};

/// Number of bits the sort key gets packed into.
pub const SORT_KEY_BITS: u32 = 22;
/// Maximum allowed sort key.
pub const SORT_KEY_MAX: i32 = (1 << (SORT_KEY_BITS - 1)) - 1;
/// Minimum allowed sort key.
pub const SORT_KEY_MIN: i32 = -(1 << (SORT_KEY_BITS - 1));
const SORT_KEY_MASK: i64 = (1 << SORT_KEY_BITS) - 1;
const TILE_ID_BITS: u32 = 33;

thread_local! {
    static CURRENT_TILE: Cell<Option<TileCoord>> = const { Cell::new(None) };
}

/// Returns the tile currently being post-processed on this thread (only tracked when `--log-jts-exceptions` is set).
pub fn get_current_tile_for_debugging() -> Option<TileCoord> {
    CURRENT_TILE.with(Cell::get)
}

/// Encode key by `tile` asc, `layer` asc, `sort_key` asc with an extra bit to indicate whether the value contains
/// grouping information.
///
/// Bit layout (64 bits total): `[tile: 33 bits (63-31)][layer: 8 bits (30-23)][sortKey: 22 bits (22-1)][hasGroup: 1
/// bit (0)]`. Keys compare as unsigned.
#[inline]
pub fn encode_key(tile: i64, layer: u8, sort_key: i32, has_group: bool) -> i64 {
    (tile << (64 - TILE_ID_BITS))
        | ((layer as i64) << SORT_KEY_BITS << 1)
        | ((((sort_key as i64) - SORT_KEY_MIN as i64) & SORT_KEY_MASK) << 1)
        | has_group as i64
}

#[inline]
pub fn extract_has_group_from_key(key: i64) -> bool {
    (key & 1) == 1
}

#[inline]
pub fn extract_tile_from_key(key: i64) -> u64 {
    (key as u64) >> (64 - TILE_ID_BITS)
}

#[inline]
pub fn extract_layer_id_from_key(key: i64) -> u8 {
    ((key as u64) >> (SORT_KEY_BITS + 1)) as u8
}

#[inline]
pub fn extract_sort_key_from_key(key: i64) -> i32 {
    ((key >> 1) & SORT_KEY_MASK) as i32 + SORT_KEY_MIN
}

/// Geometry type stored in the low 3 bits of the geometry metadata byte.
#[inline]
pub fn decode_geom_type(geom_type_and_scale: u8) -> GeometryType {
    GeometryType::from_byte(geom_type_and_scale & 0b111)
}

/// Scale stored in the high 5 bits of the geometry metadata byte.
#[inline]
pub fn decode_scale(geom_type_and_scale: u8) -> u32 {
    (geom_type_and_scale >> 3) as u32
}

/// Packs geometry type and scale into one byte.
#[inline]
pub fn encode_geom_type_and_scale(geometry: &VectorGeometry) -> u8 {
    debug_assert!(geometry.geom_type().as_byte() <= 8);
    debug_assert!(geometry.scale() < (1 << 5));
    geometry.geom_type().as_byte() | ((geometry.scale() as u8) << 3)
}

/// Lets feature construction work whether `vector_tile::Feature::tags` is `Tags` or `Arc<Tags>`.
trait FromTags {
    fn from_tags(tags: Tags) -> Self;
}

impl FromTags for Tags {
    #[inline]
    fn from_tags(tags: Tags) -> Self {
        tags
    }
}

impl FromTags for Arc<Tags> {
    #[inline]
    fn from_tags(tags: Tags) -> Self {
        Arc::new(tags)
    }
}

/// State shared between the feature group, encoders and [`TileFeatures`].
struct Shared {
    profile: Arc<dyn Profile>,
    config: Arc<PlanetilerConfig>,
    stats: StatsRef,
    tile_order: TileOrder,
    common_layer_strings: AsByte,
    common_value_strings: CommonStringEncoder,
}

/// Groups rendered features by tile. See the [module docs](self).
pub struct FeatureGroup {
    sorter: Box<dyn FeatureSort>,
    shared: Arc<Shared>,
    prepared: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StringEncoders {
    layer_strings: Vec<String>,
    value_strings: Vec<String>,
}

impl FeatureGroup {
    /// Returns a feature grouper backed by `sorter`.
    pub fn new(
        sorter: Box<dyn FeatureSort>,
        tile_order: TileOrder,
        profile: Arc<dyn Profile>,
        config: Arc<PlanetilerConfig>,
        stats: StatsRef,
    ) -> FeatureGroup {
        FeatureGroup {
            sorter,
            shared: Arc::new(Shared {
                profile,
                config,
                stats,
                tile_order,
                common_layer_strings: AsByte::new(),
                common_value_strings: CommonStringEncoder::new(100_000),
            }),
            prepared: false,
        }
    }

    /// Returns a feature grouper that stores all features in-memory. Only suitable for toy use-cases like unit tests.
    pub fn new_in_memory(
        tile_order: TileOrder,
        profile: Arc<dyn Profile>,
        config: Arc<PlanetilerConfig>,
        stats: StatsRef,
    ) -> FeatureGroup {
        Self::new(Box::new(feature_sort::new_in_memory()), tile_order, profile, config, stats)
    }

    /// Returns a feature grouper that writes all elements to disk in chunks, sorts each chunk, then reads back in
    /// order from those chunks. Suitable for making maps up to planet-scale.
    ///
    /// When `reuse_existing` is true, skips clearing the temp directory so that a previously-sorted feature DB can be
    /// restored via [`FeatureGroup::init_from_manifest`].
    pub fn new_disk_backed(
        tile_order: TileOrder,
        temp_dir: &Path,
        profile: Arc<dyn Profile>,
        config: Arc<PlanetilerConfig>,
        stats: StatsRef,
        reuse_existing: bool,
    ) -> anyhow::Result<FeatureGroup> {
        let sorter = ExternalMergeSort::new(temp_dir, reuse_existing, &config, stats.clone())?;
        Ok(Self::new(Box::new(sorter), tile_order, profile, config, stats))
    }

    /// Saves the string encoder tables to `path` so they can be restored via [`FeatureGroup::load_string_encoders`].
    pub fn save_string_encoders(&self, path: &Path) -> anyhow::Result<()> {
        let payload = StringEncoders {
            layer_strings: self
                .shared
                .common_layer_strings
                .get_strings_in_order()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            value_strings: self
                .shared
                .common_value_strings
                .get_strings_in_order()
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        let json = serde_json::to_vec(&payload)?;
        std::fs::write(path, json).with_context(|| format!("Unable to save string encoders to {path:?}"))
    }

    /// Restores string encoder tables from a file written by [`FeatureGroup::save_string_encoders`].
    pub fn load_string_encoders(&self, path: &Path) -> anyhow::Result<()> {
        let bytes = std::fs::read(path).with_context(|| format!("Unable to load string encoders from {path:?}"))?;
        let payload: StringEncoders = serde_json::from_slice(&bytes)?;
        for s in &payload.layer_strings {
            self.shared.common_layer_strings.encode(s)?;
        }
        for s in &payload.value_strings {
            self.shared.common_value_strings.encode(s)?;
        }
        Ok(())
    }

    /// Saves the sorted chunk manifest to `path` (only supported for disk-backed groups).
    pub fn save_chunk_manifest(&self, path: &Path) -> anyhow::Result<()> {
        self.sorter.save_manifest(path)
    }

    /// Restores sorted chunks from a manifest written by [`FeatureGroup::save_chunk_manifest`].
    pub fn init_from_manifest(&mut self, path: &Path) -> anyhow::Result<()> {
        self.sorter.init_from_manifest(path)?;
        self.prepared = true;
        Ok(())
    }

    pub fn num_features_written(&self) -> u64 {
        self.sorter.num_features_written()
    }

    pub fn disk_usage_bytes(&self) -> u64 {
        self.sorter.disk_usage_bytes()
    }

    pub fn chunks_to_read(&self) -> usize {
        self.sorter.chunks_to_read()
    }

    /// The config this group was created with.
    pub fn config(&self) -> &Arc<PlanetilerConfig> {
        &self.shared.config
    }

    /// Returns a new encoder for a single thread to use to serialize rendered features.
    pub fn new_rendered_feature_encoder(&self) -> RenderedFeatureEncoder {
        RenderedFeatureEncoder {
            shared: self.shared.clone(),
            buf: Vec::with_capacity(256),
            last_fill: None,
            strings: StringCache::default(),
        }
    }

    /// Returns a new sort writer for a single thread.
    pub fn writer_for_thread(&self) -> anyhow::Result<Box<dyn FeatureSortWriter + '_>> {
        self.sorter.writer_for_thread()
    }

    /// Returns an encoder and sort writer for a single thread, to write rendered features with
    /// [`FeatureWriter::write`].
    pub fn feature_writer(&self) -> anyhow::Result<FeatureWriter<'_>> {
        Ok(FeatureWriter { encoder: self.new_rendered_feature_encoder(), writer: self.writer_for_thread()? })
    }

    /// Sorts features to prepare for grouping after all features have been written. Does nothing if already sorted.
    pub fn prepare(&mut self) -> anyhow::Result<()> {
        if !self.prepared {
            self.sorter.sort()?;
            self.prepared = true;
        }
        Ok(())
    }

    /// True once [`FeatureGroup::prepare`] has been called.
    pub fn is_prepared(&self) -> bool {
        self.prepared
    }

    /// Sorts features if needed, then returns an iterator over the features in each tile, in tile order.
    pub fn iter(&mut self) -> anyhow::Result<TileFeaturesIter> {
        self.prepare()?;
        self.iter_prepared()
    }

    /// Returns an iterator over the features in each tile; fails if [`FeatureGroup::prepare`] has not been called.
    pub fn iter_prepared(&self) -> anyhow::Result<TileFeaturesIter> {
        anyhow::ensure!(self.prepared, "FeatureGroup::prepare must be called before iterating");
        Ok(TileFeaturesIter::new(self.shared.clone(), self.sorter.iterator(0, 1)?, Vec::new()))
    }

    /// Reads features using `threads` parallel threads and merges them into a sorted iterator of tiles. Call
    /// [`TileFeaturesIter::join`] when done to wait for the reader threads.
    pub fn parallel_iterator(&mut self, threads: usize) -> anyhow::Result<TileFeaturesIter> {
        self.prepare()?;
        let par = self.sorter.parallel_iterator(threads)?;
        Ok(TileFeaturesIter::new(self.shared.clone(), par.iterator, par.reader_threads))
    }
}

/// Serializes [`RenderedFeature`]s on a single thread (Java `RenderedFeatureEncoder`).
///
/// This gets called billions of times when generating the planet, so it re-uses one output buffer and avoids
/// re-encoding identical filled geometries (i.e. ocean): consecutive features that share the same
/// `Arc<vector_tile::Feature>` without a group reuse the previous encoded value.
pub struct RenderedFeatureEncoder {
    shared: Arc<Shared>,
    buf: Vec<u8>,
    last_fill: Option<Arc<vector_tile::Feature>>,
    strings: StringCache,
}

/// Per-thread cache in front of the shared layer and attribute-key string encoders, so that encoding a feature does
/// not touch the shared `RwLock` (every writer thread hammering its reader count was ~17% of OSM pass 2 CPU).
#[derive(Default)]
struct StringCache {
    last_layer: Option<(String, u8)>,
    keys: HashMap<Box<str>, u32, foldhash::fast::FixedState>,
}

impl StringCache {
    #[inline]
    fn layer(&mut self, shared: &Shared, layer: &str) -> anyhow::Result<u8> {
        if let Some((name, id)) = &self.last_layer
            && name == layer
        {
            return Ok(*id);
        }
        let id = shared.common_layer_strings.encode(layer)?;
        self.last_layer = Some((layer.to_string(), id));
        Ok(id)
    }

    #[inline]
    fn key(&mut self, shared: &Shared, key: &str) -> anyhow::Result<u32> {
        if let Some(&id) = self.keys.get(key) {
            return Ok(id);
        }
        let id = shared.common_value_strings.encode(key)?;
        self.keys.insert(key.into(), id);
        Ok(id)
    }
}

impl RenderedFeatureEncoder {
    /// Encodes `feature` into its sort key and value bytes (valid until the next call).
    pub fn encode(&mut self, feature: &RenderedFeature) -> anyhow::Result<(i64, &[u8])> {
        let vt = &feature.vector_tile_feature;
        let layer = self.strings.layer(&self.shared, &vt.layer)?;
        let key = encode_key(
            self.shared.tile_order.encode(&feature.tile) as i64,
            layer,
            feature.sort_key,
            feature.group.is_some(),
        );
        match &feature.group {
            Some(group) => {
                // don't bother memoizing if group is present
                self.last_fill = None;
                encode_value(&self.shared, &mut self.strings, vt, Some(group), &mut self.buf)?;
            }
            None => {
                let same = self.last_fill.as_ref().is_some_and(|last| Arc::ptr_eq(last, vt));
                if !same {
                    encode_value(&self.shared, &mut self.strings, vt, None, &mut self.buf)?;
                    self.last_fill = Some(vt.clone());
                }
            }
        }
        Ok((key, &self.buf))
    }

    /// Encodes `feature` into a new [`SortableFeature`] (Java `RenderedFeatureEncoder.apply`).
    pub fn apply(&mut self, feature: &RenderedFeature) -> anyhow::Result<SortableFeature> {
        let (key, value) = self.encode(feature)?;
        Ok(SortableFeature::new(key, value.to_vec()))
    }
}

fn encode_value(
    shared: &Shared,
    strings: &mut StringCache,
    feature: &vector_tile::Feature,
    group: Option<&Group>,
    buf: &mut Vec<u8>,
) -> anyhow::Result<()> {
    use rmp::encode::*;
    buf.clear();
    // hasGroup bit in key will tell consumers whether they need to decode group info from value
    if let Some(group) = group {
        put_sint(buf, group.group);
        put_sint(buf, group.limit as i64);
    }
    put_sint(buf, feature.id);
    put_sint(buf, encode_geom_type_and_scale(&feature.geometry) as i8 as i64);
    let attrs = &feature.tags;
    let count = attrs.values().filter(|v| !v.is_null()).count();
    write_map_len(buf, count as u32)?;
    for (key, value) in attrs.iter() {
        if value.is_null() {
            continue;
        }
        put_sint(buf, strings.key(shared, key)? as i64);
        match value {
            Value::String(s) => write_str(buf, s)?,
            Value::Int(i) => put_sint(buf, *i),
            Value::Float(f) => write_f64(buf, *f as f64)?,
            Value::Double(d) => write_f64(buf, *d)?,
            Value::Bool(b) => write_bool(buf, *b)?,
            other => write_str(buf, &other.to_string())?,
        }
    }
    // Use the same binary format for encoding geometries in output vector tiles. Benchmarking showed it was faster
    // and smaller for encoding/decoding intermediate geometries than alternatives like WKB.
    let commands = feature.geometry.commands();
    write_array_len(buf, commands.len() as u32)?;
    buf.reserve(commands.len() * 3);
    for &command in commands {
        put_sint(buf, command as i64);
    }
    Ok(())
}

/// Appends `val` to `buf` in the smallest msgpack integer encoding (byte-identical to `rmp::encode::write_sint`).
#[inline]
fn put_sint(buf: &mut Vec<u8>, val: i64) {
    if (0..128).contains(&val) {
        buf.push(val as u8);
    } else if (-32..0).contains(&val) {
        buf.push(val as i8 as u8);
    } else if val < 0 {
        if val >= -128 {
            buf.extend_from_slice(&[0xd0, val as i8 as u8]);
        } else if val >= -32768 {
            buf.push(0xd1);
            buf.extend_from_slice(&(val as i16).to_be_bytes());
        } else if val >= -2147483648 {
            buf.push(0xd2);
            buf.extend_from_slice(&(val as i32).to_be_bytes());
        } else {
            buf.push(0xd3);
            buf.extend_from_slice(&val.to_be_bytes());
        }
    } else if val < 256 {
        buf.extend_from_slice(&[0xcc, val as u8]);
    } else if val < 65536 {
        buf.push(0xcd);
        buf.extend_from_slice(&(val as u16).to_be_bytes());
    } else if val < 4294967296 {
        buf.push(0xce);
        buf.extend_from_slice(&(val as u32).to_be_bytes());
    } else {
        buf.push(0xcf);
        buf.extend_from_slice(&(val as u64).to_be_bytes());
    }
}

/// Minimal zero-copy msgpack reader for the values written by [`encode_value`].
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    #[inline]
    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        anyhow::ensure!(self.buf.len() >= n, "unexpected end of msgpack value");
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    #[inline]
    fn u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.take(1)?[0])
    }

    #[inline]
    fn be<const N: usize>(&mut self) -> anyhow::Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }

    /// Reads `n` ints (as written by [`put_sint`] for geometry commands, which fit in an `i32`) into `out`.
    #[inline]
    fn ints_into(&mut self, n: usize, out: &mut Vec<i32>) -> anyhow::Result<()> {
        let mut buf = self.buf;
        for _ in 0..n {
            let (value, len) = match *buf {
                [m @ 0x00..=0x7f, ..] => (m as i32, 1),
                [m @ 0xe0..=0xff, ..] => (m as i8 as i32, 1),
                [0xcc, b, ..] => (b as i32, 2),
                [0xcd, b0, b1, ..] => (u16::from_be_bytes([b0, b1]) as i32, 3),
                [0xd0, b, ..] => (b as i8 as i32, 2),
                [0xd1, b0, b1, ..] => (i16::from_be_bytes([b0, b1]) as i32, 3),
                _ => {
                    self.buf = buf;
                    out.push(self.int()? as i32);
                    buf = self.buf;
                    continue;
                }
            };
            out.push(value);
            buf = &buf[len..];
        }
        self.buf = buf;
        Ok(())
    }

    fn int_from_marker(&mut self, m: u8) -> anyhow::Result<Option<i64>> {
        Ok(Some(match m {
            0x00..=0x7f => m as i64,
            0xe0..=0xff => m as i8 as i64,
            0xcc => self.u8()? as i64,
            0xcd => u16::from_be_bytes(self.be()?) as i64,
            0xce => u32::from_be_bytes(self.be()?) as i64,
            0xcf => u64::from_be_bytes(self.be()?) as i64,
            0xd0 => self.u8()? as i8 as i64,
            0xd1 => i16::from_be_bytes(self.be()?) as i64,
            0xd2 => i32::from_be_bytes(self.be()?) as i64,
            0xd3 => i64::from_be_bytes(self.be()?),
            _ => return Ok(None),
        }))
    }

    #[inline]
    fn int(&mut self) -> anyhow::Result<i64> {
        let m = self.u8()?;
        self.int_from_marker(m)?.ok_or_else(|| anyhow::anyhow!("expected msgpack int, got marker {m:#x}"))
    }

    fn len(&mut self, m: u8, fix_lo: u8, fix_hi: u8, m16: u8, m32: u8) -> anyhow::Result<usize> {
        if (fix_lo..=fix_hi).contains(&m) {
            Ok((m - fix_lo) as usize)
        } else if m == m16 {
            Ok(u16::from_be_bytes(self.be()?) as usize)
        } else if m == m32 {
            Ok(u32::from_be_bytes(self.be()?) as usize)
        } else {
            anyhow::bail!("unexpected msgpack marker {m:#x}")
        }
    }

    fn map_len(&mut self) -> anyhow::Result<usize> {
        let m = self.u8()?;
        self.len(m, 0x80, 0x8f, 0xde, 0xdf)
    }

    fn array_len(&mut self) -> anyhow::Result<usize> {
        let m = self.u8()?;
        self.len(m, 0x90, 0x9f, 0xdc, 0xdd)
    }

    /// Reads any value, returning `None` for values the decoder ignores (nil).
    fn value(&mut self) -> anyhow::Result<Option<Value>> {
        let m = self.u8()?;
        if let Some(i) = self.int_from_marker(m)? {
            return Ok(Some(Value::Int(i)));
        }
        let str_len = match m {
            0xa0..=0xbf => Some((m - 0xa0) as usize),
            0xd9 => Some(self.u8()? as usize),
            0xda => Some(u16::from_be_bytes(self.be()?) as usize),
            0xdb => Some(u32::from_be_bytes(self.be()?) as usize),
            _ => None,
        };
        if let Some(n) = str_len {
            let bytes = self.take(n)?;
            return Ok(Some(Value::String(Arc::from(std::str::from_utf8(bytes)?))));
        }
        Ok(match m {
            0xc0 => None,
            0xc2 => Some(Value::Bool(false)),
            0xc3 => Some(Value::Bool(true)),
            0xca => Some(Value::Double(f32::from_be_bytes(self.be()?) as f64)),
            0xcb => Some(Value::Double(f64::from_be_bytes(self.be()?))),
            _ => anyhow::bail!("unsupported msgpack marker {m:#x}"),
        })
    }
}

fn peek_at_group_info(encoded: &[u8]) -> anyhow::Result<Group> {
    let mut r = Reader { buf: encoded };
    let group = r.int()?;
    let limit = r.int()? as i32;
    Ok(Group { group, limit })
}

impl crate::util::file_utils::DiskBacked for FeatureGroup {
    fn disk_usage_bytes(&self) -> u64 {
        FeatureGroup::disk_usage_bytes(self)
    }
}

/// Encodes rendered features and writes them to a per-thread sort writer.
pub struct FeatureWriter<'a> {
    encoder: RenderedFeatureEncoder,
    writer: Box<dyn FeatureSortWriter + 'a>,
}

impl FeatureWriter<'_> {
    /// Encodes and writes `feature`.
    #[inline]
    pub fn write(&mut self, feature: &RenderedFeature) -> anyhow::Result<()> {
        let (key, value) = self.encoder.encode(feature)?;
        self.writer.accept_parts(key, value)
    }

    /// The encoder, for callers that want to encode features themselves.
    pub fn encoder(&mut self) -> &mut RenderedFeatureEncoder {
        &mut self.encoder
    }

    /// Flushes and closes the underlying writer.
    pub fn close(mut self) -> anyhow::Result<()> {
        self.writer.close()
    }
}

/// Destination for encoded features in [`process_and_write`]: either a sort writer (inline writes) or a queue to
/// writer threads.
pub trait FeatureSink {
    fn accept_parts(&mut self, key: i64, value: &[u8]) -> anyhow::Result<()>;

    /// Encodes `feature` with `encoder` and accepts it.
    fn write(&mut self, encoder: &mut RenderedFeatureEncoder, feature: &RenderedFeature) -> anyhow::Result<()> {
        let (key, value) = encoder.encode(feature)?;
        self.accept_parts(key, value)
    }
}

impl FeatureSink for Box<dyn FeatureSortWriter + '_> {
    #[inline]
    fn accept_parts(&mut self, key: i64, value: &[u8]) -> anyhow::Result<()> {
        FeatureSortWriter::accept_parts(&mut **self, key, value)
    }
}

impl FeatureSink for WorkQueueWriter<SortableFeature> {
    #[inline]
    fn accept_parts(&mut self, key: i64, value: &[u8]) -> anyhow::Result<()> {
        self.accept(SortableFeature::new(key, value.to_vec()));
        Ok(())
    }
}

impl FeatureSink for Vec<SortableFeature> {
    fn accept_parts(&mut self, key: i64, value: &[u8]) -> anyhow::Result<()> {
        self.push(SortableFeature::new(key, value.to_vec()));
        Ok(())
    }
}

/// Port of Java's `WorkerPipeline.Builder.processAndWrite`: runs `handler` in `process_threads` threads and, when
/// `inline_writes` is true, writes features to temp storage directly from the process threads, or when false sends
/// them through a queue to `write_threads` writer threads.
///
/// `handler` receives the input items and a [`FeatureSink`]; create a [`RenderedFeatureEncoder`] per thread with
/// `group.new_rendered_feature_encoder()` and call [`FeatureSink::write`]. After the returned pipeline finishes, drop
/// it so `group` is uniquely owned again before calling [`FeatureGroup::prepare`].
pub fn process_and_write<O, F>(
    builder: Builder<O>,
    inline_writes: bool,
    process_threads: usize,
    write_threads: usize,
    group: Arc<FeatureGroup>,
    handler: F,
) -> WorkerPipeline
where
    O: Send + 'static,
    F: Fn(&mut WorkQueueReader<O>, &mut dyn FeatureSink) -> anyhow::Result<()> + Send + Sync + 'static,
{
    if inline_writes {
        builder.sink_to("process", process_threads, move |prev| {
            let mut writer = group.writer_for_thread()?;
            handler(prev, &mut writer)?;
            writer.close()
        })
    } else {
        builder
            .add_worker("process", process_threads, move |prev, next: &mut WorkQueueWriter<SortableFeature>| {
                handler(prev, next)
            })
            .add_buffer_batched("write_queue", 50_000, 1_000)
            .sink_to("write", write_threads, move |prev| {
                let mut writer = group.writer_for_thread()?;
                for item in prev {
                    writer.accept(item)?;
                }
                writer.close()
            })
    }
}

/// Iterator over the features in each tile, in tile order.
pub struct TileFeaturesIter {
    shared: Arc<Shared>,
    entries: FeatureIter,
    pending: Option<SortableFeature>,
    reader_threads: Vec<JoinHandle<()>>,
}

impl TileFeaturesIter {
    fn new(shared: Arc<Shared>, mut entries: FeatureIter, reader_threads: Vec<JoinHandle<()>>) -> TileFeaturesIter {
        let pending = entries.next();
        TileFeaturesIter { shared, entries, pending, reader_threads }
    }

    /// Waits for any parallel reader threads to finish, returning an error if one panicked.
    pub fn join(self) -> anyhow::Result<()> {
        let TileFeaturesIter { entries, reader_threads, .. } = self;
        drop(entries);
        for handle in reader_threads {
            if handle.join().is_err() {
                anyhow::bail!("feature reader thread panicked");
            }
        }
        Ok(())
    }
}

impl Iterator for TileFeaturesIter {
    type Item = TileFeatures;

    fn next(&mut self) -> Option<TileFeatures> {
        // entries are sorted by tile ID, so group consecutive entries in same tile into tiles
        let first = self.pending.take()?;
        let tile_id = extract_tile_from_key(first.key);
        let mut result = TileFeatures::new(self.shared.clone(), tile_id);
        result.add(first);
        for next in self.entries.by_ref() {
            if extract_tile_from_key(next.key) != tile_id {
                self.pending = Some(next);
                break;
            }
            result.add(next);
        }
        Some(result)
    }
}

/// Features contained in a single tile.
pub struct TileFeatures {
    shared: Arc<Shared>,
    tile_coord: TileCoord,
    entries: Vec<SortableFeature>,
    num_features_processed: u64,
    counts: Option<HashMap<i64, i64>>,
    last_layer: u8,
}

impl std::fmt::Debug for TileFeatures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TileFeatures{{tile={:?}, num entries={}}}", self.tile_coord, self.entries.len())
    }
}

impl TileFeatures {
    fn new(shared: Arc<Shared>, tile_id: u64) -> TileFeatures {
        let tile_coord = shared.tile_order.decode(tile_id);
        TileFeatures {
            shared,
            tile_coord,
            entries: Vec::new(),
            num_features_processed: 0,
            counts: None,
            last_layer: u8::MAX,
        }
    }

    /// Returns the number of features read including features discarded from being over the limit in a group.
    pub fn num_features_processed(&self) -> u64 {
        self.num_features_processed
    }

    /// Returns the number of features to output, excluding features discarded from being over the limit in a group.
    pub fn num_features_to_emit(&self) -> u64 {
        self.entries.len() as u64
    }

    pub fn tile_coord(&self) -> TileCoord {
        self.tile_coord
    }

    /// The raw encoded entries in this tile.
    pub fn entries(&self) -> &[SortableFeature] {
        &self.entries
    }

    /// Returns true if `other` contains features with identical layer, geometry, and attributes as this tile - even
    /// if the tiles have separate coordinates.
    ///
    /// Used as an optimization to avoid re-encoding the same ocean tiles over and over again.
    pub fn has_same_contents(&self, other: Option<&TileFeatures>) -> bool {
        let Some(other) = other else { return false };
        other.entries.len() == self.entries.len()
            && self.entries.iter().zip(&other.entries).all(|(a, b)| {
                extract_layer_id_from_key(a.key) == extract_layer_id_from_key(b.key) && a.value == b.value
            })
    }

    fn decode_vector_tile_feature(&self, entry: &SortableFeature) -> anyhow::Result<vector_tile::Feature> {
        let mut r = Reader { buf: &entry.value };
        let group = if extract_has_group_from_key(entry.key) {
            let group = r.int()?;
            r.int()?; // groupLimit - features over the limit were already discarded
            group
        } else {
            vector_tile::Feature::NO_GROUP
        };
        let id = r.int()?;
        let geom_type_and_scale = r.int()? as u8;
        let geom_type = decode_geom_type(geom_type_and_scale);
        let scale = decode_scale(geom_type_and_scale);
        let map_size = r.map_len()?;
        let mut attrs = crate::value::tags_with_capacity(map_size);
        let strings = &self.shared.common_value_strings;
        for _ in 0..map_size {
            let key = strings.decode(r.int()? as u32)?;
            if let Some(value) = r.value()? {
                attrs.insert(key.to_string(), value);
            }
        }
        let command_size = r.array_len()?;
        let mut commands = Vec::with_capacity(command_size);
        r.ints_into(command_size, &mut commands)?;
        let layer = self.shared.common_layer_strings.decode(extract_layer_id_from_key(entry.key))?;
        Ok(vector_tile::Feature {
            layer: layer.to_string(),
            id,
            geometry: VectorGeometry::new(commands, geom_type, scale),
            tags: FromTags::from_tags(attrs),
            group,
        })
    }

    /// Decodes the features in this tile, applies profile post-processing, and returns the vector tile.
    pub fn get_vector_tile(&self) -> VectorTile {
        self.get_vector_tile_with_stats(None)
    }

    /// Like [`TileFeatures::get_vector_tile`], recording layer attribute stats into `layer_stats`.
    pub fn get_vector_tile_with_stats(&self, layer_stats: Option<&layer_attr_stats::Updater>) -> VectorTile {
        let track = self.shared.config.log_jts_exceptions;
        if track {
            CURRENT_TILE.with(|c| c.set(Some(self.tile_coord)));
        }
        let result = self.build_vector_tile(layer_stats);
        if track {
            CURRENT_TILE.with(|c| c.set(None));
        }
        result
    }

    fn build_vector_tile(&self, layer_stats: Option<&layer_attr_stats::Updater>) -> VectorTile {
        let mut tile = VectorTile::new();
        if let Some(stats) = layer_stats {
            tile.track_layer_stats(stats.for_zoom(self.tile_coord.z));
        }
        let mut layer_features: LayerFeatures = LayerFeatures::new();
        let mut current: Option<(u8, Vec<vector_tile::Feature>)> = None;
        for entry in &self.entries {
            let feature = match self.decode_vector_tile_feature(entry) {
                Ok(f) => f,
                Err(e) => panic!("Error decoding feature in {:?}: {e:#}", self.tile_coord),
            };
            let layer_id = extract_layer_id_from_key(entry.key);
            match &mut current {
                Some((id, items)) if *id == layer_id => items.push(feature),
                _ => {
                    if let Some((_, items)) = current.take()
                        && let Some(first) = items.first()
                    {
                        layer_features.entry(first.layer.clone()).or_default().extend(items);
                    }
                    current = Some((layer_id, vec![feature]));
                }
            }
        }
        if let Some((_, items)) = current.take()
            && let Some(first) = items.first()
        {
            layer_features.entry(first.layer.clone()).or_default().extend(items);
        }

        // first post-process entire tile by invoking post_process_tile_features to allow for post-processing that
        // combines features across different layers, infers new layers, or removes layers
        if let Err(e) = self.shared.profile.post_process_tile_features(self.tile_coord, &mut layer_features) {
            self.handle_post_process_failure(&e, "entire tile");
        }
        // then let profiles post-process each layer in isolation with post_process_layer_features
        for (layer, mut features) in layer_features {
            if features.is_empty() {
                continue;
            }
            if let Err(e) = self.shared.profile.post_process_layer_features(&layer, self.tile_coord.z, &mut features) {
                self.handle_post_process_failure(&e, &layer);
            }
            // lines are stored using a higher precision so that rounding does not introduce artificial
            // intersections between endpoints to confuse line merging, so we have to reduce the precision here, now
            // that line merging is done. Also remove points more than --max-point-buffer pixels outside the tile if
            // the user has requested a narrower buffer than the profile provides by default.
            let had_features = !features.is_empty();
            unscale_and_remove_points_outside_buffer(&mut features, self.shared.config.max_point_buffer);
            if had_features && features.is_empty() {
                // Java nulls out the removed features but still adds the (now featureless) layer, so the tile is
                // written (empty) instead of being skipped
                tile.add_empty_layer(&layer);
            }
            tile.add_layer_features(&layer, &features);
        }
        tile
    }

    fn handle_post_process_failure(&self, e: &GeometryException, entity: &str) {
        // failures in tile post-processing happen very late so err on the side of caution and log failures
        e.log_with_details(
            &*self.shared.stats,
            "postprocess_layer",
            &format!("Caught error postprocessing features for {entity} on {:?}", self.tile_coord),
            self.shared.config.log_jts_exceptions,
        );
    }

    fn add(&mut self, entry: SortableFeature) {
        self.num_features_processed += 1;
        let key = entry.key;
        if extract_has_group_from_key(key) {
            let this_layer = extract_layer_id_from_key(key);
            let counts = match &mut self.counts {
                None => {
                    self.last_layer = this_layer;
                    self.counts.insert(HashMap::new())
                }
                Some(counts) => {
                    if this_layer != self.last_layer {
                        self.last_layer = this_layer;
                        counts.clear();
                    }
                    counts
                }
            };
            let group_info = match peek_at_group_info(&entry.value) {
                Ok(g) => g,
                Err(e) => {
                    error!("Error decoding group info in {:?}: {e:#}", self.tile_coord);
                    return;
                }
            };
            let old = counts.get(&group_info.group).copied().unwrap_or(0);
            if group_info.limit > 0 && old >= group_info.limit as i64 {
                // discard if there are too many features in this group already
                return;
            }
            counts.insert(group_info.group, old + 1);
        }
        self.entries.push(entry);
    }
}

fn unscale_and_remove_points_outside_buffer(features: &mut Vec<vector_tile::Feature>, max_point_buffer: f64) {
    let check_points = (-128.0..=256.0).contains(&max_point_buffer);
    features.retain_mut(|feature| {
        let geometry = &feature.geometry;
        let mut replacement = None;
        if geometry.scale() != 0 {
            replacement = Some(geometry.unscale());
        }
        let current = replacement.as_ref().unwrap_or(geometry);
        if check_points && current.geom_type() == GeometryType::Point && !current.is_empty() {
            replacement = Some(current.filter_points_outside_buffer(max_point_buffer));
        }
        if let Some(g) = replacement {
            feature.geometry = g;
        }
        !feature.geometry.is_empty()
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use geo_types::Geometry;

    use super::*;
    use crate::feature_collector::FeatureCollector;
    use crate::geo::geometry_exception::GeometryResult;
    use crate::geo::test_utils::*;
    use crate::profile::NullProfile;
    use crate::reader::source_feature::SourceFeature;
    use crate::stats::stats::in_memory;

    #[test]
    fn test_put_sint_matches_rmp() {
        let mut values = vec![0i64, 1, -1, i64::MIN, i64::MAX];
        for b in [7u32, 8, 15, 16, 31, 32, 63] {
            let p = 1i64 << b;
            values.extend([p - 1, p, p + 1, -p - 1, -p, -p + 1]);
        }
        values.extend([-32, -33, 127, 128, 255, 256, -128, -129]);
        for v in values {
            let mut a = Vec::new();
            let mut b = Vec::new();
            put_sint(&mut a, v);
            rmp::encode::write_sint(&mut b, v).unwrap();
            assert_eq!(a, b, "{v}");
        }
    }
    use crate::tags;

    trait OwnedTags {
        fn owned(&self) -> Tags;
    }
    impl OwnedTags for Tags {
        fn owned(&self) -> Tags {
            self.clone()
        }
    }
    impl OwnedTags for Arc<Tags> {
        fn owned(&self) -> Tags {
            (**self).clone()
        }
    }
    fn owned_tags(t: &impl OwnedTags) -> Tags {
        t.owned()
    }

    type Decoded = BTreeMap<u64, BTreeMap<String, Vec<(Tags, Geometry<f64>)>>>;

    struct Harness {
        features: FeatureGroup,
        id: i64,
        written: Vec<SortableFeature>,
    }

    fn config() -> Arc<PlanetilerConfig> {
        Arc::new(PlanetilerConfig::defaults())
    }

    impl Harness {
        fn new() -> Harness {
            Self::with(TileOrder::Tms, Arc::new(NullProfile))
        }

        fn with(order: TileOrder, profile: Arc<dyn Profile>) -> Harness {
            Harness {
                features: FeatureGroup::new_in_memory(order, profile, config(), in_memory()),
                id: 0,
                written: vec![],
            }
        }

        fn put(&mut self, tile: u64, layer: &str, attrs: Tags, geom: impl Into<Geometry<f64>>) {
            self.put_with_sort_key(tile, layer, attrs, geom, 0);
        }

        fn put_with_sort_key(&mut self, tile: u64, layer: &str, attrs: Tags, geom: impl Into<Geometry<f64>>, sk: i32) {
            let id = self.id;
            self.id += 1;
            self.put_full(id, TileCoord::decode(tile), layer, attrs, geom.into(), sk, None);
        }

        #[allow(clippy::too_many_arguments)]
        fn put_with_group(
            &mut self,
            tile: u64,
            layer: &str,
            attrs: Tags,
            geom: impl Into<Geometry<f64>>,
            sk: i32,
            group: i64,
            limit: i32,
        ) {
            let id = self.id;
            self.id += 1;
            self.put_full(id, TileCoord::decode(tile), layer, attrs, geom.into(), sk, Some(Group { group, limit }));
        }

        #[allow(clippy::too_many_arguments)]
        fn put_full(
            &mut self,
            id: i64,
            tile: TileCoord,
            layer: &str,
            attrs: Tags,
            geom: Geometry<f64>,
            sort_key: i32,
            group: Option<Group>,
        ) {
            let feature = RenderedFeature::new(
                tile,
                vector_tile::Feature::new(layer, id, vector_tile::encode_geometry(&geom), attrs),
                sort_key,
                group,
            );
            let encoded = self.features.new_rendered_feature_encoder().apply(&feature).unwrap();
            let mut writer = self.features.writer_for_thread().unwrap();
            writer.accept(encoded.clone()).unwrap();
            writer.close().unwrap();
            self.written.push(encoded);
        }

        fn put_args(&mut self, a: &PutArgs) {
            let group = a.has_group.then_some(Group { group: a.group, limit: a.limit });
            self.put_full(a.id, TileCoord::decode(a.tile), a.layer, a.attrs.clone(), a.geom.clone(), a.sort_key, group);
        }

        fn decode_tiles(iter: impl Iterator<Item = TileFeatures>) -> Decoded {
            let mut map = Decoded::new();
            for tile in iter {
                for feature in vector_tile::decode(&tile.get_vector_tile().encode()).unwrap() {
                    map.entry(tile.tile_coord().encoded)
                        .or_default()
                        .entry(feature.layer.clone())
                        .or_default()
                        .push((owned_tags(&feature.tags), feature.geometry.decode().unwrap()));
                }
            }
            map
        }

        fn get_features(&mut self) -> Decoded {
            Self::decode_tiles(self.features.iter().unwrap())
        }

        fn get_features_parallel(&mut self) -> Decoded {
            let mut iter = self.features.parallel_iterator(2).unwrap();
            let result = Self::decode_tiles(iter.by_ref());
            iter.join().unwrap();
            result
        }
    }

    fn pt(x: f64, y: f64) -> Geometry<f64> {
        Geometry::Point(new_point(x, y))
    }

    type TileLayers<'a> = Vec<(&'a str, Vec<(Tags, Geometry<f64>)>)>;

    fn decoded(items: Vec<(u64, TileLayers<'_>)>) -> Decoded {
        items
            .into_iter()
            .map(|(tile, layers)| (tile, layers.into_iter().map(|(l, f)| (l.to_string(), f)).collect()))
            .collect()
    }

    #[test]
    fn test_empty() {
        let mut h = Harness::new();
        assert!(h.features.iter().unwrap().next().is_none());
    }

    fn put_points(h: &mut Harness) {
        h.put(3, "layer3", tags! {"a" => 1.5, "b" => "string"}, new_point(5.0, 6.0));
        h.put(3, "layer4", tags! {"a" => 1.5, "b" => "string"}, new_point(5.0, 6.0));
        h.put(2, "layer", tags! {"a" => 1.5, "b" => "string"}, new_point(5.0, 6.0));
        h.put(1, "layer", tags! {"a" => 1, "b" => 2i64}, new_point(1.0, 2.0));
        h.put(1, "layer2", tags! {"c" => 3.0, "d" => true}, new_point(3.0, 4.0));
    }

    fn expected_points() -> Decoded {
        decoded(vec![
            (
                1,
                vec![
                    ("layer", vec![(tags! {"a" => 1i64, "b" => 2i64}, pt(1.0, 2.0))]),
                    ("layer2", vec![(tags! {"c" => 3.0, "d" => true}, pt(3.0, 4.0))]),
                ],
            ),
            (2, vec![("layer", vec![(tags! {"a" => 1.5, "b" => "string"}, pt(5.0, 6.0))])]),
            (
                3,
                vec![
                    ("layer3", vec![(tags! {"a" => 1.5, "b" => "string"}, pt(5.0, 6.0))]),
                    ("layer4", vec![(tags! {"a" => 1.5, "b" => "string"}, pt(5.0, 6.0))]),
                ],
            ),
        ])
    }

    #[test]
    fn test_put_points() {
        let mut h = Harness::new();
        put_points(&mut h);
        assert_eq!(expected_points(), h.get_features());
    }

    #[test]
    fn test_sharded_read() {
        let mut h = Harness::new();
        put_points(&mut h);
        assert_eq!(expected_points(), h.get_features_parallel());
    }

    #[test]
    fn test_put_points_with_sort_key() {
        let mut h = Harness::new();
        h.put_with_sort_key(1, "layer", tags! {"id" => 1}, new_point(1.0, 2.0), 2);
        h.put_with_sort_key(1, "layer", tags! {"id" => 2}, new_point(3.0, 4.0), 1);
        assert_eq!(
            decoded(vec![(
                1,
                vec![(
                    "layer",
                    // order reversed because of sort-key
                    vec![(tags! {"id" => 2i64}, pt(3.0, 4.0)), (tags! {"id" => 1i64}, pt(1.0, 2.0))]
                )]
            )]),
            h.get_features()
        );
    }

    #[test]
    fn test_limit_points() {
        let mut h = Harness::new();
        h.put_with_group(1, "layer", tags! {"id" => 3}, new_point(5.0, 6.0), 2, 1, 2);
        h.put_with_group(1, "layer", tags! {"id" => 2}, new_point(3.0, 4.0), 1, 1, 2);
        h.put_with_group(1, "layer", tags! {"id" => 1}, new_point(1.0, 2.0), 0, 1, 2);
        assert_eq!(
            decoded(vec![(
                1,
                vec![(
                    "layer",
                    // id=3 omitted because past limit, sorted by sortKey ascending
                    vec![(tags! {"id" => 1i64}, pt(1.0, 2.0)), (tags! {"id" => 2i64}, pt(3.0, 4.0))]
                )]
            )]),
            h.get_features()
        );
    }

    #[test]
    fn test_limit_points_in_different_groups() {
        let mut h = Harness::new();
        h.put_with_group(1, "layer", tags! {"id" => 3}, new_point(5.0, 6.0), 0, 2, 2);
        h.put_with_group(1, "layer", tags! {"id" => 1}, new_point(1.0, 2.0), 2, 1, 2);
        h.put_with_group(1, "layer", tags! {"id" => 2}, new_point(3.0, 4.0), 1, 1, 2);
        assert_eq!(
            decoded(vec![(
                1,
                vec![(
                    "layer",
                    vec![
                        (tags! {"id" => 3i64}, pt(5.0, 6.0)),
                        (tags! {"id" => 2i64}, pt(3.0, 4.0)),
                        (tags! {"id" => 1i64}, pt(1.0, 2.0)),
                    ]
                )]
            )]),
            h.get_features()
        );
    }

    #[test]
    fn test_dont_limit_points_with_group() {
        let mut h = Harness::new();
        h.put_with_group(1, "layer", tags! {"id" => 3}, new_point(5.0, 6.0), 0, 1, 0);
        h.put_with_group(1, "layer", tags! {"id" => 1}, new_point(1.0, 2.0), 2, 1, 0);
        h.put_with_group(1, "layer", tags! {"id" => 2}, new_point(3.0, 4.0), 1, 1, 0);
        assert_eq!(
            decoded(vec![(
                1,
                vec![(
                    "layer",
                    vec![
                        (tags! {"id" => 3i64}, pt(5.0, 6.0)),
                        (tags! {"id" => 2i64}, pt(3.0, 4.0)),
                        (tags! {"id" => 1i64}, pt(1.0, 2.0)),
                    ]
                )]
            )]),
            h.get_features()
        );
    }

    struct ReversingProfile;
    impl Profile for ReversingProfile {
        fn process_feature(&self, _source_feature: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {}
        fn post_process_layer_features(
            &self,
            _layer: &str,
            _zoom: u32,
            items: &mut Vec<vector_tile::Feature>,
        ) -> GeometryResult<()> {
            items.reverse();
            Ok(())
        }
    }

    #[test]
    fn test_profile_changes_geometry() {
        let mut h = Harness::with(TileOrder::Tms, Arc::new(ReversingProfile));
        h.put_with_group(1, "layer", tags! {"id" => 3}, new_point(5.0, 6.0), 2, 1, 2);
        h.put_with_group(1, "layer", tags! {"id" => 1}, new_point(1.0, 2.0), 0, 1, 2);
        h.put_with_group(1, "layer", tags! {"id" => 2}, new_point(3.0, 4.0), 1, 1, 2);
        assert_eq!(
            decoded(vec![(
                1,
                vec![(
                    "layer",
                    // not sorted by sortKey asc because profile reversed it
                    vec![(tags! {"id" => 2i64}, pt(3.0, 4.0)), (tags! {"id" => 1i64}, pt(1.0, 2.0))]
                )]
            )]),
            h.get_features()
        );
    }

    struct FailingProfile;
    impl Profile for FailingProfile {
        fn process_feature(&self, _source_feature: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {}
        fn post_process_layer_features(
            &self,
            _layer: &str,
            _zoom: u32,
            items: &mut Vec<vector_tile::Feature>,
        ) -> GeometryResult<()> {
            items.pop();
            Err(GeometryException::non_fatal("failed", "failed"))
        }
        fn post_process_tile_features(&self, _tile: TileCoord, layers: &mut LayerFeatures) -> GeometryResult<()> {
            layers.remove("removed");
            Err(GeometryException::non_fatal("failed", "failed"))
        }
    }

    #[test]
    fn test_post_process_failure_keeps_features() {
        let mut h = Harness::with(TileOrder::Tms, Arc::new(FailingProfile));
        h.put(1, "layer", tags! {"id" => 1}, new_point(1.0, 2.0));
        h.put(1, "layer", tags! {"id" => 2}, new_point(3.0, 4.0));
        h.put(1, "removed", tags! {"id" => 3}, new_point(3.0, 4.0));
        assert_eq!(decoded(vec![(1, vec![("layer", vec![(tags! {"id" => 1i64}, pt(1.0, 2.0))])])]), h.get_features());
    }

    fn tile_order_of(order: TileOrder) -> Vec<(u32, u32)> {
        let mut h = Harness::with(order, Arc::new(NullProfile));
        for i in 1..=4 {
            h.put(i, "layer", tags! {"id" => i as i64}, new_point(0.0, 0.0));
        }
        h.features.iter().unwrap().map(|t| (t.tile_coord().x, t.tile_coord().y)).collect()
    }

    #[test]
    fn test_hilbert_ordering() {
        // Hilbert tile IDs at zoom level 1:
        // 1 4
        // 2 3
        assert_eq!(vec![(0, 0), (0, 1), (1, 1), (1, 0)], tile_order_of(TileOrder::Hilbert));
    }

    #[test]
    fn test_tms_ordering() {
        // TMS tile IDs at zoom level 1:
        // 2 4
        // 1 3
        assert_eq!(vec![(0, 1), (0, 0), (1, 1), (1, 0)], tile_order_of(TileOrder::Tms));
    }

    #[test]
    fn test_encode_long_key() {
        let tiles = [
            TileCoord::of_xyz(0, 0, 0),
            TileCoord::of_xyz(0, 0, 7),
            TileCoord::of_xyz((1 << 7) - 1, (1 << 7) - 1, 7),
            TileCoord::of_xyz(0, 0, 14),
            TileCoord::of_xyz((1 << 14) - 1, (1 << 14) - 1, 14),
            TileCoord::of_xyz(0, 0, 15),
            TileCoord::of_xyz((1 << 15) - 1, (1 << 15) - 1, 15),
            TileCoord::of_xyz(1, 0, 15),
            TileCoord::of_xyz(0, 1, 15),
            TileCoord::of_xyz(0, 0, 16),
            TileCoord::of_xyz((1 << 16) - 1, (1 << 16) - 1, 16),
            TileCoord::of_xyz(32768, 32768, 16),
            TileCoord::of_xyz(1, 0, 16),
            TileCoord::of_xyz(0, 1, 16),
            TileCoord::of_xyz((1 << 16) - 1, 0, 16),
            TileCoord::of_xyz(0, (1 << 16) - 1, 16),
        ];
        for tile in tiles {
            for layer in [0u8, 1, 255] {
                for sort_key in [SORT_KEY_MIN, 0, SORT_KEY_MAX] {
                    for has_group in [false, true] {
                        let key = encode_key(tile.encoded as i64, layer, sort_key, has_group);
                        let msg = format!("{tile:?} {layer} {sort_key} {has_group}");
                        assert_eq!(tile.encoded, extract_tile_from_key(key), "tile {msg}");
                        assert_eq!(layer, extract_layer_id_from_key(key), "layer {msg}");
                        assert_eq!(sort_key, extract_sort_key_from_key(key), "sortKey {msg}");
                        assert_eq!(has_group, extract_has_group_from_key(key), "hasGroup {msg}");
                    }
                }
            }
        }
    }

    #[test]
    fn test_encode_long_key_ordering() {
        type KeyArgs = (i64, u8, i32, bool);
        let cases: [(KeyArgs, KeyArgs); 8] = [
            ((0, 0, -2, true), (0, 0, -1, false)),
            ((0, 0, 1, false), (0, 0, 2, false)),
            ((0, 0, -1, false), (0, 0, 1, false)),
            ((-1, 0, -2, false), (-1, 0, -1, false)),
            ((-1, 0, 1, false), (-1, 0, 2, false)),
            ((-1, 0, -1, false), (-1, 0, 1, false)),
            ((-1, 0, -1, false), (-1, 0, -1, true)),
            ((1, 0, 1, false), (1, 0, 1, true)),
        ];
        for ((ta, la, sa, ga), (tb, lb, sb, gb)) in cases {
            let a = encode_key(ta, la, sa, ga) as u64;
            let b = encode_key(tb, lb, sb, gb) as u64;
            assert!(a < b, "{ta} {la} {sa} {ga} < {tb} {lb} {sb} {gb}");
        }
    }

    #[test]
    fn test_z16_high_tile_ids_sort_after_lower_zooms() {
        let key = |t: TileCoord| encode_key(t.encoded as i64, 0, 0, false);
        let k0 = key(TileCoord::of_xyz(0, 0, 0));
        let k14 = key(TileCoord::of_xyz(8192, 8192, 14));
        let k15 = key(TileCoord::of_xyz(16384, 16384, 15));
        let k16 = key(TileCoord::of_xyz(65535, 65535, 16));
        assert!((k0 as u64) < (k14 as u64), "z0 < z14");
        assert!((k14 as u64) < (k15 as u64), "z14 < z15");
        assert!((k15 as u64) < (k16 as u64), "z15 < z16");
        let f0 = SortableFeature::new(k0, vec![]);
        let f16 = SortableFeature::new(k16, vec![]);
        assert!(f0 < f16);
        assert!(f16 > f0);
    }

    #[test]
    fn test_z16_high_tile_iterates_after_lower_zoom_tiles() {
        let mut h = Harness::new();
        let z0 = TileCoord::of_xyz(0, 0, 0);
        let z16 = TileCoord::of_xyz(50_000, 500, 16);
        h.put_full(0, z16, "layer", tags! {}, pt(1.0, 2.0), 0, None);
        h.put_full(1, z0, "layer", tags! {}, pt(3.0, 4.0), 0, None);
        let order: Vec<u64> = h.features.iter().unwrap().map(|t| t.tile_coord().encoded).collect();
        assert_eq!(vec![z0.encoded, z16.encoded], order);
    }

    struct PutArgs {
        id: i64,
        tile: u64,
        layer: &'static str,
        attrs: Tags,
        geom: Geometry<f64>,
        sort_key: i32,
        has_group: bool,
        group: i64,
        limit: i32,
    }

    #[allow(clippy::too_many_arguments)]
    fn args(
        id: i64,
        tile: u64,
        layer: &'static str,
        attrs: Tags,
        geom: Geometry<f64>,
        sort_key: i32,
        has_group: bool,
        group: i64,
        limit: i32,
    ) -> PutArgs {
        PutArgs { id, tile, layer, attrs, geom, sort_key, has_group, group, limit }
    }

    fn same_feature_group_cases() -> Vec<(&'static str, bool, PutArgs, PutArgs)> {
        vec![
            (
                "same despite diff sort key",
                true,
                args(1, 1, "layer", tags! {"id" => 1}, pt(1.0, 2.0), 1, true, 2, 3),
                args(1, 2, "layer", tags! {"id" => 1}, pt(1.0, 2.0), 2, true, 2, 3),
            ),
            (
                "diff when geometry changes",
                false,
                args(1, 1, "layer", tags! {"id" => 1}, pt(1.0, 2.0), 1, true, 2, 3),
                args(1, 2, "layer", tags! {"id" => 1}, pt(1.0, 3.0), 1, true, 2, 3),
            ),
            (
                "diff when attrs changes",
                false,
                args(1, 1, "layer", tags! {"id" => 1}, pt(1.0, 2.0), 1, true, 2, 3),
                args(1, 2, "layer", tags! {"id" => 2}, pt(1.0, 2.0), 1, true, 2, 3),
            ),
            (
                "diff when layer changes",
                false,
                args(1, 1, "layer", tags! {"id" => 1}, pt(1.0, 2.0), 1, true, 2, 3),
                args(1, 2, "layer2", tags! {"id" => 1}, pt(1.0, 2.0), 1, true, 2, 3),
            ),
            (
                "diff when id changes",
                false,
                args(1, 1, "layer", tags! {"id" => 1}, pt(1.0, 2.0), 1, true, 2, 3),
                args(2, 2, "layer", tags! {"id" => 1}, pt(1.0, 2.0), 1, true, 2, 3),
            ),
        ]
    }

    #[test]
    fn test_has_same_contents() {
        for (name, expect_same, a0, a1) in same_feature_group_cases() {
            let mut h = Harness::new();
            h.put_args(&a0);
            h.put_args(&a1);
            let mut iter = h.features.iter().unwrap();
            let tile0 = iter.next().unwrap();
            let tile1 = iter.next().unwrap();
            assert_eq!(expect_same, tile0.has_same_contents(Some(&tile1)), "{name}");
            assert!(!tile0.has_same_contents(None));
        }
    }

    #[test]
    fn test_generate_content_hash() {
        // TileArchiveWriter.generateContentHash hashes the gzipped tile bytes; identical bytes <=> identical hash
        for (name, expect_same, a0, a1) in same_feature_group_cases() {
            let mut h = Harness::new();
            h.put_args(&a0);
            h.put_args(&a1);
            let mut iter = h.features.iter().unwrap();
            let t0 = crate::util::gzip::gzip(&iter.next().unwrap().get_vector_tile().encode()).unwrap();
            let t1 = crate::util::gzip::gzip(&iter.next().unwrap().get_vector_tile().encode()).unwrap();
            assert_eq!(expect_same, t0 == t1, "{name}");
        }
    }

    #[test]
    fn test_encode_decode_geometry_metadata() {
        for (geom_type, scale) in [
            (GeometryType::Unknown, 0),
            (GeometryType::Line, 2),
            (GeometryType::Polygon, 15),
            (GeometryType::Point, 14),
        ] {
            let encoded = encode_geom_type_and_scale(&VectorGeometry::new(Vec::<i32>::new(), geom_type, scale));
            assert_eq!(geom_type, decode_geom_type(encoded));
            assert_eq!(scale, decode_scale(encoded));
        }
    }

    #[test]
    fn test_msgpack_value_encoding_matches_java() {
        let h = Harness::new();
        let feature = RenderedFeature::new(
            TileCoord::decode(1),
            vector_tile::Feature::new(
                "layer",
                300,
                VectorGeometry::new(vec![9, 2, 4], GeometryType::Point, 17),
                tags! {"s" => "ab", "i" => -5, "f" => 1.5f32, "d" => 2.5, "b" => true, "n" => Value::Null},
            ),
            0,
            Some(Group { group: 1 << 40, limit: 200 }),
        );
        let mut encoder = h.features.new_rendered_feature_encoder();
        let (key, value) = encoder.encode(&feature).unwrap();
        assert!(extract_has_group_from_key(key));
        let mut expected = vec![0xcf, 0, 0, 1, 0, 0, 0, 0, 0]; // group as uint64
        expected.extend([0xcc, 200]); // limit
        expected.extend([0xcd, 1, 44]); // id 300
        expected.extend([0xd0, (17u8 << 3 | 1)]); // geom type + scale as int8 (negative byte)
        expected.push(0x85); // map with 5 non-null entries
        expected.extend([0x00, 0xa2, b'a', b'b']);
        expected.extend([0x01, 0xfb]);
        expected.push(0x02);
        expected.push(0xcb);
        expected.extend(1.5f64.to_be_bytes());
        expected.push(0x03);
        expected.push(0xcb);
        expected.extend(2.5f64.to_be_bytes());
        expected.extend([0x04, 0xc3]);
        expected.extend([0x93, 9, 2, 4]);
        assert_eq!(expected, value);

        // decodes back
        let tile = {
            let mut t = TileFeatures::new(h.features.shared.clone(), 1);
            t.add(SortableFeature::new(key, value.to_vec()));
            t
        };
        let decoded = tile.decode_vector_tile_feature(&tile.entries[0]).unwrap();
        assert_eq!(300, decoded.id);
        assert_eq!(1 << 40, decoded.group);
        assert_eq!(17, decoded.geometry.scale());
        assert_eq!(GeometryType::Point, decoded.geometry.geom_type());
        assert_eq!(&[9, 2, 4], decoded.geometry.commands());
        assert_eq!(tags! {"s" => "ab", "i" => -5i64, "f" => 1.5, "d" => 2.5, "b" => true}, owned_tags(&decoded.tags));
    }

    #[test]
    fn test_memoizes_fill_features() {
        let h = Harness::new();
        let vt = Arc::new(vector_tile::Feature::new("layer", 1, vector_tile::encode_fill(4.0), tags! {"a" => 1}));
        let mut encoder = h.features.new_rendered_feature_encoder();
        let a = encoder.apply(&RenderedFeature::new(TileCoord::of_xyz(0, 0, 1), vt.clone(), 0, None)).unwrap();
        let b = encoder.apply(&RenderedFeature::new(TileCoord::of_xyz(1, 0, 1), vt.clone(), 0, None)).unwrap();
        assert_eq!(a.value, b.value);
        assert_ne!(a.key, b.key);
    }

    #[test]
    fn test_unscale_and_remove_points() {
        let mut features = vec![
            vector_tile::Feature::new(
                "layer",
                1,
                vector_tile::encode_geometry_scaled(&new_line_string(&[0.0, 0.0, 10.0, 10.0]).into(), 2),
                tags! {},
            ),
            vector_tile::Feature::new("layer", 2, vector_tile::encode_geometry(&pt(-100.0, -100.0)), tags! {}),
            vector_tile::Feature::new("layer", 3, vector_tile::encode_geometry(&pt(10.0, 10.0)), tags! {}),
        ];
        unscale_and_remove_points_outside_buffer(&mut features, 4.0);
        assert_eq!(2, features.len());
        assert_eq!(0, features[0].geometry.scale());
        assert_eq!(3, features[1].id);
    }

    #[test]
    fn test_feature_writer_and_process_and_write() {
        use crate::worker::worker_pipeline::WorkerPipeline;
        for inline in [true, false] {
            let group =
                Arc::new(FeatureGroup::new_in_memory(TileOrder::Tms, Arc::new(NullProfile), config(), in_memory()));
            let g2 = group.clone();
            let pipeline = process_and_write(
                WorkerPipeline::start("test", in_memory()).read_from_tiny("read", vec![1u64, 2, 3]),
                inline,
                2,
                1,
                group.clone(),
                move |prev, next| {
                    let mut encoder = g2.new_rendered_feature_encoder();
                    for tile in prev {
                        let f = RenderedFeature::new(
                            TileCoord::decode(tile),
                            vector_tile::Feature::new(
                                "layer",
                                tile as i64,
                                vector_tile::encode_geometry(&pt(1.0, 1.0)),
                                tags! {},
                            ),
                            0,
                            None,
                        );
                        next.write(&mut encoder, &f)?;
                    }
                    Ok(())
                },
            );
            pipeline.await_done().unwrap();
            drop(pipeline);
            // worker threads may still be dropping their clones of the processing closure (and its `g2`) right after
            // the pipeline reports done
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut shared = group;
            let mut group = loop {
                match Arc::try_unwrap(shared) {
                    Ok(group) => break group,
                    Err(still_shared) if std::time::Instant::now() < deadline => {
                        shared = still_shared;
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(_) => panic!("feature group still shared"),
                }
            };
            let tiles: Vec<u64> = group.iter().unwrap().map(|t| t.tile_coord().encoded).collect();
            assert_eq!(vec![1, 2, 3], tiles, "inline={inline}");
        }

        let group = FeatureGroup::new_in_memory(TileOrder::Tms, Arc::new(NullProfile), config(), in_memory());
        let mut writer = group.feature_writer().unwrap();
        writer
            .write(&RenderedFeature::new(
                TileCoord::decode(5),
                vector_tile::Feature::new("layer", 1, vector_tile::encode_geometry(&pt(1.0, 1.0)), tags! {}),
                0,
                None,
            ))
            .unwrap();
        writer.close().unwrap();
        assert_eq!(1, group.num_features_written());
    }

    #[test]
    fn test_string_encoder_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let strings_path = dir.path().join("strings");
        let manifest_path = dir.path().join("manifest");
        let feature_db_dir = dir.path().join("feature.db");

        let mut disk = FeatureGroup::new_disk_backed(
            TileOrder::Tms,
            &feature_db_dir,
            Arc::new(NullProfile),
            config(),
            in_memory(),
            false,
        )
        .unwrap();
        {
            let mut writer = disk.feature_writer().unwrap();
            writer
                .write(&RenderedFeature::new(
                    TileCoord::decode(1),
                    vector_tile::Feature::new(
                        "mylayer",
                        1,
                        vector_tile::encode_geometry(&pt(1.0, 2.0)),
                        tags! {"k" => "v"},
                    ),
                    0,
                    None,
                ))
                .unwrap();
            writer.close().unwrap();
        }
        disk.prepare().unwrap();
        disk.save_string_encoders(&strings_path).unwrap();
        disk.save_chunk_manifest(&manifest_path).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&strings_path).unwrap()).unwrap();
        assert_eq!(serde_json::json!({"layerStrings": ["mylayer"], "valueStrings": ["k"]}), json);
        // keep `disk` alive: dropping a non-reused ExternalMergeSort deletes its chunk files

        let mut reused = FeatureGroup::new_disk_backed(
            TileOrder::Tms,
            &feature_db_dir,
            Arc::new(NullProfile),
            config(),
            in_memory(),
            true,
        )
        .unwrap();
        reused.load_string_encoders(&strings_path).unwrap();
        reused.init_from_manifest(&manifest_path).unwrap();
        let tiles: Vec<TileFeatures> = reused.iter().unwrap().collect();
        assert_eq!(1, tiles.len());
        let decoded = vector_tile::decode(&tiles[0].get_vector_tile().encode()).unwrap();
        assert_eq!(1, decoded.len());
        assert_eq!("mylayer", decoded[0].layer);
        assert_eq!(tags! {"k" => "v"}, owned_tags(&decoded[0].tags));
        drop(reused);
        drop(disk);
    }

    #[test]
    fn test_disk_backed_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut disk = FeatureGroup::new_disk_backed(
            TileOrder::Tms,
            &dir.path().join("db"),
            Arc::new(NullProfile),
            config(),
            in_memory(),
            false,
        )
        .unwrap();
        {
            let mut writer = disk.feature_writer().unwrap();
            for tile in [3u64, 1, 2, 1] {
                writer
                    .write(&RenderedFeature::new(
                        TileCoord::decode(tile),
                        vector_tile::Feature::new(
                            "l",
                            tile as i64,
                            vector_tile::encode_geometry(&pt(1.0, 2.0)),
                            tags! {},
                        ),
                        0,
                        None,
                    ))
                    .unwrap();
            }
            writer.close().unwrap();
        }
        let tiles: Vec<(u64, u64)> =
            disk.iter().unwrap().map(|t| (t.tile_coord().encoded, t.num_features_to_emit())).collect();
        assert_eq!(vec![(1, 2), (2, 1), (3, 1)], tiles);
        let mut par = disk.parallel_iterator(2).unwrap();
        assert_eq!(3, par.by_ref().count());
        par.join().unwrap();
    }
}
