/*
Copyright (c) 2021, MapTiler.com & OpenMapTiles contributors.
All rights reserved.

Code license: BSD 3-Clause License

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

* Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.

* Redistributions in binary form must reproduce the above copyright notice,
  this list of conditions and the following disclaimer in the documentation
  and/or other materials provided with the distribution.

* Neither the name of the copyright holder nor the names of its
  contributors may be used to endorse or promote products derived from
  this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Design license: CC-BY 4.0

See https://github.com/openmaptiles/openmaptiles/blob/master/LICENSE.md for details on usage
*/
//! Port of `layers/Poi.java`: defines the logic for generating map elements for things like shops, parks, and
//! schools in the `poi` layer from source features.
//!
//! This is ported from the
//! [OpenMapTiles poi sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/poi).
//!
//! `process_*` creates the raw POI feature from OSM elements and `post_process` assigns the feature rank from order
//! in the tile at render-time.
//!
//! Differences from Java: elements set aside for `agg_stop` processing are stored as owned [`SimpleFeature`]s (world
//! geometry + copied tags + id) since table rows borrow the source feature. Java mutates the source tags to add a
//! fallback `name` for ATMs/parcel lockers/charging stations; Rust copies the tags only in that case.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use geo::Centroid;
use geo_types::{MultiPoint, Point};
use indexmap::IndexMap;
use parking_lot::Mutex;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::expression::multi_expression::Index;
use planetiler_core::feature_collector::{Factory, Feature, FeatureCollector};
use planetiler_core::forwarding_profile::{FinishHandler, Handler, LayerPostProcessor};
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::geo::with_geometry::WithGeometry;
use planetiler_core::profile::FeatureConsumer;
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::parse;
use planetiler_core::util::translations::Translations;
use planetiler_core::value::{Tags, Value};
use planetiler_core::vector_tile;
use tracing::info;

use crate::generated::open_map_tiles_schema::poi::*;
use crate::generated::tables::*;
use crate::layer::Layer;
use crate::open_map_tiles_profile::OSM_SOURCE;
use crate::util::omt_language_utils;
use crate::util::utils::{null_if_empty, null_if_long, null_or_empty};

const UNIVERSITY_POI_SUBCLASSES: &[&str] = &["university", "college"];
const AGG_STOP_SUBCLASS_ORDER: &[&str] = &["subway", "tram_stop", "bus_station", "bus_stop"];
const BRAND_OPERATOR_REF_SUBCLASSES: &[&str] = &["charging_station", "parcel_locker"];

/// Rank of a POI class (lower is more important).
pub fn poi_class_rank(clazz: &str) -> i32 {
    match clazz {
        field_values::CLASS_HOSPITAL => 20,
        field_values::CLASS_RAILWAY => 40,
        field_values::CLASS_BUS => 50,
        field_values::CLASS_ATTRACTION => 70,
        field_values::CLASS_HARBOR => 75,
        field_values::CLASS_COLLEGE => 80,
        field_values::CLASS_SCHOOL => 85,
        field_values::CLASS_STADIUM => 90,
        "zoo" => 95,
        field_values::CLASS_TOWN_HALL => 100,
        field_values::CLASS_CAMPSITE => 110,
        field_values::CLASS_CEMETERY => 115,
        field_values::CLASS_PARK => 120,
        field_values::CLASS_LIBRARY => 130,
        "police" => 135,
        field_values::CLASS_POST => 140,
        field_values::CLASS_GOLF => 150,
        field_values::CLASS_SHOP => 400,
        field_values::CLASS_GROCERY => 500,
        field_values::CLASS_FAST_FOOD => 600,
        field_values::CLASS_CLOTHING_STORE => 700,
        field_values::CLASS_BAR => 800,
        _ => 1_000,
    }
}

fn agg_stop_order(subclass: Option<&str>) -> i32 {
    subclass.and_then(|s| AGG_STOP_SUBCLASS_ORDER.iter().position(|o| *o == s)).map_or(-1, |i| i as i32)
}

const CLASS_CACHE_LIMIT: usize = 10_000;

thread_local! {
    /// Per-thread cache of `"<mapping_key>\0<subclass>"` -> class (`None` = use the subclass), so that looking up the
    /// class doesn't need to build a tag map for every element.
    static CLASS_CACHE: RefCell<(String, HashMap<String, Option<&'static str>>)> = RefCell::new((String::new(), HashMap::new()));
}

/// An element set aside for `agg_stop` processing in [`Poi::finish`].
struct AggStop {
    feature: SimpleFeature,
    mapping_key: String,
}

impl AggStop {
    fn row(&self) -> OsmPoiPoint<'_> {
        OsmPoiPoint::new(&self.feature, &self.mapping_key)
    }

    fn point(&self) -> GeometryResult<Point<f64>> {
        let geom = self.feature.world_geometry()?;
        Ok(geom.centroid().unwrap_or(Point::new(f64::NAN, f64::NAN)))
    }
}

pub struct Poi {
    class_mapping: Index<&'static str>,
    translations: Arc<Translations>,
    stats: StatsRef,
    agg_stops: Mutex<IndexMap<String, Vec<AggStop>>>,
}

impl Poi {
    pub fn new(translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, stats: &StatsRef) -> Poi {
        Poi {
            class_mapping: field_mappings::CLASS.index(),
            translations: translations.clone(),
            stats: stats.clone(),
            agg_stops: Mutex::new(IndexMap::new()),
        }
    }

    fn lookup_class(&self, subclass: &str, mapping_key: &str) -> Option<&'static str> {
        let mut tags = Tags::with_capacity(2);
        tags.insert("subclass".into(), Value::from(subclass));
        tags.insert("mapping_key".into(), Value::from(mapping_key));
        self.class_mapping.get_first(&tags).copied()
    }

    fn poi_class<'s>(&self, subclass: Option<&'s str>, mapping_key: &str) -> Cow<'s, str> {
        // Special case subclass collision between office=university and amenity=university
        if mapping_key == "amenity" && subclass == Some("university") {
            return Cow::Borrowed(field_values::CLASS_COLLEGE);
        }
        let subclass = subclass.unwrap_or("");
        let class = CLASS_CACHE.with(|cache| {
            let (buf, map) = &mut *cache.borrow_mut();
            buf.clear();
            buf.push_str(mapping_key);
            buf.push('\0');
            buf.push_str(subclass);
            if let Some(result) = map.get(buf.as_str()) {
                return *result;
            }
            let result = self.lookup_class(subclass, mapping_key);
            if map.len() < CLASS_CACHE_LIMIT {
                map.insert(buf.clone(), result);
            }
            result
        });
        match class {
            Some(class) => Cow::Borrowed(class),
            None => Cow::Borrowed(subclass),
        }
    }

    fn minzoom(subclass: Option<&str>, mapping_key: &str) -> u32 {
        let low_zoom = (subclass == Some("station") && mapping_key == "railway")
            || subclass == Some("halt")
            || subclass == Some("ferry_terminal");
        if low_zoom { 12 } else { 14 }
    }

    fn process_agg_stop(
        &self,
        stop: &AggStop,
        feature_collectors: &Factory,
        emit: &mut FeatureConsumer<'_>,
        agg_stop: Option<i32>,
    ) {
        let mut features = feature_collectors.get(&stop.feature);
        let row = stop.row();
        self.setup_poi_feature(&row, features.point(LAYER_NAME), agg_stop);
        for feature in features.iter() {
            emit(feature);
        }
    }

    fn setup_poi_feature<'a, T>(&self, element: &T, output: &mut Feature<'_>, agg_stop: Option<i32>)
    where
        T: WithSubclass<'a>
            + WithStation<'a>
            + WithFunicular<'a>
            + WithSport<'a>
            + WithInformation<'a>
            + WithReligion<'a>
            + WithMappingKey<'a>
            + WithName<'a>
            + WithIndoor<'a>
            + WithLayer<'a>
            + WithSource<'a>
            + WithOperator<'a>
            + WithNetwork<'a>
            + WithBrand<'a>
            + WithRef<'a>,
    {
        let source = element.source();
        let element_subclass = element.subclass();
        let mut raw_subclass = element_subclass.as_deref();
        if raw_subclass == Some("station") && element.station().as_deref() == Some("subway") {
            raw_subclass = Some("subway");
        }
        if raw_subclass == Some("station") && element.funicular().as_deref() == Some("yes") {
            raw_subclass = Some("halt");
        }

        // ATM names fall back to operator, or else network
        let mut name: Option<Cow<'_, str>> = element.name();
        let mut name_override: Option<String> = None;
        if raw_subclass == Some("atm") && null_or_empty(name.as_deref()) {
            let fallback = null_if_empty(element.operator()).or_else(|| null_if_empty(element.network()));
            if let Some(fallback) = fallback {
                name_override = Some(fallback.to_string());
                name = Some(fallback);
            }
        }

        // Parcel locker without name: use either brand or operator and add ref if present
        if raw_subclass.is_some_and(|s| BRAND_OPERATOR_REF_SUBCLASSES.contains(&s)) && null_or_empty(name.as_deref()) {
            let mut fallback: Option<String> =
                null_if_empty(element.brand()).or_else(|| null_if_empty(element.operator())).map(Cow::into_owned);
            if let Some(r) = null_if_empty(element.r#ref()) {
                fallback = Some(match fallback {
                    None => r.into_owned(),
                    Some(n) => format!("{n} {r}"),
                });
            }
            if let Some(fallback) = fallback {
                name = Some(Cow::Owned(fallback.clone()));
                name_override = Some(fallback);
            }
        }

        let information;
        let religion;
        let sport;
        let subclass: Option<&str> = match raw_subclass {
            Some("information") => {
                information = element.information();
                null_if_empty(information.as_deref())
            }
            Some("place_of_worship") => {
                religion = element.religion();
                null_if_empty(religion.as_deref())
            }
            Some("pitch") => {
                sport = element.sport();
                null_if_empty(sport.as_deref())
            }
            other => other,
        };
        let mapping_key = element.mapping_key();
        let poi_class = self.poi_class(raw_subclass, mapping_key);
        let poi_class_rank = poi_class_rank(&poi_class);
        let rank_order = poi_class_rank + if null_or_empty(name.as_deref()) { 2000 } else { 0 };

        let mut minzoom = Self::minzoom(element_subclass.as_deref(), mapping_key);
        if raw_subclass.is_some_and(|s| UNIVERSITY_POI_SUBCLASSES.contains(&s)) {
            // universities that are at least 10% of a tile may appear from Z10
            output.set_min_pixel_size_below_zoom(13, 80.0); // 80x80px is ~10% of a 256x256px tile
            minzoom = 10;
        }

        let names = match name_override {
            Some(name) => {
                let mut tags = source.tags().clone();
                tags.insert("name".into(), Value::from(name));
                omt_language_utils::get_names(&tags, Some(&self.translations))
            }
            None => omt_language_utils::get_names(source.tags(), Some(&self.translations)),
        };

        output
            .set_buffer_pixels(BUFFER_SIZE)
            .set_attr(fields::CLASS, &*poi_class)
            .set_attr(fields::SUBCLASS, subclass)
            .set_attr(fields::LAYER, null_if_long(element.layer(), 0))
            .set_attr(fields::LEVEL, source.get_tag("level").and_then(parse::parse_long_or_null))
            .set_attr(fields::INDOOR, if element.indoor() { Some(1) } else { None })
            .set_attr(fields::AGG_STOP, agg_stop)
            .put_attrs(names)
            .set_point_label_grid_pixel_size(14, 64.0)
            .set_sort_key(rank_order)
            .set_min_zoom(minzoom);
    }

    fn finish_agg_stops(&self, feature_collectors: &Factory, emit: &mut FeatureConsumer<'_>) {
        let agg_stops = std::mem::take(&mut *self.agg_stops.lock());
        info!("Processing {} agg_stop sets", agg_stops.len());

        for agg_stop_set in agg_stops.values() {
            if agg_stop_set.len() == 1 {
                self.process_agg_stop(&agg_stop_set[0], feature_collectors, emit, Some(1));
                continue;
            }
            let nearest = match Self::nearest_agg_stop(agg_stop_set) {
                Ok(nearest) => nearest,
                Err(e) => {
                    e.log_with_stats(
                        &*self.stats,
                        "agg_stop_geometry_1",
                        &format!(
                            "Error getting geometry for some of the stops with UIC ref. {} (agg_stop)",
                            agg_stop_set[0].row().uic_ref().unwrap_or_default()
                        ),
                    );
                    // we're not able to calculate agg_stop, so simply dump the stops as they are
                    None
                }
            };
            // now emit the stops
            for (i, stop) in agg_stop_set.iter().enumerate() {
                self.process_agg_stop(stop, feature_collectors, emit, if nearest == Some(i) { Some(1) } else { None });
            }
        }
    }

    /// Index of the most important stop (by subclass) nearest to the centroid of all stops in the set.
    fn nearest_agg_stop(agg_stop_set: &[AggStop]) -> GeometryResult<Option<usize>> {
        // find most important stops based on subclass
        let first_subclass =
            agg_stop_set.iter().map(|s| s.row().subclass()).min_by_key(|s| agg_stop_order(s.as_deref())).flatten();

        // calculate the centroid and ...
        let mut points = Vec::with_capacity(agg_stop_set.len());
        for stop in agg_stop_set {
            points.push(stop.point()?);
        }
        let centroid = MultiPoint::from(points.clone()).centroid().unwrap_or(Point::new(f64::NAN, f64::NAN));

        // ... find one stop nearest to the centroid
        let mut min_distance = f64::MAX;
        let mut nearest: Option<usize> = None;
        for (i, stop) in agg_stop_set.iter().enumerate() {
            if stop.row().subclass() != first_subclass {
                continue;
            }
            let p = points[i];
            let distance = (p.x() - centroid.x()).hypot(p.y() - centroid.y());
            let is_nearer = match nearest {
                None => true,
                Some(n) => {
                    distance < min_distance
                        || (distance == min_distance && stop.feature.id() < agg_stop_set[n].feature.id())
                }
            };
            if is_nearer {
                min_distance = distance;
                nearest = Some(i);
            }
        }
        Ok(nearest)
    }
}

impl OsmPoiPointHandler for Poi {
    fn process_osm_poi_point(&self, element: &OsmPoiPoint<'_>, features: &mut FeatureCollector<'_>) {
        let subclass = element.subclass();
        if let Some(uic_ref) = element.uic_ref()
            && subclass.as_deref().is_some_and(|s| AGG_STOP_SUBCLASS_ORDER.contains(&s))
        {
            let mut key = uic_ref.into_owned();
            for part in [element.name(), element.network(), element.operator()] {
                if let Some(part) = null_if_empty(part) {
                    key.push_str(&part);
                }
            }
            let source = element.source;
            let stop = match source.world_geometry() {
                Ok(geom) => AggStop {
                    feature: SimpleFeature::create_world(
                        geom.clone(),
                        source.tags().clone(),
                        Some(OSM_SOURCE),
                        None::<&str>,
                        source.id(),
                    ),
                    mapping_key: element.mapping_key.to_string(),
                },
                Err(e) => {
                    e.log_with_stats(
                        &*self.stats,
                        "agg_stop_geometry_2",
                        &format!("Error getting geometry for the stop {} (agg_stop)", source.id()),
                    );
                    return;
                }
            };
            // multiple threads may update this concurrently
            self.agg_stops.lock().entry(key).or_default().push(stop);
        } else {
            self.setup_poi_feature(element, features.point(LAYER_NAME), None);
        }
    }
}

impl OsmPoiPolygonHandler for Poi {
    fn process_osm_poi_polygon(&self, element: &OsmPoiPolygon<'_>, features: &mut FeatureCollector<'_>) {
        self.setup_poi_feature(element, features.centroid_if_convex(LAYER_NAME), None);
    }
}

impl FinishHandler for Poi {
    /// We've put aside some stops for `agg_stop` processing and we do that processing here.
    ///
    /// The main point is to group together stops with same `uid_ref` and then order them first based on subclass
    /// (see `AGG_STOP_SUBCLASS_ORDER`) and then based on distance from centroid (calculated from all the stops). The
    /// first one gets `agg_stop=1`, the rest will be "normal" (e.g. no `agg_stop` attribute).
    ///
    /// ref: [poi_stop_agg.sql](https://github.com/openmaptiles/openmaptiles/blob/master/layers/poi/poi_stop_agg.sql#L26,L28)
    fn finish(&self, source_name: &str, feature_collectors: &Factory, emit: &mut FeatureConsumer<'_>) {
        if source_name == OSM_SOURCE {
            let timer = self.stats.start_stage("agg_stop");
            self.finish_agg_stops(feature_collectors, emit);
            timer.stop();
        }
    }
}

impl LayerPostProcessor for Poi {
    fn post_process(&self, _zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        // infer the "rank" field from the order of features within each label grid square
        let mut group_counts: HashMap<i64, i64> = HashMap::new();
        for feature in items.iter_mut() {
            let count = group_counts.entry(feature.group).or_insert(1);
            let gridrank = *count;
            *count += 1;
            if !feature.tags.contains_key(fields::RANK) {
                feature.tags.insert(fields::RANK.into(), Value::Int(gridrank));
            }
        }
        Ok(())
    }
}

impl Handler for Poi {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn release(&self) {
        self.agg_stops.lock().clear();
    }

    fn as_finish_handler(&self) -> Option<&dyn FinishHandler> {
        Some(self)
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }
}

impl RowHandlers for Poi {
    fn as_osm_poi_point_handler(&self) -> Option<&dyn OsmPoiPointHandler> {
        Some(self)
    }

    fn as_osm_poi_polygon_handler(&self) -> Option<&dyn OsmPoiPolygonHandler> {
        Some(self)
    }
}

impl Layer for Poi {}

#[cfg(test)]
mod tests {
    use planetiler_core::feature_collector::Feature;
    use planetiler_core::geo::test_utils::new_point;
    use planetiler_core::profile::Profile;
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;
    use planetiler_core::value::Tags;

    use super::LAYER_NAME;
    use crate::open_map_tiles_profile::OSM_SOURCE;
    use crate::test_support::*;

    fn feature(area: bool, tags: Tags) -> SimpleFeature {
        if area { polygon_feature(tags) } else { point_feature(tags) }
    }

    /// Processes `source_features`, then runs `finish` and returns the maps of the emitted features at `zoom`.
    fn test_agg_stops(t: &LayerTest, source_features: Vec<SimpleFeature>, zoom: u32, expected: &[Tags]) {
        for f in &source_features {
            t.process(f);
        }
        let mut maps: Vec<Tags> = Vec::new();
        t.profile.finish(OSM_SOURCE, &t.feature_collector_factory, &mut |f: &Feature<'_>| maps.push(to_map(f, zoom)));
        assert_eq!(expected.len(), maps.len(), "{maps:?}");
        for (e, a) in expected.iter().zip(&maps) {
            assert_submap(e, a);
        }
    }

    #[test]
    fn test_fenway_park() {
        let t = LayerTest::new();
        assert_features(
            7,
            &[tags! {
                "_layer" => "poi",
                "class" => "stadium",
                "subclass" => "stadium",
                "name" => "Fenway Park",
                "rank" => "<null>",
                "_minzoom" => 14,
                "_labelgrid_size" => 64.0,
            }],
            &t.process(&point_feature(tags! {"leisure" => "stadium", "name" => "Fenway Park"})),
        );
    }

    #[test]
    fn test_funicular_halt() {
        let t = LayerTest::new();
        for area in [false, true] {
            assert_features(
                7,
                &[
                    tags! {"_layer" => "poi", "class" => "railway", "subclass" => "halt", "rank" => "<null>", "_minzoom" => 12},
                ],
                &t.process(&feature(area, tags! {"railway" => "station", "funicular" => "yes", "name" => "station"})),
            );
        }
    }

    #[test]
    fn test_subway() {
        let t = LayerTest::new();
        for area in [false, true] {
            assert_features(
                7,
                &[
                    tags! {"_layer" => "poi", "class" => "railway", "subclass" => "subway", "rank" => "<null>", "_minzoom" => 12},
                ],
                &t.process(&feature(area, tags! {"railway" => "station", "station" => "subway", "name" => "station"})),
            );
        }
    }

    #[test]
    fn test_agg_stop_just_one() {
        let t = LayerTest::new();
        test_agg_stops(
            &t,
            vec![point_feature(tags! {"highway" => "bus_stop", "name" => "station", "uic_ref" => "1"})],
            14,
            &[tags! {"_layer" => "poi", "class" => "bus", "subclass" => "bus_stop", "agg_stop" => 1, "_minzoom" => 14}],
        );
    }

    #[test]
    fn test_agg_stop_two_with_same_subclass() {
        let t = LayerTest::new();
        test_agg_stops(
            &t,
            vec![
                point_feature(
                    tags! {"railway" => "tram_stop", "name" => "station", "name:es" => "test 1", "uic_ref" => "1"},
                ),
                point_feature(
                    tags! {"railway" => "tram_stop", "name" => "station", "name:es" => "test 2", "uic_ref" => "1"},
                ),
            ],
            14,
            &[
                tags! {"_layer" => "poi", "name:es" => "test 1", "class" => "railway", "subclass" => "tram_stop", "agg_stop" => 1, "_minzoom" => 14},
                tags! {"_layer" => "poi", "name:es" => "test 2", "class" => "railway", "subclass" => "tram_stop", "agg_stop" => "<null>", "_minzoom" => 14},
            ],
        );
    }

    #[test]
    fn test_agg_stop_three_with_mixed_subclass() {
        let t = LayerTest::new();
        test_agg_stops(
            &t,
            vec![
                point_feature(
                    tags! {"highway" => "bus_stop", "name" => "station", "name:es" => "test 1", "uic_ref" => "1"},
                ),
                point_feature(
                    tags! {"highway" => "bus_stop", "name" => "station", "name:es" => "test 2", "uic_ref" => "1"},
                ),
                point_feature(
                    tags! {"railway" => "tram_stop", "name" => "station", "name:es" => "test 3", "uic_ref" => "1"},
                ),
            ],
            14,
            &[
                tags! {"_layer" => "poi", "name:es" => "test 1", "class" => "bus", "subclass" => "bus_stop", "agg_stop" => "<null>", "_minzoom" => 14},
                tags! {"_layer" => "poi", "name:es" => "test 2", "class" => "bus", "subclass" => "bus_stop", "agg_stop" => "<null>", "_minzoom" => 14},
                tags! {"_layer" => "poi", "name:es" => "test 3", "class" => "railway", "subclass" => "tram_stop", "agg_stop" => 1, "_minzoom" => 14},
            ],
        );
    }

    #[test]
    fn test_agg_stop_three_with_same_subclass() {
        let t = LayerTest::new();
        let f = |x: f64, name: &str, id: i64| {
            SimpleFeature::create(
                new_point(x, 0.0),
                tags! {"highway" => "bus_stop", "name" => "station", "name:es" => name, "uic_ref" => "1"},
                Some(OSM_SOURCE),
                None::<&str>,
                id,
            )
        };
        test_agg_stops(
            &t,
            vec![f(0.0, "test 1", 0), f(1.0, "test 2", 1), f(2.0, "test 3", 2)],
            14,
            &[
                tags! {"_layer" => "poi", "name:es" => "test 1", "class" => "bus", "subclass" => "bus_stop", "agg_stop" => "<null>", "_minzoom" => 14},
                tags! {"_layer" => "poi", "name:es" => "test 2", "class" => "bus", "subclass" => "bus_stop", "agg_stop" => 1, "_minzoom" => 14},
                tags! {"_layer" => "poi", "name:es" => "test 3", "class" => "bus", "subclass" => "bus_stop", "agg_stop" => "<null>", "_minzoom" => 14},
            ],
        );
    }

    #[test]
    fn test_place_of_worship_from_religion_tag() {
        let t = LayerTest::new();
        for area in [false, true] {
            assert_features(
                7,
                &[
                    tags! {"_layer" => "poi", "class" => "place_of_worship", "subclass" => "religion value", "rank" => "<null>", "_minzoom" => 14},
                ],
                &t.process(&feature(
                    area,
                    tags! {"amenity" => "place_of_worship", "religion" => "religion value", "name" => "station"},
                )),
            );
        }
    }

    #[test]
    fn test_pitch_from_sport_tag() {
        let t = LayerTest::new();
        assert_features(
            7,
            &[tags! {"_layer" => "poi", "class" => "pitch", "subclass" => "soccer", "rank" => "<null>"}],
            &t.process(&point_feature(tags! {"leisure" => "pitch", "sport" => "soccer", "name" => "station"})),
        );
    }

    #[test]
    fn test_information() {
        let t = LayerTest::new();
        for area in [false, true] {
            assert_features(
                7,
                &[tags! {
                    "_layer" => "poi",
                    "class" => "information",
                    "subclass" => "infotype",
                    "layer" => 3i64,
                    "level" => 2i64,
                    "indoor" => 1,
                    "rank" => "<null>",
                }],
                &t.process(&feature(
                    area,
                    tags! {
                        "tourism" => "information",
                        "information" => "infotype",
                        "name" => "station",
                        "layer" => "3",
                        "level" => "2",
                        "indoor" => "yes",
                    },
                )),
            );
        }
    }

    #[test]
    fn test_ferry_terminal() {
        let t = LayerTest::new();
        for area in [false, true] {
            assert_features(
                7,
                &[
                    tags! {"_layer" => "poi", "class" => "ferry_terminal", "subclass" => "ferry_terminal", "name" => "Water Taxi", "_minzoom" => 12},
                ],
                &t.process(&feature(
                    area,
                    tags! {
                        "amenity" => "ferry_terminal",
                        "information" => "infotype",
                        "name" => "Water Taxi",
                        "layer" => "3",
                        "level" => "2",
                        "indoor" => "yes",
                    },
                )),
            );
        }
    }

    #[test]
    fn test_grid_rank() {
        let t = LayerTest::new();
        assert_eq!(Vec::<planetiler_core::vector_tile::Feature>::new(), t.post_process(LAYER_NAME, 13, vec![]));

        assert_eq!(
            vec![vt_point_feature(LAYER_NAME, tags! {"rank" => 1}, 1)],
            t.post_process(LAYER_NAME, 14, vec![vt_point_feature(LAYER_NAME, tags! {}, 1)])
        );

        assert_eq!(
            vec![
                vt_point_feature(LAYER_NAME, tags! {"name" => "a", "rank" => 1}, 1),
                vt_point_feature(LAYER_NAME, tags! {"name" => "b", "rank" => 2}, 1),
                vt_point_feature(LAYER_NAME, tags! {"name" => "c", "rank" => 1}, 2),
            ],
            t.post_process(
                LAYER_NAME,
                14,
                vec![
                    vt_point_feature(LAYER_NAME, tags! {"name" => "a"}, 1),
                    vt_point_feature(LAYER_NAME, tags! {"name" => "b"}, 1),
                    vt_point_feature(LAYER_NAME, tags! {"name" => "c"}, 2),
                ]
            )
        );
    }

    #[test]
    fn test_embassy() {
        let t = LayerTest::new();
        assert_features(
            7,
            &[tags! {"_layer" => "poi", "class" => "office", "subclass" => "diplomatic", "name" => "The Embassy"}],
            &t.process(&point_feature(tags! {"office" => "diplomatic", "name" => "The Embassy"})),
        );
    }

    #[test]
    fn test_locksmith() {
        let t = LayerTest::new();
        assert_features(
            7,
            &[tags! {"_layer" => "poi", "class" => "shop", "subclass" => "locksmith", "name" => "The Locksmith"}],
            &t.process(&point_feature(tags! {"shop" => "locksmith", "name" => "The Locksmith"})),
        );
    }

    #[test]
    fn test_atm() {
        let t = LayerTest::new();
        let expected = [tags! {"_layer" => "poi", "class" => "atm", "subclass" => "atm", "name" => "ATM name"}];
        // prefer name, otherwise fall back to operator, or else network
        assert_features(14, &expected, &t.process(&point_feature(tags! {"amenity" => "atm", "name" => "ATM name"})));
        assert_features(
            14,
            &expected,
            &t.process(&point_feature(
                tags! {"amenity" => "atm", "name" => "ATM name", "operator" => "ATM operator", "network" => "ATM network"},
            )),
        );
        assert_features(
            14,
            &expected,
            &t.process(&point_feature(
                tags! {"amenity" => "atm", "operator" => "ATM name", "network" => "ATM network"},
            )),
        );
        assert_features(14, &expected, &t.process(&point_feature(tags! {"amenity" => "atm", "network" => "ATM name"})));
    }

    #[test]
    fn test_parcel_locker() {
        let t = LayerTest::new();
        let expected = [
            tags! {"_layer" => "poi", "class" => "post", "subclass" => "parcel_locker", "name" => "Parcel Locker name"},
        ];
        assert_features(
            14,
            &expected,
            &t.process(&point_feature(tags! {"amenity" => "parcel_locker", "brand" => "Parcel Locker name"})),
        );
        assert_features(
            14,
            &expected,
            &t.process(&point_feature(tags! {"amenity" => "parcel_locker", "operator" => "Parcel Locker name"})),
        );
        assert_features(
            14,
            &expected,
            &t.process(&point_feature(
                tags! {"amenity" => "parcel_locker", "operator" => "Parcel Locker", "ref" => "name"},
            )),
        );
    }

    #[test]
    fn test_parcel_locker_corner_case() {
        let t = LayerTest::new();
        // no brand, no operator, just ref
        assert_features(
            14,
            &[tags! {"_layer" => "poi", "class" => "post", "subclass" => "parcel_locker", "name" => "Corner Case"}],
            &t.process(&point_feature(tags! {"amenity" => "parcel_locker", "ref" => "Corner Case"})),
        );
    }

    #[test]
    fn test_charging_station() {
        let t = LayerTest::new();
        let expected = [tags! {
            "_layer" => "poi",
            "class" => "fuel",
            "subclass" => "charging_station",
            "name" => "Some Charging Station Operator",
        }];
        assert_features(
            14,
            &expected,
            &t.process(&point_feature(
                tags! {"amenity" => "charging_station", "brand" => "Some Charging Station Operator"},
            )),
        );
        assert_features(
            14,
            &expected,
            &t.process(&point_feature(
                tags! {"amenity" => "charging_station", "operator" => "Some Charging Station Operator"},
            )),
        );
        assert_features(
            14,
            &expected,
            &t.process(&point_feature(
                tags! {"amenity" => "charging_station", "operator" => "Some Charging Station", "ref" => "Operator"},
            )),
        );
    }
}
