//! Port of `layers/MountainPeak.java`: defines the logic for generating map elements for mountain peak label points in
//! the `mountain_peak` layer from source features.
//!
//! This is ported from the
//! [OpenMapTiles mountain_peak sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/mountain_peak).
//!
//! Mountain peaks come from OpenStreetMap data and are ranked by importance (based on if they have a name or wikipedia
//! page) then by elevation. Uses the "label grid" feature to limit label density by only taking the top 5 most
//! important mountain peaks within each 100x100px square.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use geo::{BoundingRect, Intersects};
use geo_types::{Geometry, Rect};
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::forwarding_profile::{Handler, LayerPostProcessor};
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::parse;
use planetiler_core::util::translations::Translations;
use planetiler_core::vector_tile;
use tracing::{error, warn};

use crate::generated::open_map_tiles_schema::mountain_peak::*;
use crate::generated::tables::{
    OsmMountainLinestring, OsmMountainLinestringHandler, OsmPeakPoint, OsmPeakPointHandler, RowHandlers, WithEle,
    WithName, WithSource, WithWikipedia,
};
use crate::layer::Layer;
use crate::open_map_tiles_profile::NaturalEarthProcessor;
use crate::util::omt_language_utils;
use crate::util::utils::{elevation, null_if_empty};

/// A polygon with its precomputed bounding box for fast rejection.
struct PreparedPolygon {
    bounds: Rect<f64>,
    geometry: Geometry<f64>,
}

impl PreparedPolygon {
    fn intersects(&self, other: &Geometry<f64>) -> bool {
        other.bounding_rect().is_some_and(|b| b.intersects(&self.bounds)) && self.geometry.intersects(other)
    }
}

pub struct MountainPeak {
    translations: Arc<Translations>,
    stats: StatsRef,
    /// keep track of areas that prefer feet to meters to set customary_ft=1 (just U.S.)
    united_states: OnceLock<PreparedPolygon>,
    logged_no_us: AtomicBool,
}

impl MountainPeak {
    pub fn new(translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, stats: &StatsRef) -> MountainPeak {
        MountainPeak {
            translations: translations.clone(),
            stats: stats.clone(),
            united_states: OnceLock::new(),
            logged_no_us: AtomicBool::new(false),
        }
    }

    /// Returns true if `element` is a point in an area where feet are used instead of meters (the US).
    fn peak_in_area_using_feet(&self, element: &OsmPeakPoint<'_>) -> bool {
        match self.united_states.get() {
            None => {
                if !self.logged_no_us.swap(true, Ordering::Relaxed) {
                    warn!("No US polygon for inferring mountain_peak customary_ft tag");
                }
                false
            }
            Some(us) => match element.source().world_geometry() {
                Ok(geometry) => us.intersects(geometry),
                Err(e) => {
                    e.log_with_stats(
                        &*self.stats,
                        "omt_mountain_peak_us_test",
                        &format!("Unable to test mountain_peak against US polygon: {}", element.source().id()),
                    );
                    false
                }
            },
        }
    }
}

impl NaturalEarthProcessor for MountainPeak {
    fn process_natural_earth(&self, table: &str, feature: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {
        if table == "ne_10m_admin_0_countries" && feature.has_tag_str("iso_a2", "US") {
            // US polygon *should* only be found once
            match feature.polygon() {
                Ok(boundary) => {
                    if let Some(bounds) = boundary.bounding_rect() {
                        let _ = self.united_states.set(PreparedPolygon { bounds, geometry: boundary.clone() });
                    }
                }
                Err(e) => error!("Failed to get United States Polygon for mountain_peak layer: {e}"),
            }
        }
    }
}

impl OsmPeakPointHandler for MountainPeak {
    fn process_osm_peak_point(&self, element: &OsmPeakPoint<'_>, features: &mut FeatureCollector<'_>) {
        let meters = element.ele().and_then(|e| parse::meters_str(&e));
        let has_name = null_if_empty(element.name()).is_some();
        if meters.is_some_and(|m| m.abs() < 10_000.0) || has_name {
            let source = element.source();
            let has_wikipedia = null_if_empty(element.wikipedia()).is_some();
            let in_feet_area = self.peak_in_area_using_feet(element);
            let feature = features
                .point(LAYER_NAME)
                .set_attr(fields::CLASS, source.get_tag("natural").cloned())
                .put_attrs(omt_language_utils::get_names(source.tags(), Some(&self.translations)));
            if let Some(meters) = meters {
                let (ele, ele_ft) = elevation(meters);
                feature.set_attr(fields::ELE, ele).set_attr(fields::ELE_FT, ele_ft);
            }
            feature
                .set_sort_key_descending(
                    meters.map_or(0, |m| m as i32)
                        + if has_wikipedia { 10_000 } else { 0 }
                        + if has_name { 10_000 } else { 0 },
                )
                .set_min_zoom(7)
                // need to use a larger buffer size to allow enough points through to not cut off
                // any label grid squares which could lead to inconsistent label ranks for a feature
                // in adjacent tiles. post_process() will remove anything outside the desired buffer.
                .set_buffer_pixels(100.0)
                .set_point_label_grid_size_and_limit(13, 100.0, 5);
            if in_feet_area {
                feature.set_attr(fields::CUSTOMARY_FT, 1);
            }
        }
    }
}

impl OsmMountainLinestringHandler for MountainPeak {
    fn process_osm_mountain_linestring(
        &self,
        element: &OsmMountainLinestring<'_>,
        features: &mut FeatureCollector<'_>,
    ) {
        // TODO rank is approximate to sort important/named ridges before others, should switch to labelgrid for
        // linestrings later
        let rank = 3
            - if null_if_empty(element.wikipedia()).is_some() { 1 } else { 0 }
            - if null_if_empty(element.name()).is_some() { 1 } else { 0 };
        let source = element.source();
        features
            .line(LAYER_NAME)
            .set_attr(fields::CLASS, source.get_tag("natural").cloned())
            .set_attr(fields::RANK, rank)
            .put_attrs(omt_language_utils::get_names(source.tags(), Some(&self.translations)))
            .set_sort_key(rank)
            .set_min_zoom(13)
            .set_buffer_pixels(100.0);
    }
}

fn inside_tile_buffer_coord(x_or_y: f64) -> bool {
    (-BUFFER_SIZE..=256.0 + BUFFER_SIZE).contains(&x_or_y)
}

impl MountainPeak {
    fn inside_tile_buffer(&self, feature: &vector_tile::Feature) -> bool {
        match feature.geometry.decode() {
            Ok(Geometry::Point(point)) => inside_tile_buffer_coord(point.x()) && inside_tile_buffer_coord(point.y()),
            Ok(_) => true,
            Err(e) => {
                e.log_with_stats(
                    &*self.stats,
                    "mountain_peak_decode_point",
                    &format!("Error decoding mountain peak point: {:?}", feature.tags),
                );
                false
            }
        }
    }
}

impl LayerPostProcessor for MountainPeak {
    fn post_process(&self, _zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        let mut group_counts: HashMap<i64, i64> = HashMap::new();
        items.retain_mut(|feature| {
            let count = group_counts.entry(feature.group).or_insert(1);
            let gridrank = *count;
            *count += 1;
            // now that we have accurate ranks, remove anything outside the desired buffer
            if !self.inside_tile_buffer(feature) {
                false
            } else {
                if !feature.tags.contains_key(fields::RANK) {
                    feature.tags.insert(fields::RANK.into(), gridrank.into());
                }
                true
            }
        });
        Ok(())
    }
}

impl Handler for MountainPeak {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }
}

impl RowHandlers for MountainPeak {
    fn as_osm_peak_point_handler(&self) -> Option<&dyn OsmPeakPointHandler> {
        Some(self)
    }

    fn as_osm_mountain_linestring_handler(&self) -> Option<&dyn OsmMountainLinestringHandler> {
        Some(self)
    }
}

impl Layer for MountainPeak {
    fn as_natural_earth_processor(&self) -> Option<&dyn NaturalEarthProcessor> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use planetiler_core::geo::test_utils::{new_point, rectangle};
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;
    use planetiler_core::value::Tags;

    use super::LAYER_NAME;
    use crate::open_map_tiles_profile::{NATURAL_EARTH_SOURCE, OSM_SOURCE};
    use crate::test_support::*;

    fn new_test() -> LayerTest {
        let t = LayerTest::new();
        t.wikidata_translations.put(123, "es", "es wd name");
        t
    }

    #[test]
    fn test_happy_path() {
        let t = new_test();
        let feature =
            point_feature(tags! {"natural" => "peak", "name" => "test", "ele" => "100", "wikidata" => "Q123"});
        let peak = t.process(&feature);
        assert_features(
            14,
            &[tags! {
                "class" => "peak",
                "ele" => 100,
                "ele_ft" => 328,
                "customary_ft" => "<null>",
                "_layer" => "mountain_peak",
                "_type" => "point",
                "_minzoom" => 7,
                "_maxzoom" => 14,
                "_buffer" => 100.0,
            }],
            &peak,
        );
        assert_features(14, &[tags! {"name:latin" => "test", "name" => "test", "name:es" => "es wd name"}], &peak);
    }

    #[test]
    fn test_label_grid() {
        let t = new_test();
        let feature = point_feature(tags! {"natural" => "peak", "ele" => "100"});
        let peak = t.process(&feature);
        assert_features(14, &[tags! {"_labelgrid_limit" => 0}], &peak);
        assert_features(13, &[tags! {"_labelgrid_limit" => 5, "_labelgrid_size" => 100.0}], &peak);
    }

    #[test]
    fn test_volcano() {
        let t = new_test();
        assert_features(
            14,
            &[tags! {"class" => "volcano"}],
            &t.process(&point_feature(tags! {"natural" => "volcano", "ele" => "100"})),
        );
    }

    #[test]
    fn test_elevation_feet() {
        let t = new_test();
        assert_features(
            14,
            &[tags! {"class" => "volcano", "ele" => 30, "ele_ft" => 100}],
            &t.process(&point_feature(tags! {"natural" => "volcano", "ele" => "100'"})),
        );
    }

    #[test]
    fn test_elevation_feet_inches() {
        let t = new_test();
        assert_features(
            14,
            &[tags! {"class" => "volcano", "ele" => 31, "ele_ft" => 101}],
            &t.process(&point_feature(tags! {"natural" => "volcano", "ele" => "100' 11\""})),
        );
    }

    #[test]
    fn test_saddle() {
        let t = new_test();
        assert_features(
            14,
            &[tags! {"class" => "saddle"}],
            &t.process(&point_feature(tags! {"natural" => "saddle", "ele" => "100"})),
        );
    }

    #[test]
    fn test_no_elevation() {
        let t = new_test();
        assert_features(14, &[], &t.process(&point_feature(tags! {"natural" => "volcano"})));
    }

    #[test]
    fn test_no_elevation_but_with_name() {
        let t = new_test();
        assert_features(
            14,
            &[tags! {"class" => "peak", "name" => "test", "ele" => "<null>", "ele_ft" => "<null>"}],
            &t.process(&point_feature(tags! {"natural" => "peak", "name" => "test"})),
        );
    }

    #[test]
    fn test_bogus_elevation() {
        let t = new_test();
        assert_features(14, &[], &t.process(&point_feature(tags! {"natural" => "volcano", "ele" => "11000"})));
    }

    #[test]
    fn test_ignore_peak_lines() {
        let t = new_test();
        assert_features(
            14,
            &[],
            &t.process(&line_feature(tags! {"natural" => "peak", "name" => "name", "ele" => "100"})),
        );
    }

    #[test]
    fn test_mountain_linestring() {
        let t = new_test();
        assert_features(
            14,
            &[tags! {
                "class" => "ridge",
                "name" => "Ridge",
                "_layer" => "mountain_peak",
                "_type" => "line",
                "_minzoom" => 13,
                "_maxzoom" => 14,
                "_buffer" => 100.0,
            }],
            &t.process(&line_feature(tags! {"natural" => "ridge", "name" => "Ridge"})),
        );
    }

    #[test]
    fn test_customary_ft() {
        let t = new_test();
        let us = SimpleFeature::create(
            rectangle(0.0, 0.1),
            tags! {"iso_a2" => "US"},
            Some(NATURAL_EARTH_SOURCE),
            Some("ne_10m_admin_0_countries"),
            0,
        );
        t.process(&us);

        // inside US - customary_ft=1
        let inside = SimpleFeature::create(
            new_point(0.0, 0.0),
            tags! {"natural" => "volcano", "ele" => "100"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
        );
        assert_features(
            14,
            &[tags! {"class" => "volcano", "customary_ft" => 1, "ele" => 100, "ele_ft" => 328}],
            &t.process(&inside),
        );

        // outside US - customary_ft omitted
        let outside = SimpleFeature::create(
            new_point(1.0, 1.0),
            tags! {"natural" => "volcano", "ele" => "100"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
        );
        assert_features(
            14,
            &[tags! {"class" => "volcano", "customary_ft" => "<null>", "ele" => 100, "ele_ft" => 328}],
            &t.process(&outside),
        );
    }

    fn get_sort_key(t: &LayerTest, tags: Tags) -> i32 {
        let feature = point_feature(tags);
        t.process(&feature).iter().next().unwrap().get_sort_key()
    }

    #[test]
    fn test_sort_key() {
        let t = new_test();
        assert_ascending(&[
            get_sort_key(&t, tags! {"natural" => "peak", "name" => "name", "wikipedia" => "wikilink", "ele" => "100"}),
            get_sort_key(&t, tags! {"natural" => "peak", "name" => "name", "ele" => "100"}),
            get_sort_key(&t, tags! {"natural" => "peak", "ele" => "100"}),
        ]);
    }

    #[test]
    fn test_mountain_peak_post_processing() {
        let t = new_test();
        assert_eq!(Vec::<planetiler_core::vector_tile::Feature>::new(), t.post_process(LAYER_NAME, 13, vec![]));

        assert_eq!(
            vec![vt_point_feature(LAYER_NAME, tags! {"rank" => 1}, 1)],
            t.post_process(LAYER_NAME, 13, vec![vt_point_feature(LAYER_NAME, Tags::new(), 1)])
        );

        assert_eq!(
            vec![
                vt_point_feature(LAYER_NAME, tags! {"rank" => 1, "name" => "a"}, 1),
                vt_point_feature(LAYER_NAME, tags! {"rank" => 2, "name" => "b"}, 1),
                vt_point_feature(LAYER_NAME, tags! {"rank" => 1, "name" => "c"}, 2),
            ],
            t.post_process(
                LAYER_NAME,
                13,
                vec![
                    vt_point_feature(LAYER_NAME, tags! {"name" => "a"}, 1),
                    vt_point_feature(LAYER_NAME, tags! {"name" => "b"}, 1),
                    vt_point_feature(LAYER_NAME, tags! {"name" => "c"}, 2),
                ]
            )
        );
    }

    #[test]
    fn test_mountain_peak_post_processing_limits_features_outside_zoom() {
        let t = new_test();
        let feature = |id, xy: f64, attrs, group| vt_feature(LAYER_NAME, id, &new_point(xy, xy).into(), attrs, group);
        // Java sets dropped features to null, Rust removes them
        assert_eq!(
            vec![feature(1, -64.0, tags! {"rank" => 1}, 1), feature(3, 256.0 + 64.0, tags! {"rank" => 1}, 2)],
            t.post_process(
                LAYER_NAME,
                13,
                vec![
                    feature(1, -64.0, Tags::new(), 1),
                    feature(2, -65.0, Tags::new(), 1),
                    feature(3, 256.0 + 64.0, Tags::new(), 2),
                    feature(4, 256.0 + 65.0, Tags::new(), 2),
                ]
            )
        );
    }
}
