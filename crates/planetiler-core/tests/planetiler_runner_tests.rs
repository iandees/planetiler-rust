//! Ports of the `PlanetilerTests` cases that drive the [`Planetiler`] orchestrator end-to-end
//! (`testPlanetilerRunner*`, `testPlanetilerMemoryCheck*`, `testReuseFeatureDb`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use planetiler_core::archive::readable_tile_archive::ReadableTileArchive;
use planetiler_core::archive::tile_archive_config::ArchiveOptions;
use planetiler_core::archive::tile_compression::TileCompression;
use planetiler_core::config::arguments::Arguments;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::files::readable_files_archive::ReadableFilesArchive;
use planetiler_core::geo::tile_coord::TileCoord;
use planetiler_core::mbtiles::mbtiles::Mbtiles;
use planetiler_core::planetiler::Planetiler;
use planetiler_core::pmtiles::readable_pmtiles::ReadablePmtiles;
use planetiler_core::profile::Profile;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::testing::{ComparableFeature, get_tile_map_with_compression};
use planetiler_core::util::gzip::gunzip;
use planetiler_core::value::Value;

fn resource(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources").join(name)
}

/// The profile used by `testPlanetilerRunner`.
struct BuildingsAndPlaces;

impl Profile for BuildingsAndPlaces {
    fn process_feature(&self, source: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if source.can_be_polygon() && source.has_tag_str("building", "yes") {
            features.polygon("building").set_zoom_range(0, 14).set_min_pixel_size(1.0);
        } else if source.is_point() && source.has_tag("place") {
            features.point("place").set_zoom_range(0, 14).inherit_attr_from_source("name");
        }
    }
}

fn split_args(args: &str) -> Vec<String> {
    args.split_whitespace().map(str::to_string).collect()
}

fn read_tiles(
    format: &str,
    output: &Path,
    compression: TileCompression,
) -> (BTreeMap<TileCoord, Vec<ComparableFeature>>, Option<indexmap::IndexMap<String, String>>) {
    let archive: Box<dyn ReadableTileArchive> = match format {
        "mbtiles" => Box::new(Mbtiles::new_read_only_database(output).unwrap()),
        "pmtiles" => Box::new(ReadablePmtiles::new_read_from_file(output).unwrap()),
        "files" => Box::new(ReadableFilesArchive::new_reader(output, &ArchiveOptions::empty()).unwrap()),
        other => panic!("unsupported {other}"),
    };
    let tiles = get_tile_map_with_compression(archive.as_ref(), compression).unwrap();
    let metadata = archive.metadata().unwrap().map(|m| m.to_map());
    (tiles, metadata)
}

/// `testPlanetilerRunner` with only the OSM source.
///
/// TODO(port): add the natural earth, shapefile, geopackage and geojson sources once the simple readers are wired
/// into the runner, and the csv/tsv/json/proto output formats once an in-memory stream archive reader is ported.
fn run_osm(args: &str) {
    let temp = tempfile::tempdir().unwrap();
    let temp_dir = temp.path();
    let arg_parsed = Arguments::from_args(&split_args(args));
    let original_osm = resource(&arg_parsed.get_string("osm-test-path", "osm-test-path", "monaco-latest.osm.pbf"));
    let temp_osm = temp_dir.join("monaco-temp.osm.pbf");
    std::fs::copy(original_osm, &temp_osm).unwrap();
    let compression =
        if args.contains("tile-compression=none") { TileCompression::None } else { TileCompression::Gzip };
    let format = arg_parsed.get_string("output-format", "", "mbtiles");
    let (output_path, output_uri) = if format == "files" {
        let p = temp_dir.join("output");
        let uri = format!("{}?format=files", p.display());
        (p, uri)
    } else {
        let p = temp_dir.join(format!("output.{format}"));
        (p.clone(), p.display().to_string())
    };

    let mut all_args = vec![format!("--tmpdir={}", temp_dir.join("data").display())];
    all_args.extend(split_args(args));
    let mut planetiler = Planetiler::create(Arguments::from_args(&all_args)).unwrap();
    planetiler
        .set_profile(BuildingsAndPlaces)
        .add_osm_source("osm", &temp_osm)
        .unwrap()
        .set_output(&output_uri)
        .unwrap();
    planetiler.run().unwrap();

    if args.contains("free-osm-after-read") {
        assert!(!temp_osm.exists());
    }

    let (tiles, metadata) = read_tiles(&format, &output_path, compression);
    let mut features = 0;
    let mut features_with_ids = 0;
    let mut monaco_points = 0;
    for tile in tiles.values() {
        for feature in tile {
            planetiler_core::geo::test_utils::validate_geometry(feature.geometry.geom());
            features += 1;
            if feature.attrs.get("name") == Some(&Value::str("Monaco")) {
                monaco_points += 1;
            }
            if feature.id.is_some_and(|id| id != 0) {
                features_with_ids += 1;
            }
        }
    }
    assert_eq!(30, monaco_points, "monaco points");
    let expected_features = if args.contains("max-point-buffer=1") { 2311 } else { 2313 };
    assert_eq!(22, tiles.len(), "num tiles");
    assert_eq!(expected_features, features, "num features");
    let expected_ids = if args.contains("exclude-ids") { 0 } else { expected_features };
    assert_eq!(expected_ids, features_with_ids);

    if let Some(metadata) = metadata.filter(|_| format != "files") {
        assert_eq!(Some("pbf"), metadata.get("format").map(String::as_str));
        assert_eq!(
            Some(env!("CARGO_PKG_VERSION")),
            metadata.get("planetiler:version").map(String::as_str),
            "{metadata:?}"
        );
        assert_eq!(
            Some("2021-04-21T20:21:46Z"),
            metadata.get("planetiler:osm:osmosisreplicationtime").map(String::as_str)
        );
        assert_eq!(Some("2947"), metadata.get("planetiler:osm:osmosisreplicationseq").map(String::as_str));
        assert_eq!(
            Some("http://download.geofabrik.de/europe/monaco-updates"),
            metadata.get("planetiler:osm:osmosisreplicationurl").map(String::as_str)
        );
    }

    let mut layerstats = output_path.as_os_str().to_owned();
    layerstats.push(".layerstats.tsv.gz");
    let layerstats = PathBuf::from(layerstats);
    if args.contains("--output-layerstats") {
        let data = gunzip(&std::fs::read(&layerstats).unwrap()).unwrap();
        let text = String::from_utf8(data).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            "z\tx\ty\thilbert\tarchived_tile_bytes\tlayer\tlayer_bytes\tlayer_features\tlayer_geometries\tlayer_attr_bytes\tlayer_attr_keys\tlayer_attr_values",
            lines[0]
        );
        assert_eq!(33, lines.len());
        for line in &lines[1..] {
            let cols: Vec<&str> = line.split('\t').collect();
            let (z, x, y): (u32, u32, u32) =
                (cols[0].parse().unwrap(), cols[1].parse().unwrap(), cols[2].parse().unwrap());
            let hilbert: i64 = cols[3].parse().unwrap();
            assert_eq!(hilbert, TileCoord::of_xyz(x, y, z).hilbert_encoded() as i64);
            assert!(z <= 14);
        }
    } else {
        assert!(!layerstats.exists());
    }
}

#[test]
fn test_planetiler_runner_default() {
    run_osm("");
}

#[test]
fn test_planetiler_runner_threads() {
    run_osm("--write-threads=2 --process-threads=2 --feature-read-threads=2 --threads=4");
}

#[test]
fn test_planetiler_runner_free_osm_after_read() {
    run_osm("--free-osm-after-read");
}

#[test]
fn test_planetiler_runner_compress_temp() {
    run_osm("--compress-temp");
}

#[test]
fn test_planetiler_runner_parse_node_bounds() {
    run_osm("--osm-parse-node-bounds");
}

#[test]
fn test_planetiler_runner_exclude_ids() {
    run_osm("--exclude-ids");
}

#[test]
fn test_planetiler_runner_pmtiles() {
    run_osm("--output-format=pmtiles");
}

#[test]
fn test_planetiler_runner_files() {
    run_osm("--output-format=files");
}

#[test]
fn test_planetiler_runner_no_compression() {
    run_osm("--tile-compression=none");
}

#[test]
fn test_planetiler_runner_gzip() {
    run_osm("--tile-compression=gzip");
}

#[test]
fn test_planetiler_runner_layerstats() {
    run_osm("--output-layerstats");
}

#[test]
fn test_planetiler_runner_max_point_buffer() {
    run_osm("--max-point-buffer=1");
}

#[test]
fn test_planetiler_runner_lz4() {
    run_osm("--osm-test-path=monaco-latest.lz4.osm.pbf");
}

#[test]
fn test_planetiler_runner_parallel_tmp_io() {
    run_osm("--parallel-tmp-io");
}

/// A profile that estimates huge resource requirements.
struct HugeEstimates {
    disk: bool,
    output: bool,
    ram: bool,
}

impl Profile for HugeEstimates {
    fn process_feature(&self, _source: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {}

    fn estimate_intermediate_disk_bytes(&self, _osm_file_size: u64) -> u64 {
        if self.disk { i64::MAX as u64 / 10 } else { 0 }
    }

    fn estimate_output_bytes(&self, _osm_file_size: u64) -> u64 {
        if self.output { i64::MAX as u64 / 10 } else { 0 }
    }

    fn estimate_ram_required(&self, _osm_file_size: u64) -> u64 {
        if self.ram { i64::MAX as u64 / 10 } else { 0 }
    }
}

/// `runWithProfile` (TODO(port): also add the natural earth, shapefile and geopackage sources like Java once the
/// simple readers are wired into the runner).
fn run_with_profile(temp_dir: &Path, profile: HugeEstimates, force: bool) -> anyhow::Result<()> {
    let mut planetiler = Planetiler::create(Arguments::of_pairs([
        ("tmpdir", temp_dir.display().to_string()),
        ("force", force.to_string()),
    ]))?;
    planetiler
        .set_profile(profile)
        .add_osm_source("osm", resource("monaco-latest.osm.pbf"))?
        .set_output(temp_dir.join("output.mbtiles"))?;
    planetiler.run()
}

#[test]
fn test_planetiler_memory_check() {
    let temp = tempfile::tempdir().unwrap();
    assert!(run_with_profile(temp.path(), HugeEstimates { disk: true, output: false, ram: false }, false).is_err());
    assert!(run_with_profile(temp.path(), HugeEstimates { disk: false, output: true, ram: false }, false).is_err());
    assert!(run_with_profile(temp.path(), HugeEstimates { disk: false, output: false, ram: true }, false).is_err());
}

#[test]
fn test_planetiler_memory_check_force() {
    let temp = tempfile::tempdir().unwrap();
    run_with_profile(temp.path(), HugeEstimates { disk: true, output: false, ram: false }, true).unwrap();
    run_with_profile(temp.path(), HugeEstimates { disk: false, output: true, ram: false }, true).unwrap();
    run_with_profile(temp.path(), HugeEstimates { disk: false, output: false, ram: true }, true).unwrap();
}

#[test]
fn test_output_exists_without_force_fails() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("output.mbtiles");
    std::fs::write(&output, b"").unwrap();
    let mut planetiler =
        Planetiler::create(Arguments::of_pairs([("tmpdir", temp.path().join("data").display().to_string())])).unwrap();
    planetiler.set_profile(BuildingsAndPlaces).add_osm_source("osm", resource("monaco-latest.osm.pbf")).unwrap();
    planetiler.set_output(&output).unwrap();
    let err = planetiler.run().unwrap_err().to_string();
    assert!(err.contains("already exists"), "{err}");
}
