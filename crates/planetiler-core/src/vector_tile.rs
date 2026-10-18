//! Port of `VectorTile.java`.
//!
//! Encodes a single output tile containing [`Geometry`] features into the compact binary Mapbox Vector Tile format.
//!
//! This was originally copied (in Java) from
//! [VectorTileEncoder.java](https://github.com/ElectronicChartCentre/java-vector-tile/blob/master/src/main/java/no/ecc/vectortile/VectorTileEncoder.java)
//! and `VectorTileDecoder.java` and modified to decouple geometry encoding from vector tile encoding so that encoded
//! commands can be stored in the sorted feature map prior to encoding vector tiles.
//!
//! See the [Mapbox Vector Tile Specification](https://github.com/mapbox/vector-tile-spec/tree/master/2.1).

use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use geo::CoordsIter;
use geo::kernels::{Kernel, Orientation, RobustKernel};
use geo_types::{
    Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};
use indexmap::IndexSet;
use prost::Message;

use crate::geo::geometry_exception::{GeometryException, GeometryResult};
use crate::geo::geometry_type::GeometryType;
use crate::proto::vector_tile as pb;
use crate::util::hilbert;
use crate::util::layer_attr_stats;
use crate::value::{Tags, Value};

pub const LEFT: i32 = 1;
pub const RIGHT: i32 = 1 << 1;
pub const TOP: i32 = 1 << 2;
pub const BOTTOM: i32 = 1 << 3;
pub const INSIDE: i32 = 0;
pub const ALL: i32 = TOP | LEFT | RIGHT | BOTTOM;

pub const NO_FEATURE_ID: i64 = 0;

// TODO make these configurable
const EXTENT: i32 = 4096;
const SIZE: f64 = 256.0;

/// Vector tile geometry commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    MoveTo = 1,
    LineTo = 2,
    ClosePath = 7,
}

impl Command {
    pub const fn value(self) -> i32 {
        self as i32
    }
}

const MOVE_TO: i32 = Command::MoveTo as i32;
const LINE_TO: i32 = Command::LineTo as i32;
const CLOSE_PATH: i32 = Command::ClosePath as i32;

/// Java's `Math.round(double)`: rounds half up (towards positive infinity), NaN to 0, saturating.
#[inline]
fn java_round(v: f64) -> i64 {
    let floor = v.floor();
    (if v - floor >= 0.5 { floor + 1.0 } else { floor }) as i64
}

/// `(int) Math.round(v)` in Java.
#[inline]
fn round_int(v: f64) -> i32 {
    java_round(v) as i32
}

/// Zig-zag encode a signed integer (see protobuf encoding docs).
#[inline]
pub fn zig_zag_encode(n: i32) -> i32 {
    (n << 1) ^ (n >> 31)
}

/// Zig-zag decode a signed integer (see protobuf encoding docs).
#[inline]
pub fn zig_zag_decode(n: i32) -> i32 {
    (n >> 1) ^ (-(n & 1))
}

#[inline]
fn command_and_length(command: i32, repeat: i32) -> i32 {
    (repeat << 3) | command
}

/// The empty geometry (Java's `GeoUtils.EMPTY_GEOMETRY`).
// TODO(port): switch to a shared constant in geo_utils once one exists.
fn empty_geometry() -> Geometry<f64> {
    Geometry::GeometryCollection(GeometryCollection(Vec::new()))
}

/// Port of JTS `Orientation.isCCW(CoordinateSequence)` (returns false for flat or degenerate rings).
// TODO(port): move to geo/ once a shared JTS orientation port exists.
fn is_ccw(ring: &[Coord<f64>]) -> bool {
    if ring.is_empty() {
        return false;
    }
    let n_pts = ring.len() - 1;
    if n_pts < 3 {
        return false;
    }
    let mut up_hi_pt = ring[0];
    let mut prev_y = up_hi_pt.y;
    let mut up_low_pt = ring[0];
    let mut i_up_hi = 0;
    for i in 1..=n_pts {
        let py = ring[i].y;
        if py > prev_y && py >= up_hi_pt.y {
            up_hi_pt = ring[i];
            i_up_hi = i;
            up_low_pt = ring[i - 1];
        }
        prev_y = py;
    }
    if i_up_hi == 0 {
        return false;
    }
    let mut i_down_low = i_up_hi;
    loop {
        i_down_low = (i_down_low + 1) % n_pts;
        if i_down_low == i_up_hi || ring[i_down_low].y != up_hi_pt.y {
            break;
        }
    }
    let down_low_pt = ring[i_down_low];
    let i_down_hi = if i_down_low > 0 { i_down_low - 1 } else { n_pts - 1 };
    let down_hi_pt = ring[i_down_hi];
    if up_hi_pt == down_hi_pt {
        if up_low_pt == up_hi_pt || down_low_pt == up_hi_pt || up_low_pt == down_low_pt {
            return false;
        }
        RobustKernel::orient2d(up_low_pt, up_hi_pt, down_low_pt) == Orientation::CounterClockwise
    } else {
        let del_x = down_hi_pt.x - up_hi_pt.x;
        del_x < 0.0
    }
}

thread_local! {
    // scratch buffer reused across geometry encodings so each one only allocates its final `Arc<[i32]>`
    static COMMAND_BUFFER: std::cell::Cell<Vec<i32>> = const { std::cell::Cell::new(Vec::new()) };
}

fn encode_commands(input: &Geometry<f64>, scale: u32) -> Arc<[i32]> {
    let mut buffer = COMMAND_BUFFER.take();
    buffer.clear();
    let mut encoder = CommandEncoder { result: buffer, ..CommandEncoder::new(scale) };
    encoder.accept(input);
    let result: Arc<[i32]> = Arc::from(&encoder.result[..]);
    COMMAND_BUFFER.set(encoder.result);
    result
}

/// Scales a geometry down by a factor of `2^scale` without materializing an intermediate geometry and returns the
/// encoded result.
fn unscale_commands(commands: &[i32], scale: u32, geom_type: GeometryType) -> Vec<i32> {
    let mut result: Vec<i32> = Vec::with_capacity(commands.len());
    let geometry_count = commands.len();
    let mut length = 0;
    let mut command = 0;
    let mut i = 0;
    let (mut in_x, mut in_y) = (0i32, 0i32);
    let (mut out_x, mut out_y) = (0i32, 0i32);
    let (mut start_x, mut start_y) = (0i32, 0i32);
    let scale_factor = 2f64.powi(-(scale as i32));
    let mut length_idx = 0;
    let mut move_to_idx = 0;
    let mut points_in_shape = 0usize;
    let mut first = true;
    let min_points = geom_type.min_points();
    while i < geometry_count {
        if length <= 0 {
            length = commands[i];
            i += 1;
            length_idx = result.len();
            result.push(length);
            command = length & ((1 << 3) - 1);
            length >>= 3;
        }

        if length > 0 {
            if command == MOVE_TO {
                // degenerate geometry, remove it from output entirely
                if !first && points_in_shape < min_points {
                    let prev_command = result[length_idx];
                    result.truncate(move_to_idx);
                    result.push(prev_command);
                    // reset deltas
                    out_x = start_x;
                    out_y = start_y;
                }
                // keep track of size of next shape...
                points_in_shape = 0;
                start_x = out_x;
                start_y = out_y;
                move_to_idx = result.len() - 1;
            }
            first = false;
            if command == CLOSE_PATH {
                points_in_shape += 1;
                length -= 1;
                continue;
            }

            let dx = zig_zag_decode(commands[i]);
            let dy = zig_zag_decode(commands[i + 1]);
            i += 2;
            length -= 1;

            in_x = in_x.wrapping_add(dx);
            in_y = in_y.wrapping_add(dy);

            let next_x = round_int(in_x as f64 * scale_factor);
            let next_y = round_int(in_y as f64 * scale_factor);

            if next_x == out_x && next_y == out_y && command == LINE_TO {
                let command_length = result[length_idx] - 8;
                if command_length < 8 {
                    // get rid of lineto section if empty
                    result.truncate(length_idx);
                } else {
                    result[length_idx] = command_length;
                }
            } else {
                points_in_shape += 1;
                result.push(zig_zag_encode(next_x.wrapping_sub(out_x)));
                result.push(zig_zag_encode(next_y.wrapping_sub(out_y)));
                out_x = next_x;
                out_y = next_y;
            }
        }
    }
    // degenerate geometry, remove it from output entirely
    if points_in_shape < min_points {
        result.truncate(move_to_idx);
    }
    result
}

fn decode_error(message: impl fmt::Display) -> GeometryException {
    GeometryException::new("decode_vector_tile", "Unable to decode geometry").with_cause(message)
}

/// JTS `LinearRing` construction validation.
fn validate_ring(ring: &[Coord<f64>]) -> GeometryResult<()> {
    if !ring.is_empty() && ring.first() != ring.last() {
        return Err(decode_error("Points of LinearRing do not form a closed linestring"));
    }
    if !ring.is_empty() && ring.len() < 4 {
        return Err(decode_error(format!(
            "Invalid number of points in LinearRing (found {} - must be 0 or >= 4)",
            ring.len()
        )));
    }
    Ok(())
}

fn decode_commands(geom_type: GeometryType, commands: &[i32], scale: f64) -> GeometryResult<Geometry<f64>> {
    let mut x = 0i32;
    let mut y = 0i32;

    let mut all_coord_seqs: Vec<Vec<Coord<f64>>> = Vec::new();

    let geometry_count = commands.len();
    let mut length = 0;
    let mut command = 0;
    let mut i = 0;
    while i < geometry_count {
        if length <= 0 {
            length = commands[i];
            i += 1;
            command = length & ((1 << 3) - 1);
            length >>= 3;
        }

        if length > 0 {
            if command == MOVE_TO {
                all_coord_seqs.push(Vec::new());
            } else if all_coord_seqs.is_empty() {
                return Err(decode_error("command before first move to"));
            }
            let current = all_coord_seqs.last_mut().expect("checked above");

            if command == CLOSE_PATH {
                if geom_type != GeometryType::Point && !current.is_empty() {
                    let first = current[0];
                    if current.last() != Some(&first) {
                        current.push(first);
                    }
                }
                length -= 1;
                continue;
            }

            if i + 1 >= geometry_count {
                return Err(decode_error("truncated geometry"));
            }
            let dx = zig_zag_decode(commands[i]);
            let dy = zig_zag_decode(commands[i + 1]);
            i += 2;
            length -= 1;

            x = x.wrapping_add(dx);
            y = y.wrapping_add(dy);

            current.push(Coord { x: x as f64 / scale, y: y as f64 / scale });
        }
    }

    let geometry: Option<Geometry<f64>> = match geom_type {
        GeometryType::Line => {
            let mut line_strings: Vec<LineString<f64>> =
                all_coord_seqs.into_iter().filter(|seq| seq.len() > 1).map(LineString::new).collect();
            match line_strings.len() {
                0 => None,
                1 => Some(Geometry::LineString(line_strings.pop().expect("len 1"))),
                _ => Some(Geometry::MultiLineString(MultiLineString::new(line_strings))),
            }
        }
        GeometryType::Point => {
            let mut points: Vec<Point<f64>> = Vec::with_capacity(all_coord_seqs.len());
            for seq in &all_coord_seqs {
                match seq.first() {
                    Some(c) => points.push(Point(*c)),
                    None => return Err(decode_error("empty point")),
                }
            }
            match points.len() {
                0 => None,
                1 => Some(Geometry::Point(points[0])),
                _ => Some(Geometry::MultiPoint(MultiPoint::new(points))),
            }
        }
        GeometryType::Polygon => {
            let mut polygon_rings: Vec<Vec<Vec<Coord<f64>>>> = Vec::new();
            let mut first = true;
            let mut outer_ccw = false;
            for seq in all_coord_seqs {
                // skip hole with too few coordinates
                if polygon_rings.last().is_some_and(|rings| !rings.is_empty()) && seq.len() < 2 {
                    continue;
                }
                validate_ring(&seq)?;
                let ccw = is_ccw(&seq);
                if first {
                    first = false;
                    outer_ccw = ccw;
                    // NOTE: Java asserts that outer rings are counter-clockwise here.
                }
                if ccw == outer_ccw || polygon_rings.is_empty() {
                    polygon_rings.push(Vec::new());
                }
                polygon_rings.last_mut().expect("non-empty").push(seq);
            }
            let mut polygons: Vec<Polygon<f64>> = polygon_rings
                .into_iter()
                .map(|mut rings| {
                    let shell = LineString::new(rings.remove(0));
                    Polygon::new(shell, rings.into_iter().map(LineString::new).collect())
                })
                .collect();
            match polygons.len() {
                0 => None,
                1 => Some(Geometry::Polygon(polygons.pop().expect("len 1"))),
                _ => Some(Geometry::MultiPolygon(MultiPolygon::new(polygons))),
            }
        }
        GeometryType::Unknown => None,
    };

    Ok(geometry.unwrap_or_else(empty_geometry))
}

fn decode_value(value: &pb::tile::Value) -> Value {
    if let Some(b) = value.bool_value {
        Value::Bool(b)
    } else if let Some(d) = value.double_value {
        Value::Double(d)
    } else if let Some(f) = value.float_value {
        Value::Float(f)
    } else if let Some(i) = value.int_value {
        Value::Int(i)
    } else if let Some(i) = value.sint_value {
        Value::Int(i)
    } else if let Some(i) = value.uint_value {
        Value::Int(i as i64)
    } else if let Some(s) = &value.string_value {
        Value::str(s)
    } else {
        Value::Null
    }
}

fn encode_value(value: &Value) -> pb::tile::Value {
    let mut result = pb::tile::Value::default();
    match value {
        Value::String(s) => result.string_value = Some(s.to_string()),
        Value::Int(i) => result.sint_value = Some(*i),
        Value::Float(f) => result.float_value = Some(*f),
        Value::Double(d) => result.double_value = Some(*d),
        Value::Bool(b) => result.bool_value = Some(*b),
        other => result.string_value = Some(other.to_string()),
    }
    result
}

/// Parses a binary-encoded vector tile protobuf into a list of features.
///
/// Does not decode geometries, but clients can call [`VectorGeometry::decode`] if needed. If `encoded` is compressed,
/// clients must decompress it first.
pub fn decode(encoded: &[u8]) -> anyhow::Result<Vec<Feature>> {
    let tile = pb::Tile::decode(encoded)?;
    let mut features = Vec::new();
    for layer in &tile.layers {
        debug_assert_eq!(4096, layer.extent.unwrap_or(4096));
        let values: Vec<Value> = layer.values.iter().map(decode_value).collect();
        for feature in &layer.features {
            let mut attrs = Tags::with_capacity(feature.tags.len() / 2);
            for pair in feature.tags.chunks(2) {
                let key_idx = pair[0] as usize;
                let value_idx = *pair.get(1).ok_or_else(|| anyhow::anyhow!("odd number of tags"))? as usize;
                let key =
                    layer.keys.get(key_idx).ok_or_else(|| anyhow::anyhow!("key index {key_idx} out of bounds"))?;
                let value =
                    values.get(value_idx).ok_or_else(|| anyhow::anyhow!("value index {value_idx} out of bounds"))?;
                attrs.insert(key.clone(), value.clone());
            }
            let geom_type = GeometryType::from_protobuf(feature.r#type());
            let commands: Vec<i32> = feature.geometry.iter().map(|&c| c as i32).collect();
            features.push(Feature::new(
                layer.name.clone(),
                feature.id.unwrap_or(0) as i64,
                VectorGeometry::new(commands, geom_type, 0),
                attrs,
            ));
        }
    }
    Ok(features)
}

/// Encodes a geometry according to the
/// [Geometry Encoding Specification](https://github.com/mapbox/vector-tile-spec/tree/master/2.1#43-geometry-encoding).
///
/// `geometry` should be in tile pixel coordinates (0-256).
pub fn encode_geometry(geometry: &Geometry<f64>) -> VectorGeometry {
    encode_geometry_scaled(geometry, 0)
}

/// Encodes a geometry with `scale` extra bits of precision (extent `4096 * 2^scale`).
pub fn encode_geometry_scaled(geometry: &Geometry<f64>, scale: u32) -> VectorGeometry {
    VectorGeometry::new(encode_commands(geometry, scale), GeometryType::type_of(geometry), scale)
}

/// Returns a new [`VectorGeometryMerger`] that combines encoded geometries of the same type into a merged multipoint,
/// multilinestring, or multipolygon.
pub fn new_merger(geometry_type: GeometryType) -> VectorGeometryMerger {
    VectorGeometryMerger::new(geometry_type)
}

/// Returns the hilbert index of the zig-zag-encoded first point of `geometry`.
///
/// This can be useful for sorting geometries to minimize encoded vector tile geometry command size since smaller
/// offsets take fewer bytes using protobuf varint encoding.
pub fn hilbert_index(geometry: &Geometry<f64>) -> i32 {
    let Some(coord) = geometry.coords_iter().next() else {
        return 0;
    };
    let x = zig_zag_encode(round_int(coord.x * 4096.0 / 256.0));
    let y = zig_zag_encode(round_int(coord.y * 4096.0 / 256.0));
    hilbert::hilbert_xy_to_index(15, x as u32, y as u32) as i32
}

/// Returns the number of internal geometries in this feature including points/lines/polygons inside multigeometries.
pub fn count_geometries(feature: &pb::tile::Feature) -> usize {
    let mut result = 0usize;
    let mut idx = 0usize;
    let geom = &feature.geometry;
    while idx < geom.len() {
        let mut length = geom[idx] as i32;
        let command = length & ((1 << 3) - 1);
        length >>= 3;
        if command == MOVE_TO {
            result += length.max(0) as usize;
        }
        idx += 1;
        if command != CLOSE_PATH {
            idx += length.max(0) as usize * 2;
        }
    }
    result
}

/// Returns the encoded geometry for a polygon that fills an entire tile plus `buffer` pixels as a shortcut to avoid
/// needing to create an extra geometry for encoding.
pub fn encode_fill(buffer: f64) -> VectorGeometry {
    let min = round_int(EXTENT as f64 * buffer / 256.0);
    let width = EXTENT + min + min;
    VectorGeometry::new(
        vec![
            command_and_length(MOVE_TO, 1),
            zig_zag_encode(-min),
            zig_zag_encode(-min),
            command_and_length(LINE_TO, 3),
            zig_zag_encode(width),
            0,
            0,
            zig_zag_encode(width),
            zig_zag_encode(-width),
            0,
            command_and_length(CLOSE_PATH, 1),
        ],
        GeometryType::Polygon,
        0,
    )
}

/// Encodes a single output tile containing features into the compact binary Mapbox Vector Tile format.
///
/// Not thread safe.
#[derive(Default)]
pub struct VectorTile {
    // use a sorted map to ensure that layers are encoded in a consistent order
    layers: BTreeMap<String, Layer>,
    layer_stats_tracker: layer_attr_stats::ForZoom,
}

impl VectorTile {
    pub fn new() -> VectorTile {
        VectorTile::default()
    }

    /// Adds features in a layer to this tile.
    pub fn add_layer_features(&mut self, layer_name: &str, features: &[Feature]) -> &mut Self {
        if features.is_empty() {
            return self;
        }
        if !self.layers.contains_key(layer_name) {
            self.layers.insert(layer_name.to_string(), Layer::default());
        }
        let layer = self.layers.get_mut(layer_name).expect("inserted above");
        let mut stats_tracker = self.layer_stats_tracker.for_layer(layer_name);

        layer.encoded_features.reserve(features.len());
        for in_feature in features {
            if !in_feature.geometry.commands.is_empty() {
                let tags_start = layer.tag_data.len() as u32;
                for (key, value) in &in_feature.tags {
                    // skip attribute without value
                    if !value.is_null() {
                        let k = layer.key(key);
                        let v = layer.value(value);
                        layer.tag_data.push(k);
                        layer.tag_data.push(v);
                        stats_tracker.accept(key, value);
                    }
                }
                layer.encoded_features.push(EncodedFeature {
                    tags_start,
                    tags_end: layer.tag_data.len() as u32,
                    id: in_feature.id,
                    geometry: in_feature.geometry.clone(),
                });
            }
        }
        drop(stats_tracker);
        self
    }

    /// Returns a vector tile protobuf object with all features in this tile.
    ///
    /// `include_ids`: true to set IDs on each feature, false to exclude.
    pub fn to_proto(&self, include_ids: bool) -> pb::Tile {
        let mut tile = pb::Tile::default();
        for (layer_name, layer) in &self.layers {
            if layer.encoded_features.is_empty() {
                continue;
            }
            let mut tile_layer = pb::tile::Layer {
                version: 2,
                name: layer_name.clone(),
                extent: Some(EXTENT as u32),
                keys: layer.keys.iter().cloned().collect(),
                values: layer.values.iter().map(encode_value).collect(),
                features: Vec::with_capacity(layer.encoded_features.len()),
            };
            for feature in &layer.encoded_features {
                let mut feature_builder = pb::tile::Feature {
                    tags: layer.tags_of(feature).to_vec(),
                    r#type: Some(feature.geometry.geom_type.as_protobuf() as i32),
                    geometry: feature.geometry.commands.iter().map(|&c| c as u32).collect(),
                    id: None,
                };
                if include_ids && feature.id != NO_FEATURE_ID {
                    feature_builder.id = Some(feature.id as u64);
                }
                tile_layer.features.push(feature_builder);
            }
            tile.layers.push(tile_layer);
        }
        tile
    }

    /// Creates a vector tile protobuf with all features in this tile (including IDs) and serializes it as a byte array.
    ///
    /// Does not compress the result.
    pub fn encode(&self) -> Vec<u8> {
        self.encode_with_ids(true)
    }

    /// Serializes this tile directly to protobuf bytes, byte-identical to
    /// `self.to_proto(include_ids).encode_to_vec()` but without building intermediate protobuf structs.
    pub fn encode_with_ids(&self, include_ids: bool) -> Vec<u8> {
        // First pass: compute the size of every nested message so the output can be written in one pass into an
        // exactly-sized buffer.
        let mut feature_sizes: Vec<FeatureSize> = Vec::new();
        let mut layer_sizes: Vec<usize> = Vec::with_capacity(self.layers.len());
        let mut total = 0usize;
        for (layer_name, layer) in self.non_empty_layers() {
            let mut len = len_field_size(layer_name.len());
            feature_sizes.reserve(layer.encoded_features.len());
            for feature in &layer.encoded_features {
                let size = FeatureSize::compute(layer.tags_of(feature), feature, include_ids);
                len += len_field_size(size.total);
                feature_sizes.push(size);
            }
            for key in &layer.keys {
                len += len_field_size(key.len());
            }
            for value in &layer.values {
                len += len_field_size(encoded_value_size(value));
            }
            // extent (field 5) + version (field 15)
            len += 1 + varint_size(EXTENT as u64) + 2;
            layer_sizes.push(len);
            total += len_field_size(len);
        }

        let mut out = PbWriter::with_capacity(total);
        let mut sizes = feature_sizes.iter();
        for ((layer_name, layer), &layer_len) in self.non_empty_layers().zip(&layer_sizes) {
            out.tag_len(0x1a, layer_len);
            out.tag_len(0x0a, layer_name.len());
            out.bytes(layer_name.as_bytes());
            for feature in &layer.encoded_features {
                let size = sizes.next().expect("feature size");
                out.tag_len(0x12, size.total);
                if include_ids && feature.id != NO_FEATURE_ID {
                    out.byte(0x08);
                    out.varint(feature.id as u64);
                }
                let tags = layer.tags_of(feature);
                if !tags.is_empty() {
                    out.tag_len(0x12, size.tags);
                    for &t in tags {
                        out.varint32(t);
                    }
                }
                out.byte(0x18);
                out.varint(geom_type_varint(feature));
                let commands = feature.geometry.commands();
                if !commands.is_empty() {
                    out.tag_len(0x22, size.geometry);
                    for &c in commands {
                        out.varint32(c as u32);
                    }
                }
            }
            for key in &layer.keys {
                out.tag_len(0x1a, key.len());
                out.bytes(key.as_bytes());
            }
            for value in &layer.values {
                out.tag_len(0x22, encoded_value_size(value));
                write_value(&mut out, value);
            }
            out.byte(0x28);
            out.varint(EXTENT as u64);
            out.byte(0x78);
            out.byte(2);
        }
        out.finish()
    }

    fn non_empty_layers(&self) -> impl Iterator<Item = (&String, &Layer)> {
        self.layers.iter().filter(|(_, l)| !l.encoded_features.is_empty())
    }

    /// Returns true if this tile contains only polygon fills.
    pub fn contains_only_fills(&self) -> bool {
        self.contains_only_fills_or_edges_impl(false)
    }

    /// Returns true if this tile contains only polygon fills or horizontal/vertical edges that are likely to be
    /// repeated across tiles.
    pub fn contains_only_fills_or_edges(&self) -> bool {
        self.contains_only_fills_or_edges_impl(true)
    }

    fn contains_only_fills_or_edges_impl(&self, allow_edges: bool) -> bool {
        let mut empty = true;
        for layer in self.layers.values() {
            for feature in &layer.encoded_features {
                empty = false;
                if !feature.geometry.is_fill_or_edge_with(allow_edges) {
                    return false;
                }
            }
        }
        !empty
    }

    /// Determine whether a tile is likely to be a duplicate of some other tile hence it makes sense to calculate a
    /// hash for it.
    ///
    /// The [`VectorTile::contains_only_fills_or_edges`] heuristic catches >99.9% of repeated tiles and cuts down the
    /// number of tile hashes we need to track by 98%.
    pub fn likely_to_be_duplicated(&self) -> bool {
        self.layers.values().all(|l| l.encoded_features.is_empty()) || self.contains_only_fills_or_edges()
    }

    /// Call back to `layer_stats` as vector tile features are being encoded in
    /// [`VectorTile::add_layer_features`] to track attribute types present on features in each layer.
    pub fn track_layer_stats(&mut self, layer_stats: layer_attr_stats::ForZoom) {
        self.layer_stats_tracker = layer_stats;
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Converts this tile to the input of the MapLibre Tile (MLT) encoder.
    // TODO(port): Java delegates to `org.maplibre.mlt` (`toMltInput` -> `MapboxVectorTile`, then
    // `MltConverter.convertMvt`). MLT encoding is not ported yet.
    pub fn encode_mlt(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("MLT (MapLibre Tile) output format is not supported yet in the Rust port")
    }

    /// Number of distinct keys in `layer`, or `None` if the layer does not exist.
    pub fn get_num_keys(&self, layer: &str) -> Option<usize> {
        self.layers.get(layer).map(|l| l.keys.len())
    }

    /// Number of distinct values in `layer`, or `None` if the layer does not exist.
    pub fn get_num_values(&self, layer: &str) -> Option<usize> {
        self.layers.get(layer).map(|l| l.values.len())
    }

    /// Returns the decoded features of each layer along with their attributes, with tile pixel geometries scaled to
    /// the tile extent (0-4096 instead of 0-256). Used as MLT input in Java.
    pub fn layers_to_extent_geometries(&self) -> Vec<(String, Vec<ExtentFeature>)> {
        self.layers
            .iter()
            .filter(|(_, layer)| !layer.encoded_features.is_empty())
            .map(|(name, layer)| {
                let features = layer
                    .encoded_features
                    .iter()
                    .map(|feature| {
                        let tags = layer.tags_of(feature);
                        let mut properties = Tags::with_capacity(tags.len() / 2);
                        for pair in tags.as_chunks::<2>().0 {
                            properties
                                .insert(layer.keys[pair[0] as usize].clone(), layer.values[pair[1] as usize].clone());
                        }
                        (feature.id, feature.geometry.decode_to_extent(), properties)
                    })
                    .collect();
                (name.clone(), features)
            })
            .collect()
    }
}

/// A decoded feature in tile extent coordinates: `(id, geometry, attributes)`.
pub type ExtentFeature = (i64, GeometryResult<Geometry<f64>>, Tags);

/// Utility that combines encoded geometries of the same type into a merged multipoint, multilinestring, or
/// multipolygon.
pub struct VectorGeometryMerger {
    // For the most part this just concatenates the individual command arrays together
    // EXCEPT we need to adjust the first coordinate of each subsequent linestring to
    // be an offset from the end of the previous linestring.
    // AND we need to combine all multipoint "move to" commands into one at the start of
    // the sequence
    geometry_type: GeometryType,
    result: Vec<i32>,
    overall_x: i32,
    overall_y: i32,
}

impl VectorGeometryMerger {
    fn new(geometry_type: GeometryType) -> Self {
        VectorGeometryMerger { geometry_type, result: Vec::new(), overall_x: 0, overall_y: 0 }
    }

    /// Adds `vector_geometry` to the merged result. Fails if it has a different type than this merger.
    pub fn accept(&mut self, vector_geometry: &VectorGeometry) -> anyhow::Result<()> {
        if vector_geometry.geom_type != self.geometry_type {
            let name = format!("{:?}", vector_geometry.geom_type).to_lowercase();
            anyhow::bail!("Cannot merge a {name} geometry into a multi{name}");
        }
        if vector_geometry.is_empty() {
            return Ok(());
        }
        let unscaled = vector_geometry.unscale();
        let commands = unscaled.commands();
        // NOTE: Java throws an index-out-of-bounds exception here when the geometry becomes empty after unscaling.
        if commands.len() < 3 {
            return Ok(());
        }
        let mut x = 0i32;
        let mut y = 0i32;

        let geometry_count = commands.len();
        let mut length = 0;
        let mut command = 0;
        let mut i = 0;

        let result = &mut self.result;
        result.reserve(commands.len());
        // and multipoints will end up with only one command ("move to" with length=# points)
        if self.geometry_type != GeometryType::Point || result.is_empty() {
            result.push(commands[0]);
        }
        result.push(zig_zag_encode(zig_zag_decode(commands[1]).wrapping_sub(self.overall_x)));
        result.push(zig_zag_encode(zig_zag_decode(commands[2]).wrapping_sub(self.overall_y)));
        result.extend_from_slice(&commands[3..]);

        while i < geometry_count {
            if length <= 0 {
                length = commands[i];
                i += 1;
                command = length & ((1 << 3) - 1);
                length >>= 3;
            }

            if length > 0 {
                length -= 1;
                if command != CLOSE_PATH {
                    x = x.wrapping_add(zig_zag_decode(commands[i]));
                    y = y.wrapping_add(zig_zag_decode(commands[i + 1]));
                    i += 2;
                }
            }
        }
        self.overall_x = x;
        self.overall_y = y;
        Ok(())
    }

    /// Returns the merged multi-geometry.
    pub fn finish(mut self) -> VectorGeometry {
        // set the correct "move to" length for multipoints based on how many points were actually added
        if self.geometry_type == GeometryType::Point && !self.result.is_empty() {
            self.result[0] = MOVE_TO | (((self.result.len() as i32 - 1) / 2) << 3);
        }
        VectorGeometry::new(self.result, self.geometry_type, 0)
    }
}

/// A vector geometry encoded as a list of commands according to the
/// [vector tile specification](https://github.com/mapbox/vector-tile-spec/tree/master/2.1#43-geometry-encoding).
///
/// To encode extra precision in intermediate feature geometries, the geometry contained in `commands` is scaled to a
/// tile extent of `EXTENT * 2^scale`, so when `scale == 0` the extent is 4096 and when `scale == 2` the extent is
/// 4x4096. Geometries must be scaled back to 0 using [`VectorGeometry::unscale`] before outputting to the archive.
///
/// Equality and hashing consider only `commands` and `geom_type` (not `scale`), matching Java.
#[derive(Clone)]
pub struct VectorGeometry {
    commands: Arc<[i32]>,
    geom_type: GeometryType,
    scale: u32,
}

impl VectorGeometry {
    pub fn new(commands: impl Into<Arc<[i32]>>, geom_type: GeometryType, scale: u32) -> VectorGeometry {
        VectorGeometry { commands: commands.into(), geom_type, scale }
    }

    fn empty_point() -> VectorGeometry {
        VectorGeometry::new(Vec::new(), GeometryType::Point, 0)
    }

    pub fn commands(&self) -> &[i32] {
        &self.commands
    }

    pub fn geom_type(&self) -> GeometryType {
        self.geom_type
    }

    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// Returns [`LEFT`] or [`RIGHT`] bitwise OR'd with [`TOP`] or [`BOTTOM`] to indicate which side of the rectangle
    /// from `(x=min, y=min)` to `(x=max, y=max)` the `(x,y)` point falls in.
    pub fn get_side(x: i32, y: i32, min: i32, max: i32) -> i32 {
        (if x < min {
            LEFT
        } else if x > max {
            RIGHT
        } else {
            INSIDE
        }) | (if y < min {
            TOP
        } else if y > max {
            BOTTOM
        } else {
            INSIDE
        })
    }

    /// Same as [`VectorGeometry::get_side`] for floating-point coordinates.
    pub fn get_side_f64(x: f64, y: f64, min: f64, max: f64) -> i32 {
        (if x < min {
            LEFT
        } else if x > max {
            RIGHT
        } else {
            INSIDE
        }) | (if y < min {
            TOP
        } else if y > max {
            BOTTOM
        } else {
            INSIDE
        })
    }

    fn slanted(x1: i32, y1: i32, x2: i32, y2: i32) -> bool {
        x1 != x2 && y1 != y2
    }

    /// Returns `false` if the segment from a point in `side1` to `side2` is definitely outside of the rectangle, or
    /// true if it might cross the inside where `side1` and `side2` are from [`VectorGeometry::get_side`].
    pub fn segment_crosses_tile(side1: i32, side2: i32) -> bool {
        (side1 & side2) == 0
    }

    fn is_segment_invalid(allow_edges: bool, x1: i32, y1: i32, x2: i32, y2: i32, extent: i32) -> bool {
        let crosses_tile =
            Self::segment_crosses_tile(Self::get_side(x1, y1, 0, extent), Self::get_side(x2, y2, 0, extent));
        if allow_edges { crosses_tile && Self::slanted(x1, y1, x2, y2) } else { crosses_tile }
    }

    fn visited_enough_sides(allow_edges: bool, sides: i32) -> bool {
        if allow_edges {
            ((sides & LEFT) > 0 && (sides & RIGHT) > 0) || ((sides & TOP) > 0 && (sides & BOTTOM) > 0)
        } else {
            sides == ALL
        }
    }

    /// Converts an encoded geometry back to a geometry in tile pixel coordinates (0-256).
    pub fn decode(&self) -> GeometryResult<Geometry<f64>> {
        decode_commands(self.geom_type, &self.commands, ((EXTENT << self.scale) as f64) / SIZE)
    }

    /// Converts an encoded geometry back to a geometry in tile extent coordinates (0-4096).
    pub fn decode_to_extent(&self) -> GeometryResult<Geometry<f64>> {
        decode_commands(self.geom_type, &self.commands, (1i64 << self.scale) as f64)
    }

    /// Returns this encoded geometry, scaled back to 0, so it is safe to emit to archive output.
    pub fn unscale(&self) -> VectorGeometry {
        if self.scale == 0 {
            self.clone()
        } else {
            VectorGeometry::new(unscale_commands(&self.commands, self.scale, self.geom_type), self.geom_type, 0)
        }
    }

    /// Returns true if the encoded geometry is a polygon fill.
    pub fn is_fill(&self) -> bool {
        self.is_fill_or_edge_with(false)
    }

    /// Returns true if the encoded geometry is a polygon fill, rectangle edge, or part of a horizontal/vertical line
    /// that is likely to be repeated across tiles.
    pub fn is_fill_or_edge(&self) -> bool {
        self.is_fill_or_edge_with(true)
    }

    /// Returns true if the encoded geometry is a polygon fill, or if `allow_edges == true` then also a rectangle edge,
    /// or part of a horizontal/vertical line that is likely to be repeated across tiles.
    pub fn is_fill_or_edge_with(&self, allow_edges: bool) -> bool {
        if self.geom_type != GeometryType::Polygon && (!allow_edges || self.geom_type != GeometryType::Line) {
            return false;
        }

        let is_line = self.geom_type == GeometryType::Line;

        let extent = EXTENT << self.scale;
        let mut visited = INSIDE;
        let mut first_x = 0;
        let mut first_y = 0;
        let mut x = 0i32;
        let mut y = 0i32;

        let commands = &self.commands;
        let geometry_count = commands.len();
        let mut length = 0;
        let mut command = 0;
        let mut i = 0;
        while i < geometry_count {
            if length <= 0 {
                length = commands[i];
                i += 1;
                command = length & ((1 << 3) - 1);
                length >>= 3;
                if is_line && length > 2 {
                    return false;
                }
            }

            if length > 0 {
                if command == CLOSE_PATH {
                    if Self::is_segment_invalid(allow_edges, x, y, first_x, first_y, extent)
                        || !Self::visited_enough_sides(allow_edges, visited)
                    {
                        return false;
                    }
                    length -= 1;
                    continue;
                }

                let dx = zig_zag_decode(commands[i]);
                let dy = zig_zag_decode(commands[i + 1]);
                i += 2;
                length -= 1;

                let next_x = x.wrapping_add(dx);
                let next_y = y.wrapping_add(dy);

                if command == MOVE_TO {
                    first_x = next_x;
                    first_y = next_y;
                    visited = Self::get_side(first_x, first_y, 0, extent);
                    if visited == INSIDE {
                        return false;
                    }
                } else {
                    if Self::is_segment_invalid(allow_edges, x, y, next_x, next_y, extent) {
                        return false;
                    }
                    visited |= Self::get_side(next_x, next_y, 0, extent);
                }
                y = next_y;
                x = next_x;
            }
        }

        Self::visited_enough_sides(allow_edges, visited)
    }

    /// Returns true if there are no commands in this geometry.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// If this is a point, returns an empty geometry if more than `buffer` pixels outside the tile bounds, or if it is
    /// a multipoint then removes all points outside the buffer.
    pub fn filter_points_outside_buffer(&self, buffer: f64) -> VectorGeometry {
        if self.geom_type != GeometryType::Point {
            return self.clone();
        }
        let commands = &self.commands;
        let mut result: Option<Vec<i32>> = None;

        let extent = EXTENT << self.scale;
        let buffer_int = (buffer * extent as f64 / 256.0).ceil() as i32;
        let min = -buffer_int;
        let max = extent + buffer_int;

        let mut x = 0i32;
        let mut y = 0i32;
        let mut last_x = 0i32;
        let mut last_y = 0i32;

        let geometry_count = commands.len();
        let mut length = 0;
        let mut i = 0;

        while i < geometry_count {
            if length <= 0 {
                length = commands[i] >> 3;
                i += 1;
                debug_assert!(i <= 1, "Bad index {i}");
            }

            if length > 0 {
                length -= 1;
                x = x.wrapping_add(zig_zag_decode(commands[i]));
                y = y.wrapping_add(zig_zag_decode(commands[i + 1]));
                i += 2;
                if x < min || y < min || x > max || y > max {
                    if result.is_none() {
                        // short-circuit the common case of only a single point that gets filtered-out
                        if commands.len() == 3 {
                            return Self::empty_point();
                        }
                        let mut r = Vec::with_capacity(commands.len());
                        r.extend_from_slice(&commands[0..i - 2]);
                        result = Some(r);
                    }
                } else {
                    if let Some(r) = result.as_mut() {
                        r.push(zig_zag_encode(x.wrapping_sub(last_x)));
                        r.push(zig_zag_encode(y.wrapping_sub(last_y)));
                    }
                    last_x = x;
                    last_y = y;
                }
            }
        }
        match result {
            Some(mut r) => {
                if r.len() < 3 {
                    r.clear();
                } else {
                    r[0] = MOVE_TO | (((r.len() as i32 - 1) / 2) << 3);
                }
                VectorGeometry::new(r, self.geom_type, self.scale)
            }
            None => self.clone(),
        }
    }

    /// Returns the hilbert index of the zig-zag-encoded first point of this feature.
    ///
    /// This can be useful for sorting geometries to minimize encoded vector tile geometry command size since smaller
    /// offsets take fewer bytes using protobuf varint encoding.
    pub fn hilbert_index(&self) -> i32 {
        if self.commands.len() < 3 {
            return 0;
        }
        let x = self.commands[1];
        let y = self.commands[2];
        hilbert::hilbert_xy_to_index(15, (x >> self.scale) as u32, (y >> self.scale) as u32) as i32
    }

    /// Returns the coordinate of the first point in this geometry in tile pixel coordinates from (0,0) at the top left
    /// to (256,256) at the bottom right.
    pub fn first_coordinate(&self) -> Option<Coord<f64>> {
        if self.commands.len() < 3 {
            return None;
        }
        let factor = (1i64 << self.scale) as f64;
        let x = zig_zag_decode(self.commands[1]) as f64 * SIZE / EXTENT as f64 / factor;
        let y = zig_zag_decode(self.commands[2]) as f64 * SIZE / EXTENT as f64 / factor;
        Some(Coord { x, y })
    }
}

impl PartialEq for VectorGeometry {
    fn eq(&self, other: &Self) -> bool {
        self.geom_type == other.geom_type && self.commands == other.commands
    }
}

impl Eq for VectorGeometry {}

impl Hash for VectorGeometry {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.commands.hash(state);
        self.geom_type.hash(state);
    }
}

impl fmt::Display for VectorGeometry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "VectorGeometry[commands=int[{}], geomType={} ({})]",
            self.commands.len(),
            format!("{:?}", self.geom_type).to_uppercase(),
            self.geom_type.as_byte()
        )
    }
}

impl fmt::Debug for VectorGeometry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VectorGeometry")
            .field("commands", &&self.commands[..])
            .field("geom_type", &self.geom_type)
            .field("scale", &self.scale)
            .finish()
    }
}

/// A feature in a vector tile.
///
/// - `layer`: the layer the feature was in
/// - `id`: the feature ID
/// - `geometry`: the encoded feature geometry (decode using [`VectorGeometry::decode`])
/// - `tags`: tags for the feature to output
/// - `group`: grouping key used to limit point density or [`Feature::NO_GROUP`] if not in a group. NOTE: this is only
///   populated when this feature was deserialized from `FeatureGroup`, not when parsed from a tile since vector tile
///   schema does not encode group.
// TODO(port): implement `crate::reader::with_tags::WithTags` once that trait is ported.
#[derive(Clone, Debug, PartialEq)]
pub struct Feature {
    pub layer: String,
    pub id: i64,
    pub geometry: VectorGeometry,
    pub tags: Tags,
    pub group: i64,
}

impl Feature {
    pub const NO_GROUP: i64 = i64::MIN;

    /// A feature without a group.
    pub fn new(layer: impl Into<String>, id: i64, geometry: VectorGeometry, tags: Tags) -> Feature {
        Feature { layer: layer.into(), id, geometry, tags, group: Self::NO_GROUP }
    }

    pub fn with_group(layer: impl Into<String>, id: i64, geometry: VectorGeometry, tags: Tags, group: i64) -> Feature {
        Feature { layer: layer.into(), id, geometry, tags, group }
    }

    pub fn has_group(&self) -> bool {
        self.group != Self::NO_GROUP
    }

    /// Encodes `new_geometry` and returns a copy of this feature with `geometry` replaced with the encoded new
    /// geometry.
    pub fn copy_with_new_geometry(&self, new_geometry: &Geometry<f64>) -> Feature {
        self.copy_with_new_vector_geometry(encode_geometry(new_geometry))
    }

    /// Returns a copy of this feature with `geometry` replaced with `new_geometry`.
    pub fn copy_with_new_vector_geometry(&self, new_geometry: VectorGeometry) -> Feature {
        Feature { geometry: new_geometry, ..self.clone() }
    }

    /// Returns a copy of this feature with `id` and `geometry` replaced.
    pub fn copy_with_id_and_geometry(&self, new_id: i64, new_geometry: VectorGeometry) -> Feature {
        Feature { id: new_id, geometry: new_geometry, ..self.clone() }
    }

    /// Returns a copy of this feature with `extra_attrs` added to `tags`.
    ///
    /// NOTE: Java throws on duplicate keys; here values from `extra_attrs` replace existing ones.
    pub fn copy_with_extra_attrs(&self, extra_attrs: &Tags) -> Feature {
        let mut tags = self.tags.clone();
        for (k, v) in extra_attrs {
            tags.insert(k.clone(), v.clone());
        }
        Feature { tags, ..self.clone() }
    }
}

/// Encodes a geometry as a sequence of integers according to the
/// [Geometry Encoding Specification](https://github.com/mapbox/vector-tile-spec/tree/master/2.1#43-geometry-encoding).
struct CommandEncoder {
    result: Vec<i32>,
    scale: f64,
    // Initial points use absolute locations, then subsequent points in a geometry use offsets so
    // need to keep track of previous x/y location during the encoding.
    x: i32,
    y: i32,
}

impl CommandEncoder {
    fn new(scale: u32) -> Self {
        CommandEncoder { result: Vec::new(), scale: ((EXTENT << scale) as f64) / SIZE, x: 0, y: 0 }
    }

    fn accept_polygon(&mut self, polygon: &Polygon<f64>) {
        self.encode(&polygon.exterior().0, true, false, GeometryType::Polygon);
        for interior in polygon.interiors() {
            self.encode(&interior.0, true, false, GeometryType::Line);
        }
    }

    fn accept(&mut self, geometry: &Geometry<f64>) {
        match geometry {
            Geometry::MultiLineString(mls) => {
                for line in &mls.0 {
                    self.encode(&line.0, false, false, GeometryType::Line);
                }
            }
            Geometry::Polygon(polygon) => self.accept_polygon(polygon),
            Geometry::MultiPolygon(mp) => {
                for polygon in &mp.0 {
                    self.accept_polygon(polygon);
                }
            }
            Geometry::LineString(line) => self.encode(&line.0, false, false, GeometryType::Line),
            Geometry::Line(line) => self.encode(&[line.start, line.end], false, false, GeometryType::Line),
            Geometry::Point(point) => self.encode(&[point.0], false, false, GeometryType::Point),
            Geometry::MultiPoint(mp) => {
                let coords: Vec<Coord<f64>> = mp.0.iter().map(|p| p.0).collect();
                self.encode(&coords, false, true, GeometryType::Point);
            }
            Geometry::Rect(rect) => self.accept_polygon(&rect.to_polygon()),
            Geometry::Triangle(triangle) => self.accept_polygon(&triangle.to_polygon()),
            Geometry::GeometryCollection(_) => {
                tracing::warn!("Unrecognized geometry type: GeometryCollection");
            }
        }
    }

    fn encode(&mut self, cs: &[Coord<f64>], close_path_at_end: bool, multi_point: bool, geom_type: GeometryType) {
        if cs.is_empty() {
            // NOTE: Java throws IllegalArgumentException("empty geometry") here; we emit nothing instead.
            return;
        }

        let result = &mut self.result;
        let start_idx = result.len();
        let mut num_points = 0usize;
        let mut line_to_index = 0usize;
        let mut line_to_length: i32 = 0;
        let start_x = self.x;
        let start_y = self.y;
        let n = cs.len();

        for (i, c) in cs.iter().enumerate() {
            let cx = c.x;
            let cy = c.y;

            if i == 0 {
                result.push(command_and_length(MOVE_TO, if multi_point { n as i32 } else { 1 }));
            }

            let nx = round_int(cx * self.scale);
            let ny = round_int(cy * self.scale);

            // prevent point equal to the previous
            if i > 0 && nx == self.x && ny == self.y && !multi_point {
                line_to_length -= 1;
                continue;
            }

            // prevent double closing
            if close_path_at_end && n > 1 && i == n - 1 && cs[0].x == cx && cs[0].y == cy {
                line_to_length -= 1;
                continue;
            }

            // delta, then zigzag
            result.push(zig_zag_encode(nx.wrapping_sub(self.x)));
            result.push(zig_zag_encode(ny.wrapping_sub(self.y)));
            num_points += 1;

            self.x = nx;
            self.y = ny;

            if i == 0 && n > 1 && !multi_point {
                // can length be too long?
                line_to_index = result.len();
                line_to_length = n as i32 - 1;
                result.push(command_and_length(LINE_TO, line_to_length));
            }
        }

        // update LineTo length
        if line_to_index > 0 {
            if line_to_length == 0 {
                // remove empty LineTo
                result.remove(line_to_index);
            } else {
                // update LineTo with new length
                result[line_to_index] = command_and_length(LINE_TO, line_to_length);
            }
        }

        if close_path_at_end {
            result.push(command_and_length(CLOSE_PATH, 1));
            num_points += 1;
        }

        // degenerate geometry, skip emitting
        if num_points < geom_type.min_points() {
            result.truncate(start_idx);
            // reset deltas
            self.x = start_x;
            self.y = start_y;
        }
    }
}

/// Sizes of the nested parts of an encoded feature message.
struct FeatureSize {
    total: usize,
    tags: usize,
    geometry: usize,
}

impl FeatureSize {
    fn compute(tags: &[u32], feature: &EncodedFeature, include_ids: bool) -> FeatureSize {
        let mut total = 0;
        if include_ids && feature.id != NO_FEATURE_ID {
            total += 1 + varint_size(feature.id as u64);
        }
        let tags_len: usize = tags.iter().map(|&t| varint_size32(t)).sum();
        if !tags.is_empty() {
            total += len_field_size(tags_len);
        }
        total += 1 + varint_size(geom_type_varint(feature));
        let commands = feature.geometry.commands();
        let geometry_len: usize = commands.iter().map(|&c| varint_size32(c as u32)).sum();
        if !commands.is_empty() {
            total += len_field_size(geometry_len);
        }
        FeatureSize { total, tags: tags_len, geometry: geometry_len }
    }
}

/// The geometry type enum as a protobuf varint (`int32` enums are sign-extended to 64 bits).
#[inline]
fn geom_type_varint(feature: &EncodedFeature) -> u64 {
    feature.geometry.geom_type.as_protobuf() as i32 as i64 as u64
}

/// Number of bytes in the protobuf varint encoding of `v`.
#[inline]
fn varint_size(v: u64) -> usize {
    // 1 + floor(log2(v) / 7)
    ((64 - (v | 1).leading_zeros()) as usize * 9 + 64) / 64
}

#[inline]
fn varint_size32(v: u32) -> usize {
    ((32 - (v | 1).leading_zeros()) as usize * 9 + 64) / 64
}

/// Size of a length-delimited field with a single-byte tag and a `len`-byte payload.
#[inline]
fn len_field_size(len: usize) -> usize {
    1 + varint_size(len as u64) + len
}

fn value_string(value: &Value) -> std::borrow::Cow<'_, str> {
    match value {
        Value::String(s) => std::borrow::Cow::Borrowed(s),
        other => std::borrow::Cow::Owned(other.to_string()),
    }
}

/// Size of the encoded `Tile.Value` message for `value` (see [`encode_value`]).
fn encoded_value_size(value: &Value) -> usize {
    match value {
        Value::Int(i) => 1 + varint_size(zig_zag_encode64(*i)),
        Value::Float(_) => 5,
        Value::Double(_) => 9,
        Value::Bool(_) => 2,
        other => len_field_size(value_string(other).len()),
    }
}

/// Writes the `Tile.Value` message for `value` (see [`encode_value`]).
fn write_value(out: &mut PbWriter, value: &Value) {
    match value {
        Value::Int(i) => {
            out.byte(0x30);
            out.varint(zig_zag_encode64(*i));
        }
        Value::Float(f) => {
            out.byte(0x15);
            out.bytes(&f.to_le_bytes());
        }
        Value::Double(d) => {
            out.byte(0x19);
            out.bytes(&d.to_le_bytes());
        }
        Value::Bool(b) => {
            out.byte(0x38);
            out.byte(*b as u8);
        }
        other => {
            let s = value_string(other);
            out.tag_len(0x0a, s.len());
            out.bytes(s.as_bytes());
        }
    }
}

#[inline]
fn zig_zag_encode64(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

/// Minimal protobuf writer into a pre-sized buffer.
struct PbWriter {
    buf: Vec<u8>,
}

impl PbWriter {
    fn with_capacity(capacity: usize) -> PbWriter {
        PbWriter { buf: Vec::with_capacity(capacity) }
    }

    #[inline]
    fn byte(&mut self, b: u8) {
        self.buf.push(b);
    }

    #[inline]
    fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    #[inline]
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }

    #[inline]
    fn varint32(&mut self, v: u32) {
        if v < 0x80 {
            self.buf.push(v as u8);
        } else {
            self.varint(v as u64);
        }
    }

    #[inline]
    fn tag_len(&mut self, tag: u8, len: usize) {
        self.buf.push(tag);
        self.varint(len as u64);
    }

    fn finish(self) -> Vec<u8> {
        debug_assert_eq!(self.buf.len(), self.buf.capacity());
        self.buf
    }
}

struct EncodedFeature {
    // range of this feature's key/value index pairs in `Layer::tag_data`
    tags_start: u32,
    tags_end: u32,
    id: i64,
    geometry: VectorGeometry,
}

type FastIndexSet<T> = IndexSet<T, foldhash::fast::FixedState>;

/// Holds all features in an output layer of this tile, along with the index of each tag key/value so that features
/// can store each key/value as a pair of integers.
#[derive(Default)]
struct Layer {
    encoded_features: Vec<EncodedFeature>,
    // key/value index pairs of all features, stored contiguously to avoid an allocation per feature
    tag_data: Vec<u32>,
    keys: FastIndexSet<String>,
    values: FastIndexSet<Value>,
}

impl Layer {
    fn tags_of(&self, feature: &EncodedFeature) -> &[u32] {
        &self.tag_data[feature.tags_start as usize..feature.tags_end as usize]
    }

    /// Returns the ID associated with `key` or adds a new one if not present.
    fn key(&mut self, key: &str) -> u32 {
        match self.keys.get_index_of(key) {
            Some(i) => i as u32,
            None => self.keys.insert_full(key.to_string()).0 as u32,
        }
    }

    /// Returns the ID associated with `value` or adds a new one if not present.
    fn value(&mut self, value: &Value) -> u32 {
        match self.values.get_index_of(value) {
            Some(i) => i as u32,
            None => self.values.insert_full(value.clone()).0 as u32,
        }
    }
}

impl fmt::Debug for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Layer{{{}}}", self.encoded_features.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags;
    use geo::MapCoords;

    // ---- local geometry helpers (TODO(port): switch to crate::geo::test_utils) ----

    fn coords(c: &[f64]) -> Vec<Coord<f64>> {
        c.chunks(2).map(|p| Coord { x: p[0], y: p[1] }).collect()
    }

    fn new_point(x: f64, y: f64) -> Geometry<f64> {
        Geometry::Point(Point::new(x, y))
    }

    fn new_multi_point(points: &[Geometry<f64>]) -> Geometry<f64> {
        Geometry::MultiPoint(MultiPoint::new(
            points
                .iter()
                .map(|p| match p {
                    Geometry::Point(p) => *p,
                    _ => panic!("not a point"),
                })
                .collect(),
        ))
    }

    fn new_line_string(c: &[f64]) -> Geometry<f64> {
        Geometry::LineString(LineString::new(coords(c)))
    }

    fn new_multi_line_string(lines: &[Geometry<f64>]) -> Geometry<f64> {
        Geometry::MultiLineString(MultiLineString::new(
            lines
                .iter()
                .map(|l| match l {
                    Geometry::LineString(l) => l.clone(),
                    _ => panic!("not a line"),
                })
                .collect(),
        ))
    }

    fn polygon_of(c: &[f64]) -> Polygon<f64> {
        Polygon::new(LineString::new(coords(c)), vec![])
    }

    fn new_polygon(c: &[f64]) -> Geometry<f64> {
        Geometry::Polygon(polygon_of(c))
    }

    fn new_multi_polygon(polys: &[Geometry<f64>]) -> Geometry<f64> {
        Geometry::MultiPolygon(MultiPolygon::new(
            polys
                .iter()
                .map(|p| match p {
                    Geometry::Polygon(p) => p.clone(),
                    _ => panic!("not a polygon"),
                })
                .collect(),
        ))
    }

    fn rectangle(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Geometry<f64> {
        new_polygon(&[min_x, min_y, max_x, min_y, max_x, max_y, min_x, max_y, min_x, min_y])
    }

    fn rectangle_sq(min: f64, max: f64) -> Geometry<f64> {
        rectangle(min, min, max, max)
    }

    fn rotate(geom: &Geometry<f64>, degrees: f64, cx: f64, cy: f64) -> Geometry<f64> {
        let theta = std::f64::consts::PI * degrees / 180.0;
        let (sin, cos) = (theta.sin(), theta.cos());
        geom.map_coords(|c| {
            let (dx, dy) = (c.x - cx, c.y - cy);
            Coord { x: cx + dx * cos - dy * sin, y: cy + dx * sin + dy * cos }
        })
    }

    fn scale_geom(geom: &Geometry<f64>, factor: f64) -> Geometry<f64> {
        geom.map_coords(|c| Coord { x: c.x * factor, y: c.y * factor })
    }

    fn is_empty_geom(g: &Geometry<f64>) -> bool {
        g.coords_iter().next().is_none()
    }

    /// Normalize a ring: drop closing point, rotate to start at min coordinate, orient, re-close.
    fn norm_ring(ring: &[Coord<f64>], clockwise: bool) -> Vec<Coord<f64>> {
        let mut pts: Vec<Coord<f64>> = ring.to_vec();
        if pts.len() > 1 && pts.first() == pts.last() {
            pts.pop();
        }
        let area: f64 = (0..pts.len())
            .map(|i| {
                let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                a.x * b.y - b.x * a.y
            })
            .sum();
        if (area < 0.0) != clockwise {
            pts.reverse();
        }
        let min_idx = (0..pts.len())
            .min_by(|&a, &b| pts[a].x.total_cmp(&pts[b].x).then(pts[a].y.total_cmp(&pts[b].y)))
            .unwrap_or(0);
        pts.rotate_left(min_idx);
        if let Some(first) = pts.first().copied() {
            pts.push(first);
        }
        pts
    }

    fn norm_polygon(p: &Polygon<f64>) -> Polygon<f64> {
        let mut holes: Vec<Vec<Coord<f64>>> = p.interiors().iter().map(|r| norm_ring(&r.0, false)).collect();
        holes.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        Polygon::new(
            LineString::new(norm_ring(&p.exterior().0, true)),
            holes.into_iter().map(LineString::new).collect(),
        )
    }

    /// Simplified JTS `normalize()` for comparing geometries.
    fn normalize(g: &Geometry<f64>) -> Geometry<f64> {
        match g {
            Geometry::LineString(l) => {
                let mut c = l.0.clone();
                let rev: Vec<_> = c.iter().rev().copied().collect();
                if format!("{rev:?}") < format!("{c:?}") {
                    c = rev;
                }
                Geometry::LineString(LineString::new(c))
            }
            Geometry::MultiLineString(m) => {
                let mut lines: Vec<LineString<f64>> =
                    m.0.iter()
                        .map(|l| match normalize(&Geometry::LineString(l.clone())) {
                            Geometry::LineString(l) => l,
                            _ => unreachable!(),
                        })
                        .collect();
                lines.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
                Geometry::MultiLineString(MultiLineString::new(lines))
            }
            Geometry::MultiPoint(m) => {
                let mut pts = m.0.clone();
                pts.sort_by(|a, b| a.x().total_cmp(&b.x()).then(a.y().total_cmp(&b.y())));
                Geometry::MultiPoint(MultiPoint::new(pts))
            }
            Geometry::Polygon(p) => Geometry::Polygon(norm_polygon(p)),
            Geometry::MultiPolygon(m) => {
                let mut polys: Vec<Polygon<f64>> = m.0.iter().map(norm_polygon).collect();
                polys.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
                Geometry::MultiPolygon(MultiPolygon::new(polys))
            }
            other => other.clone(),
        }
    }

    fn assert_same_geometry(expected: &Geometry<f64>, actual: &Geometry<f64>) {
        if is_empty_geom(expected) && is_empty_geom(actual) {
            return;
        }
        assert_eq!(normalize(expected), normalize(actual));
    }

    /// Approximation of JTS `GeometryPrecisionReducer.reduce` for the simple geometries used in these tests: snap to
    /// grid, remove repeated points, and drop collapsed components.
    fn reduce(g: &Geometry<f64>, scale: f64) -> Geometry<f64> {
        let snap = |c: &Coord<f64>| Coord {
            x: java_round(c.x * scale) as f64 / scale,
            y: java_round(c.y * scale) as f64 / scale,
        };
        let snap_seq = |cs: &[Coord<f64>]| {
            let mut out: Vec<Coord<f64>> = Vec::new();
            for c in cs {
                let s = snap(c);
                if out.last() != Some(&s) {
                    out.push(s);
                }
            }
            out
        };
        let reduce_poly = |p: &Polygon<f64>| {
            let ext = snap_seq(&p.exterior().0);
            if ext.len() < 4 { None } else { Some(Polygon::new(LineString::new(ext), vec![])) }
        };
        let reduce_line = |l: &LineString<f64>| {
            let c = snap_seq(&l.0);
            if c.len() < 2 { None } else { Some(LineString::new(c)) }
        };
        match g {
            Geometry::Point(p) => Geometry::Point(Point(snap(&p.0))),
            Geometry::MultiPoint(m) => {
                Geometry::MultiPoint(MultiPoint::new(m.0.iter().map(|p| Point(snap(&p.0))).collect()))
            }
            Geometry::LineString(l) => reduce_line(l).map_or_else(empty_geometry, Geometry::LineString),
            Geometry::MultiLineString(m) => {
                let mut lines: Vec<_> = m.0.iter().filter_map(reduce_line).collect();
                match lines.len() {
                    0 => empty_geometry(),
                    1 => Geometry::LineString(lines.pop().unwrap()),
                    _ => Geometry::MultiLineString(MultiLineString::new(lines)),
                }
            }
            Geometry::Polygon(p) => reduce_poly(p).map_or_else(empty_geometry, Geometry::Polygon),
            Geometry::MultiPolygon(m) => {
                let mut polys: Vec<_> = m.0.iter().filter_map(reduce_poly).collect();
                match polys.len() {
                    0 => empty_geometry(),
                    1 => Geometry::Polygon(polys.pop().unwrap()),
                    _ => Geometry::MultiPolygon(MultiPolygon::new(polys)),
                }
            }
            other => other.clone(),
        }
    }

    // ---- tests ----

    const TILE_TRANSFORM: f64 = 4096.0 / 256.0;

    fn get_commands(geom: &Geometry<f64>) -> Vec<i32> {
        // TRANSFORM_TO_TILE in Java scales world coordinates to tile pixel coordinates: these tests already use tile
        // extent coordinates, so scale down to pixels first.
        encode_geometry(&scale_geom(geom, 1.0 / TILE_TRANSFORM)).commands().to_vec()
    }

    fn new_vector_tile_feature(layer: &str, geom: &Geometry<f64>, attrs: Tags) -> Feature {
        Feature::new(layer, 1, encode_geometry(geom), attrs)
    }

    fn decode_silently(g: &VectorGeometry) -> Geometry<f64> {
        g.decode().expect("decode")
    }

    fn assert_same_geometries(expected: &[Geometry<f64>], actual: &[Feature]) {
        let decoded: Vec<Geometry<f64>> = actual.iter().map(|f| decode_silently(&f.geometry)).collect();
        assert_eq!(expected, &decoded[..]);
    }

    #[test]
    fn test_to_geom_type() {
        let geometry = new_line_string(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(pb::tile::GeomType::Linestring as i32 as u8, encode_geometry(&geometry).geom_type().as_byte());
    }

    #[test]
    fn test_commands() {
        assert_eq!(
            vec![9, 6, 12, 18, 10, 12, 24, 44, 15],
            get_commands(&new_polygon(&[3.0, 6.0, 8.0, 12.0, 20.0, 34.0, 3.0, 6.0]))
        );
    }

    #[test]
    fn test_commands_filter() {
        assert_eq!(
            vec![9, 6, 12, 18, 10, 12, 24, 44, 15],
            get_commands(&new_polygon(&[3.0, 6.0, 8.0, 12.0, 8.0, 12.0, 20.0, 34.0, 3.0, 6.0]))
        );
    }

    #[test]
    fn test_point() {
        assert_eq!(vec![9, 6, 12], get_commands(&new_multi_point(&[new_point(3.0, 6.0)])));
    }

    #[test]
    fn test_multi_point() {
        assert_eq!(vec![17, 10, 14, 3, 9], get_commands(&new_multi_point(&[new_point(5.0, 7.0), new_point(3.0, 2.0)])));
    }

    #[test]
    fn test_null_attribute_value() {
        let mut vtm = VectorTile::new();
        let attrs = tags! {"key1" => "value1", "key2" => Value::Null, "key3" => "value3"};
        vtm.add_layer_features("DEPCNT", &[new_vector_tile_feature("DEPCNT", &new_point(3.0, 6.0), attrs)]);
        let encoded = vtm.encode();
        assert!(!encoded.is_empty());
        let decoded = decode(&encoded).unwrap();
        assert_eq!(
            vec![Feature::new(
                "DEPCNT",
                1,
                encode_geometry(&new_point(3.0, 6.0)),
                tags! {"key1" => "value1", "key3" => "value3"}
            )],
            decoded
        );
        assert_same_geometries(&[new_point(3.0, 6.0)], &decoded);
    }

    #[test]
    fn test_attribute_types() {
        let mut vtm = VectorTile::new();
        let attrs = tags! {
            "key1" => "value1",
            "key2" => 123,
            "key3" => 234.1f32,
            "key4" => 567.123f64,
            "key5" => -123i64,
            "key6" => "value6",
            "key7" => true,
            "key8" => false,
        };
        vtm.add_layer_features("DEPCNT", &[new_vector_tile_feature("DEPCNT", &new_point(3.0, 6.0), attrs)]);
        let encoded = vtm.encode();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(1, decoded.len());
        let attrs = &decoded[0].tags;
        assert_eq!(Some(&Value::str("value1")), attrs.get("key1"));
        assert_eq!(Some(&Value::Int(123)), attrs.get("key2"));
        assert_eq!(Some(&Value::Float(234.1)), attrs.get("key3"));
        assert_eq!(Some(&Value::Double(567.123)), attrs.get("key4"));
        assert_eq!(Some(&Value::Int(-123)), attrs.get("key5"));
        assert_eq!(Some(&Value::str("value6")), attrs.get("key6"));
        assert_eq!(Some(&Value::Bool(true)), attrs.get("key7"));
        assert_eq!(Some(&Value::Bool(false)), attrs.get("key8"));
    }

    #[test]
    fn test_direct_encoding_matches_prost() {
        let mut vtm = VectorTile::new();
        let big = "x".repeat(300);
        let attrs = tags! {
            "key1" => "value1",
            "key2" => 123,
            "key3" => 234.1f32,
            "key4" => 567.123f64,
            "key5" => -123i64,
            "key6" => i64::MIN,
            "key7" => true,
            "key8" => false,
            "key9" => big.as_str(),
            "key10" => Value::list([Value::Int(1), Value::str("a")]),
            "null" => Value::Null,
        };
        let mut features = Vec::new();
        for (i, id) in [1i64, 0, -1, i64::MAX, 300].into_iter().enumerate() {
            let x = 3.0 + i as f64 * 50.0;
            features.push(Feature::new("a", id, encode_geometry(&new_point(x, 6.0)), attrs.clone()));
            features.push(Feature::new("a", id, encode_geometry(&rectangle(x, x, x + 100.0, x + 120.0)), Tags::new()));
            features.push(Feature::new(
                "a",
                id,
                encode_geometry(&new_line_string(&[x, 1.0, x + 300.0, 2.0, 0.0, 250.0])),
                tags! { "i" => i as i64 },
            ));
        }
        vtm.add_layer_features("layer a", &features);
        vtm.add_layer_features("", &features[3..7]);
        vtm.add_layer_features(&big, &features[..1]);
        vtm.add_layer_features("empty", &[Feature::new("e", 1, encode_geometry(&empty_geometry()), attrs)]);
        for include_ids in [true, false] {
            assert_eq!(vtm.to_proto(include_ids).encode_to_vec(), vtm.encode_with_ids(include_ids));
        }
        assert_eq!(VectorTile::new().to_proto(true).encode_to_vec(), VectorTile::new().encode());
    }

    #[test]
    fn test_varint_size() {
        for v in [0u64, 1, 127, 128, 16383, 16384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            prost::encoding::encode_varint(v, &mut buf);
            assert_eq!(buf.len(), varint_size(v), "{v}");
            if v <= u32::MAX as u64 {
                assert_eq!(buf.len(), varint_size32(v as u32), "{v}");
            }
        }
    }

    #[test]
    fn test_multi_polygon_commands() {
        // see https://github.com/mapbox/vector-tile-spec/blob/master/2.1/README.md
        assert_eq!(
            vec![
                9, 0, 0, 26, 20, 0, 0, 20, 19, 0, 15, 9, 22, 2, 26, 18, 0, 0, 18, 17, 0, 15, 9, 4, 13, 26, 0, 8, 8, 0,
                0, 7, 15
            ],
            get_commands(&new_multi_polygon(&[
                new_polygon(&[0.0, 0.0, 10.0, 0.0, 10.0, 10.0, 0.0, 10.0, 0.0, 0.0]),
                new_polygon(&[11.0, 11.0, 20.0, 11.0, 20.0, 20.0, 11.0, 20.0, 11.0, 11.0]),
                new_polygon(&[13.0, 13.0, 13.0, 17.0, 17.0, 17.0, 17.0, 13.0, 13.0, 13.0]),
            ]))
        );
    }

    fn octagon(cx: f64, cy: f64, r: f64) -> Geometry<f64> {
        // counter-clockwise in math orientation (Java test uses buffer(...).reverse())
        let mut c = Vec::new();
        for i in 0..=8 {
            let a = std::f64::consts::PI * 2.0 * (i % 8) as f64 / 8.0;
            c.push(cx + r * a.cos());
            c.push(cy + r * a.sin());
        }
        new_polygon(&c)
    }

    #[test]
    fn test_multi_polygon() {
        let mp = new_multi_polygon(&[octagon(13.0, 16.0, 3.0), octagon(24.0, 25.0, 5.0)]);
        let mut vtm = VectorTile::new();
        vtm.add_layer_features("mp", &[new_vector_tile_feature("mp", &mp, tags! {"key1" => "value1"})]);
        let encoded = vtm.encode();
        assert!(!encoded.is_empty());
        let features = decode(&encoded).unwrap();
        assert_eq!(1, features.len());
        match decode_silently(&features[0].geometry) {
            Geometry::MultiPolygon(mp2) => assert_eq!(2, mp2.0.len()),
            other => panic!("expected multipolygon, got {other:?}"),
        }
    }

    #[test]
    fn test_geometry_collection_silently_ignored() {
        let gc =
            Geometry::GeometryCollection(GeometryCollection(vec![octagon(13.0, 16.0, 3.0), new_point(24.0, 25.0)]));
        let mut vtm = VectorTile::new();
        vtm.add_layer_features("gc", &[new_vector_tile_feature("gc", &gc, tags! {"key1" => "value1"})]);
        let encoded = vtm.encode();
        assert_eq!(0, decode(&encoded).unwrap().len());
    }

    fn test_round_trip(input: &Geometry<f64>, layer: &str, attrs: Tags, id: i64) {
        let encoded_geom = encode_geometry(input);
        let output = decode_silently(&encoded_geom);
        assert_eq!(input, &output);

        let encoded = VectorTile::new()
            .add_layer_features(layer, &[Feature::new(layer, id, encode_geometry(input), attrs.clone())])
            .encode();
        let decoded = decode(&encoded).unwrap();
        let expected = Feature::new(layer, id, encode_geometry(input), attrs);
        assert_eq!(vec![expected], decoded);
        assert_same_geometries(std::slice::from_ref(input), &decoded);
    }

    fn test_round_trip_geometry(input: Geometry<f64>) {
        test_round_trip(&input, "layer", Tags::new(), 1);
    }

    #[test]
    fn test_round_trip_point() {
        test_round_trip_geometry(new_point(1.0, 2.0));
    }

    #[test]
    fn test_round_trip_multipoint() {
        test_round_trip_geometry(new_multi_point(&[new_point(1.0, 2.0), new_point(3.0, 4.0)]));
    }

    #[test]
    fn test_round_trip_line_string() {
        test_round_trip_geometry(new_line_string(&[1.0, 2.0, 3.0, 4.0]));
    }

    #[test]
    fn test_round_trip_polygon() {
        test_round_trip_geometry(Geometry::Polygon(Polygon::new(
            LineString::new(coords(&[0.0, 0.0, 4.0, 0.0, 4.0, 4.0, 0.0, 4.0, 0.0, 0.0])),
            vec![LineString::new(coords(&[1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 2.0, 1.0, 1.0, 1.0]))],
        )));
    }

    #[test]
    fn test_round_trip_multi_polygon() {
        test_round_trip_geometry(new_multi_polygon(&[
            new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]),
            new_polygon(&[3.0, 0.0, 4.0, 0.0, 4.0, 1.0, 3.0, 1.0, 3.0, 0.0]),
        ]));
    }

    #[test]
    fn test_round_trip_attributes() {
        test_round_trip(
            &new_point(0.0, 0.0),
            "layer",
            tags! {"string" => "string", "long" => 1i64, "double" => 3.5f64, "true" => true, "false" => false},
            1,
        );
    }

    #[test]
    fn test_multiple_features_multiple_layer() {
        let point = new_point(0.0, 0.0);
        let attrs1 = tags! {"a" => 1i64, "b" => 2i64};
        let attrs2 = tags! {"b" => 3i64, "c" => 2i64};
        let encoded = VectorTile::new()
            .add_layer_features(
                "layer1",
                &[
                    Feature::new("layer1", 1, encode_geometry(&point), attrs1.clone()),
                    Feature::new("layer1", 2, encode_geometry(&point), attrs2.clone()),
                ],
            )
            .add_layer_features("layer2", &[Feature::new("layer2", 3, encode_geometry(&point), attrs1.clone())])
            .encode();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(attrs1, decoded[0].tags);
        assert_eq!("layer1", decoded[0].layer);
        assert_eq!(attrs2, decoded[1].tags);
        assert_eq!("layer1", decoded[1].layer);
        assert_eq!(attrs1, decoded[2].tags);
        assert_eq!("layer2", decoded[2].layer);
    }

    fn assert_is_fill_or_edge(is_fill_or_edge: bool, is_fill: bool, geom: &Geometry<f64>) {
        for rotation in [0.0, 90.0, 180.0, 270.0] {
            let rectangle = rotate(geom, rotation, 128.0, 128.0);
            for scale in 0..4 {
                assert_eq!(
                    is_fill_or_edge,
                    encode_geometry_scaled(&rectangle, scale).is_fill_or_edge(),
                    "scale={scale} rotation={rotation}"
                );
                assert_eq!(
                    is_fill,
                    encode_geometry_scaled(&rectangle, scale).is_fill(),
                    "scale={scale} rotation={rotation}"
                );
            }
        }
    }

    #[test]
    fn test_rectangle_is_fill_or_edge() {
        let cases: &[(bool, bool, f64, f64, f64, f64)] = &[
            (true, true, -1.0, -1.0, 257.0, 257.0),
            (true, true, -5.0, -5.0, 260.0, 260.0),
            (false, false, -1.0, -1.0, 254.0, 254.0),
            (true, false, 0.0, -1.0, 257.0, 257.0),
            (true, false, -1.0, 0.0, 257.0, 257.0),
            (true, false, -1.0, -1.0, 256.0, 257.0),
            (true, false, -1.0, -1.0, 257.0, 256.0),
            (false, false, 0.0, 0.0, 1.0, 1.0),
            (false, false, 1.0, 1.0, 2.0, 2.0),
            (false, false, 1.0, 1.0, 2.0, 2.0),
            (false, false, -10.0, -10.0, -5.0, -5.0),
            (false, false, 260.0, -10.0, 270.0, 5.0),
            (false, false, -10.0, 260.0, -5.0, 270.0),
            (false, false, 260.0, 260.0, 270.0, 270.0),
            (true, false, 1.0, -1.0, 257.0, 257.0),
            (true, false, 1.0, -1.0, 255.0, 257.0),
            (false, false, 1.0, -1.0, 255.0, 255.0),
        ];
        for &(is_fill_or_edge, is_fill, x1, y1, x2, y2) in cases {
            assert_is_fill_or_edge(is_fill_or_edge, is_fill, &rectangle(x1, y1, x2, y2));
        }
    }

    #[test]
    fn test_rectangle_with_slanted_edge_is_not_fill() {
        assert_is_fill_or_edge(
            false,
            false,
            &new_polygon(&[1.0, -1.0, 257.0, -1.0, 257.0, 257.0, 2.0, 257.0, 1.0, -1.0]),
        );
    }

    #[test]
    fn test_line_is_edge() {
        let cases: &[(bool, f64, f64, f64, f64)] = &[
            (true, 1.0, -1.0, 1.0, 257.0),
            (false, 1.0, 1.0, 1.0, 257.0),
            (false, 1.0, -1.0, 1.0, 255.0),
            (false, 1.0, -1.0, 2.0, 257.0),
        ];
        for &(is_fill_or_edge, x1, y1, x2, y2) in cases {
            assert_is_fill_or_edge(is_fill_or_edge, false, &new_line_string(&[x1, y1, x2, y2]));
        }
    }

    #[test]
    fn test_cross_boundary_not_fill_or_edge() {
        assert_is_fill_or_edge(false, false, &new_polygon(&[-1.0, -1.0, 257.0, -1.0, -1.0, 257.0, -1.0, -1.0]));
        assert_is_fill_or_edge(false, false, &new_polygon(&[257.0, -1.0, -1.0, 257.0, -1.0, -1.0, 257.0, -1.0]));
        assert_is_fill_or_edge(false, false, &new_polygon(&[-1.0, 257.0, -1.0, -1.0, 257.0, -1.0, -1.0, 257.0]));
        assert_is_fill_or_edge(false, false, &new_polygon(&[-1.0, -1.0, 513.0, -1.0, -1.0, 513.0, -1.0, -1.0]));
    }

    #[test]
    fn test_scale_unscale() {
        let scales = [0u32, 1, 2, 16];
        let geoms = vec![
            new_point(0.0, 0.0),
            new_point(0.25, -0.25),
            new_point(1.25, 1.25),
            new_point(1.5, 1.5),
            new_multi_point(&[new_point(1.25, 1.25), new_point(1.5, 1.5)]),
            new_line_string(&[0.0, 0.0, 1.2, 1.2]),
            new_line_string(&[0.0, 0.0, 0.1, 0.1]),
            new_line_string(&[0.0, 0.0, 1.0, 1.0, 1.2, 1.2, 2.0, 2.0]),
            new_line_string(&[8000.0, 8000.0, 8000.0, 8001.0, 8001.0, 8001.0]),
            new_line_string(&[-4000.0, -4000.0, -4000.0, -4001.0, -4001.0, -4001.0]),
            new_multi_line_string(&[new_line_string(&[0.0, 0.0, 1.0, 1.0]), new_line_string(&[1.1, 1.1, 2.0, 2.0])]),
            new_multi_line_string(&[new_line_string(&[0.0, 0.0, 0.1, 0.1]), new_line_string(&[1.1, 1.1, 2.0, 2.0])]),
            new_multi_line_string(&[
                new_line_string(&[-10.0, -10.0, -9.0, -9.0]),
                new_line_string(&[0.0, 0.0, 0.1, 0.1]),
                new_line_string(&[1.1, 1.1, 2.0, 2.0]),
            ]),
            new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]),
            new_polygon(&[0.0, 0.0, 0.1, 0.0, 0.1, 0.1, 0.0, 0.1, 0.0, 0.0]),
            new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 0.1, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]),
            new_multi_polygon(&[
                new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]),
                new_polygon(&[0.0, 0.0, -1.0, 0.0, -1.0, -1.0, 0.0, -1.0, 0.0, 0.0]),
            ]),
            new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.1, 0.0, 0.0]),
        ];
        for geom in geoms {
            let geometry = scale_geom(&geom, 256.0 / 4096.0);
            for scale in scales {
                let pm = ((4096i64 << scale) as f64) / 256.0;
                let pm0 = 4096.0 / 256.0;
                let msg = format!("{geom:?} scale: {scale}");
                let decoded = encode_geometry_scaled(&geometry, scale).decode().unwrap();
                let expected = reduce(&geometry, pm);
                if !(is_empty_geom(&expected) && is_empty_geom(&decoded)) {
                    assert_eq!(normalize(&expected), normalize(&decoded), "{msg}");
                }
                let unscaled = encode_geometry_scaled(&geometry, scale).unscale().decode().unwrap();
                let expected = reduce(&reduce(&geometry, pm), pm0);
                if !(is_empty_geom(&expected) && is_empty_geom(&unscaled)) {
                    assert_eq!(normalize(&expected), normalize(&unscaled), "unscale {msg}");
                }
            }
        }
    }

    #[test]
    fn test_unscale_degenerate() {
        let less_than_one_px = 256.0 / 4096.0 / 4.0;
        let encoded = encode_geometry_scaled(&new_line_string(&[0.0, 0.0, less_than_one_px, less_than_one_px]), 2);
        assert_eq!(6, encoded.commands().len());
        let unscaled = encoded.unscale();
        assert_eq!(0, unscaled.commands().len());
        assert!(!encoded.is_empty());
        assert!(unscaled.is_empty());
        assert_eq!(empty_geometry(), unscaled.decode().unwrap());
        let re_encoded = encode_geometry(&unscaled.decode().unwrap());
        assert_eq!(0, re_encoded.commands().len());
    }

    #[test]
    fn test_filter_points_outside_buffer() {
        assert_eq!(
            Vec::<i32>::new(),
            encode_geometry(&new_point(-5.0, -5.0)).filter_points_outside_buffer(4.0).commands()
        );
        assert_eq!(
            vec![
                Command::MoveTo.value() | (1 << 3),
                zig_zag_encode((-5.0 * 4096.0 / 256.0) as i32),
                zig_zag_encode((-5.0 * 4096.0 / 256.0) as i32)
            ],
            encode_geometry(&new_point(-5.0, -5.0)).filter_points_outside_buffer(5.0).commands()
        );
    }

    #[test]
    fn test_filter_multi_points_all_outside_buffer() {
        assert_eq!(
            Vec::<i32>::new(),
            encode_geometry(&new_multi_point(&[new_point(-5.0, -5.0), new_point(261.0, 261.0)]))
                .filter_points_outside_buffer(4.0)
                .commands()
        );
    }

    #[test]
    fn test_filter_multi_points_first_outside_buffer() {
        assert_eq!(
            vec![Command::MoveTo.value() | (1 << 3), zig_zag_encode(4096), zig_zag_encode(4096)],
            encode_geometry(&new_multi_point(&[new_point(-5.0, -5.0), new_point(256.0, 256.0)]))
                .filter_points_outside_buffer(4.0)
                .commands()
        );
    }

    #[test]
    fn test_filter_multi_points_last_outside_buffer() {
        assert_eq!(
            vec![Command::MoveTo.value() | (1 << 3), zig_zag_encode(4096), zig_zag_encode(4096)],
            encode_geometry(&new_multi_point(&[new_point(256.0, 256.0), new_point(-5.0, -5.0)]))
                .filter_points_outside_buffer(4.0)
                .commands()
        );
    }

    #[test]
    fn test_count_internal_geometries() {
        let cases = vec![
            (1, new_point(0.0, 0.0)),
            (2, new_multi_point(&[new_point(0.0, 0.0), new_point(0.0, 1.0)])),
            (3, new_multi_point(&[new_point(0.0, 0.0), new_point(0.0, 1.0), new_point(0.0, 2.0)])),
            (1, new_line_string(&[0.0, 0.0, 1.0, 1.0])),
            (
                2,
                new_multi_line_string(&[
                    new_line_string(&[0.0, 0.0, 1.0, 1.0]),
                    new_line_string(&[0.0, 0.0, 2.0, 2.0]),
                ]),
            ),
            (
                3,
                new_multi_line_string(&[
                    new_line_string(&[0.0, 0.0, 1.0, 1.0]),
                    new_line_string(&[0.0, 0.0, 2.0, 2.0]),
                    new_line_string(&[0.0, 0.0, 2.0, 3.0]),
                ]),
            ),
            (1, rectangle_sq(0.0, 1.0)),
            (2, new_multi_polygon(&[rectangle_sq(0.0, 1.0), rectangle_sq(3.0, 4.0)])),
            (3, new_multi_polygon(&[rectangle_sq(0.0, 1.0), rectangle_sq(3.0, 4.0), rectangle_sq(6.0, 8.0)])),
        ];
        for (expected, geom) in cases {
            let feature = Feature::new("layer", 1, encode_geometry(&geom), Tags::new());
            let proto = VectorTile::new().add_layer_features("layer", &[feature]).to_proto(true);
            assert_eq!(expected, count_geometries(&proto.layers[0].features[0]), "{geom:?}");
        }
    }

    const FIRST_COORD_CASES: [(f64, f64); 4] = [(0.0, 0.0), (1.0, 1.0), (-10.0, -1.0), (300.25, 200.75)];

    #[test]
    fn first_coordinate_of_point() {
        for (x, y) in FIRST_COORD_CASES {
            for scale in 0..10 {
                assert_eq!(
                    Some(Coord { x, y }),
                    encode_geometry_scaled(&new_point(x, y), scale).first_coordinate(),
                    "scale={scale}"
                );
            }
        }
    }

    #[test]
    fn first_coordinate_of_multi_point() {
        for (x, y) in FIRST_COORD_CASES {
            for scale in 0..10 {
                assert_eq!(
                    Some(Coord { x, y }),
                    encode_geometry_scaled(&new_multi_point(&[new_point(x, y), new_point(0.0, 0.0)]), scale)
                        .first_coordinate(),
                    "scale={scale}"
                );
            }
        }
    }

    #[test]
    fn first_coordinate_of_line() {
        for (x, y) in FIRST_COORD_CASES {
            for scale in 0..10 {
                assert_eq!(
                    Some(Coord { x, y }),
                    encode_geometry_scaled(&new_line_string(&[x, y, x + 1.0, y + 1.0]), scale).first_coordinate(),
                    "scale={scale}"
                );
            }
        }
    }

    #[test]
    fn first_coordinate_of_polygon() {
        for (x, y) in FIRST_COORD_CASES {
            for scale in 0..10 {
                assert_eq!(
                    Some(Coord { x, y }),
                    encode_geometry_scaled(
                        &new_polygon(&[x, y, x + 1.0, y, x + 1.0, y + 1.0, x, y + 1.0, x, y]),
                        scale
                    )
                    .first_coordinate(),
                    "scale={scale}"
                );
            }
        }
    }

    #[test]
    fn test_fill() {
        for (buffer, min, max) in [(0.0, 0.0, 256.0), (1.0, -1.0, 257.0), (10.0, -10.0, 266.0)] {
            let geom = encode_fill(buffer);
            assert_same_geometry(&rectangle_sq(min, max), &geom.decode().unwrap());
            assert_eq!(encode_geometry(&rectangle_sq(min, max)).commands(), geom.commands());
        }
    }

    // ---- additional tests for the Rust port ----

    #[test]
    fn test_merger() {
        let mut merger = new_merger(GeometryType::Point);
        merger.accept(&encode_geometry(&new_point(1.0, 2.0))).unwrap();
        merger.accept(&encode_geometry(&new_point(3.0, 4.0))).unwrap();
        assert!(merger.accept(&encode_geometry(&new_line_string(&[0.0, 0.0, 1.0, 1.0]))).is_err());
        let merged = merger.finish();
        assert_eq!(encode_geometry(&new_multi_point(&[new_point(1.0, 2.0), new_point(3.0, 4.0)])), merged);

        let mut merger = new_merger(GeometryType::Line);
        merger.accept(&encode_geometry(&new_line_string(&[0.0, 0.0, 1.0, 1.0]))).unwrap();
        merger.accept(&encode_geometry_scaled(&new_line_string(&[2.0, 2.0, 3.0, 3.0]), 2)).unwrap();
        assert_eq!(
            encode_geometry(&new_multi_line_string(&[
                new_line_string(&[0.0, 0.0, 1.0, 1.0]),
                new_line_string(&[2.0, 2.0, 3.0, 3.0]),
            ])),
            merger.finish()
        );

        let mut merger = new_merger(GeometryType::Polygon);
        merger.accept(&encode_geometry(&rectangle_sq(0.0, 1.0))).unwrap();
        merger.accept(&encode_geometry(&rectangle_sq(3.0, 4.0))).unwrap();
        assert_eq!(
            encode_geometry(&new_multi_polygon(&[rectangle_sq(0.0, 1.0), rectangle_sq(3.0, 4.0)])),
            merger.finish()
        );
    }

    #[test]
    fn test_zig_zag() {
        for n in [0, 1, -1, 2, -2, 1000, -1000, 1 << 29, -(1 << 29)] {
            assert_eq!(n, zig_zag_decode(zig_zag_encode(n)));
        }
        assert_eq!(1, zig_zag_encode(-1));
        assert_eq!(2, zig_zag_encode(1));
    }

    #[test]
    fn test_java_round() {
        assert_eq!(-2, java_round(-2.5));
        assert_eq!(3, java_round(2.5));
        assert_eq!(0, java_round(0.49999999999999994));
        assert_eq!(0, java_round(f64::NAN));
    }

    #[test]
    fn test_fills_and_duplicates() {
        let mut tile = VectorTile::new();
        assert!(tile.is_empty());
        assert!(tile.likely_to_be_duplicated());
        tile.add_layer_features("water", &[Feature::new("water", 1, encode_fill(1.0), Tags::new())]);
        assert!(tile.contains_only_fills());
        assert!(tile.contains_only_fills_or_edges());
        assert!(tile.likely_to_be_duplicated());
        tile.add_layer_features(
            "poi",
            &[Feature::new("poi", 2, encode_geometry(&new_point(1.0, 1.0)), tags! {"a" => 1, "b" => 1})],
        );
        assert!(!tile.contains_only_fills());
        assert!(!tile.likely_to_be_duplicated());
        assert_eq!(Some(2), tile.get_num_keys("poi"));
        assert_eq!(Some(1), tile.get_num_values("poi"));
        assert_eq!(None, tile.get_num_keys("other"));
        // ids excluded
        assert!(tile.to_proto(false).layers.iter().all(|l| l.features.iter().all(|f| f.id.is_none())));
        assert!(tile.encode_mlt().is_err());
    }

    #[test]
    fn test_feature_copy() {
        let f = Feature::new("layer", 1, encode_geometry(&new_point(1.0, 1.0)), tags! {"a" => 1});
        assert!(!f.has_group());
        let f2 = f.copy_with_extra_attrs(&tags! {"b" => 2});
        assert_eq!(tags! {"a" => 1, "b" => 2}, f2.tags);
        let f3 = f.copy_with_id_and_geometry(5, encode_geometry(&new_point(2.0, 2.0)));
        assert_eq!(5, f3.id);
        assert_eq!(encode_geometry(&new_point(2.0, 2.0)), f3.geometry);
        let f4 = f.copy_with_new_geometry(&new_point(3.0, 3.0));
        assert_eq!(encode_geometry(&new_point(3.0, 3.0)), f4.geometry);
        assert_eq!("VectorGeometry[commands=int[3], geomType=POINT (1)]", f4.geometry.to_string());
    }

    #[test]
    fn test_hilbert_index() {
        let g = new_point(1.0, 2.0);
        assert_eq!(hilbert_index(&g), encode_geometry(&g).hilbert_index());
        assert_eq!(hilbert_index(&g), encode_geometry_scaled(&g, 2).hilbert_index());
    }

    #[test]
    fn test_layer_stats_tracking() {
        let stats = crate::util::layer_attr_stats::LayerAttrStats::new();
        let mut tile = VectorTile::new();
        tile.track_layer_stats(stats.handler_for_thread().for_zoom(3));
        tile.add_layer_features(
            "poi",
            &[Feature::new("poi", 2, encode_geometry(&new_point(1.0, 1.0)), tags! {"a" => 1, "b" => "c"})],
        );
        let result = stats.get_tile_stats();
        assert_eq!(1, result.len());
        assert_eq!("poi", result[0].id);
        assert_eq!(Some(3), result[0].minzoom);
        assert_eq!(2, result[0].fields.len());
    }
}
