#![allow(dead_code)]
//! Port of `layers/AbstractLayerTest.java` (plus the `TestUtils.toMap`/`assertSubmap` helpers it uses): shared
//! helpers for the per-layer tests.
//!
//! ```ignore
//! let t = LayerTest::new();
//! let feature = point_feature(tags! {"natural" => "peak", "name" => "test"});
//! assert_features(14, &[tags! {"_layer" => "mountain_peak", "name" => "test"}], &t.process(&feature));
//! ```

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use geo_types::Geometry;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::{Factory, Feature, FeatureCollector};
use planetiler_core::geo::geo_utils;
use planetiler_core::geo::geometry_type::GeometryType;
use planetiler_core::geo::test_utils::{new_line_string, new_point, rectangle};
use planetiler_core::profile::Profile;
use planetiler_core::reader::osm::osm_relation_info::{OsmRelationInfo, RelationMember};
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::stats::stats::{StatsRef, in_memory};
use planetiler_core::util::translations::{TranslationProvider, Translations};
use planetiler_core::util::wikidata::WikidataTranslations;
use planetiler_core::value::{Tags, Value};
use planetiler_core::vector_tile;

use crate::open_map_tiles_profile::{OSM_SOURCE, OpenMapTilesProfile};

/// Wikidata translations that tests can keep adding to after they were registered as a translation provider.
#[derive(Clone, Default)]
pub struct SharedWikidataTranslations(pub Arc<RwLock<WikidataTranslations>>);

impl SharedWikidataTranslations {
    pub fn put(&self, qid: i64, lang: &str, value: &str) {
        self.0.write().unwrap().put(qid, lang, value);
    }
}

impl TranslationProvider for SharedWikidataTranslations {
    fn get_name_translations(&self, tags: &Tags) -> Option<Cow<'_, BTreeMap<String, String>>> {
        let guard = self.0.read().unwrap();
        guard.get_name_translations(tags).map(|t| Cow::Owned(t.into_owned()))
    }
}

/// `Translations.defaultProvider(List.of("en", "es", "de")).addFallbackTranslationProvider(wikidataTranslations)`.
pub fn test_translations() -> (SharedWikidataTranslations, Translations) {
    let wikidata = SharedWikidataTranslations::default();
    let translations =
        Translations::default_provider(&["en", "es", "de"]).with_fallback_translation_provider(wikidata.clone());
    (wikidata, translations)
}

/// A profile with the default test translations, config, and stats.
pub fn new_profile() -> OpenMapTilesProfile {
    let (_, translations) = test_translations();
    OpenMapTilesProfile::new(Arc::new(translations), Arc::new(PlanetilerConfig::defaults()), in_memory())
}

/// Port of `AbstractLayerTest` state.
pub struct LayerTest {
    pub wikidata_translations: SharedWikidataTranslations,
    pub translations: Arc<Translations>,
    pub params: Arc<PlanetilerConfig>,
    pub profile: OpenMapTilesProfile,
    pub stats: StatsRef,
    pub feature_collector_factory: Factory,
}

impl Default for LayerTest {
    fn default() -> Self {
        Self::new()
    }
}

impl LayerTest {
    pub fn new() -> LayerTest {
        Self::with_config(PlanetilerConfig::defaults())
    }

    /// Same as [`LayerTest::new`] but with a custom profile config (Java tests that build their own profile).
    pub fn with_config(config: PlanetilerConfig) -> LayerTest {
        let (wikidata_translations, translations) = test_translations();
        let translations = Arc::new(translations);
        let params = Arc::new(PlanetilerConfig::defaults());
        let profile = OpenMapTilesProfile::new(translations.clone(), Arc::new(config), in_memory());
        let stats = in_memory();
        let feature_collector_factory = Factory::new(params.clone(), stats.clone());
        LayerTest { wikidata_translations, translations, params, profile, stats, feature_collector_factory }
    }

    /// Runs the profile on `feature` and returns the collected output features.
    pub fn process<'a>(&'a self, feature: &'a dyn SourceFeature) -> FeatureCollector<'a> {
        let mut collector = self.feature_collector_factory.get(feature);
        self.profile.process_feature(feature, &mut collector);
        collector
    }

    /// `profile.postProcessLayerFeatures(layer, zoom, items)` returning the result.
    pub fn post_process(&self, layer: &str, zoom: u32, items: Vec<vector_tile::Feature>) -> Vec<vector_tile::Feature> {
        let mut items = items;
        self.profile.post_process_layer_features(layer, zoom, &mut items).expect("post process");
        items
    }

    pub fn test_merges_linestrings(&self, attrs: &Tags, layer: &str, length: f64, zoom: u32) {
        let line1 = vt_feature(layer, 1, &new_line_string(&[0.0, 0.0, length / 2.0, 0.0]).into(), attrs.clone(), 0);
        let line2 = vt_feature(layer, 1, &new_line_string(&[length / 2.0, 0.0, length, 0.0]).into(), attrs.clone(), 0);
        let connected = vt_feature(layer, 0, &new_line_string(&[0.0, 0.0, length, 0.0]).into(), attrs.clone(), 0);
        assert_eq!(vec![connected], self.post_process(layer, zoom, vec![line1, line2]));
    }

    pub fn test_does_not_merge_linestrings(&self, attrs: &Tags, layer: &str, length: f64, zoom: u32) {
        let line1 = vt_feature(layer, 1, &new_line_string(&[0.0, 0.0, length / 2.0, 0.0]).into(), attrs.clone(), 0);
        let line2 = vt_feature(layer, 1, &new_line_string(&[length / 2.0, 0.0, length, 0.0]).into(), attrs.clone(), 0);
        assert_eq!(vec![line1.clone(), line2.clone()], self.post_process(layer, zoom, vec![line1, line2]));
    }
}

/// A vector tile feature with an encoded geometry (Java `new VectorTile.Feature(layer, id, encodeGeometry(g), attrs,
/// group)`).
pub fn vt_feature(layer: &str, id: i64, geometry: &Geometry<f64>, attrs: Tags, group: i64) -> vector_tile::Feature {
    let mut feature = vector_tile::Feature::new(layer, id, vector_tile::encode_geometry(geometry), attrs);
    feature.group = group;
    feature
}

/// `AbstractLayerTest.pointFeature(layer, map, group)`: a vector tile point feature at (0, 0).
pub fn vt_point_feature(layer: &str, map: Tags, group: i64) -> vector_tile::Feature {
    vt_feature(layer, 1, &new_point(0.0, 0.0).into(), map, group)
}

fn osm(geometry: impl Into<Geometry<f64>>, props: Tags) -> SimpleFeature {
    SimpleFeature::create(geometry, props, Some(OSM_SOURCE), None::<&str>, 0)
}

pub fn point_feature(props: Tags) -> SimpleFeature {
    osm(new_point(0.0, 0.0), props)
}

pub fn line_feature(props: Tags) -> SimpleFeature {
    osm(new_line_string(&[0.0, 0.0, 1.0, 1.0]), props)
}

fn lat_lon_line_with_length(length: f64) -> Geometry<f64> {
    geo_utils::world_to_lat_lon_coords(
        &new_line_string(&[0.5, 0.5, 0.5 + geo_utils::meters_to_pixel_at_equator(0, length), 0.5]).into(),
    )
}

pub fn line_feature_with_length(length: f64, props: Tags) -> SimpleFeature {
    osm(lat_lon_line_with_length(length), props)
}

pub fn closed_way_feature(props: Tags) -> SimpleFeature {
    SimpleFeature::create_fake_osm_feature(
        new_line_string(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]),
        props,
        Some(OSM_SOURCE),
        None::<&str>,
        0,
        vec![],
        None,
    )
}

pub fn polygon_feature_with_area(area: f64, props: Tags) -> SimpleFeature {
    osm(geo_utils::world_to_lat_lon_coords(&rectangle(0.0, area.sqrt()).into()), props)
}

pub fn polygon_feature(props: Tags) -> SimpleFeature {
    polygon_feature_with_area(1.0, props)
}

fn relation_members(relation_infos: Vec<Arc<dyn OsmRelationInfo>>) -> Vec<RelationMember<dyn OsmRelationInfo>> {
    relation_infos.into_iter().map(|r| RelationMember::new("", r)).collect()
}

pub fn line_feature_with_relation_and_length(
    length: f64,
    relation_infos: Vec<Arc<dyn OsmRelationInfo>>,
    props: Tags,
) -> SimpleFeature {
    SimpleFeature::create_fake_osm_feature(
        lat_lon_line_with_length(length),
        props,
        Some(OSM_SOURCE),
        None::<&str>,
        0,
        relation_members(relation_infos),
        None,
    )
}

pub fn line_feature_with_relation(relation_infos: Vec<Arc<dyn OsmRelationInfo>>, props: Tags) -> SimpleFeature {
    SimpleFeature::create_fake_osm_feature(
        new_line_string(&[0.0, 0.0, 1.0, 1.0]),
        props,
        Some(OSM_SOURCE),
        None::<&str>,
        0,
        relation_members(relation_infos),
        None,
    )
}

/// Port of `TestUtils.toMap(feature, zoom)`: attributes at `zoom` plus `_`-prefixed keys describing the feature.
pub fn to_map(feature: &Feature<'_>, zoom: u32) -> Tags {
    let mut result: Tags = (**feature.get_attrs_at_zoom(zoom)).clone();
    let geom = feature.get_geometry();
    result.insert("_id".into(), feature.get_id().into());
    result.insert("_minzoom".into(), feature.get_min_zoom().into());
    result.insert("_maxzoom".into(), feature.get_max_zoom().into());
    result.insert("_buffer".into(), feature.get_buffer_pixels_at_zoom(zoom).into());
    result.insert("_layer".into(), feature.get_layer().into());
    result.insert("_sortkey".into(), feature.get_sort_key().into());
    result.insert("_labelgrid_limit".into(), feature.get_point_label_grid_limit_at_zoom(zoom).into());
    result.insert("_labelgrid_size".into(), feature.get_point_label_grid_pixel_size_at_zoom(zoom).into());
    result.insert("_minpixelsize".into(), feature.get_min_pixel_size_at_zoom(zoom).into());
    result.insert(
        "_type".into(),
        match GeometryType::type_of(geom) {
            GeometryType::Point => "point",
            GeometryType::Line => "line",
            GeometryType::Polygon => "polygon",
            GeometryType::Unknown => "GeometryCollection",
        }
        .into(),
    );
    result.insert("_numpointsattr".into(), feature.get_num_points_attr().into());
    result.sort_keys();
    result
}

/// Normalizes `f32` to `f64` so expectations can be written with plain float literals. Integers and floats stay
/// distinct (write `4.0` for double-valued keys like `_buffer` and `_minpixelsize`).
fn normalize(value: &Value) -> Value {
    match value {
        Value::Float(f) => Value::Double(*f as f64),
        other => other.clone(),
    }
}

/// Port of `TestUtils.assertSubmap`: every key in `expected` must have the same value in `actual`, where an expected
/// `"<null>"` value means the key must be absent.
pub fn assert_submap(expected: &Tags, actual: &Tags) {
    let mut actual_filtered = BTreeMap::new();
    let mut others = BTreeMap::new();
    for (k, v) in actual {
        if expected.contains_key(k) {
            actual_filtered.insert(k.clone(), normalize(v));
        } else {
            others.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in expected {
        if v.as_str() == Some("<null>") && !actual.contains_key(k) {
            actual_filtered.insert(k.clone(), v.clone());
        }
    }
    let expected: BTreeMap<_, _> = expected.iter().map(|(k, v)| (k.clone(), normalize(v))).collect();
    assert_eq!(expected, actual_filtered, "others: {others:?}");
}

/// Port of `AbstractLayerTest.assertFeatures`: features (sorted by layer) must match `expected` (sorted by `_layer`).
pub fn assert_features<'a>(zoom: u32, expected: &[Tags], actual: impl IntoIterator<Item = &'a Feature<'a>>) {
    let mut expected_list: Vec<&Tags> = expected.iter().collect();
    expected_list.sort_by_key(|d| d.get("_layer").map(|v| v.to_string()).unwrap_or_default());
    let mut actual_list: Vec<&Feature<'_>> = actual.into_iter().collect();
    actual_list.sort_by(|a, b| a.get_layer().cmp(b.get_layer()));
    assert_eq!(
        expected_list.len(),
        actual_list.len(),
        "size: {:?}",
        actual_list.iter().map(|f| to_map(f, zoom)).collect::<Vec<_>>()
    );
    for (e, a) in expected_list.iter().zip(actual_list) {
        assert_submap(e, &to_map(a, zoom));
    }
}

pub fn assert_descending(vals: &[i32]) {
    for i in 1..vals.len() {
        assert!(vals[i - 1] >= vals[i], "element at {} is less than element at {}: {vals:?}", i - 1, i);
    }
}

pub fn assert_ascending(vals: &[i32]) {
    for i in 1..vals.len() {
        assert!(
            vals[i - 1] <= vals[i],
            "{vals:?}\nelement at {} ({}) is greater than element at {} ({})",
            i - 1,
            vals[i - 1],
            i,
            vals[i]
        );
    }
}

/// Port of `AbstractLayerTest.assertCoversZoomRange`: exactly one `layer` feature at each zoom in
/// `minzoom..=maxzoom` and none outside it (zooms 0-14).
pub fn assert_covers_zoom_range(minzoom: u32, maxzoom: u32, layer: &str, collectors: &[&FeatureCollector<'_>]) {
    let mut zooms: Vec<Option<Tags>> = vec![None; 15.max(maxzoom as usize + 1)];
    for features in collectors {
        for feature in features.iter() {
            if feature.get_layer() == layer {
                for zoom in feature.get_min_zoom()..=feature.get_max_zoom() {
                    if zoom as usize >= zooms.len() {
                        continue;
                    }
                    let map = to_map(feature, zoom);
                    if let Some(existing) = &zooms[zoom as usize] {
                        panic!("Multiple features at z{zoom}:\n{existing:?}\n{map:?}");
                    }
                    zooms[zoom as usize] = Some(map);
                }
            }
        }
    }
    for zoom in 0..=14u32 {
        let found = &zooms[zoom as usize];
        if zoom < minzoom || zoom > maxzoom {
            assert!(found.is_none(), "Expected nothing at z{zoom} but found: {found:?}");
        } else {
            assert!(found.is_some(), "No feature at z{zoom}");
        }
    }
}
