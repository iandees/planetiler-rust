//! Single-threaded CPU time of `FeatureMerge.mergeNearbyPolygons` (buffer / cascaded union / unbuffer) on the
//! building tiles used by the `merge_*_matches_java` tests (z13 Boston and Jakarta buildings, OpenMapTiles'
//! building merge arguments). Reports thread CPU time per merged tile (`CLOCK_THREAD_CPUTIME_ID`), so it is usable
//! for A/B comparisons while other processes are running.
//!
//! Run with `cargo run --release -p planetiler-benchmarks --bin benchmark_feature_merge [-- <cpu seconds per tile>]`.

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::hint::black_box;
use std::path::PathBuf;

use planetiler_core::archive::readable_tile_archive::ReadableTileArchive;
use planetiler_core::feature_merge::merge_nearby_polygons_with;
use planetiler_core::mbtiles::mbtiles::Mbtiles;
use planetiler_core::stats::stats::InMemoryStats;
use planetiler_core::util::gzip::gunzip;
use planetiler_core::vector_tile::{self, Feature};

fn thread_cpu_nanos() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn read_tile(file: &str, x: u32, y: u32, z: u32) -> Vec<Feature> {
    let path = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../planetiler-core/tests/resources")).join(file);
    let db = Mbtiles::new_read_only_database(&path).unwrap();
    let tile_data = db.get_tile(x, y, z).unwrap().unwrap();
    vector_tile::decode(&gunzip(&tile_data).unwrap()).unwrap()
}

fn main() {
    let secs: f64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(2.0);
    let budget = (secs * 1e9) as u64;
    let tiles = [
        ("boston 2477/3028", read_tile("bostonbuildings.mbtiles", 2477, 3028, 13)),
        ("boston 2481/3026", read_tile("bostonbuildings.mbtiles", 2481, 3026, 13)),
        ("boston 2479/3028", read_tile("bostonbuildings.mbtiles", 2479, 3028, 13)),
        ("jakarta 6527/4240", read_tile("jakartabuildings.mbtiles", 6527, 4240, 13)),
    ];
    let stats = InMemoryStats::new();
    let mut total = 0.0;
    for (name, features) in &tiles {
        let run = || {
            let merged = merge_nearby_polygons_with(features.clone(), 4.0, 0.0, 0.5, 0.5, &stats, None).unwrap();
            black_box(merged.len())
        };
        run();
        let start = thread_cpu_nanos();
        let mut n = 0u64;
        while thread_cpu_nanos() - start < budget {
            run();
            n += 1;
        }
        let ms = (thread_cpu_nanos() - start) as f64 / n as f64 / 1e6;
        total += ms;
        println!("{name:>18}: {ms:8.2} ms cpu/tile ({n} runs)");
    }
    println!("{:>18}: {total:8.2} ms cpu", "sum");
}
