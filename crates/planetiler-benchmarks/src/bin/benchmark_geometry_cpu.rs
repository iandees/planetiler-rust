//! Load-insensitive companion to `benchmark_line_merge` / `benchmark_simplify`: runs the same workloads and reports
//! single-threaded CPU time per operation (`CLOCK_THREAD_CPUTIME_ID`) and heap allocations per operation (counting
//! global allocator) instead of wall-clock rates. Useful for A/B comparisons of two builds while other processes are
//! running.
//!
//! Run with `cargo run --release -p planetiler-benchmarks --bin benchmark_geometry_cpu [-- <cpu seconds per cell>]`.
//! Merge-lines resources are read from `$PLANETILER_REF/planetiler-core/src/test/resources/mergelines` (defaults to
//! `../planetiler-ref/planetiler` next to the workspace).

use std::alloc::{GlobalAlloc, Layout};

// count allocations on top of the same allocator the other binaries use (see the `mimalloc` feature in Cargo.toml)
#[cfg(feature = "mimalloc")]
use mimalloc::MiMalloc as Inner;
#[cfg(not(feature = "mimalloc"))]
use std::alloc::System as Inner;
use std::hint::black_box;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use geo::CoordsIter;
use geo_types::{Coord, Geometry, LineString, MultiLineString, Polygon};
use planetiler_core::geo::douglas_peucker_simplifier;
use planetiler_core::geo::geometry_transformer::GeometryTransformer;
use planetiler_core::geo::vw_simplifier::VWSimplifier;
use planetiler_core::util::loop_line_merger::LoopLineMerger;

struct CountingAlloc;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { Inner.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { Inner.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { Inner.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn thread_cpu_nanos() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Returns (cpu ns per op, allocations per op) after running `f` for about `budget_ns` of CPU time.
fn measure(budget_ns: u64, f: &mut dyn FnMut() -> usize) -> (f64, f64) {
    // warm up
    for _ in 0..3 {
        black_box(f());
    }
    let start = thread_cpu_nanos();
    let allocs = ALLOCS.load(Ordering::Relaxed);
    let mut n = 0u64;
    let mut batch = 1u64;
    loop {
        for _ in 0..batch {
            black_box(f());
        }
        n += batch;
        let elapsed = thread_cpu_nanos() - start;
        if elapsed >= budget_ns {
            let a = ALLOCS.load(Ordering::Relaxed) - allocs;
            return (elapsed as f64 / n as f64, a as f64 / n as f64);
        }
        batch = (batch * 2).min(1 << 16);
    }
}

fn fmt_ns(ns: f64) -> String {
    if ns >= 1e6 {
        format!("{:.2}ms", ns / 1e6)
    } else if ns >= 1e3 {
        format!("{:.2}us", ns / 1e3)
    } else {
        format!("{ns:.0}ns")
    }
}

fn resource(file: &str) -> Geometry<f64> {
    let reference = std::env::var("PLANETILER_REF")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../../planetiler-ref/planetiler").to_string());
    let mut path = PathBuf::from(&reference).join("planetiler-core/src/test/resources/mergelines").join(file);
    if !path.exists() {
        path = PathBuf::from(
            "/Users/ian.dees/Workspace/planetiler-ref/planetiler/planetiler-core/src/test/resources/mergelines",
        )
        .join(file);
    }
    let compressed = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(&compressed[..]).read_to_end(&mut bytes).unwrap();
    planetiler_core::geo::geo_utils::read_wkb(&bytes).unwrap()
}

/// Same synthetic lines as `BenchmarkLineMerge.makeLines`.
fn make_lines(lines: usize, parts: usize) -> Geometry<f64> {
    let mut result = Vec::with_capacity(lines);
    let mut idx = 0.0;
    for _ in 0..lines {
        let mut coords = Vec::with_capacity(parts);
        for _ in 0..parts {
            coords.push(Coord { x: idx, y: idx });
            idx += 0.5;
        }
        result.push(LineString(coords));
    }
    Geometry::MultiLineString(MultiLineString(result))
}

/// Same circles as `BenchmarkSimplify` (JTS `GeometricShapeFactory.createCircle`).
fn make_circle(parts: usize) -> Geometry<f64> {
    let snap = |v: f64| if v.abs() < 5e-16 { 0.0 } else { v };
    let mut pts: Vec<Coord<f64>> = (0..parts)
        .map(|i| {
            let ang = i as f64 * (2.0 * std::f64::consts::PI / parts as f64);
            Coord { x: 5.0 * snap(ang.cos()), y: 5.0 * snap(ang.sin()) }
        })
        .collect();
    pts.push(pts[0]);
    Geometry::Polygon(Polygon::new(LineString(pts), vec![]))
}

fn row(name: &str, budget: u64, inputs: &[Geometry<f64>], f: &dyn Fn(&Geometry<f64>) -> usize) {
    let cells: Vec<String> = inputs
        .iter()
        .map(|g| {
            let (ns, allocs) = measure(budget, &mut || f(g));
            format!("{:>9} {:>7.1}a", fmt_ns(ns), allocs)
        })
        .collect();
    println!("{name:>11}\t{}", cells.join("\t"));
}

fn main() {
    let secs: f64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0.3);
    let budget = (secs * 1e9) as u64;

    let merge_inputs = [
        resource("mergelines_200433_lines.wkb.gz"),
        resource("mergelines_239823_lines.wkb.gz"),
        resource("mergelines_1759_point_line.wkb.gz"),
        make_lines(50, 2),
        make_lines(10, 10),
        make_lines(2, 50),
    ];
    println!("LoopLineMerger: cpu time and allocations per op (200433, 239823, 1759_point_line, 50x2, 10x10, 2x50)");
    for min_length in [0.0, 0.1, 20.0] {
        row(&format!("loop({min_length})"), budget, &merge_inputs, &|g| {
            let mut lm = LoopLineMerger::new();
            lm.set_min_length(min_length)
                .set_stub_min_length(min_length)
                .set_loop_min_length(min_length)
                .set_tolerance(1.0)
                .set_merge_strokes(true);
            lm.add(g).get_merged_line_strings().len()
        });
    }

    let circles: Vec<Geometry<f64>> = [2, 10, 50, 100, 10_000].iter().map(|n| make_circle(*n)).collect();
    println!("Simplify: cpu time and allocations per op (circles of 2, 10, 50, 100, 10000 points)");
    for tolerance in [0.1, 1.0, 20.0] {
        row(&format!("DP({tolerance})"), budget, &circles, &|g| {
            douglas_peucker_simplifier::simplify(g, tolerance).coords_count()
        });
    }
    for tolerance in [0.0, 0.1, 1.0, 20.0] {
        row(&format!("VW({tolerance})"), budget, &circles, &|g| {
            VWSimplifier::new().set_tolerance(tolerance).set_weight(0.7).transform(g).coords_count()
        });
    }
}
