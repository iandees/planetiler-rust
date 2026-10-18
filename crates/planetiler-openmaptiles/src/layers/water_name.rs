//! Port of `layers/WaterName.java`: defines the logic for generating map elements for ocean and lake names in the
//! `water_name` layer from source features.
//!
//! This is ported from the
//! [OpenMapTiles water_name sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/water_name).
//!
//! Labels for lakes and oceans come primarily from OpenStreetMap data, but we also join with the lake centerlines
//! source to get linestring geometries for prominent lakes. We also join with natural earth to make certain
//! important lake/ocean labels visible at lower zoom levels.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use geo::{Distance, Euclidean};
use geo_types::Geometry;
use parking_lot::RwLock;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::{Feature, FeatureCollector};
use planetiler_core::forwarding_profile::Handler;
use planetiler_core::geo::geo_utils;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::parse;
use planetiler_core::util::translations::Translations;
use planetiler_core::value::Value;

use crate::generated::open_map_tiles_schema::water_name::*;
use crate::generated::tables::*;
use crate::layer::Layer;
use crate::open_map_tiles_profile::{LakeCenterlineProcessor, NaturalEarthProcessor};
use crate::util::omt_language_utils;
use crate::util::utils::null_if_empty;

const MINZOOM_BAY: u32 = 9;
const MINZOOM_LAKE: u32 = 3;
const MINZOOM_SEA_AND_OCEAN: u32 = 0;

fn important_marine_regions_join_distance() -> f64 {
    geo_utils::meters_to_pixel_at_equator(0, 50_000.0) / 256.0
}

#[derive(Clone, Debug)]
struct NaturalEarthRegion {
    geometry: Geometry<f64>,
    scalerank: i32,
}

pub struct WaterName {
    translations: Arc<Translations>,
    stats: StatsRef,
    lake_centerlines: RwLock<HashMap<i64, Geometry<f64>>>,
    important_marine_points: RwLock<BTreeMap<String, NaturalEarthRegion>>,
}

/// Java `String.toLowerCase()`.
fn lower(s: &str) -> String {
    s.to_lowercase()
}

impl WaterName {
    pub fn new(translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, stats: &StatsRef) -> WaterName {
        WaterName {
            translations: translations.clone(),
            stats: stats.clone(),
            lake_centerlines: RwLock::new(HashMap::new()),
            important_marine_points: RwLock::new(BTreeMap::new()),
        }
    }

    fn get_important_marine_region(&self, element: &OsmMarinePoint<'_>) -> Option<i32> {
        let source = element.source;
        let name = lower(element.name().as_deref().unwrap_or(""));
        let points = self.important_marine_points.read();
        let lookup_tag = |key: &str| {
            let value = source.get_tag(key).map(|v| lower(&v.to_string_lossy())).unwrap_or_default();
            points.get(&value)
        };
        let result =
            points.get(&name).or_else(|| lookup_tag("name:en")).or_else(|| lookup_tag("name:es")).or_else(|| {
                points.range(name.clone()..).next().filter(|(k, _)| k.starts_with(&name)).map(|(_, v)| v)
            })?;
        match source.world_geometry() {
            Ok(geom) => {
                let distance = Euclidean.distance(&result.geometry, geom);
                (distance <= important_marine_regions_join_distance()).then_some(result.scalerank)
            }
            Err(e) => {
                e.log_with_stats(
                    &*self.stats,
                    "osm_marine_point",
                    &format!("Error getting geometry for OSM marine point {}", source.id()),
                );
                None
            }
        }
    }

    fn setup_osm_water_polygon_feature<'f, 'a>(
        &self,
        element: &OsmWaterPolygon<'_>,
        output: &'f mut Feature<'a>,
        clazz: &'static str,
        minzoom: u32,
    ) -> &'f mut Feature<'a> {
        output
            .set_attr(fields::CLASS, clazz)
            .set_buffer_pixels(BUFFER_SIZE)
            .put_attrs(omt_language_utils::get_names(element.source.tags(), Some(&self.translations)))
            .set_attr(fields::INTERMITTENT, if element.is_intermittent() { 1 } else { 0 })
            .set_min_zoom(minzoom)
    }
}

impl LakeCenterlineProcessor for WaterName {
    fn process_lake_centerline(&self, feature: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {
        // TODO pull lake centerline computation into planetiler?
        let osm_id = feature.get_long("OSM_ID").abs();
        if osm_id == 0 {
            tracing::warn!("Bad lake centerline. Tags: {:?}", feature.tags());
            return;
        }
        match feature.world_geometry() {
            Ok(geom) => {
                // multiple threads call this concurrently
                let mut lines = self.lake_centerlines.write();
                // if we already have a centerline for this OSM_ID, then merge the existing one with this one
                let new_geometry = match lines.remove(&osm_id) {
                    Some(old) => geo_utils::combine(vec![old, geom.clone()]),
                    None => geom.clone(),
                };
                lines.insert(osm_id, new_geometry);
            }
            Err(e) => {
                e.log_with_stats(&*self.stats, "omt_water_name_lakeline", &format!("Bad lake centerline: {feature:?}"))
            }
        }
    }
}

impl NaturalEarthProcessor for WaterName {
    fn process_natural_earth(&self, table: &str, feature: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {
        // use natural earth named polygons just as a source of name to zoom-level mappings for later
        if table == "ne_10m_geography_marine_polys" {
            let name = feature.get_tag("name").filter(|v| !v.is_null());
            let scalerank = feature.get_tag("scalerank").and_then(parse::parse_int_or_null);
            if let (Some(name), Some(scalerank)) = (name, scalerank) {
                let name = lower(name.to_string_lossy().split_whitespace().collect::<Vec<_>>().join(" ").as_str());
                match feature.world_geometry() {
                    Ok(geom) => {
                        self.important_marine_points
                            .write()
                            .insert(name, NaturalEarthRegion { geometry: geom.clone(), scalerank });
                    }
                    Err(e) => e.log_with_stats(
                        &*self.stats,
                        "ne_marine_polys",
                        &format!(
                            "Error getting geometry for natural earth feature {table} {}",
                            feature.get_tag("ogc_fid").unwrap_or(&Value::Null)
                        ),
                    ),
                }
            }
        }
    }
}

impl OsmMarinePointHandler for WaterName {
    fn process_osm_marine_point(&self, element: &OsmMarinePoint<'_>, features: &mut FeatureCollector<'_>) {
        if element.name().is_some_and(|n| !n.trim().is_empty()) {
            let natural = element.natural();
            let place = element.place();
            let clazz = null_if_empty(natural.as_deref()).or(null_if_empty(place.as_deref()));
            let source = element.source;
            // use name from OSM, but get min zoom from natural earth based on fuzzy name match...
            let mut rank = source.get_tag("rank").and_then(parse::parse_int_or_null);
            if let Some(scalerank) = self.get_important_marine_region(element) {
                rank = Some(scalerank);
            }
            let min_zoom = if place.as_deref() == Some("ocean") {
                0
            } else if let Some(rank) = rank {
                rank.max(0) as u32
            } else if natural.as_deref() == Some("bay") {
                13
            } else {
                8
            };
            features
                .point(LAYER_NAME)
                .set_buffer_pixels(BUFFER_SIZE)
                .put_attrs(omt_language_utils::get_names(source.tags(), Some(&self.translations)))
                .set_attr(fields::CLASS, clazz)
                .set_attr(fields::INTERMITTENT, if element.is_intermittent() { 1 } else { 0 })
                .set_min_zoom(min_zoom);
        }
    }
}

impl OsmWaterPolygonHandler for WaterName {
    fn process_osm_water_polygon(&self, element: &OsmWaterPolygon<'_>, features: &mut FeatureCollector<'_>) {
        let name = element.name();
        let Some(name) = null_if_empty(name.as_deref()) else {
            return;
        };
        if element.leisure().as_deref() == Some("swimming_pool") {
            return;
        }
        let centerline_geometry = {
            let lines = self.lake_centerlines.read();
            if lines.is_empty() { None } else { lines.get(&element.source.id()).cloned() }
        };
        let mut minzoom_cl = MINZOOM_BAY;
        let place = element.place();
        let place = place.as_deref();
        let clazz = if element.natural().as_deref() == Some("bay") {
            field_values::CLASS_BAY
        } else if place == Some("sea") {
            field_values::CLASS_SEA
        } else {
            minzoom_cl = MINZOOM_LAKE;
            field_values::CLASS_LAKE
        };
        let has_centerline = centerline_geometry.is_some();
        if let Some(centerline) = centerline_geometry {
            // prefer lake centerline if it exists, but point will be also used if minzoom below 9 is calculated from
            // area. note: Here we're diverging from OpenMapTiles: For bays with minzoom (based on area) point is used
            // between minzoom and Z8 and for Z9+ centerline is used, while OpenMaptiles sticks with points.
            let name_length = name.encode_utf16().count() as f64;
            let feature = features.geometry(LAYER_NAME, centerline);
            self.setup_osm_water_polygon_feature(element, feature, clazz, minzoom_cl)
                .set_min_pixel_size_below_zoom(13, 6.0 * name_length);
        }

        let minzoom = if matches!(place, Some("sea") | Some("ocean")) { MINZOOM_SEA_AND_OCEAN } else { MINZOOM_LAKE };
        if !has_centerline || minzoom < minzoom_cl {
            // use a label point inside the lake but ...
            // ... if centerline already created, adjust maxzoom here to make sure we're not having both at same zoom
            // level
            let maxzoom = if has_centerline { minzoom_cl - 1 } else { 14 };
            let feature = features.point_on_surface(LAYER_NAME);
            self.setup_osm_water_polygon_feature(element, feature, clazz, minzoom)
                .set_max_zoom(maxzoom)
                // Show a label if a water feature covers at least 1/4 of a tile or z14+
                .set_min_pixel_size_below_zoom(13, 128.0);
        }
    }
}

impl Handler for WaterName {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn release(&self) {
        self.lake_centerlines.write().clear();
        self.important_marine_points.write().clear();
    }
}

impl RowHandlers for WaterName {
    fn as_osm_marine_point_handler(&self) -> Option<&dyn OsmMarinePointHandler> {
        Some(self)
    }

    fn as_osm_water_polygon_handler(&self) -> Option<&dyn OsmWaterPolygonHandler> {
        Some(self)
    }
}

impl Layer for WaterName {
    fn as_natural_earth_processor(&self) -> Option<&dyn NaturalEarthProcessor> {
        Some(self)
    }

    fn as_lake_centerline_processor(&self) -> Option<&dyn LakeCenterlineProcessor> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use planetiler_core::geo::test_utils::{assert_same_normalized_feature, new_line_string, rectangle};
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;
    use planetiler_core::value::Tags;

    use super::*;
    use crate::open_map_tiles_profile::{LAKE_CENTERLINE_SOURCE, NATURAL_EARTH_SOURCE, OSM_SOURCE};
    use crate::test_support::*;

    fn lake_centerline(coords: &[f64]) -> SimpleFeature {
        SimpleFeature::create(
            new_line_string(coords),
            tags! {"OSM_ID" => -10},
            Some(LAKE_CENTERLINE_SOURCE),
            None::<&str>,
            0,
        )
    }

    fn osm(geom: Geometry<f64>, tags: Tags, id: i64) -> SimpleFeature {
        SimpleFeature::create(geom, tags, Some(OSM_SOURCE), None::<&str>, id)
    }

    fn lat_lon_rect(size: f64) -> Geometry<f64> {
        geo_utils::world_to_lat_lon_coords(&rectangle(0.0, size).into())
    }

    fn water_name_geometry<'a>(features: &'a FeatureCollector<'a>, ty: &str) -> &'a Geometry<f64> {
        features
            .iter()
            .find(|f| f.get_layer() == LAYER_NAME && to_map(f, 14).get("_type").and_then(Value::as_str) == Some(ty))
            .unwrap()
            .get_geometry()
    }

    #[test]
    fn test_water_name_point() {
        let t = LayerTest::new();
        assert_features(
            11,
            &[
                tags! {"_layer" => "water"},
                tags! {
                    "class" => "lake", "name" => "waterway", "name:es" => "waterway es", "intermittent" => 1,
                    "_layer" => "water_name", "_type" => "point", "_minzoom" => 3, "_maxzoom" => 14
                },
            ],
            &t.process(&polygon_feature_with_area(
                1.0,
                tags! {"name" => "waterway", "name:es" => "waterway es", "natural" => "water", "water" => "pond", "intermittent" => "1"},
            )),
        );
    }

    #[test]
    fn test_water_name_lakeline() {
        let t = LayerTest::new();
        assert_features(11, &[], &t.process(&lake_centerline(&[0.0, 0.0, 1.0, 1.0])));
        let f = osm(
            lat_lon_rect(1e-7f64.sqrt()),
            tags! {"name" => "waterway", "name:es" => "waterway es", "natural" => "water", "water" => "pond"},
            10,
        );
        let features = t.process(&f);
        assert_features(
            10,
            &[
                tags! {"_layer" => "water"},
                tags! {
                    "name" => "waterway", "name:es" => "waterway es", "_layer" => "water_name", "_type" => "line",
                    "_minzoom" => 3, "_maxzoom" => 14, "_minpixelsize" => 8.0 * 6.0
                },
            ],
            &features,
        );
        assert_same_normalized_feature(
            &geo_utils::lat_lon_to_world_coords(&new_line_string(&[0.0, 0.0, 1.0, 1.0]).into()),
            water_name_geometry(&features, "line"),
        );
    }

    #[test]
    fn test_water_name_multiple_lakelines() {
        let t = LayerTest::new();
        assert_features(11, &[], &t.process(&lake_centerline(&[0.0, 0.0, 1.0, 1.0])));
        assert_features(11, &[], &t.process(&lake_centerline(&[2.0, 2.0, 3.0, 3.0])));
        let f = osm(
            lat_lon_rect(1e-7f64.sqrt()),
            tags! {"name" => "waterway", "name:es" => "waterway es", "natural" => "water", "water" => "pond"},
            10,
        );
        let features = t.process(&f);
        assert_features(
            10,
            &[
                tags! {"_layer" => "water"},
                tags! {
                    "name" => "waterway", "name:es" => "waterway es", "_layer" => "water_name",
                    "_minzoom" => 3, "_maxzoom" => 14, "_minpixelsize" => 8.0 * 6.0
                },
            ],
            &features,
        );
        let expected =
            geo_utils::lat_lon_to_world_coords(&Geometry::GeometryCollection(geo_types::GeometryCollection(vec![
                new_line_string(&[0.0, 0.0, 1.0, 1.0]).into(),
                new_line_string(&[2.0, 2.0, 3.0, 3.0]).into(),
            ])));
        let actual = features.iter().find(|f| f.get_layer() == LAYER_NAME).unwrap().get_geometry();
        assert_same_normalized_feature(&expected, actual);
    }

    #[test]
    fn test_water_name_bay_small() {
        let t = LayerTest::new();
        assert_features(11, &[], &t.process(&lake_centerline(&[0.0, 0.0, 1.0, 1.0])));
        let f =
            osm(lat_lon_rect(1e-7f64.sqrt()), tags! {"name" => "bay", "name:es" => "bay es", "natural" => "bay"}, 10);
        let features = t.process(&f);
        assert_features(
            10,
            &[
                tags! {
                    "name" => "bay", "name:es" => "bay es", "_layer" => "water_name", "_type" => "line",
                    "_minzoom" => 9, "_maxzoom" => 14, "_minpixelsize" => 3.0 * 6.0
                },
                tags! {
                    "name" => "bay", "name:es" => "bay es", "_layer" => "water_name", "_type" => "point",
                    "_minzoom" => 3, "_maxzoom" => 8, "_minpixelsize" => 128.0
                },
            ],
            &features,
        );
        assert_same_normalized_feature(
            &geo_utils::lat_lon_to_world_coords(&new_line_string(&[0.0, 0.0, 1.0, 1.0]).into()),
            water_name_geometry(&features, "line"),
        );
    }

    #[test]
    fn test_water_name_bay_big() {
        let t = LayerTest::new();
        assert_features(11, &[], &t.process(&lake_centerline(&[0.0, 0.0, 1.0, 1.0])));
        let f = osm(lat_lon_rect(1.0), tags! {"name" => "bay", "name:es" => "bay es", "natural" => "bay"}, 10);
        assert_features(
            10,
            &[
                tags! {"name" => "bay", "name:es" => "bay es", "_layer" => "water_name", "_type" => "line", "_minzoom" => 9, "_maxzoom" => 14},
                tags! {"name" => "bay", "name:es" => "bay es", "_layer" => "water_name", "_type" => "point", "_minzoom" => 3, "_maxzoom" => 8},
            ],
            &t.process(&f),
        );
    }

    #[test]
    fn test_marine_point() {
        let t = LayerTest::new();
        let ne = SimpleFeature::create(
            new_line_string(&[0.0, 0.0, 1.0, 1.0]),
            tags! {"scalerank" => 1, "name" => "Black sea"},
            Some(NATURAL_EARTH_SOURCE),
            Some("ne_10m_geography_marine_polys"),
            0,
        );
        assert_features(11, &[], &t.process(&ne));

        // name match - use scale rank from NE
        assert_features(
            10,
            &[
                tags! {"name" => "Black Sea", "name:es" => "Mar Negro", "_layer" => "water_name", "_type" => "point", "_minzoom" => 1, "_maxzoom" => 14},
            ],
            &t.process(&point_feature(
                tags! {"rank" => 9, "name" => "Black Sea", "name:es" => "Mar Negro", "place" => "sea"},
            )),
        );
        // name match but ocean - use min zoom=0
        assert_features(
            10,
            &[tags! {"_layer" => "water_name", "_type" => "point", "_minzoom" => 0, "_maxzoom" => 14}],
            &t.process(&point_feature(tags! {"rank" => 9, "name" => "Black Sea", "place" => "ocean"})),
        );
        // no name match - use OSM rank
        assert_features(
            10,
            &[tags! {"_layer" => "water_name", "_type" => "point", "_minzoom" => 9, "_maxzoom" => 14}],
            &t.process(&point_feature(tags! {"rank" => 9, "name" => "Atlantic", "place" => "sea"})),
        );
        // no rank at all, default to 8
        assert_features(
            10,
            &[tags! {"_layer" => "water_name", "_type" => "point", "_minzoom" => 8, "_maxzoom" => 14}],
            &t.process(&point_feature(tags! {"name" => "Atlantic", "place" => "sea"})),
        );
    }

    #[test]
    fn test_swimming_pool_with_name() {
        let t = LayerTest::new();
        assert_features(
            14,
            &[tags! {"_layer" => "poi"}, tags! {"_layer" => "water"}],
            &t.process(&polygon_feature_with_area(1.0, tags! {"name" => "test", "leisure" => "swimming_pool"})),
        );
    }

    /// `test_swimming_pool_with_name` without the poi layer: no water_name label for swimming pools.
    #[test]
    fn test_swimming_pool_with_name_has_no_water_name() {
        let t = LayerTest::new();
        let f = polygon_feature_with_area(1.0, tags! {"name" => "test", "leisure" => "swimming_pool"});
        let features = t.process(&f);
        assert!(features.iter().all(|f| f.get_layer() != LAYER_NAME));
        assert!(features.iter().any(|f| f.get_layer() == "water"));
    }
}
