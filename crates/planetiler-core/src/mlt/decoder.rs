//! Decoder for the MLT tiles the [encoder](super::converter) produces (tag `0x01` layers with the encodings MLT
//! 0.0.10 writes). Equivalent to Java `MltDecoder.decodeMlTile`, used by tests, the `compare` task and tile stats.

use geo_types::{Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};

use super::encoding::{
    DictionaryType, LengthType, LogicalLevelTechnique, LogicalStreamType, OffsetType, PhysicalLevelTechnique,
    PhysicalStreamType, StreamExtra, StreamMetadata, decode_boolean_rle, decode_varint, decode_varlong, unzigzag,
    unzigzag_long,
};
use super::geometry_encoder::{LINESTRING, MULTILINESTRING, MULTIPOINT, MULTIPOLYGON, POINT, POLYGON};
use super::metadata::{Column, ColumnType, FeatureTable, ScalarType};
use super::{Feature, Layer, MapboxVectorTile, fastpfor, fsst};
use crate::value::Value;

/// A raw stream: its metadata and data bytes.
struct Stream<'a> {
    meta: StreamMetadata,
    data: &'a [u8],
}

fn read_stream<'a>(tile: &'a [u8], offset: &mut usize) -> anyhow::Result<Stream<'a>> {
    let meta = StreamMetadata::decode(tile, offset)?;
    let end = *offset + meta.byte_length as usize;
    let data = tile.get(*offset..end).ok_or_else(|| anyhow::anyhow!("truncated stream data"))?;
    *offset = end;
    Ok(Stream { meta, data })
}

/// Skips over one stream, returning its metadata (Java `TileSizeStats.skipOverStream`).
pub fn skip_stream(tile: &[u8], offset: &mut usize) -> anyhow::Result<StreamMetadata> {
    Ok(read_stream(tile, offset)?.meta)
}

fn physical_u32(stream: &Stream<'_>, num_values: usize) -> anyhow::Result<Vec<u32>> {
    match stream.meta.physical_level_technique {
        PhysicalLevelTechnique::FastPfor => fastpfor::decode(stream.data, num_values),
        PhysicalLevelTechnique::Varint => {
            let mut out = Vec::with_capacity(num_values);
            let mut pos = 0;
            for _ in 0..num_values {
                out.push(decode_varint(stream.data, &mut pos)?);
            }
            Ok(out)
        }
        other => anyhow::bail!("unsupported physical level technique for integers {other:?}"),
    }
}

fn physical_u64(stream: &Stream<'_>, num_values: usize) -> anyhow::Result<Vec<u64>> {
    match stream.meta.physical_level_technique {
        PhysicalLevelTechnique::Varint => {
            let mut out = Vec::with_capacity(num_values);
            let mut pos = 0;
            for _ in 0..num_values {
                out.push(decode_varlong(stream.data, &mut pos)?);
            }
            Ok(out)
        }
        other => anyhow::bail!("unsupported physical level technique for longs {other:?}"),
    }
}

fn rle_runs(meta: &StreamMetadata) -> anyhow::Result<usize> {
    match meta.extra {
        StreamExtra::Rle(runs, _) => Ok(runs as usize),
        _ => anyhow::bail!("missing RLE metadata"),
    }
}

fn expand_rle<T>(values: &[T], runs: usize, map: impl Fn(T) -> T) -> anyhow::Result<Vec<T>>
where
    T: Copy + TryInto<usize>,
{
    let (lengths, vals) = values.split_at(runs.min(values.len()));
    let mut out = Vec::new();
    for (&len, &v) in lengths.iter().zip(vals) {
        let len: usize = len.try_into().map_err(|_| anyhow::anyhow!("invalid run length"))?;
        out.extend(std::iter::repeat_n(map(v), len));
    }
    Ok(out)
}

/// Decodes an integer stream into 32-bit values (`signed` applies zigzag decoding where the encoder zigzags).
fn decode_int_stream(stream: &Stream<'_>, signed: bool) -> anyhow::Result<Vec<i32>> {
    let meta = &stream.meta;
    let raw = physical_u32(stream, meta.num_values as usize)?;
    let zz = |v: u32| if signed { unzigzag(v) } else { v as i32 };
    Ok(match (meta.logical_level_technique1, meta.logical_level_technique2) {
        (LogicalLevelTechnique::None, LogicalLevelTechnique::None) => raw.into_iter().map(zz).collect(),
        (LogicalLevelTechnique::Delta, LogicalLevelTechnique::None) => {
            let mut prev = 0i32;
            raw.into_iter()
                .map(|v| {
                    prev = prev.wrapping_add(unzigzag(v));
                    prev
                })
                .collect()
        }
        (LogicalLevelTechnique::Rle, LogicalLevelTechnique::None) => {
            let runs = rle_runs(meta)?;
            expand_rle(&raw, runs, |v| v)?.into_iter().map(zz).collect()
        }
        (LogicalLevelTechnique::Delta, LogicalLevelTechnique::Rle) => {
            let runs = rle_runs(meta)?;
            let mut prev = 0i32;
            expand_rle(&raw, runs, |v| v)?
                .into_iter()
                .map(|v| {
                    prev = prev.wrapping_add(unzigzag(v));
                    prev
                })
                .collect()
        }
        (LogicalLevelTechnique::ComponentwiseDelta, _) => {
            let mut px = 0i32;
            let mut py = 0i32;
            raw.chunks(2)
                .flat_map(|c| {
                    px = px.wrapping_add(unzigzag(c[0]));
                    py = py.wrapping_add(unzigzag(*c.get(1).unwrap_or(&0)));
                    [px, py]
                })
                .collect()
        }
        (LogicalLevelTechnique::Morton, LogicalLevelTechnique::Delta) => {
            let mut prev = 0i32;
            raw.into_iter()
                .map(|v| {
                    prev = prev.wrapping_add(v as i32);
                    prev
                })
                .collect()
        }
        other => anyhow::bail!("unsupported logical level techniques {other:?}"),
    })
}

fn decode_long_stream(stream: &Stream<'_>, signed: bool) -> anyhow::Result<Vec<i64>> {
    let meta = &stream.meta;
    let raw = physical_u64(stream, meta.num_values as usize)?;
    let zz = |v: u64| if signed { unzigzag_long(v) } else { v as i64 };
    Ok(match (meta.logical_level_technique1, meta.logical_level_technique2) {
        (LogicalLevelTechnique::None, LogicalLevelTechnique::None) => raw.into_iter().map(zz).collect(),
        (LogicalLevelTechnique::Delta, LogicalLevelTechnique::None) => {
            let mut prev = 0i64;
            raw.into_iter()
                .map(|v| {
                    prev = prev.wrapping_add(unzigzag_long(v));
                    prev
                })
                .collect()
        }
        (LogicalLevelTechnique::Rle, LogicalLevelTechnique::None) => {
            let runs = rle_runs(meta)?;
            expand_rle(&raw, runs, |v| v)?.into_iter().map(zz).collect()
        }
        (LogicalLevelTechnique::Delta, LogicalLevelTechnique::Rle) => {
            let runs = rle_runs(meta)?;
            let mut prev = 0i64;
            expand_rle(&raw, runs, |v| v)?
                .into_iter()
                .map(|v| {
                    prev = prev.wrapping_add(unzigzag_long(v));
                    prev
                })
                .collect()
        }
        other => anyhow::bail!("unsupported logical level techniques {other:?}"),
    })
}

fn decode_present(stream: &Stream<'_>) -> anyhow::Result<Vec<bool>> {
    decode_boolean_rle(stream.data, stream.meta.num_values as usize)
}

/// Spreads `values` over the features marked present.
fn with_presence<T>(present: Option<&[bool]>, values: Vec<T>, num_features: usize) -> anyhow::Result<Vec<Option<T>>> {
    match present {
        None => {
            if values.len() != num_features {
                anyhow::bail!("expected {num_features} values, got {}", values.len());
            }
            Ok(values.into_iter().map(Some).collect())
        }
        Some(present) => {
            let mut iter = values.into_iter();
            let result = present.iter().map(|&p| if p { iter.next() } else { None }).collect::<Vec<_>>();
            if result.iter().zip(present).any(|(v, &p)| p && v.is_none()) {
                anyhow::bail!("fewer values than present bits");
            }
            Ok(result)
        }
    }
}

/// Decodes the dictionary streams of a string column, returning the dictionary entries.
fn decode_dictionary(streams: &[Stream<'_>]) -> anyhow::Result<Vec<String>> {
    let mut symbol_lengths: Option<Vec<i32>> = None;
    let mut symbols: Option<&[u8]> = None;
    let mut lengths: Option<Vec<i32>> = None;
    let mut data: Option<&[u8]> = None;
    for s in streams {
        match (s.meta.physical_stream_type, s.meta.logical_stream_type) {
            (PhysicalStreamType::Length, Some(LogicalStreamType::Length(LengthType::Symbol))) => {
                symbol_lengths = Some(decode_int_stream(s, false)?);
            }
            (PhysicalStreamType::Data, Some(LogicalStreamType::Dictionary(DictionaryType::Fsst))) => {
                symbols = Some(s.data);
            }
            (PhysicalStreamType::Length, Some(LogicalStreamType::Length(_))) => {
                lengths = Some(decode_int_stream(s, false)?);
            }
            (PhysicalStreamType::Data, Some(LogicalStreamType::Dictionary(_))) => data = Some(s.data),
            _ => {}
        }
    }
    let lengths = lengths.ok_or_else(|| anyhow::anyhow!("missing string lengths"))?;
    let data = data.ok_or_else(|| anyhow::anyhow!("missing string data"))?;
    let decoded;
    let bytes: &[u8] = match (symbol_lengths, symbols) {
        (Some(symbol_lengths), Some(symbols)) => {
            let total: usize = lengths.iter().map(|&l| l as usize).sum();
            let symbol_lengths: Vec<u32> = symbol_lengths.into_iter().map(|l| l as u32).collect();
            decoded = fsst::decode(symbols, &symbol_lengths, data, total);
            &decoded
        }
        _ => data,
    };
    let mut result = Vec::with_capacity(lengths.len());
    let mut pos = 0usize;
    for len in lengths {
        let end = pos + len as usize;
        let s = bytes.get(pos..end).ok_or_else(|| anyhow::anyhow!("truncated string data"))?;
        result.push(String::from_utf8_lossy(s).into_owned());
        pos = end;
    }
    Ok(result)
}

fn decode_string_column(
    tile: &[u8],
    offset: &mut usize,
    column: &Column,
    num_features: usize,
) -> anyhow::Result<Vec<Option<Value>>> {
    let num_streams = decode_varint(tile, offset)? as usize;
    if num_streams == 0 {
        return Ok(vec![None; num_features]);
    }
    let mut streams = Vec::with_capacity(num_streams);
    for _ in 0..num_streams {
        streams.push(read_stream(tile, offset)?);
    }
    let mut rest: &[Stream<'_>] = &streams;
    let present = if column.nullable {
        let p = decode_present(&rest[0])?;
        rest = &rest[1..];
        Some(p)
    } else {
        None
    };
    let offsets = rest.iter().find(|s| {
        s.meta.physical_stream_type == PhysicalStreamType::Offset
            && s.meta.logical_stream_type == Some(LogicalStreamType::Offset(OffsetType::String))
    });
    let dictionary = decode_dictionary(rest)?;
    let values: Vec<String> = match offsets {
        Some(offsets) => decode_int_stream(offsets, false)?
            .into_iter()
            .map(|i| dictionary.get(i as usize).cloned().ok_or_else(|| anyhow::anyhow!("invalid string offset")))
            .collect::<anyhow::Result<_>>()?,
        None => dictionary,
    };
    Ok(with_presence(present.as_deref(), values, num_features)?.into_iter().map(|v| v.map(Value::str)).collect())
}

/// Decodes a shared dictionary struct column into one value vector per child field.
fn decode_struct_column(
    tile: &[u8],
    offset: &mut usize,
    children: &[Column],
    num_features: usize,
) -> anyhow::Result<Vec<Vec<Option<Value>>>> {
    let num_streams = decode_varint(tile, offset)? as usize;
    if num_streams == 0 {
        return Ok(vec![vec![None; num_features]; children.len()]);
    }
    // dictionary streams until the (shared or single) dictionary data stream
    let mut dictionary_streams = Vec::new();
    loop {
        let s = read_stream(tile, offset)?;
        let done = s.meta.physical_stream_type == PhysicalStreamType::Data
            && matches!(
                s.meta.logical_stream_type,
                Some(LogicalStreamType::Dictionary(DictionaryType::Single | DictionaryType::Shared))
            );
        dictionary_streams.push(s);
        if done {
            break;
        }
    }
    let dictionary = decode_dictionary(&dictionary_streams)?;
    let mut result = Vec::with_capacity(children.len());
    for _ in children {
        let n = decode_varint(tile, offset)?;
        if n == 0 {
            result.push(vec![None; num_features]);
            continue;
        }
        let present = decode_present(&read_stream(tile, offset)?)?;
        let offsets = decode_int_stream(&read_stream(tile, offset)?, false)?;
        let values = offsets
            .into_iter()
            .map(|i| dictionary.get(i as usize).cloned().ok_or_else(|| anyhow::anyhow!("invalid string offset")))
            .collect::<anyhow::Result<Vec<_>>>()?;
        result.push(
            with_presence(Some(&present), values, num_features)?.into_iter().map(|v| v.map(Value::str)).collect(),
        );
    }
    Ok(result)
}

fn decode_scalar_column(
    tile: &[u8],
    offset: &mut usize,
    column: &Column,
    scalar_type: ScalarType,
    num_features: usize,
) -> anyhow::Result<Vec<Option<Value>>> {
    if scalar_type == ScalarType::String {
        return decode_string_column(tile, offset, column, num_features);
    }
    let present = if column.nullable { Some(decode_present(&read_stream(tile, offset)?)?) } else { None };
    let data = read_stream(tile, offset)?;
    let values: Vec<Value> = match scalar_type {
        ScalarType::Boolean => {
            decode_boolean_rle(data.data, data.meta.num_values as usize)?.into_iter().map(Value::Bool).collect()
        }
        ScalarType::Int8 | ScalarType::Int32 => {
            decode_int_stream(&data, true)?.into_iter().map(|v| Value::Int(v as i64)).collect()
        }
        ScalarType::Uint8 | ScalarType::Uint32 => {
            decode_int_stream(&data, false)?.into_iter().map(|v| Value::Int(v as u32 as i64)).collect()
        }
        ScalarType::Int64 => decode_long_stream(&data, true)?.into_iter().map(Value::Int).collect(),
        ScalarType::Uint64 => decode_long_stream(&data, false)?.into_iter().map(Value::Int).collect(),
        ScalarType::Float => {
            data.data.as_chunks::<4>().0.iter().map(|c| Value::Float(f32::from_le_bytes(*c))).collect()
        }
        ScalarType::Double => {
            data.data.as_chunks::<8>().0.iter().map(|c| Value::Double(f64::from_le_bytes(*c))).collect()
        }
        ScalarType::String => unreachable!("handled above"),
    };
    with_presence(present.as_deref(), values, num_features)
}

fn decode_id_column(
    tile: &[u8],
    offset: &mut usize,
    column: &Column,
    long: bool,
) -> anyhow::Result<(Option<Vec<bool>>, Vec<i64>)> {
    let present = if column.nullable { Some(decode_present(&read_stream(tile, offset)?)?) } else { None };
    let data = read_stream(tile, offset)?;
    let ids = if long {
        decode_long_stream(&data, false)?
    } else {
        decode_int_stream(&data, false)?.into_iter().map(|v| v as u32 as i64).collect()
    };
    Ok((present, ids))
}

fn decode_morton(code: i32, num_bits: u32, shift: i32) -> (i32, i32) {
    let decode = |code: i32| -> i32 {
        let mut c = 0i32;
        for i in 0..num_bits {
            c |= ((code as i64 & (1i64 << (2 * i))) >> i) as i32;
        }
        c
    };
    (decode(code) - shift, decode(code >> 1) - shift)
}

struct GeometryCursor<'a> {
    vertex_offsets: Option<&'a [i32]>,
    vertices: &'a [(i32, i32)],
    num_geometries: &'a [i32],
    num_parts: &'a [i32],
    num_rings: &'a [i32],
    line_counts_in_rings: bool,
    geom_i: usize,
    part_i: usize,
    ring_i: usize,
    vertex_i: usize,
}

fn next_count(values: &[i32], i: &mut usize) -> anyhow::Result<usize> {
    let r = *values.get(*i).ok_or_else(|| anyhow::anyhow!("truncated geometry topology"))?;
    *i += 1;
    Ok(r as usize)
}

impl GeometryCursor<'_> {
    fn next_geometries(&mut self) -> anyhow::Result<usize> {
        next_count(self.num_geometries, &mut self.geom_i)
    }

    fn take_vertices(&mut self, n: usize) -> anyhow::Result<Vec<Coord<f64>>> {
        let mut coords = Vec::with_capacity(n + 1);
        for _ in 0..n {
            let idx = match self.vertex_offsets {
                Some(offsets) => {
                    *offsets.get(self.vertex_i).ok_or_else(|| anyhow::anyhow!("truncated vertex offsets"))? as usize
                }
                None => self.vertex_i,
            };
            let (x, y) = *self.vertices.get(idx).ok_or_else(|| anyhow::anyhow!("truncated vertices"))?;
            coords.push(Coord { x: x as f64, y: y as f64 });
            self.vertex_i += 1;
        }
        Ok(coords)
    }

    fn line(&mut self) -> anyhow::Result<LineString<f64>> {
        let n = if self.line_counts_in_rings {
            next_count(self.num_rings, &mut self.ring_i)?
        } else {
            next_count(self.num_parts, &mut self.part_i)?
        };
        Ok(LineString::new(self.take_vertices(n)?))
    }

    fn polygon(&mut self) -> anyhow::Result<Polygon<f64>> {
        let rings = next_count(self.num_parts, &mut self.part_i)?;
        let mut ring_list = Vec::with_capacity(rings);
        for _ in 0..rings {
            let count = next_count(self.num_rings, &mut self.ring_i)?;
            let mut coords = self.take_vertices(count)?;
            if let Some(&first) = coords.first() {
                coords.push(first);
            }
            ring_list.push(LineString::new(coords));
        }
        let mut iter = ring_list.into_iter();
        let shell = iter.next().unwrap_or_else(|| LineString::new(Vec::new()));
        Ok(Polygon::new(shell, iter.collect()))
    }
}

/// Decodes a geometry column into one geometry per feature (extent coordinates).
fn decode_geometry_column(tile: &[u8], offset: &mut usize) -> anyhow::Result<Vec<Geometry<f64>>> {
    let num_streams = decode_varint(tile, offset)?;
    let mut geometry_types = None;
    let mut num_geometries: Vec<i32> = Vec::new();
    let mut num_parts: Vec<i32> = Vec::new();
    let mut num_rings: Option<Vec<i32>> = None;
    let mut vertex_offsets: Option<Vec<i32>> = None;
    let mut vertices: Vec<(i32, i32)> = Vec::new();
    for i in 0..num_streams {
        let s = read_stream(tile, offset)?;
        if i == 0 {
            geometry_types = Some(decode_int_stream(&s, false)?);
            continue;
        }
        match (s.meta.physical_stream_type, s.meta.logical_stream_type) {
            (PhysicalStreamType::Length, Some(LogicalStreamType::Length(LengthType::Geometries))) => {
                num_geometries = decode_int_stream(&s, false)?;
            }
            (PhysicalStreamType::Length, Some(LogicalStreamType::Length(LengthType::Parts))) => {
                num_parts = decode_int_stream(&s, false)?;
            }
            (PhysicalStreamType::Length, Some(LogicalStreamType::Length(LengthType::Rings))) => {
                num_rings = Some(decode_int_stream(&s, false)?);
            }
            (PhysicalStreamType::Length, Some(LogicalStreamType::Length(LengthType::Triangles)))
            | (PhysicalStreamType::Offset, Some(LogicalStreamType::Offset(OffsetType::Index))) => {}
            (PhysicalStreamType::Offset, Some(LogicalStreamType::Offset(OffsetType::Vertex))) => {
                vertex_offsets = Some(decode_int_stream(&s, false)?);
            }
            (PhysicalStreamType::Data, Some(LogicalStreamType::Dictionary(DictionaryType::Vertex))) => {
                let flat = decode_int_stream(&s, false)?;
                vertices = flat.chunks(2).map(|c| (c[0], *c.get(1).unwrap_or(&0))).collect();
            }
            (PhysicalStreamType::Data, Some(LogicalStreamType::Dictionary(DictionaryType::Morton))) => {
                let StreamExtra::Morton(num_bits, shift) = s.meta.extra else {
                    anyhow::bail!("missing morton metadata");
                };
                vertices = decode_int_stream(&s, false)?
                    .into_iter()
                    .map(|code| decode_morton(code, num_bits, shift as i32))
                    .collect();
            }
            other => anyhow::bail!("unexpected geometry stream {other:?}"),
        }
    }
    let geometry_types = geometry_types.ok_or_else(|| anyhow::anyhow!("missing geometry types"))?;
    let has_polygons = geometry_types.iter().any(|&t| t == POLYGON || t == MULTIPOLYGON);
    if has_polygons && num_rings.is_none() {
        anyhow::bail!("pre-tessellated polygons without outlines cannot be decoded");
    }
    // linestring vertex counts are in the rings stream when the column also contains polygons
    let line_counts_in_rings = num_rings.is_some();
    let num_rings = num_rings.unwrap_or_default();

    let mut cursor = GeometryCursor {
        vertex_offsets: vertex_offsets.as_deref(),
        vertices: &vertices,
        num_geometries: &num_geometries,
        num_parts: &num_parts,
        num_rings: &num_rings,
        line_counts_in_rings,
        geom_i: 0,
        part_i: 0,
        ring_i: 0,
        vertex_i: 0,
    };
    let mut result = Vec::with_capacity(geometry_types.len());
    for &t in &geometry_types {
        let geometry = match t {
            POINT => Geometry::Point(Point(cursor.take_vertices(1)?[0])),
            MULTIPOINT => {
                let n = cursor.next_geometries()?;
                Geometry::MultiPoint(MultiPoint::new(cursor.take_vertices(n)?.into_iter().map(Point).collect()))
            }
            LINESTRING => Geometry::LineString(cursor.line()?),
            MULTILINESTRING => {
                let n = cursor.next_geometries()?;
                let mut lines = Vec::with_capacity(n);
                for _ in 0..n {
                    lines.push(cursor.line()?);
                }
                Geometry::MultiLineString(MultiLineString::new(lines))
            }
            POLYGON => Geometry::Polygon(cursor.polygon()?),
            MULTIPOLYGON => {
                let n = cursor.next_geometries()?;
                let mut polygons = Vec::with_capacity(n);
                for _ in 0..n {
                    polygons.push(cursor.polygon()?);
                }
                Geometry::MultiPolygon(MultiPolygon::new(polygons))
            }
            other => anyhow::bail!("unknown geometry type {other}"),
        };
        result.push(geometry);
    }
    Ok(result)
}

/// Decodes the body of one tag `0x01` layer.
fn decode_layer(body: &[u8]) -> anyhow::Result<Layer> {
    let mut offset = 0;
    let (table, extent) = FeatureTable::parse_embedded(body, &mut offset)?;
    let mut ids: Option<(Option<Vec<bool>>, Vec<i64>)> = None;
    let mut geometries: Option<Vec<Geometry<f64>>> = None;
    let mut properties: Vec<(String, Vec<Option<Value>>)> = Vec::new();
    for column in &table.columns {
        match &column.column_type {
            ColumnType::Id { long } => ids = Some(decode_id_column(body, &mut offset, column, *long)?),
            ColumnType::Geometry => geometries = Some(decode_geometry_column(body, &mut offset)?),
            ColumnType::Scalar(t) => {
                let n = geometries.as_ref().map_or(0, Vec::len);
                properties.push((column.name.clone(), decode_scalar_column(body, &mut offset, column, *t, n)?));
            }
            ColumnType::Struct(children) => {
                let n = geometries.as_ref().map_or(0, Vec::len);
                let values = decode_struct_column(body, &mut offset, children, n)?;
                for (child, values) in children.iter().zip(values) {
                    properties.push((format!("{}{}", column.name, child.name), values));
                }
            }
        }
    }
    let geometries = geometries.ok_or_else(|| anyhow::anyhow!("layer {} has no geometry column", table.name))?;
    let ids: Option<Vec<Option<i64>>> = match ids {
        Some((present, ids)) => Some(with_presence(present.as_deref(), ids, geometries.len())?),
        None => None,
    };
    let mut features = Vec::with_capacity(geometries.len());
    for (i, geometry) in geometries.into_iter().enumerate() {
        let mut props = crate::value::tags_with_capacity(properties.len());
        for (name, values) in &properties {
            if let Some(Some(v)) = values.get(i) {
                props.insert(name.clone(), v.clone());
            }
        }
        features.push(Feature { id: ids.as_ref().and_then(|ids| ids[i]), geometry, properties: props });
    }
    Ok(Layer { name: table.name, features, extent })
}

/// Iterates over the `(tag, body)` blocks of an MLT tile.
pub fn layer_blocks(tile: &[u8]) -> impl Iterator<Item = anyhow::Result<(u32, &[u8])>> {
    let mut offset = 0usize;
    std::iter::from_fn(move || {
        if offset >= tile.len() {
            return None;
        }
        let result = (|| {
            let length = decode_varint(tile, &mut offset)? as usize;
            let tag_start = offset;
            let tag = decode_varint(tile, &mut offset)?;
            let body_len =
                length.checked_sub(offset - tag_start).ok_or_else(|| anyhow::anyhow!("invalid layer length"))?;
            let body = tile.get(offset..offset + body_len).ok_or_else(|| anyhow::anyhow!("truncated layer"))?;
            offset += body_len;
            Ok((tag, body))
        })();
        if result.is_err() {
            offset = tile.len();
        }
        Some(result)
    })
}

/// `MltDecoder.decodeMlTile`: decodes all tag `0x01` layers of an MLT tile.
pub fn decode(tile: &[u8]) -> anyhow::Result<MapboxVectorTile> {
    let mut layers = Vec::new();
    for block in layer_blocks(tile) {
        let (tag, body) = block?;
        if tag == 1 {
            layers.push(decode_layer(body)?);
        }
    }
    Ok(MapboxVectorTile { layers })
}

/// True if `bytes` look like an MLT tile rather than an MVT protobuf (MVT tiles start with field 3 = `0x1a`).
pub fn looks_like_mlt(bytes: &[u8]) -> bool {
    let mut offset = 0;
    match decode_varint(bytes, &mut offset) {
        Ok(len) => {
            let mut tag_offset = offset;
            bytes.first() != Some(&0x1a)
                && decode_varint(bytes, &mut tag_offset).is_ok_and(|t| t == 1)
                && offset + len as usize <= bytes.len()
        }
        Err(_) => false,
    }
}

/// Java `TileSizeStats.consumeColumn`: skips over a column, returning the number of attribute bytes it used (0 for
/// ID and geometry columns).
pub fn consume_column(column: &Column, tile: &[u8], offset: &mut usize) -> anyhow::Result<usize> {
    let num_streams = if column.has_stream_count() { decode_varint(tile, offset)? } else { 0 };
    let start = *offset;
    if num_streams == 0 {
        if column.nullable {
            skip_stream(tile, offset)?;
        }
        skip_stream(tile, offset)?;
    } else if let ColumnType::Struct(children) = &column.column_type {
        // skipOverSharedDictionary
        loop {
            let meta = skip_stream(tile, offset)?;
            if meta.physical_stream_type == PhysicalStreamType::Data
                && matches!(
                    meta.logical_stream_type,
                    Some(LogicalStreamType::Dictionary(DictionaryType::Single | DictionaryType::Shared))
                )
            {
                break;
            }
        }
        for child in children {
            consume_column(child, tile, offset)?;
        }
    } else {
        for _ in 0..num_streams {
            skip_stream(tile, offset)?;
        }
    }
    let size = *offset - start;
    Ok(if !column.is_geometry() && !column.is_id() { size } else { 0 })
}
