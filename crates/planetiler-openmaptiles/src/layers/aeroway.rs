//! Port of `layers/Aeroway.java`: defines the logic for generating map elements in the `aeroway` layer from source
//! features.
//!
//! This is ported from the
//! [OpenMapTiles aeroway sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/aeroway).

use std::sync::Arc;

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::forwarding_profile::Handler;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;

use crate::generated::open_map_tiles_schema::aeroway::*;
use crate::generated::tables::{
    OsmAerowayLinestring, OsmAerowayLinestringHandler, OsmAerowayPoint, OsmAerowayPointHandler, OsmAerowayPolygon,
    OsmAerowayPolygonHandler, RowHandlers, WithAeroway, WithRef,
};
use crate::layer::Layer;

pub struct Aeroway {}

impl Aeroway {
    pub fn new(_translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, _stats: &StatsRef) -> Aeroway {
        Aeroway {}
    }
}

impl OsmAerowayPolygonHandler for Aeroway {
    fn process_osm_aeroway_polygon(&self, element: &OsmAerowayPolygon<'_>, features: &mut FeatureCollector<'_>) {
        features
            .polygon(LAYER_NAME)
            .set_min_zoom(10)
            .set_min_pixel_size(2.0)
            .set_attr(fields::CLASS, element.aeroway().as_deref())
            .set_attr(fields::REF, element.r#ref().as_deref());
    }
}

impl OsmAerowayLinestringHandler for Aeroway {
    fn process_osm_aeroway_linestring(&self, element: &OsmAerowayLinestring<'_>, features: &mut FeatureCollector<'_>) {
        features
            .line(LAYER_NAME)
            .set_min_zoom(10)
            .set_attr(fields::CLASS, element.aeroway().as_deref())
            .set_attr(fields::REF, element.r#ref().as_deref());
    }
}

impl OsmAerowayPointHandler for Aeroway {
    fn process_osm_aeroway_point(&self, element: &OsmAerowayPoint<'_>, features: &mut FeatureCollector<'_>) {
        features
            .point(LAYER_NAME)
            .set_min_zoom(14)
            .set_attr(fields::CLASS, element.aeroway().as_deref())
            .set_attr(fields::REF, element.r#ref().as_deref());
    }
}

impl Handler for Aeroway {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }
}

impl RowHandlers for Aeroway {
    fn as_osm_aeroway_polygon_handler(&self) -> Option<&dyn OsmAerowayPolygonHandler> {
        Some(self)
    }

    fn as_osm_aeroway_linestring_handler(&self) -> Option<&dyn OsmAerowayLinestringHandler> {
        Some(self)
    }

    fn as_osm_aeroway_point_handler(&self) -> Option<&dyn OsmAerowayPointHandler> {
        Some(self)
    }
}

impl Layer for Aeroway {}

#[cfg(test)]
mod tests {
    use planetiler_core::tags;

    use crate::test_support::*;

    #[test]
    fn aeroway_gate() {
        let t = LayerTest::new();
        assert_features(
            14,
            &[tags! {
                "class" => "gate",
                "ref" => "123",
                "_layer" => "aeroway",
                "_type" => "point",
                "_minzoom" => 14,
                "_maxzoom" => 14,
                "_buffer" => 4.0,
            }],
            &t.process(&point_feature(tags! {"aeroway" => "gate", "ref" => "123"})),
        );
        assert_features(14, &[], &t.process(&line_feature(tags! {"aeroway" => "gate"})));
        assert_features(14, &[], &t.process(&polygon_feature(tags! {"aeroway" => "gate"})));
    }

    #[test]
    fn aeroway_line() {
        let t = LayerTest::new();
        assert_features(
            14,
            &[tags! {
                "class" => "runway",
                "ref" => "123",
                "_layer" => "aeroway",
                "_type" => "line",
                "_minzoom" => 10,
                "_maxzoom" => 14,
                "_buffer" => 4.0,
            }],
            &t.process(&line_feature(tags! {"aeroway" => "runway", "ref" => "123"})),
        );
        assert_features(14, &[], &t.process(&point_feature(tags! {"aeroway" => "runway"})));
    }

    #[test]
    fn aeroway_polygon() {
        let t = LayerTest::new();
        assert_features(
            14,
            &[tags! {
                "class" => "runway",
                "ref" => "123",
                "_layer" => "aeroway",
                "_type" => "polygon",
                "_minzoom" => 10,
                "_maxzoom" => 14,
                "_buffer" => 4.0,
            }],
            &t.process(&polygon_feature(tags! {"aeroway" => "runway", "ref" => "123"})),
        );
        assert_features(
            14,
            &[tags! {"class" => "runway", "ref" => "123", "_layer" => "aeroway", "_type" => "polygon"}],
            &t.process(&polygon_feature(tags! {"area:aeroway" => "runway", "ref" => "123"})),
        );
        assert_features(
            14,
            &[tags! {"class" => "heliport", "ref" => "123", "_layer" => "aeroway", "_type" => "polygon"}],
            &t.process(&polygon_feature(tags! {"aeroway" => "heliport", "ref" => "123"})),
        );
        assert_features(14, &[], &t.process(&line_feature(tags! {"aeroway" => "heliport"})));
        assert_features(14, &[], &t.process(&point_feature(tags! {"aeroway" => "heliport"})));
    }
}
