//! Micro-benchmark for vector tile geometry + tile encoding.
//!
//! The equivalent Java program is `bench/java/BenchmarkVectorTile.java`; both use an identical deterministic workload
//! (1000 polygons, 1000 lines, 1000 points with a few attributes) so their results can be compared directly.
//!
//! Usage: `cargo run --release -p planetiler-benchmarks --bin benchmark_vector_tile -- [iterations]`

use std::hint::black_box;
use std::time::Instant;

use geo_types::{Coord, Geometry, LineString, Point, Polygon};
use planetiler_core::value::{Tags, Value};
use planetiler_core::vector_tile::{Feature, VectorTile, encode_geometry};

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

fn workload() -> Vec<(&'static str, Geometry<f64>, Tags)> {
    let mut rng = Rng(42);
    let mut result = Vec::with_capacity(3000);
    for i in 0..1000 {
        let cx = rng.next_double() * 256.0;
        let cy = rng.next_double() * 256.0;
        let r = 1.0 + rng.next_double() * 4.0;
        let mut coords = Vec::with_capacity(9);
        for j in 0..8 {
            let a = std::f64::consts::PI * 2.0 * j as f64 / 8.0;
            let rr = r * (0.75 + rng.next_double() * 0.5);
            coords.push(Coord { x: cx + rr * a.cos(), y: cy + rr * a.sin() });
        }
        coords.push(coords[0]);
        let mut tags = Tags::new();
        tags.insert("kind".into(), Value::str(KINDS[rng.next_int(10) as usize]));
        tags.insert("rank".into(), Value::Int(rng.next_int(20)));
        tags.insert("area".into(), Value::Double((rng.next_double() * 1000.0).floor() / 10.0));
        tags.insert("id".into(), Value::Int(i));
        result.push(("polygons", Geometry::Polygon(Polygon::new(LineString::new(coords), vec![])), tags));
    }
    for i in 0..1000 {
        let mut x = rng.next_double() * 256.0;
        let mut y = rng.next_double() * 256.0;
        let mut coords = Vec::with_capacity(10);
        for _ in 0..10 {
            coords.push(Coord { x, y });
            x += rng.next_double() * 8.0 - 4.0;
            y += rng.next_double() * 8.0 - 4.0;
        }
        let mut tags = Tags::new();
        tags.insert("kind".into(), Value::str(KINDS[rng.next_int(10) as usize]));
        tags.insert("rank".into(), Value::Int(rng.next_int(20)));
        tags.insert("oneway".into(), Value::Bool(rng.next_int(2) == 1));
        tags.insert("id".into(), Value::Int(i));
        result.push(("lines", Geometry::LineString(LineString::new(coords)), tags));
    }
    for i in 0..1000 {
        let x = rng.next_double() * 256.0;
        let y = rng.next_double() * 256.0;
        let mut tags = Tags::new();
        tags.insert("kind".into(), Value::str(KINDS[rng.next_int(10) as usize]));
        tags.insert("rank".into(), Value::Int(rng.next_int(20)));
        tags.insert("name".into(), Value::str(format!("name{i}")));
        result.push(("points", Geometry::Point(Point::new(x, y)), tags));
    }
    result
}

fn encode_features(input: &[(&'static str, Geometry<f64>, Tags)]) -> Vec<Feature> {
    input
        .iter()
        .enumerate()
        .map(|(i, (layer, geom, tags))| Feature::new(*layer, i as i64 + 1, encode_geometry(geom), tags.clone()))
        .collect()
}

fn encode_tile(features: &[Feature]) -> Vec<u8> {
    let mut tile = VectorTile::new();
    tile.add_layer_features("polygons", &features[0..1000]);
    tile.add_layer_features("lines", &features[1000..2000]);
    tile.add_layer_features("points", &features[2000..3000]);
    tile.encode()
}

fn main() {
    let iterations: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(2000);
    let input = workload();

    // warmup
    for _ in 0..(iterations / 4).max(1) {
        black_box(encode_tile(&encode_features(&input)));
    }

    let start = Instant::now();
    for _ in 0..iterations {
        let geoms: Vec<_> = input.iter().map(|(_, geom, _)| encode_geometry(geom)).collect();
        black_box(geoms);
    }
    let geom_secs = start.elapsed().as_secs_f64();
    let features = encode_features(&input);

    let start = Instant::now();
    let mut bytes = 0usize;
    for _ in 0..iterations {
        bytes = black_box(encode_tile(&features)).len();
    }
    let tile_secs = start.elapsed().as_secs_f64();

    println!("rust iterations={iterations} features/tile={}", features.len());
    println!(
        "encodeGeometry: {:.0} features/sec ({:.3} ms per 3000)",
        iterations as f64 * input.len() as f64 / geom_secs,
        geom_secs * 1000.0 / iterations as f64
    );
    println!(
        "tile encode:    {:.0} tiles/sec ({:.3} ms per tile), {bytes} bytes/tile",
        iterations as f64 / tile_secs,
        tile_secs * 1000.0 / iterations as f64
    );
}
