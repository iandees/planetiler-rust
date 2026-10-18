//! Port of `OpenMapTilesTest.java`: generates an entire map for the smallest openstreetmap extract available (Monaco)
//! and asserts that expected output features exist.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use geo::{Distance, Euclidean};
use geo_types::Geometry;
use planetiler_core::archive::readable_tile_archive::ReadableTileArchive;
use planetiler_core::config::arguments::Arguments;
use planetiler_core::geo::geometry_type::GeometryType;
use planetiler_core::geo::tile_coord::TileCoord;
use planetiler_core::mbtiles::mbtiles::Mbtiles;
use planetiler_core::tags;
use planetiler_core::util::gzip::gunzip;
use planetiler_core::value::Tags;
use planetiler_core::vector_tile;
use planetiler_openmaptiles::open_map_tiles_main;
use planetiler_openmaptiles::util::verify_monaco::{self, MONACO_BOUNDS};

fn core_resource(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../planetiler-core/tests/resources").join(name)
}

struct Output {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
}

/// Runs planetiler once for all tests in this file (Java `@BeforeAll`).
fn output() -> &'static Output {
    static OUTPUT: OnceLock<Output> = OnceLock::new();
    OUTPUT.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path();
        let db_path = tmp.join("output.mbtiles");
        let water_path = tmp.join("water");
        // extract first like Java
        planetiler_core::util::file_utils::unzip_file(&core_resource("water-polygons-split-3857.zip"), &water_path)
            .unwrap();
        let path = |p: &Path| p.display().to_string();
        open_map_tiles_main::run(&Arguments::of_pairs([
            // Override input source locations
            ("osm_path", path(&core_resource("monaco-latest.osm.pbf"))),
            ("natural_earth_path", path(&core_resource("natural_earth_vector.sqlite.zip"))),
            ("water_polygons_path", path(&water_path)),
            // no centerlines in monaco - so fake it out with an empty source
            ("lake_centerlines_path", path(&water_path)),
            // Override temp dir location
            ("tmpdir", path(&tmp.join("tmp"))),
            // Override output location
            ("mbtiles", path(&db_path)),
        ]))
        .unwrap();
        Output { _dir: dir, db_path }
    })
}

fn mbtiles() -> Mbtiles {
    Mbtiles::new_read_only_database(&output().db_path).unwrap()
}

#[track_caller]
fn assert_contains(needle: &str, haystack: Option<&String>) {
    let haystack = haystack.expect("missing value");
    assert!(haystack.contains(needle), "{haystack:?} does not contain {needle:?}");
}

#[test]
fn test_metadata() {
    let db = mbtiles();
    let metadata = db.metadata_table().get_all();
    assert_eq!(Some("OpenMapTiles"), metadata.get("name").map(String::as_str));
    assert_eq!(Some("0"), metadata.get("minzoom").map(String::as_str));
    assert_eq!(Some("14"), metadata.get("maxzoom").map(String::as_str));
    assert_eq!(Some("baselayer"), metadata.get("type").map(String::as_str));
    assert_eq!(Some("pbf"), metadata.get("format").map(String::as_str));
    assert_eq!(Some("7.40921,43.72335,7.44864,43.75169"), metadata.get("bounds").map(String::as_str));
    assert_eq!(Some("7.42892,43.73752,14"), metadata.get("center").map(String::as_str));
    assert_contains("openmaptiles.org", metadata.get("description"));
    assert_contains("openmaptiles.org", metadata.get("attribution"));
    assert_contains("www.openstreetmap.org/copyright", metadata.get("attribution"));
}

#[test]
fn ensure_valid_geometries() {
    let db = mbtiles();
    for tile in db.get_all_tiles().unwrap() {
        let tile = tile.unwrap();
        for feature in vector_tile::decode(&gunzip(&tile.bytes).unwrap()).unwrap() {
            planetiler_core::geo::test_utils::validate_geometry(&feature.geometry.decode().unwrap());
        }
    }
}

/// Java `TestUtils.assertFeatureNear`.
#[track_caller]
fn assert_feature_near(layer: &str, attrs: Tags, lng: f64, lat: f64, minzoom: u32, maxzoom: u32) {
    let db = mbtiles();
    let mut failures = vec![];
    'outer: for zoom in 0..=14 {
        let should_find = zoom >= minzoom && zoom <= maxzoom;
        let coord = TileCoord::around_lng_lat(lng, lat, zoom);
        let tile_point = Geometry::Point(coord.lng_lat_to_tile_coords(lng, lat).into());
        let features = match db.get_tile(coord.x, coord.y, coord.z).unwrap() {
            Some(bytes) => vector_tile::decode(&gunzip(&bytes).unwrap()).unwrap(),
            None => vec![],
        };
        let mut contained_in_layers = BTreeSet::new();
        let mut contained_in_layer_features = BTreeSet::new();
        for feature in features {
            let geometry = feature.geometry.decode().unwrap();
            if Euclidean.distance(&geometry, &tile_point) <= 2.0 {
                contained_in_layers.insert(feature.layer.clone());
                if feature.layer == layer {
                    contained_in_layer_features.insert(format!("{:?}", feature.tags));
                    if attrs.iter().all(|(k, v)| feature.tags.get(k) == Some(v)) {
                        if !should_find {
                            failures.push(format!("z{zoom} found feature but should not have"));
                        }
                        continue 'outer;
                    }
                }
            }
        }
        if should_find {
            failures.push(format!(
                "z{zoom} no feature was found in layers {contained_in_layers:?} features: {contained_in_layer_features:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[track_caller]
fn assert_num_features(layer: &str, attrs: Tags, zoom: u32, expected: usize, geometry_type: GeometryType) {
    let num = verify_monaco::get_num_features(&mbtiles(), layer, zoom, &attrs, &MONACO_BOUNDS, geometry_type).unwrap();
    assert_eq!(expected, num, "z{zoom} features in {layer}");
}

#[test]
fn test_contains_ocean_polygons() {
    assert_feature_near("water", tags! {"class" => "ocean"}, 7.4484, 43.70783, 0, 14);
}

#[test]
fn test_contains_country_name() {
    assert_feature_near(
        "place",
        tags! {"class" => "country", "iso_a2" => "MC", "name" => "Monaco"},
        7.42769,
        43.73235,
        2,
        14,
    );
}

#[test]
fn test_contains_suburb() {
    assert_feature_near("place", tags! {"name" => "Les Moneghetti", "class" => "suburb"}, 7.41746, 43.73638, 11, 14);
}

#[test]
fn test_contains_buildings() {
    assert_feature_near("building", tags! {}, 7.41919, 43.73401, 13, 14);
    assert_num_features("building", tags! {}, 14, 1316, GeometryType::Polygon);
    assert_num_features("building", tags! {}, 13, 196, GeometryType::Polygon);
}

#[test]
fn test_contains_housenumber() {
    assert_feature_near("housenumber", tags! {"housenumber" => "27"}, 7.42117, 43.73652, 14, 14);
    assert_num_features("housenumber", tags! {}, 14, 231, GeometryType::Point);
}

#[test]
fn test_boundary() {
    assert_feature_near(
        "boundary",
        tags! {"admin_level" => 2i64, "maritime" => 1i64, "disputed" => 0i64},
        7.41884,
        43.72396,
        4,
        14,
    );
}

#[test]
fn test_aeroway() {
    assert_num_features("aeroway", tags! {"class" => "heliport"}, 14, 1, GeometryType::Polygon);
    assert_num_features("aeroway", tags! {"class" => "helipad"}, 14, 11, GeometryType::Polygon);
}

#[test]
fn test_landcover() {
    assert_num_features("landcover", tags! {"class" => "grass", "subclass" => "park"}, 14, 20, GeometryType::Polygon);
    assert_num_features("landcover", tags! {"class" => "grass", "subclass" => "garden"}, 14, 33, GeometryType::Polygon);
}

#[test]
fn test_poi() {
    assert_num_features(
        "poi",
        tags! {"class" => "restaurant", "subclass" => "restaurant"},
        14,
        217,
        GeometryType::Point,
    );
    assert_num_features("poi", tags! {"class" => "art_gallery", "subclass" => "artwork"}, 14, 132, GeometryType::Point);
}

#[test]
fn test_landuse() {
    assert_num_features("landuse", tags! {"class" => "residential"}, 14, 8, GeometryType::Polygon);
    assert_num_features("landuse", tags! {"class" => "hospital"}, 14, 4, GeometryType::Polygon);
}

#[test]
fn test_transportation() {
    assert_num_features(
        "transportation",
        tags! {"class" => "path", "subclass" => "footway"},
        14,
        828,
        GeometryType::Line,
    );
    assert_num_features("transportation", tags! {"class" => "primary"}, 14, 249, GeometryType::Line);
}

#[test]
fn test_transportation_name() {
    assert_num_features(
        "transportation_name",
        tags! {"name" => "Boulevard du Larvotto", "class" => "primary"},
        14,
        9,
        GeometryType::Line,
    );
}

#[test]
fn test_waterway() {
    assert_num_features("waterway", tags! {"class" => "stream"}, 14, 6, GeometryType::Line);
}

#[test]
fn test_verify_checks() {
    let db = mbtiles();
    let verify = verify_monaco::verify(&db);
    let failures: Vec<String> = verify
        .results()
        .iter()
        .filter_map(|check| check.error.as_ref().map(|e| format!("{}: {e}", check.name)))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}
