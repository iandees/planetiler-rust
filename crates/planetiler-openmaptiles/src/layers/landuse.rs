//! Port of `layers/Landuse.java`: defines the logic for generating map elements for man-made land use polygons like
//! cemeteries, zoos, and hospitals in the `landuse` layer from source features.
//!
//! This is ported from the
//! [OpenMapTiles landuse sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/landuse).

use std::sync::{Arc, LazyLock};

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::forwarding_profile::{Handler, LayerPostProcessor};
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::parse;
use planetiler_core::util::translations::Translations;
use planetiler_core::util::zoom_function::ZoomFunction;
use planetiler_core::value::Value;
use planetiler_core::vector_tile;

use crate::feature_merge;
use crate::generated::open_map_tiles_schema::landuse::*;
use crate::generated::tables::{
    OsmLandusePolygon, OsmLandusePolygonHandler, RowHandlers, WithAmenity, WithLanduse, WithLeisure, WithPlace,
    WithTourism, WithWaterway,
};
use crate::layer::Layer;
use crate::open_map_tiles_profile::NaturalEarthProcessor;
use crate::util::utils::null_if_empty;

static MIN_PIXEL_SIZE_THRESHOLDS: LazyLock<ZoomFunction<f64>> =
    LazyLock::new(|| ZoomFunction::from_max_zoom_thresholds([(13, 4.0), (7, 2.0), (6, 1.0)]));

/// Java `MINDIST_AND_BUFFER_SIZES.ceilingEntry(zoom)`.
fn min_dist_and_buffer_size(zoom: u32) -> f64 {
    match zoom {
        0..=5 => 0.1,
        // there is quite huge jump between Z5:NE and Z6:OSM => bigger generalization needed to make the transition
        // more smooth
        6 => 0.5,
        7 => 0.25,
        8 => 0.125,
        _ => 0.1,
    }
}

fn is_z6_class(clazz: &str) -> bool {
    matches!(
        clazz,
        field_values::CLASS_RESIDENTIAL
            | field_values::CLASS_SUBURB
            | field_values::CLASS_QUARTER
            | field_values::CLASS_NEIGHBOURHOOD
    )
}

pub struct Landuse {}

impl Landuse {
    pub fn new(_translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, _stats: &StatsRef) -> Landuse {
        Landuse {}
    }
}

impl NaturalEarthProcessor for Landuse {
    fn process_natural_earth(&self, table: &str, feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if table == "ne_50m_urban_areas" {
            let scalerank = feature.get_tag("scalerank").and_then(parse::parse_double_or_null);
            let minzoom = if scalerank.is_some_and(|s| s <= 2.0) { 4 } else { 5 };
            features
                .polygon(LAYER_NAME)
                .set_buffer_pixels(BUFFER_SIZE)
                .set_attr(fields::CLASS, field_values::CLASS_RESIDENTIAL)
                .set_zoom_range(minzoom, 5);
        }
    }
}

impl OsmLandusePolygonHandler for Landuse {
    fn process_osm_landuse_polygon(&self, element: &OsmLandusePolygon<'_>, features: &mut FeatureCollector<'_>) {
        let clazz = null_if_empty(element.landuse())
            .or_else(|| null_if_empty(element.amenity()))
            .or_else(|| null_if_empty(element.leisure()))
            .or_else(|| null_if_empty(element.tourism()))
            .or_else(|| null_if_empty(element.place()))
            .or_else(|| null_if_empty(element.waterway()));
        if let Some(clazz) = clazz {
            let clazz: &str = if clazz == "grave_yard" { field_values::CLASS_CEMETERY } else { &clazz };
            let feature = features
                .polygon(LAYER_NAME)
                .set_buffer_pixels(BUFFER_SIZE)
                .set_attr(fields::CLASS, clazz)
                .set_min_zoom(if is_z6_class(clazz) { 6 } else { 9 });
            if clazz == field_values::CLASS_RESIDENTIAL {
                feature.set_min_pixel_size(0.1).set_pixel_tolerance(0.25);
            } else {
                feature.set_min_pixel_size_overrides(Some(MIN_PIXEL_SIZE_THRESHOLDS.clone()));
            }
        }
    }
}

impl LayerPostProcessor for Landuse {
    fn post_process(&self, zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        let (to_merge, mut result): (Vec<_>, Vec<_>) = std::mem::take(items).into_iter().partition(|item| {
            matches!(item.tags.get(fields::CLASS), Some(Value::String(s)) if &**s == field_values::CLASS_RESIDENTIAL)
        });
        let merged = if zoom <= 12 {
            let min_dist_and_buffer = min_dist_and_buffer_size(zoom);
            feature_merge::merge_nearby_polygons(to_merge, 1.0, 1.0, min_dist_and_buffer, min_dist_and_buffer)?
        } else {
            // reduces size of some heavy z13-14 tiles with lots of small polygons
            feature_merge::merge_multi_polygon(to_merge)
        };
        result.extend(merged);
        *items = result;
        Ok(())
    }
}

impl Handler for Landuse {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }
}

impl RowHandlers for Landuse {
    fn as_osm_landuse_polygon_handler(&self) -> Option<&dyn OsmLandusePolygonHandler> {
        Some(self)
    }
}

impl Layer for Landuse {
    fn as_natural_earth_processor(&self) -> Option<&dyn NaturalEarthProcessor> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use geo::GeometryCollection;
    use geo_types::Geometry;
    use planetiler_core::geo::geo_utils;
    use planetiler_core::geo::test_utils::{rectangle, rectangle_xy};
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;

    use super::LAYER_NAME;
    use crate::open_map_tiles_profile::NATURAL_EARTH_SOURCE;
    use crate::test_support::*;

    #[test]
    fn test_natural_earth_urban_areas() {
        let t = LayerTest::new();
        let ne = |scalerank: f64| {
            SimpleFeature::create(
                geo_utils::world_to_lat_lon_coords(&rectangle(0.0, 1.0).into()),
                tags! {"scalerank" => scalerank},
                Some(NATURAL_EARTH_SOURCE),
                Some("ne_50m_urban_areas"),
                0,
            )
        };
        assert_features(
            0,
            &[tags! {"_layer" => "landuse", "class" => "residential", "_buffer" => 4.0, "_minzoom" => 4}],
            &t.process(&ne(1.9)),
        );
        assert_features(
            0,
            &[tags! {"_layer" => "landuse", "class" => "residential", "_buffer" => 4.0, "_minzoom" => 5}],
            &t.process(&ne(2.1)),
        );
    }

    #[test]
    fn test_osm_landuse() {
        let t = LayerTest::new();
        assert_features(
            13,
            &[
                tags! {"_layer" => "poi"},
                tags! {
                    "_layer" => "landuse",
                    "class" => "railway",
                    "_minpixelsize" => 4.0,
                    "_minzoom" => 9,
                    "_maxzoom" => 14,
                },
            ],
            &t.process(&polygon_feature(tags! {"landuse" => "railway", "amenity" => "school"})),
        );
        assert_features(
            13,
            &[
                tags! {"_layer" => "poi"},
                tags! {
                    "_layer" => "landuse",
                    "class" => "school",
                    "_minpixelsize" => 4.0,
                    "_minzoom" => 9,
                    "_maxzoom" => 14,
                },
            ],
            &t.process(&polygon_feature(tags! {"amenity" => "school"})),
        );
    }

    /// Same as [`test_osm_landuse`] but only checks the landuse layer (runs while the poi layer is a stub).
    #[test]
    fn test_osm_landuse_only_landuse_layer() {
        let t = LayerTest::new();
        let check = |input, expected| {
            let collector = t.process(&input);
            assert_features(13, &[expected], collector.iter().filter(|f| f.get_layer() == "landuse"));
        };
        check(
            polygon_feature(tags! {"landuse" => "railway", "amenity" => "school"}),
            tags! {"_layer" => "landuse", "class" => "railway", "_minpixelsize" => 4.0, "_minzoom" => 9, "_maxzoom" => 14},
        );
        check(polygon_feature(tags! {"amenity" => "grave_yard"}), tags! {"_layer" => "landuse", "class" => "cemetery"});
    }

    #[test]
    fn test_grave_yard_becomes_cemetery() {
        let t = LayerTest::new();
        assert_features(
            14,
            &[tags! {"_layer" => "poi"}, tags! {"_layer" => "landuse", "class" => "cemetery"}],
            &t.process(&polygon_feature(tags! {"amenity" => "grave_yard"})),
        );
    }

    #[test]
    fn test_osm_landuse_lower_zoom() {
        let t = LayerTest::new();
        assert_features(
            6,
            &[
                tags! {"_layer" => "landuse", "class" => "suburb", "_minzoom" => 6, "_maxzoom" => 14, "_minpixelsize" => 1.0},
            ],
            &t.process(&polygon_feature(tags! {"place" => "suburb"})),
        );
        assert_features(
            7,
            &[tags! {
                "_layer" => "landuse",
                "class" => "residential",
                "_minzoom" => 6,
                "_maxzoom" => 14,
                "_minpixelsize" => 0.1,
            }],
            &t.process(&polygon_feature(tags! {"landuse" => "residential"})),
        );
    }

    fn num_geometries(g: &Geometry<f64>) -> usize {
        match g {
            Geometry::MultiPolygon(m) => m.0.len(),
            Geometry::MultiLineString(m) => m.0.len(),
            Geometry::MultiPoint(m) => m.0.len(),
            Geometry::GeometryCollection(GeometryCollection(c)) => c.len(),
            _ => 1,
        }
    }

    #[test]
    fn test_merge_polygons_z12() {
        let t = LayerTest::new();
        let poly1 = vt_feature(LAYER_NAME, 1, &rectangle(10.0, 20.0).into(), tags! {"class" => "residential"}, 0);
        let poly2 = vt_feature(
            LAYER_NAME,
            1,
            &rectangle_xy(20.0, 10.0, 22.0, 20.0).into(),
            tags! {"class" => "residential"},
            0,
        );
        let poly3 =
            vt_feature(LAYER_NAME, 1, &rectangle_xy(10.0, 20.0, 20.0, 22.0).into(), tags! {"class" => "suburb"}, 0);
        let counts = |zoom| {
            t.post_process(LAYER_NAME, zoom, vec![poly1.clone(), poly2.clone(), poly3.clone()])
                .iter()
                .map(|d| num_geometries(&d.geometry.decode().unwrap()))
                .collect::<Vec<_>>()
        };
        assert_eq!(vec![1, 2], counts(13));
        assert_eq!(vec![1, 1], counts(12));
    }
}
