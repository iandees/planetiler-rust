//! Port of `BenchmarkTileCoord.java`: encodes and decodes every tile coordinate from z0 to z14, 3 times, printing
//! throughput to stderr.

// see the `mimalloc` feature in Cargo.toml
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use planetiler_core::geo::tile_coord::TileCoord;
use planetiler_core::stats::timer::Timer;
use planetiler_core::util::format;

fn main() {
    for _ in 0..3 {
        let timer = Timer::start();
        let mut num: i64 = 0;
        for z in 0..=14u32 {
            let max = 1u32 << z;
            for x in 0..max {
                for y in 0..max {
                    let encoded = TileCoord::encode(x, y, z);
                    let decoded = TileCoord::decode(std::hint::black_box(encoded)).encoded;
                    // make sure we use the result so it doesn't get optimized out
                    if encoded != decoded {
                        eprintln!("Error on {z}/{x}/{y}");
                    }
                    num += 1;
                }
            }
        }
        let wall = timer.stop().elapsed().wall;
        eprintln!(
            "z0-z14 took {} ({}/s)",
            format::duration(wall),
            format::numeric(num as f64 / wall.as_secs_f64(), false)
        );
    }
}
