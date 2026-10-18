//! Port of `BenchmarkLineSplitter.java`: prints the milliseconds taken to extract random sub-lines from arcs of 10,
//! 100 and 1,000 points when the splitter is reused, recreated each time, or queried with cacheable ratios.
//!
//! Note: `LineSplitter::new` takes ownership of the line, so the "!reused" case also clones the line each iteration
//! (Java just wraps the existing reference).
//!
//! Run with `cargo run --release -p planetiler-benchmarks --bin benchmark_line_splitter`.

// see the `mimalloc` feature in Cargo.toml
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::hint::black_box;
use std::time::Instant;

use geo_types::{Coord, LineString};
use planetiler_core::geo::line_splitter::LineSplitter;

/// Small splitmix64 PRNG standing in for `ThreadLocalRandom`.
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos());
        Rng(seed as u64)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Uniform double in `[origin, bound)`.
    fn next_double(&mut self, origin: f64, bound: f64) -> f64 {
        let unit = (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64);
        let r = origin + unit * (bound - origin);
        if r >= bound { origin } else { r }
    }

    /// Uniform int in `[origin, bound)`.
    fn next_int(&mut self, origin: i32, bound: i32) -> i32 {
        origin + (self.next_u64() % (bound - origin) as u64) as i32
    }
}

/// JTS `GeometricShapeFactory` with `numPoints=points`, `width=10` (height 0), `createArc(0, PI)`.
fn make_arc(points: usize) -> LineString<f64> {
    let (x_radius, y_radius) = (5.0, 0.0);
    let (centre_x, centre_y) = (5.0, 0.0);
    let ang_inc = std::f64::consts::PI / (points - 1) as f64;
    let snap = |v: f64| if v.abs() < 5e-16 { 0.0 } else { v };
    LineString(
        (0..points)
            .map(|i| {
                let ang = i as f64 * ang_inc;
                Coord { x: x_radius * snap(ang.cos()) + centre_x, y: y_radius * snap(ang.sin()) + centre_y }
            })
            .collect(),
    )
}

fn time_cacheable(points: usize, iters: usize) -> u128 {
    let shape = make_arc(points);
    let start = Instant::now();
    let splitter = LineSplitter::new(shape);
    let mut random = Rng::new();
    for _ in 0..iters {
        let a = random.next_int(0, 90);
        let b = random.next_int(a + 2, 100);
        black_box(splitter.get(a as f64 / 100.0, b as f64 / 100.0));
    }
    start.elapsed().as_millis()
}

fn time_reused(points: usize, iters: usize) -> u128 {
    let shape = make_arc(points);
    let start = Instant::now();
    let splitter = LineSplitter::new(shape);
    let mut random = Rng::new();
    for _ in 0..iters {
        let a = random.next_double(0.0, 1.0);
        let b = random.next_double(a, 1.0);
        black_box(splitter.get(a, b));
    }
    start.elapsed().as_millis()
}

fn time_not_reused(points: usize, iters: usize) -> u128 {
    let shape = make_arc(points);
    let start = Instant::now();
    let mut random = Rng::new();
    for _ in 0..iters {
        let splitter = LineSplitter::new(shape.clone());
        let a = random.next_double(0.0, 1.0);
        let b = random.next_double(a, 1.0);
        black_box(splitter.get(a, b));
    }
    start.elapsed().as_millis()
}

fn main() {
    for _ in 0..10 {
        eprintln!(
            "reused:\t{}\t{}\t{}\t!reused\t{}\t{}\t{}\tcacheable\t{}\t{}\t{}",
            time_reused(10, 1_000_000),
            time_reused(100, 100_000),
            time_reused(1_000, 10_000),
            time_not_reused(10, 1_000_000),
            time_not_reused(100, 100_000),
            time_not_reused(1_000, 10_000),
            time_cacheable(10, 1_000_000),
            time_cacheable(100, 100_000),
            time_cacheable(1_000, 10_000),
        );
    }
}
