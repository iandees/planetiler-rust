//! Port of `BenchmarkMbtilesWriter.java`: writes synthetic tiles (a mix of distinct and duplicated tile data) into a
//! temporary mbtiles file several times and prints tile-writes-per-second statistics.
//!
//! Arguments (same as Java): `--bench_tiles_to_write=1000000 --bench_repetitions=10 --bench_distinct_tiles=10
//! --bench_distinct_tile_data_size=800 --bench_dupe_tile_data_size=100 --bench_dupe_spread=10`, plus any mbtiles
//! options like `--compact=false`, `--no_index=true` or sqlite pragmas.

// see the `mimalloc` feature in Cargo.toml
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::time::Instant;

use indexmap::IndexMap;
use planetiler_core::archive::tile_archive_config::ArchiveOptions;
use planetiler_core::archive::tile_encoding_result::TileEncodingResult;
use planetiler_core::archive::writeable_tile_archive::{TileWriter, WriteableTileArchive};
use planetiler_core::geo::tile_coord::TileCoord;
use planetiler_core::mbtiles::mbtiles::Mbtiles;

/// Port of `java.util.Random` so the workload matches Java exactly.
struct JavaRandom {
    seed: u64,
}

impl JavaRandom {
    const MULTIPLIER: u64 = 0x5DEECE66D;
    const MASK: u64 = (1 << 48) - 1;

    fn new(seed: u64) -> JavaRandom {
        JavaRandom { seed: (seed ^ Self::MULTIPLIER) & Self::MASK }
    }

    fn next(&mut self, bits: u32) -> i32 {
        self.seed = (self.seed.wrapping_mul(Self::MULTIPLIER).wrapping_add(0xB)) & Self::MASK;
        (self.seed >> (48 - bits)) as i32
    }

    fn next_bytes(&mut self, bytes: &mut [u8]) {
        let mut i = 0;
        while i < bytes.len() {
            let mut rnd = self.next(32);
            let n = (bytes.len() - i).min(4);
            for _ in 0..n {
                bytes[i] = rnd as u8;
                i += 1;
                rnd >>= 8;
            }
        }
    }
}

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

fn create_filled_byte_array(len: usize) -> Vec<u8> {
    let mut data = vec![0u8; len];
    JavaRandom::new(0).next_bytes(&mut data);
    data
}

fn java_round(v: f64) -> i64 {
    (v + 0.5).floor() as i64
}

fn write_tiles(
    writer: &mut dyn TileWriter,
    tiles_to_write: i64,
    distinct_tiles_in_percent: i64,
    distinct_tile_data: &[u8],
    dupe_tile_data: &[u8],
    dupe_spread_in_percent: i64,
) -> anyhow::Result<()> {
    let dupes_to_write = java_round(tiles_to_write as f64 * (100 - distinct_tiles_in_percent) as f64 / 100.0);
    let dupe_hash_mod = java_round(dupes_to_write as f64 * dupe_spread_in_percent as f64 / 100.0);
    let mut tiles_written = 0i64;
    let mut dupe_counter = 0i64;
    for z in 0..=14u32 {
        let max_coord = 1u32 << z;
        for x in 0..max_coord {
            for y in (0..max_coord).rev() {
                let coord = TileCoord::of_xyz(x, y, z);
                let to_write = if tiles_written % 100 < distinct_tiles_in_percent {
                    TileEncodingResult::new(coord, distinct_tile_data.to_vec(), None)
                } else {
                    dupe_counter += 1;
                    let hash = if dupe_hash_mod == 0 { 0 } else { dupe_counter % dupe_hash_mod };
                    TileEncodingResult::new(coord, dupe_tile_data.to_vec(), Some(hash as u64))
                };
                writer.write(to_write)?;
                tiles_written += 1;
                if tiles_written >= tiles_to_write {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = parse_args();
    let tiles_to_write = get_int(&args, "bench_tiles_to_write", 1_000_000);
    let repetitions = get_int(&args, "bench_repetitions", 10);
    // select count(distinct(tile_data_id)) * 100.0 / count(*) from tiles_shallow => ~8% (Australia)
    let distinct_tiles_in_percent = get_int(&args, "bench_distinct_tiles", 10);
    // avg size of distinct tiles => ~785 (Australia)
    let distinct_tile_data_size = get_int(&args, "bench_distinct_tile_data_size", 800);
    // avg size of duplicated tiles => ~93 (Australia)
    let dupe_tile_data_size = get_int(&args, "bench_dupe_tile_data_size", 100);
    // => ~0.17% (Australia)
    let dupe_spread_in_percent = get_int(&args, "bench_dupe_spread", 10);

    let distinct_tile_data = create_filled_byte_array(distinct_tile_data_size as usize);
    let dupe_tile_data = create_filled_byte_array(dupe_tile_data_size as usize);
    let options = ArchiveOptions::of_map(args.clone());

    let mut stats: Vec<f64> = Vec::new();
    for _ in 0..repetitions {
        let tmp = tempfile::Builder::new().prefix("planetiler").suffix(".mbtiles").tempfile()?;
        let output_path = tmp.path().to_path_buf();
        drop(tmp);
        {
            let mbtiles = Mbtiles::new_write_to_file_database(&output_path, &options)?;
            if mbtiles.skip_index_creation() {
                mbtiles.create_tables_without_indexes()?;
            } else {
                mbtiles.create_tables_with_indexes()?;
            }
            let mut writer = mbtiles.new_tile_writer()?;
            let sw = Instant::now();
            write_tiles(
                writer.as_mut(),
                tiles_to_write,
                distinct_tiles_in_percent,
                &distinct_tile_data,
                &dupe_tile_data,
                dupe_spread_in_percent,
            )?;
            // Java stops the stopwatch before the try-with-resources closes (flushes) the writer
            let seconds = sw.elapsed().as_secs_f64();
            writer.close()?;
            stats.push(tiles_to_write as f64 / seconds);
        }
        std::fs::remove_file(&output_path)?;
    }

    let count = stats.len();
    let sum: f64 = stats.iter().sum();
    let min = stats.iter().copied().fold(f64::INFINITY, f64::min);
    let max = stats.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let avg = if count > 0 { sum / count as f64 } else { 0.0 };
    println!(
        "tileWritesPerSecondsStats: DoubleSummaryStatistics{{count={count}, sum={sum:.6}, min={min:.6}, \
         average={avg:.6}, max={max:.6}}}"
    );
    Ok(())
}
