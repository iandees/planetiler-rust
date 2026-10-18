//! Port of `TramRouteOverlay.java`: an overlay of tram routes (from `type=route route=tram` relations), tram ways
//! (`railway=tram`) and tram stops (`railway=tram_stop`).

use std::path::Path;
use std::sync::Arc;

use planetiler_core::config::arguments::Arguments;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::planetiler::Planetiler;
use planetiler_core::profile::Profile;
use planetiler_core::reader::osm::osm_element::Relation;
use planetiler_core::reader::osm::osm_relation_info::OsmRelationInfo;
use planetiler_core::reader::source_feature::{SourceFeature, SourceFeatureExt};
use planetiler_core::reader::with_tags::WithTags;
use planetiler_core::vector_tile;

use crate::{OSM_ATTRIBUTION, feature_merge};

/// Tags extracted from a tram route relation.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteRelationInfo {
    pub id: i64,
    pub r#ref: Option<String>,
    pub name: Option<String>,
    pub network: Option<String>,
}

impl OsmRelationInfo for RouteRelationInfo {
    fn id(&self) -> i64 {
        self.id
    }
}

/// Tram routes and stops profile.
#[derive(Debug, Default)]
pub struct TramRouteOverlay;

impl Profile for TramRouteOverlay {
    fn preprocess_osm_relation(&self, relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
        if relation.has_tag_str("type", "route") && relation.has_tag_str("route", "tram") {
            return Some(vec![Arc::new(RouteRelationInfo {
                id: relation.id,
                r#ref: relation.get_string("ref"),
                name: relation.get_string("name"),
                network: relation.get_string("network"),
            })]);
        }
        Some(vec![])
    }

    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if source_feature.can_be_line() {
            for route_info in source_feature.relation_info_with_super::<RouteRelationInfo>(true) {
                let relation = route_info.relation();
                features
                    .line("tram_route")
                    .set_attr("ref", relation.r#ref.clone())
                    .set_attr("network", relation.network.clone())
                    .set_attr("name", relation.name.clone())
                    .set_min_pixel_size(0.0); // Prevents visual gaps in tram routes
            }
        }

        if source_feature.can_be_line() && source_feature.has_tag_str("railway", "tram") {
            features.line("tram_way").set_attr("ref", source_feature.get_tag("ref").cloned()).set_min_pixel_size(0.0);
        }

        if source_feature.is_point() && source_feature.has_tag_str("railway", "tram_stop") {
            features.point("tram_stop").set_attr("name", source_feature.get_tag("name").cloned()).set_min_zoom(11); // Prevent tram stops from cluttering routes when zoomed far out
        }
    }

    fn post_process_layer_features(
        &self,
        _layer: &str,
        _zoom: u32,
        items: &mut Vec<vector_tile::Feature>,
    ) -> GeometryResult<()> {
        *items = feature_merge::merge_line_strings(
            std::mem::take(items),
            0.5, // remove lines that are less than 0.5px long
            0.1, // simplify linestring output with a 0.1px tolerance
            4.0, // remove detail more than 4px outside the tile boundary
        );
        Ok(())
    }

    fn is_overlay(&self) -> bool {
        true
    }

    fn name(&self) -> String {
        "Tram Routes and Stops Overlay".into()
    }

    fn description(&self) -> Option<String> {
        Some("This example overlay shows tram routes with their stops".into())
    }

    fn attribution(&self) -> Option<String> {
        Some(OSM_ATTRIBUTION.into())
    }
}

/// Main entrypoint for the example program.
pub fn main<S: AsRef<str>>(args: &[S]) -> anyhow::Result<()> {
    run(crate::parse_args(args)?)
}

/// Runs the example with `args`.
pub fn run(args: Arguments) -> anyhow::Result<()> {
    let area = args.get_string("area", "geofabrik area to download", "bremen");
    Planetiler::create(args)?
        .set_profile(TramRouteOverlay)
        .add_osm_source_with_url(
            "osm",
            Path::new("data").join("sources").join(format!("{area}.osm.pbf")),
            Some(&format!("geofabrik:{area}")),
        )?
        .overwrite_output(Path::new("data").join("tramroutes.mbtiles"))?
        .run()
}

#[cfg(test)]
mod tests {
    use planetiler_core::geo::geo_utils;
    use planetiler_core::geo::geometry_type::GeometryType;
    use planetiler_core::geo::test_utils::{new_line_string, new_point};
    use planetiler_core::mbtiles::mbtiles::Mbtiles;
    use planetiler_core::reader::osm::osm_element::{Member, OsmElementType};
    use planetiler_core::reader::osm::osm_relation_info::RelationMember;
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;
    use planetiler_core::value::Tags;

    use super::*;
    use crate::test_support::*;

    #[test]
    fn test_source_feature_processing_for_nodes() {
        let node = SimpleFeature::create_tags(new_point(5.0, 4.0), tags! {"railway" => "tram_stop"});
        process_source_feature(&node, &TramRouteOverlay, |features| {
            assert_eq!(1, features.len());
            let point = features[0];
            assert_eq!("tram_stop", point.get_layer());
            assert_eq!(11, point.get_min_zoom());
        });
    }

    #[test]
    fn test_source_processing_feature_for_routes() {
        let relation_result = TramRouteOverlay
            .preprocess_osm_relation(&Relation::with_tags(
                17,
                tags! {"type" => "route", "route" => "tram", "ref" => "17", "name" => "Tram 17", "network" => "BD BBÖ"},
                vec![Member::new(OsmElementType::Way, 2, "role")],
            ))
            .unwrap();
        // Process a way element in the tram route relation
        // test points: (1 west 2 south), (3 east 5 north)
        let way = SimpleFeature::create_fake_osm_feature(
            new_line_string(&[-1.0, -2.0, 3.0, 5.0]),
            Tags::new(),
            None::<&str>,
            None::<&str>,
            35,
            relation_result.into_iter().map(|info| RelationMember::new("role", info)).collect(),
            None,
        );
        process_source_feature(&way, &TramRouteOverlay, |features| {
            assert_eq!(1, features.len());
            let route = features[0];
            assert_eq!("tram_route", route.get_layer());
            assert_eq!(
                &tags! {"ref" => "17", "network" => "BD BBÖ", "name" => "Tram 17"},
                &**route.get_attrs_at_zoom(10)
            );
            // output geometry is in world coordinates where 0,0 is top left and 1,1 is bottom right
            assert!((0.022 - geo_utils::length(route.get_geometry())).abs() < 1e-2);
            assert_eq!(0, route.get_min_zoom());
            assert_eq!(14, route.get_max_zoom());
        });
    }

    #[test]
    fn test_source_feature_processing_for_ways() {
        // tram ways that are not in a tram route; test points: (5 east 7 north), (4 east 11 north)
        let tram_way = SimpleFeature::create_tags(new_line_string(&[5.0, 7.0, 4.0, 11.0]), tags! {"railway" => "tram"});
        process_source_feature(&tram_way, &TramRouteOverlay, |features| {
            assert_eq!(1, features.len());
            let way = features[0];
            assert_eq!("tram_way", way.get_layer());
            assert!((0.011 - geo_utils::length(way.get_geometry())).abs() < 1e-2);
        });
    }

    #[test]
    fn integration_test() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("output.mbtiles");
        run(Arguments::of_pairs([
            ("osm_path", resource("bremen-trams.osm.pbf").display().to_string()),
            ("tmpDir", tmp.path().display().to_string()),
            ("output", db_path.display().to_string()),
        ]))
        .unwrap();
        let mbtiles = Mbtiles::new_read_only_database(&db_path).unwrap();
        assert_metadata(&mbtiles, "Tram Routes and Stops Overlay");
        let none = Tags::new();
        assert_num_features(&mbtiles, "tram_stop", 12, &none, 352, GeometryType::Point);
        assert_num_features(&mbtiles, "tram_route", 12, &none, 1333, GeometryType::Line);
        assert_num_features(&mbtiles, "tram_way", 12, &none, 386, GeometryType::Line);
        assert_tile_duplicates(&mbtiles, 0);
    }
}
