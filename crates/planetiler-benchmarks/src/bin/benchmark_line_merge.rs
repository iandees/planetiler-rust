//! Port of `BenchmarkLineMerge.java`: compares JTS-style line merging with `LoopLineMerger` on real-world and
//! synthetic inputs.
//!
//! Run with `cargo run --release -p planetiler-benchmarks --bin benchmark_line_merge`. Test resources are read from
//! `planetiler-core/src/test/resources/mergelines` relative to the working directory, then
//! `crates/planetiler-core/src/test/resources/mergelines`, then `$PLANETILER_REF/planetiler-core/...`
//! (defaults to `../planetiler-ref/planetiler`).

// see the `mimalloc` feature in Cargo.toml
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use geo_types::{Coord, Geometry, GeometryCollection, LineString, MultiLineString, Point, Polygon};
use planetiler_core::util::loop_line_merger::{LoopLineMerger, line_merger::LineMerger};

static mut NUM_LINES: usize = 0;

fn add_lines(n: usize) {
    // single-threaded benchmark
    unsafe { NUM_LINES += n };
}

fn loop_merger(min_length: f64) -> LoopLineMerger {
    let mut lm = LoopLineMerger::new();
    lm.set_min_length(min_length);
    lm.set_stub_min_length(min_length);
    lm.set_loop_min_length(min_length);
    lm.set_tolerance(1.0);
    lm.set_merge_strokes(true);
    lm
}

type Merge = dyn Fn(&Geometry<f64>) -> Vec<LineString<f64>>;

fn time(name: &str, f: &Merge) {
    let cols = [
        name.to_string(),
        time_millis(&read("mergelines_200433_lines.wkb.gz"), f),
        time_millis(&read("mergelines_239823_lines.wkb.gz"), f),
        "(/s):".to_string(),
        time_per_sec(&read("mergelines_1759_point_line.wkb.gz"), f),
        time_per_sec(&make_lines(50, 2), f),
        time_per_sec(&make_lines(10, 10), f),
        time_per_sec(&make_lines(2, 50), f),
    ];
    eprintln!("{}", cols.join("\t"));
}

fn time_per_sec(geometry: &Geometry<f64>, f: &Merge) -> String {
    let start = Instant::now();
    let end = start + Duration::from_secs(1);
    let mut num = 0u64;
    while Instant::now() < end {
        add_lines(f(geometry).len());
        num += 1;
    }
    let per_sec = (num as f64 / start.elapsed().as_secs_f64()).round();
    format_numeric(per_sec as i64, true)
}

fn time_millis(geometry: &Geometry<f64>, f: &Merge) -> String {
    let start = Instant::now();
    let end = start + Duration::from_secs(1);
    let mut num = 0u64;
    while Instant::now() < end {
        add_lines(f(geometry).len());
        num += 1;
    }
    // equivalent of toPrecision(3)
    let nanos_per = start.elapsed().as_nanos() as i64 / num as i64;
    pad_right(&duration_to_string(round_to_precision(nanos_per, 3)), 6)
}

/// `new BigDecimal(value, new MathContext(digits)).longValue()` (HALF_UP rounding).
fn round_to_precision(value: i64, digits: u32) -> i64 {
    let len = value.abs().to_string().len() as u32;
    if len <= digits {
        return value;
    }
    let factor = 10i64.pow(len - digits);
    let q = value / factor;
    let r = value % factor;
    (if r * 2 >= factor { q + 1 } else { q }) * factor
}

/// Java `Duration.ofNanos(n).toString()` without the `PT` prefix (only seconds-level output is needed here).
fn duration_to_string(nanos: i64) -> String {
    let secs = nanos / 1_000_000_000;
    let frac = nanos % 1_000_000_000;
    let mut out = String::new();
    let (hours, minutes, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if hours > 0 {
        out.push_str(&format!("{hours}H"));
    }
    if minutes > 0 {
        out.push_str(&format!("{minutes}M"));
    }
    if s == 0 && frac == 0 && !out.is_empty() {
        return out;
    }
    if frac == 0 {
        out.push_str(&format!("{s}S"));
    } else {
        let f = format!("{frac:09}");
        out.push_str(&format!("{s}.{}S", f.trim_end_matches('0')));
    }
    out
}

fn pad_right(s: &str, size: usize) -> String {
    format!("{s:<size$}")
}

fn pad_left(s: &str, size: usize) -> String {
    format!("{s:>size$}")
}

/// planetiler `Format.numeric(num, pad)`.
fn format_numeric(value: i64, pad: bool) -> String {
    let width = if pad { 4 } else { 0 };
    if value < 0 {
        return pad_left("-", width);
    } else if value < 1000 {
        return pad_left(&value.to_string(), width);
    }
    let suffixes = [
        (1_000_000_000_000_000i64, "Q"),
        (1_000_000_000_000, "T"),
        (1_000_000_000, "B"),
        (1_000_000, "M"),
        (1_000, "k"),
    ];
    let (divide_by, suffix) = suffixes.iter().find(|(d, _)| value >= *d).unwrap();
    let truncated = value / (divide_by / 10);
    let has_decimal = truncated < 100 && truncated % 10 != 0;
    let s = if has_decimal {
        format!("{}{suffix}", format_decimal(truncated as f64 / 10.0))
    } else {
        format!("{}{suffix}", truncated / 10)
    };
    pad_left(&s, width)
}

fn format_decimal(v: f64) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

fn resource_path(file: &str) -> PathBuf {
    let rel = PathBuf::from("planetiler-core/src/test/resources/mergelines").join(file);
    if rel.exists() {
        return rel;
    }
    let crate_rel = PathBuf::from("crates/planetiler-core/src/test/resources/mergelines").join(file);
    if crate_rel.exists() {
        return crate_rel;
    }
    let reference = std::env::var("PLANETILER_REF")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../../planetiler-ref/planetiler").to_string());
    let candidate = PathBuf::from(&reference).join("planetiler-core/src/test/resources/mergelines").join(file);
    if candidate.exists() {
        return candidate;
    }
    PathBuf::from("/Users/ian.dees/Workspace/planetiler-ref/planetiler/planetiler-core/src/test/resources/mergelines")
        .join(file)
}

fn read(file: &str) -> Geometry<f64> {
    let path = resource_path(file);
    let compressed = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(&compressed[..]).read_to_end(&mut bytes).unwrap();
    read_wkb(&bytes)
}

/// Minimal WKB reader for the benchmark resources.
fn read_wkb(bytes: &[u8]) -> Geometry<f64> {
    fn read(b: &[u8], pos: &mut usize) -> Geometry<f64> {
        let le = b[*pos] == 1;
        *pos += 1;
        let u32_at = |pos: &mut usize| {
            let v: [u8; 4] = b[*pos..*pos + 4].try_into().unwrap();
            *pos += 4;
            if le { u32::from_le_bytes(v) } else { u32::from_be_bytes(v) }
        };
        let f64_at = |pos: &mut usize| {
            let v: [u8; 8] = b[*pos..*pos + 8].try_into().unwrap();
            *pos += 8;
            if le { f64::from_le_bytes(v) } else { f64::from_be_bytes(v) }
        };
        let coords = |pos: &mut usize| {
            let n = u32_at(pos) as usize;
            (0..n).map(|_| Coord { x: f64_at(pos), y: f64_at(pos) }).collect::<Vec<_>>()
        };
        match u32_at(pos) {
            1 => {
                let x = f64_at(pos);
                let y = f64_at(pos);
                Geometry::Point(Point::new(x, y))
            }
            2 => Geometry::LineString(LineString(coords(pos))),
            3 => {
                let n = u32_at(pos) as usize;
                let mut rings: Vec<LineString<f64>> = (0..n).map(|_| LineString(coords(pos))).collect();
                let ext = if rings.is_empty() { LineString(vec![]) } else { rings.remove(0) };
                Geometry::Polygon(Polygon::new(ext, rings))
            }
            4..=7 => {
                let n = u32_at(pos) as usize;
                Geometry::GeometryCollection(GeometryCollection((0..n).map(|_| read(b, pos)).collect()))
            }
            other => panic!("unsupported wkb type {other}"),
        }
    }
    let mut pos = 0;
    read(bytes, &mut pos)
}

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

fn main() {
    // BENCH_ITERS overrides the number of rounds (default 10, like Java)
    let iters: usize = std::env::var("BENCH_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
    for _ in 0..iters {
        time("       JTS", &|geom| {
            let mut lm = LineMerger::new();
            lm.add(geom);
            lm.get_merged_line_strings()
        });
        time("   loop(0)", &|geom| loop_merger(0.0).add(geom).get_merged_line_strings());
        time(" loop(0.1)", &|geom| loop_merger(0.1).add(geom).get_merged_line_strings());
        time("loop(20.0)", &|geom| loop_merger(20.0).add(geom).get_merged_line_strings());
    }
    eprintln!("{}", unsafe { NUM_LINES });
}
