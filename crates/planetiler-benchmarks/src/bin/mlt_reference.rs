//! MLT encoder verification against Java (not a benchmark). The Java counterpart is `bench/java/MltReference.java`.
//!
//! - `encode <in.mbtiles> <out.bin> [fsst] [fastpfor] [tessellate] [outline] [reorder] [shareddict] [excludeids]`:
//!   re-encodes every gzipped MVT tile to MLT like `TileArchiveWriter` and writes `z, x, y, length` (big-endian
//!   ints) + bytes per tile, the same format as `MltReference.java encode`. Also checks that decoding each MLT tile
//!   gives back the features of the MVT tile.
//! - `compare <a.bin> <b.bin>`: compares two such files tile by tile: identical bytes, or decoded contents.
//! - `decode <tiles.bin>`: prints one line per decoded feature in the format of `MltReference.java decode`
//!   (normalize Java's `Integer`/`Long` type names to `Int` before diffing).

use std::collections::BTreeMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use geo_types::{Coord, Geometry, LineString};
use planetiler_core::archive::readable_tile_archive::ReadableTileArchive;
use planetiler_core::mbtiles::mbtiles::Mbtiles;
use planetiler_core::mlt::{self, MltOptions};
use planetiler_core::value::Value;
use planetiler_core::vector_tile::{self, VectorTile};

type Tiles = BTreeMap<(u32, u32, u32), Vec<u8>>;

fn read_bin(path: &Path) -> anyhow::Result<Tiles> {
    let mut input = BufReader::new(std::fs::File::open(path).with_context(|| format!("{path:?}"))?);
    let mut tiles = Tiles::new();
    let mut buf = [0u8; 16];
    loop {
        match input.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        let int = |i: usize| u32::from_be_bytes(buf[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
        let mut bytes = vec![0; int(3) as usize];
        input.read_exact(&mut bytes)?;
        tiles.insert((int(0), int(1), int(2)), bytes);
    }
    Ok(tiles)
}

fn options(args: &[String]) -> (bool, MltOptions) {
    let has = |s: &str| args.iter().any(|a| a == s);
    (
        !has("excludeids"),
        MltOptions {
            fsst: has("fsst"),
            fast_pfor: has("fastpfor"),
            tessellate_polygons: has("tessellate"),
            polygon_outline: has("outline"),
            reorder_features: has("reorder"),
            shared_dictionaries: has("shareddict"),
        },
    )
}

fn round_geometry(g: &Geometry<f64>) -> Geometry<f64> {
    use geo::MapCoords;
    g.map_coords(|c| Coord { x: (c.x as i32) as f64, y: (c.y as i32) as f64 })
}

/// MLT drops polygon closing points and stores vertices as ints; normalize the MVT side the same way.
fn normalize_rings(g: Geometry<f64>) -> Geometry<f64> {
    let close = |l: &LineString<f64>| {
        let mut l = l.clone();
        if l.0.first() != l.0.last()
            && let Some(&first) = l.0.first()
        {
            l.0.push(first);
        }
        l
    };
    match g {
        Geometry::Polygon(p) => {
            Geometry::Polygon(geo_types::Polygon::new(close(p.exterior()), p.interiors().iter().map(close).collect()))
        }
        Geometry::MultiPolygon(mp) => Geometry::MultiPolygon(geo_types::MultiPolygon::new(
            mp.0.iter()
                .map(|p| geo_types::Polygon::new(close(p.exterior()), p.interiors().iter().map(close).collect()))
                .collect(),
        )),
        other => other,
    }
}

fn encode(args: &[String]) -> anyhow::Result<()> {
    let (include_ids, options) = options(&args[2..]);
    let db = Mbtiles::new_read_only_database(Path::new(&args[0]))?;
    let mut tiles: Vec<_> = db.get_all_tiles()?.collect::<anyhow::Result<Vec<_>>>()?;
    tiles.sort_by_key(|t| (t.coord.z, t.coord.x, t.coord.y));
    let mut out = BufWriter::new(std::fs::File::create(&args[1])?);
    let start = Instant::now();
    let mut checked_features = 0;
    for tile in &tiles {
        let features = vector_tile::decode(&planetiler_core::util::gzip::gunzip(&tile.bytes)?)?;
        let mut by_layer: indexmap::IndexMap<String, Vec<vector_tile::Feature>> = indexmap::IndexMap::new();
        for f in features {
            by_layer.entry(f.layer.clone()).or_default().push(f);
        }
        let mut vt = VectorTile::new();
        for (layer, features) in &by_layer {
            vt.add_layer_features(layer, features);
        }
        let (encoded, _) = vt.encode_mlt_with_layer_stats(include_ids, &options, None)?;
        for v in [tile.coord.z, tile.coord.x, tile.coord.y, encoded.len() as u32] {
            out.write_all(&v.to_be_bytes())?;
        }
        out.write_all(&encoded)?;

        // decoded MLT == MVT input (polygons tessellated without their outlines cannot be decoded)
        if options.tessellate_polygons && !options.polygon_outline {
            continue;
        }
        let input = vt.to_mlt_input(None);
        let decoded = mlt::decoder::decode(&encoded)?;
        anyhow::ensure!(input.layers.len() == decoded.layers.len(), "{:?} layer count", tile.coord);
        for (expected, actual) in input.layers.iter().zip(&decoded.layers) {
            anyhow::ensure!(expected.name == actual.name, "{:?} layer name", tile.coord);
            let mut expected_features: Vec<&mlt::Feature> = expected.features.iter().collect();
            if options.reorder_features && include_ids {
                expected_features.sort_by_key(|f| f.id);
            }
            anyhow::ensure!(expected_features.len() == actual.features.len(), "{:?} feature count", tile.coord);
            for (e, a) in expected_features.iter().zip(&actual.features) {
                let id = if include_ids { e.id } else { None };
                anyhow::ensure!(id == a.id, "{:?} {} id {:?} != {:?}", tile.coord, expected.name, id, a.id);
                let eg = normalize_rings(round_geometry(&e.geometry));
                anyhow::ensure!(
                    eg == a.geometry,
                    "{:?} {} geometry\n{eg:?}\n{:?}",
                    tile.coord,
                    expected.name,
                    a.geometry
                );
                let mut props = e.properties.clone();
                // mixed-type columns are coerced to strings
                for (k, v) in props.iter_mut() {
                    if let Some(Value::String(s)) = a.properties.get(k)
                        && !matches!(v, Value::String(_))
                    {
                        *v = Value::str(mlt::property_encoder::java_to_string(v));
                        anyhow::ensure!(**s == *v.as_str().unwrap_or(""), "{:?} coerced {k}", tile.coord);
                    }
                    if let (Value::Double(d), Some(Value::Float(_))) = (&*v, a.properties.get(k)) {
                        *v = Value::Float(*d as f32);
                    }
                    if let (Value::Float(f), Some(Value::Double(_))) = (&*v, a.properties.get(k)) {
                        *v = Value::Double(*f as f64);
                    }
                }
                anyhow::ensure!(
                    props == a.properties,
                    "{:?} {} properties\n{props:?}\n{:?}",
                    tile.coord,
                    expected.name,
                    a.properties
                );
                checked_features += 1;
            }
        }
    }
    out.flush()?;
    let treeified = mlt::fsst::TREEIFIED_BINS.load(std::sync::atomic::Ordering::Relaxed);
    if treeified > 0 {
        eprintln!("warning: {treeified} FSST candidate bins would be trees in Java (order not emulated)");
    }
    if options.tessellate_polygons && !options.polygon_outline {
        eprintln!(
            "encoded {} tiles in {:?} (polygons without outlines cannot be decoded)",
            tiles.len(),
            start.elapsed()
        );
    } else {
        eprintln!(
            "encoded {} tiles in {:?}; decoded MLT matches the MVT input for all {checked_features} features",
            tiles.len(),
            start.elapsed()
        );
    }
    Ok(())
}

fn compare(args: &[String]) -> anyhow::Result<()> {
    let a = read_bin(Path::new(&args[0]))?;
    let b = read_bin(Path::new(&args[1]))?;
    let (mut identical, mut same_content, mut different, mut missing) = (0, 0, 0, 0);
    let (mut size_a, mut size_b) = (0usize, 0usize);
    for (coord, bytes_a) in &a {
        let Some(bytes_b) = b.get(coord) else {
            missing += 1;
            continue;
        };
        size_a += bytes_a.len();
        size_b += bytes_b.len();
        if bytes_a == bytes_b {
            identical += 1;
            continue;
        }
        let da = mlt::decoder::decode(bytes_a)?;
        let db = mlt::decoder::decode(bytes_b)?;
        if da == db {
            same_content += 1;
        } else {
            different += 1;
            if different <= 5 {
                println!("{coord:?}: decoded content differs ({} vs {} bytes)", bytes_a.len(), bytes_b.len());
            }
        }
        if identical + same_content + different <= 5 || different <= 5 {
            let first_diff = bytes_a.iter().zip(bytes_b).position(|(x, y)| x != y);
            println!("{coord:?}: bytes differ at {first_diff:?} ({} vs {} bytes)", bytes_a.len(), bytes_b.len());
        }
    }
    missing += b.keys().filter(|k| !a.contains_key(k)).count();
    println!(
        "tiles: {} vs {}; byte-identical {identical}, same decoded content {same_content}, different {different}, \
         missing {missing}; total bytes {size_a} vs {size_b}",
        a.len(),
        b.len()
    );
    Ok(())
}

fn wkt(g: &Geometry<f64>) -> String {
    fn coords(l: &LineString<f64>) -> String {
        l.0.iter().map(|c| format!("{} {}", c.x, c.y)).collect::<Vec<_>>().join(", ")
    }
    fn poly(p: &geo_types::Polygon<f64>) -> String {
        std::iter::once(p.exterior())
            .chain(p.interiors())
            .map(|r| format!("({})", coords(r)))
            .collect::<Vec<_>>()
            .join(", ")
    }
    match g {
        Geometry::Point(p) => format!("POINT ({} {})", p.x(), p.y()),
        Geometry::MultiPoint(mp) => format!(
            "MULTIPOINT ({})",
            mp.0.iter().map(|p| format!("({} {})", p.x(), p.y())).collect::<Vec<_>>().join(", ")
        ),
        Geometry::LineString(l) => format!("LINESTRING ({})", coords(l)),
        Geometry::MultiLineString(ml) => {
            format!(
                "MULTILINESTRING ({})",
                ml.0.iter().map(|l| format!("({})", coords(l))).collect::<Vec<_>>().join(", ")
            )
        }
        Geometry::Polygon(p) => format!("POLYGON ({})", poly(p)),
        Geometry::MultiPolygon(mp) => {
            format!("MULTIPOLYGON ({})", mp.0.iter().map(|p| format!("({})", poly(p))).collect::<Vec<_>>().join(", "))
        }
        other => format!("{other:?}"),
    }
}

fn decode(args: &[String]) -> anyhow::Result<()> {
    let tiles = read_bin(Path::new(&args[0]))?;
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    for ((z, x, y), bytes) in &tiles {
        for layer in mlt::decoder::decode(bytes)?.layers {
            for f in layer.features {
                let mut props: Vec<_> = f.properties.iter().collect();
                props.sort_by(|a, b| a.0.cmp(b.0));
                let props: Vec<String> = props
                    .iter()
                    .map(|(k, v)| {
                        let (text, ty) = match v {
                            Value::String(s) => (s.to_string(), "String"),
                            Value::Int(i) => (i.to_string(), "Int"),
                            Value::Bool(b) => (b.to_string(), "Boolean"),
                            Value::Float(f) => (mlt::property_encoder::java_float_to_string(*f), "Float"),
                            Value::Double(d) => (planetiler_core::value::java_double_to_string(*d), "Double"),
                            other => (other.to_string(), "Other"),
                        };
                        format!("{k}={text}({ty})")
                    })
                    .collect();
                let id = f.id.map_or("-".to_string(), |id| id.to_string());
                writeln!(out, "{z}/{x}/{y}\t{}\t{id}\t{}\t{}", layer.name, wkt(&f.geometry), props.join(" "))?;
            }
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("encode") => encode(&args[1..]),
        Some("compare") => compare(&args[1..]),
        Some("decode") => decode(&args[1..]),
        _ => anyhow::bail!("usage: mlt_reference encode|compare|decode ..."),
    }
}
