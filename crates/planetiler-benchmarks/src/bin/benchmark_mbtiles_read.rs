//! Port of `BenchmarkMbtilesRead.java`: reads a random sample of tiles from one or more (copies of the same) mbtiles
//! files and prints read-operations-per-second statistics.
//!
//! Arguments (same as Java): `--bench_mbtiles=a.mbtiles,b.mbtiles --bench_repetitions=10
//! --bench_nr_tile_reads=500000 --bench_pre_warms=3`

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use indexmap::IndexMap;
use planetiler_core::archive::readable_tile_archive::ReadableTileArchive;
use planetiler_core::geo::tile_coord::TileCoord;
use planetiler_core::mbtiles::mbtiles::Mbtiles;
use tracing::info;

const SELECT_RANDOM_COORDS: &str = "select tile_column, tile_row, zoom_level from tiles order by random() limit ?";

/// Minimal port of `Arguments.fromArgs`: `--key=value`, `key=value`, `--key value` and `--flag`.
fn parse_args() -> IndexMap<String, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut result = IndexMap::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].trim_start_matches('-');
        if let Some((k, v)) = arg.split_once('=') {
            result.insert(k.replace('-', "_"), v.to_string());
        } else if args[i].starts_with("--") {
            match args.get(i + 1) {
                Some(next) if !next.starts_with("--") && !next.contains('=') => {
                    result.insert(arg.replace('-', "_"), next.clone());
                    i += 1;
                }
                _ => {
                    result.insert(arg.replace('-', "_"), "true".to_string());
                }
            }
        }
        i += 1;
    }
    result
}

fn get_int(args: &IndexMap<String, String>, key: &str, default: i64) -> i64 {
    args.get(key).map(|v| v.parse().unwrap_or_else(|_| panic!("invalid {key}: {v}"))).unwrap_or(default)
}

struct ReadResult {
    duration_secs: f64,
    coords_fetched_count: usize,
}

impl ReadResult {
    fn read_operations_per_second(&self) -> f64 {
        self.coords_fetched_count as f64 / self.duration_secs
    }
}

fn read_each_tile(coords_to_fetch: &[TileCoord], db_path: &Path) -> anyhow::Result<ReadResult> {
    let db = Mbtiles::new_read_only_database(db_path)?;
    db.get_tile(0, 0, 0)?; // trigger prepared statement creation
    let total_sw = Instant::now();
    for coord in coords_to_fetch {
        if db.get_tile_coord(coord)?.is_none() {
            anyhow::bail!("{coord} should exist in {}", db_path.display());
        }
    }
    Ok(ReadResult { duration_secs: total_sw.elapsed().as_secs_f64(), coords_fetched_count: coords_to_fetch.len() })
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = parse_args();
    let repetitions = get_int(&args, "bench_repetitions", 10);
    let nr_tile_reads = get_int(&args, "bench_nr_tile_reads", 500_000) as usize;
    let pre_warms = get_int(&args, "bench_pre_warms", 3);
    let mbtiles_paths: Vec<PathBuf> = args
        .get("bench_mbtiles")
        .map(|s| s.split(',').map(|p| PathBuf::from(p.trim())).filter(|p| !p.as_os_str().is_empty()).collect())
        .unwrap_or_default();
    if mbtiles_paths.is_empty() {
        anyhow::bail!("pass one or many paths to the same mbtiles file");
    }
    for p in &mbtiles_paths {
        if !p.is_file() {
            anyhow::bail!("{} does not exists", p.display());
        }
    }

    let mut random_coords: Vec<TileCoord> = Vec::new();
    while random_coords.len() < nr_tile_reads {
        let db = Mbtiles::new_read_only_database(&mbtiles_paths[0])?;
        let conn = db.connection();
        let mut stmt = conn.prepare(SELECT_RANDOM_COORDS)?;
        let rows = stmt.query_map([(nr_tile_reads - random_coords.len()) as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
        })?;
        let before = random_coords.len();
        for row in rows {
            let (x, y, z) = row?;
            random_coords.push(TileCoord::of_xyz(x as u32, ((1i64 << z) - 1 - y) as u32, z as u32));
        }
        if random_coords.len() == before {
            anyhow::bail!("no tiles in {}", mbtiles_paths[0].display());
        }
    }

    let mut avg_per_db: HashMap<PathBuf, f64> = HashMap::new();
    for db_path in &mbtiles_paths {
        info!("working on {}", db_path.display());
        for _ in 0..pre_warms {
            read_each_tile(&random_coords, db_path)?;
        }
        let mut results = Vec::new();
        for _ in 0..repetitions {
            results.push(read_each_tile(&random_coords, db_path)?.read_operations_per_second());
        }
        let count = results.len();
        let sum: f64 = results.iter().sum();
        let min = results.iter().copied().fold(f64::INFINITY, f64::min);
        let max = results.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let avg = if count > 0 { sum / count as f64 } else { 0.0 };
        info!(
            "readOperationsPerSecondStats: DoubleSummaryStatistics{{count={count}, sum={sum:.6}, min={min:.6}, \
             average={avg:.6}, max={max:.6}}}"
        );
        avg_per_db.insert(db_path.clone(), avg);
    }

    let mut keys_sorted: Vec<&PathBuf> = avg_per_db.keys().collect();
    keys_sorted.sort_by(|a, b| avg_per_db[*a].total_cmp(&avg_per_db[*b]));
    info!("diffs");
    for i in 0..keys_sorted.len().saturating_sub(1) {
        for db1 in &keys_sorted[i + 1..] {
            let db0 = keys_sorted[i];
            let diff = avg_per_db[*db1] * 100.0 / avg_per_db[db0] - 100.0;
            info!("\"{}\" to \"{}\": avg read operations per second improved by {diff}%", db0.display(), db1.display());
        }
    }
    Ok(())
}
