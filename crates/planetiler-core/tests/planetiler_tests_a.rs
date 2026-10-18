//! Port of `PlanetilerTests.java` (testAttributeTypeCoercion .. testZ16HighTileIdSortOrder): in-memory end-to-end
//! tests with `SimpleFeature` inputs run through the `runWithReaderFeatures` harness in
//! `planetiler_core::testing`.

use geo_types::{Coord, Geometry};
use planetiler_core::geo::geo_utils::{get_world_lat, get_world_lon};
use planetiler_core::geo::simplify_method::SimplifyMethod;
use planetiler_core::geo::test_utils::*;
use planetiler_core::geo::tile_coord::TileCoord;
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::tags;
use planetiler_core::testing::*;
use planetiler_core::value::Tags;

const Z16_TILES: u32 = 1 << 16;
const Z16_WIDTH: f64 = 1.0 / Z16_TILES as f64;
const Z15_TILES: u32 = 1 << 15;
const Z14_TILES: u32 = 1 << 14;
const Z14_WIDTH: f64 = 1.0 / Z14_TILES as f64;
const Z13_TILES: u32 = 1 << 13;
const Z13_WIDTH: f64 = 1.0 / Z13_TILES as f64;
const Z12_TILES: u32 = 1 << 12;
const Z12_WIDTH: f64 = 1.0 / Z12_TILES as f64;

fn new_reader_feature(geometry: impl Into<Geometry<f64>>, attrs: Tags) -> SimpleFeature {
    SimpleFeature::create_tags(geometry, attrs)
}

fn assert_same_json(expected: &str, actual: Option<&String>) {
    let expected: serde_json::Value = serde_json::from_str(expected).unwrap();
    let actual: serde_json::Value = serde_json::from_str(actual.expect("missing json")).unwrap();
    assert_eq!(expected, actual);
}

fn gc(geoms: Vec<Geometry<f64>>) -> Geometry<f64> {
    Geometry::GeometryCollection(new_geometry_collection(geoms))
}

/// Java `z14CoordinateList`: coordinates in units of z14 tiles offset from the center of the world.
fn z14_coordinate_list(coords: &[f64]) -> Vec<Coord<f64>> {
    new_coordinate_list(coords)
        .into_iter()
        .map(|c| Coord { x: get_world_lon(0.5 + c.x * Z14_WIDTH), y: get_world_lat(0.5 + c.y * Z14_WIDTH) })
        .collect()
}

fn lat_lon(x: f64, y: f64) -> (f64, f64) {
    (get_world_lon(x), get_world_lat(y))
}

#[test]
#[ignore = "TODO(port): MLT tile format is not supported yet (attribute type coercion is an MLT encoder feature)"]
fn test_attribute_type_coercion() {
    let (lng, lat) = lat_lon(0.5 + Z14_WIDTH / 4.0, 0.5 + Z14_WIDTH / 4.0);
    let results = run_with_reader_features(
        &[("threads", "1"), ("maxzoom", "15"), ("tile-format", "mlt")],
        vec![
            new_reader_feature(new_point(lng, lat), tags! {"attr" => "string"}),
            new_reader_feature(new_point(lng, lat), tags! {"attr" => 1}),
            new_reader_feature(new_point(lng, lat), tags! {"attr" => 1.5}),
            new_reader_feature(new_point(lng, lat), tags! {"attr" => true}),
        ],
        |_, features| {
            features.point("layer").set_zoom_range(15, 15).inherit_attr_from_source("attr");
        },
    )
    .unwrap();
    assert_lists_contain_same_elements(
        &[
            feature_in_layer("layer", new_point(128.0, 128.0), tags! {"attr" => "string"}),
            feature_in_layer("layer", new_point(128.0, 128.0), tags! {"attr" => "1"}),
            feature_in_layer("layer", new_point(128.0, 128.0), tags! {"attr" => "1.5"}),
            feature_in_layer("layer", new_point(128.0, 128.0), tags! {"attr" => "true"}),
        ],
        &results.tiles[&TileCoord::of_xyz(Z15_TILES / 2, Z15_TILES / 2, 15)],
    );
    assert_same_json(
        r#"[{"id": "layer", "fields": {"attr": "String"}, "minzoom": 15, "maxzoom": 15}]"#,
        results.metadata.get("vector_layers"),
    );
}

fn two_points() -> ((f64, f64), (f64, f64)) {
    let x1 = 0.5 + Z14_WIDTH / 2.0;
    let y1 = 0.5 + Z14_WIDTH / 2.0;
    let x2 = x1 + Z13_WIDTH / 256.0;
    let y2 = y1 + Z13_WIDTH / 256.0;
    (lat_lon(x1, y1), lat_lon(x2, y2))
}

#[test]
fn test_multi_point() {
    let ((lng1, lat1), (lng2, lat2)) = two_points();
    let results = run_with_reader_features(
        &[("threads", "1")],
        vec![new_reader_feature(
            new_multi_point(vec![new_point(lng1, lat1), new_point(lng2, lat2)]),
            tags! {"attr" => "value"},
        )],
        |_, features| {
            features
                .point("layer")
                .set_zoom_range(13, 14)
                .set_attr("name", "name value")
                .inherit_attr_from_source("attr");
        },
    )
    .unwrap();
    let attrs = || tags! {"attr" => "value", "name" => "name value"};
    assert_submap(
        &tile_map([
            (
                (Z14_TILES / 2, Z14_TILES / 2, 14),
                vec![feature(new_multi_point(vec![new_point(128.0, 128.0), new_point(130.0, 130.0)]), attrs())],
            ),
            (
                (Z13_TILES / 2, Z13_TILES / 2, 13),
                vec![feature(new_multi_point(vec![new_point(64.0, 64.0), new_point(65.0, 65.0)]), attrs())],
            ),
        ]),
        &results.tiles,
    );
}

fn point_collections() -> Vec<Vec<SimpleFeature>> {
    let ((lng1, lat1), (lng2, lat2)) = two_points();
    vec![
        // one collection with several points
        vec![new_reader_feature(
            gc(vec![new_point(lng1, lat1).into(), new_point(lng2, lat2).into()]),
            tags! {"attr" => "value"},
        )],
        // nested collections, i.e. several collection, each with one point, in one collection
        vec![new_reader_feature(
            gc(vec![gc(vec![new_point(lng1, lat1).into()]), gc(vec![new_point(lng2, lat2).into()])]),
            tags! {"attr" => "value"},
        )],
    ]
}

// note: Same as test_multi_point() but we get list of points in the result, not one multipoint
#[test]
fn test_point_collection() {
    for points in point_collections() {
        let results = run_with_reader_features(&[("threads", "1")], points, |_, features| {
            features
                .point("layer")
                .set_zoom_range(13, 14)
                .set_attr("name", "name value")
                .inherit_attr_from_source("attr");
        })
        .unwrap();
        let attrs = || tags! {"attr" => "value", "name" => "name value"};
        assert_submap(
            &tile_map([
                (
                    (Z14_TILES / 2, Z14_TILES / 2, 14),
                    vec![feature(new_point(128.0, 128.0), attrs()), feature(new_point(130.0, 130.0), attrs())],
                ),
                (
                    (Z13_TILES / 2, Z13_TILES / 2, 13),
                    vec![feature(new_point(64.0, 64.0), attrs()), feature(new_point(65.0, 65.0), attrs())],
                ),
            ]),
            &results.tiles,
        );
    }
}

#[test]
fn test_label_grid_limit() {
    let lat = get_world_lat(0.5 + Z14_WIDTH / 2.0);
    let x1 = 0.5 + Z14_WIDTH / 4.0;
    let lng1 = get_world_lon(x1);
    let lng2 = get_world_lon(x1 + Z14_WIDTH * 10.0 / 256.0);
    let lng3 = get_world_lon(x1 + Z14_WIDTH * 20.0 / 256.0);
    let results = run_with_reader_features(
        &[("threads", "1")],
        vec![
            new_reader_feature(new_point(lng1, lat), tags! {"rank" => "1"}),
            new_reader_feature(new_point(lng2, lat), tags! {"rank" => "2"}),
            new_reader_feature(new_point(lng3, lat), tags! {"rank" => "3"}),
        ],
        |source, features| {
            let rank: i32 = source.get_tag("rank").unwrap().to_string().parse().unwrap();
            features
                .point("layer")
                .set_zoom_range(13, 14)
                .inherit_attr_from_source("rank")
                .set_sort_key(rank)
                .set_point_label_grid_size_and_limit(13, 128.0, 2)
                .set_buffer_pixels(128.0);
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
                    // omit rank=3 due to label grid size
                    feature(new_point(32.0, 64.0), tags! {"rank" => "1"}),
                    feature(new_point(37.0, 64.0), tags! {"rank" => "2"}),
                ],
            ),
        ]),
        &results.tiles,
    );
}

#[test]
fn test_min_size() {
    let (lng, lat) = lat_lon(0.5 + Z14_WIDTH / 4.0, 0.5 + Z14_WIDTH / 4.0);
    let delta = 5e-5;
    let results = run_with_reader_features(
        &[("threads", "1"), ("maxzoom", "15")],
        vec![
            new_reader_feature(new_point(lng, lat), tags! {"type" => "point"}),
            new_reader_feature(
                rectangle_xy(lng - delta, lat - delta, lng + delta, lat + delta),
                tags! {"type" => "poly"},
            ),
            new_reader_feature(new_line_string(&[lng - delta, lat, lng + delta, lat]), tags! {"type" => "line"}),
        ],
        |_, features| {
            features
                .centroid("layer")
                .set_zoom_range(13, 15)
                .set_min_pixel_size_at_all_zooms(1.0)
                .inherit_attr_from_source("type");
        },
    )
    .unwrap();
    assert_eq!(
        tile_map([
            (
                (Z15_TILES / 2, Z15_TILES / 2, 15),
                vec![
                    feature(new_point(128.0, 128.0), tags! {"type" => "line"}),
                    feature(new_point(128.0, 128.0), tags! {"type" => "poly"}),
                    // omit point when min size is set
                ],
            ),
            (
                (Z14_TILES / 2, Z14_TILES / 2, 14),
                vec![
                    feature(new_point(64.0, 64.0), tags! {"type" => "line"}),
                    feature(new_point(64.0, 64.0), tags! {"type" => "poly"}),
                ],
            ), // features are too small at z13
        ]),
        results.tiles
    );
}

#[test]
fn test_line_string() {
    for (any_geom, simplify_strategy) in [
        (false, SimplifyMethod::RetainImportantPoints),
        (false, SimplifyMethod::RetainEffectiveAreas),
        (false, SimplifyMethod::RetainWeightedEffectiveAreas),
        (true, SimplifyMethod::RetainImportantPoints),
    ] {
        let x1 = 0.5 + Z14_WIDTH / 2.0;
        let y1 = 0.5 + Z14_WIDTH / 2.0;
        let x2 = x1 + Z14_WIDTH;
        let y2 = y1 + Z14_WIDTH;
        let (lng1, lat1) = lat_lon(x1, y1);
        let (lng_mid, lat_mid) = lat_lon((x1 + x2) / 2.0, (y1 + y2) / 2.0);
        let (lng2, lat2) = lat_lon(x2, y2);
        let results = run_with_reader_features(
            &[("threads", "1")],
            vec![new_reader_feature(
                new_line_string(&[lng1, lat1, lng_mid, lat_mid, lng2, lat2]),
                tags! {"attr" => "value"},
            )],
            move |_, features| {
                let f = if any_geom { features.any_geometry("layer") } else { features.line("layer") };
                f.set_zoom_range(13, 14)
                    .set_pixel_tolerance(1.0)
                    .set_simplify_method(simplify_strategy)
                    .set_buffer_pixels(4.0);
            },
        )
        .unwrap();
        assert_submap(
            &tile_map([
                (
                    (Z14_TILES / 2, Z14_TILES / 2, 14),
                    vec![feature(new_line_string(&[128.0, 128.0, 260.0, 260.0]), tags! {})],
                ),
                (
                    (Z14_TILES / 2 + 1, Z14_TILES / 2 + 1, 14),
                    vec![feature(new_line_string(&[-4.0, -4.0, 128.0, 128.0]), tags! {})],
                ),
                (
                    (Z13_TILES / 2, Z13_TILES / 2, 13),
                    vec![feature(new_line_string(&[64.0, 64.0, 192.0, 192.0]), tags! {})],
                ),
            ]),
            &results.tiles,
        );
    }
}

fn diagonal_z14_line() -> SimpleFeature {
    let x1 = 0.5 + Z14_WIDTH / 2.0;
    let y1 = 0.5 + Z14_WIDTH / 2.0;
    let (lng1, lat1) = lat_lon(x1, y1);
    let (lng2, lat2) = lat_lon(x1 + Z14_WIDTH, y1 + Z14_WIDTH);
    new_reader_feature(new_line_string(&[lng1, lat1, lng2, lat2]), tags! {"attr" => "value"})
}

#[test]
fn test_partial_line() {
    let results = run_with_reader_features(&[("threads", "1")], vec![diagonal_z14_line()], |_, features| {
        features.partial_line("layer", 0.0, 0.5).set_zoom_range(13, 14).set_buffer_pixels(4.0);
    })
    .unwrap();
    assert_submap(
        &tile_map([
            (
                (Z14_TILES / 2, Z14_TILES / 2, 14),
                vec![feature(new_line_string(&[128.0, 128.0, 256.0, 256.0]), tags! {})],
            ),
            (
                (Z14_TILES / 2 + 1, Z14_TILES / 2 + 1, 14),
                vec![feature(new_line_string(&[-4.0, -4.0, 0.0, 0.0]), tags! {})],
            ),
            ((Z13_TILES / 2, Z13_TILES / 2, 13), vec![feature(new_line_string(&[64.0, 64.0, 128.0, 128.0]), tags! {})]),
        ]),
        &results.tiles,
    );
}

#[test]
fn test_line_with_partial_attr() {
    let results = run_with_reader_features(&[("threads", "1")], vec![diagonal_z14_line()], |_, features| {
        features
            .line("layer")
            .linear_range(0.0, 0.25)
            .set_attr_with_minzoom("k", "v", 14)
            .entire_line()
            .set_zoom_range(13, 14)
            .set_buffer_pixels(4.0);
    })
    .unwrap();
    assert_submap(
        &tile_map([
            (
                (Z14_TILES / 2, Z14_TILES / 2, 14),
                vec![
                    feature(new_line_string(&[192.0, 192.0, 260.0, 260.0]), tags! {}),
                    feature(new_line_string(&[128.0, 128.0, 192.0, 192.0]), tags! {"k" => "v"}),
                ],
            ),
            (
                (Z14_TILES / 2 + 1, Z14_TILES / 2 + 1, 14),
                vec![feature(new_line_string(&[-4.0, -4.0, 128.0, 128.0]), tags! {})],
            ),
            ((Z13_TILES / 2, Z13_TILES / 2, 13), vec![feature(new_line_string(&[64.0, 64.0, 192.0, 192.0]), tags! {})]),
        ]),
        &results.tiles,
    );
}

#[test]
fn test_line_string_degenerate_when_unscaled() {
    let x1 = 0.5 + Z12_WIDTH / 2.0;
    let y1 = 0.5 + Z12_WIDTH / 2.0;
    let (lng1, lat1) = lat_lon(x1, y1);
    let (lng2, lat2) = lat_lon(x1 + Z12_WIDTH / 4096.0 / 3.0, y1 + Z12_WIDTH / 4096.0 / 3.0);
    let results = run_with_reader_features(
        &[("threads", "1")],
        vec![new_reader_feature(new_line_string(&[lng1, lat1, lng2, lat2]), tags! {"attr" => "value"})],
        |_, features| {
            features.line("layer").set_zoom_range(12, 12).set_min_pixel_size(0.0).set_buffer_pixels(4.0);
        },
    )
    .unwrap();
    assert_submap(&tile_map([]), &results.tiles);
}

#[test]
fn test_num_points_attr() {
    let x1 = 0.5 + Z14_WIDTH / 2.0;
    let y1 = 0.5 + Z14_WIDTH / 2.0 - Z14_WIDTH / 2.0;
    let x2 = x1 + Z14_WIDTH;
    let y2 = y1 + Z14_WIDTH + Z14_WIDTH / 2.0;
    let x3 = x2 + Z14_WIDTH;
    let y3 = y2 + Z14_WIDTH;
    let (lng1, lat1) = lat_lon(x1, y1);
    let (lng2, lat2) = lat_lon(x2, y2);
    let (lng3, lat3) = lat_lon(x3, y3);
    let results = run_with_reader_features(
        &[("threads", "1")],
        vec![new_reader_feature(new_line_string(&[lng1, lat1, lng2, lat2, lng3, lat3]), tags! {"attr" => "value"})],
        |_, features| {
            features.line("layer").set_zoom_range(13, 14).set_buffer_pixels(4.0).set_num_points_attr("_numpoints");
        },
    )
    .unwrap();
    assert_submap(
        &tile_map([(
            (Z14_TILES / 2 + 2, Z14_TILES / 2 + 2, 14),
            vec![feature(new_line_string(&[-4.0, -4.0, 128.0, 128.0]), tags! {"_numpoints" => 3i64})],
        )]),
        &results.tiles,
    );
}

fn line_collections() -> Vec<Vec<SimpleFeature>> {
    let x1 = 0.5 + Z14_WIDTH / 2.0;
    let y1 = 0.5 + Z14_WIDTH / 2.0;
    let (lng1, lat1) = lat_lon(x1, y1);
    let (lng2, lat2) = lat_lon(x1 + Z14_WIDTH, y1 + Z14_WIDTH);
    let a = || new_line_string(&[lng1, lat1, lng2, lat2]);
    let b = || new_line_string(&[lng2, lat2, lng1, lat1]);
    vec![
        // simple multiline
        vec![new_reader_feature(new_multi_line_string(vec![a(), b()]), tags! {"attr" => "value"})],
        // one collection with several lines
        vec![new_reader_feature(gc(vec![a().into(), b().into()]), tags! {"attr" => "value"})],
        // nested collections, i.e. several collection, each with one line, in one collection
        vec![new_reader_feature(gc(vec![gc(vec![a().into()]), gc(vec![b().into()])]), tags! {"attr" => "value"})],
    ]
}

#[test]
fn test_multi_line_string() {
    for lines in line_collections() {
        let results = run_with_reader_features(&[("threads", "1")], lines, |_, features| {
            features.line("layer").set_zoom_range(13, 14).set_buffer_pixels(4.0);
        })
        .unwrap();
        let mls = |a: f64, b: f64| {
            new_multi_line_string(vec![new_line_string(&[a, a, b, b]), new_line_string(&[b, b, a, a])])
        };
        assert_submap(
            &tile_map([
                ((Z14_TILES / 2, Z14_TILES / 2, 14), vec![feature(mls(128.0, 260.0), tags! {})]),
                ((Z14_TILES / 2 + 1, Z14_TILES / 2 + 1, 14), vec![feature(mls(-4.0, 128.0), tags! {})]),
                ((Z13_TILES / 2, Z13_TILES / 2, 13), vec![feature(mls(64.0, 192.0), tags! {})]),
            ]),
            &results.tiles,
        );
    }
}

#[test]
fn test_polygon_with_hole_spanning_multiple_tiles() {
    for any_geom in [false, true] {
        let outer_points = z14_coordinate_list(&[0.5, 0.5, 3.5, 0.5, 3.5, 2.5, 0.5, 2.5, 0.5, 0.5]);
        let inner_points = z14_coordinate_list(&[1.25, 1.25, 1.75, 1.25, 1.75, 1.75, 1.25, 1.75, 1.25, 1.25]);
        let results = run_with_reader_features(
            &[("threads", "1")],
            vec![new_reader_feature(new_polygon_with_holes(outer_points, vec![inner_points]), tags! {})],
            move |_, features| {
                let f = if any_geom { features.any_geometry("layer") } else { features.polygon("layer") };
                f.set_zoom_range(12, 14).set_buffer_pixels(4.0);
            },
        )
        .unwrap();
        let f = |g: geo_types::Polygon<f64>| vec![feature(g, tags! {})];
        assert_eq!(
            tile_map([
                // Z12
                (
                    (Z12_TILES / 2, Z12_TILES / 2, 12),
                    f(new_polygon_with_holes(
                        rectangle_coord_list_xy(32.0, 32.0, 256.0 - 32.0, 128.0 + 32.0),
                        vec![rectangle_coord_list(64.0 + 16.0, 128.0 - 16.0)], // hole
                    )),
                ),
                // Z13
                (
                    (Z13_TILES / 2, Z13_TILES / 2, 13),
                    f(new_polygon_with_holes(
                        rectangle_coord_list(64.0, 256.0 + 4.0),
                        vec![rectangle_coord_list(128.0 + 32.0, 256.0 - 32.0)], // hole
                    )),
                ),
                ((Z13_TILES / 2 + 1, Z13_TILES / 2, 13), f(rectangle_xy(-4.0, 64.0, 256.0 - 64.0, 256.0 + 4.0))),
                ((Z13_TILES / 2, Z13_TILES / 2 + 1, 13), f(rectangle_xy(64.0, -4.0, 256.0 + 4.0, 64.0))),
                ((Z13_TILES / 2 + 1, Z13_TILES / 2 + 1, 13), f(rectangle_xy(-4.0, -4.0, 256.0 - 64.0, 64.0))),
                // Z14 - row 1
                ((Z14_TILES / 2, Z14_TILES / 2, 14), f(tile_bottom_right(4.0))),
                ((Z14_TILES / 2 + 1, Z14_TILES / 2, 14), f(tile_bottom(4.0))),
                ((Z14_TILES / 2 + 2, Z14_TILES / 2, 14), f(tile_bottom(4.0))),
                ((Z14_TILES / 2 + 3, Z14_TILES / 2, 14), f(tile_bottom_left(4.0))),
                // Z14 - row 2
                ((Z14_TILES / 2, Z14_TILES / 2 + 1, 14), f(tile_right(4.0))),
                (
                    (Z14_TILES / 2 + 1, Z14_TILES / 2 + 1, 14),
                    f(new_polygon_with_holes(
                        tile_fill(4.0),
                        vec![new_coordinate_list(&[64.0, 64.0, 192.0, 64.0, 192.0, 192.0, 64.0, 192.0, 64.0, 64.0])],
                    )),
                ),
                ((Z14_TILES / 2 + 2, Z14_TILES / 2 + 1, 14), f(new_polygon_from_coords(tile_fill(4.0)))),
                ((Z14_TILES / 2 + 3, Z14_TILES / 2 + 1, 14), f(tile_left(4.0))),
                // Z14 - row 3
                ((Z14_TILES / 2, Z14_TILES / 2 + 2, 14), f(tile_top_right(4.0))),
                ((Z14_TILES / 2 + 1, Z14_TILES / 2 + 2, 14), f(tile_top(4.0))),
                ((Z14_TILES / 2 + 2, Z14_TILES / 2 + 2, 14), f(tile_top(4.0))),
                ((Z14_TILES / 2 + 3, Z14_TILES / 2 + 2, 14), f(tile_top_left(4.0))),
            ]),
            results.tiles,
            "any_geom={any_geom}"
        );
    }
}

fn run_fill_test(zoom: u32) {
    let outer_points = z14_coordinate_list(&[-2.0, -2.0, 2.0, -2.0, 2.0, 2.0, -2.0, 2.0, -2.0, -2.0]);
    let maxzoom = zoom.to_string();
    let results = run_with_reader_features(
        &[("threads", "1"), ("maxzoom", &maxzoom)],
        vec![new_reader_feature(new_polygon_from_coords(outer_points), tags! {})],
        move |_, features| {
            features.polygon("layer").set_zoom_range(zoom, zoom).set_buffer_pixels(4.0);
        },
    )
    .unwrap();
    let tiles = 1u32 << zoom;
    assert_eq!(
        Some(&vec![feature(new_polygon_from_coords(tile_fill(4.0)), tags! {})]),
        results.tiles.get(&TileCoord::of_xyz(tiles / 2, tiles / 2, zoom))
    );
}

#[test]
fn test_z15_fill() {
    run_fill_test(15);
}

#[test]
fn test_z16_fill() {
    run_fill_test(16);
}

#[test]
fn test_z16_high_tile_id_sort_order() {
    // Verify that z16 tiles with high IDs (where FeatureGroup.encodeKey sets bit 63) sort correctly.
    // A point at z16 tile x=50000 produces an encoded tile ID > 2^32, so tile << 31 sets bit 63.
    let (lng16, lat16) = lat_lon((50_000.0 + 0.5) * Z16_WIDTH, (500.0 + 0.5) * Z16_WIDTH);
    // A point near the center for z0
    let (lng0, lat0) = lat_lon(0.5, 0.5);
    let results = run_with_reader_features(
        &[("threads", "1"), ("maxzoom", "16")],
        vec![
            new_reader_feature(new_point(lng16, lat16), tags! {"name" => "z16point"}),
            new_reader_feature(new_point(lng0, lat0), tags! {"name" => "z0point"}),
        ],
        |_, features| {
            features.point("layer").set_zoom_range(0, 16).inherit_attr_from_source("name");
        },
    )
    .unwrap();
    let z0_tile = TileCoord::of_xyz(0, 0, 0);
    let z16_tile = TileCoord::of_xyz(50_000, 500, 16);
    assert!(results.tiles.contains_key(&z0_tile), "z0 tile should be in output");
    assert!(results.tiles.contains_key(&z16_tile), "z16 tile should be in output");
    // results.tiles is ordered by TileCoord, verify z16 comes after z0
    let tile_coords: Vec<&TileCoord> = results.tiles.keys().collect();
    let pos = |t: &TileCoord| tile_coords.iter().position(|c| *c == t).unwrap();
    assert!(pos(&z0_tile) < pos(&z16_tile), "z0 tile must appear before z16 high tile in output order");
}
