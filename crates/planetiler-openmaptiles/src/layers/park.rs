//! Port of `layers/Park.java`: defines the logic for generating map elements for designated parks polygons and their
//! label points in the `park` layer from source features.
//!
//! This is ported from the
//! [OpenMapTiles park sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/park).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::forwarding_profile::{Handler, LayerPostProcessor};
use planetiler_core::geo::geo_utils;
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::geo::geometry_type::GeometryType;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::sort_key::{SORT_KEY_BITS, SortKey};
use planetiler_core::util::translations::Translations;
use planetiler_core::vector_tile;

use crate::feature_merge;
use crate::generated::open_map_tiles_schema::park::*;
use crate::generated::tables::{
    OsmParkPolygon, OsmParkPolygonHandler, RowHandlers, WithBoundary, WithHistoric, WithLeisure, WithMaritime,
    WithName, WithProtectClass, WithProtectedArea, WithProtectionTitle, WithSource,
};
use crate::layer::Layer;
use crate::util::omt_language_utils;
use crate::util::utils::null_if_empty;

// constants for determining the minimum zoom level for a park label based on its area
static WORLD_AREA_FOR_70K_SQUARE_METERS: LazyLock<f64> =
    LazyLock::new(|| (geo_utils::meters_to_pixel_at_equator(0, 70_000f64.sqrt()) / 256.0).powi(2));
/// 2^14 tiles, 2^12 pixels per tile
const SMALLEST_PARK_WORLD_AREA: f64 = 1.0 / ((1u64 << 52) as f64); // 4^-26

fn protect_class(value: &str) -> Option<&'static str> {
    Some(match value {
        "1a" => "conservation",
        "1b" => "wilderness_preserve",
        "2" => "national_park",
        "3" => "conservation",
        "4" => "wildlife_refuge",
        "5" => "conservation",
        "6" => "sustainable",
        _ => return None,
    })
}

pub struct Park {
    translations: Arc<Translations>,
    stats: StatsRef,
}

impl Park {
    pub fn new(translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, stats: &StatsRef) -> Park {
        Park { translations: translations.clone(), stats: stats.clone() }
    }

    fn park_class(element: &OsmParkPolygon<'_>) -> String {
        let boundary = element.boundary();
        let leisure = element.leisure();
        if element.maritime() {
            "marine".into()
        } else if boundary.as_deref() == Some("national_park") {
            "national_park".into()
        } else if boundary.as_deref() == Some("protected_area") {
            null_if_empty(element.protected_area())
                .map(String::from)
                .or_else(|| null_if_empty(element.protect_class()).and_then(|c| protect_class(&c)).map(String::from))
                .or_else(|| null_if_empty(element.protection_title()).map(String::from))
                .unwrap_or_else(|| "protected_area".into())
        } else if leisure.as_deref() == Some("nature_reserve") {
            "nature_reserve".into()
        } else if leisure.as_deref() == Some("recreation_ground") {
            "recreation_ground".into()
        } else if element.historic().is_some_and(|h| !h.is_empty()) {
            "historic".into()
        } else {
            "nature_reserve".into()
        }
    }
}

fn get_min_zoom_for_area(area: f64) -> u32 {
    // sql filter:    area > 70000*2^(20-zoom_level)
    // simplifies to: zoom_level > 20 - log(area / 70000) / log(2)
    let minzoom = (20.0 - (area / *WORLD_AREA_FOR_70K_SQUARE_METERS).ln() / std::f64::consts::LN_2).floor();
    minzoom.clamp(5.0, 14.0) as u32
}

impl OsmParkPolygonHandler for Park {
    fn process_osm_park_polygon(&self, element: &OsmParkPolygon<'_>, features: &mut FeatureCollector<'_>) {
        let clazz = Self::park_class(element);
        let source = element.source();

        // park name label point (if it has one)
        let label = if element.name().is_some() {
            match source.area() {
                Ok(area) => Some((area, omt_language_utils::get_names_without_translations(source.tags()))),
                Err(e) => {
                    e.log_with_stats(
                        &*self.stats,
                        "omt_park_area",
                        &format!("Unable to get park area for {}", source.id()),
                    );
                    None
                }
            }
        } else {
            None
        };

        // park shape
        let outline = features
            .polygon(LAYER_NAME)
            .set_buffer_pixels(BUFFER_SIZE)
            .set_attr_with_minzoom(fields::CLASS, clazz.as_str(), 5)
            .set_min_pixel_size(2.0)
            .set_min_zoom(4);

        if let Some((area, names)) = label {
            let minzoom = get_min_zoom_for_area(area);
            outline.put_attrs_with_minzoom(names.iter().map(|(k, v)| (k.clone(), v)), 5);

            features
                .point_on_surface(LAYER_NAME)
                .set_buffer_pixels(256.0)
                .set_attr(fields::CLASS, clazz.as_str())
                .put_attrs(names)
                .put_attrs(omt_language_utils::get_names(source.tags(), Some(&self.translations)))
                .set_point_label_grid_pixel_size(14, 100.0)
                .set_sort_key(
                    SortKey::order_by_trues_first(clazz == "national_park")
                        .then_by_trues_first(source.has_tag("wikipedia") || source.has_tag("wikidata"))
                        .then_by_log(area, 1.0, SMALLEST_PARK_WORLD_AREA, 1 << ((SORT_KEY_BITS - 2) - 1))
                        .get(),
                )
                .set_min_zoom(minzoom);
        }
    }
}

impl LayerPostProcessor for Park {
    fn post_process(&self, zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        // infer the "rank" attribute from point ordering within each label grid square
        let mut counts: HashMap<i64, i64> = HashMap::new();
        for feature in items.iter_mut() {
            if feature.geometry.geom_type() == GeometryType::Point && feature.has_group() {
                let count = counts.entry(feature.group).or_insert(0);
                *count += 1;
                feature.tags.insert("rank".into(), (*count).into());
            }
        }
        if zoom <= 4 {
            *items = feature_merge::merge_overlapping_polygons(std::mem::take(items), 0.0)?;
        }
        Ok(())
    }
}

impl Handler for Park {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }
}

impl RowHandlers for Park {
    fn as_osm_park_polygon_handler(&self) -> Option<&dyn OsmParkPolygonHandler> {
        Some(self)
    }
}

impl Layer for Park {}

#[cfg(test)]
mod tests {
    use planetiler_core::geo::geo_utils;
    use planetiler_core::tags;
    use planetiler_core::value::Tags;

    use crate::test_support::*;

    #[test]
    fn test_national_park() {
        let t = LayerTest::new();
        assert_features(
            13,
            &[
                tags! {
                    "_layer" => "park",
                    "_type" => "polygon",
                    "class" => "national_park",
                    "name" => "Grand Canyon National Park",
                    "_minpixelsize" => 2.0,
                    "_minzoom" => 4,
                    "_maxzoom" => 14,
                },
                tags! {
                    "_layer" => "park",
                    "_type" => "point",
                    "class" => "national_park",
                    "name" => "Grand Canyon National Park",
                    "name_int" => "Grand Canyon National Park",
                    "name:latin" => "Grand Canyon National Park",
                    "_minzoom" => 5,
                    "_maxzoom" => 14,
                },
            ],
            &t.process(&polygon_feature(tags! {
                "boundary" => "national_park",
                "name" => "Grand Canyon National Park",
                "name:es" => "es name",
                "protection_title" => "National Park",
                "wikipedia" => "en:Grand Canyon National Park",
            })),
        );

        // needs a name
        assert_features(
            13,
            &[tags! {"_layer" => "park", "_type" => "polygon"}],
            &t.process(&polygon_feature(tags! {"boundary" => "national_park", "protection_title" => "National Park"})),
        );
    }

    #[test]
    fn test_smaller_park() {
        let t = LayerTest::new();
        let z11area = (geo_utils::meters_to_pixel_at_equator(0, 70_000f64.sqrt()) / 256.0).powi(2) * 2f64.powi(20 - 11);
        assert_features(
            13,
            &[
                tags! {
                    "_layer" => "park",
                    "_type" => "polygon",
                    "class" => "protected_area",
                    "name" => "Small park",
                    "_minpixelsize" => 2.0,
                    "_minzoom" => 4,
                    "_maxzoom" => 14,
                },
                tags! {
                    "_layer" => "park",
                    "_type" => "point",
                    "class" => "protected_area",
                    "name" => "Small park",
                    "name_int" => "Small park",
                    "_minzoom" => 11,
                    "_maxzoom" => 14,
                },
            ],
            &t.process(&polygon_feature_with_area(
                z11area,
                tags! {"boundary" => "protected_area", "name" => "Small park", "wikipedia" => "en:Small park"},
            )),
        );
        assert_features(
            13,
            &[
                tags! {"_layer" => "park", "_type" => "polygon"},
                tags! {"_layer" => "park", "_type" => "point", "_minzoom" => 5, "_maxzoom" => 14},
            ],
            &t.process(&polygon_feature_with_area(
                1.0,
                tags! {"boundary" => "protected_area", "name" => "Small park", "wikidata" => "Q123"},
            )),
        );
    }

    fn get_label_sort_key(t: &LayerTest, area: f64, tags: Tags) -> i32 {
        let feature = polygon_feature_with_area(area, tags);
        let collector = t.process(&feature);
        let mut iter = collector.iter();
        iter.next();
        iter.next().unwrap().get_sort_key()
    }

    #[test]
    fn test_sort_keys() {
        let t = LayerTest::new();
        assert_ascending(&[
            get_label_sort_key(&t, 1.0, tags! {"boundary" => "national_park", "name" => "a", "wikipedia" => "en:park"}),
            get_label_sort_key(
                &t,
                1e-10,
                tags! {"boundary" => "national_park", "name" => "a", "wikipedia" => "en:Park"},
            ),
            get_label_sort_key(&t, 1.0, tags! {"boundary" => "national_park", "name" => "a"}),
            get_label_sort_key(&t, 1e-10, tags! {"boundary" => "national_park", "name" => "a"}),
            get_label_sort_key(
                &t,
                1.0,
                tags! {"boundary" => "protected_area", "name" => "a", "wikipedia" => "en:park"},
            ),
            get_label_sort_key(
                &t,
                1e-10,
                tags! {"boundary" => "protected_area", "name" => "a", "wikipedia" => "en:Park"},
            ),
            get_label_sort_key(&t, 1.0, tags! {"boundary" => "protected_area", "name" => "a"}),
            get_label_sort_key(&t, 1e-10, tags! {"boundary" => "protected_area", "name" => "a"}),
        ]);
    }

    #[test]
    fn test_clazz() {
        let t = LayerTest::new();
        for (props, expected_clazz) in [
            (tags! {"boundary" => "national_park", "maritime" => "yes"}, "marine"),
            (tags! {"boundary" => "national_park"}, "national_park"),
            (tags! {"boundary" => "aboriginal_lands", "leisure" => "nature_reserve"}, "nature_reserve"),
            (tags! {"boundary" => "aboriginal_lands", "leisure" => "recreation_ground"}, "recreation_ground"),
            (tags! {"boundary" => "aboriginal_lands", "leisure" => "some_other_leisure_value"}, "nature_reserve"),
            (tags! {"historic" => "archaeological_site"}, "historic"),
            (tags! {"boundary" => "aboriginal_lands"}, "nature_reserve"),
            (tags! {"boundary" => "protected_area", "protected_area" => "forest_reserve"}, "forest_reserve"),
            (tags! {"boundary" => "protected_area", "protect_class" => "1a"}, "conservation"),
            (
                tags! {"boundary" => "protected_area", "protect_class" => "some_other_protect_class_value"},
                "protected_area",
            ),
            (tags! {"boundary" => "protected_area", "protection_title" => "National Forest"}, "National Forest"),
            (tags! {"boundary" => "protected_area"}, "protected_area"),
        ] {
            let feature = polygon_feature(props.clone());
            let collector = t.process(&feature);
            assert_features(
                13,
                &[tags! {"_layer" => "park", "_type" => "polygon", "class" => expected_clazz}],
                &collector,
            );
        }
    }
}
