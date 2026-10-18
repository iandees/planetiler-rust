//! Port of `BenchmarkSimplify.java`: measures simplifier throughput (operations/second) on circles with 2, 10, 50,
//! 100 and 10,000 points.
//!
//! The Java benchmark also measures JTS's own topology-preserving `VWSimplifier`; here those rows (`geo VW(..)`) use
//! the `geo` crate's `SimplifyVw` with the same area threshold JTS uses (`tolerance^2`), without JTS's validity repair.
//!
//! Run with `cargo run --release -p planetiler-benchmarks --bin benchmark_simplify`.

use std::hint::black_box;
use std::time::{Duration, Instant};

use geo::{CoordsIter, SimplifyVw};
use geo_types::{Coord, Geometry, LineString, Polygon};
use planetiler_core::geo::douglas_peucker_simplifier;
use planetiler_core::geo::geometry_transformer::GeometryTransformer;
use planetiler_core::geo::vw_simplifier::VWSimplifier;

/// JTS `Angle.cosSnap` / `sinSnap`.
fn snap(v: f64) -> f64 {
    if v.abs() < 5e-16 { 0.0 } else { v }
}

/// JTS `GeometricShapeFactory` with `numPoints=parts`, `centre=(0,0)`, `size=10`, `createCircle()`.
fn make_lines(parts: usize) -> Geometry<f64> {
    let radius = 5.0;
    let mut pts: Vec<Coord<f64>> = (0..parts)
        .map(|i| {
            let ang = i as f64 * (2.0 * std::f64::consts::PI / parts as f64);
            Coord { x: radius * snap(ang.cos()), y: radius * snap(ang.sin()) }
        })
        .collect();
    pts.push(pts[0]);
    Geometry::Polygon(Polygon::new(LineString(pts), vec![]))
}

/// planetiler `Format.numeric(value, pad=true)`.
fn format_numeric(value: i64) -> String {
    let s = if value < 0 {
        "-".to_string()
    } else if value < 1000 {
        value.to_string()
    } else {
        let suffixes = [(1_000i64, "k"), (1_000_000, "M"), (1_000_000_000, "B"), (1_000_000_000_000, "T")];
        let (divide_by, suffix) = suffixes.iter().rev().find(|(d, _)| value >= *d).copied().unwrap();
        let truncated = value / (divide_by / 10);
        if truncated < 100 && truncated % 10 != 0 {
            format!("{:.1}{suffix}", truncated as f64 / 10.0)
        } else {
            format!("{}{suffix}", truncated / 10)
        }
    };
    format!("{s:>4}")
}

struct Bench {
    num_lines: usize,
}

impl Bench {
    fn time_per_sec(&mut self, geometry: &Geometry<f64>, f: &dyn Fn(&Geometry<f64>) -> Geometry<f64>) -> String {
        let start = Instant::now();
        let end = start + Duration::from_secs(1);
        let mut num = 0u64;
        while Instant::now() < end {
            self.num_lines += black_box(f(black_box(geometry))).coords_count();
            num += 1;
        }
        format_numeric((num as f64 / start.elapsed().as_secs_f64()).round() as i64)
    }

    fn time(&mut self, name: &str, f: &dyn Fn(&Geometry<f64>) -> Geometry<f64>) {
        let cols: Vec<String> =
            [2, 10, 50, 100, 10_000].iter().map(|parts| self.time_per_sec(&make_lines(*parts), f)).collect();
        eprintln!("{name}\t{}", cols.join("\t"));
    }
}

fn geo_vw(geom: &Geometry<f64>, tolerance: f64) -> Geometry<f64> {
    match geom {
        Geometry::Polygon(p) => Geometry::Polygon(p.simplify_vw(tolerance * tolerance)),
        other => other.clone(),
    }
}

fn main() {
    let mut bench = Bench { num_lines: 0 };
    for _ in 0..10 {
        bench.time("    DP(0.1)", &|g| douglas_peucker_simplifier::simplify(g, 0.1));
        bench.time("      DP(1)", &|g| douglas_peucker_simplifier::simplify(g, 1.0));
        bench.time("     DP(20)", &|g| douglas_peucker_simplifier::simplify(g, 20.0));
        bench.time("  geo VW(0)", &|g| geo_vw(g, 0.01));
        bench.time("geo VW(0.1)", &|g| geo_vw(g, 0.1));
        bench.time("  geo VW(1)", &|g| geo_vw(g, 1.0));
        bench.time(" geo VW(20)", &|g| geo_vw(g, 20.0));
        bench.time("      VW(0)", &|g| VWSimplifier::new().set_tolerance(0.0).set_weight(0.7).transform(g));
        bench.time("    VW(0.1)", &|g| VWSimplifier::new().set_tolerance(0.1).set_weight(0.7).transform(g));
        bench.time("      VW(1)", &|g| VWSimplifier::new().set_tolerance(1.0).set_weight(0.7).transform(g));
        bench.time("     VW(20)", &|g| VWSimplifier::new().set_tolerance(20.0).set_weight(0.7).transform(g));
    }
    eprintln!("{}", bench.num_lines);
}
