//! Runs `OsmReader` pass 1 + pass 2 over an `.osm.pbf` file and reports wall/CPU time per pass.
//!
//! Java equivalent: `bench/java/BenchmarkOsmReader.java` (same profiles, same storage defaults).
//!
//! Arguments (`--key=value`, `-`/`_` interchangeable; anything else is passed to `PlanetilerConfig`, e.g.
//! `--threads=8 --nodemap_type=array --nodemap_storage=ram`):
//! - `osm_path` (default `../planetiler-bench/switzerland-latest.osm.pbf`; also accepted as a bare `*.pbf` argument)
//! - `profile` (default `null`): `null` emits nothing, `emit` emits a z14 point/line/polygon for every tagged
//!   element (nodes -> point, closed ways/multipolygons -> polygon, other ways -> line) into a disk-backed
//!   `FeatureGroup`
//! - `iterations` (default 1)
//! - `tmpdir` (default `data/tmp/benchmark_osm_reader`)

use std::path::PathBuf;
use std::sync::Arc;

use planetiler_core::collection::feature_group::FeatureGroup;
use planetiler_core::collection::{long_long_map, long_long_multimap};
use planetiler_core::config::arguments::Arguments;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::geo::tile_order::TileOrder;
use planetiler_core::profile::{NullProfile, Profile};
use planetiler_core::reader::osm::osm_input_file::OsmInputFile;
use planetiler_core::reader::osm::osm_reader::OsmReader;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::stats::process_time::ProcessTime;
use planetiler_core::stats::stats::InMemoryStats;

/// Emits one z14 feature per tagged element.
struct EmitProfile;

impl Profile for EmitProfile {
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if source_feature.tags().is_empty() {
            return;
        }
        if source_feature.is_point() {
            features.point("point").set_min_zoom(14);
        } else if source_feature.can_be_polygon() {
            features.polygon("polygon").set_min_zoom(14);
        } else if source_feature.can_be_line() {
            features.line("line").set_min_zoom(14);
        }
    }

    fn name(&self) -> String {
        "emit".to_string()
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_max_level(tracing::Level::INFO).init();
    let raw: Vec<String> = std::env::args()
        .skip(1)
        .map(|a| if a.ends_with(".pbf") && !a.contains('=') { format!("--osm_path={a}") } else { a })
        .collect();
    let arguments = Arguments::from_args(&raw);
    let path = arguments.get_arg_or("osm_path", "../planetiler-bench/switzerland-latest.osm.pbf");
    let profile_name = arguments.get_arg_or("profile", "null");
    let iterations: usize = arguments.get_arg_or("iterations", "1").parse()?;
    let tmp = PathBuf::from(arguments.get_arg_or("tmpdir", "data/tmp/benchmark_osm_reader"));
    let config = Arc::new(PlanetilerConfig::from(&arguments)?);
    eprintln!(
        "osm_path={path} profile={profile_name} threads={} process_threads={} write_threads={} nodemap={}/{} lazy={}",
        config.threads,
        config.feature_process_threads,
        config.feature_write_threads,
        config.node_map_type,
        config.node_map_storage,
        config.osm_lazy_reads
    );
    let profile: Arc<dyn Profile> = match profile_name.as_str() {
        "null" => Arc::new(NullProfile),
        "emit" => Arc::new(EmitProfile),
        other => anyhow::bail!("unknown profile {other}"),
    };

    for _ in 0..iterations {
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        let stats = InMemoryStats::arc();
        let input = OsmInputFile::new(&path, config.osm_lazy_reads);
        let node_locations = long_long_map::from_config(
            &config.node_map_type,
            &config.node_map_storage,
            &tmp.join("node.db"),
            config.node_map_madvise,
        )?;
        let multipolygons = long_long_multimap::new_replaceable_multimap_from_config(
            &config.multipolygon_geometry_storage,
            &tmp.join("multipolygon.db"),
            config.multipolygon_geometry_madvise,
        )?;
        let group = Arc::new(FeatureGroup::new_disk_backed(
            TileOrder::Tms,
            &tmp.join("feature.db"),
            profile.clone(),
            config.clone(),
            stats.clone(),
            false,
        )?);
        let reader = OsmReader::new(
            "osm",
            input.get()?,
            Arc::from(node_locations),
            Box::new(multipolygons),
            profile.clone(),
            stats.clone(),
        );

        let start = ProcessTime::now();
        reader.pass1(&config)?;
        let pass1 = ProcessTime::now().minus(&start);
        let start = ProcessTime::now();
        reader.pass2(&group, &config)?;
        let pass2 = ProcessTime::now().minus(&start);
        reader.close();

        eprintln!(
            "RESULT pass1: {pass1} ({:.3}s wall {:.3}s cpu) pass2: {pass2} ({:.3}s wall {:.3}s cpu) features={} data_errors={:?}",
            pass1.wall.as_secs_f64(),
            pass1.cpu.unwrap_or_default().as_secs_f64(),
            pass2.wall.as_secs_f64(),
            pass2.cpu.unwrap_or_default().as_secs_f64(),
            group.num_features_written(),
            stats.data_errors(),
        );
        drop(group);
        let _ = std::fs::remove_dir_all(&tmp);
    }
    Ok(())
}
