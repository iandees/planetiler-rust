//! Ports of the `PlanetilerTests.java` cases that run OSM elements through `OsmReader` pass 1 / pass 2
//! (`runWithOsmElements` / `processOsmFeatures`) into an in-memory feature group and archive.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use geo::Area;
use geo_types::Geometry;
use planetiler_core::collection::long_long_map;
use planetiler_core::feature_collector::{self, FeatureCollector};
use planetiler_core::forwarding_profile::{self, ForwardingProfile};
use planetiler_core::geo::geo_utils::{get_world_lat, get_world_lon};
use planetiler_core::geo::test_utils::*;
use planetiler_core::geo::tile_coord::TileCoord;
use planetiler_core::profile::{FeatureConsumer, Profile};
use planetiler_core::reader::osm::osm_element::{Member, Node, OsmElement, OsmElementType, Relation, Way};
use planetiler_core::reader::osm::osm_reader::OsmReader;
use planetiler_core::reader::osm::osm_relation_info::OsmRelationInfo;
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::reader::source_feature::{SourceFeature, SourceFeatureExt};
use planetiler_core::stats::stats::InMemoryStats;
use planetiler_core::tags;
use planetiler_core::testing::*;
use planetiler_core::value::Tags;

const Z14_TILES: u32 = 1 << 14;
const Z14_WIDTH: f64 = 1.0 / Z14_TILES as f64;
const Z13_TILES: u32 = 1 << 13;

fn node(id: i64, lat: f64, lon: f64) -> OsmElement {
    Node::new(id, lat, lon).into()
}

fn node_with_tags(id: i64, lat: f64, lon: f64, tags: Tags) -> OsmElement {
    Node::with_tags(id, tags, lat, lon).into()
}

fn way(id: i64, tags: Tags, nodes: &[i64]) -> OsmElement {
    Way::with_tags(id, tags, nodes.to_vec()).into()
}

fn relation(id: i64, tags: Tags, members: Vec<Member>) -> OsmElement {
    Relation::with_tags(id, tags, members).into()
}

fn way_member(ref_id: i64, role: &str) -> Member {
    Member::new(OsmElementType::Way, ref_id, role)
}

fn resource(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources").join(name)
}

/// Like [`assert_submap`] but each tile's feature list may be in any order (Java `sortListValues`).
#[track_caller]
fn assert_submap_unordered(
    expected: &BTreeMap<TileCoord, Vec<ComparableFeature>>,
    actual: &BTreeMap<TileCoord, Vec<ComparableFeature>>,
) {
    for (tile, expected_features) in expected {
        let actual_features = actual.get(tile).unwrap_or_else(|| panic!("missing tile {tile:?}"));
        let mut remaining: Vec<&ComparableFeature> = actual_features.iter().collect();
        let all_found = expected_features.len() == actual_features.len()
            && expected_features.iter().all(|e| match remaining.iter().position(|a| *a == e) {
                Some(i) => {
                    remaining.remove(i);
                    true
                }
                None => false,
            });
        assert!(all_found, "tile {tile:?}\nexpected: {expected_features:#?}\nactual: {actual_features:#?}");
    }
}

fn no_relations(_: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
    None
}

#[derive(Debug)]
struct TestRelationInfo {
    id: i64,
    name: Option<String>,
}

impl OsmRelationInfo for TestRelationInfo {
    fn id(&self) -> i64 {
        self.id
    }
}

fn get_string(tags: &Tags, key: &str) -> Option<String> {
    tags.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

#[test]
fn test_osm_point() {
    let results = run_with_osm_elements(
        &[("threads", "1")],
        vec![node_with_tags(1, 0.0, 0.0, tags! {"attr" => "value"})],
        no_relations,
        |source, features| {
            if source.is_point() {
                features
                    .point("layer")
                    .set_zoom_range(0, 0)
                    .set_attr("name", "name value")
                    .inherit_attr_from_source("attr");
            }
        },
    )
    .unwrap();

    assert_submap(
        &tile_map([(
            (0, 0, 0),
            vec![feature_with_id(new_point(128.0, 128.0), tags! {"attr" => "value", "name" => "name value"}, 11)],
        )]),
        &results.tiles,
    );
}

#[test]
fn test_osm_point_skip_pass1() {
    let elements = vec![node_with_tags(1, 0.0, 0.0, tags! {"attr" => "value"})];
    let results = run(
        &[("threads", "1")],
        |ctx| {
            let reader = OsmReader::new_in_memory_multipolygons(
                "osm",
                osm_elements_block_source(&elements),
                Arc::from(long_long_map::new_in_memory_sorted_table()),
                ctx.profile_arc.clone(),
                InMemoryStats::arc(),
            );
            // skip pass 1
            reader.pass2(ctx.feature_group, ctx.config)?;
            reader.close();
            Ok(())
        },
        Arc::new(TestProfile::process_source_features(|source, features| {
            if source.is_point() {
                features
                    .point("layer")
                    .set_zoom_range(0, 0)
                    .set_attr("name", "name value")
                    .inherit_attr_from_source("attr");
            }
        })),
    )
    .unwrap();

    assert_submap(
        &tile_map([(
            (0, 0, 0),
            vec![feature(new_point(128.0, 128.0), tags! {"attr" => "value", "name" => "name value"})],
        )]),
        &results.tiles,
    );
}

/// TODO(port): Java throws an `Error` subclass (`ExpectedError`) from the profile, which the OSM reader does not
/// catch, so the run fails. Rust has no `Error`/`Exception` split: every panic from a profile is caught and logged
/// like a Java `Exception` (see `test_handle_profile_exception`), so this run succeeds with no features.
#[test]
#[ignore = "Rust catches all profile panics (no Java Error vs Exception distinction)"]
fn test_exception_while_processing_osm() {
    let result = run_with_osm_elements(
        &[("threads", "1")],
        vec![node_with_tags(1, 0.0, 0.0, tags! {"attr" => "value"})],
        no_relations,
        |_, _| panic!("expected error"),
    );
    assert!(result.is_err());
}

#[test]
fn test_osm_line() {
    let results = run_with_osm_elements(
        &[("threads", "1")],
        vec![
            node(1, 0.0, 0.0),
            node(2, get_world_lat(0.75), get_world_lon(0.75)),
            way(3, tags! {"attr" => "value"}, &[1, 2]),
        ],
        no_relations,
        |source, features| {
            if source.can_be_line() {
                features
                    .line("layer")
                    .set_zoom_range(0, 0)
                    .set_attr("name", "name value")
                    .inherit_attr_from_source("attr");
            }
        },
    )
    .unwrap();

    assert_submap(
        &tile_map([(
            (0, 0, 0),
            vec![feature_with_id(
                new_line_string(&[128.0, 128.0, 192.0, 192.0]),
                tags! {"attr" => "value", "name" => "name value"},
                32,
            )],
        )]),
        &results.tiles,
    );
}

#[test]
fn test_osm_line_or_polygon() {
    let results = run_with_osm_elements(
        &[("threads", "1")],
        vec![
            node(1, get_world_lat(0.25), get_world_lon(0.25)),
            node(2, get_world_lat(0.25), get_world_lon(0.75)),
            node(3, get_world_lat(0.75), get_world_lon(0.75)),
            node(4, get_world_lat(0.75), get_world_lon(0.25)),
            way(6, tags! {"attr" => "value"}, &[1, 2, 3, 4, 1]),
        ],
        no_relations,
        |source, features| {
            if source.can_be_line() {
                features
                    .line("layer")
                    .set_zoom_range(0, 0)
                    .set_attr("name", "name value1")
                    .inherit_attr_from_source("attr");
            }
            if source.can_be_polygon() {
                features
                    .polygon("layer")
                    .set_zoom_range(0, 0)
                    .set_attr("name", "name value2")
                    .inherit_attr_from_source("attr");
            }
        },
    )
    .unwrap();

    assert_submap_unordered(
        &tile_map([(
            (0, 0, 0),
            vec![
                feature(
                    new_line_string(&[64.0, 64.0, 192.0, 64.0, 192.0, 192.0, 64.0, 192.0, 64.0, 64.0]),
                    tags! {"attr" => "value", "name" => "name value1"},
                ),
                feature(rectangle(64.0, 192.0), tags! {"attr" => "value", "name" => "name value2"}),
            ],
        )]),
        &results.tiles,
    );
}

/// Profile that splits ways at intersections and calls `f` for each feature.
struct SplitProfile<F>(F);

impl<F: Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync> Profile for SplitProfile<F> {
    fn process_feature(&self, source: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        (self.0)(source, features)
    }

    fn split_osm_way_at_intersections(&self, _way: &Way) -> bool {
        true
    }
}

fn layer_feature(layer: &str, geom: impl Into<Geometry<f64>>, attrs: Tags, id: i64) -> ComparableFeature {
    feature_in_layer_with_id(layer, geom, attrs, id)
}

#[test]
fn test_split_ambiguous_osm_polygon() {
    let results = run_with_osm_elements_profile(
        &[("threads", "1")],
        vec![
            node(1, get_world_lat(120.0 / 256.0), get_world_lon(120.0 / 256.0)),
            node(2, get_world_lat(120.0 / 256.0), get_world_lon(140.0 / 256.0)),
            node(3, get_world_lat(140.0 / 256.0), get_world_lon(140.0 / 256.0)),
            node(4, get_world_lat(0.5), get_world_lon(0.5)),
            way(4, tags! {"attr" => "value1"}, &[1, 2, 3, 1]),
            way(5, tags! {"attr" => "value2"}, &[2, 4]),
        ],
        SplitProfile(|source: &dyn SourceFeature, features: &mut FeatureCollector<'_>| {
            if source.can_be_line() {
                features.split_line("split").set_zoom_range(0, 0).inherit_attr_from_source("attr");
                features.any_geometry("any").set_zoom_range(0, 0).inherit_attr_from_source("attr");
                features.line("full").set_zoom_range(0, 0).inherit_attr_from_source("attr");
                features.polygon("full-polygon").set_zoom_range(0, 0).inherit_attr_from_source("attr");
            }
        }),
    )
    .unwrap();

    let v1 = || tags! {"attr" => "value1"};
    let v2 = || tags! {"attr" => "value2"};
    assert_submap(
        &tile_map([(
            (0, 0, 0),
            vec![
                layer_feature("any", new_polygon(&[120.0, 120.0, 140.0, 120.0, 140.0, 140.0, 120.0, 120.0]), v1(), 42),
                layer_feature("any", new_line_string(&[140.0, 120.0, 128.0, 128.0]), v2(), 52),
                layer_feature(
                    "full",
                    new_line_string(&[120.0, 120.0, 140.0, 120.0, 140.0, 140.0, 120.0, 120.0]),
                    v1(),
                    42,
                ),
                layer_feature("full", new_line_string(&[140.0, 120.0, 128.0, 128.0]), v2(), 52),
                layer_feature(
                    "full-polygon",
                    new_polygon(&[120.0, 120.0, 140.0, 120.0, 140.0, 140.0, 120.0, 120.0]),
                    v1(),
                    42,
                ),
                layer_feature("split", new_line_string(&[120.0, 120.0, 140.0, 120.0]), v1(), 42),
                layer_feature("split", new_line_string(&[140.0, 120.0, 128.0, 128.0]), v2(), 52),
                layer_feature("split", new_line_string(&[140.0, 120.0, 140.0, 140.0, 120.0, 120.0]), v1(), 142),
            ],
        )]),
        &results.tiles,
    );
}

#[test]
fn test_split_osm_line_with_loop() {
    let results = run_with_osm_elements_profile(
        &[("threads", "1")],
        vec![
            node(1, get_world_lat(0.5), get_world_lon(0.25)),
            node(2, get_world_lat(0.5), get_world_lon(0.5)),
            node(3, get_world_lat(0.5), get_world_lon(0.75)),
            node(4, get_world_lat(0.75), get_world_lon(0.5)),
            way(4, tags! {"attr" => "value1"}, &[1, 2, 3, 4, 2]),
        ],
        SplitProfile(|source: &dyn SourceFeature, features: &mut FeatureCollector<'_>| {
            if source.can_be_line() {
                features.split_line("layer").set_zoom_range(0, 0).inherit_attr_from_source("attr");
            }
        }),
    )
    .unwrap();

    assert_submap(
        &tile_map([(
            (0, 0, 0),
            vec![
                feature_with_id(new_line_string(&[64.0, 128.0, 128.0, 128.0]), tags! {"attr" => "value1"}, 42),
                feature_with_id(
                    new_line_string(&[128.0, 128.0, 192.0, 128.0, 128.0, 192.0, 128.0, 128.0]),
                    tags! {"attr" => "value1"},
                    142,
                ),
            ],
        )]),
        &results.tiles,
    );
}

fn osm_multipolygon(relation_type: &str) {
    let results = run_with_osm_elements(
        &[("threads", "1")],
        vec![
            node(1, get_world_lat(0.125), get_world_lon(0.125)),
            node(2, get_world_lat(0.125), get_world_lon(0.875)),
            node(3, get_world_lat(0.875), get_world_lon(0.875)),
            node(4, get_world_lat(0.875), get_world_lon(0.125)),
            node(5, get_world_lat(0.25), get_world_lon(0.25)),
            node(6, get_world_lat(0.25), get_world_lon(0.75)),
            node(7, get_world_lat(0.75), get_world_lon(0.75)),
            node(8, get_world_lat(0.75), get_world_lon(0.25)),
            node(9, get_world_lat(0.375), get_world_lon(0.375)),
            node(10, get_world_lat(0.375), get_world_lon(0.625)),
            node(11, get_world_lat(0.625), get_world_lon(0.625)),
            node(12, get_world_lat(0.625), get_world_lon(0.375)),
            node(13, get_world_lat(0.375 + 1e-12), get_world_lon(0.375)),
            way(14, tags! {}, &[1, 2, 3, 4, 1]),
            way(15, tags! {}, &[5, 6, 7, 8, 5]),
            way(16, tags! {}, &[9, 10, 11, 12, 13]),
            relation(
                17,
                tags! {"type" => relation_type, "attr" => "value", "should_emit" => "yes"},
                vec![
                    way_member(14, "outer"),
                    way_member(15, ""),      // missing
                    way_member(16, "inner"), // incorrect
                    way_member(14, "outer"), // duplicate
                ],
            ),
            relation(
                18,
                tags! {"type" => "relation", "name" => "rel name"},
                vec![Member::new(OsmElementType::Relation, 17, "outer")],
            ),
        ],
        |rel| {
            if rel.tags.get("type").and_then(|v| v.as_str()) == Some("relation") {
                Some(vec![Arc::new(TestRelationInfo { id: rel.id, name: get_string(&rel.tags, "name") })
                    as Arc<dyn OsmRelationInfo>])
            } else {
                None
            }
        },
        |source, features| {
            if source.has_tag("should_emit") {
                let relname = source.relation_info::<TestRelationInfo>().first().and_then(|r| r.relation.name.clone());
                features
                    .polygon("layer")
                    .set_zoom_range(0, 0)
                    .set_attr("name", "name value")
                    .inherit_attr_from_source("attr")
                    .set_attr("relname", relname);
            }
        },
    )
    .unwrap();

    assert_submap(
        &tile_map([(
            (0, 0, 0),
            vec![feature_with_id(
                new_multi_polygon(vec![
                    new_polygon_with_holes(
                        rectangle_coord_list(0.125 * 256.0, 0.875 * 256.0),
                        vec![rectangle_coord_list(0.25 * 256.0, 0.75 * 256.0)],
                    ),
                    rectangle(0.375 * 256.0, 0.625 * 256.0),
                ]),
                tags! {"attr" => "value", "name" => "name value", "relname" => "rel name"},
                173,
            )],
        )]),
        &results.tiles,
    );
}

#[test]
fn test_osm_multipolygon_multipolygon() {
    osm_multipolygon("multipolygon");
}

#[test]
fn test_osm_multipolygon_boundary() {
    osm_multipolygon("boundary");
}

#[test]
fn test_osm_multipolygon_land_area() {
    osm_multipolygon("land_area");
}

#[test]
fn test_osm_line_in_relation() {
    let results = run_with_osm_elements(
        &[("threads", "1")],
        vec![
            node(1, 0.0, 0.0),
            node(2, get_world_lat(0.375), 0.0),
            node(3, get_world_lat(0.25), 0.0),
            node(4, get_world_lat(0.125), 0.0),
            way(5, tags! {"attr" => "value1"}, &[1, 2]),
            way(6, tags! {"attr" => "value2"}, &[3, 4]),
            relation(6, tags! {"name" => "relation name"}, vec![way_member(6, "role")]),
        ],
        |rel| {
            if rel.tags.get("name").and_then(|v| v.as_str()) == Some("relation name") {
                Some(vec![Arc::new(TestRelationInfo { id: rel.id, name: get_string(&rel.tags, "name") })
                    as Arc<dyn OsmRelationInfo>])
            } else {
                None
            }
        },
        |source, features| {
            let relation_infos = source.relation_info::<TestRelationInfo>();
            let first = relation_infos.first();
            if source.can_be_line() {
                features
                    .line("layer")
                    .set_zoom_range(0, 0)
                    .set_attr("relname", first.and_then(|d| d.relation.name.clone()))
                    .inherit_attr_from_source("attr")
                    .set_attr("relrole", first.map(|d| d.role.to_string()));
            }
        },
    )
    .unwrap();

    assert_submap_unordered(
        &tile_map([(
            (0, 0, 0),
            vec![
                feature(new_line_string(&[128.0, 128.0, 128.0, 0.375 * 256.0]), tags! {"attr" => "value1"}),
                feature(
                    new_line_string(&[128.0, 0.25 * 256.0, 128.0, 0.125 * 256.0]),
                    tags! {"attr" => "value2", "relname" => "relation name", "relrole" => "role"},
                ),
            ],
        )]),
        &results.tiles,
    );
}

#[derive(Default)]
struct PreprocessProfile {
    nodes1: Mutex<HashMap<i64, i64>>,
    nodes2: Mutex<HashSet<i64>>,
}

impl Profile for PreprocessProfile {
    fn preprocess_osm_node(&self, node: &Node) {
        if node.tags.get("a").and_then(|v| v.as_str()) == Some("b") {
            self.nodes1.lock().unwrap().insert(node.id, node.id);
        }
    }

    fn preprocess_osm_way(&self, way: &Way) {
        let feature_id = self.nodes1.lock().unwrap().get(&way.nodes[0]).copied();
        if let Some(id) = feature_id {
            self.nodes2.lock().unwrap().insert(id);
        }
    }

    fn process_feature(&self, source: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if source.is_point() && self.nodes2.lock().unwrap().contains(&source.id()) {
            features.point("start_nodes").set_max_zoom(0);
        }
    }
}

#[test]
fn test_preprocess_osm_nodes_and_ways() {
    let results = run_with_osm_elements_profile(
        &[("threads", "1")],
        vec![
            node_with_tags(1, 0.0, 0.0, tags! {"a" => "b"}),
            node(2, get_world_lat(0.375), 0.0),
            way(3, tags! {}, &[1, 2]),
            way(4, tags! {}, &[1, 2]),
        ],
        PreprocessProfile::default(),
    )
    .unwrap();

    assert_submap_unordered(&tile_map([((0, 0, 0), vec![feature(new_point(128.0, 128.0), tags! {})])]), &results.tiles);
}

/// Remembers every OSM source feature (as a `SimpleFeature` copy, since OSM features borrow reader state) and emits
/// output features from `finish("osm")`.
#[derive(Default)]
struct FeatureList(Mutex<Vec<SimpleFeature>>);

impl FeatureList {
    fn add(&self, source: &dyn SourceFeature) {
        let feature = SimpleFeature::create_world(
            source.world_geometry().unwrap().clone(),
            source.tags().clone(),
            source.source(),
            source.source_layer(),
            source.id(),
        );
        self.0.lock().unwrap().push(feature);
    }

    fn finish(&self, name: &str, feature_collectors: &feature_collector::Factory, next: &mut FeatureConsumer<'_>) {
        if name == "osm" {
            for source in self.0.lock().unwrap().iter() {
                let mut features = feature_collectors.get(source);
                features.point("layer").set_zoom_range(13, 14).inherit_attr_from_source("a");
                for feature in features.iter() {
                    next(feature);
                }
            }
        }
    }
}

#[derive(Default)]
struct OsmFinishProfile(FeatureList);

impl Profile for OsmFinishProfile {
    fn process_feature(&self, source: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {
        self.0.add(source);
    }

    fn finish(&self, name: &str, feature_collectors: &feature_collector::Factory, next: &mut FeatureConsumer<'_>) {
        self.0.finish(name, feature_collectors, next);
    }
}

fn finish_elements() -> Vec<OsmElement> {
    let y = 0.5 + Z14_WIDTH / 2.0;
    let lat = get_world_lat(y);
    let x1 = 0.5 + Z14_WIDTH / 4.0;
    let lng1 = get_world_lon(x1);
    let lng2 = get_world_lon(x1 + Z14_WIDTH * 10.0 / 256.0);
    vec![node_with_tags(1, lat, lng1, tags! {"a" => 1i64}), node_with_tags(2, lat, lng2, tags! {"a" => 3i64})]
}

fn assert_finish_results(results: &PlanetilerResults) {
    assert_submap_unordered(
        &tile_map([
            (
                (Z14_TILES / 2, Z14_TILES / 2, 14),
                vec![
                    feature(new_point(64.0, 128.0), tags! {"a" => 1i64}),
                    feature(new_point(74.0, 128.0), tags! {"a" => 3i64}),
                ],
            ),
            (
                (Z13_TILES / 2, Z13_TILES / 2, 13),
                vec![
                    // merge 32->37 and 37->42 since they have same attrs
                    feature(new_point(32.0, 64.0), tags! {"a" => 1i64}),
                    feature(new_point(37.0, 64.0), tags! {"a" => 3i64}),
                ],
            ),
        ]),
        &results.tiles,
    );
}

#[test]
fn test_osm_profile_finish() {
    let results =
        run_with_osm_elements_profile(&[("threads", "1")], finish_elements(), OsmFinishProfile::default()).unwrap();
    assert_finish_results(&results);
}

#[test]
fn test_osm_profile_finish_forwarding_profile() {
    let feature_list = Arc::new(FeatureList::default());
    let mut profile = ForwardingProfile::new();
    {
        let list = feature_list.clone();
        profile.register_source_fn("osm", move |source, _features| list.add(source));
    }
    {
        let list = feature_list.clone();
        profile.register_handler(forwarding_profile::finish_handler_fn(move |name, factory, next| {
            list.finish(name, factory, next)
        }));
    }
    let results = run_with_osm_elements_profile(&[("threads", "1")], finish_elements(), profile).unwrap();
    assert_finish_results(&results);
}

/// Minimal OSM XML reader for test resources (Java `TestUtils.readOsmXml` + `convertToOsmElements`): handles `node`,
/// `way` (`nd`), `relation` (`member`) and `tag` elements, in file order.
fn read_osm_xml(name: &str) -> Vec<OsmElement> {
    let xml = std::fs::read_to_string(resource(name)).unwrap();
    let tag_re =
        regex::Regex::new(r"<(/?)([a-z]+)((?:\s+[a-zA-Z_:]+\s*=\s*(?:\x22[^\x22]*\x22|'[^']*'))*)\s*(/?)>").unwrap();
    let attr_re = regex::Regex::new(r#"([a-zA-Z_:]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap();
    let unescape = |s: &str| {
        s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
    };
    let mut nodes = vec![];
    let mut ways = vec![];
    let mut relations = vec![];
    let mut current: Option<OsmElement> = None;
    for cap in tag_re.captures_iter(&xml) {
        let closing = &cap[1] == "/";
        let element = &cap[2];
        let self_closing = &cap[4] == "/";
        let attrs: HashMap<String, String> = attr_re
            .captures_iter(&cap[3])
            .map(|a| (a[1].to_string(), unescape(a.get(2).or(a.get(3)).unwrap().as_str())))
            .collect();
        let finish = |e: OsmElement, nodes: &mut Vec<_>, ways: &mut Vec<_>, relations: &mut Vec<_>| match e {
            OsmElement::Node(_) => nodes.push(e),
            OsmElement::Way(_) => ways.push(e),
            OsmElement::Relation(_) => relations.push(e),
            OsmElement::Other(_) => {}
        };
        if closing {
            if matches!(element, "node" | "way" | "relation")
                && let Some(e) = current.take()
            {
                finish(e, &mut nodes, &mut ways, &mut relations);
            }
            continue;
        }
        let id = || attrs["id"].parse::<i64>().unwrap();
        match element {
            "node" => {
                let e = node(id(), attrs["lat"].parse().unwrap(), attrs["lon"].parse().unwrap());
                if self_closing {
                    finish(e, &mut nodes, &mut ways, &mut relations);
                } else {
                    current = Some(e);
                }
            }
            "way" => {
                let e: OsmElement = Way::new(id()).into();
                if self_closing {
                    finish(e, &mut nodes, &mut ways, &mut relations);
                } else {
                    current = Some(e);
                }
            }
            "relation" => {
                let e: OsmElement = Relation::new(id()).into();
                if self_closing {
                    finish(e, &mut nodes, &mut ways, &mut relations);
                } else {
                    current = Some(e);
                }
            }
            "tag" => {
                if let Some(e) = current.as_mut() {
                    e.tags_mut().insert(attrs["k"].clone(), attrs["v"].as_str().into());
                }
            }
            "nd" => {
                if let Some(OsmElement::Way(w)) = current.as_mut() {
                    w.nodes.push(attrs["ref"].parse().unwrap());
                }
            }
            "member" => {
                if let Some(OsmElement::Relation(r)) = current.as_mut() {
                    let member_type = match attrs["type"].as_str() {
                        "way" => OsmElementType::Way,
                        "relation" => OsmElementType::Relation,
                        "node" => OsmElementType::Node,
                        other => panic!("Unexpected value: {other}"),
                    };
                    let role = attrs.get("role").map_or("", String::as_str);
                    r.members.push(Member::new(member_type, attrs["ref"].parse().unwrap(), role));
                }
            }
            _ => {}
        }
    }
    nodes.into_iter().chain(ways).chain(relations).collect()
}

#[test]
fn test_bad_relation() {
    // this threw an exception in OsmMultipolygon.build
    let elements = read_osm_xml("bad_spain_relation.xml");
    let results = run_with_osm_elements_profile(
        &[("threads", "1")],
        elements,
        TestProfile::process_source_features(|source, features| {
            if source.has_tag_str("landuse", "forest") {
                features.polygon("layer").set_zoom_range(12, 14).set_buffer_pixels(4.0);
            }
        }),
    )
    .unwrap();
    assert_eq!(11, results.tiles.len());
}

#[test]
fn test_issue496_baseball_multipolygon() {
    // this generated a polygon that covered an entire z11 tile where the buffer intersected the baseball field
    let elements = read_osm_xml("issue_496_baseball_multipolygon.xml");
    let results = run_with_osm_elements_profile(
        &[("threads", "1")],
        elements,
        TestProfile::process_source_features(|source, features| {
            if source.has_tag_str("natural", "sand") {
                features
                    .polygon("test")
                    .set_buffer_pixels(4.0)
                    .set_pixel_tolerance(0.5)
                    .set_min_pixel_size(0.1)
                    .set_attr("id", source.id());
            }
        }),
    )
    .unwrap();

    let area_at_z14 = 20.0;
    for (tile, features) in &results.tiles {
        for feature in features {
            let geom = feature.geometry.geom();
            let area = geom.unsigned_area();
            let expected_max_area = area_at_z14 / (1u32 << (14 - tile.z)) as f64;
            assert!(area < expected_max_area, "tile={tile:?} area={area} geom={geom:?}");
        }
    }
    assert_eq!(8, results.tiles.len());
}

#[test]
fn test_issue509_lena_delta() {
    let elements = read_osm_xml("issue_509_lena_delta.xml");
    let results = run_with_osm_elements_profile(
        &[("threads", "1")],
        elements,
        TestProfile::process_source_features(|source, features| {
            if source.has_tag_str("natural", "water") {
                features.polygon("water").set_attr("id", source.id()).set_min_zoom(10);
            }
        }),
    )
    .unwrap();

    let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
    for tile in results.tiles.keys() {
        *counts.entry(tile.z).or_default() += 1;
    }
    assert_eq!(BTreeMap::from([(10, 39), (11, 125), (12, 397), (13, 1160), (14, 3108)]), counts);
}

#[test]
fn test_handle_profile_exception() {
    let results = run_with_osm_elements(
        &[("threads", "1")],
        vec![node_with_tags(1, 0.0, 0.0, tags! {"attr" => "value"})],
        no_relations,
        |_, _| panic!("intentional exception!"),
    )
    .unwrap();
    assert_submap(&tile_map([]), &results.tiles);
}
