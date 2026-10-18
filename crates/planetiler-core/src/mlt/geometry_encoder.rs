//! Port of `org.maplibre.mlt.converter.encodings.GeometryEncoder`, the space filling curves from
//! `org.maplibre.mlt.converter.geometry` and `TessellationUtils`.

use geo_types::{Coord, Geometry, LineString, Polygon};

use super::earcut;
use super::encoding::{
    DictionaryType, LengthType, LogicalLevelTechnique, LogicalStreamType, OffsetType, PhysicalLevelTechnique,
    PhysicalStreamType, StreamMetadata, put_varint, varint_size, zigzag,
};
use super::fastpfor;
use super::integer_encoder::{
    encode_int_stream, encode_morton_stream, encoded_int_size, encoded_int_size_below, encoded_morton_codes_size,
};

/// `GeometryType` ordinals.
pub const POINT: i32 = 0;
pub const LINESTRING: i32 = 1;
pub const POLYGON: i32 = 2;
pub const MULTIPOINT: i32 = 3;
pub const MULTILINESTRING: i32 = 4;
pub const MULTIPOLYGON: i32 = 5;

/// `GeometryEncoder.EncodedGeometryColumn` (minus the unused `maxVertexValue`).
pub struct EncodedGeometryColumn {
    pub num_streams: u32,
    pub encoded: Vec<u8>,
}

/// `SpaceFillingCurve` parameters shared by the Hilbert and Z-order curves.
#[derive(Clone, Copy, Debug)]
pub struct CurveParams {
    pub num_bits: u32,
    pub coordinate_shift: i32,
}

impl CurveParams {
    pub fn new(min_vertex_value: i32, max_vertex_value: i32) -> CurveParams {
        let coordinate_shift = if min_vertex_value < 0 { min_vertex_value.wrapping_abs() } else { 0 };
        let tile_extent = max_vertex_value.wrapping_add(coordinate_shift);
        let num_bits = ((tile_extent.wrapping_add(1) as f64).ln() / 2f64.ln()).ceil() as i32;
        CurveParams { num_bits: num_bits as u32, coordinate_shift }
    }

    /// `ZOrderCurve.encode`.
    #[inline]
    pub fn morton(&self, x: i32, y: i32) -> i32 {
        let sx = x.wrapping_add(self.coordinate_shift);
        let sy = y.wrapping_add(self.coordinate_shift);
        let mut code = 0i32;
        for i in 0..self.num_bits {
            let bit = 1i32.wrapping_shl(i);
            code |= (sx & bit).wrapping_shl(i) | (sy & bit).wrapping_shl(i + 1);
        }
        code
    }

    /// `HilbertCurve.encode` (`Hilbert.hilbertXYToIndex`).
    #[inline]
    pub fn hilbert(&self, x: i32, y: i32) -> i32 {
        hilbert_xy_to_index(
            self.num_bits as i32,
            x.wrapping_add(self.coordinate_shift),
            y.wrapping_add(self.coordinate_shift),
        )
    }
}

/// `Hilbert.interleave`.
#[inline]
fn interleave(tx: u32) -> u32 {
    let mut tx = tx;
    tx = (tx | (tx << 8)) & 0x00FF00FF;
    tx = (tx | (tx << 4)) & 0x0F0F0F0F;
    tx = (tx | (tx << 2)) & 0x33333333;
    tx = (tx | (tx << 1)) & 0x55555555;
    tx
}

/// `Hilbert.hilbertXYToIndex` with Java int semantics (shift distances taken mod 32).
pub fn hilbert_xy_to_index(level: i32, x: i32, y: i32) -> i32 {
    let shl = |v: u32, s: i32| v.wrapping_shl(s as u32);
    let shr = |v: u32, s: i32| v.wrapping_shr(s as u32);
    let x = shl(x as u32, 16 - level);
    let y = shl(y as u32, 16 - level);

    let a1 = x ^ y;
    let b1 = 0xFFFF ^ a1;
    let c1 = 0xFFFF ^ (x | y);
    let d1 = x & (y ^ 0xFFFF);

    let mut h_a = a1 | (b1 >> 1);
    let mut h_b = (a1 >> 1) ^ a1;
    let mut h_c = ((c1 >> 1) ^ (b1 & (d1 >> 1))) ^ c1;
    let mut h_d = ((a1 & (c1 >> 1)) ^ (d1 >> 1)) ^ d1;

    let (a2, b2, c2, d2) = (h_a, h_b, h_c, h_d);
    h_a = (a2 & (a2 >> 2)) ^ (b2 & (b2 >> 2));
    h_b = (a2 & (b2 >> 2)) ^ (b2 & ((a2 ^ b2) >> 2));
    h_c ^= (a2 & (c2 >> 2)) ^ (b2 & (d2 >> 2));
    h_d ^= (b2 & (c2 >> 2)) ^ ((a2 ^ b2) & (d2 >> 2));

    let (a3, b3, c3, d3) = (h_a, h_b, h_c, h_d);
    h_a = (a3 & (a3 >> 4)) ^ (b3 & (b3 >> 4));
    h_b = (a3 & (b3 >> 4)) ^ (b3 & ((a3 ^ b3) >> 4));
    h_c ^= (a3 & (c3 >> 4)) ^ (b3 & (d3 >> 4));
    h_d ^= (b3 & (c3 >> 4)) ^ ((a3 ^ b3) & (d3 >> 4));

    let (a4, b4, c4, d4) = (h_a, h_b, h_c, h_d);
    h_c ^= (a4 & (c4 >> 8)) ^ (b4 & (d4 >> 8));
    h_d ^= (b4 & (c4 >> 8)) ^ ((a4 ^ b4) & (d4 >> 8));

    let a = h_c ^ (h_c >> 1);
    let b = h_d ^ (h_d >> 1);

    let i0 = x ^ y;
    let i1 = b | (0xFFFF ^ (i0 | a));

    shr((interleave(i1) << 1) | interleave(i0), 32 - 2 * level) as i32
}

#[derive(Default)]
struct Topology {
    geometry_types: Vec<i32>,
    num_geometries: Vec<i32>,
    num_parts: Vec<i32>,
    num_rings: Vec<i32>,
    vertices: Vec<(i32, i32)>,
    num_triangles: Vec<i32>,
    index_buffer: Vec<i32>,
}

#[inline]
fn vertex(c: &Coord<f64>) -> (i32, i32) {
    (c.x as i32, c.y as i32)
}

impl Topology {
    fn add_line_string(&mut self, contains_polygon: bool, line: &LineString<f64>) {
        // Depending on the max geometry type in the column add to the numRings or numParts stream
        let n = line.0.len() as i32;
        if contains_polygon {
            self.num_rings.push(n);
        } else {
            self.num_parts.push(n);
        }
        self.vertices.extend(line.0.iter().map(vertex));
    }

    /// `flatPolygon`: rings without their closing coordinate.
    fn flat_polygon(&mut self, polygon: &Polygon<f64>) {
        self.num_parts.push(1 + polygon.interiors().len() as i32);
        for ring in std::iter::once(polygon.exterior()).chain(polygon.interiors()) {
            let coords = &ring.0[..ring.0.len().saturating_sub(1)];
            self.vertices.extend(coords.iter().map(vertex));
            self.num_rings.push(coords.len() as i32);
        }
    }

    fn build(geometries: &[&Geometry<f64>], tessellate: bool) -> anyhow::Result<Topology> {
        let contains_polygon = geometries.iter().any(|g| matches!(g, Geometry::Polygon(_) | Geometry::MultiPolygon(_)));
        let mut t = Topology::default();
        for geometry in geometries {
            match geometry {
                Geometry::Point(p) => {
                    t.geometry_types.push(POINT);
                    t.vertices.push(vertex(&p.0));
                }
                Geometry::LineString(line) => {
                    t.geometry_types.push(LINESTRING);
                    t.add_line_string(contains_polygon, line);
                }
                Geometry::Polygon(polygon) => {
                    t.geometry_types.push(POLYGON);
                    t.flat_polygon(polygon);
                    if tessellate {
                        let (indices, _) = tessellate_polygon(polygon, 0);
                        t.num_triangles.push((indices.len() / 3) as i32);
                        t.index_buffer.extend(indices);
                    }
                }
                Geometry::MultiLineString(lines) => {
                    t.geometry_types.push(MULTILINESTRING);
                    t.num_geometries.push(lines.0.len() as i32);
                    for line in &lines.0 {
                        t.add_line_string(contains_polygon, line);
                    }
                }
                Geometry::MultiPolygon(polygons) => {
                    t.geometry_types.push(MULTIPOLYGON);
                    t.num_geometries.push(polygons.0.len() as i32);
                    for polygon in &polygons.0 {
                        t.flat_polygon(polygon);
                    }
                    if tessellate {
                        // The range of the values of the indices are created per MultiPolygon
                        let mut num_triangles = 0;
                        let mut num_vertices = 0;
                        for polygon in &polygons.0 {
                            let (indices, n) = tessellate_polygon(polygon, num_vertices);
                            num_triangles += indices.len() / 3;
                            t.index_buffer.extend(indices);
                            num_vertices += n;
                        }
                        t.num_triangles.push(num_triangles as i32);
                    }
                }
                Geometry::MultiPoint(points) => {
                    t.geometry_types.push(MULTIPOINT);
                    t.num_geometries.push(points.0.len() as i32);
                    t.vertices.extend(points.0.iter().map(|p| vertex(&p.0)));
                }
                other => anyhow::bail!("Specified geometry type is not (yet) supported: {other:?}"),
            }
        }
        if t.vertices.is_empty() {
            anyhow::bail!("The geometry column contains no vertices");
        }
        Ok(t)
    }
}

/// `TessellationUtils.tessellatePolygon`: earcut triangle indices (offset by `index_offset`) and the number of
/// vertices of the polygon (without closing points).
fn tessellate_polygon(polygon: &Polygon<f64>, index_offset: i32) -> (Vec<i32>, i32) {
    let mut coords = Vec::new();
    let mut hole_indices = Vec::new();
    let ring_coords = |ring: &LineString<f64>| -> Vec<Coord<f64>> { ring.0[..ring.0.len().saturating_sub(1)].to_vec() };
    let shell = ring_coords(polygon.exterior());
    let mut num_vertices = polygon.exterior().0.len() as i32 - 1;
    for c in &shell {
        coords.push(c.x);
        coords.push(c.y);
    }
    for hole in polygon.interiors() {
        hole_indices.push(num_vertices);
        num_vertices += hole.0.len() as i32 - 1;
        for c in ring_coords(hole) {
            coords.push(c.x);
            coords.push(c.y);
        }
    }
    let indices = earcut::earcut(&coords, &hole_indices);
    (indices.into_iter().map(|i| i + index_offset).collect(), num_vertices)
}

/// `zigZagDeltaEncodeVertices`.
fn zigzag_delta_encode_vertices(vertices: &[(i32, i32)]) -> Vec<i32> {
    let mut out = Vec::with_capacity(vertices.len() * 2);
    let (mut px, mut py) = (0i32, 0i32);
    for &(x, y) in vertices {
        out.push(zigzag(x.wrapping_sub(px)) as i32);
        out.push(zigzag(y.wrapping_sub(py)) as i32);
        px = x;
        py = y;
    }
    out
}

/// `encodeVertexBuffer`: componentwise delta encoded vertices with the physical technique applied directly.
fn encode_vertex_buffer(out: &mut Vec<u8>, values: &[i32], fast_pfor: bool) {
    let header = |out: &mut Vec<u8>, size: usize| {
        StreamMetadata::new(
            PhysicalStreamType::Data,
            Some(LogicalStreamType::Dictionary(DictionaryType::Vertex)),
            LogicalLevelTechnique::ComponentwiseDelta,
            LogicalLevelTechnique::None,
            if fast_pfor { PhysicalLevelTechnique::FastPfor } else { PhysicalLevelTechnique::Varint },
            values.len() as u32,
            size as u32,
        )
        .encode_into(out)
    };
    if fast_pfor {
        let encoded = fastpfor::encode(&values.iter().map(|&v| v as u32).collect::<Vec<_>>());
        header(out, encoded.len());
        out.extend_from_slice(&encoded);
    } else {
        header(out, values.iter().map(|&v| varint_size(v as u32)).sum());
        for &v in values {
            put_varint(out, v as u32);
        }
    }
}

fn length_stream(out: &mut Vec<u8>, values: &[i32], fast_pfor: bool, length_type: Option<LengthType>) {
    encode_int_stream(
        out,
        values,
        fast_pfor,
        false,
        PhysicalStreamType::Length,
        length_type.map(LogicalStreamType::Length),
    );
}

/// `code << 32 | position` with the (signed) code's order preserved.
#[inline]
fn sort_key(code: i32, position: usize) -> u64 {
    (((code ^ i32::MIN) as u32 as u64) << 32) | position as u64
}

/// Sorts keys made by [`sort_key`] in position order (the same order as `keys.sort_unstable()`, since positions
/// are unique): a stable LSD radix sort on the code bits that vary, or a comparison sort for short inputs.
fn sort_keys(keys: &mut Vec<u64>) {
    const DIGIT_BITS: u32 = 8;
    if keys.len() < 256 {
        keys.sort_unstable();
        return;
    }
    let (mut min, mut max) = (u32::MAX, 0u32);
    for &k in keys.iter() {
        let code = (k >> 32) as u32;
        min = min.min(code);
        max = max.max(code);
    }
    let bits = 32 - (max - min).leading_zeros();
    let mut from = std::mem::take(keys);
    let mut to = vec![0u64; from.len()];
    let mut shift = 0;
    while shift < bits {
        let digit = |k: u64| ((((k >> 32) as u32 - min) >> shift) & ((1 << DIGIT_BITS) - 1)) as usize;
        let mut counts = [0usize; 1 << DIGIT_BITS];
        for &k in &from {
            counts[digit(k)] += 1;
        }
        let mut sum = 0;
        for c in counts.iter_mut() {
            let count = *c;
            *c = sum;
            sum += count;
        }
        for &k in &from {
            let d = digit(k);
            to[counts[d]] = k;
            counts[d] += 1;
        }
        std::mem::swap(&mut from, &mut to);
        shift += DIGIT_BITS;
    }
    *keys = from;
}

/// For sorted `keys` (see [`sort_key`]), calls `f(position, end)` for each key with the end (exclusive) of the run
/// of keys with the same code.
fn for_each_run_end(keys: &[u64], mut f: impl FnMut(usize, usize)) {
    let mut start = 0;
    while start < keys.len() {
        let code = keys[start] >> 32;
        let mut end = start + 1;
        while end < keys.len() && keys[end] >> 32 == code {
            end += 1;
        }
        for &k in &keys[start..end] {
            f((k & 0xFFFF_FFFF) as usize, end);
        }
        start = end;
    }
}

/// The vertex buffer candidates of a geometry column: the plain (componentwise delta) vertex buffer, the Hilbert
/// sorted "dictionary" and the sorted Morton codes, each with an offset stream.
///
/// Java computes all three and their encoded sizes for every column. Here the dictionaries are only built when
/// needed and their offset streams only sized when the result can matter: a dictionary candidate is at least its
/// vertex / code stream plus the smallest possible offset stream, and once that already reaches the plain size, the
/// candidate can't be chosen ([`Dictionaries::dictionary_size`] / [`Dictionaries::morton_size`] then return
/// `usize::MAX`, which compares like the real size in every decision below since the plain size is not larger).
struct Dictionaries<'a> {
    vertices: &'a [(i32, i32)],
    fast_pfor: bool,
    curve: CurveParams,
    zigzag_delta_vertex_buffer: Vec<i32>,
    plain_size: usize,
    /// Sorted `hilbert << 32 | position` (see [`sort_key`]), the dictionary in Hilbert order and its stream size.
    hilbert: Option<(Vec<u64>, Vec<i32>, usize)>,
    /// Sorted `morton << 32 | position`, the sorted codes and the size of their stream.
    morton: Option<(Vec<u64>, Vec<i32>, usize)>,
}

impl<'a> Dictionaries<'a> {
    fn new(vertices: &'a [(i32, i32)], fast_pfor: bool) -> Dictionaries<'a> {
        let mut min = i32::MAX;
        let mut max = i32::MIN;
        for &(x, y) in vertices {
            min = min.min(x).min(y);
            max = max.max(x).max(y);
        }
        let curve = CurveParams::new(min, max);
        let zigzag_delta_vertex_buffer = zigzag_delta_encode_vertices(vertices);
        let plain_size = encoded_int_size(&zigzag_delta_vertex_buffer, fast_pfor, false);
        Dictionaries { vertices, fast_pfor, curve, zigzag_delta_vertex_buffer, plain_size, hilbert: None, morton: None }
    }

    /// A lower bound of the size of an offset stream of `n > 0` values: varint plain / delta streams have at least
    /// one byte per value and RLE ones at least a run and a value; any FastPFOR stream is at least two words.
    fn min_offsets_size(&self) -> usize {
        if self.fast_pfor { 8 } else { self.vertices.len().min(2) }
    }

    /// `addVerticesToDictionary`: a stable sort by hilbert index (Java does not actually remove duplicates here).
    /// Sorting `hilbert << 32 | position` gives the stable order.
    fn hilbert(&mut self) -> &(Vec<u64>, Vec<i32>, usize) {
        self.hilbert.get_or_insert_with(|| {
            let (vertices, curve) = (self.vertices, self.curve);
            let mut keys: Vec<u64> = Vec::with_capacity(vertices.len());
            keys.extend(vertices.iter().enumerate().map(|(i, &(x, y))| sort_key(curve.hilbert(x, y), i)));
            sort_keys(&mut keys);
            let dictionary_vertices: Vec<(i32, i32)> =
                keys.iter().map(|&k| vertices[(k & 0xFFFF_FFFF) as usize]).collect();
            let zigzag_delta_vertex_dictionary = zigzag_delta_encode_vertices(&dictionary_vertices);
            let size = encoded_int_size(&zigzag_delta_vertex_dictionary, self.fast_pfor, false);
            (keys, zigzag_delta_vertex_dictionary, size)
        })
    }

    /// Each vertex's offset is the last dictionary position of its Hilbert index (Java's index map keeps the last
    /// put).
    fn dictionary_offsets(&mut self) -> Vec<i32> {
        let n = self.vertices.len();
        let (keys, _, _) = self.hilbert();
        let mut offsets = vec![0i32; n];
        for_each_run_end(keys, |position, end| offsets[position] = end as i32 - 1);
        offsets
    }

    /// Encoded size of the dictionary candidate, or `usize::MAX` if it is known to be at least the plain size.
    fn dictionary_size(&mut self) -> usize {
        let lower_bound = self.hilbert().2 + self.min_offsets_size();
        if lower_bound >= self.plain_size {
            return usize::MAX;
        }
        let vertices_size = self.hilbert().2;
        let offsets = self.dictionary_offsets();
        encoded_int_size_below(&offsets, self.fast_pfor, false, self.plain_size - vertices_size)
            .map_or(usize::MAX, |size| size + vertices_size)
    }

    /// `addVerticesToMortonDictionary`: sorted morton codes, also without removing duplicates.
    fn morton(&mut self) -> &(Vec<u64>, Vec<i32>, usize) {
        self.morton.get_or_insert_with(|| {
            let (vertices, curve) = (self.vertices, self.curve);
            let mut keys: Vec<u64> = Vec::with_capacity(vertices.len());
            keys.extend(vertices.iter().enumerate().map(|(i, &(x, y))| sort_key(curve.morton(x, y), i)));
            sort_keys(&mut keys);
            let codes: Vec<i32> = keys.iter().map(|&k| (k >> 32) as u32 as i32 ^ i32::MIN).collect();
            let size = encoded_morton_codes_size(&codes, self.fast_pfor);
            (keys, codes, size)
        })
    }

    fn morton_offsets(&mut self) -> Vec<i32> {
        let n = self.vertices.len();
        let (keys, _, _) = self.morton();
        let mut offsets = vec![0i32; n];
        for_each_run_end(keys, |position, end| offsets[position] = end as i32 - 1);
        offsets
    }

    /// Encoded size of the Morton candidate, or `usize::MAX` if it is known to be at least the plain size.
    fn morton_size(&mut self) -> usize {
        let lower_bound = self.morton().2 + self.min_offsets_size();
        if lower_bound >= self.plain_size {
            return usize::MAX;
        }
        let codes_size = self.morton().2;
        let offsets = self.morton_offsets();
        encoded_int_size_below(&offsets, self.fast_pfor, false, self.plain_size - codes_size)
            .map_or(usize::MAX, |size| size + codes_size)
    }
}

/// `GeometryEncoder.encodeGeometryColumn` (without feature sorting: `SortSettings.isSortable` is never set by
/// Planetiler).
pub fn encode_geometry_column(
    geometries: &[&Geometry<f64>],
    fast_pfor: bool,
    use_morton_encoding: bool,
) -> anyhow::Result<EncodedGeometryColumn> {
    let t = Topology::build(geometries, false)?;
    let mut d = Dictionaries::new(&t.vertices, fast_pfor);
    let plain_size = d.plain_size;
    let dictionary_size = d.dictionary_size();
    let morton_size = if use_morton_encoding { d.morton_size() } else { usize::MAX };
    let mut out = Vec::new();
    let num_streams = encode_topology(&mut out, &t, fast_pfor);
    if plain_size <= dictionary_size && (!use_morton_encoding || plain_size <= morton_size) {
        encode_vertex_buffer(&mut out, &d.zigzag_delta_vertex_buffer, fast_pfor);
        Ok(EncodedGeometryColumn { num_streams: num_streams + 1, encoded: out })
    } else if dictionary_size < plain_size && (!use_morton_encoding || dictionary_size <= morton_size) {
        encode_vertex_dictionary(&mut out, &mut d, fast_pfor);
        Ok(EncodedGeometryColumn { num_streams: num_streams + 2, encoded: out })
    } else {
        encode_int_stream(
            &mut out,
            &d.morton_offsets(),
            fast_pfor,
            false,
            PhysicalStreamType::Offset,
            Some(LogicalStreamType::Offset(OffsetType::Vertex)),
        );
        let curve = d.curve;
        encode_morton_stream(&mut out, &d.morton().1, curve.num_bits, curve.coordinate_shift as u32, fast_pfor);
        Ok(EncodedGeometryColumn { num_streams: num_streams + 2, encoded: out })
    }
}

fn encode_vertex_dictionary(out: &mut Vec<u8>, d: &mut Dictionaries, fast_pfor: bool) {
    encode_int_stream(
        out,
        &d.dictionary_offsets(),
        fast_pfor,
        false,
        PhysicalStreamType::Offset,
        Some(LogicalStreamType::Offset(OffsetType::Vertex)),
    );
    encode_vertex_buffer(out, &d.hilbert().1, fast_pfor);
}

/// Geometry types, then the non-empty geometries / parts / rings streams. Returns the number of streams written.
fn encode_topology(out: &mut Vec<u8>, t: &Topology, fast_pfor: bool) -> u32 {
    length_stream(out, &t.geometry_types, fast_pfor, None);
    let mut num_streams = 1;
    for (values, length_type) in [
        (&t.num_geometries, LengthType::Geometries),
        (&t.num_parts, LengthType::Parts),
        (&t.num_rings, LengthType::Rings),
    ] {
        if !values.is_empty() {
            length_stream(out, values, fast_pfor, Some(length_type));
            num_streams += 1;
        }
    }
    num_streams
}

/// `GeometryEncoder.encodePretessellatedGeometryColumn`.
pub fn encode_pretessellated_geometry_column(
    geometries: &[&Geometry<f64>],
    fast_pfor: bool,
    encode_polygon_outlines: bool,
) -> anyhow::Result<EncodedGeometryColumn> {
    let t = Topology::build(geometries, true)?;
    let mut d = Dictionaries::new(&t.vertices, fast_pfor);
    let mut out = Vec::new();
    // Currently use pre-tessellation only if all geometries in a FeatureTable are Polygons or MultiPolygons
    let only_polygons = t.geometry_types.iter().all(|&g| g == POLYGON || g == MULTIPOLYGON);
    if only_polygons {
        length_stream(&mut out, &t.geometry_types, fast_pfor, None);
        let num_streams = if encode_polygon_outlines {
            length_stream(&mut out, &t.num_geometries, fast_pfor, Some(LengthType::Geometries));
            length_stream(&mut out, &t.num_parts, fast_pfor, Some(LengthType::Parts));
            length_stream(&mut out, &t.num_rings, fast_pfor, Some(LengthType::Rings));
            7
        } else {
            4
        };
        length_stream(&mut out, &t.num_triangles, fast_pfor, Some(LengthType::Triangles));
        encode_int_stream(
            &mut out,
            &t.index_buffer,
            fast_pfor,
            false,
            PhysicalStreamType::Offset,
            Some(LogicalStreamType::Offset(OffsetType::Index)),
        );
        encode_vertex_buffer(&mut out, &d.zigzag_delta_vertex_buffer, fast_pfor);
        return Ok(EncodedGeometryColumn { num_streams, encoded: out });
    }
    let num_streams = encode_topology(&mut out, &t, fast_pfor);
    if d.plain_size <= d.dictionary_size() && d.plain_size <= d.morton_size() {
        encode_vertex_buffer(&mut out, &d.zigzag_delta_vertex_buffer, fast_pfor);
        Ok(EncodedGeometryColumn { num_streams: num_streams + 1, encoded: out })
    } else {
        // Java only considers morton encoding here when useMortonEncoding is set, which it never is for
        // pre-tessellated columns
        encode_vertex_dictionary(&mut out, &mut d, fast_pfor);
        Ok(EncodedGeometryColumn { num_streams: num_streams + 2, encoded: out })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_keys_matches_sort_unstable() {
        let mut state = 3u64;
        let mut next = move || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as i32
        };
        for (len, range) in [(10, 100), (255, 1 << 20), (256, 7), (300, 1 << 26), (5000, 1000), (70_000, i32::MAX)] {
            let codes: Vec<i32> = (0..len).map(|i| if i % 5 == 0 { -next() % range } else { next() % range }).collect();
            let mut keys: Vec<u64> = codes.iter().enumerate().map(|(i, &c)| sort_key(c, i)).collect();
            let mut expected = keys.clone();
            expected.sort_unstable();
            sort_keys(&mut keys);
            assert_eq!(expected, keys);
        }
    }

    #[test]
    fn curve_params() {
        let c = CurveParams::new(0, 4096);
        assert_eq!(13, c.num_bits);
        assert_eq!(0, c.coordinate_shift);
        let c = CurveParams::new(-128, 4223);
        assert_eq!(128, c.coordinate_shift);
        assert_eq!(13, c.num_bits);
        assert_eq!(CurveParams::new(0, 4095).num_bits, 12);
        assert_eq!(CurveParams::new(0, 0).num_bits, 0);
    }

    #[test]
    fn morton() {
        let c = CurveParams::new(0, 3);
        assert_eq!(0, c.morton(0, 0));
        assert_eq!(1, c.morton(1, 0));
        assert_eq!(2, c.morton(0, 1));
        assert_eq!(3, c.morton(1, 1));
        assert_eq!(15, c.morton(3, 3));
    }

    #[test]
    fn hilbert() {
        // level 1 curve: (0,0) -> 0, (0,1) -> 1, (1,1) -> 2, (1,0) -> 3
        assert_eq!(0, hilbert_xy_to_index(1, 0, 0));
        assert_eq!(1, hilbert_xy_to_index(1, 0, 1));
        assert_eq!(2, hilbert_xy_to_index(1, 1, 1));
        assert_eq!(3, hilbert_xy_to_index(1, 1, 0));
    }
}
