//! Port of `layers/Landcover.java`: defines the logic for generating map elements for natural land cover polygons
//! like ice, sand, and forest in the `landcover` layer from source features.
//!
//! This is ported from the
//! [OpenMapTiles landcover sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/landcover).
//!
//! Large ice areas come from natural earth and the rest come from OpenStreetMap at higher zoom levels. At render-time,
//! post-processing merges polygons into larger connected area based on the number of points in the original area.
//! Since post-processing only has visibility into features on a single tile, processing needs to pass the number of
//! points the original feature had through using a temporary `_numpoints` attribute.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::expression::expression::Expression;
use planetiler_core::expression::multi_expression::Index;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::forwarding_profile::{Handler, LayerPostProcessor};
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;
use planetiler_core::util::zoom_function::ZoomFunction;
use planetiler_core::value::{Tags, Value};
use planetiler_core::vector_tile;

use crate::feature_merge;
use crate::generated::open_map_tiles_schema::landcover::*;
use crate::generated::tables::{OsmLandcoverPolygon, OsmLandcoverPolygonHandler, RowHandlers, WithSubclass};
use crate::layer::Layer;
use crate::open_map_tiles_profile::NaturalEarthProcessor;

/// Min pixel size by zoom for OSM landcover polygons.
pub static MIN_PIXEL_SIZE_THRESHOLDS: LazyLock<ZoomFunction<f64>> =
    LazyLock::new(|| ZoomFunction::from_max_zoom_thresholds([(13, 8.0), (10, 4.0), (9, 2.0)]));
const TEMP_NUM_POINTS_ATTR: &str = "_numpoints";
const TEMP_GROUP_KEY: &str = "_group";

fn is_wood_or_forest(subclass: &str) -> bool {
    subclass == field_values::SUBCLASS_WOOD || subclass == field_values::SUBCLASS_FOREST
}

/// Lookup from subclass to class: a hash map when every class mapping is a plain `match_any("subclass", ...)`
/// (the case for the current schema), otherwise the general multi-expression index.
enum ClassMapping {
    Exact(HashMap<String, &'static str>),
    Index(Index<&'static str>),
}

impl ClassMapping {
    fn new() -> ClassMapping {
        let mut exact = HashMap::new();
        for entry in &field_mappings::CLASS.expressions {
            match &entry.expression {
                Expression::MatchAny(any)
                    if any.field.as_deref() == Some(fields::SUBCLASS)
                        && any.pattern.is_none()
                        && !any.match_when_missing =>
                {
                    for value in &any.exact_matches {
                        exact.entry(value.clone()).or_insert(entry.result);
                    }
                }
                Expression::Constant(false) => {}
                _ => return ClassMapping::Index(field_mappings::CLASS.index()),
            }
        }
        ClassMapping::Exact(exact)
    }

    fn get(&self, subclass: &str) -> Option<&'static str> {
        match self {
            ClassMapping::Exact(map) => map.get(subclass).copied(),
            ClassMapping::Index(index) => {
                let mut tags = Tags::with_capacity(1);
                tags.insert(fields::SUBCLASS.to_string(), Value::from(subclass));
                index.get_first(&tags).copied()
            }
        }
    }
}

pub struct Landcover {
    class_mapping: ClassMapping,
}

impl Landcover {
    pub fn new(_translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, _stats: &StatsRef) -> Landcover {
        Landcover { class_mapping: ClassMapping::new() }
    }

    fn get_class_from_subclass(&self, subclass: Option<&str>) -> Option<&'static str> {
        subclass.and_then(|s| self.class_mapping.get(s))
    }
}

impl NaturalEarthProcessor for Landcover {
    fn process_natural_earth(&self, table: &str, _feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        let (subclass, minzoom, maxzoom) = match table {
            "ne_110m_glaciated_areas" => (field_values::SUBCLASS_GLACIER, 0, 1),
            "ne_50m_glaciated_areas" => (field_values::SUBCLASS_GLACIER, 2, 4),
            "ne_10m_glaciated_areas" => (field_values::SUBCLASS_GLACIER, 5, 6),
            "ne_50m_antarctic_ice_shelves_polys" => ("ice_shelf", 2, 4),
            "ne_10m_antarctic_ice_shelves_polys" => ("ice_shelf", 5, 6),
            _ => return,
        };
        if let Some(clazz) = self.get_class_from_subclass(Some(subclass)) {
            features
                .polygon(LAYER_NAME)
                .set_buffer_pixels(BUFFER_SIZE)
                .set_attr(fields::CLASS, clazz)
                .set_attr(fields::SUBCLASS, subclass)
                .set_zoom_range(minzoom, maxzoom);
        }
    }
}

impl OsmLandcoverPolygonHandler for Landcover {
    fn process_osm_landcover_polygon(&self, element: &OsmLandcoverPolygon<'_>, features: &mut FeatureCollector<'_>) {
        let subclass = element.subclass();
        if let Some(clazz) = self.get_class_from_subclass(subclass.as_deref()) {
            features
                .polygon(LAYER_NAME)
                .set_buffer_pixels(BUFFER_SIZE)
                .set_min_pixel_size_overrides(Some(MIN_PIXEL_SIZE_THRESHOLDS.clone()))
                // default is 0.1, this helps reduce size of some heavy z7-10 tiles
                .set_pixel_tolerance_below_zoom(10, 0.25)
                .set_attr(fields::CLASS, clazz)
                .set_attr(fields::SUBCLASS, subclass.as_deref())
                .set_num_points_attr(TEMP_NUM_POINTS_ATTR)
                .set_min_zoom(7);
        }
    }
}

impl LayerPostProcessor for Landcover {
    fn post_process(&self, zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        if !(7..=13).contains(&zoom) {
            for item in items.iter_mut() {
                item.tags.shift_remove(TEMP_NUM_POINTS_ATTR);
            }
            return Ok(());
        }
        // z7-13
        // merging only merges polygons with the same attributes, so use this temporary key
        // to separate features into layers that will be merged separately
        let mut result = Vec::with_capacity(items.len());
        let mut to_merge = Vec::new();
        for mut item in std::mem::take(items) {
            let num_points = item.tags.shift_remove(TEMP_NUM_POINTS_ATTR);
            let num_points = num_points.as_ref().filter(|v| v.is_number()).and_then(Value::as_f64);
            let is_wood = match item.tags.get(fields::SUBCLASS) {
                Some(Value::String(subclass)) => Some(is_wood_or_forest(subclass)),
                _ => None,
            };
            match (num_points, is_wood) {
                (Some(num_points), Some(is_wood)) => {
                    let num_points = num_points as i64;
                    if zoom >= 10 {
                        if is_wood && num_points < 300 {
                            item.tags.insert(TEMP_GROUP_KEY.into(), "<300".into());
                            to_merge.push(item);
                        } else {
                            // don't merge
                            result.push(item);
                        }
                    } else if (8..=9).contains(&zoom) {
                        if is_wood {
                            item.tags
                                .insert(TEMP_GROUP_KEY.into(), if num_points < 300 { "<300" } else { ">300" }.into());
                            to_merge.push(item);
                        } else {
                            // don't merge
                            result.push(item);
                        }
                    } else {
                        // zoom 7
                        to_merge.push(item);
                    }
                }
                _ => result.push(item),
            }
        }
        let mut merged = feature_merge::merge_overlapping_polygons(to_merge, 4.0)?;
        for item in &mut merged {
            item.tags.shift_remove(TEMP_GROUP_KEY);
        }
        result.extend(merged);
        *items = result;
        Ok(())
    }
}

impl Handler for Landcover {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }
}

impl RowHandlers for Landcover {
    fn as_osm_landcover_polygon_handler(&self) -> Option<&dyn OsmLandcoverPolygonHandler> {
        Some(self)
    }
}

impl Layer for Landcover {
    fn as_natural_earth_processor(&self) -> Option<&dyn NaturalEarthProcessor> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use geo_types::Polygon;
    use planetiler_core::geo::geo_utils;
    use planetiler_core::geo::test_utils::rectangle;
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;
    use planetiler_core::value::Tags;
    use planetiler_core::vector_tile;

    use crate::open_map_tiles_profile::NATURAL_EARTH_SOURCE;
    use crate::test_support::*;

    fn ne_feature(layer: &str) -> SimpleFeature {
        SimpleFeature::create(
            geo_utils::world_to_lat_lon_coords(&rectangle(0.0, 1.0).into()),
            Tags::new(),
            Some(NATURAL_EARTH_SOURCE),
            Some(layer),
            0,
        )
    }

    #[test]
    fn test_natural_earth_glaciers() {
        let t = LayerTest::new();
        let (f1, f2, f3) = (
            ne_feature("ne_110m_glaciated_areas"),
            ne_feature("ne_50m_glaciated_areas"),
            ne_feature("ne_10m_glaciated_areas"),
        );
        let glacier1 = t.process(&f1);
        let glacier2 = t.process(&f2);
        let glacier3 = t.process(&f3);
        let expected = tags! {"_layer" => "landcover", "subclass" => "glacier", "class" => "ice", "_buffer" => 4.0};
        assert_features(0, std::slice::from_ref(&expected), &glacier1);
        assert_features(0, std::slice::from_ref(&expected), &glacier2);
        assert_features(0, std::slice::from_ref(&expected), &glacier3);
        assert_covers_zoom_range(0, 6, "landcover", &[&glacier1, &glacier2, &glacier3]);
    }

    #[test]
    fn test_natural_earth_antarctic_ice_shelves() {
        let t = LayerTest::new();
        let (f1, f2) =
            (ne_feature("ne_50m_antarctic_ice_shelves_polys"), ne_feature("ne_10m_antarctic_ice_shelves_polys"));
        let ice1 = t.process(&f1);
        let ice2 = t.process(&f2);
        let expected = tags! {"_layer" => "landcover", "subclass" => "ice_shelf", "class" => "ice", "_buffer" => 4.0};
        assert_features(0, std::slice::from_ref(&expected), &ice1);
        assert_features(0, std::slice::from_ref(&expected), &ice2);
        assert_covers_zoom_range(2, 6, "landcover", &[&ice1, &ice2]);
    }

    #[test]
    fn test_osm_landcover() {
        let t = LayerTest::new();
        assert_features(
            13,
            &[tags! {
                "_layer" => "landcover",
                "subclass" => "wood",
                "class" => "wood",
                "_minpixelsize" => 8.0,
                "_numpointsattr" => "_numpoints",
                "_minzoom" => 7,
                "_maxzoom" => 14,
            }],
            &t.process(&polygon_feature(tags! {"natural" => "wood"})),
        );
        assert_features(
            12,
            &[tags! {
                "_layer" => "landcover",
                "subclass" => "forest",
                "class" => "wood",
                "_minpixelsize" => 8.0,
                "_minzoom" => 7,
                "_maxzoom" => 14,
            }],
            &t.process(&polygon_feature(tags! {"landuse" => "forest"})),
        );
        assert_features(
            10,
            &[tags! {
                "_layer" => "landcover",
                "subclass" => "dune",
                "class" => "sand",
                "_minpixelsize" => 4.0,
                "_minzoom" => 7,
                "_maxzoom" => 14,
            }],
            &t.process(&polygon_feature(tags! {"natural" => "dune"})),
        );
        assert_features(10, &[], &t.process(&polygon_feature(tags! {"landuse" => "park"})));
    }

    fn feature(geom: Polygon<f64>, m: Tags) -> vector_tile::Feature {
        vt_feature("landcover", 1, &geom.into(), m, 0)
    }

    fn assert_merges(t: &LayerTest, expected: &[Tags], input: Vec<vector_tile::Feature>, zoom: u32) {
        let actual: Vec<Tags> = t.post_process("landcover", zoom, input).into_iter().map(|f| f.tags).collect();
        assert_eq!(expected.to_vec(), actual, "z{zoom}");
    }

    fn wood(n: i64) -> Tags {
        tags! {"_numpoints" => n, "subclass" => "wood"}
    }

    #[test]
    fn test_merge_forests_by_num_points_z9_to_13() {
        let t = LayerTest::new();
        let map = tags! {"subclass" => "wood"};
        let input = || {
            vec![
                feature(rectangle(10.0, 20.0), wood(48)),
                feature(rectangle(10.0, 20.0), wood(49)),
                feature(rectangle(12.0, 18.0), wood(50)),
                feature(rectangle(12.0, 18.0), wood(299)),
                feature(rectangle(12.0, 18.0), wood(300)),
                feature(rectangle(12.0, 18.0), wood(301)),
            ]
        };
        // don't merge any
        assert_merges(&t, &vec![map.clone(); 6], input(), 14);
        // < 300 - merge, >= 300 - don't merge
        assert_merges(&t, &vec![map.clone(); 3], input(), 13);
        // < 300 - merge, >= 300 - merge
        assert_merges(&t, &vec![map.clone(); 2], input(), 9);
    }

    #[test]
    fn test_merge_non_forests_below_z8() {
        let t = LayerTest::new();
        let map = tags! {"subclass" => "dune"};
        let input = || {
            vec![
                feature(rectangle(10.0, 20.0), tags! {"_numpoints" => 48, "subclass" => "dune"}),
                feature(rectangle(12.0, 18.0), tags! {"_numpoints" => 301, "subclass" => "dune"}),
            ]
        };
        assert_merges(&t, &vec![map.clone(); 2], input(), 8);
        assert_merges(&t, &vec![map.clone(); 1], input(), 7);
        assert_merges(&t, &vec![map.clone(); 2], input(), 6);
    }
}
