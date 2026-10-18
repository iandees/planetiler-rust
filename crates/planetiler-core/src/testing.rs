//! End-to-end test harness: a port of the `PlanetilerTests.runWithReaderFeatures` / `run` helpers plus the parts of
//! `TestUtils` that read tiles back out of an archive (`getTileMap`, `ComparableFeature`, `feature(...)`).
//!
//! Runs input [`SimpleFeature`]s through a [`Profile`] -> [`FeatureCollector`] -> [`FeatureRenderer`] ->
//! [`FeatureGroup`] -> [`tile_archive_writer::write_output`] into an in-memory mbtiles database and returns the
//! decoded tiles and metadata. Not `#[cfg(test)]`-gated so that integration tests and other crates (custommap,
//! openmaptiles) can use it.
//!
//! ```no_run
//! use planetiler_core::testing::*;
//! use planetiler_core::geo::test_utils::new_point;
//! use planetiler_core::reader::simple_feature::SimpleFeature;
//! use planetiler_core::tags;
//!
//! let results = run_with_reader_features(
//!     &[("threads", "1")],
//!     vec![SimpleFeature::create_tags(new_point(0.0, 0.0), tags! {"attr" => "value"})],
//!     |_source, features| {
//!         features.point("layer").set_zoom_range(13, 14).inherit_attr_from_source("attr");
//!     },
//! )
//! .unwrap();
//! assert!(!results.tiles.is_empty());
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use geo::Relate;
use geo_types::Geometry;
use indexmap::IndexMap;

use crate::archive::readable_tile_archive::ReadableTileArchive;
use crate::archive::tile_archive_config::ArchiveOptions;
use crate::archive::tile_archive_metadata::TileArchiveMetadata;
use crate::archive::tile_archive_writer;
use crate::archive::tile_compression::TileCompression;
use crate::collection::feature_group::FeatureGroup;
use crate::collection::long_long_map;
use crate::config::arguments::Arguments;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::feature_collector::{self, FeatureCollector};
use crate::geo::geometry_exception::GeometryResult;
use crate::geo::normalize;
use crate::geo::test_utils;
use crate::geo::tile_coord::TileCoord;
use crate::geo::tile_order::TileOrder;
use crate::mbtiles::mbtiles::Mbtiles;
use crate::profile::{FeatureConsumer, LayerFeatures, Profile};
use crate::reader::osm::osm_block_source::ElementsBlockSource;
use crate::reader::osm::osm_element::{OsmElement, Relation};
use crate::reader::osm::osm_reader::OsmReader;
use crate::reader::osm::osm_relation_info::OsmRelationInfo;
use crate::reader::simple_feature::SimpleFeature;
use crate::reader::source_feature::SourceFeature;
use crate::render::feature_renderer::FeatureRenderer;
use crate::stats::stats::{InMemoryStats, StatsRef};
use crate::util::file_utils::DiskBacked;
use crate::util::gzip::gunzip;
use crate::value::Tags;
use crate::vector_tile;

pub const TEST_PROFILE_NAME: &str = "test name";
pub const TEST_PROFILE_DESCRIPTION: &str = "test description";
pub const TEST_PROFILE_ATTRIBUTION: &str = "test attribution";
pub const TEST_PROFILE_VERSION: &str = "test version";

/// `Profile::process_feature` as a closure.
pub type ProcessFeatureFn = Box<dyn Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync>;
/// `Profile::preprocess_osm_relation` as a closure.
pub type PreprocessOsmRelationFn = Box<dyn Fn(&Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> + Send + Sync>;
/// `Profile::post_process_layer_features` as a closure (Java `LayerPostprocessFunction`).
pub type LayerPostProcessFn =
    Box<dyn Fn(&str, u32, &mut Vec<vector_tile::Feature>) -> GeometryResult<()> + Send + Sync>;
/// `Profile::post_process_tile_features` as a closure (Java `TilePostprocessFunction`).
pub type TilePostProcessFn = Box<dyn Fn(TileCoord, &mut LayerFeatures) -> GeometryResult<()> + Send + Sync>;

/// A [`Profile`] built from closures (Java `PlanetilerTests.TestProfile`).
pub struct TestProfile {
    pub name: String,
    pub description: String,
    pub attribution: String,
    pub version: String,
    pub process_feature: ProcessFeatureFn,
    pub preprocess_osm_relation: Option<PreprocessOsmRelationFn>,
    pub post_process_layer_features: Option<LayerPostProcessFn>,
    pub post_process_tile_features: Option<TilePostProcessFn>,
}

impl TestProfile {
    /// A profile that only processes source features (Java `TestProfile.processSourceFeatures`).
    pub fn process_source_features(
        process_feature: impl Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync + 'static,
    ) -> TestProfile {
        TestProfile {
            name: TEST_PROFILE_NAME.to_string(),
            description: TEST_PROFILE_DESCRIPTION.to_string(),
            attribution: TEST_PROFILE_ATTRIBUTION.to_string(),
            version: TEST_PROFILE_VERSION.to_string(),
            process_feature: Box::new(process_feature),
            preprocess_osm_relation: None,
            post_process_layer_features: None,
            post_process_tile_features: None,
        }
    }

    pub fn with_preprocess_osm_relation(
        mut self,
        f: impl Fn(&Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> + Send + Sync + 'static,
    ) -> TestProfile {
        self.preprocess_osm_relation = Some(Box::new(f));
        self
    }

    pub fn with_layer_post_process(
        mut self,
        f: impl Fn(&str, u32, &mut Vec<vector_tile::Feature>) -> GeometryResult<()> + Send + Sync + 'static,
    ) -> TestProfile {
        self.post_process_layer_features = Some(Box::new(f));
        self
    }

    pub fn with_tile_post_process(
        mut self,
        f: impl Fn(TileCoord, &mut LayerFeatures) -> GeometryResult<()> + Send + Sync + 'static,
    ) -> TestProfile {
        self.post_process_tile_features = Some(Box::new(f));
        self
    }
}

impl Profile for TestProfile {
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        (self.process_feature)(source_feature, features)
    }

    fn preprocess_osm_relation(&self, relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
        self.preprocess_osm_relation.as_ref().and_then(|f| f(relation))
    }

    fn post_process_layer_features(
        &self,
        layer: &str,
        zoom: u32,
        items: &mut Vec<vector_tile::Feature>,
    ) -> GeometryResult<()> {
        match &self.post_process_layer_features {
            Some(f) => f(layer, zoom, items),
            None => Ok(()),
        }
    }

    fn post_process_tile_features(&self, tile_coord: TileCoord, layers: &mut LayerFeatures) -> GeometryResult<()> {
        match &self.post_process_tile_features {
            Some(f) => f(tile_coord, layers),
            None => Ok(()),
        }
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn description(&self) -> Option<String> {
        Some(self.description.clone())
    }

    fn attribution(&self) -> Option<String> {
        Some(self.attribution.clone())
    }

    fn version(&self) -> Option<String> {
        Some(self.version.clone())
    }
}

/// Output of [`run`]: decoded tiles, archive metadata and (for compact mbtiles) the number of distinct tile blobs.
#[derive(Debug)]
pub struct PlanetilerResults {
    pub tiles: BTreeMap<TileCoord, Vec<ComparableFeature>>,
    pub metadata: IndexMap<String, String>,
    pub tile_data_count: usize,
}

struct NoDiskUsage;

impl DiskBacked for NoDiskUsage {
    fn disk_usage_bytes(&self) -> u64 {
        0
    }
}

/// Context handed to a [`run`] runner to populate the feature group.
pub struct RunContext<'a> {
    pub feature_group: &'a Arc<FeatureGroup>,
    pub profile: &'a dyn Profile,
    /// The same profile as a shared handle (for readers like `OsmReader` that need to own it).
    pub profile_arc: &'a Arc<dyn Profile>,
    pub config: &'a Arc<PlanetilerConfig>,
    pub stats: &'a StatsRef,
}

/// Parses `args` into a config, lets `runner` write features into an in-memory feature group, then encodes all
/// tiles into an in-memory mbtiles archive with [`tile_archive_writer::write_output`] and decodes them (Java
/// `PlanetilerTests.run`).
pub fn run(
    args: &[(&str, &str)],
    runner: impl FnOnce(&RunContext<'_>) -> anyhow::Result<()>,
    profile: Arc<dyn Profile>,
) -> anyhow::Result<PlanetilerResults> {
    let config = Arc::new(PlanetilerConfig::from(&Arguments::of_pairs(args.iter().copied()))?);
    let stats = InMemoryStats::arc();
    let feature_group =
        Arc::new(FeatureGroup::new_in_memory(TileOrder::Tms, profile.clone(), config.clone(), stats.clone()));
    runner(&RunContext {
        feature_group: &feature_group,
        profile: &*profile,
        profile_arc: &profile,
        config: &config,
        stats: &stats,
    })?;
    let mut feature_group =
        Arc::into_inner(feature_group).ok_or_else(|| anyhow::anyhow!("feature group still in use after runner"))?;
    feature_group.prepare()?;
    let mut db =
        Mbtiles::new_in_memory_database_with_options(&ArchiveOptions::from_arguments(config.arguments.clone()))?;
    tile_archive_writer::write_output(
        &mut feature_group,
        &mut db,
        Arc::new(NoDiskUsage),
        &TileArchiveMetadata::from_profile(&*profile, &config),
        None,
        &config,
        &stats,
    )?;
    let tiles = get_tile_map_with_compression(&db, config.tile_compression)?;
    for features in tiles.values() {
        for feature in features {
            test_utils::validate_geometry(feature.geometry.geom());
        }
    }
    let tile_data_count = if db.compact_db() { get_tiles_data_count(&db)? } else { 0 };
    let metadata = db.metadata()?.map(|m| m.to_map()).unwrap_or_default();
    Ok(PlanetilerResults { tiles, metadata, tile_data_count })
}

/// Processes `features` with `profile` and writes the rendered features into the feature group (Java
/// `PlanetilerTests.processReaderFeatures`, which uses `SourceFeatureProcessor.processFiles` with a source named
/// `"test"`).
pub fn process_reader_features(ctx: &RunContext<'_>, features: &[SimpleFeature]) -> anyhow::Result<()> {
    let factory = feature_collector::Factory::new(ctx.config.clone(), ctx.stats.clone());
    let renderer = FeatureRenderer::new(ctx.config.clone(), ctx.stats.clone());
    let mut writer = ctx.feature_group.feature_writer()?;
    let mut error: Option<anyhow::Error> = None;
    {
        let mut write = |feature: &feature_collector::Feature<'_>| {
            renderer.accept(feature, &mut |rendered| {
                if error.is_none()
                    && let Err(e) = writer.write(&rendered)
                {
                    error = Some(e);
                }
            });
        };
        for source_feature in features {
            let mut collector = factory.get(source_feature);
            ctx.profile.process_feature(source_feature, &mut collector);
            for feature in collector.iter() {
                write(feature);
            }
        }
        let next: &mut FeatureConsumer<'_> = &mut write;
        ctx.profile.finish("test", &factory, next);
    }
    if let Some(e) = error {
        return Err(e);
    }
    writer.close()
}

/// Splits `elements` into blocks the way they come from an OSM file (other, nodes, ways, relations) (Java
/// `PlanetilerTests.processOsmFeatures` block source).
pub fn osm_elements_block_source(elements: &[OsmElement]) -> ElementsBlockSource {
    let of_type = |f: fn(&OsmElement) -> bool| elements.iter().filter(|e| f(e)).cloned().collect::<Vec<_>>();
    ElementsBlockSource(vec![
        of_type(|e| matches!(e, OsmElement::Other(_))),
        of_type(|e| matches!(e, OsmElement::Node(_))),
        of_type(|e| matches!(e, OsmElement::Way(_))),
        of_type(|e| matches!(e, OsmElement::Relation(_))),
    ])
}

/// Runs OSM `elements` through `OsmReader` pass 1 and pass 2 into the feature group (Java
/// `PlanetilerTests.processOsmFeatures`).
pub fn process_osm_features(ctx: &RunContext<'_>, elements: &[OsmElement]) -> anyhow::Result<()> {
    let reader = OsmReader::new_in_memory_multipolygons(
        "osm",
        osm_elements_block_source(elements),
        Arc::from(long_long_map::new_in_memory_sorted_table()),
        ctx.profile_arc.clone(),
        InMemoryStats::arc(),
    );
    reader.pass1(ctx.config)?;
    reader.pass2(ctx.feature_group, ctx.config)?;
    reader.close();
    Ok(())
}

/// Runs OSM `elements` through `profile` (Java `PlanetilerTests.runWithOsmElements(args, features, profile)`).
pub fn run_with_osm_elements_profile(
    args: &[(&str, &str)],
    elements: Vec<OsmElement>,
    profile: impl Profile + 'static,
) -> anyhow::Result<PlanetilerResults> {
    run(args, |ctx| process_osm_features(ctx, &elements), Arc::new(profile))
}

/// Runs OSM `elements` through a [`TestProfile`] with `preprocess_osm_relation` and `profile_function` (Java
/// `PlanetilerTests.runWithOsmElements(args, features, preprocessOsmRelation, profileFunction)`).
pub fn run_with_osm_elements(
    args: &[(&str, &str)],
    elements: Vec<OsmElement>,
    preprocess_osm_relation: impl Fn(&Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> + Send + Sync + 'static,
    profile_function: impl Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync + 'static,
) -> anyhow::Result<PlanetilerResults> {
    run_with_osm_elements_profile(
        args,
        elements,
        TestProfile::process_source_features(profile_function).with_preprocess_osm_relation(preprocess_osm_relation),
    )
}

/// Runs `features` through a [`TestProfile`] that calls `profile_function` for each one (Java
/// `PlanetilerTests.runWithReaderFeatures`).
pub fn run_with_reader_features(
    args: &[(&str, &str)],
    features: Vec<SimpleFeature>,
    profile_function: impl Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync + 'static,
) -> anyhow::Result<PlanetilerResults> {
    run_with_reader_features_profile(args, features, TestProfile::process_source_features(profile_function))
}

/// Like [`run_with_reader_features`] with a layer post-processing function.
pub fn run_with_reader_features_and_post_process(
    args: &[(&str, &str)],
    features: Vec<SimpleFeature>,
    profile_function: impl Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync + 'static,
    post_process: impl Fn(&str, u32, &mut Vec<vector_tile::Feature>) -> GeometryResult<()> + Send + Sync + 'static,
) -> anyhow::Result<PlanetilerResults> {
    run_with_reader_features_profile(
        args,
        features,
        TestProfile::process_source_features(profile_function).with_layer_post_process(post_process),
    )
}

/// Runs `features` through `profile` (Java `PlanetilerTests.runWithReaderFeaturesProfile`).
pub fn run_with_reader_features_profile(
    args: &[(&str, &str)],
    features: Vec<SimpleFeature>,
    profile: impl Profile + 'static,
) -> anyhow::Result<PlanetilerResults> {
    run(args, |ctx| process_reader_features(ctx, &features), Arc::new(profile))
}

/// Number of rows in `tiles_data` (compact mbtiles) or `tiles` (Java `TestUtils.getTilesDataCount`).
pub fn get_tiles_data_count(db: &Mbtiles) -> anyhow::Result<usize> {
    let conn = db.connection();
    let is_compact: i64 =
        conn.query_row("select count(*) from sqlite_master where type='view' and name='tiles'", [], |r| r.get(0))?;
    let table = if is_compact > 0 { "tiles_data" } else { "tiles" };
    let count: i64 = conn.query_row(&format!("select count(*) from {table}"), [], |r| r.get(0))?;
    Ok(count as usize)
}

/// Decodes every (gzipped) tile in `db` (Java `TestUtils.getTileMap`).
pub fn get_tile_map(db: &dyn ReadableTileArchive) -> anyhow::Result<BTreeMap<TileCoord, Vec<ComparableFeature>>> {
    get_tile_map_with_compression(db, TileCompression::Gzip)
}

/// Decodes every tile in `db` compressed with `compression`.
pub fn get_tile_map_with_compression(
    db: &dyn ReadableTileArchive,
    compression: TileCompression,
) -> anyhow::Result<BTreeMap<TileCoord, Vec<ComparableFeature>>> {
    let mut tiles = BTreeMap::new();
    for tile in db.get_all_tiles()? {
        let tile = tile?;
        let bytes = match compression {
            TileCompression::Gzip => gunzip(&tile.bytes)?,
            TileCompression::None => tile.bytes,
            TileCompression::Unknown => anyhow::bail!("cannot decompress \"UNKNOWN\""),
        };
        let decoded = vector_tile::decode(&bytes)?
            .into_iter()
            .map(|f| {
                ComparableFeature::new(
                    GeometryComparison::Norm(test_utils::decode_silently(&f.geometry)),
                    Some(f.layer),
                    f.tags,
                    Some(f.id),
                )
            })
            .collect();
        tiles.insert(tile.coord, decoded);
    }
    Ok(tiles)
}

/// How to compare a geometry with another (Java `TestUtils.GeometryComparision` records).
///
/// Equality uses the mode of whichever side is not [`GeometryComparison::Norm`] (expected values built with
/// [`GeometryComparison::Round`] or [`GeometryComparison::Topo`] compare that way against decoded tiles).
#[derive(Clone)]
pub enum GeometryComparison {
    /// Equal after normalizing (JTS `equalsNorm`).
    Norm(Geometry<f64>),
    /// Equal after rounding to 5 decimal places and normalizing.
    Round(Geometry<f64>),
    /// Exactly equal coordinates and structure (JTS `equalsExact`).
    Exact(Geometry<f64>),
    /// Topologically equal (JTS `equalsTopo`).
    Topo(Geometry<f64>),
}

impl GeometryComparison {
    pub fn geom(&self) -> &Geometry<f64> {
        match self {
            GeometryComparison::Norm(g)
            | GeometryComparison::Round(g)
            | GeometryComparison::Exact(g)
            | GeometryComparison::Topo(g) => g,
        }
    }
}

impl PartialEq for GeometryComparison {
    fn eq(&self, other: &Self) -> bool {
        let mode = if matches!(self, GeometryComparison::Norm(_)) { other } else { self };
        let (a, b) = (self.geom(), other.geom());
        match mode {
            GeometryComparison::Norm(_) => normalize::equals_norm(a, b),
            GeometryComparison::Round(_) => normalize::equals_norm(&test_utils::round(a), &test_utils::round(b)),
            GeometryComparison::Exact(_) => a == b,
            GeometryComparison::Topo(_) => a.relate(b).is_equal_topo(),
        }
    }
}

impl fmt::Debug for GeometryComparison {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GeometryComparison::Norm(g) => write!(f, "Norm{{{:?}}}", normalize::norm(g)),
            GeometryComparison::Round(g) => write!(f, "Round{{{:?}}}", normalize::norm(&test_utils::round(g))),
            GeometryComparison::Exact(g) => write!(f, "Exact{{{g:?}}}"),
            GeometryComparison::Topo(g) => write!(f, "Topo{{{g:?}}}"),
        }
    }
}

/// A decoded (or expected) vector tile feature (Java `TestUtils.ComparableFeature`).
///
/// `layer` and `id` are ignored in comparisons when either side is `None`.
#[derive(Clone, Debug)]
pub struct ComparableFeature {
    pub geometry: GeometryComparison,
    pub layer: Option<String>,
    pub attrs: Tags,
    pub id: Option<i64>,
}

impl ComparableFeature {
    pub fn new(geometry: GeometryComparison, layer: Option<String>, attrs: Tags, id: Option<i64>) -> Self {
        ComparableFeature { geometry, layer, attrs, id }
    }

    pub fn with_id(mut self, id: i64) -> Self {
        self.id = Some(id);
        self
    }
}

impl PartialEq for ComparableFeature {
    fn eq(&self, other: &Self) -> bool {
        (self.id.is_none() || other.id.is_none() || self.id == other.id)
            && self.geometry == other.geometry
            && self.attrs == other.attrs
            && (self.layer.is_none() || other.layer.is_none() || self.layer == other.layer)
    }
}

/// `feature(geom, attrs)`: an expected feature in any layer with any id, compared with normalized geometry.
pub fn feature(geom: impl Into<Geometry<f64>>, attrs: Tags) -> ComparableFeature {
    ComparableFeature::new(GeometryComparison::Norm(geom.into()), None, attrs, None)
}

/// `feature(geom, layer, attrs)` / `feature(layer, geom, attrs)`.
pub fn feature_in_layer(layer: &str, geom: impl Into<Geometry<f64>>, attrs: Tags) -> ComparableFeature {
    ComparableFeature::new(GeometryComparison::Norm(geom.into()), Some(layer.to_string()), attrs, None)
}

/// `feature(geom, attrs, id)`.
pub fn feature_with_id(geom: impl Into<Geometry<f64>>, attrs: Tags, id: i64) -> ComparableFeature {
    ComparableFeature::new(GeometryComparison::Norm(geom.into()), None, attrs, Some(id))
}

/// `feature(geom, layer, attrs, id)`.
pub fn feature_in_layer_with_id(
    layer: &str,
    geom: impl Into<Geometry<f64>>,
    attrs: Tags,
    id: i64,
) -> ComparableFeature {
    ComparableFeature::new(GeometryComparison::Norm(geom.into()), Some(layer.to_string()), attrs, Some(id))
}

/// Asserts that every key of `expected` maps to an equal value in `actual` (Java `TestUtils.assertSubmap` for maps
/// such as tiles).
#[track_caller]
pub fn assert_submap<K: Ord + Clone + fmt::Debug, V: PartialEq + fmt::Debug>(
    expected: &BTreeMap<K, V>,
    actual: &BTreeMap<K, V>,
) {
    let actual_filtered: BTreeMap<&K, &V> = actual.iter().filter(|(k, _)| expected.contains_key(k)).collect();
    let others: Vec<&K> = actual.keys().filter(|k| !expected.contains_key(k)).collect();
    let expected: BTreeMap<&K, &V> = expected.iter().collect();
    assert_eq!(expected, actual_filtered, "others: {others:?}");
}

/// Asserts that every key of `expected` has the same value in `actual` string map (e.g. archive metadata).
#[track_caller]
pub fn assert_string_submap(expected: &[(&str, &str)], actual: &IndexMap<String, String>) {
    let expected: BTreeMap<&str, &str> = expected.iter().copied().collect();
    let actual_filtered: BTreeMap<&str, &str> = actual
        .iter()
        .filter(|(k, _)| expected.contains_key(k.as_str()))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(expected, actual_filtered, "all: {actual:?}");
}

/// Builds a tile map from `(x, y, z, features)` entries (Java `newTileEntry` + `Map.ofEntries`).
pub fn tile_map(
    entries: impl IntoIterator<Item = ((u32, u32, u32), Vec<ComparableFeature>)>,
) -> BTreeMap<TileCoord, Vec<ComparableFeature>> {
    entries.into_iter().map(|((x, y, z), features)| (TileCoord::of_xyz(x, y, z), features)).collect()
}
