//! Decodes MLT tiles with the independent `mlt-core` 0.21 decoder (the official Rust MLT crate) and with
//! planetiler-core's port of the Java decoder, and checks that both give the same features.
//!
//! Input: `.bin` files of `z, x, y, length` (big-endian ints) + tile bytes, as written by
//! `cargo run --release -p planetiler-benchmarks --bin mlt_reference -- encode ...` or
//! `bench/java/MltReference.java encode ...`. Not part of the workspace (it pulls in `mlt-core` and its
//! dependencies only for this check):
//!
//! ```sh
//! cargo run --release --manifest-path bench/mlt-core-check/Cargo.toml -- tiles.bin [more.bin ...]
//! ```
//!
//! Layers whose polygons were tessellated without outlines (`--mlt-tessellate-polygons` without
//! `--mlt-polygon-outline`) cannot be decoded by either decoder; for those only the mlt-core parse is checked.

use std::io::Read;

use geo_types::Geometry;
use mlt_core::{Decoder, Parser, PropValue};
use planetiler_core::value::Value;

fn to_f64(g: &Geometry<i32>) -> Geometry<f64> {
    use geo_types::*;
    let c = |c: &Coord<i32>| Coord { x: c.x as f64, y: c.y as f64 };
    let ls = |l: &LineString<i32>| LineString::new(l.0.iter().map(c).collect());
    let poly = |p: &Polygon<i32>| Polygon::new(ls(p.exterior()), p.interiors().iter().map(ls).collect());
    match g {
        Geometry::Point(p) => Geometry::Point(Point(c(&p.0))),
        Geometry::MultiPoint(mp) => {
            Geometry::MultiPoint(MultiPoint::new(mp.0.iter().map(|p| Point(c(&p.0))).collect()))
        }
        Geometry::LineString(l) => Geometry::LineString(ls(l)),
        Geometry::MultiLineString(ml) => Geometry::MultiLineString(MultiLineString::new(ml.0.iter().map(ls).collect())),
        Geometry::Polygon(p) => Geometry::Polygon(poly(p)),
        Geometry::MultiPolygon(mp) => Geometry::MultiPolygon(MultiPolygon::new(mp.0.iter().map(poly).collect())),
        other => panic!("unexpected {other:?}"),
    }
}

fn value(v: &PropValue) -> Option<Value> {
    match v {
        PropValue::Bool(b) => b.map(Value::Bool),
        PropValue::I8(i) => i.map(|i| Value::Int(i as i64)),
        PropValue::U8(i) => i.map(|i| Value::Int(i as i64)),
        PropValue::I32(i) => i.map(|i| Value::Int(i as i64)),
        PropValue::U32(i) => i.map(|i| Value::Int(i as i64)),
        PropValue::I64(i) => i.map(Value::Int),
        PropValue::U64(i) => i.map(|i| Value::Int(i as i64)),
        PropValue::F32(f) => f.map(Value::Float),
        PropValue::F64(f) => f.map(Value::Double),
        PropValue::Str(s) => s.as_ref().map(Value::str),
    }
}

fn main() -> anyhow::Result<()> {
    for path in std::env::args().skip(1) {
        let mut data = Vec::new();
        std::fs::File::open(&path)?.read_to_end(&mut data)?;
        let mut pos = 0;
        let (mut tiles, mut features, mut errors, mut unparsed) = (0, 0, 0, 0);
        while pos < data.len() {
            let int = |i: usize| u32::from_be_bytes(data[pos + i * 4..pos + i * 4 + 4].try_into().unwrap());
            let (z, x, y, len) = (int(0), int(1), int(2), int(3) as usize);
            let bytes = &data[pos + 16..pos + 16 + len];
            pos += 16 + len;
            tiles += 1;
            let ours = match planetiler_core::mlt::decoder::decode(bytes) {
                Ok(o) => o,
                Err(e) => {
                    // only the layer framing and metadata can be checked
                    let layers = Parser::default().parse_layers(bytes).map_err(|e| anyhow::anyhow!("{e}"))?;
                    if unparsed == 0 {
                        eprintln!("{z}/{x}/{y}: not decodable ({e}); checking only that mlt-core parses its layers");
                    }
                    unparsed += layers.len();
                    continue;
                }
            };
            let mut parser = Parser::default();
            let layers = match parser.parse_layers(bytes) {
                Ok(l) => l,
                Err(e) => {
                    errors += 1;
                    if errors < 5 {
                        eprintln!("{z}/{x}/{y}: mlt-core parse error {e}");
                    }
                    continue;
                }
            };
            let mut dec = Decoder::default();
            for (layer, our_layer) in layers.into_iter().zip(&ours.layers) {
                let tile_layer = match layer.into_tile(&mut dec) {
                    Ok(Some(l)) => l,
                    Ok(None) => anyhow::bail!("unknown layer"),
                    Err(e) => {
                        errors += 1;
                        if errors < 5 {
                            eprintln!("{z}/{x}/{y} {}: mlt-core decode error {e}", our_layer.name);
                        }
                        continue;
                    }
                };
                anyhow::ensure!(tile_layer.name() == our_layer.name, "layer name");
                anyhow::ensure!(tile_layer.features().len() == our_layer.features.len(), "feature count");
                let names = tile_layer.property_names();
                for (f, ours) in tile_layer.features().iter().zip(&our_layer.features) {
                    features += 1;
                    anyhow::ensure!(f.id().map(|i| i as i64) == ours.id, "{z}/{x}/{y} id {:?} {:?}", f.id(), ours.id);
                    let g = to_f64(f.geometry());
                    anyhow::ensure!(
                        g == ours.geometry,
                        "{z}/{x}/{y} {} geometry\n{g:?}\n{:?}",
                        our_layer.name,
                        ours.geometry
                    );
                    let mut props = planetiler_core::value::Tags::default();
                    for (name, v) in names.iter().zip(f.properties()) {
                        if let Some(v) = value(v) {
                            props.insert(name.clone(), v);
                        }
                    }
                    let mut a: Vec<_> = props.iter().collect();
                    let mut b: Vec<_> = ours.properties.iter().collect();
                    a.sort_by(|x, y| x.0.cmp(y.0));
                    b.sort_by(|x, y| x.0.cmp(y.0));
                    anyhow::ensure!(a == b, "{z}/{x}/{y} {} props\n{a:?}\n{b:?}", our_layer.name);
                }
            }
        }
        println!(
            "{path}: {tiles} tiles, {features} features identical between mlt-core and planetiler-core decoders, \
             {errors} mlt-core errors, {unparsed} layers only parsed"
        );
    }
    Ok(())
}
