//! Generates an Overture-like buildings GeoParquet dataset with a deliberately skewed row-group size distribution, to
//! measure how well parquet processing balances work between threads (`ParquetReader` + `Distributor`).
//!
//! Writes `<out>/theme=buildings/type=building/part-<n>.parquet` (the layout `add_overture_source` / the `overture`
//! example expect), so it can be run with `planetiler overture --base=<out>` or the Java equivalent. Each file has
//! `small_groups` row groups of `small_rows` rows; the first file also gets one row group of `big_rows` rows in the
//! middle. Columns follow the Overture buildings schema subset the example uses: `id` (string), `geometry` (WKB
//! polygon, GeoParquet 1.0 metadata), `height` (double, nullable), `roof_color` (string, nullable), `subtype`,
//! `class` and `num_floors`. Each row group covers its own strip of the bounding box (like Overture's spatially sorted
//! files) with uniform density; buildings are rotated rectangles of 8-30m with a few extra vertices. Output is
//! deterministic for a given set of arguments.
//!
//! Arguments (`--key=value` or `key=value`):
//! - `out` (required) output directory
//! - `files` (default `4`)
//! - `small_groups` (default `40`) row groups per file
//! - `small_rows` (default `15000`) rows per small row group
//! - `big_rows` (default `1500000`) rows in the big row group (`0` for none)
//! - `bbox` (default `-72,42,-70,44`) `minlon,minlat,maxlon,maxlat`

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{ArrayRef, BinaryBuilder, Float64Builder, Int32Builder, StringBuilder};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use geo_types::{Coord, LineString, Polygon};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;
use planetiler_core::geo::geo_utils;

const BATCH: usize = 65_536;
const METERS_PER_DEGREE: f64 = 111_320.0;

fn parse_args() -> HashMap<String, String> {
    let mut result = HashMap::new();
    for arg in std::env::args().skip(1) {
        let arg = arg.trim_start_matches('-');
        match arg.split_once('=') {
            Some((k, v)) => result.insert(k.replace('-', "_"), v.to_string()),
            None => result.insert(arg.replace('-', "_"), "true".to_string()),
        };
    }
    result
}

/// splitmix64
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.f64()
    }
}

/// A rotated rectangle building footprint around `(x, y)`, with 0-4 extra vertices on its edges.
fn building(rng: &mut Rng, x: f64, y: f64) -> Polygon<f64> {
    let (w, h) = (rng.range(8.0, 30.0), rng.range(8.0, 30.0));
    let angle = rng.range(0.0, std::f64::consts::PI);
    let (sin, cos) = angle.sin_cos();
    let lon_scale = 1.0 / (METERS_PER_DEGREE * y.to_radians().cos());
    let lat_scale = 1.0 / METERS_PER_DEGREE;
    let corners = [(-w / 2.0, -h / 2.0), (w / 2.0, -h / 2.0), (w / 2.0, h / 2.0), (-w / 2.0, h / 2.0)];
    let extra = (rng.next_u64() % 5) as usize;
    let mut points = Vec::with_capacity(5 + extra);
    for (i, &(cx, cy)) in corners.iter().enumerate() {
        points.push((cx, cy));
        if i < extra {
            // a vertex part-way along the edge to the next corner, nudged outwards
            let (nx, ny) = corners[(i + 1) % 4];
            let t = rng.range(0.3, 0.7);
            points.push((cx + (nx - cx) * t + rng.range(-1.0, 1.0), cy + (ny - cy) * t + rng.range(-1.0, 1.0)));
        }
    }
    let mut coords: Vec<Coord<f64>> = points
        .into_iter()
        .map(|(px, py)| Coord { x: x + (px * cos - py * sin) * lon_scale, y: y + (px * sin + py * cos) * lat_scale })
        .collect();
    coords.push(coords[0]);
    Polygon::new(LineString(coords), vec![])
}

struct Bbox {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("geometry", DataType::Binary, false),
        Field::new("height", DataType::Float64, true),
        Field::new("roof_color", DataType::Utf8, true),
        Field::new("subtype", DataType::Utf8, true),
        Field::new("class", DataType::Utf8, true),
        Field::new("num_floors", DataType::Int32, true),
    ]))
}

/// Writes one file with row groups of `groups[i]` rows, row group `i` covering `strips[i]` (`(min_x, max_x)`).
fn write_file(path: &Path, groups: &[usize], strips: &[(f64, f64)], bbox: &Bbox, rng: &mut Rng) -> anyhow::Result<()> {
    let schema = schema();
    let geo = format!(
        r#"{{"version":"1.0.0","primary_column":"geometry","columns":{{"geometry":{{"encoding":"WKB","geometry_types":["Polygon"],"bbox":[{},{},{},{}]}}}}}}"#,
        bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y
    );
    let props = WriterProperties::builder()
        .set_key_value_metadata(Some(vec![KeyValue::new("geo".to_string(), geo)]))
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_max_row_group_row_count(None)
        .set_max_row_group_bytes(None)
        .build();
    let mut writer = ArrowWriter::try_new(File::create(path)?, schema.clone(), Some(props))?;
    let subtypes = ["residential", "commercial", "industrial", "education", "outbuilding"];
    let classes = ["house", "apartments", "garage", "shed", "retail", "school"];
    let colors = ["#aa5533", "#888888", "#334455", "red", "white"];
    for (&rows, &(x0, x1)) in groups.iter().zip(strips) {
        let mut remaining = rows;
        while remaining > 0 {
            let n = remaining.min(BATCH);
            remaining -= n;
            let mut id = StringBuilder::with_capacity(n, n * 32);
            let mut geometry = BinaryBuilder::with_capacity(n, n * 140);
            let mut height = Float64Builder::with_capacity(n);
            let mut roof_color = StringBuilder::new();
            let mut subtype = StringBuilder::new();
            let mut class = StringBuilder::new();
            let mut num_floors = Int32Builder::with_capacity(n);
            for _ in 0..n {
                id.append_value(format!("08b{:016x}{:013x}", rng.next_u64(), rng.next_u64() >> 12));
                let (x, y) = (rng.range(x0, x1), rng.range(bbox.min_y, bbox.max_y));
                geometry.append_value(geo_utils::write_wkb(&building(rng, x, y).into())?);
                let r = rng.next_u64();
                if r.is_multiple_of(3) {
                    height.append_value(((r >> 8) % 400) as f64 / 10.0 + 3.0);
                } else {
                    height.append_null();
                }
                if r.is_multiple_of(7) {
                    roof_color.append_value(colors[(r >> 16) as usize % colors.len()]);
                } else {
                    roof_color.append_null();
                }
                if r.is_multiple_of(2) {
                    subtype.append_value(subtypes[(r >> 20) as usize % subtypes.len()]);
                    class.append_value(classes[(r >> 24) as usize % classes.len()]);
                } else {
                    subtype.append_null();
                    class.append_null();
                }
                if r.is_multiple_of(5) {
                    num_floors.append_value(((r >> 28) % 12) as i32 + 1);
                } else {
                    num_floors.append_null();
                }
            }
            let columns: Vec<ArrayRef> = vec![
                Arc::new(id.finish()),
                Arc::new(geometry.finish()),
                Arc::new(height.finish()),
                Arc::new(roof_color.finish()),
                Arc::new(subtype.finish()),
                Arc::new(class.finish()),
                Arc::new(num_floors.finish()),
            ];
            writer.write(&RecordBatch::try_new(schema.clone(), columns)?)?;
        }
        writer.flush()?;
    }
    writer.close()?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = parse_args();
    let out = PathBuf::from(args.get("out").ok_or_else(|| anyhow::anyhow!("--out=<dir> is required"))?);
    let get = |key: &str, default: usize| args.get(key).map_or(Ok(default), |v| v.parse::<usize>());
    let files = get("files", 4)?.max(1);
    let small_groups = get("small_groups", 40)?;
    let small_rows = get("small_rows", 15_000)?;
    let big_rows = get("big_rows", 1_500_000)?;
    let bbox: Vec<f64> = args
        .get("bbox")
        .map_or("-72,42,-70,44", String::as_str)
        .split(',')
        .map(str::parse)
        .collect::<Result<_, _>>()?;
    anyhow::ensure!(bbox.len() == 4, "bbox must be minlon,minlat,maxlon,maxlat");
    let bbox = Bbox { min_x: bbox[0], min_y: bbox[1], max_x: bbox[2], max_y: bbox[3] };

    // row group sizes of every file, then strips of the bbox proportional to row counts (uniform density)
    let layout: Vec<Vec<usize>> = (0..files)
        .map(|f| {
            let mut groups = vec![small_rows; small_groups];
            if f == 0 && big_rows > 0 {
                groups.insert(small_groups / 2, big_rows);
            }
            groups
        })
        .collect();
    let total: usize = layout.iter().flatten().sum();
    let mut x = bbox.min_x;
    let strips: Vec<Vec<(f64, f64)>> = layout
        .iter()
        .map(|groups| {
            groups
                .iter()
                .map(|&rows| {
                    let x1 = x + (bbox.max_x - bbox.min_x) * rows as f64 / total as f64;
                    let strip = (x, x1);
                    x = x1;
                    strip
                })
                .collect()
        })
        .collect();

    let dir = out.join("theme=buildings").join("type=building");
    std::fs::create_dir_all(&dir)?;
    let start = std::time::Instant::now();
    for (f, (groups, strips)) in layout.iter().zip(&strips).enumerate() {
        let path = dir.join(format!("part-{f:05}.parquet"));
        let mut rng = Rng(0x5eed_0000 + f as u64);
        write_file(&path, groups, strips, &bbox, &mut rng)?;
        println!(
            "wrote {} ({} row groups, {} rows, {} bytes)",
            path.display(),
            groups.len(),
            groups.iter().sum::<usize>(),
            std::fs::metadata(&path)?.len()
        );
    }
    println!("{total} rows in {files} files in {:.1}s", start.elapsed().as_secs_f64());
    Ok(())
}
