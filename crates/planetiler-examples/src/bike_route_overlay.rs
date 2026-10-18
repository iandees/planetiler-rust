//! Port of `BikeRouteOverlay.java`: builds a map of bicycle routes from ways contained in OpenStreetMap relations
//! tagged with [`type=route`](https://wiki.openstreetmap.org/wiki/Tag:type%3Droute) and
//! [`route=bicycle`](https://wiki.openstreetmap.org/wiki/Tag:route%3Dbicycle) in 3 steps:
//!
//! 1. On the first pass through the input file, store relevant information from OSM bike route relations
//! 2. On the second pass, emit linestrings for each OSM way contained in one of those relations
//! 3. Before storing each finished tile, merge linestrings in each tile with the same tags and touching endpoints

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

/// Minimal container for data we extract from OSM bicycle route relations. This is held in RAM so keep it small.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteRelationInfo {
    /// OSM ID of the relation (required)
    pub id: i64,
    // Values for tags extracted from the OSM relation:
    pub name: Option<String>,
    pub r#ref: Option<String>,
    pub route: Option<String>,
    pub network: &'static str,
}

impl OsmRelationInfo for RouteRelationInfo {
    fn id(&self) -> i64 {
        self.id
    }
}

/// Bicycle route overlay profile.
#[derive(Debug, Default)]
pub struct BikeRouteOverlay;

impl Profile for BikeRouteOverlay {
    /// Step 1) Planetiler processes the .osm.pbf input file in two passes. The first pass stores node locations, and
    /// invokes `preprocess_osm_relation` for each relation and stores information the profile needs during the second
    /// pass when we emit map features for ways contained in that relation.
    fn preprocess_osm_relation(&self, relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
        // If this is a "route" relation where route=bicycle or route=mtb ...
        if relation.has_tag_str("type", "route") && relation.has_tag_any("route", &["mtb", "bicycle"]) {
            // then store a RouteRelationInfo instance with tags we'll need later
            return Some(vec![Arc::new(RouteRelationInfo {
                id: relation.id,
                name: relation.get_string("name"),
                r#ref: relation.get_string("ref"),
                route: relation.get_string("route"),
                // except map network abbreviation to a human-readable value
                network: match relation.get_string_or("network", "").as_str() {
                    "icn" => "international",
                    "ncn" => "national",
                    "rcn" => "regional",
                    "lcn" => "local",
                    _ => "other",
                },
            })]);
        }
        // for any other relation, return None to ignore
        None
    }

    /// Step 2) On the second pass through the input .osm.pbf file, for each way in a relation that we stored data
    /// about, emit a linestring map feature with attributes derived from the relation.
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        // ignore nodes and ways that should only be treated as polygons
        if source_feature.can_be_line() {
            // get all the RouteRelationInfo instances we returned from preprocess_osm_relation that this way belongs to
            for route_info in source_feature.relation_info_with_super::<RouteRelationInfo>(true) {
                // (route_info.role() also has the "role" of this relation member if needed)
                let relation = route_info.relation();
                // Break the output into layers named: "{bicycle,route}-route-{international,national,regional,local,other}"
                let layer_name = format!("{}-route-{}", relation.route.as_deref().unwrap_or("null"), relation.network);
                features
                    .line(&layer_name)
                    .set_attr("name", relation.name.clone())
                    .set_attr("ref", relation.r#ref.clone())
                    .set_zoom_range(0, 14)
                    // don't filter out short line segments even at low zooms because the next step needs them
                    // to merge lines with the same tags where the endpoints are touching
                    .set_min_pixel_size(0.0);
            }
        }
    }

    /// Step 3) Before writing tiles to the output, first merge linestrings where the endpoints are touching that share
    /// the same tags to improve line and text rendering in clients.
    fn post_process_layer_features(
        &self,
        _layer: &str,
        _zoom: u32,
        items: &mut Vec<vector_tile::Feature>,
    ) -> GeometryResult<()> {
        // `merge_line_strings` combines lines with the same tags where the endpoints touch. Tiles are 256x256 pixels
        // and all FeatureMerge operations work in tile pixel coordinates.
        *items = feature_merge::merge_line_strings(
            std::mem::take(items),
            0.5, // after merging, remove lines that are still less than 0.5px long
            0.1, // simplify output linestrings using a 0.1px tolerance
            4.0, // remove any detail more than 4px outside the tile boundary
        );
        Ok(())
    }

    fn name(&self) -> String {
        "Bike Paths Overlay".into()
    }

    fn description(&self) -> Option<String> {
        Some("An example overlay showing bicycle routes".into())
    }

    fn is_overlay(&self) -> bool {
        true // when true sets type=overlay, otherwise type=baselayer
    }

    fn attribution(&self) -> Option<String> {
        Some(OSM_ATTRIBUTION.into())
    }
}

/// Main entrypoint for this example program.
pub fn main<S: AsRef<str>>(args: &[S]) -> anyhow::Result<()> {
    run(crate::parse_args(args)?)
}

/// Runs the example with `args`.
pub fn run(args: Arguments) -> anyhow::Result<()> {
    let area = args.get_string("area", "geofabrik area to download", "monaco");
    Planetiler::create(args)?
        .set_profile(BikeRouteOverlay)
        // override this default with osm_path="path/to/data.osm.pbf"
        .add_osm_source_with_url(
            "osm",
            Path::new("data").join("sources").join(format!("{area}.osm.pbf")),
            Some(&format!("geofabrik:{area}")),
        )?
        // override this default with output="path/to/output.mbtiles"
        .overwrite_output(Path::new("data").join("bikeroutes.mbtiles"))?
        .run()
}

#[cfg(test)]
mod tests {
    use planetiler_core::geo::geo_utils;
    use planetiler_core::geo::geometry_type::GeometryType;
    use planetiler_core::geo::test_utils::new_line_string;
    use planetiler_core::mbtiles::mbtiles::Mbtiles;
    use planetiler_core::reader::osm::osm_element::{Member, OsmElementType};
    use planetiler_core::reader::osm::osm_relation_info::RelationMember;
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;
    use planetiler_core::value::Tags;

    use super::*;
    use crate::test_support::*;

    #[test]
    fn test_source_feature_processing() {
        let profile = BikeRouteOverlay;
        // step 1) preprocess an example OSM relation
        let relation_result = profile
            .preprocess_osm_relation(&Relation::with_tags(
                1,
                tags! {"type" => "route", "route" => "bicycle", "name" => "rail trail", "network" => "lcn", "ref" => "1"},
                vec![Member::new(OsmElementType::Way, 2, "role")],
            ))
            .unwrap();
        // step 2) preprocess an example OSM super-relation containing that relation
        let super_relation_result = profile
            .preprocess_osm_relation(&Relation::with_tags(
                2,
                tags! {"type" => "route", "route" => "bicycle", "name" => "national trail network", "network" => "ncn", "ref" => "1"},
                vec![Member::new(OsmElementType::Relation, 1, "role")],
            ))
            .unwrap();

        // step 3) process a way contained in that relation
        let relations = relation_result
            .into_iter()
            .map(|info| RelationMember::new("role", info))
            .chain(super_relation_result.into_iter().map(|info| RelationMember::with_path("role", info, vec![1])))
            .collect();
        let way = SimpleFeature::create_fake_osm_feature(
            new_line_string(&[
                10.0, 20.0, // point 1: 10 east 20 north
                30.0, 40.0, // point 2: 30 east 40 north
            ]),
            Tags::new(),
            None::<&str>,
            None::<&str>,
            3,
            relations,
            None,
        );
        process_source_feature(&way, &profile, |features| {
            // verify output geometry
            assert_eq!(2, features.len());
            let feature = features[0];
            assert_eq!("bicycle-route-local", feature.get_layer());
            assert_eq!(&tags! {"name" => "rail trail", "ref" => "1"}, &**feature.get_attrs_at_zoom(14));
            let feature2 = features[1];
            assert_eq!("bicycle-route-national", feature2.get_layer());
            assert_eq!(&tags! {"name" => "national trail network", "ref" => "1"}, &**feature2.get_attrs_at_zoom(14));
            // output geometry is in world coordinates where 0,0 is top left and 1,1 is bottom right
            assert!((0.085 - geo_utils::length(feature.get_geometry())).abs() < 1e-2);
            assert_eq!(0, feature.get_min_zoom());
            assert_eq!(14, feature.get_max_zoom());
        });
    }

    #[test]
    fn test_tile_post_processing_merges_connected_lines() {
        let layer = "bicycle-route-local";
        let attrs = tags! {"name" => "rail trail", "ref" => "1"};
        // segment 1: (0, 0) to (10, 0)
        let line1 = vector_tile::Feature::new(
            layer,
            1,
            vector_tile::encode_geometry(&new_line_string(&[0.0, 0.0, 10.0, 0.0]).into()),
            attrs.clone(),
        );
        // segment 2: (10, 0) to (20, 0)
        let line2 = vector_tile::Feature::new(
            layer,
            2,
            vector_tile::encode_geometry(&new_line_string(&[10.0, 0.0, 20.0, 0.0]).into()),
            attrs.clone(),
        );
        // merged: (0, 0) to (20, 0)
        let connected = vector_tile::Feature::new(
            layer,
            0,
            vector_tile::encode_geometry(&new_line_string(&[0.0, 0.0, 20.0, 0.0]).into()),
            attrs,
        );

        // ensure that 2 touching linestrings with same tags are merged
        let mut items = vec![line1, line2];
        profile_post_process(&BikeRouteOverlay, layer, &mut items);
        assert_eq!(vec![connected], items);
    }

    pub(crate) fn profile_post_process(profile: &dyn Profile, layer: &str, items: &mut Vec<vector_tile::Feature>) {
        profile.post_process_layer_features(layer, 14, items).unwrap();
    }

    #[test]
    fn integration_test() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("output.mbtiles");
        run(Arguments::of_pairs([
            ("osm_path", resource("monaco-latest.osm.pbf").display().to_string()),
            ("tmpdir", tmp.path().display().to_string()),
            ("output", db_path.display().to_string()),
        ]))
        .unwrap();
        let mbtiles = Mbtiles::new_read_only_database(&db_path).unwrap();
        assert_metadata(&mbtiles, "Bike Paths Overlay");
        assert_num_features(
            &mbtiles,
            "bicycle-route-international",
            14,
            &tags! {"name" => "EuroVelo 8 - Mediterranean Route - part Monaco", "ref" => "EV8"},
            13,
            GeometryType::Line,
        );
        assert_tile_duplicates(&mbtiles, 0);
    }
}
