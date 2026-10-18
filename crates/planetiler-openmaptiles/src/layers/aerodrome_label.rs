//! Port of `layers/AerodromeLabel.java`: defines the logic for generating map elements in the `aerodrome_label` layer
//! from source features.
//!
//! This is ported from the
//! [OpenMapTiles aerodrome_layer sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/aerodrome_label).

use std::sync::Arc;

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::expression::multi_expression::Index;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::forwarding_profile::Handler;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;

use crate::generated::open_map_tiles_schema::aerodrome_label::*;
use crate::generated::tables::*;
use crate::layer::Layer;
use crate::util::omt_language_utils;
use crate::util::utils::{elevation_tags_str, null_if_empty, null_or_empty};

pub struct AerodromeLabel {
    class_lookup: Index<&'static str>,
    translations: Arc<Translations>,
}

impl AerodromeLabel {
    pub fn new(translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, _stats: &StatsRef) -> AerodromeLabel {
        AerodromeLabel { class_lookup: field_mappings::CLASS.index(), translations: translations.clone() }
    }
}

impl OsmAerodromeLabelPointHandler for AerodromeLabel {
    fn process_osm_aerodrome_label_point(
        &self,
        element: &OsmAerodromeLabelPoint<'_>,
        features: &mut FeatureCollector<'_>,
    ) {
        let source = element.source;
        let clazz = *self.class_lookup.get_or_else(source, &field_values::CLASS_OTHER);
        let iata = element.iata();
        let important = !null_or_empty(iata.as_deref()) && clazz == field_values::CLASS_INTERNATIONAL;
        let ele = element.ele();
        features
            .centroid(LAYER_NAME)
            .set_buffer_pixels(BUFFER_SIZE)
            .set_min_zoom(if important { 8 } else { 10 })
            .put_attrs(omt_language_utils::get_names(source.tags(), Some(&self.translations)))
            .put_attrs(elevation_tags_str(ele.as_deref()))
            .set_attr(fields::IATA, null_if_empty(iata.as_deref()))
            .set_attr(fields::ICAO, null_if_empty(element.icao().as_deref()))
            .set_attr(fields::CLASS, clazz);
    }
}

impl Handler for AerodromeLabel {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }
}

impl RowHandlers for AerodromeLabel {
    fn as_osm_aerodrome_label_point_handler(&self) -> Option<&dyn OsmAerodromeLabelPointHandler> {
        Some(self)
    }
}

impl Layer for AerodromeLabel {}

#[cfg(test)]
mod tests {
    use planetiler_core::tags;

    use crate::test_support::*;

    fn setup() -> LayerTest {
        let t = LayerTest::new();
        t.wikidata_translations.put(123, "es", "es wd name");
        t
    }

    #[test]
    fn test_intl_with_iata() {
        let t = setup();
        assert_features(
            14,
            &[tags! {
                "class" => "international",
                "ele" => 100,
                "ele_ft" => 328,
                "name" => "osm name",
                "name:es" => "es wd name",
                "_layer" => "aerodrome_label",
                "_type" => "point",
                "_minzoom" => 8,
                "_maxzoom" => 14,
                "_buffer" => 64.0,
            }],
            &t.process(&point_feature(tags! {
                "aeroway" => "aerodrome",
                "name" => "osm name",
                "wikidata" => "Q123",
                "ele" => "100",
                "aerodrome" => "international",
                "iata" => "123",
                "icao" => "1234",
            })),
        );
    }

    #[test]
    fn test_elevation_feet() {
        let t = setup();
        assert_features(
            14,
            &[tags! {"ele" => 100, "ele_ft" => 328}],
            &t.process(&point_feature(tags! {
                "aeroway" => "aerodrome",
                "name" => "osm name",
                "ele" => "328'",
                "aerodrome" => "international",
                "iata" => "123",
                "icao" => "1234",
            })),
        );
    }

    #[test]
    fn test_elevation_feet_inches() {
        let t = setup();
        assert_features(
            14,
            &[tags! {"ele" => 100, "ele_ft" => 328}],
            &t.process(&point_feature(tags! {
                "aeroway" => "aerodrome",
                "name" => "osm name",
                "ele" => "328' 1\"",
                "aerodrome" => "international",
                "iata" => "123",
                "icao" => "1234",
            })),
        );
    }

    #[test]
    fn test_international() {
        let t = setup();
        assert_features(
            14,
            // no IATA
            &[tags! {"class" => "international", "_layer" => "aerodrome_label", "_minzoom" => 10}],
            &t.process(&point_feature(tags! {"aeroway" => "aerodrome", "aerodrome_type" => "international"})),
        );
    }

    #[test]
    fn test_public() {
        let t = setup();
        assert_features(
            14,
            &[tags! {"class" => "public", "_layer" => "aerodrome_label", "_minzoom" => 10}],
            &t.process(&point_feature(tags! {"aeroway" => "aerodrome", "aerodrome_type" => "public airport"})),
        );
        assert_features(
            14,
            &[tags! {"class" => "public", "_layer" => "aerodrome_label"}],
            &t.process(&point_feature(tags! {"aeroway" => "aerodrome", "aerodrome_type" => "civil"})),
        );
    }

    #[test]
    fn test_military() {
        let t = setup();
        assert_features(
            14,
            &[tags! {"class" => "military", "_layer" => "aerodrome_label", "_minzoom" => 10}],
            &t.process(&point_feature(tags! {"aeroway" => "aerodrome", "aerodrome_type" => "military airport"})),
        );
        assert_features(
            14,
            &[tags! {"class" => "military", "_layer" => "aerodrome_label"}],
            &t.process(&point_feature(tags! {"aeroway" => "aerodrome", "military" => "airfield"})),
        );
    }

    #[test]
    fn test_private() {
        let t = setup();
        assert_features(
            14,
            &[tags! {"class" => "private", "_layer" => "aerodrome_label", "_minzoom" => 10}],
            &t.process(&point_feature(tags! {"aeroway" => "aerodrome", "aerodrome_type" => "private"})),
        );
        assert_features(
            14,
            &[tags! {"class" => "private", "_layer" => "aerodrome_label"}],
            &t.process(&point_feature(tags! {"aeroway" => "aerodrome", "aerodrome" => "private"})),
        );
    }

    #[test]
    fn test_other() {
        let t = setup();
        assert_features(
            14,
            &[tags! {"class" => "other", "_layer" => "aerodrome_label", "_minzoom" => 10}],
            &t.process(&point_feature(tags! {"aeroway" => "aerodrome"})),
        );
    }

    #[test]
    fn test_ignore_non_points() {
        let t = setup();
        assert_features(14, &[], &t.process(&line_feature(tags! {"aeroway" => "aerodrome"})));
    }
}
