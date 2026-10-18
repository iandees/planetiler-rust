//! Port of the parts of `PlanetilerTests.java` that run in-memory with `SimpleFeature` inputs (the
//! `runWithReaderFeatures` style): SimpleFeature -> Profile -> FeatureCollector -> FeatureRenderer -> FeatureGroup ->
//! TileArchiveWriter -> in-memory mbtiles -> decoded tiles.
//!
//! TODO(port): tests that need `OsmReader` (`runWithOsmElements`: testOsmPoint, testOsmPointSkipPass1,
//! testExceptionWhileProcessingOsm, testOsmLine, testOsmLineOrPolygon, testSplitAmbiguousOsmPolygon,
//! testSplitOsmLineWithLoop, testOsmMultipolygon, testOsmLineInRelation, testPreprocessOsmNodesAndWays,
//! testOsmProfileFinish, testOsmProfileFinishForwardingProfile, testBadRelation, testIssue496BaseballMultipolygon,
//! testIssue509LenaDelta, testHandleProfileException) or the `Planetiler` orchestrator (testPlanetilerRunner*,
//! testPlanetilerMemoryCheck*, testDefaultLanguages, testReuseFeatureDb) are not ported yet.

use std::sync::Arc;

use planetiler_core::geo::geo_utils::{get_world_lat, get_world_lon};
use planetiler_core::geo::test_utils::*;
use planetiler_core::geo::tile_coord::TileCoord;
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::tags;
use planetiler_core::testing::*;
use planetiler_core::value::Tags;

const Z16_TILES: u32 = 1 << 16;
const Z15_TILES: u32 = 1 << 15;
const Z15_WIDTH: f64 = 1.0 / Z15_TILES as f64;
const Z14_TILES: u32 = 1 << 14;
const Z14_WIDTH: f64 = 1.0 / Z14_TILES as f64;
const Z13_TILES: u32 = 1 << 13;
const Z4_TILES: u32 = 1 << 4;

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

fn new_reader_feature(geometry: impl Into<geo_types::Geometry<f64>>, attrs: Tags) -> SimpleFeature {
    SimpleFeature::create_tags(geometry, attrs)
}

fn assert_same_json(expected: &str, actual: Option<&String>) {
    let expected: serde_json::Value = serde_json::from_str(expected).unwrap();
    let actual: serde_json::Value = serde_json::from_str(actual.expect("missing json")).unwrap();
    assert_eq!(expected, actual);
}

#[test]
fn test_metadata_but_no_points() {
    let results = run_with_reader_features(&[("threads", "1")], vec![], |_, _| {}).unwrap();
    assert!(results.tiles.is_empty());
    assert_string_submap(
        &[
            ("name", TEST_PROFILE_NAME),
            ("description", TEST_PROFILE_DESCRIPTION),
            ("attribution", TEST_PROFILE_ATTRIBUTION),
            ("version", TEST_PROFILE_VERSION),
            ("type", "baselayer"),
            ("format", "pbf"),
            ("minzoom", "0"),
            ("maxzoom", "14"),
            ("center", "0,0"),
            ("bounds", "-180,-85.05113,180,85.05113"),
        ],
        &results.metadata,
    );
    assert!(results.metadata.contains_key("planetiler:version"));
    assert_same_json("[]", results.metadata.get("vector_layers"));
}

#[test]
fn test_override_metadata() {
    let results = run_with_reader_features(
        &[
            ("archive_name", "override_name"),
            ("archive_description", "override_description"),
            ("archive_attribution", "override_attribution"),
            ("archive_version", "override_version"),
            ("archive_type", "override_type"),
        ],
        vec![],
        |_, _| {},
    )
    .unwrap();
    assert!(results.tiles.is_empty());
    assert_string_submap(
        &[
            ("name", "override_name"),
            ("description", "override_description"),
            ("attribution", "override_attribution"),
            ("version", "override_version"),
            ("type", "override_type"),
        ],
        &results.metadata,
    );
}

#[test]
fn test_single_point() {
    for any_geom in [false, true] {
        let x = 0.5 + Z15_WIDTH / 4.0;
        let y = 0.5 + Z15_WIDTH / 4.0;
        let lat = get_world_lat(y);
        let lng = get_world_lon(x);
        let results = run_with_reader_features(
            &[("threads", "1"), ("maxzoom", "16"), ("tile-format", "mvt")],
            vec![new_reader_feature(new_point(lng, lat), tags! {"attr" => "value"})],
            move |_, features| {
                let f = if any_geom { features.any_geometry("layer") } else { features.point("layer") };
                f.set_zoom_range(13, 16).set_attr("name", "name value").inherit_attr_from_source("attr");
            },
        )
        .unwrap();
        let attrs = || tags! {"attr" => "value", "name" => "name value"};
        assert_submap(
            &tile_map([
                ((Z16_TILES / 2, Z16_TILES / 2, 16), vec![feature(new_point(128.0, 128.0), attrs())]),
                ((Z15_TILES / 2, Z15_TILES / 2, 15), vec![feature(new_point(64.0, 64.0), attrs())]),
                ((Z14_TILES / 2, Z14_TILES / 2, 14), vec![feature(new_point(32.0, 32.0), attrs())]),
                ((Z13_TILES / 2, Z13_TILES / 2, 13), vec![feature(new_point(16.0, 16.0), attrs())]),
            ]),
            &results.tiles,
        );
        assert_same_json(
            r#"[{"id": "layer", "fields": {"name": "String", "attr": "String"}, "minzoom": 13, "maxzoom": 16}]"#,
            results.metadata.get("vector_layers"),
        );
    }
}

#[test]
fn test_single_point_mlt_unsupported() {
    // TODO(port): MLT output is not supported yet; Java runs testSinglePoint with tile-format=mlt too.
    let result = run_with_reader_features(
        &[("threads", "1"), ("tile-format", "mlt")],
        vec![new_reader_feature(new_point(0.0, 0.0), tags! {})],
        |_, features| {
            features.point("layer");
        },
    );
    assert!(result.is_err());
}

#[test]
fn test_empty_tile_after_post_processing_omitted_from_archive() {
    let results = run_with_reader_features_and_post_process(
        &[("threads", "1"), ("maxzoom", "6")],
        vec![new_reader_feature(world_polygon(), tags! {}), new_reader_feature(new_point(0.5, 0.5), tags! {})],
        |_, features| {
            features.any_geometry("layer");
        },
        |_, _, features| {
            features.clear();
            Ok(())
        },
    )
    .unwrap();
    assert!(results.tiles.is_empty());
    assert_eq!(0, results.tile_data_count);
}

#[test]
fn test_single_empty_tile_omitted_from_archive() {
    let run_omitting = |tile_to_remove: Option<TileCoord>| {
        let features =
            vec![new_reader_feature(world_polygon(), tags! {}), new_reader_feature(new_point(0.5, 0.5), tags! {})];
        let profile = TestProfile::process_source_features(|_, f| {
            f.any_geometry("layer");
        })
        .with_tile_post_process(move |tile, layers| {
            if Some(tile) == tile_to_remove {
                layers.clear();
            }
            Ok(())
        });
        run(&[("threads", "1"), ("maxzoom", "6")], |ctx| process_reader_features(ctx, &features), Arc::new(profile))
            .unwrap()
    };
    let to_omit = TileCoord::of_xyz(54, 63, 6);
    let result1 = run_omitting(None);
    let result2 = run_omitting(Some(to_omit));
    assert_eq!(result1.tiles.len() - 1, result2.tiles.len());
    let mut expected = result1.tiles.clone();
    expected.remove(&to_omit);
    assert_eq!(expected, result2.tiles);
}

#[test]
fn test_full_world_polygon() {
    let results = run_with_reader_features(
        &[("threads", "1")],
        vec![new_reader_feature(world_polygon(), tags! {})],
        |_, features| {
            features.polygon("layer").set_zoom_range(0, 6).set_buffer_pixels(4.0);
        },
    )
    .unwrap();
    assert_eq!(5461, results.tiles.len());
    // spot-check one filled tile
    let tile = &results.tiles[&TileCoord::of_xyz(Z4_TILES / 2, Z4_TILES / 2, 4)];
    assert_eq!(vec![feature(rectangle(-4.0, 256.0 + 4.0), tags! {})], *tile);
}

#[test]
fn test_skip_fill() {
    let results = run_with_reader_features(
        &[("threads", "1"), ("skip-filled-tiles", "true")],
        vec![new_reader_feature(world_polygon(), tags! {})],
        |_, features| {
            features.polygon("layer").set_zoom_range(0, 6).set_buffer_pixels(4.0);
        },
    )
    .unwrap();
    assert_eq!(481, results.tiles.len());
    // spot-check one filled tile does not exist
    assert!(!results.tiles.contains_key(&TileCoord::of_xyz(Z4_TILES / 2, Z4_TILES / 2, 4)));
}

#[test]
fn test_full_world_polygon_multithreaded_compact() {
    // not in Java: exercise multiple encoder threads, batching, and tile deduplication in compact mbtiles
    let results = run_with_reader_features(
        &[("threads", "4"), ("compact-db", "true")],
        vec![new_reader_feature(world_polygon(), tags! {})],
        |_, features| {
            features.polygon("layer").set_zoom_range(0, 7).set_buffer_pixels(4.0);
        },
    )
    .unwrap();
    assert_eq!(5461 + 4usize.pow(7), results.tiles.len());
    assert!(results.tile_data_count < 1000, "{}", results.tile_data_count);
}
