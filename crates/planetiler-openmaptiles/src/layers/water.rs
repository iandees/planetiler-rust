//! Port of `layers/Water.java`: defines the logic for generating map elements for oceans and lakes in the `water`
//! layer from source features.
//!
//! This is ported from the
//! [OpenMapTiles water sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/water).
//!
//! At low zoom levels, use natural earth for oceans and major lakes, and at high zoom levels use OpenStreetMap data.
//! OpenStreetMap data contains smaller bodies of water, but not large ocean polygons. For oceans, use
//! <https://osmdata.openstreetmap.de/data/water-polygons.html> which infers ocean polygons by preprocessing all
//! coastline elements.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use geo::{Area, Intersects};
use geo_types::Geometry;
use parking_lot::{Mutex, RwLock};
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::expression::multi_expression::Index;
use planetiler_core::feature_collector::{self, FeatureCollector};
use planetiler_core::forwarding_profile::{FinishHandler, Handler, LayerPostProcessor};
use planetiler_core::geo::geometry_exception::{GeometryException, GeometryResult};
use planetiler_core::geo::is_valid::is_valid;
use planetiler_core::geo::polygon_index::PolygonIndex;
use planetiler_core::geo::polygon_overlay;
use planetiler_core::profile::FeatureConsumer;
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;
use planetiler_core::value::Value;
use planetiler_core::vector_tile;

use crate::feature_merge;
use crate::generated::open_map_tiles_schema::water::*;
use crate::generated::tables::*;
use crate::layer::Layer;
use crate::open_map_tiles_profile::{NaturalEarthProcessor, OSM_SOURCE, OsmWaterPolygonProcessor};
use crate::util::utils;

/// Smallest NE lake is around 4.42E-13, smallest matching OSM lake is 9.34E-13, this is slightly below that and
/// approx. 33% of OSM features are smaller than this, hence to save some CPU cycles.
const OSM_ID_MATCH_AREA_LIMIT: f64 = 9.094947017729282e-13; // 4^-20

static CLASS_MAPPING: LazyLock<Index<&'static str>> = LazyLock::new(|| field_mappings::CLASS.index());

/// Information to hold onto from processing an NE lake to determine OSM ID later.
#[derive(Debug)]
struct LakeInfo {
    min_zoom: u32,
    max_zoom: u32,
    clazz: &'static str,
    geom: Geometry<f64>,
    area: f64,
    /// `(osm_id, intersection area)` of the best matching OSM lake so far.
    osm_id: Mutex<(Option<i64>, f64)>,
}

impl LakeInfo {
    fn merge_id(&self, new_id: i64, new_area: f64) {
        let mut guard = self.osm_id.lock();
        if new_area > guard.1 {
            *guard = (Some(new_id), new_area);
        }
    }
}

pub struct Water {
    config: Arc<PlanetilerConfig>,
    stats: StatsRef,
    ne_lake_index: RwLock<Option<PolygonIndex<Arc<LakeInfo>>>>,
    ne_lake_name_maps: Mutex<HashMap<String, HashMap<String, Arc<LakeInfo>>>>,
    ne_all_lake_infos: Mutex<Vec<Arc<LakeInfo>>>,
}

impl Water {
    pub fn new(_translations: &Arc<Translations>, config: &Arc<PlanetilerConfig>, stats: &StatsRef) -> Water {
        LazyLock::force(&CLASS_MAPPING);
        Water {
            config: config.clone(),
            stats: stats.clone(),
            ne_lake_index: RwLock::new(Some(PolygonIndex::create())),
            ne_lake_name_maps: Mutex::new(HashMap::new()),
            ne_all_lake_infos: Mutex::new(Vec::new()),
        }
    }

    fn setup_ne_water_feature(
        features: &mut FeatureCollector<'_>,
        min_zoom: u32,
        max_zoom: u32,
        clazz: &'static str,
        osm_id: Option<i64>,
    ) {
        features
            .polygon(LAYER_NAME)
            .set_buffer_pixels(BUFFER_SIZE)
            .set_zoom_range(min_zoom, max_zoom)
            .set_attr(fields::CLASS, clazz)
            .set_attr(fields::ID, osm_id);
    }

    fn process_ne_lake(
        &self,
        table: &str,
        feature: &dyn SourceFeature,
        min_zoom: u32,
        max_zoom: u32,
    ) -> GeometryResult<()> {
        let geom = feature.world_geometry()?;
        let geom = if is_valid(geom) {
            geom.clone()
        } else {
            tracing::trace!("Fixing geometry of NE lake {}", feature.get_long("ne_id"));
            polygon_overlay::geometry_fixer(geom)?
        };
        let name = feature.get_tag("name").filter(|v| !v.is_null()).map(|v| v.to_string_lossy().into_owned());
        let area = geom.unsigned_area();
        let lake_info = Arc::new(LakeInfo {
            min_zoom,
            max_zoom,
            clazz: field_values::CLASS_LAKE,
            geom,
            area,
            osm_id: Mutex::new((None, 0.0)),
        });
        self.ne_all_lake_infos.lock().push(lake_info.clone());
        if let Some(index) = self.ne_lake_index.read().as_ref() {
            index.put(&lake_info.geom, lake_info.clone());
        }
        let mut maps = self.ne_lake_name_maps.lock();
        let map = maps.entry(table.to_string()).or_default();
        if let Some(name) = name {
            // on name collision, bigger lake gets on the name list
            match map.get(&name) {
                Some(prev) if prev.area >= lake_info.area => {}
                _ => {
                    map.insert(name, lake_info);
                }
            }
        }
        Ok(())
    }

    fn attempt_ne_lake_id_mapping(&self, element: &OsmWaterPolygon<'_>) -> GeometryResult<()> {
        // if OSM lake is too small for Z6 (e.g. area below ~4px) we assume there is no matching NE lake
        let source = element.source;
        let geom = source.world_geometry()?;
        if geom.unsigned_area() < OSM_ID_MATCH_AREA_LIMIT {
            return Ok(());
        }
        let fixed;
        let geom = if is_valid(geom) {
            geom
        } else {
            fixed = polygon_overlay::geometry_fixer(geom)?;
            self.stats.data_error("omt_fix_water_before_ne_intersect");
            tracing::trace!(
                "Fixing geometry of OSM element {} before attempt to add ID to natural earth water feature",
                source.id()
            );
            &fixed
        };

        // match by name:
        let mut matched = false;
        if let Some(name) = element.name() {
            let candidates: Vec<Arc<LakeInfo>> =
                self.ne_lake_name_maps.lock().values().filter_map(|m| m.get(name.as_ref()).cloned()).collect();
            for lake_info in candidates {
                matched = true;
                self.fill_osm_id_into_ne_lake(source.id(), geom, &lake_info, true)?;
            }
        }
        if matched {
            return Ok(());
        }

        // match by intersection:
        let items = match self.ne_lake_index.read().as_ref() {
            Some(index) => index.get_intersecting(geom),
            None => vec![],
        };
        for lake_info in items {
            self.fill_osm_id_into_ne_lake(source.id(), geom, &lake_info, false)?;
        }
        Ok(())
    }

    /// When we match lakes with the index then `intersects_check_needed` should be `false`, otherwise `true`, to make
    /// sure we DO check the intersection but to avoid checking it twice.
    fn fill_osm_id_into_ne_lake(
        &self,
        osm_id: i64,
        geom: &Geometry<f64>,
        lake_info: &LakeInfo,
        intersects_check_needed: bool,
    ) -> GeometryResult<()> {
        let ne_geom = &lake_info.geom;
        if intersects_check_needed && !ne_geom.intersects(geom) {
            return Ok(());
        }
        let intersection = polygon_overlay::intersection(ne_geom, geom)?;
        // Should match following in OpenMapTiles: Distinct on keeps just the first occurrence -> order by 'area_ratio
        // DESC'. With a twist: NE geometry is always the same, hence we can make it a little bit faster by dropping
        // "ratio" and compare only the intersection area: bigger area -> bigger ratio.
        lake_info.merge_id(osm_id, intersection.unsigned_area());
        Ok(())
    }
}

impl NaturalEarthProcessor for Water {
    fn process_natural_earth(&self, table: &str, feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        let ocean = match table {
            "ne_110m_ocean" => Some((0, 1)),
            "ne_50m_ocean" => Some((2, 4)),
            "ne_10m_ocean" => Some((5, 5)),
            _ => None,
        };
        if let Some((min_zoom, max_zoom)) = ocean {
            Self::setup_ne_water_feature(features, min_zoom, max_zoom, field_values::CLASS_OCEAN, None);
            return;
        }

        let lake = match table {
            "ne_110m_lakes" => Some((0, 1)),
            "ne_50m_lakes" => Some((2, 3)),
            "ne_10m_lakes" => Some((4, 5)),
            _ => None,
        };
        if let Some((min_zoom, max_zoom)) = lake
            && let Err(e) = self.process_ne_lake(table, feature, min_zoom, max_zoom)
        {
            e.log_with_stats(
                &*self.stats,
                "omt_water_ne",
                &format!(
                    "Error getting geometry for natural earth feature {table} {}",
                    feature.get_tag("ogc_fid").unwrap_or(&Value::Null)
                ),
            );
            // make sure we have this NE lake even if without OSM ID
            Self::setup_ne_water_feature(features, min_zoom, max_zoom, field_values::CLASS_LAKE, None);
        }
    }
}

impl OsmWaterPolygonProcessor for Water {
    fn process_osm_water(&self, _feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        features
            .polygon(LAYER_NAME)
            .set_buffer_pixels(BUFFER_SIZE)
            .set_attr(fields::CLASS, field_values::CLASS_OCEAN)
            .set_min_zoom(6);
    }
}

impl OsmWaterPolygonHandler for Water {
    fn process_osm_water_polygon(&self, element: &OsmWaterPolygon<'_>, features: &mut FeatureCollector<'_>) {
        if element.natural().as_deref() != Some("bay") {
            let source = element.source;
            let clazz = *CLASS_MAPPING.get_or_else(source, &field_values::CLASS_LAKE);
            features
                .polygon(LAYER_NAME)
                .set_buffer_pixels(BUFFER_SIZE)
                .set_min_pixel_size_below_zoom(11, 2.0)
                .set_min_zoom(6)
                .set_attr(fields::ID, source.id())
                .set_attr(fields::INTERMITTENT, if element.is_intermittent() { 1 } else { 0 })
                .set_attr_with_minzoom(fields::BRUNNEL, utils::brunnel(element.is_bridge(), element.is_tunnel()), 12)
                .set_attr(fields::CLASS, clazz);

            if let Err(e) = self.attempt_ne_lake_id_mapping(element) {
                e.log_with_details(
                    &*self.stats,
                    "omt_water",
                    "Unable to add OSM ID to natural earth water feature",
                    self.config.log_jts_exceptions,
                );
            }
        }
    }
}

impl FinishHandler for Water {
    fn finish(
        &self,
        source_name: &str,
        feature_collectors: &feature_collector::Factory,
        emit: &mut FeatureConsumer<'_>,
    ) {
        if source_name == OSM_SOURCE {
            let timer = self.stats.start_stage("ne_lakes");
            let items = std::mem::take(&mut *self.ne_all_lake_infos.lock());
            for item in items {
                let osm_id = item.osm_id.lock().0;
                let source = SimpleFeature::from_world_geometry(item.geom.clone());
                let mut features = feature_collectors.get(&source);
                Self::setup_ne_water_feature(&mut features, item.min_zoom, item.max_zoom, item.clazz, osm_id);
                for feature in features.iter() {
                    emit(feature);
                }
            }
            self.ne_lake_name_maps.lock().clear();
            *self.ne_lake_index.write() = None;
            drop(timer);
        }
    }
}

impl LayerPostProcessor for Water {
    fn post_process(&self, zoom: u32, items: &mut Vec<vector_tile::Feature>) -> Result<(), GeometryException> {
        if items.len() > 1 {
            *items =
                feature_merge::merge_overlapping_polygons(std::mem::take(items), self.config.min_feature_size(zoom))?;
        }
        Ok(())
    }
}

impl Handler for Water {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn as_finish_handler(&self) -> Option<&dyn FinishHandler> {
        Some(self)
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }
}

impl RowHandlers for Water {
    fn as_osm_water_polygon_handler(&self) -> Option<&dyn OsmWaterPolygonHandler> {
        Some(self)
    }
}

impl Layer for Water {
    fn as_natural_earth_processor(&self) -> Option<&dyn NaturalEarthProcessor> {
        Some(self)
    }

    fn as_osm_water_polygon_processor(&self) -> Option<&dyn OsmWaterPolygonProcessor> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use planetiler_core::feature_collector::{Factory, Feature};
    use planetiler_core::geo::geo_utils;
    use planetiler_core::geo::test_utils::rectangle;
    use planetiler_core::profile::Profile;
    use planetiler_core::tags;
    use planetiler_core::value::Tags;

    use super::*;
    use crate::open_map_tiles_profile::{NATURAL_EARTH_SOURCE, WATER_POLYGON_SOURCE};
    use crate::test_support::*;

    fn feature(
        geom: impl Into<Geometry<f64>>,
        tags: Tags,
        source: &str,
        layer: Option<&str>,
        id: i64,
    ) -> SimpleFeature {
        SimpleFeature::create(geom, tags, Some(source), layer, id)
    }

    fn ne(geom: impl Into<Geometry<f64>>, tags: Tags, layer: &str) -> SimpleFeature {
        feature(geom, tags, NATURAL_EARTH_SOURCE, Some(layer), 0)
    }

    fn osm_lake(geom: impl Into<Geometry<f64>>, mut tags: Tags, id: i64) -> SimpleFeature {
        tags.insert("natural".into(), "water".into());
        tags.insert("water".into(), "reservoir".into());
        feature(geom, tags, OSM_SOURCE, None, id)
    }

    /// Runs `profile.finish(OSM_SOURCE)` and checks the emitted features.
    fn assert_finish(t: &LayerTest, expected: &[Tags]) {
        let factory = Factory::new(t.params.clone(), t.stats.clone());
        let mut checks: Vec<Tags> = Vec::new();
        t.profile.finish(OSM_SOURCE, &factory, &mut |f: &Feature<'_>| checks.push(to_map(f, 0)));
        assert_eq!(expected.len(), checks.len(), "{checks:?}");
        let mut expected: Vec<&Tags> = expected.iter().collect();
        let key = |t: &Tags| format!("{:?}{:?}", t.get("_minzoom"), t.get("id"));
        expected.sort_by_key(|t| key(t));
        checks.sort_by_key(key);
        // match each expected to the actual with the same minzoom when present, otherwise by order
        for (e, a) in expected.iter().zip(&checks) {
            assert_submap(e, a);
        }
    }

    #[test]
    fn test_water_natural_earth() {
        let t = LayerTest::new();
        assert_features(
            0,
            &[
                tags! {"class" => "ocean", "intermittent" => "<null>", "_layer" => "water", "_type" => "polygon", "_minzoom" => 0},
            ],
            &t.process(&ne(rectangle(0.0, 10.0), Tags::new(), "ne_110m_ocean")),
        );
        assert_features(
            6,
            &[tags! {"class" => "ocean", "_layer" => "water", "_type" => "polygon", "_maxzoom" => 5}],
            &t.process(&ne(rectangle(0.0, 10.0), Tags::new(), "ne_10m_ocean")),
        );
    }

    #[test]
    fn test_lake_natural_earth_by_intersection() {
        let t = LayerTest::new();
        let polygon = rectangle(0.0, 0.1);
        t.process(&ne(polygon.clone(), Tags::new(), "ne_110m_lakes"));
        t.process(&ne(polygon.clone(), Tags::new(), "ne_10m_lakes"));
        t.process(&osm_lake(polygon, Tags::new(), 123));
        assert_finish(
            &t,
            &[
                tags! {"class" => "lake", "intermittent" => "<null>", "id" => 123i64, "_layer" => "water", "_type" => "polygon", "_minzoom" => 0, "_maxzoom" => 1},
                tags! {"class" => "lake", "intermittent" => "<null>", "id" => 123i64, "_layer" => "water", "_type" => "polygon", "_minzoom" => 4, "_maxzoom" => 5},
            ],
        );
    }

    #[test]
    fn test_lake_natural_earth_intersection_miss() {
        let t = LayerTest::new();
        let polygon1 = rectangle(0.0, 0.1);
        let polygon2 = rectangle(0.2, 0.3);
        t.process(&ne(polygon1.clone(), Tags::new(), "ne_110m_lakes"));
        t.process(&ne(polygon1, Tags::new(), "ne_10m_lakes"));
        t.process(&osm_lake(polygon2, Tags::new(), 123));
        assert_finish(
            &t,
            &[
                tags! {"class" => "lake", "id" => "<null>", "_layer" => "water", "_type" => "polygon"},
                tags! {"class" => "lake", "id" => "<null>", "_layer" => "water", "_type" => "polygon"},
            ],
        );
    }

    #[test]
    fn test_lake_natural_earth_by_bigger_intersection() {
        let t = LayerTest::new();
        let polygon1 = rectangle(0.0, 0.1);
        let polygon2 = rectangle(0.0, 0.2);
        t.process(&ne(polygon2.clone(), Tags::new(), "ne_110m_lakes"));
        t.process(&ne(polygon2.clone(), Tags::new(), "ne_10m_lakes"));
        t.process(&osm_lake(polygon1, Tags::new(), 123));
        t.process(&osm_lake(polygon2, Tags::new(), 234));
        assert_finish(
            &t,
            &[
                tags! {"class" => "lake", "intermittent" => "<null>", "id" => 234i64, "_layer" => "water", "_type" => "polygon", "_minzoom" => 0, "_maxzoom" => 1},
                tags! {"class" => "lake", "intermittent" => "<null>", "id" => 234i64, "_layer" => "water", "_type" => "polygon", "_minzoom" => 4, "_maxzoom" => 5},
            ],
        );
    }

    #[test]
    fn test_lake_natural_earth_by_name() {
        let t = LayerTest::new();
        let polygon = rectangle(0.0, 0.1);
        t.process(&ne(polygon.clone(), tags! {"name" => "Test Lake"}, "ne_50m_lakes"));
        t.process(&ne(polygon.clone(), tags! {"name" => "Test Lake"}, "ne_10m_lakes"));
        t.process(&osm_lake(polygon, tags! {"name" => "Test Lake"}, 123));
        assert_finish(
            &t,
            &[
                tags! {"class" => "lake", "id" => 123i64, "_layer" => "water", "_minzoom" => 2, "_maxzoom" => 3},
                tags! {"class" => "lake", "id" => 123i64, "_layer" => "water", "_minzoom" => 4, "_maxzoom" => 5},
            ],
        );
    }

    #[test]
    fn test_lake_natural_earth_by_name_intersection_miss() {
        let t = LayerTest::new();
        t.process(&ne(rectangle(0.0, 0.1), tags! {"name" => "Test Lake"}, "ne_50m_lakes"));
        t.process(&osm_lake(rectangle(0.2, 0.3), tags! {"name" => "Test Lake"}, 123));
        assert_finish(&t, &[tags! {"class" => "lake", "id" => "<null>", "_layer" => "water"}]);
    }

    #[test]
    fn test_lake_natural_earth_by_name_and_bigger_intersection() {
        let t = LayerTest::new();
        let polygon1 = rectangle(0.0, 0.1);
        let polygon2 = rectangle(0.0, 0.2);
        t.process(&ne(polygon2.clone(), tags! {"name" => "Test Lake"}, "ne_50m_lakes"));
        t.process(&osm_lake(polygon1, tags! {"name" => "Test Lake"}, 123));
        t.process(&osm_lake(polygon2, tags! {"name" => "Test Lake"}, 234));
        assert_finish(&t, &[tags! {"class" => "lake", "id" => 234i64, "_layer" => "water"}]);
    }

    #[test]
    fn test_lake_natural_earth_by_name_with_colision() {
        let t = LayerTest::new();
        let polygon_smaller = rectangle(0.0, 0.1);
        let polygon_bigger = rectangle(0.0, 0.2);
        t.process(&ne(polygon_smaller, tags! {"name" => "Test Lake"}, "ne_10m_lakes"));
        t.process(&ne(polygon_bigger.clone(), tags! {"name" => "Test Lake"}, "ne_10m_lakes"));
        t.process(&osm_lake(polygon_bigger, tags! {"name" => "Test Lake"}, 123));
        let factory = Factory::new(t.params.clone(), t.stats.clone());
        let mut ids = Vec::new();
        t.profile.finish(OSM_SOURCE, &factory, &mut |f: &Feature<'_>| {
            let map = to_map(f, 0);
            assert_eq!(Some("lake"), map.get("class").and_then(Value::as_str));
            ids.push(map.get("id").cloned());
        });
        ids.sort_by_key(|v| format!("{v:?}"));
        assert_eq!(vec![None, Some(Value::Int(123))], ids);
    }

    #[test]
    fn test_water_osm_water_polygon() {
        let t = LayerTest::new();
        assert_features(
            0,
            &[
                tags! {"class" => "ocean", "intermittent" => "<null>", "_layer" => "water", "_type" => "polygon", "_minzoom" => 6, "_maxzoom" => 14},
            ],
            &t.process(&feature(rectangle(0.0, 10.0), Tags::new(), WATER_POLYGON_SOURCE, None, 0)),
        );
    }

    #[test]
    fn test_water_osm_id() {
        let t = LayerTest::new();
        let id = 123i64;
        assert_features(
            14,
            &[
                tags! {"class" => "lake", "id" => id, "_layer" => "water", "_type" => "polygon", "_minzoom" => 6, "_maxzoom" => 14},
            ],
            &t.process(&osm_lake(geo_utils::world_to_lat_lon_coords(&rectangle(0.0, 1.0).into()), Tags::new(), id)),
        );
    }

    #[test]
    #[ignore = "TODO(port): enable once the poi layer is ported (expects poi features)"]
    fn test_water() {
        let t = LayerTest::new();
        assert_features(
            14,
            &[tags! {"class" => "lake", "_layer" => "water", "_type" => "polygon", "_minzoom" => 6, "_maxzoom" => 14}],
            &t.process(&polygon_feature(tags! {"natural" => "water", "water" => "reservoir"})),
        );
        assert_features(
            14,
            &[
                tags! {"_layer" => "poi"},
                tags! {"class" => "swimming_pool", "_layer" => "water", "_type" => "polygon", "_minzoom" => 6, "_maxzoom" => 14},
            ],
            &t.process(&polygon_feature(tags! {"leisure" => "swimming_pool"})),
        );
        assert_features(14, &[], &t.process(&polygon_feature(tags! {"natural" => "bay"})));
        assert_features(14, &[Tags::new()], &t.process(&polygon_feature(tags! {"natural" => "water"})));
        assert_features(14, &[], &t.process(&polygon_feature(tags! {"natural" => "water", "covered" => "yes"})));
        assert_features(
            14,
            &[
                tags! {"class" => "dock", "intermittent" => 1, "_layer" => "water", "_type" => "polygon", "_minzoom" => 6, "_maxzoom" => 14},
                tags! {"class" => "harbor", "subclass" => "dock", "_layer" => "poi", "_type" => "point", "_minzoom" => 14, "_maxzoom" => 14},
            ],
            &t.process(&polygon_feature(tags! {"waterway" => "dock", "intermittent" => "1"})),
        );
        assert_features(
            11,
            &[tags! {
                "class" => "lake", "brunnel" => "<null>", "intermittent" => 0, "_layer" => "water", "_type" => "polygon",
                "_minzoom" => 6, "_maxzoom" => 14, "_minpixelsize" => 2.0
            }],
            &t.process(&polygon_feature(tags! {"landuse" => "salt_pond", "bridge" => "1"})),
        );
    }

    /// The water-only parts of `test_water` that do not depend on other layers.
    #[test]
    fn test_water_without_other_layers() {
        let t = LayerTest::new();
        assert_features(
            14,
            &[tags! {"class" => "lake", "_layer" => "water", "_type" => "polygon", "_minzoom" => 6, "_maxzoom" => 14}],
            &t.process(&polygon_feature(tags! {"natural" => "water", "water" => "reservoir"})),
        );
        assert_features(14, &[], &t.process(&polygon_feature(tags! {"natural" => "bay"})));
        assert_features(14, &[Tags::new()], &t.process(&polygon_feature(tags! {"natural" => "water"})));
        assert_features(14, &[], &t.process(&polygon_feature(tags! {"natural" => "water", "covered" => "yes"})));
        assert_features(
            11,
            &[tags! {
                "class" => "lake", "brunnel" => "<null>", "intermittent" => 0, "_layer" => "water", "_type" => "polygon",
                "_minzoom" => 6, "_maxzoom" => 14, "_minpixelsize" => 2.0
            }],
            &t.process(&polygon_feature(tags! {"landuse" => "salt_pond", "bridge" => "1"})),
        );
        let swimming_pool = polygon_feature(tags! {"leisure" => "swimming_pool"});
        let features = t.process(&swimming_pool);
        let water: Vec<_> = features.iter().filter(|f| f.get_layer() == "water").collect();
        assert_features(
            14,
            &[
                tags! {"class" => "swimming_pool", "_layer" => "water", "_type" => "polygon", "_minzoom" => 6, "_maxzoom" => 14},
            ],
            water,
        );
        let dock = polygon_feature(tags! {"waterway" => "dock", "intermittent" => "1"});
        let features = t.process(&dock);
        let water: Vec<_> = features.iter().filter(|f| f.get_layer() == "water").collect();
        assert_features(
            14,
            &[
                tags! {"class" => "dock", "intermittent" => 1, "_layer" => "water", "_type" => "polygon", "_minzoom" => 6, "_maxzoom" => 14},
            ],
            water,
        );
    }

    #[test]
    #[ignore = "TODO(port): enable once the poi layer is ported (expects a poi feature)"]
    fn test_dock() {
        let t = LayerTest::new();
        assert_features(
            11,
            &[
                tags! {"class" => "dock", "_layer" => "water", "_type" => "polygon"},
                tags! {"class" => "harbor", "_layer" => "poi", "_type" => "point"},
            ],
            &t.process(&polygon_feature(tags! {"waterway" => "dock"})),
        );
    }

    #[test]
    fn test_river() {
        let t = LayerTest::new();
        assert_features(
            11,
            &[tags! {"class" => "river", "_layer" => "water", "_type" => "polygon"}],
            &t.process(&polygon_feature(tags! {"water" => "river"})),
        );
    }

    #[test]
    fn test_spring() {
        let t = LayerTest::new();
        assert_features(
            11,
            &[tags! {"class" => "lake", "_layer" => "water", "_type" => "polygon"}],
            &t.process(&polygon_feature(tags! {"natural" => "spring"})),
        );
    }

    #[test]
    fn test_ocean_zoom_levels() {
        let t = LayerTest::new();
        let f1 = ne(rectangle(0.0, 10.0), Tags::new(), "ne_110m_ocean");
        let f2 = ne(rectangle(0.0, 10.0), Tags::new(), "ne_50m_ocean");
        let f3 = ne(rectangle(0.0, 10.0), Tags::new(), "ne_10m_ocean");
        let f4 = feature(rectangle(0.0, 10.0), Tags::new(), WATER_POLYGON_SOURCE, None, 0);
        assert_covers_zoom_range(0, 14, "water", &[&t.process(&f1), &t.process(&f2), &t.process(&f3), &t.process(&f4)]);
    }

    #[test]
    fn test_lake_zoom_levels() {
        let t = LayerTest::new();
        let f = osm_lake(rectangle(0.0, 10.0), Tags::new(), 0);
        assert_covers_zoom_range(6, 14, "water", &[&t.process(&f)]);
    }
}
