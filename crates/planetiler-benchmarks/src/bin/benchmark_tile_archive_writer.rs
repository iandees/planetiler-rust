//! Benchmark for the archive (tile encoding + writing) stage: `TileArchiveWriter.writeOutput`.
//!
//! Synthesizes a deterministic workload (one large "water" polygon plus N points / lines / small polygons inside a
//! Switzerland-sized bounding box with minzooms spread over z4-z14), renders it into a disk-backed `FeatureGroup`,
//! sorts it, then times `write_output` into an mbtiles file several times.
//!
//! The equivalent Java program is `bench/java/BenchmarkTileArchiveWriter.java` (identical workload and settings).
//!
//! Usage: `cargo run --release -p planetiler-benchmarks --bin benchmark_tile_archive_writer -- [features] [rounds]
//! [tmpdir]`

// see the `mimalloc` feature in Cargo.toml
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use geo_types::{Coord, Geometry, LineString, Point, Polygon};
use planetiler_core::archive::tile_archive_config::ArchiveOptions;
use planetiler_core::archive::tile_archive_metadata::TileArchiveMetadata;
use planetiler_core::archive::tile_archive_writer;
use planetiler_core::archive::writeable_tile_archive::WriteableTileArchive;
use planetiler_core::collection::feature_group::FeatureGroup;
use planetiler_core::config::arguments::Arguments;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::{self, FeatureCollector};
use planetiler_core::geo::tile_order::TileOrder;
use planetiler_core::mbtiles::mbtiles::Mbtiles;
use planetiler_core::profile::Profile;
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::render::feature_renderer::FeatureRenderer;
use planetiler_core::stats::process_info;
use planetiler_core::stats::stats::{InMemoryStats, StatsRef};
use planetiler_core::util::file_utils::DiskBacked;
use planetiler_core::value::{Tags, Value};

/// Deterministic 64-bit LCG (same constants and arithmetic as the Java version).
struct Rng(u64);

impl Rng {
    fn next_double(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    fn next_int(&mut self, bound: i64) -> i64 {
        (self.next_double() * bound as f64) as i64
    }
}

const KINDS: [&str; 10] =
    ["residential", "park", "water", "forest", "farm", "school", "retail", "industrial", "grass", "wood"];
const MIN_LON: f64 = 5.9;
const MIN_LAT: f64 = 45.8;
const WIDTH: f64 = 4.6;
const HEIGHT: f64 = 2.0;

fn tags(t: i64, minzoom: i64, rng: &mut Rng) -> Tags {
    let mut tags = Tags::default();
    tags.insert("t".into(), Value::Int(t));
    tags.insert("minzoom".into(), Value::Int(minzoom));
    tags.insert("kind".into(), Value::str(KINDS[rng.next_int(10) as usize]));
    tags.insert("rank".into(), Value::Int(rng.next_int(20)));
    tags
}

fn workload(n: usize) -> Vec<SimpleFeature> {
    let mut rng = Rng(42);
    let mut result = Vec::with_capacity(n + 1);
    let water = Polygon::new(
        LineString::new(vec![
            Coord { x: MIN_LON, y: MIN_LAT },
            Coord { x: MIN_LON + WIDTH, y: MIN_LAT },
            Coord { x: MIN_LON + WIDTH, y: MIN_LAT + HEIGHT },
            Coord { x: MIN_LON, y: MIN_LAT + HEIGHT },
            Coord { x: MIN_LON, y: MIN_LAT },
        ]),
        vec![],
    );
    let water_tags = tags(0, 0, &mut rng);
    result.push(SimpleFeature::create_tags_with_id(Geometry::Polygon(water), water_tags, 1));
    for i in 0..n {
        let u = i % 10;
        let id = i as i64 + 2;
        if u < 5 {
            let p = Point::new(MIN_LON + rng.next_double() * WIDTH, MIN_LAT + rng.next_double() * HEIGHT);
            let minzoom = 10 + rng.next_int(5);
            let mut tags = tags(1, minzoom, &mut rng);
            tags.insert("name".into(), Value::str(format!("poi {}", i % 1000)));
            result.push(SimpleFeature::create_tags_with_id(Geometry::Point(p), tags, id));
        } else if u < 8 {
            let mut x = MIN_LON + rng.next_double() * WIDTH;
            let mut y = MIN_LAT + rng.next_double() * HEIGHT;
            let mut coords = Vec::with_capacity(6);
            for _ in 0..6 {
                coords.push(Coord { x, y });
                x += (rng.next_double() * 2.0 - 1.0) * 0.002;
                y += (rng.next_double() * 2.0 - 1.0) * 0.002;
            }
            let minzoom = 6 + rng.next_int(9);
            let tags = tags(2, minzoom, &mut rng);
            result.push(SimpleFeature::create_tags_with_id(Geometry::LineString(LineString::new(coords)), tags, id));
        } else {
            let cx = MIN_LON + rng.next_double() * WIDTH;
            let cy = MIN_LAT + rng.next_double() * HEIGHT;
            let r = 0.0002 + rng.next_double() * 0.0018;
            let mut coords = Vec::with_capacity(9);
            for j in 0..8 {
                let a = std::f64::consts::PI * 2.0 * j as f64 / 8.0;
                let rr = r * (0.75 + rng.next_double() * 0.5);
                coords.push(Coord { x: cx + rr * a.cos(), y: cy + rr * a.sin() });
            }
            coords.push(coords[0]);
            let minzoom = 8 + rng.next_int(7);
            let tags = tags(3, minzoom, &mut rng);
            result.push(SimpleFeature::create_tags_with_id(
                Geometry::Polygon(Polygon::new(LineString::new(coords), vec![])),
                tags,
                id,
            ));
        }
    }
    result
}

struct BenchProfile;

impl Profile for BenchProfile {
    fn process_feature(&self, source: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        let t = source.get_long("t");
        let minzoom = source.get_long("minzoom") as u32;
        let feature = match t {
            0 => features.polygon("water"),
            1 => features.point("poi"),
            2 => features.line("road"),
            _ => features.polygon("building"),
        };
        feature.set_zoom_range(minzoom, 14).inherit_attr_from_source("kind").inherit_attr_from_source("rank");
        if t == 1 {
            feature.inherit_attr_from_source("name");
        }
    }

    fn name(&self) -> String {
        "bench".to_string()
    }
}

struct FileSize(PathBuf);

impl DiskBacked for FileSize {
    fn disk_usage_bytes(&self) -> u64 {
        std::fs::metadata(&self.0).map(|m| m.len()).unwrap_or(0)
    }
}

fn cpu() -> Duration {
    process_info::get_process_cpu_time().unwrap_or_default()
}

fn render(
    features: &[SimpleFeature],
    group: &FeatureGroup,
    profile: &dyn Profile,
    config: &Arc<PlanetilerConfig>,
    stats: &StatsRef,
) {
    let threads = config.threads.max(1);
    let chunk = features.len().div_ceil(threads);
    std::thread::scope(|scope| {
        for part in features.chunks(chunk.max(1)) {
            scope.spawn(move || {
                let factory = feature_collector::Factory::new(config.clone(), stats.clone());
                let renderer = FeatureRenderer::new(config.clone(), stats.clone());
                let mut writer = group.feature_writer().expect("writer");
                for source in part {
                    let mut collector = factory.get(source);
                    profile.process_feature(source, &mut collector);
                    for feature in collector.iter() {
                        renderer.accept(feature, &mut |rendered| writer.write(&rendered).expect("write"));
                    }
                }
                writer.close().expect("close");
            });
        }
    });
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).map(|s| s.parse()).transpose()?.unwrap_or(2_000_000);
    let rounds: usize = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(3);
    let tmp = match args.get(3) {
        Some(dir) => tempfile::tempdir_in(dir)?,
        None => tempfile::tempdir()?,
    };
    let config =
        Arc::new(PlanetilerConfig::from(&Arguments::of_pairs([("tmpdir", tmp.path().to_string_lossy().to_string())]))?);
    let stats: StatsRef = InMemoryStats::arc();
    let profile: Arc<dyn Profile> = Arc::new(BenchProfile);

    let start = Instant::now();
    let features = workload(n);
    println!("generate: {} features in {:.2}s", features.len(), start.elapsed().as_secs_f64());

    let mut group = FeatureGroup::new_disk_backed(
        TileOrder::Tms,
        &tmp.path().join("feature.db"),
        profile.clone(),
        config.clone(),
        stats.clone(),
        false,
    )?;
    let (start, cpu_start) = (Instant::now(), cpu());
    render(&features, &group, &*profile, &config, &stats);
    drop(features);
    println!(
        "render: {} rendered features in {:.2}s wall {:.2}s cpu",
        group.num_features_written(),
        start.elapsed().as_secs_f64(),
        (cpu() - cpu_start).as_secs_f64()
    );
    let (start, cpu_start) = (Instant::now(), cpu());
    group.prepare()?;
    println!("sort: {:.2}s wall {:.2}s cpu", start.elapsed().as_secs_f64(), (cpu() - cpu_start).as_secs_f64());

    let metadata = TileArchiveMetadata::from_profile(&*profile, &config);
    for round in 0..rounds {
        let path = tmp.path().join(format!("output{round}.mbtiles"));
        let mut archive =
            Mbtiles::new_write_to_file_database(&path, &ArchiveOptions::from_arguments(config.arguments.clone()))?;
        let (start, cpu_start) = (Instant::now(), cpu());
        tile_archive_writer::write_output(
            &mut group,
            &mut archive,
            Arc::new(FileSize(path.clone())),
            &metadata,
            None,
            &config,
            &stats,
        )?;
        archive.close()?;
        drop(archive);
        let (wall, cpu_time) = (start.elapsed(), cpu() - cpu_start);
        let tiles: i64 =
            Mbtiles::new_read_only_database(&path)?
                .connection()
                .query_row("select count(*) from tiles", [], |r| r.get(0))?;
        println!(
            "archive round {round}: {:.2}s wall {:.2}s cpu, {tiles} tiles, {} bytes",
            wall.as_secs_f64(),
            cpu_time.as_secs_f64(),
            file_size(&path)
        );
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}
