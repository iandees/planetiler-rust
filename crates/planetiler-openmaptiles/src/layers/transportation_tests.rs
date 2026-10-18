//! Port of `TransportationTest.java` (covers both the `transportation` and `transportation_name` layers).

use std::sync::Arc;

use planetiler_core::config::arguments::Arguments;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::geo::test_utils::{new_line_string, new_point, rectangle};
use planetiler_core::profile::Profile;
use planetiler_core::reader::osm::osm_element::{Node, Relation, Way};
use planetiler_core::reader::osm::osm_relation_info::OsmRelationInfo;
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::tags;
use planetiler_core::value::{Tags, Value};

use crate::open_map_tiles_profile::{NATURAL_EARTH_SOURCE, OSM_SOURCE};
use crate::test_support::*;

const TRANSPORTATION: &str = "transportation";
const TRANSPORTATION_NAME: &str = "transportation_name";

fn set_tag(tags: &mut Tags, key: &str, value: impl Into<Value>) {
    tags.insert(key.to_string(), value.into());
}

fn concat_rels(a: Vec<Arc<dyn OsmRelationInfo>>, b: Vec<Arc<dyn OsmRelationInfo>>) -> Vec<Arc<dyn OsmRelationInfo>> {
    a.into_iter().chain(b).collect()
}

fn config(args: &[(&str, &str)]) -> PlanetilerConfig {
    PlanetilerConfig::from(&Arguments::of_pairs(args.iter().copied())).unwrap()
}

fn route_relation(id: i64, network: &str, r#ref: &str) -> Relation {
    let mut rel = Relation::new(id);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", network);
    set_tag(&mut rel.tags, "ref", r#ref);
    rel
}

fn rel_us() -> Relation {
    route_relation(1, "US:US", "3")
}

fn rel_ma() -> Relation {
    route_relation(2, "US:MA", "2")
}

fn get_way_sort_key(t: &LayerTest, tags: Tags) -> i32 {
    let feature = line_feature(tags);
    let collector = t.process(&feature);
    collector.iter().next().unwrap().get_sort_key()
}

#[test]
fn test_named_footway() {
    let t = LayerTest::new();
    let result_src = line_feature(
        tags! {"name" => "Lagoon Path", "surface" => "asphalt", "level" => "0", "highway" => "footway", "indoor" => "no", "oneway" => "no", "foot" => "designated", "bicycle" => "dismount"},
    );
    let result = t.process(&result_src);
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "_type" => "line", "class" => "path", "subclass" => "footway", "oneway" => "<null>", "name" => "<null>", "layer" => "<null>", "_buffer" => 4.0, "_minpixelsize" => 0.0, "_minzoom" => 13},
            tags! {"_layer" => "transportation_name", "_type" => "line", "class" => "path", "subclass" => "footway", "name" => "Lagoon Path", "name_int" => "Lagoon Path", "name:latin" => "Lagoon Path", "_minpixelsize" => 0.0, "_minzoom" => 13, "_maxzoom" => 14},
        ],
        &result,
    );
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "surface" => "paved", "oneway" => "<null>", "layer" => "<null>", "level" => 0, "ramp" => "<null>", "bicycle" => "dismount", "foot" => "designated"},
            tags! {"_layer" => "transportation_name", "level" => 0, "surface" => "<null>", "oneway" => "<null>", "ramp" => "<null>", "bicycle" => "<null>", "foot" => "<null>"},
        ],
        &result,
    );
}

#[test]
fn test_important_path() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);

    set_tag(&mut rel.tags, "colour", "white");
    set_tag(&mut rel.tags, "name", "Appalachian Trail - 11 MA");
    set_tag(&mut rel.tags, "network", "nwn");
    set_tag(&mut rel.tags, "osmc", "symbol	white::white_stripe");
    set_tag(&mut rel.tags, "ref", "AT");
    set_tag(&mut rel.tags, "route", "hiking");
    set_tag(&mut rel.tags, "short_name", "AT 11 MA");
    set_tag(&mut rel.tags, "symbol", "white-paint blazes");
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "wikidata", "Q620648");
    set_tag(&mut rel.tags, "wikipedia", "en:Appalachian Trail");

    let features_src = line_feature_with_relation(
        t.profile.preprocess_osm_relation(&rel).unwrap(),
        tags! {"bicycle" => "no", "highway" => "path", "horse" => "no", "name" => "Appalachian Trail", "ref" => "AT", "surface" => "ground"},
    );
    let features = t.process(&features_src);
    assert_features(
        12,
        &[
            tags! {"_layer" => "transportation", "_type" => "line", "class" => "path", "subclass" => "path", "oneway" => "<null>", "name" => "<null>", "layer" => "<null>", "_buffer" => 4.0, "_minpixelsize" => 0.0, "_minzoom" => 12},
            tags! {"_layer" => "transportation_name", "_type" => "line", "class" => "path", "subclass" => "path", "name" => "Appalachian Trail", "name_int" => "Appalachian Trail", "name:latin" => "Appalachian Trail", "_minpixelsize" => 0.0, "_minzoom" => 12},
        ],
        &features,
    );
}

#[test]
fn test_unnamed_path() {
    let t = LayerTest::new();
    assert_features(
        14,
        &[
            tags! {"_layer" => "transportation", "class" => "path", "subclass" => "path", "surface" => "unpaved", "oneway" => "<null>", "_minzoom" => 14},
        ],
        &t.process(&line_feature(tags! {"surface" => "dirt", "highway" => "path"})),
    );
}

#[test]
fn test_private_path() {
    let t = LayerTest::new();
    assert_features(
        9,
        &[tags! {"_layer" => "transportation", "class" => "path", "access" => "no"}],
        &t.process(&line_feature(tags! {"access" => "private", "highway" => "path"})),
    );
    assert_features(
        9,
        &[tags! {"_layer" => "transportation", "class" => "path", "access" => "no"}],
        &t.process(&line_feature(tags! {"access" => "no", "highway" => "path"})),
    );
    assert_features(
        8,
        &[tags! {"_layer" => "transportation", "class" => "path", "access" => "<null>"}],
        &t.process(&line_feature(tags! {"access" => "no", "highway" => "path"})),
    );
}

#[test]
fn test_official_path() {
    let t = LayerTest::new();
    assert_features(
        9,
        &[tags! {"_layer" => "transportation", "class" => "path", "subclass" => "footway", "official" => 1}],
        &t.process(&line_feature(tags! {"highway" => "footway", "operator" => "National Park Service"})),
    );
    assert_features(
        9,
        &[tags! {"_layer" => "transportation", "class" => "path", "subclass" => "bridleway", "official" => 1}],
        &t.process(&line_feature(tags! {"highway" => "bridleway", "informal" => "no"})),
    );
    assert_features(
        9,
        &[tags! {"_layer" => "transportation", "class" => "path", "subclass" => "cycleway", "official" => "<null>"}],
        &t.process(&line_feature(tags! {"highway" => "cycleway"})),
    );
}

#[test]
fn test_informal_path() {
    let t = LayerTest::new();
    assert_features(
        9,
        &[tags! {"_layer" => "transportation", "class" => "path", "subclass" => "path", "official" => 0}],
        &t.process(&line_feature(tags! {"highway" => "path", "informal" => "yes"})),
    );
}

#[test]
fn test_expressway() {
    let t = LayerTest::new();
    assert_features(
        8,
        &[tags! {"_layer" => "transportation", "class" => "motorway", "expressway" => "<null>"}],
        &t.process(&line_feature(tags! {"highway" => "motorway", "expressway" => "yes"})),
    );
    assert_features(
        8,
        &[tags! {"_layer" => "transportation", "class" => "primary", "expressway" => 1}],
        &t.process(&line_feature(tags! {"highway" => "primary", "expressway" => "yes"})),
    );
    assert_features(
        7,
        &[tags! {"_layer" => "transportation", "class" => "primary", "expressway" => "<null>"}],
        &t.process(&line_feature(tags! {"highway" => "primary", "expressway" => "yes"})),
    );
}

#[test]
fn test_toll() {
    let t = LayerTest::new();
    assert_features(
        9,
        &[tags! {"_layer" => "transportation", "class" => "motorway", "toll" => "<null>"}],
        &t.process(&line_feature(tags! {"highway" => "motorway"})),
    );
    assert_features(
        9,
        &[tags! {"_layer" => "transportation", "class" => "motorway", "toll" => 1}],
        &t.process(&line_feature(tags! {"highway" => "motorway", "toll" => "yes"})),
    );
    assert_features(
        8,
        &[tags! {"_layer" => "transportation", "class" => "motorway", "toll" => "<null>"}],
        &t.process(&line_feature(tags! {"highway" => "motorway", "toll" => "yes"})),
    );
}

#[test]
fn test_indoor_tunnel_steps() {
    let t = LayerTest::new();
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "path", "subclass" => "steps", "brunnel" => "tunnel", "indoor" => 1, "oneway" => 1, "ramp" => "<null>"},
        ],
        &t.process(&line_feature(
            tags! {"highway" => "steps", "tunnel" => "building_passage", "oneway" => "yes", "indoor" => "yes"},
        )),
    );
}

#[test]
fn test_interstate_motorway() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "US:I");
    set_tag(&mut rel.tags, "ref", "90");

    let features_src = line_feature_with_relation(
        t.profile.preprocess_osm_relation(&rel).unwrap(),
        tags! {"highway" => "motorway", "oneway" => "yes", "name" => "Massachusetts Turnpike", "ref" => "I 90", "surface" => "asphalt", "foot" => "no", "bicycle" => "no", "horse" => "no", "bridge" => "yes"},
    );
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "motorway", "surface" => "paved", "oneway" => 1, "ramp" => "<null>", "bicycle" => "no", "foot" => "no", "horse" => "no", "brunnel" => "bridge", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "motorway", "name" => "Massachusetts Turnpike", "name_en" => "Massachusetts Turnpike", "ref" => "90", "ref_length" => 2, "network" => "us-interstate", "route_1_network" => "US:I", "route_1_ref" => "90", "brunnel" => "<null>", "_minzoom" => 6},
        ],
        &features,
    );

    assert_features(
        8,
        &[
            tags! {"_layer" => "transportation", "class" => "motorway", "surface" => "<null>", "oneway" => "<null>", "ramp" => "<null>", "bicycle" => "<null>", "foot" => "<null>", "horse" => "<null>", "brunnel" => "bridge", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "motorway", "name" => "Massachusetts Turnpike", "name_en" => "Massachusetts Turnpike", "ref" => "90", "ref_length" => 2, "network" => "us-interstate", "route_1_network" => "US:I", "route_1_ref" => "90", "brunnel" => "<null>", "_minzoom" => 6},
        ],
        &features,
    );
}

#[test]
fn test_duplicate_route() {
    let t = LayerTest::new();
    let mut rel1 = Relation::new(1);
    set_tag(&mut rel1.tags, "type", "route");
    set_tag(&mut rel1.tags, "route", "road");
    set_tag(&mut rel1.tags, "network", "US:OK");
    set_tag(&mut rel1.tags, "ref", "104");
    set_tag(&mut rel1.tags, "direction", "north");
    let mut rel2 = Relation::new(1);
    set_tag(&mut rel2.tags, "type", "route");
    set_tag(&mut rel2.tags, "route", "road");
    set_tag(&mut rel2.tags, "network", "US:OK");
    set_tag(&mut rel2.tags, "ref", "104");
    set_tag(&mut rel2.tags, "direction", "south");

    let features_src = line_feature_with_relation(
        concat_rels(
            t.profile.preprocess_osm_relation(&rel2).unwrap(),
            t.profile.preprocess_osm_relation(&rel1).unwrap(),
        ),
        tags! {"highway" => "trunk", "ref" => "US 23;SR 104", "lanes" => 5, "maxspeed" => "55 mph", "expressway" => "no"},
    );
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "network" => "us-state", "_minzoom" => 6},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "ref" => "104", "ref_length" => 3, "network" => "us-state", "route_1_network" => "US:OK", "route_1_ref" => "104", "route_2_network" => "<null>", "route_2_ref" => "<null>", "_minzoom" => 8},
        ],
        &features,
    );
}

#[test]
fn test_route_without_network_type() {
    let t = LayerTest::new();
    let mut rel1 = Relation::new(1);
    set_tag(&mut rel1.tags, "type", "route");
    set_tag(&mut rel1.tags, "route", "road");
    set_tag(&mut rel1.tags, "network", "US:NJ:NJTP");
    set_tag(&mut rel1.tags, "ref", "NJTP");
    set_tag(&mut rel1.tags, "name", "New Jersey Turnpike (mainline)");

    let mut rel2 = Relation::new(2);
    set_tag(&mut rel2.tags, "type", "route");
    set_tag(&mut rel2.tags, "route", "road");
    set_tag(&mut rel2.tags, "network", "US:I");
    set_tag(&mut rel2.tags, "ref", "95");
    set_tag(&mut rel2.tags, "name", "I 95 (NJ)");

    let rendered_src = line_feature_with_relation(
        concat_rels(
            t.profile.preprocess_osm_relation(&rel1).unwrap(),
            t.profile.preprocess_osm_relation(&rel2).unwrap(),
        ),
        tags! {"highway" => "motorway", "name" => "New Jersey Turnpike", "ref" => "I 95;NJTP"},
    );
    let rendered = t.process(&rendered_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "motorway", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "motorway", "name" => "New Jersey Turnpike", "ref" => "95", "ref_length" => 2, "route_1_network" => "US:I", "route_1_ref" => "95", "route_2_network" => "US:NJ:NJTP", "route_2_ref" => "NJTP", "_minzoom" => 6},
        ],
        &rendered,
    );
}

#[test]
fn test_route_with_colour() {
    let t = LayerTest::new();
    let mut rel1 = Relation::new(1);
    set_tag(&mut rel1.tags, "type", "route");
    set_tag(&mut rel1.tags, "route", "road");
    set_tag(&mut rel1.tags, "network", "US:NJ:NJTP");
    set_tag(&mut rel1.tags, "ref", "NJTP");
    set_tag(&mut rel1.tags, "name", "New Jersey Turnpike (mainline)");
    set_tag(&mut rel1.tags, "colour", "#FFFFFF");
    set_tag(&mut rel1.tags, "ref:colour", "#888888");

    let mut rel2 = Relation::new(2);
    set_tag(&mut rel2.tags, "type", "route");
    set_tag(&mut rel2.tags, "route", "road");
    set_tag(&mut rel2.tags, "network", "US:I");
    set_tag(&mut rel2.tags, "ref", "95");
    set_tag(&mut rel2.tags, "name", "I 95 (NJ)");
    set_tag(&mut rel2.tags, "ref:colour", "#000000");

    let rendered_src = line_feature_with_relation(
        concat_rels(
            t.profile.preprocess_osm_relation(&rel1).unwrap(),
            t.profile.preprocess_osm_relation(&rel2).unwrap(),
        ),
        tags! {"highway" => "motorway", "name" => "New Jersey Turnpike", "ref" => "I 95;NJTP"},
    );
    let rendered = t.process(&rendered_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "motorway", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "motorway", "route_1_network" => "US:I", "route_1_colour" => "#000000", "route_2_network" => "US:NJ:NJTP", "route_2_colour" => "#FFFFFF", "_minzoom" => 6},
        ],
        &rendered,
    );
}

#[test]
fn test_segment_with_many_routes() {
    let t = LayerTest::new();
    let mut relations: Vec<Arc<dyn OsmRelationInfo>> = Vec::new();
    for route in 1..=16 {
        let num = (route + 1) / 2; // to make dups
        let mut rel = Relation::new(num);
        set_tag(&mut rel.tags, "type", "route");
        set_tag(&mut rel.tags, "route", "road");
        set_tag(&mut rel.tags, "network", "US:I");
        set_tag(&mut rel.tags, "ref", num.to_string());
        set_tag(&mut rel.tags, "name", format!("Route {num}"));
        relations.extend(t.profile.preprocess_osm_relation(&rel).unwrap());
    }

    let rendered_src = line_feature_with_relation(
        relations,
        tags! {"highway" => "motorway", "name" => "New Jersey Turnpike", "ref" => "I 95;NJTP"},
    );
    let rendered = t.process(&rendered_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "motorway", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "route_1_network" => "US:I", "route_1_ref" => "1", "route_2_network" => "US:I", "route_2_ref" => "2", "route_3_network" => "US:I", "route_3_ref" => "3", "route_4_network" => "US:I", "route_4_ref" => "4", "route_5_network" => "US:I", "route_5_ref" => "5", "route_6_network" => "US:I", "route_6_ref" => "6", "route_7_network" => "US:I", "route_7_ref" => "7", "route_8_network" => "US:I", "route_8_ref" => "8", "route_9_network" => "<null>", "route_9_ref" => "<null>"},
        ],
        &rendered,
    );
}

#[test]
fn test_minor_route_ref() {
    let t = LayerTest::new();
    let mut rel1 = Relation::new(1);
    set_tag(&mut rel1.tags, "type", "route");
    set_tag(&mut rel1.tags, "route", "road");
    set_tag(&mut rel1.tags, "network", "rwn");
    set_tag(&mut rel1.tags, "ref", "GFW");
    set_tag(&mut rel1.tags, "name", "Georg-Fahrbach-Weg");

    assert_features(
        13,
        &[tags! {"_layer" => "transportation", "class" => "tertiary"}],
        &t.process(&line_feature_with_relation(
            t.profile.preprocess_osm_relation(&rel1).unwrap(),
            tags! {"highway" => "tertiary"},
        )),
    );

    let t = LayerTest::with_config(config(&[("transportation_name_minor_refs", "true")]));
    let feature =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel1).unwrap(), tags! {"highway" => "tertiary"});
    let collector = t.process(&feature);
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "tertiary"},
            tags! {"_layer" => "transportation_name", "class" => "tertiary", "ref" => "GFW", "network" => "road"},
        ],
        &collector,
    );
}

#[test]
fn test_polish_highway_issue165() {
    let t = LayerTest::new();
    let mut rel1 = Relation::new(1);
    set_tag(&mut rel1.tags, "type", "route");
    set_tag(&mut rel1.tags, "route", "road");
    set_tag(&mut rel1.tags, "network", "e-road");
    set_tag(&mut rel1.tags, "ref", "E 77");
    set_tag(&mut rel1.tags, "name", "European route E 77");

    let mut rel2 = Relation::new(2);
    set_tag(&mut rel2.tags, "type", "route");
    set_tag(&mut rel2.tags, "route", "road");
    set_tag(&mut rel2.tags, "network", "e-road");
    set_tag(&mut rel2.tags, "ref", "E 28");
    set_tag(&mut rel2.tags, "name", "European route E 28");

    let rendered_src = line_feature_with_relation(
        concat_rels(
            t.profile.preprocess_osm_relation(&rel1).unwrap(),
            t.profile.preprocess_osm_relation(&rel2).unwrap(),
        ),
        tags! {"highway" => "trunk", "ref" => "S7"},
    );
    let rendered = t.process(&rendered_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk"},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "name" => "<null>", "ref" => "S7", "ref_length" => 2, "route_1_network" => "e-road", "route_1_ref" => "E 28", "route_2_network" => "e-road", "route_2_ref" => "E 77"},
        ],
        &rendered,
    );
}

#[test]
fn test_motorway_junction() {
    let t = LayerTest::new();
    let other_node1 = Node::new(1, 1.0, 1.0);
    let mut junction_node = Node::new(2, 1.0, 2.0);
    let other_node2 = Node::new(3, 1.0, 3.0);
    let other_node3 = Node::new(4, 2.0, 3.0);

    set_tag(&mut junction_node.tags, "highway", "motorway_junction");
    set_tag(&mut junction_node.tags, "name", "exit 1");
    set_tag(&mut junction_node.tags, "layer", "1");
    set_tag(&mut junction_node.tags, "ref", "12");

    // 2 ways meet at junctionNode (id=2) - use most important class of a highway intersecting it (motorway)
    let mut way1 = Way::new(5);
    set_tag(&mut way1.tags, "highway", "motorway");
    way1.nodes.extend([other_node1.id, junction_node.id, other_node2.id]);
    let mut way2 = Way::new(6);
    set_tag(&mut way2.tags, "highway", "primary");
    way2.nodes.extend([junction_node.id, other_node3.id]);

    t.profile.preprocess_osm_node(&other_node1);
    t.profile.preprocess_osm_node(&junction_node);
    t.profile.preprocess_osm_node(&other_node2);
    t.profile.preprocess_osm_node(&other_node3);

    t.profile.preprocess_osm_way(&way1);
    t.profile.preprocess_osm_way(&way2);

    let features_src = SimpleFeature::create(
        new_point(1.0, 2.0),
        junction_node.tags.clone(),
        Some(OSM_SOURCE),
        None::<&str>,
        junction_node.id,
    );
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation_name", "class" => "motorway", "subclass" => "junction", "name" => "exit 1", "ref" => "12", "ref_length" => 2, "layer" => 1, "_type" => "point", "_minzoom" => 10},
        ],
        &features,
    );
}

#[test]
fn test_interstate_motorway_without_way_info() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "US:I");
    set_tag(&mut rel.tags, "ref", "90");

    let features_src =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel).unwrap(), tags! {"highway" => "motorway"});
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "motorway", "network" => "us-interstate", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "motorway", "ref" => "90", "ref_length" => 2, "network" => "us-interstate", "brunnel" => "<null>", "route_1_network" => "US:I", "route_1_ref" => "90", "_minzoom" => 6},
        ],
        &features,
    );
}

#[test]
fn test_motorway_road_construction() {
    let t = LayerTest::new();
    assert_features(13, &[tags! {"_layer" => "transportation", "class" => "motorway_construction", "oneway" => 1, "_minzoom" => 4}, tags! {"_layer" => "transportation_name", "name" => "D4 II/118 – Milín", "class" => "motorway_construction", "_minzoom" => 6}], &t.process(&line_feature(tags! {"highway" => "construction", "construction" => "motorway", "name" => "D4 II/118 – Milín", "oneway" => "yes"})));
}

#[test]
fn test_primary_road_construction() {
    let t = LayerTest::new();
    assert_features(13, &[tags! {"_layer" => "transportation", "class" => "primary_construction", "brunnel" => "bridge", "layer" => 1, "oneway" => 1, "_minzoom" => 7}, tags! {"_layer" => "transportation_name", "name" => "North Washington Street", "class" => "primary_construction", "brunnel" => "<null>", "_minzoom" => 12}], &t.process(&line_feature(tags! {"highway" => "construction", "construction" => "primary", "bridge" => "yes", "layer" => "1", "name" => "North Washington Street", "oneway" => "yes"})));
}

#[test]
fn test_bridge_construction() {
    let t = LayerTest::new();
    assert_features(
        14,
        &[],
        &t.process(&line_feature(
            tags! {"highway" => "construction", "construction" => "bridge", "man_made" => "bridge", "layer" => "1"},
        )),
    );
    assert_features(
        14,
        &[tags! {"_layer" => "transportation", "class" => "minor_construction", "brunnel" => "bridge", "layer" => 1}],
        &t.process(&closed_way_feature(
            tags! {"highway" => "construction", "construction" => "bridge", "man_made" => "bridge", "layer" => "1"},
        )),
    );
}

#[test]
fn test_ignore_man_made_when_not_bridge_or_pier() {
    let t = LayerTest::new();
    // https://github.com/onthegomap/planetiler/issues/69
    assert_features(14, &[], &t.process(&line_feature(tags! {"man_made" => "storage_tank", "service" => "driveway"})));
    assert_features(14, &[], &t.process(&line_feature(tags! {"man_made" => "courtyard", "service" => "driveway"})));
    assert_features(
        14,
        &[],
        &t.process(&line_feature(
            tags! {"man_made" => "courtyard", "service" => "driveway", "name" => "Named Driveway"},
        )),
    );
}

#[test]
fn test_raceway() {
    let t = LayerTest::new();
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "raceway", "oneway" => 1, "_minzoom" => 12},
            tags! {"_layer" => "transportation_name", "class" => "raceway", "name" => "Climbing Turn", "ref" => "5", "_minzoom" => 12},
        ],
        &t.process(&line_feature(
            tags! {"highway" => "raceway", "oneway" => "yes", "ref" => "5", "name" => "Climbing Turn"},
        )),
    );
}

#[test]
fn test_driveway() {
    let t = LayerTest::new();
    assert_features(
        13,
        &[tags! {"_layer" => "transportation", "class" => "service", "service" => "driveway", "_minzoom" => 14}],
        &t.process(&line_feature(tags! {"highway" => "service", "service" => "driveway"})),
    );
}

#[test]
fn test_mountain_bike_trail() {
    let t = LayerTest::new();
    assert_features(13, &[tags! {"_layer" => "transportation", "class" => "path", "subclass" => "path", "mtb_scale" => "4", "surface" => "unpaved", "bicycle" => "yes", "_minzoom" => 13}, tags! {"_layer" => "transportation_name", "class" => "path", "subclass" => "path", "name" => "Path name", "_minzoom" => 13}], &t.process(&line_feature(tags! {"highway" => "path", "mtb:scale" => "4", "name" => "Path name", "bicycle" => "yes", "surface" => "ground"})));
}

#[test]
fn test_named_track() {
    let t = LayerTest::new();
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "track", "surface" => "unpaved", "horse" => "yes", "_minzoom" => 13},
            tags! {"_layer" => "transportation_name", "class" => "track", "name" => "name", "_minzoom" => 13},
        ],
        &t.process(&line_feature(
            tags! {"highway" => "track", "surface" => "dirt", "horse" => "yes", "name" => "name"},
        )),
    );
}

#[test]
fn test_unnamed_track() {
    let t = LayerTest::new();
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "track", "surface" => "unpaved", "horse" => "yes", "_minzoom" => 14},
        ],
        &t.process(&line_feature(tags! {"highway" => "track", "surface" => "dirt", "horse" => "yes"})),
    );
}

#[test]
fn test_busway() {
    let t = LayerTest::new();
    assert_features(13, &[tags! {"_layer" => "transportation", "class" => "busway", "brunnel" => "tunnel", "_minzoom" => 11}, tags! {"_layer" => "transportation_name", "class" => "busway", "name" => "Silver Line", "_minzoom" => 12}], &t.process(&line_feature(tags! {"access" => "no", "bus" => "yes", "highway" => "busway", "layer" => "-1", "name" => "Silver Line", "trolley_wire" => "yes", "tunnel" => "yes"})));
}

#[test]
fn test_bus_guideway() {
    let t = LayerTest::new();
    assert_features(13, &[tags! {"_layer" => "transportation", "class" => "bus_guideway", "brunnel" => "tunnel", "_minzoom" => 11}, tags! {"_layer" => "transportation_name", "class" => "bus_guideway", "name" => "Silver Line", "_minzoom" => 12}], &t.process(&line_feature(tags! {"access" => "no", "bus" => "yes", "highway" => "bus_guideway", "layer" => "-1", "name" => "Silver Line", "trolley_wire" => "yes", "tunnel" => "yes"})));
}

#[test]
fn test_u_s_and_state_highway() {
    let t = LayerTest::new();
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "primary", "surface" => "paved", "oneway" => "<null>", "ramp" => "<null>", "network" => "us-highway", "_minzoom" => 7},
            tags! {"_layer" => "transportation_name", "class" => "primary", "name" => "Memorial Drive", "name_en" => "Memorial Drive", "ref" => "3", "ref_length" => 1, "network" => "us-highway", "route_1_network" => "US:US", "route_1_ref" => "3", "route_2_network" => "US:MA", "route_2_ref" => "2", "_minzoom" => 12},
        ],
        &t.process(&line_feature_with_relation(
            concat_rels(
                t.profile.preprocess_osm_relation(&rel_us()).unwrap(),
                t.profile.preprocess_osm_relation(&rel_ma()).unwrap(),
            ),
            tags! {"highway" => "primary", "name" => "Memorial Drive", "ref" => "US 3;MA 2", "surface" => "asphalt"},
        )),
    );

    // swap order
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "primary"},
            tags! {"_layer" => "transportation_name", "class" => "primary", "route_1_network" => "US:US", "route_1_ref" => "3", "route_2_network" => "US:MA", "route_2_ref" => "2", "ref" => "3", "network" => "us-highway"},
        ],
        &t.process(&line_feature_with_relation(
            concat_rels(
                t.profile.preprocess_osm_relation(&rel_ma()).unwrap(),
                concat_rels(
                    // ignore duplicates
                    t.profile.preprocess_osm_relation(&rel_us()).unwrap(),
                    t.profile.preprocess_osm_relation(&rel_us()).unwrap(),
                ),
            ),
            tags! {"highway" => "primary", "name" => "Memorial Drive", "ref" => "US 3;MA 2", "surface" => "asphalt"},
        )),
    );
}

#[test]
fn test_us_state_highway() {
    let t = LayerTest::new();
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "primary"},
            tags! {"_layer" => "transportation_name", "class" => "primary", "name" => "Memorial Drive", "name_en" => "Memorial Drive", "ref" => "2", "ref_length" => 1, "network" => "us-state", "_minzoom" => 12},
        ],
        &t.process(&line_feature_with_relation(
            t.profile.preprocess_osm_relation(&rel_ma()).unwrap(),
            tags! {"highway" => "primary", "name" => "Memorial Drive", "ref" => "US 3;MA 2", "surface" => "asphalt"},
        )),
    );
}

#[test]
fn test_compound_ref() {
    let t = LayerTest::new();
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "primary", "network" => "<null>"},
            tags! {"_layer" => "transportation_name", "class" => "primary", "name" => "Memorial Drive", "name_en" => "Memorial Drive", "ref" => "US 3;MA 2", "ref_length" => 9, "network" => "road", "_minzoom" => 12},
        ],
        &t.process(&line_feature(
            tags! {"highway" => "primary", "name" => "Memorial Drive", "ref" => "US 3;MA 2", "surface" => "asphalt"},
        )),
    );
}

#[test]
fn test_trans_canada_highway() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "CA:transcanada:namedRoute");

    let features_src = line_feature_with_relation(
        t.profile.preprocess_osm_relation(&rel).unwrap(),
        tags! {"highway" => "motorway", "oneway" => "yes", "name" => "Autoroute Claude-Béchard", "ref" => "85", "surface" => "asphalt"},
    );
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "motorway", "surface" => "paved", "oneway" => 1, "ramp" => "<null>", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "motorway", "name" => "Autoroute Claude-Béchard", "name_en" => "Autoroute Claude-Béchard", "ref" => "85", "ref_length" => 2, "network" => "ca-transcanada", "_minzoom" => 6},
        ],
        &features,
    );
}

#[test]
fn test_trans_canada_trunk() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "CA:transcanada:namedRoute");

    let features_src =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel).unwrap(), tags! {"highway" => "trunk"});
    let features = t.process(&features_src);

    assert_features(13, &[tags! {"_layer" => "transportation", "class" => "trunk", "_minzoom" => 4}], &features);
}

#[test]
fn test_trans_canada_provincial_ca_qc_a() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "CA:QC:A");

    let features_src =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel).unwrap(), tags! {"highway" => "trunk"});
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "network" => "ca-provincial-arterial", "_minzoom" => 4},
        ],
        &features,
    );
}

#[test]
fn test_trans_canada_provincial_ca_on_primary_ref4xx() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "CA:ON:primary");
    set_tag(&mut rel.tags, "ref", "420");

    let features_src =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel).unwrap(), tags! {"highway" => "trunk"});
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "network" => "ca-provincial-arterial", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "ref" => "420", "network" => "ca-provincial-arterial"},
        ],
        &features,
    );
}

#[test]
fn test_trans_canada_provincial_ca_on_primary_ref_qew() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "CA:ON:primary");
    set_tag(&mut rel.tags, "ref", "QEW");

    let features_src =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel).unwrap(), tags! {"highway" => "trunk"});
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "network" => "ca-provincial-arterial", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "ref" => "QEW", "network" => "ca-provincial-arterial"},
        ],
        &features,
    );
}

#[test]
fn test_trans_canada_provincial_ca_on_primary_ref_other() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "CA:ON:primary");
    set_tag(&mut rel.tags, "ref", "85");

    let features_src =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel).unwrap(), tags! {"highway" => "trunk"});
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "network" => "ca-provincial", "_minzoom" => 6},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "ref" => "85", "network" => "ca-provincial"},
        ],
        &features,
    );
}

#[test]
fn test_trans_canada_provincial_ca_mb_pth_ref75() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "CA:MB:PTH");
    set_tag(&mut rel.tags, "ref", "75");

    let features_src =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel).unwrap(), tags! {"highway" => "trunk"});
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "network" => "ca-provincial-arterial", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "ref" => "75", "network" => "ca-provincial-arterial"},
        ],
        &features,
    );
}

#[test]
fn test_trans_canada_provincial_ca_mb_pth_ref_other() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "CA:MB:PTH");
    set_tag(&mut rel.tags, "ref", "77");

    let features_src =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel).unwrap(), tags! {"highway" => "trunk"});
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "network" => "ca-provincial", "_minzoom" => 6},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "ref" => "77", "network" => "ca-provincial"},
        ],
        &features,
    );
}

#[test]
fn test_trans_canada_provincial_ca_ab_primary_ref3() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "CA:AB:primary");
    set_tag(&mut rel.tags, "ref", "3");

    let features_src =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel).unwrap(), tags! {"highway" => "trunk"});
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "network" => "ca-provincial-arterial", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "ref" => "3", "network" => "ca-provincial-arterial"},
        ],
        &features,
    );
}

#[test]
fn test_trans_canada_provincial_ca_ab_primary_ref_other() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "CA:AB:primary");
    set_tag(&mut rel.tags, "ref", "10");

    let features_src =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel).unwrap(), tags! {"highway" => "trunk"});
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "network" => "ca-provincial", "_minzoom" => 6},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "ref" => "10", "network" => "ca-provincial"},
        ],
        &features,
    );
}

#[test]
fn test_trans_canada_provincial_ca_bc_ref3() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "CA:BC");
    set_tag(&mut rel.tags, "ref", "3");

    let features_src =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel).unwrap(), tags! {"highway" => "trunk"});
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "network" => "ca-provincial-arterial", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "ref" => "3", "network" => "ca-provincial-arterial"},
        ],
        &features,
    );
}

#[test]
fn test_trans_canada_provincial_ca_bc_ref_other() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "CA:BC");
    set_tag(&mut rel.tags, "ref", "10");

    let features_src =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel).unwrap(), tags! {"highway" => "trunk"});
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "network" => "ca-provincial", "_minzoom" => 6},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "ref" => "10", "network" => "ca-provincial"},
        ],
        &features,
    );
}

#[test]
fn test_trans_canada_provincial_ca_other() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "CA:yellowhead");

    let features_src =
        line_feature_with_relation(t.profile.preprocess_osm_relation(&rel).unwrap(), tags! {"highway" => "trunk"});
    let features = t.process(&features_src);

    assert_features(13, &[tags! {"_layer" => "transportation", "class" => "trunk", "_minzoom" => 6}], &features);
    let ca_prov_present = features
        .iter()
        .flat_map(|f| f.get_attrs_at_zoom(13).iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>())
        .any(|(k, v)| k == "network" && matches!(v.as_str(), Some("ca-provincial" | "ca-provincial-arterial")));
    assert!(!ca_prov_present, "ca-provincial present");
}

#[test]
fn test_great_britain_highway() {
    let t = LayerTest::new();
    t.process(&SimpleFeature::create(
        rectangle(0.0, 0.1),
        tags! {"iso_a2" => "GB"},
        Some(NATURAL_EARTH_SOURCE),
        Some("ne_10m_admin_0_countries"),
        0,
    ));

    // in GB
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "motorway", "oneway" => 1, "ramp" => "<null>", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "motorway", "ref" => "M1", "ref_length" => 2, "network" => "gb-motorway", "_minzoom" => 6},
        ],
        &t.process(&SimpleFeature::create(
            new_line_string(&[0.0, 0.0, 1.0, 1.0]),
            tags! {"highway" => "motorway", "oneway" => "yes", "ref" => "M1"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
        )),
    );

    // not in GB
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "motorway", "oneway" => 1, "ramp" => "<null>", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "motorway", "ref" => "M1", "ref_length" => 2, "network" => "road", "_minzoom" => 6},
        ],
        &t.process(&SimpleFeature::create(
            new_line_string(&[1.0, 0.0, 0.0, 1.0]),
            tags! {"highway" => "motorway", "oneway" => "yes", "ref" => "M1"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
        )),
    );
}

#[test]
fn test_great_britain_trunk() {
    let t = LayerTest::new();
    t.process(&SimpleFeature::create(
        rectangle(0.0, 0.1),
        tags! {"iso_a2" => "GB"},
        Some(NATURAL_EARTH_SOURCE),
        Some("ne_10m_admin_0_countries"),
        0,
    ));

    // in GB
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "_minzoom" => 5},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "ref" => "A272", "ref_length" => 4, "network" => "gb-trunk", "_minzoom" => 8},
        ],
        &t.process(&SimpleFeature::create(
            new_line_string(&[0.0, 0.0, 1.0, 1.0]),
            tags! {"highway" => "trunk", "ref" => "A272"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
        )),
    );
}

#[test]
fn test_great_britain_primary() {
    let t = LayerTest::new();
    t.process(&SimpleFeature::create(
        rectangle(0.0, 0.1),
        tags! {"iso_a2" => "GB"},
        Some(NATURAL_EARTH_SOURCE),
        Some("ne_10m_admin_0_countries"),
        0,
    ));

    // in GB
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "primary", "_minzoom" => 7},
            tags! {"_layer" => "transportation_name", "class" => "primary", "ref" => "A598", "ref_length" => 4, "network" => "gb-primary", "_minzoom" => 12},
        ],
        &t.process(&SimpleFeature::create(
            new_line_string(&[0.0, 0.0, 1.0, 1.0]),
            tags! {"highway" => "primary", "ref" => "A598"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
        )),
    );
}

#[test]
fn test_great_britain_secondary() {
    let t = LayerTest::new();
    t.process(&SimpleFeature::create(
        rectangle(0.0, 0.1),
        tags! {"iso_a2" => "GB"},
        Some(NATURAL_EARTH_SOURCE),
        Some("ne_10m_admin_0_countries"),
        0,
    ));

    // in GB
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "secondary", "_minzoom" => 9},
            tags! {"_layer" => "transportation_name", "class" => "secondary", "ref" => "B4558", "ref_length" => 5, "network" => "gb-primary", "_minzoom" => 12},
        ],
        &t.process(&SimpleFeature::create(
            new_line_string(&[0.0, 0.0, 1.0, 1.0]),
            tags! {"highway" => "secondary", "ref" => "B4558"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
        )),
    );
}

#[test]
fn test_great_britain_tertiary() {
    let t = LayerTest::new();
    t.process(&SimpleFeature::create(
        rectangle(0.0, 0.1),
        tags! {"iso_a2" => "GB"},
        Some(NATURAL_EARTH_SOURCE),
        Some("ne_10m_admin_0_countries"),
        0,
    ));

    // in GB
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "tertiary", "_minzoom" => 11},
            tags! {"_layer" => "transportation_name", "class" => "tertiary", "ref" => "B4086", "ref_length" => 5, "network" => "road", "_minzoom" => 12},
        ],
        &t.process(&SimpleFeature::create(
            new_line_string(&[0.0, 0.0, 1.0, 1.0]),
            tags! {"highway" => "tertiary", "ref" => "B4086"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
        )),
    );
}

#[test]
fn test_ireland_highway() {
    let t = LayerTest::new();
    t.process(&SimpleFeature::create(
        rectangle(0.0, 0.1),
        tags! {"iso_a2" => "IE"},
        Some(NATURAL_EARTH_SOURCE),
        Some("ne_10m_admin_0_countries"),
        0,
    ));

    // in IE
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "motorway", "oneway" => 1, "ramp" => "<null>", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "motorway", "ref" => "M18", "ref_length" => 3, "network" => "ie-motorway", "_minzoom" => 6},
        ],
        &t.process(&SimpleFeature::create(
            new_line_string(&[0.0, 0.0, 1.0, 1.0]),
            tags! {"highway" => "motorway", "oneway" => "yes", "ref" => "M18"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
        )),
    );

    // not in IE
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "motorway", "oneway" => 1, "ramp" => "<null>", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "motorway", "ref" => "M18", "ref_length" => 3, "network" => "road", "_minzoom" => 6},
        ],
        &t.process(&SimpleFeature::create(
            new_line_string(&[1.0, 0.0, 0.0, 1.0]),
            tags! {"highway" => "motorway", "oneway" => "yes", "ref" => "M18"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
        )),
    );
}

#[test]
fn test_ireland_trunk() {
    let t = LayerTest::new();
    t.process(&SimpleFeature::create(
        rectangle(0.0, 0.1),
        tags! {"iso_a2" => "IE"},
        Some(NATURAL_EARTH_SOURCE),
        Some("ne_10m_admin_0_countries"),
        0,
    ));

    // in IE
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "ref" => "N8", "ref_length" => 2, "network" => "ie-national", "_minzoom" => 8},
        ],
        &t.process(&SimpleFeature::create(
            new_line_string(&[0.0, 0.0, 1.0, 1.0]),
            tags! {"highway" => "trunk", "ref" => "N8"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
        )),
    );
}

#[test]
fn test_ireland_primary() {
    let t = LayerTest::new();
    t.process(&SimpleFeature::create(
        rectangle(0.0, 0.1),
        tags! {"iso_a2" => "IE"},
        Some(NATURAL_EARTH_SOURCE),
        Some("ne_10m_admin_0_countries"),
        0,
    ));

    // in IE
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "primary", "_minzoom" => 7},
            tags! {"_layer" => "transportation_name", "class" => "primary", "ref" => "N59", "ref_length" => 3, "network" => "ie-national", "_minzoom" => 12},
        ],
        &t.process(&SimpleFeature::create(
            new_line_string(&[0.0, 0.0, 1.0, 1.0]),
            tags! {"highway" => "primary", "ref" => "N59"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
        )),
    );
}

#[test]
fn test_ireland_secondary() {
    let t = LayerTest::new();
    t.process(&SimpleFeature::create(
        rectangle(0.0, 0.1),
        tags! {"iso_a2" => "IE"},
        Some(NATURAL_EARTH_SOURCE),
        Some("ne_10m_admin_0_countries"),
        0,
    ));

    // in IE
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "secondary", "_minzoom" => 9},
            tags! {"_layer" => "transportation_name", "class" => "secondary", "ref" => "R813", "ref_length" => 4, "network" => "ie-regional", "_minzoom" => 12},
        ],
        &t.process(&SimpleFeature::create(
            new_line_string(&[0.0, 0.0, 1.0, 1.0]),
            tags! {"highway" => "secondary", "ref" => "R813"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
        )),
    );
}

#[test]
fn test_merges_disconnected_road_name_features() {
    let t = LayerTest::new();
    t.test_merges_linestrings(&tags! {"class" => "motorway"}, TRANSPORTATION_NAME, 10.0, 14);
}

#[test]
fn test_merges_disconnected_road_features_unless_oneway() {
    let t = LayerTest::new();
    let layer = TRANSPORTATION;
    t.test_merges_linestrings(&tags! {"class" => "motorway", "oneway" => 0}, layer, 10.0, 14);
    t.test_merges_linestrings(&tags! {"class" => "motorway"}, layer, 10.0, 14);
    t.test_does_not_merge_linestrings(&tags! {"class" => "motorway", "oneway" => 1}, layer, 10.0, 14);
    t.test_does_not_merge_linestrings(&tags! {"class" => "motorway", "oneway" => -1}, layer, 10.0, 14);
}

#[test]
fn test_merges_disconnected_road_features_unless_oneway_long() {
    let t = LayerTest::new();
    let layer = TRANSPORTATION;
    t.test_merges_linestrings(&tags! {"class" => "motorway", "oneway" => 0}, layer, 10.0, 14);
    t.test_merges_linestrings(&tags! {"class" => "motorway"}, layer, 10.0, 14);
    t.test_does_not_merge_linestrings(&tags! {"class" => "motorway", "oneway" => 1}, layer, 10.0, 14);
    t.test_does_not_merge_linestrings(&tags! {"class" => "motorway", "oneway" => -1}, layer, 10.0, 14);
}

#[test]
fn test_light_rail() {
    let t = LayerTest::new();
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "transit", "subclass" => "light_rail", "brunnel" => "tunnel", "layer" => -1, "oneway" => "<null>", "ramp" => "<null>", "_minzoom" => 11, "_maxzoom" => 14, "_type" => "line"},
        ],
        &t.process(&line_feature(
            tags! {"railway" => "light_rail", "name" => "Green Line", "tunnel" => "yes", "layer" => "-1"},
        )),
    );
}

#[test]
fn test_subway() {
    let t = LayerTest::new();
    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "transit", "subclass" => "subway", "brunnel" => "tunnel", "layer" => -2, "oneway" => "<null>", "ramp" => "<null>", "_minzoom" => 14, "_maxzoom" => 14, "_type" => "line"},
        ],
        &t.process(&line_feature(
            tags! {"railway" => "subway", "name" => "Red Line", "tunnel" => "yes", "layer" => "-2", "level" => "-2"},
        )),
    );
}

#[test]
fn test_rail() {
    let t = LayerTest::new();
    assert_features(8, &[tags! {"_layer" => "transportation", "class" => "rail", "subclass" => "rail", "brunnel" => "<null>", "layer" => "<null>", "_minzoom" => 8, "_maxzoom" => 14, "_type" => "line"}], &t.process(&line_feature(tags! {"railway" => "rail", "name" => "Boston Subdivision", "usage" => "main", "tunnel" => "yes", "layer" => "-2"})));
    assert_features(
        13,
        &[tags! {"layer" => "<null>", "_minzoom" => 10}],
        &t.process(&line_feature(tags! {"railway" => "rail", "name" => "Boston Subdivision"})),
    );
    assert_features(13, &[], &t.process(&polygon_feature(tags! {"railway" => "rail"})));
    assert_features(
        13,
        &[tags! {"class" => "rail", "subclass" => "rail", "_minzoom" => 14, "service" => "yard"}],
        &t.process(&line_feature(tags! {"railway" => "rail", "name" => "Boston Subdivision", "service" => "yard"})),
    );
}

#[test]
fn test_narrow_gauge() {
    let t = LayerTest::new();
    assert_features(
        10,
        &[
            tags! {"_layer" => "transportation", "class" => "rail", "subclass" => "narrow_gauge", "_minzoom" => 10, "_maxzoom" => 14, "_type" => "line"},
        ],
        &t.process(&line_feature(tags! {"railway" => "narrow_gauge"})),
    );
}

#[test]
fn test_ferry() {
    let t = LayerTest::new();
    assert_features(10, &[tags! {"_layer" => "transportation", "class" => "ferry", "_minzoom" => 4, "_maxzoom" => 14, "_minpixelsize" => 32.0, "_type" => "line"}, tags! {"_layer" => "transportation_name", "class" => "ferry", "name" => "Boston - Provincetown Ferry", "_minzoom" => 12, "_maxzoom" => 14, "_type" => "line"}], &t.process(&line_feature(tags! {"route" => "ferry", "name" => "Boston - Provincetown Ferry", "motor_vehicle" => "no", "foot" => "yes", "bicycle" => "yes"})));
    assert_features(10, &[], &t.process(&polygon_feature(tags! {"route" => "ferry", "name" => "Boston - Provincetown Ferry", "motor_vehicle" => "no", "foot" => "yes", "bicycle" => "yes"})));
}

#[test]
fn test_piers() {
    let t = LayerTest::new();
    // area
    assert_features(
        10,
        &[
            tags! {"_layer" => "transportation", "class" => "pier", "_minzoom" => 13, "_maxzoom" => 14, "_type" => "polygon"},
        ],
        &t.process(&polygon_feature(tags! {"man_made" => "pier"})),
    );
    assert_features(
        10,
        &[
            tags! {"_layer" => "transportation", "class" => "pier", "_minzoom" => 13, "_maxzoom" => 14, "_type" => "line"},
        ],
        &t.process(&line_feature(tags! {"man_made" => "pier"})),
    );
}

#[test]
fn test_pedestrian_area() {
    let t = LayerTest::new();
    let pedestrian_area = tags! {"_layer" => "transportation", "class" => "path", "subclass" => "pedestrian", "_minzoom" => 13, "_maxzoom" => 14, "_type" => "polygon"};
    let circular_path = tags! {"_layer" => "transportation", "class" => "path", "subclass" => "pedestrian", "_minzoom" => 14, "_maxzoom" => 14, "_type" => "line"};
    assert_features(
        14,
        std::slice::from_ref(&pedestrian_area),
        &t.process(&closed_way_feature(tags! {"highway" => "pedestrian", "area" => "yes", "foot" => "yes"})),
    );
    assert_features(
        14,
        std::slice::from_ref(&pedestrian_area),
        &t.process(&polygon_feature(tags! {"highway" => "pedestrian", "foot" => "yes"})),
    );
    assert_features(
        14,
        std::slice::from_ref(&circular_path),
        &t.process(&closed_way_feature(tags! {"highway" => "pedestrian", "foot" => "yes"})),
    );
    assert_features(
        14,
        std::slice::from_ref(&circular_path),
        &t.process(&closed_way_feature(tags! {"highway" => "pedestrian", "foot" => "yes", "area" => "no"})),
    );
    // ignore underground pedestrian areas
    assert_features(
        14,
        &[],
        &t.process(&polygon_feature(
            tags! {"highway" => "pedestrian", "area" => "yes", "foot" => "yes", "layer" => "-1"},
        )),
    );
}

#[test]
fn test_sort_keys() {
    let t = LayerTest::new();
    assert_descending(&[
        get_way_sort_key(&t, tags! {"highway" => "footway", "layer" => "2"}),
        get_way_sort_key(&t, tags! {"highway" => "motorway", "bridge" => "yes"}),
        get_way_sort_key(&t, tags! {"highway" => "footway", "bridge" => "yes"}),
        get_way_sort_key(&t, tags! {"highway" => "motorway"}),
        get_way_sort_key(&t, tags! {"highway" => "trunk"}),
        get_way_sort_key(&t, tags! {"railway" => "rail"}),
        get_way_sort_key(&t, tags! {"highway" => "primary"}),
        get_way_sort_key(&t, tags! {"highway" => "secondary"}),
        get_way_sort_key(&t, tags! {"highway" => "tertiary"}),
        get_way_sort_key(&t, tags! {"highway" => "motorway_link"}),
        get_way_sort_key(&t, tags! {"highway" => "footway"}),
        get_way_sort_key(&t, tags! {"highway" => "motorway", "tunnel" => "yes"}),
        get_way_sort_key(&t, tags! {"highway" => "footway", "tunnel" => "yes"}),
        get_way_sort_key(&t, tags! {"highway" => "motorway", "layer" => "-2"}),
    ]);
}

#[test]
fn test_transportation_name_layer_requires_transportation_layer() {
    let t = LayerTest::with_config(config(&[("only_layers", "transportation_name")]));
    let feature = line_feature(tags! {"highway" => "path", "name" => "test"});
    let collector = t.process(&feature);
    assert_features(
        14,
        &[
            tags! {"_layer" => "transportation_name", "class" => "path", "name" => "test"},
            tags! {"_layer" => "transportation", "class" => "path"},
        ],
        &collector,
    );
}

#[test]
fn test_issue86() {
    let t = LayerTest::new();
    assert_features(
        14,
        &[tags! {"_layer" => "transportation", "class" => "bridge", "_minzoom" => 13, "_type" => "polygon"}],
        &t.process(&closed_way_feature(tags! {"layer" => "1", "man_made" => "bridge", "service" => "driveway"})),
    );
    assert_features(
        14,
        &[tags! {"_layer" => "transportation", "class" => "bridge", "_minzoom" => 13, "_type" => "polygon"}],
        &t.process(&closed_way_feature(
            tags! {"layer" => "1", "man_made" => "bridge", "service" => "driveway", "name" => "name"},
        )),
    );
    assert_features(
        14,
        &[tags! {"_layer" => "transportation", "class" => "pier", "_minzoom" => 13, "_type" => "polygon"}],
        &t.process(&closed_way_feature(tags! {"layer" => "1", "man_made" => "pier", "service" => "driveway"})),
    );
}

#[test]
fn test_grade1_surface_path() {
    let t = LayerTest::new();
    assert_features(
        14,
        &[tags! {"_layer" => "transportation", "class" => "track", "surface" => "paved"}],
        &t.process(&line_feature(tags! {"surface" => "grade1", "highway" => "track"})),
    );
}

#[test]
fn test_grade1_tracktype_path() {
    let t = LayerTest::new();
    assert_features(
        14,
        &[tags! {"_layer" => "transportation", "class" => "track", "surface" => "paved"}],
        &t.process(&line_feature(tags! {"tracktype" => "grade1", "highway" => "track"})),
    );
}

#[test]
fn test_issue58() {
    let t = LayerTest::new();
    // test subject: https://www.openstreetmap.org/way/222564359
    // note: "name:es" used instead of "name:ar" since we've setup only "de" and "es" for unit tests
    let result_src = line_feature(
        tags! {"name" => "איילון דרום", "name:es" => "أيالون جنوب", "name:en" => "Ayalon South", "highway" => "motorway"},
    );
    let result = t.process(&result_src);
    assert_features(
        4,
        &[
            tags! {"_layer" => "transportation", "_type" => "line", "class" => "motorway"},
            tags! {"_layer" => "transportation_name", "_type" => "line", "class" => "motorway", "name" => "איילון דרום", "name_int" => "Ayalon South", "name:latin" => "Ayalon South", "name:es" => "أيالون جنوب", "name:en" => "Ayalon South"},
        ],
        &result,
    );
}

#[test]
fn test_a_road() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "AsianHighway");
    set_tag(&mut rel.tags, "ref", "AH11");

    let features_src = line_feature_with_relation(
        t.profile.preprocess_osm_relation(&rel).unwrap(),
        tags! {"highway" => "trunk", "name" => "National Highway 7", "ref" => "7"},
    );
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "trunk", "_minzoom" => 6},
            tags! {"_layer" => "transportation_name", "class" => "trunk", "ref" => "7", "route_1_ref" => "AH11"},
        ],
        &features,
    );
}

#[test]
fn test_e_road() {
    let t = LayerTest::new();
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "e-road");
    set_tag(&mut rel.tags, "ref", "E 77");

    let features_src = line_feature_with_relation(
        t.profile.preprocess_osm_relation(&rel).unwrap(),
        tags! {"highway" => "motorway", "ref" => "S7"},
    );
    let features = t.process(&features_src);

    assert_features(
        13,
        &[
            tags! {"_layer" => "transportation", "class" => "motorway", "_minzoom" => 4},
            tags! {"_layer" => "transportation_name", "class" => "motorway", "ref" => "S7", "route_1_ref" => "E 77"},
        ],
        &features,
    );
}

#[test]
fn test_network_qualified_trunk_gets_z5_but_not_motorway() {
    let t = LayerTest::new();
    // Test that network-qualified trunks get z5 (or z4 if they qualify for z4)
    // Network qualification should override length check
    let mut rel = Relation::new(1);
    set_tag(&mut rel.tags, "type", "route");
    set_tag(&mut rel.tags, "route", "road");
    set_tag(&mut rel.tags, "network", "US:I");
    set_tag(&mut rel.tags, "ref", "95");

    let features_src = line_feature_with_relation_and_length(
        400.0,
        t.profile.preprocess_osm_relation(&rel).unwrap(),
        tags! {"highway" => "trunk", "ref" => "95"},
    );
    let features = t.process(&features_src);

    // US:I network qualifies for z4, not z5
    // Network-qualified trunks create both transportation and transportation_name features
    assert_features(
        5,
        &[
            tags! {
                "_layer" => "transportation",
                "class" => "trunk", // don't upgrade to motorway
                "network" => "us-interstate",
                "_minzoom" => 4, // US:I network qualifies for z4
            },
            tags! {"_layer" => "transportation_name", "class" => "trunk", "network" => "us-interstate"},
        ],
        &features,
    );
}

#[test]
fn test_aerialway() {
    let t = LayerTest::new();
    for aerialway in ["gondola", "chair_lift", "j-bar", "mixed_lift"] {
        assert_features(
            12,
            &[
                tags! {"_layer" => "transportation", "class" => "aerialway", "subclass" => aerialway, "_minzoom" => 12, "_maxzoom" => 14, "_type" => "line"},
                tags! {"_layer" => "transportation_name", "class" => "aerialway", "subclass" => aerialway, "name" => "Summit Gondola", "_minzoom" => 12, "_maxzoom" => 14, "_type" => "line"},
            ],
            &t.process(&line_feature(tags! {"aerialway" => aerialway, "name" => "Summit Gondola"})),
        );
        assert_features(
            10,
            &[],
            &t.process(&polygon_feature(tags! {"aerialway" => aerialway, "name" => "Summit Gondola"})),
        );
    }
}

#[test]
fn test_short_trunk_gets_z5() {
    let t = LayerTest::new();
    for (length, highway, expected_min_zoom, class_zoom, expected_class) in [
        (499, "trunk", 5, 5, "motorway"),
        (499, "trunk", 5, 6, "trunk"),
        (501, "trunk", 6, 5, "trunk"),
        (501, "trunk", 6, 6, "trunk"),
        (499, "trunk_link", 9, 9, "trunk"), // Links have minzoom=9 regardless of length
    ] {
        // Test that a short trunk segment (< 500m) gets minzoom=5
        let feature = line_feature_with_length(length as f64, tags! {"highway" => highway});
        assert_features(
            class_zoom,
            &[tags! {"_layer" => "transportation", "class" => expected_class, "_minzoom" => expected_min_zoom}],
            &t.process(&feature),
        );

        let feature =
            line_feature_with_length(length as f64, tags! {"highway" => "construction", "construction" => highway});
        assert_features(
            class_zoom,
            &[
                tags! {"_layer" => "transportation", "class" => format!("{expected_class}_construction"), "_minzoom" => expected_min_zoom},
            ],
            &t.process(&feature),
        );
    }
}
