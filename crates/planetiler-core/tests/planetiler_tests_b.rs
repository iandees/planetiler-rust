//! Second batch of `PlanetilerTests.java` ports (roughly lines 1184-3410) that run in-memory with `SimpleFeature`
//! inputs through the [`planetiler_core::testing`] harness.
//!
//! TODO(port): tests in this range that need `OsmReader` (`runWithOsmElements` / `processOsmFeatures`:
//! testOsmPoint, testOsmPointSkipPass1, testExceptionWhileProcessingOsm, testOsmLine, testOsmLineOrPolygon,
//! testSplitAmbiguousOsmPolygon, testSplitOsmLineWithLoop, testOsmMultipolygon, testOsmLineInRelation,
//! testPreprocessOsmNodesAndWays, testOsmProfileFinish, testOsmProfileFinishForwardingProfile, testBadRelation,
//! testIssue496BaseballMultipolygon, testIssue509LenaDelta, testHandleProfileException) or the `Planetiler`
//! orchestrator (testPlanetilerRunner*, testPlanetilerMemoryCheck*) are not ported here.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use geo::Relate;
use geo_types::Geometry;
use planetiler_core::feature_collector::{self, FeatureCollector};
use planetiler_core::geo::geo_utils::{self, get_world_lat, get_world_lon, get_world_x, get_world_y};
use planetiler_core::geo::simplify_method::SimplifyMethod;
use planetiler_core::geo::test_utils::*;
use planetiler_core::geo::tile_coord::TileCoord;
use planetiler_core::profile::{FeatureConsumer, Profile};
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::tags;
use planetiler_core::testing::*;
use planetiler_core::value::Tags;

const Z15_TILES: u32 = 1 << 15;
const Z15_WIDTH: f64 = 1.0 / Z15_TILES as f64;
const Z14_TILES: u32 = 1 << 14;
const Z14_WIDTH: f64 = 1.0 / Z14_TILES as f64;
const Z13_TILES: u32 = 1 << 13;
const Z12_TILES: u32 = 1 << 12;
const Z11_TILES: u32 = 1 << 11;

fn world_polygon() -> geo_types::Polygon<f64> {
    new_polygon_from_coords(world_coordinate_list(&[
        Z14_WIDTH / 2.0,
        Z14_WIDTH / 2.0,
        1.0 - Z14_WIDTH / 2.0,
        Z14_WIDTH / 2.0,
        1.0 - Z14_WIDTH / 2.0,
        1.0 - Z14_WIDTH / 2.0,
        Z14_WIDTH / 2.0,
        1.0 - Z14_WIDTH / 2.0,
        Z14_WIDTH / 2.0,
        Z14_WIDTH / 2.0,
    ]))
}

fn new_reader_feature(geometry: impl Into<Geometry<f64>>, attrs: Tags) -> SimpleFeature {
    SimpleFeature::create_tags(geometry, attrs)
}

fn gc(geoms: Vec<Geometry<f64>>) -> Geometry<f64> {
    Geometry::GeometryCollection(new_geometry_collection(geoms))
}

fn resource(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources").join(name)
}

fn read_wkb_resource(name: &str) -> Geometry<f64> {
    let bytes = std::fs::read(resource(name)).unwrap();
    geo_utils::read_wkb(&bytes).unwrap()
}

fn z14_coordinate_list(coords: &[f64]) -> Vec<geo_types::Coord<f64>> {
    new_coordinate_list(coords)
        .into_iter()
        .map(|c| geo_types::coord! {x: get_world_lon(0.5 + c.x * Z14_WIDTH), y: get_world_lat(0.5 + c.y * Z14_WIDTH)})
        .collect()
}

fn z14_pixel_rectangle(min: f64, max: f64) -> Vec<geo_types::Coord<f64>> {
    rectangle_coord_list(min / 256.0, max / 256.0)
        .into_iter()
        .map(|c| geo_types::coord! {x: get_world_lon(0.5 + c.x * Z14_WIDTH), y: get_world_lat(0.5 + c.y * Z14_WIDTH)})
        .collect()
}

fn z14_coordinate_pixel_list(coords: &[f64]) -> Vec<geo_types::Coord<f64>> {
    z14_coordinate_list(&coords.iter().map(|c| c / 256.0).collect::<Vec<_>>())
}

fn z14_point(x: f64, y: f64) -> geo_types::Point<f64> {
    new_point(get_world_lon(0.5 + x * Z14_WIDTH / 256.0), get_world_lat(0.5 + y * Z14_WIDTH / 256.0))
}

/// `assertSubmap(sortListValues(expected), sortListValues(actual))`: like [`assert_submap`] but each tile's feature
/// list may be in any order.
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

#[test]
#[ignore = "slow (TAKES_A_MINUTE_OR_TWO in Java); run with --release --ignored"]
fn test_complex_shoreline_polygons_takes_a_minute_or_two() {
    for (file_name, expected) in
        [("chesapeake.wkb", 4076), ("mdshore.wkb", 19904), ("njshore.wkb", 10571), ("kobroor.wkb", 21693)]
    {
        let geometry = read_wkb_resource(file_name);
        // automatically checks for self-intersections
        let results = run_with_reader_features(
            &[("threads", "1")],
            vec![new_reader_feature(geometry, tags! {})],
            |_, features| {
                features.polygon("layer").set_zoom_range(0, 14).set_buffer_pixels(4.0);
            },
        )
        .unwrap();
        assert_eq!(expected, results.tiles.len(), "{file_name}");
    }
}

#[test]
fn test_reorder_nested_multipolygons() {
    let outer_points1 = world_rectangle(10.0 / 256.0, 240.0 / 256.0);
    let inner_points1 = world_rectangle(20.0 / 256.0, 230.0 / 256.0);
    let outer_points2 = world_rectangle(30.0 / 256.0, 220.0 / 256.0);
    let inner_points2 = world_rectangle(40.0 / 256.0, 210.0 / 256.0);

    let results = run_with_reader_features(
        &[("threads", "1")],
        vec![new_reader_feature(
            new_multi_polygon(vec![
                new_polygon_with_holes(outer_points2, vec![inner_points2]),
                new_polygon_with_holes(outer_points1, vec![inner_points1]),
            ]),
            tags! {},
        )],
        |_, features| {
            features.polygon("layer").set_zoom_range(0, 0).set_buffer_pixels(0.0);
        },
    )
    .unwrap();

    let tile_contents = &results.tiles[&TileCoord::of_xyz(0, 0, 0)];
    assert_eq!(1, tile_contents.len());
    let geom = tile_contents[0].geometry.geom();
    let Geometry::MultiPolygon(multi_polygon) = geom else { panic!("{geom:?}") };
    assert_eq!(2, multi_polygon.0.len());
    assert_same_normalized_feature(
        &new_polygon_with_holes(rectangle_coord_list(10.0, 240.0), vec![rectangle_coord_list(20.0, 230.0)]).into(),
        &multi_polygon.0[0].clone().into(),
    );
    assert_same_normalized_feature(
        &new_polygon_with_holes(rectangle_coord_list(30.0, 220.0), vec![rectangle_coord_list(40.0, 210.0)]).into(),
        &multi_polygon.0[1].clone().into(),
    );
}

fn polygon_collections() -> Vec<Vec<SimpleFeature>> {
    let (x1, y1, x2, y2) = (0.125, 0.125, 0.875, 0.875);
    let (x3, y3, x4, y4) = (0.25, 0.25, 0.75, 0.75);
    let (x5, y5, x6, y6) = (0.375, 0.375, 0.625, 0.625);
    let polygon1 = || -> Geometry<f64> {
        new_polygon_with_holes(
            rectangle_coord_list_xy(get_world_lon(x1), get_world_lat(y1), get_world_lon(x2), get_world_lat(y2)),
            vec![rectangle_coord_list_xy(get_world_lon(x3), get_world_lat(y3), get_world_lon(x4), get_world_lat(y4))],
        )
        .into()
    };
    let polygon2 = || -> Geometry<f64> {
        rectangle_xy(get_world_lon(x5), get_world_lat(y5), get_world_lon(x6), get_world_lat(y6)).into()
    };
    vec![
        // one collection with several polygons
        vec![new_reader_feature(gc(vec![polygon1(), polygon2()]), tags! {"attr" => "value"})],
        // nested collections, i.e. several collection, each with one polygon, in one collection
        vec![new_reader_feature(gc(vec![gc(vec![polygon1()]), gc(vec![polygon2()])]), tags! {"attr" => "value"})],
    ]
}

#[test]
fn test_polygon_collection() {
    for polygons in polygon_collections() {
        let results = run_with_reader_features(&[("threads", "1")], polygons, |_, features| {
            features
                .polygon("layer")
                .set_zoom_range(0, 0)
                .set_attr("name", "name value")
                .inherit_attr_from_source("attr");
        })
        .unwrap();

        assert_submap(
            &tile_map([(
                (0, 0, 0),
                vec![
                    feature(rectangle(0.375 * 256.0, 0.625 * 256.0), tags! {"attr" => "value", "name" => "name value"}),
                    feature(
                        new_polygon_with_holes(
                            rectangle_coord_list(0.125 * 256.0, 0.875 * 256.0),
                            vec![rectangle_coord_list(0.25 * 256.0, 0.75 * 256.0)],
                        ),
                        tags! {"attr" => "value", "name" => "name value"},
                    ),
                ],
            )]),
            &results.tiles,
        );
    }
}

#[test]
fn test_mixed_collection() {
    let point = new_point(get_world_lon(0.125), get_world_lat(0.125));
    let line = new_line_string(&[get_world_lon(0.25), get_world_lat(0.25), get_world_lon(0.75), get_world_lat(0.75)]);
    let polygon = rectangle_xy(get_world_lon(0.375), get_world_lat(0.375), get_world_lon(0.625), get_world_lat(0.625));

    let input =
        vec![new_reader_feature(gc(vec![point.into(), line.into(), polygon.into()]), tags! {"attr" => "value"})];

    let results = run_with_reader_features(&[("threads", "1")], input, |source, features| {
        // FeatureCollector.any_geometry() & co. expect just one feature hence to process several possibly even mixed
        // type features from collection we need a profile to do the iteration itself:
        let geom = source.world_geometry().unwrap();
        let Geometry::GeometryCollection(collection) = geom else {
            panic!("only collections expected in this test");
        };
        for g in &collection.0 {
            features
                .geometry("layer", g.clone())
                .set_zoom_range(0, 0)
                .set_attr("name", "name value")
                .inherit_attr_from_source("attr");
        }
    })
    .unwrap();

    let attrs = || tags! {"attr" => "value", "name" => "name value"};
    assert_submap(
        &tile_map([(
            (0, 0, 0),
            vec![
                feature(new_point(32.0, 32.0), attrs()),
                feature(rectangle(0.375 * 256.0, 0.625 * 256.0), attrs()),
                feature(new_line_string(&[64.0, 64.0, 192.0, 192.0]), attrs()),
            ],
        )]),
        &results.tiles,
    );
}

#[test]
fn test_post_process_node_use_label_grid_rank() {
    let y = 0.5 + Z14_WIDTH / 2.0;
    let lat = get_world_lat(y);

    let x1 = 0.5 + Z14_WIDTH / 4.0;
    let lng1 = get_world_lon(x1);
    let lng2 = get_world_lon(x1 + Z14_WIDTH * 10.0 / 256.0);
    let lng3 = get_world_lon(x1 + Z14_WIDTH * 20.0 / 256.0);

    let results = run_with_reader_features_and_post_process(
        &[("threads", "1")],
        vec![
            new_reader_feature(new_point(lng1, lat), tags! {"rank" => "1"}),
            new_reader_feature(new_point(lng2, lat), tags! {"rank" => "2"}),
            new_reader_feature(new_point(lng3, lat), tags! {"rank" => "3"}),
        ],
        |source, features| {
            let rank: i32 = source.base().tags.get("rank").unwrap().to_string().parse().unwrap();
            features
                .point("layer")
                .set_zoom_range(13, 14)
                .inherit_attr_from_source("rank")
                .set_sort_key(rank)
                .set_point_label_grid_pixel_size(13, 8.0)
                .set_buffer_pixels(8.0);
        },
        |layer, zoom, items| {
            if layer == "layer" && zoom == 13 {
                let mut rank_in_group: BTreeMap<i64, i64> = BTreeMap::new();
                for item in items.iter_mut() {
                    let rank = rank_in_group.entry(item.group).or_insert(0);
                    *rank += 1;
                    *item = item.copy_with_extra_attrs(&tags! {"grouprank" => *rank});
                }
            }
            Ok(())
        },
    )
    .unwrap();

    assert_submap(
        &tile_map([
            (
                (Z14_TILES / 2, Z14_TILES / 2, 14),
                vec![
                    feature(new_point(64.0, 128.0), tags! {"rank" => "1"}),
                    feature(new_point(74.0, 128.0), tags! {"rank" => "2"}),
                    feature(new_point(84.0, 128.0), tags! {"rank" => "3"}),
                ],
            ),
            (
                (Z13_TILES / 2, Z13_TILES / 2, 13),
                vec![
                    feature(new_point(32.0, 64.0), tags! {"rank" => "1", "grouprank" => 1i64}),
                    feature(new_point(37.0, 64.0), tags! {"rank" => "2", "grouprank" => 2i64}),
                    // separate group
                    feature(new_point(42.0, 64.0), tags! {"rank" => "3", "grouprank" => 1i64}),
                ],
            ),
        ]),
        &results.tiles,
    );
}

/// Java `postProcessTileFeatures(boolean postProcessLayersToo)`.
#[test]
fn post_process_tile_features() {
    for post_process_layers_too in [false, true] {
        let y = 0.5 + Z15_WIDTH / 2.0;
        let lat = get_world_lat(y);

        let x1 = 0.5 + Z15_WIDTH / 4.0;
        let lng1 = get_world_lon(x1);
        let lng2 = get_world_lon(x1 + Z15_WIDTH * 10.0 / 256.0);

        let features1 = vec![
            new_reader_feature(new_point(lng1, lat), tags! {"from" => "a"}),
            new_reader_feature(new_point(lng2, lat), tags! {"from" => "b"}),
        ];
        let profile = TestProfile::process_source_features(|source, features| {
            let from = source.base().tags.get("from").unwrap().to_string();
            features.point(&from).inherit_attr_from_source("from").set_min_zoom(15);
        })
        .with_layer_post_process(move |_, _, items| {
            if post_process_layers_too {
                items.reverse();
            }
            Ok(())
        })
        .with_tile_post_process(|_, layers| {
            let mut features = layers.remove("a").unwrap_or_default();
            features.extend(layers.remove("b").unwrap_or_default());
            layers.clear();
            layers.insert("c".to_string(), features);
            Ok(())
        });
        let results = run(
            &[("threads", "1"), ("maxzoom", "15")],
            |ctx| process_reader_features(ctx, &features1),
            Arc::new(profile),
        )
        .unwrap();

        assert_submap_unordered(
            &tile_map([(
                (Z15_TILES / 2, Z15_TILES / 2, 15),
                vec![
                    feature_in_layer("c", new_point(64.0, 128.0), tags! {"from" => "a"}),
                    feature_in_layer("c", new_point(74.0, 128.0), tags! {"from" => "b"}),
                ],
            )]),
            &results.tiles,
        );
    }
}

mod feature_merge_tests {
    use planetiler_core::feature_merge;

    use super::*;

    const Z16_TILES: u32 = 1 << 16;
    const Z16_WIDTH: f64 = 1.0 / Z16_TILES as f64;

    #[test]
    fn test_merge_line_strings() {
        for connect_endpoints in [false, true] {
            let y = 0.5 + Z16_WIDTH / 2.0;
            let lat = get_world_lat(y);

            let x1 = 0.5 + Z16_WIDTH / 4.0;
            let lng1 = get_world_lon(x1);
            let lng2 = get_world_lon(x1 + Z16_WIDTH * 10.0 / 256.0);
            let lng3 = get_world_lon(x1 + Z16_WIDTH * 20.0 / 256.0);
            let lng4 = get_world_lon(x1 + Z16_WIDTH * 30.0 / 256.0);

            let results = run_with_reader_features_and_post_process(
                &[("threads", "1"), ("maxzoom", "16")],
                vec![
                    // merge at z13 (same "group"):
                    new_reader_feature(
                        new_line_string(&[lng1, lat, lng2, lat]),
                        tags! {"group" => "1", "other" => "1"},
                    ),
                    new_reader_feature(
                        new_line_string(&[lng2, lat, lng3, lat]),
                        tags! {"group" => "1", "other" => "2"},
                    ),
                    // don't merge at z13:
                    new_reader_feature(
                        new_line_string(&[lng3, lat, lng4, lat]),
                        tags! {"group" => "2", "other" => "3"},
                    ),
                ],
                |source, features| {
                    let other = source.base().tags.get("other").cloned().unwrap_or_default();
                    features
                        .line("layer")
                        .set_min_zoom(13)
                        .set_attr_with_minzoom("z14attr", other, 14)
                        .inherit_attr_from_source("group");
                },
                move |_, _, items| {
                    let input = std::mem::take(items);
                    *items = if connect_endpoints {
                        feature_merge::merge_line_strings(input, 0.0, 0.0, 0.0)
                    } else {
                        feature_merge::merge_multi_line_string(input)
                    };
                    Ok(())
                },
            )
            .unwrap();

            let line = |c: &[f64]| new_line_string(c);
            assert_submap_unordered(
                &tile_map([
                    (
                        (Z16_TILES / 2, Z16_TILES / 2, 16),
                        vec![
                            feature(line(&[64.0, 128.0, 74.0, 128.0]), tags! {"group" => "1", "z14attr" => "1"}),
                            feature(line(&[74.0, 128.0, 84.0, 128.0]), tags! {"group" => "1", "z14attr" => "2"}),
                            feature(line(&[84.0, 128.0, 94.0, 128.0]), tags! {"group" => "2", "z14attr" => "3"}),
                        ],
                    ),
                    (
                        (Z15_TILES / 2, Z15_TILES / 2, 15),
                        vec![
                            feature(line(&[32.0, 64.0, 37.0, 64.0]), tags! {"group" => "1", "z14attr" => "1"}),
                            feature(line(&[37.0, 64.0, 42.0, 64.0]), tags! {"group" => "1", "z14attr" => "2"}),
                            feature(line(&[42.0, 64.0, 47.0, 64.0]), tags! {"group" => "2", "z14attr" => "3"}),
                        ],
                    ),
                    (
                        (Z14_TILES / 2, Z14_TILES / 2, 14),
                        vec![
                            feature(line(&[16.0, 32.0, 18.5, 32.0]), tags! {"group" => "1", "z14attr" => "1"}),
                            feature(line(&[18.5, 32.0, 21.0, 32.0]), tags! {"group" => "1", "z14attr" => "2"}),
                            feature(line(&[21.0, 32.0, 23.5, 32.0]), tags! {"group" => "2", "z14attr" => "3"}),
                        ],
                    ),
                    (
                        (Z13_TILES / 2, Z13_TILES / 2, 13),
                        if connect_endpoints {
                            vec![
                                // merge since they have same attrs
                                feature(line(&[8.0, 16.0, 10.5, 16.0]), tags! {"group" => "1"}),
                                feature(line(&[10.5, 16.0, 11.75, 16.0]), tags! {"group" => "2"}),
                            ]
                        } else {
                            vec![
                                feature(
                                    new_multi_line_string(vec![
                                        line(&[8.0, 16.0, 9.25, 16.0]),
                                        line(&[9.25, 16.0, 10.5, 16.0]),
                                    ]),
                                    tags! {"group" => "1"},
                                ),
                                feature(line(&[10.5, 16.0, 11.75, 16.0]), tags! {"group" => "2"}),
                            ]
                        },
                    ),
                ]),
                &results.tiles,
            );
        }
    }

    #[test]
    fn test_merge_line_strings_ignores_rounding_intersections() {
        let y = 0.5 + Z14_WIDTH / 2.0;
        let lat = get_world_lat(y);
        let lat2a = get_world_lat(y + Z14_WIDTH * 0.5 / 4096.0);
        let lat2b = get_world_lat(y + Z14_WIDTH * 1.5 / 4096.0);
        let lat3 = get_world_lat(y + Z14_WIDTH * 10.0 / 4096.0);

        let x1 = 0.5 + Z14_WIDTH / 4.0;
        let lng1 = get_world_lon(x1);
        let lng2 = get_world_lon(x1 + Z14_WIDTH * 10.0 / 256.0);
        let lng3 = get_world_lon(x1 + Z14_WIDTH * 20.0 / 256.0);

        let results = run_with_reader_features_and_post_process(
            &[("threads", "1")],
            vec![
                // group two parallel lines that almost touch at the midpoint
                // need to retain extra precision while merging to ensure it
                // doesn't confuse the line merger
                new_reader_feature(new_line_string(&[lng1, lat, lng2, lat2a]), tags! {}),
                new_reader_feature(new_line_string(&[lng2, lat2a, lng3, lat]), tags! {}),
                new_reader_feature(new_line_string(&[lng1, lat3, lng2, lat2b]), tags! {}),
                new_reader_feature(new_line_string(&[lng2, lat2b, lng3, lat3]), tags! {}),
            ],
            |_, features| {
                features.line("layer").set_zoom_range(13, 13);
            },
            |_, _, items| {
                *items = feature_merge::merge_line_strings(std::mem::take(items), 0.0, 0.0, 0.0);
                Ok(())
            },
        )
        .unwrap();

        assert_submap_unordered(
            &tile_map([(
                (Z13_TILES / 2, Z13_TILES / 2, 13),
                vec![feature(
                    new_multi_line_string(vec![
                        new_line_string(&[32.0, 64.3125, 37.0, 64.0625, 42.0, 64.3125]),
                        new_line_string(&[32.0, 64.0, 37.0, 64.0625, 42.0, 64.0]),
                    ]),
                    tags! {},
                )],
            )]),
            &results.tiles,
        );
    }

    #[test]
    fn test_merge_polygons() {
        for union_overlapping in [false, true] {
            let results = run_with_reader_features_and_post_process(
                &[("threads", "1")],
                vec![
                    // merge same group:
                    new_reader_feature(
                        new_polygon_from_coords(z14_coordinate_pixel_list(&[
                            10.0, 10.0, 20.0, 10.0, 20.0, 20.0, 10.0, 20.0, 10.0, 10.0,
                        ])),
                        tags! {"group" => "1"},
                    ),
                    new_reader_feature(
                        new_polygon_from_coords(z14_coordinate_pixel_list(&[
                            20.5, 10.0, 30.0, 10.0, 30.0, 20.0, 20.5, 20.0, 20.5, 10.0,
                        ])),
                        tags! {"group" => "1"},
                    ),
                    // don't merge - different group:
                    new_reader_feature(
                        new_polygon_from_coords(z14_coordinate_pixel_list(&[
                            10.0, 20.5, 20.0, 20.5, 20.0, 30.0, 10.0, 30.0, 10.0, 20.5,
                        ])),
                        tags! {"group" => "2"},
                    ),
                ],
                |_, features| {
                    features.polygon("layer").set_zoom_range(14, 14).inherit_attr_from_source("group");
                },
                move |_, _, items| {
                    let input = std::mem::take(items);
                    *items = if union_overlapping {
                        feature_merge::merge_nearby_polygons(input, 0.0, 0.0, 1.0, 1.0)?
                    } else {
                        feature_merge::merge_multi_polygon(input)
                    };
                    Ok(())
                },
            )
            .unwrap();

            let expected = if union_overlapping {
                vec![
                    feature(rectangle_xy(10.0, 10.0, 30.0, 20.0), tags! {"group" => "1"}),
                    feature(rectangle_xy(10.0, 20.5, 20.0, 30.0), tags! {"group" => "2"}),
                ]
            } else {
                vec![
                    feature(
                        new_multi_polygon(vec![
                            rectangle_xy(10.0, 10.0, 20.0, 20.0),
                            rectangle_xy(20.5, 10.0, 30.0, 20.0),
                        ]),
                        tags! {"group" => "1"},
                    ),
                    feature(rectangle_xy(10.0, 20.5, 20.0, 30.0), tags! {"group" => "2"}),
                ]
            };
            assert_submap_unordered(&tile_map([((Z14_TILES / 2, Z14_TILES / 2, 14), expected)]), &results.tiles);
        }
    }

    #[test]
    fn test_combine_multi_point() {
        let results = run_with_reader_features_and_post_process(
            &[("threads", "1")],
            vec![
                // merge same group:
                new_reader_feature(z14_point(0.0, 0.0), tags! {"group" => "1"}),
                new_reader_feature(
                    new_multi_point(vec![z14_point(1.0, 1.0), z14_point(2.0, 2.0)]),
                    tags! {"group" => "1"},
                ),
                // don't merge - different group:
                new_reader_feature(z14_point(3.0, 3.0), tags! {"group" => "2"}),
            ],
            |_, features| {
                features.point("layer").set_zoom_range(14, 14).set_buffer_pixels(0.0).inherit_attr_from_source("group");
            },
            |_, _, items| {
                *items = feature_merge::merge_multi_point(std::mem::take(items));
                Ok(())
            },
        )
        .unwrap();

        assert_submap_unordered(
            &tile_map([(
                (Z14_TILES / 2, Z14_TILES / 2, 14),
                vec![
                    feature(
                        new_multi_point(vec![new_point(0.0, 0.0), new_point(1.0, 1.0), new_point(2.0, 2.0)]),
                        tags! {"group" => "1"},
                    ),
                    feature(new_point(3.0, 3.0), tags! {"group" => "2"}),
                ],
            )]),
            &results.tiles,
        );
    }
}

#[test]
fn test_reduce_max_point_buffer() {
    let results = run_with_reader_features(
        &[("threads", "1"), ("max-point-buffer", "1")],
        vec![
            new_reader_feature(z14_point(0.0, 0.0), tags! {"group" => "1"}),
            new_reader_feature(
                new_multi_point(vec![
                    z14_point(-1.0, -1.0),
                    z14_point(-2.0, -2.0), // should get filtered out
                ]),
                tags! {"group" => "1"},
            ),
            // don't merge - different group:
            new_reader_feature(z14_point(257.0, 257.0), tags! {"group" => "2"}),
            new_reader_feature(z14_point(258.0, 258.0), tags! {"group" => "3"}), // filter out
        ],
        |_, features| {
            features.point("layer").set_zoom_range(14, 14).set_buffer_pixels(10.0).inherit_attr_from_source("group");
        },
    )
    .unwrap();

    assert_submap_unordered(
        &tile_map([(
            (Z14_TILES / 2, Z14_TILES / 2, 14),
            vec![
                feature(new_point(-1.0, -1.0), tags! {"group" => "1"}),
                feature(new_point(0.0, 0.0), tags! {"group" => "1"}),
                feature(new_point(257.0, 257.0), tags! {"group" => "2"}),
            ],
        )]),
        &results.tiles,
    );
}

/// Profile that remembers every source feature and only emits output features from `finish`.
#[derive(Default)]
struct FinishProfile {
    feature_list: Mutex<Vec<SimpleFeature>>,
}

impl Profile for FinishProfile {
    fn process_feature(&self, source: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {
        let feature = SimpleFeature::create_world(
            source.world_geometry().unwrap().clone(),
            source.base().tags.clone(),
            source.source(),
            source.source_layer(),
            source.id(),
        );
        self.feature_list.lock().unwrap().push(feature);
    }

    fn finish(&self, name: &str, feature_collectors: &feature_collector::Factory, next: &mut FeatureConsumer<'_>) {
        if name == "test" {
            for source in self.feature_list.lock().unwrap().iter() {
                let mut features = feature_collectors.get(source);
                features.point("layer").set_zoom_range(13, 14).inherit_attr_from_source("a");
                for feature in features.iter() {
                    next(feature);
                }
            }
        }
    }

    fn name(&self) -> String {
        "test".to_string()
    }
}

#[test]
fn test_reader_profile_finish() {
    let y = 0.5 + Z14_WIDTH / 2.0;
    let lat = get_world_lat(y);

    let x1 = 0.5 + Z14_WIDTH / 4.0;
    let lng1 = get_world_lon(x1);
    let lng2 = get_world_lon(x1 + Z14_WIDTH * 10.0 / 256.0);

    let results = run_with_reader_features_profile(
        &[("threads", "1")],
        vec![
            new_reader_feature(new_point(lng1, lat), tags! {"a" => 1i64, "b" => 2i64}),
            new_reader_feature(new_point(lng2, lat), tags! {"a" => 3i64, "b" => 4i64}),
        ],
        FinishProfile::default(),
    )
    .unwrap();

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
                    feature(new_point(32.0, 64.0), tags! {"a" => 1i64}),
                    feature(new_point(37.0, 64.0), tags! {"a" => 3i64}),
                ],
            ),
        ]),
        &results.tiles,
    );
}

#[test]
fn test_issue546_terschelling() {
    let geometry = geo_utils::world_to_lat_lon_coords(&read_wkb_resource("issue_546_terschelling.wkb"));

    // automatically checks for self-intersections
    let results =
        run_with_reader_features(&[("threads", "1")], vec![new_reader_feature(geometry, tags! {})], |_, features| {
            features.polygon("ocean").set_buffer_pixels(4.0).set_min_zoom(0);
        })
        .unwrap();

    // this lat/lon is in the middle of an island and should not be covered by
    for z in 4..=14 {
        let lat = 53.391958;
        let lon = 5.2438441;

        let coord = TileCoord::around_lng_lat(lon, lat, z);
        let problematic_tile = results.tiles.get(&coord);
        if z == 14 {
            assert!(problematic_tile.is_none());
            continue;
        }
        let scale = 2f64.powi(coord.z as i32);

        let tile_x = (get_world_x(lon) * scale - coord.x as f64) * 256.0;
        let tile_y = (get_world_y(lat) * scale - coord.y as f64) * 256.0;

        let point: Geometry<f64> = new_point(tile_x, tile_y).into();

        let problematic_tile = problematic_tile.unwrap_or_else(|| panic!("missing z{z}"));
        assert_eq!(1, problematic_tile.len());
        let geom = problematic_tile[0].geometry.geom();
        validate_geometry(geom);

        assert!(!geom.relate(&point).is_covers(), "z{z}");
    }
}

fn run_for_compact_test(compact_db_enabled: bool) -> PlanetilerResults {
    let compact = compact_db_enabled.to_string();
    run_with_reader_features(
        &[("threads", "1"), ("mbtiles-compact", &compact)],
        vec![new_reader_feature(world_polygon(), tags! {})],
        |_, features| {
            features.polygon("layer").set_zoom_range(0, 2).set_buffer_pixels(1.0);
        },
    )
    .unwrap()
}

#[test]
fn test_compact_db() {
    let compact_result = run_for_compact_test(true);
    let non_compact_result = run_for_compact_test(false);

    assert_eq!(non_compact_result.tiles, compact_result.tiles);
    assert!(
        compact_result.tile_data_count < compact_result.tiles.len(),
        "tileDataCount={} should be less than tileCount={}",
        compact_result.tile_data_count,
        compact_result.tiles.len()
    );
}

fn run_for_max_zoom_test(args: &[(&str, &str)]) -> PlanetilerResults {
    let z8_pixel = 1.0 / (1 << 8) as f64 / 256.0;
    let tile_middle = 0.5 + Z14_WIDTH / 2.0;
    run_with_reader_features(
        args,
        vec![new_reader_feature(
            new_line_string_from_coords(world_coordinate_list(&[
                tile_middle,
                tile_middle,
                tile_middle + z8_pixel / 2.0,
                tile_middle,
            ])),
            tags! {},
        )],
        |_, features| {
            features.line("layer").set_buffer_pixels(0.0).set_zoom_range(0, 14);
        },
    )
    .unwrap()
}

#[test]
fn test_render_maxzoom() {
    let base_result = run_for_max_zoom_test(&[("minzoom", "0"), ("maxzoom", "14")]);
    let maxzoom_result = run_for_max_zoom_test(&[("minzoom", "0"), ("maxzoom", "8")]);
    let render_maxzoom_result = run_for_max_zoom_test(&[("minzoom", "0"), ("maxzoom", "8"), ("render_maxzoom", "8")]);

    // the feature is too small to include at z8, unless z8 is the max zoom for rendering to support overzooming
    let z8_tile = TileCoord::of_xyz((1 << 8) / 2, (1 << 8) / 2, 8);
    assert!(!base_result.tiles.contains_key(&z8_tile));
    assert!(render_maxzoom_result.tiles.contains_key(&z8_tile));
    assert!(!maxzoom_result.tiles.contains_key(&z8_tile));
}

fn run_for_bounds_test(minzoom: u32, maxzoom: u32, extra_args: &[(&str, &str)]) -> PlanetilerResults {
    let mut args = vec![("threads", "1")];
    args.extend_from_slice(extra_args);
    run_with_reader_features(&args, vec![new_reader_feature(world_polygon(), tags! {})], move |_, features| {
        features.polygon("layer").set_zoom_range(minzoom, maxzoom).set_buffer_pixels(0.0);
    })
    .unwrap()
}

#[test]
fn test_bound_filters() {
    let poly_path = resource("bottomrightearth.poly").to_string_lossy().into_owned();
    let orig_result = run_for_bounds_test(0, 2, &[]);
    let mut bbox_result = run_for_bounds_test(0, 2, &[("bounds", "1,-85.05113,180,-1")]);
    let poly_result = run_for_bounds_test(0, 2, &[("polygon", &poly_path)]);

    assert_eq!(1 + 4 + 16, orig_result.tiles.len());
    let keys = |r: &PlanetilerResults| r.tiles.keys().copied().collect::<Vec<_>>();
    let set = |coords: &[(u32, u32, u32)]| {
        let mut v: Vec<TileCoord> = coords.iter().map(|&(x, y, z)| TileCoord::of_xyz(x, y, z)).collect();
        v.sort();
        v
    };
    assert_eq!(set(&[(0, 0, 0), (1, 1, 1), (2, 2, 2), (3, 2, 2), (2, 3, 2), (3, 3, 2)]), keys(&bbox_result));
    assert_eq!(
        set(&[
            (0, 0, 0),
            (1, 1, 1),
            // (2, 2, 2) - omit since this one is outside of triangle
            (3, 2, 2),
            (2, 3, 2),
            (3, 3, 2)
        ]),
        keys(&poly_result)
    );

    // but besides the omitted tile, the rest should be the same
    bbox_result.tiles.remove(&TileCoord::of_xyz(2, 2, 2));
    assert_eq!(bbox_result.tiles, poly_result.tiles);
}

#[test]
fn test_simple_polygon() {
    for strategy in [
        SimplifyMethod::RetainImportantPoints,
        SimplifyMethod::RetainEffectiveAreas,
        SimplifyMethod::RetainWeightedEffectiveAreas,
    ] {
        let points = z14_pixel_rectangle(0.0, 40.0);

        let results = run_with_reader_features(
            &[("threads", "1")],
            vec![new_reader_feature(new_polygon_from_coords(points), tags! {})],
            move |_, features| {
                features
                    .polygon("layer")
                    .set_zoom_range(0, 14)
                    .set_buffer_pixels(0.0)
                    .set_pixel_tolerance(1.0)
                    .set_simplify_method(strategy)
                    .set_min_pixel_size(10.0); // should only show up z14 (40) z13 (20) and z12 (10)
            },
        )
        .unwrap();

        assert_eq!(
            tile_map([
                ((Z12_TILES / 2, Z12_TILES / 2, 12), vec![feature(rectangle(0.0, 10.0), tags! {})]),
                ((Z13_TILES / 2, Z13_TILES / 2, 13), vec![feature(rectangle(0.0, 20.0), tags! {})]),
                ((Z14_TILES / 2, Z14_TILES / 2, 14), vec![feature(rectangle(0.0, 40.0), tags! {})]),
            ]),
            results.tiles,
            "{strategy:?}"
        );
    }
}

#[test]
fn test_centroid_with_polygon_min_size() {
    let points = z14_pixel_rectangle(0.0, 40.0);

    let results = run_with_reader_features(
        &[("threads", "1")],
        vec![new_reader_feature(new_polygon_from_coords(points), tags! {})],
        |_, features| {
            features.centroid("layer").set_zoom_range(0, 14).set_buffer_pixels(0.0).set_min_pixel_size(10.0); // should only show up z14 (40) z13 (20) and z12 (10)
        },
    )
    .unwrap();

    assert_eq!(
        tile_map([
            ((Z12_TILES / 2, Z12_TILES / 2, 12), vec![feature(new_point(5.0, 5.0), tags! {})]),
            ((Z13_TILES / 2, Z13_TILES / 2, 13), vec![feature(new_point(10.0, 10.0), tags! {})]),
            ((Z14_TILES / 2, Z14_TILES / 2, 14), vec![feature(new_point(20.0, 20.0), tags! {})]),
        ]),
        results.tiles
    );
}

#[test]
fn test_centroid_with_line_min_size() {
    let points = z14_coordinate_pixel_list(&[0.0, 4.0, 40.0, 4.0]);

    let results = run_with_reader_features(
        &[("threads", "1")],
        vec![new_reader_feature(new_line_string_from_coords(points), tags! {})],
        |_, features| {
            features.centroid("layer").set_zoom_range(0, 14).set_buffer_pixels(0.0).set_min_pixel_size(10.0); // should only show up z14 (40) z13 (20) and z12 (10)
        },
    )
    .unwrap();

    assert_eq!(
        tile_map([
            ((Z12_TILES / 2, Z12_TILES / 2, 12), vec![feature(new_point(5.0, 1.0), tags! {})]),
            ((Z13_TILES / 2, Z13_TILES / 2, 13), vec![feature(new_point(10.0, 2.0), tags! {})]),
            ((Z14_TILES / 2, Z14_TILES / 2, 14), vec![feature(new_point(20.0, 4.0), tags! {})]),
        ]),
        results.tiles
    );
}

#[test]
fn test_attribute_min_size_line() {
    let points = z14_coordinate_pixel_list(&[0.0, 4.0, 40.0, 4.0]);

    let results = run_with_reader_features(
        &[("threads", "1")],
        vec![new_reader_feature(new_line_string_from_coords(points), tags! {})],
        |_, features| {
            features
                .line("layer")
                .set_zoom_range(11, 14)
                .set_buffer_pixels(0.0)
                .set_attr_with_min_size("a", "1", 10.0)
                .set_attr_with_min_size("b", "2", 20.0)
                .set_attr_with_min_size("c", "3", 40.0)
                .set_attr_with_min_size_range("d", "4", 40.0, 0, 13); // should show up at z13 and above
        },
    )
    .unwrap();

    assert_eq!(
        tile_map([
            ((Z11_TILES / 2, Z11_TILES / 2, 11), vec![feature(new_line_string(&[0.0, 0.5, 5.0, 0.5]), tags! {})]),
            (
                (Z12_TILES / 2, Z12_TILES / 2, 12),
                vec![feature(new_line_string(&[0.0, 1.0, 10.0, 1.0]), tags! {"a" => "1"})]
            ),
            (
                (Z13_TILES / 2, Z13_TILES / 2, 13),
                vec![feature(new_line_string(&[0.0, 2.0, 20.0, 2.0]), tags! {"a" => "1", "b" => "2", "d" => "4"})]
            ),
            (
                (Z14_TILES / 2, Z14_TILES / 2, 14),
                vec![feature(
                    new_line_string(&[0.0, 4.0, 40.0, 4.0]),
                    tags! {"a" => "1", "b" => "2", "c" => "3", "d" => "4"}
                )]
            ),
        ]),
        results.tiles
    );
}

#[test]
fn test_attribute_min_size_point() {
    let points = z14_coordinate_pixel_list(&[0.0, 4.0, 40.0, 4.0]);

    let results = run_with_reader_features(
        &[("threads", "1")],
        vec![new_reader_feature(new_line_string_from_coords(points), tags! {})],
        |_, features| {
            features
                .centroid("layer")
                .set_zoom_range(11, 14)
                .set_buffer_pixels(0.0)
                .set_attr_with_min_size("a", "1", 10.0)
                .set_attr_with_min_size("b", "2", 20.0)
                .set_attr_with_min_size("c", "3", 40.0)
                .set_attr_with_min_size_range("d", "4", 40.0, 0, 13); // should show up at z13 and above
        },
    )
    .unwrap();

    assert_eq!(
        tile_map([
            ((Z11_TILES / 2, Z11_TILES / 2, 11), vec![feature(new_point(2.5, 0.5), tags! {})]),
            ((Z12_TILES / 2, Z12_TILES / 2, 12), vec![feature(new_point(5.0, 1.0), tags! {"a" => "1"})]),
            (
                (Z13_TILES / 2, Z13_TILES / 2, 13),
                vec![feature(new_point(10.0, 2.0), tags! {"a" => "1", "b" => "2", "d" => "4"})]
            ),
            (
                (Z14_TILES / 2, Z14_TILES / 2, 14),
                vec![feature(new_point(20.0, 4.0), tags! {"a" => "1", "b" => "2", "c" => "3", "d" => "4"})]
            ),
        ]),
        results.tiles
    );
}

#[test]
fn test_bound_filters_fill() {
    let poly_path = resource("bottomrightearth.poly").to_string_lossy().into_owned();
    let poly_result_z8 = run_for_bounds_test(8, 8, &[("polygon", &poly_path)]);

    let z8tiles = 1 << 8;
    assert!(!poly_result_z8.tiles.contains_key(&TileCoord::of_xyz(z8tiles * 3 / 4, z8tiles * 5 / 8, 8)));
    assert!(poly_result_z8.tiles.contains_key(&TileCoord::of_xyz(z8tiles * 3 / 4, z8tiles * 7 / 8, 8)));
}
