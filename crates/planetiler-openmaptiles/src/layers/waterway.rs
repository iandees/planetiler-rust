//! Port of `layers/Waterway.java`: defines the logic for generating river map elements in the `waterway` layer from
//! source features.
//!
//! This is ported from the
//! [OpenMapTiles waterway sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/waterway).
//!
//! Uses Natural Earth at lower zoom-levels and OpenStreetMap at higher zoom levels.
//!
//! For OpenStreetMap, attempts to merge disconnected linestrings with the same name at lower zoom levels so that
//! clients can more easily render the name. We also limit their length at merge-time which only has visibility into
//! that feature in a single tile, so at render-time we need to allow through features far enough outside the tile
//! boundary enough to not accidentally filter out a long river only because a short segment of it goes through this
//! tile.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use parking_lot::RwLock;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::forwarding_profile::{Handler, LayerPostProcessor, OsmRelationPreprocessor};
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::reader::osm::osm_element::Relation;
use planetiler_core::reader::osm::osm_relation_info::OsmRelationInfo;
use planetiler_core::reader::source_feature::{SourceFeature, SourceFeatureExt};
use planetiler_core::reader::with_tags::WithTags;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;
use planetiler_core::util::zoom_function::{MeterToPixelThresholds, ZoomFunction, meter_thresholds};
use planetiler_core::value::{Tags, Value};
use planetiler_core::vector_tile;

use crate::feature_merge;
use crate::generated::open_map_tiles_schema::waterway::*;
use crate::generated::tables::*;
use crate::layer::Layer;
use crate::open_map_tiles_profile::{NaturalEarthProcessor, OsmAllProcessor};
use crate::util::omt_language_utils;
use crate::util::utils;

const TEMP_REL_ID_ADDR: &str = "_relid";

fn class_minzoom(class: &str) -> u32 {
    match class {
        "river" | "canal" => 12,
        "stream" | "drain" | "ditch" => 13,
        _ => 14,
    }
}

static MIN_PIXEL_LENGTHS: LazyLock<MeterToPixelThresholds> = LazyLock::new(|| {
    meter_thresholds()
        .put(6, 500_000.0)
        .put(7, 400_000.0)
        .put(8, 300_000.0)
        .put(9, 8_000.0)
        .put(10, 4_000.0)
        .put(11, 1_000.0)
});

/// Info about a `waterway=river` relation with a name.
#[derive(Debug)]
pub struct WaterwayRelation {
    pub id: i64,
    pub names: Tags,
}

impl OsmRelationInfo for WaterwayRelation {
    fn id(&self) -> i64 {
        self.id
    }
}

pub struct Waterway {
    translations: Arc<Translations>,
    config: Arc<PlanetilerConfig>,
    stats: StatsRef,
    min_pixel_lengths: ZoomFunction<f64>,
    /// Total length (world units, `f64` bits) of the ways in each river relation.
    river_relation_lengths: RwLock<HashMap<i64, AtomicU64>>,
}

impl Waterway {
    pub fn new(translations: &Arc<Translations>, config: &Arc<PlanetilerConfig>, stats: &StatsRef) -> Waterway {
        Waterway {
            translations: translations.clone(),
            config: config.clone(),
            stats: stats.clone(),
            min_pixel_lengths: MIN_PIXEL_LENGTHS.clone().into_fn(),
            river_relation_lengths: RwLock::new(HashMap::new()),
        }
    }

    fn add_length(&self, rel_id: i64, length: f64) {
        if let Some(counter) = self.river_relation_lengths.read().get(&rel_id) {
            let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                Some((f64::from_bits(bits) + length).to_bits())
            });
        }
    }

    fn relation_length(&self, rel_id: i64) -> f64 {
        self.river_relation_lengths.read().get(&rel_id).map_or(0.0, |c| f64::from_bits(c.load(Ordering::Relaxed)))
    }
}

// zoom-level 3-5 come from natural earth

impl NaturalEarthProcessor for Waterway {
    fn process_natural_earth(&self, table: &str, feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if feature.has_tag_str("featurecla", "River") {
            let zoom = match table {
                "ne_110m_rivers_lake_centerlines" => Some((3, 3)),
                "ne_50m_rivers_lake_centerlines" => Some((4, 5)),
                _ => None,
            };
            if let Some((min, max)) = zoom {
                features
                    .line(LAYER_NAME)
                    .set_buffer_pixels(BUFFER_SIZE)
                    .set_attr(fields::CLASS, field_values::CLASS_RIVER)
                    .set_zoom_range(min, max);
            }
        }
    }
}

// zoom-level 6-8 come from OSM river relations

impl OsmRelationPreprocessor for Waterway {
    fn preprocess_osm_relation(&self, relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
        if relation.has_tag_str("waterway", "river") && !utils::null_or_empty_value(relation.get_tag("name")) {
            self.river_relation_lengths.write().insert(relation.id, AtomicU64::new(0f64.to_bits()));
            return Some(vec![Arc::new(WaterwayRelation {
                id: relation.id,
                names: omt_language_utils::get_names(&relation.tags, Some(&self.translations)),
            })]);
        }
        None
    }
}

impl OsmAllProcessor for Waterway {
    fn process_all_osm(&self, feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        // fast path: this runs on every OSM element
        if !feature.has_relation_info() {
            return;
        }
        let waterways = feature.relation_info::<WaterwayRelation>();
        if waterways.is_empty() || !feature.can_be_line() {
            return;
        }
        for waterway in waterways {
            let role = waterway.role();
            if role.is_empty() || role == "main_stream" {
                let rel = waterway.relation();
                match feature.length() {
                    Ok(length) => self.add_length(rel.id, length),
                    Err(e) => e.log_with_stats(
                        &*self.stats,
                        "waterway_decode",
                        &format!("Unable to get waterway length for {}", feature.id()),
                    ),
                }
                features
                    .line(LAYER_NAME)
                    .set_attr(TEMP_REL_ID_ADDR, rel.id)
                    .set_buffer_pixels(BUFFER_SIZE)
                    .set_attr(fields::CLASS, field_values::CLASS_RIVER)
                    .put_attrs(rel.names.iter().map(|(k, v)| (k.as_str(), v)))
                    .set_zoom_range(6, 8)
                    .set_min_pixel_size(0.0);
            }
        }
    }
}

// zoom-level 9+ come from OSM river ways

impl OsmWaterwayLinestringHandler for Waterway {
    fn process_osm_waterway_linestring(
        &self,
        element: &OsmWaterwayLinestring<'_>,
        features: &mut FeatureCollector<'_>,
    ) {
        let waterway = element.waterway();
        let waterway = waterway.as_deref().unwrap_or("");
        let important = waterway == "river" && utils::null_if_empty(element.name()).is_some();
        let minzoom = if important { 9 } else { class_minzoom(waterway) };
        features
            .line(LAYER_NAME)
            .set_buffer_pixels(BUFFER_SIZE)
            .set_attr(fields::CLASS, waterway)
            .put_attrs(omt_language_utils::get_names(element.source.tags(), Some(&self.translations)))
            .set_min_zoom(minzoom)
            // details only at higher zoom levels so that named rivers can be merged more aggressively
            .set_attr_with_minzoom(fields::BRUNNEL, utils::brunnel(element.is_bridge(), element.is_tunnel()), 12)
            .set_attr_with_minzoom(fields::INTERMITTENT, if element.is_intermittent() { 1 } else { 0 }, 12)
            // at lower zoom levels, we'll merge linestrings and limit length/clip afterwards
            .set_buffer_pixel_overrides(Some(self.min_pixel_lengths.clone()))
            .set_min_pixel_size_below_zoom(11, 0.0);
    }
}

impl LayerPostProcessor for Waterway {
    fn post_process(&self, zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        if (6..=8).contains(&zoom) {
            // remove ways for river relations if relation is not long enough
            let min_size_at_zoom = MIN_PIXEL_LENGTHS.apply(zoom).unwrap_or(0.0) / 2f64.powi(zoom as i32) / 256.0;
            items.retain_mut(|item| match item.tags.shift_remove(TEMP_REL_ID_ADDR) {
                Some(Value::Int(rel_id)) => self.relation_length(rel_id) >= min_size_at_zoom,
                _ => true,
            });
            *items = feature_merge::merge_line_strings(
                std::mem::take(items),
                self.config.min_feature_size(zoom),
                self.config.tolerance(zoom),
                BUFFER_SIZE,
            );
        } else if (9..=11).contains(&zoom) {
            *items = feature_merge::merge_line_strings(
                std::mem::take(items),
                MIN_PIXEL_LENGTHS.apply(zoom).unwrap_or(0.0),
                self.config.tolerance(zoom),
                BUFFER_SIZE,
            );
        }
        Ok(())
    }
}

impl Handler for Waterway {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }

    fn as_osm_relation_preprocessor(&self) -> Option<&dyn OsmRelationPreprocessor> {
        Some(self)
    }
}

impl RowHandlers for Waterway {
    fn as_osm_waterway_linestring_handler(&self) -> Option<&dyn OsmWaterwayLinestringHandler> {
        Some(self)
    }
}

impl Layer for Waterway {
    fn as_natural_earth_processor(&self) -> Option<&dyn NaturalEarthProcessor> {
        Some(self)
    }

    fn as_osm_all_processor(&self) -> Option<&dyn OsmAllProcessor> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use planetiler_core::geo::test_utils::new_line_string;
    use planetiler_core::profile::Profile;
    use planetiler_core::reader::osm::osm_relation_info::RelationMember;
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;

    use super::*;
    use crate::open_map_tiles_profile::{NATURAL_EARTH_SOURCE, OSM_SOURCE};
    use crate::test_support::*;

    fn test_osm_waterway_relation(is_long_enough: bool) {
        let t = LayerTest::new();
        let mut rel = Relation::new(1);
        rel.tags.insert("name".into(), "River Relation".into());
        rel.tags.insert("name:es".into(), "ES name".into());
        rel.tags.insert("waterway".into(), "river".into());

        let relation_infos = t.profile.preprocess_osm_relation(&rel).unwrap_or_default();
        let feature = SimpleFeature::create_fake_osm_feature(
            new_line_string(&[0.0, 0.0, 0.0, if is_long_enough { 3.0 } else { 1.0 }]),
            Tags::default(),
            Some(OSM_SOURCE),
            None::<&str>,
            0,
            relation_infos.into_iter().map(|r| RelationMember::new("", r)).collect(),
            None,
        );
        assert_features(
            14,
            &[tags! {
                "class" => "river", "name" => "River Relation", "name:es" => "ES name", "_relid" => 1i64,
                "_layer" => "waterway", "_type" => "line", "_minzoom" => 6, "_maxzoom" => 8, "_buffer" => 4.0
            }],
            &t.process(&feature),
        );

        // ensure that post-processing combines waterways, and filters out ones that
        // belong to rivers that are not long enough to be shown
        let line1 = vt_feature(
            LAYER_NAME,
            1,
            &new_line_string(&[0.0, 0.0, 10.0, 0.0]).into(),
            tags! {"name" => "river", "_relid" => 1i64},
            0,
        );
        let line2 = vt_feature(
            LAYER_NAME,
            1,
            &new_line_string(&[10.0, 0.0, 20.0, 0.0]).into(),
            tags! {"name" => "river", "_relid" => 1i64},
            0,
        );
        let connected =
            vt_feature(LAYER_NAME, 0, &new_line_string(&[0.0, 0.0, 20.0, 0.0]).into(), tags! {"name" => "river"}, 0);
        assert_eq!(
            if is_long_enough { vec![connected] } else { vec![] },
            t.post_process(LAYER_NAME, 8, vec![line1, line2])
        );
    }

    #[test]
    fn test_osm_waterway_relation_long_enough() {
        test_osm_waterway_relation(true);
    }

    #[test]
    fn test_osm_waterway_relation_not_long_enough() {
        test_osm_waterway_relation(false);
    }

    #[test]
    fn test_waterway_important_river_process() {
        let t = LayerTest::new();
        let feature = line_feature(tags! {"waterway" => "river", "name" => "charles river", "name:es" => "es name"});
        let charles_river = t.process(&feature);
        assert_features(
            14,
            &[tags! {
                "class" => "river", "name" => "charles river", "name:es" => "es name", "intermittent" => 0,
                "_layer" => "waterway", "_type" => "line", "_minzoom" => 9, "_maxzoom" => 14, "_buffer" => 4.0
            }],
            &charles_river,
        );
        assert_features(
            11,
            &[tags! {
                "class" => "river", "name" => "charles river", "name:es" => "es name", "intermittent" => "<null>",
                "_buffer" => 13.082664546679323
            }],
            &charles_river,
        );
        assert_features(10, &[tags! {"class" => "river", "_buffer" => 26.165329093358647}], &charles_river);
        assert_features(9, &[tags! {"class" => "river", "_buffer" => 26.165329093358647}], &charles_river);
    }

    #[test]
    fn test_waterway_important_river_post_process() {
        let t = LayerTest::new();
        let line1 =
            vt_feature(LAYER_NAME, 1, &new_line_string(&[0.0, 0.0, 10.0, 0.0]).into(), tags! {"name" => "river"}, 0);
        let line2 =
            vt_feature(LAYER_NAME, 1, &new_line_string(&[10.0, 0.0, 20.0, 0.0]).into(), tags! {"name" => "river"}, 0);
        let connected =
            vt_feature(LAYER_NAME, 0, &new_line_string(&[0.0, 0.0, 20.0, 0.0]).into(), tags! {"name" => "river"}, 0);
        assert_eq!(Vec::<vector_tile::Feature>::new(), t.post_process(LAYER_NAME, 11, vec![]));
        assert_eq!(
            vec![line1.clone(), line2.clone()],
            t.post_process(LAYER_NAME, 12, vec![line1.clone(), line2.clone()])
        );
        assert_eq!(vec![connected], t.post_process(LAYER_NAME, 11, vec![line1, line2]));
    }

    #[test]
    fn test_waterway_smaller() {
        let t = LayerTest::new();
        // river with no name is not important
        assert_features(
            14,
            &[
                tags! {"class" => "river", "brunnel" => "bridge", "_layer" => "waterway", "_type" => "line", "_minzoom" => 12},
            ],
            &t.process(&line_feature(tags! {"waterway" => "river", "bridge" => "1"})),
        );
        assert_features(
            14,
            &[tags! {"class" => "canal", "_layer" => "waterway", "_type" => "line", "_minzoom" => 12}],
            &t.process(&line_feature(tags! {"waterway" => "canal", "name" => "name"})),
        );
        assert_features(
            14,
            &[tags! {"class" => "stream", "_layer" => "waterway", "_type" => "line", "_minzoom" => 13}],
            &t.process(&line_feature(tags! {"waterway" => "stream", "name" => "name"})),
        );
    }

    #[test]
    fn test_waterway_natural_earth() {
        let t = LayerTest::new();
        let f = SimpleFeature::create(
            new_line_string(&[0.0, 0.0, 1.0, 1.0]),
            tags! {"featurecla" => "River", "name" => "name"},
            Some(NATURAL_EARTH_SOURCE),
            Some("ne_110m_rivers_lake_centerlines"),
            0,
        );
        assert_features(
            3,
            &[tags! {
                "class" => "river", "name" => "<null>", "intermittent" => "<null>",
                "_layer" => "waterway", "_type" => "line", "_minzoom" => 3, "_maxzoom" => 3
            }],
            &t.process(&f),
        );
        let f = SimpleFeature::create(
            new_line_string(&[0.0, 0.0, 1.0, 1.0]),
            tags! {"featurecla" => "River", "name" => "name"},
            Some(NATURAL_EARTH_SOURCE),
            Some("ne_50m_rivers_lake_centerlines"),
            0,
        );
        assert_features(
            6,
            &[
                tags! {"class" => "river", "intermittent" => "<null>", "_layer" => "waterway", "_type" => "line", "_minzoom" => 4, "_maxzoom" => 5},
            ],
            &t.process(&f),
        );
    }
}
