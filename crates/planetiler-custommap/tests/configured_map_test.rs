//! Port of `ConfiguredMapTest.java`: end-to-end test that generates a map for Monaco from the bundled `owg_simple.yml`
//! sample and asserts that expected output features exist.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use geo_types::Geometry;
use planetiler_core::archive::readable_tile_archive::ReadableTileArchive;
use planetiler_core::geo::geometry_type::GeometryType;
use planetiler_core::mbtiles::mbtiles::Mbtiles;
use planetiler_core::tags;
use planetiler_core::util::gzip::gunzip;
use planetiler_core::value::Tags;
use planetiler_core::vector_tile;
use planetiler_custommap::configured_map_main;

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
        let db_path = dir.path().join("output.mbtiles");
        let schema = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/samples/owg_simple.yml");
        configured_map_main::main(&[
            "generate-custom".to_string(),
            // Use local data extracts instead of downloading
            format!("--schema={}", schema.display()),
            format!("--osm_path={}", core_resource("monaco-latest.osm.pbf").display()),
            format!("--water_polygons_path={}", core_resource("water-polygons-split-3857.zip").display()),
            // Override temp dir location
            format!("--tmp={}", dir.path().display()),
            // Override output location
            format!("--output={}", db_path.display()),
        ])
        .unwrap();
        Output { _dir: dir, db_path }
    })
}

fn mbtiles() -> Mbtiles {
    Mbtiles::new_read_only_database(&output().db_path).unwrap()
}

#[test]
fn test_metadata() {
    let metadata = mbtiles().metadata_table().get_all();
    let get = |k: &str| metadata.get(k).map(String::as_str);
    assert_eq!(Some("OWG Simple Schema"), get("name"));
    assert_eq!(Some("0"), get("minzoom"));
    assert_eq!(Some("14"), get("maxzoom"));
    assert_eq!(Some("overlay"), get("type"));
    assert_eq!(Some("pbf"), get("format"));
    assert_eq!(Some("7.40921,43.72335,7.44864,43.75169"), get("bounds"));
    assert_eq!(Some("7.42892,43.73752,14"), get("center"));
    assert_eq!(Some("1.0.0"), get("version"));
    assert!(get("description").unwrap().contains("Simple"));
    assert!(get("attribution").unwrap().contains("www.openstreetmap.org/copyright"));
}

#[test]
fn ensure_valid_geometries() {
    for tile in mbtiles().get_all_tiles().unwrap() {
        let tile = tile.unwrap();
        for feature in vector_tile::decode(&gunzip(&tile.bytes).unwrap()).unwrap() {
            planetiler_core::geo::test_utils::validate_geometry(&feature.geometry.decode().unwrap());
        }
    }
}

fn geometry_count(geometry: &Geometry<f64>, geometry_type: GeometryType) -> usize {
    match (geometry, geometry_type) {
        (Geometry::GeometryCollection(c), _) => c.iter().map(|g| geometry_count(g, geometry_type)).sum(),
        (Geometry::MultiPoint(m), GeometryType::Point) => m.0.len(),
        (Geometry::MultiLineString(m), GeometryType::Line) => m.0.len(),
        (Geometry::MultiPolygon(m), GeometryType::Polygon) => m.0.len(),
        (Geometry::Point(_), GeometryType::Point)
        | (Geometry::LineString(_), GeometryType::Line)
        | (Geometry::Polygon(_), GeometryType::Polygon) => 1,
        _ => 0,
    }
}

/// Java `TestUtils.assertMinFeatureCount` (via `Verify.getNumFeatures`).
#[track_caller]
fn assert_min_features(layer: &str, attrs: Tags, zoom: u32, expected: usize, geometry_type: GeometryType) {
    let mut num = 0;
    for tile in mbtiles().get_all_tiles().unwrap() {
        let tile = tile.unwrap();
        if tile.coord.z != zoom {
            continue;
        }
        for feature in vector_tile::decode(&gunzip(&tile.bytes).unwrap()).unwrap() {
            if feature.layer == layer && attrs.iter().all(|(k, v)| feature.tags.get(k) == Some(v)) {
                num += geometry_count(&feature.geometry.decode().unwrap(), geometry_type);
            }
        }
    }
    assert!(expected < num, "z{zoom} features in {layer}, expected at least {expected} got {num}");
}

#[test]
fn test_contains_ocean_polygons() {
    assert_min_features("water", tags! {"natural" => "water"}, 6, 1, GeometryType::Polygon);
}

#[test]
fn test_road() {
    assert_min_features("road", tags! {"highway" => "primary"}, 14, 200, GeometryType::Line);
}
