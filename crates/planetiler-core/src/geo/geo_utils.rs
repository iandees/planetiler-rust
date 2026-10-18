//! Port of `geo/GeoUtils.java`.
//!
//! "World" coordinates are web mercator scaled to `0..1` with `(0, 0)` in the top-left (north-west) corner.

use std::sync::LazyLock;

use geo::{MapCoords, Winding};
use geo_types::{
    Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};

pub use crate::geo::crs::{Crs, MathTransform};
use crate::geo::envelope::Envelope;
use crate::geo::geometry_exception::{GeometryException, GeometryResult};
use crate::geo::geometry_transformer::{self, GeometryTransformer};
use crate::geo::geometry_type::GeometryType;
pub use crate::geo::precision_model::PrecisionModel;
use crate::geo::{crs, polygon_overlay};
use crate::stats::stats::Stats;

pub const WORLD_RADIUS_METERS_AT_EQUATOR: f64 = 6_378_137.0;
pub const AVERAGE_WORLD_RADIUS_METERS: f64 = 6_371_008.8;
pub const WORLD_CIRCUMFERENCE_METERS: f64 = std::f64::consts::PI * 2.0 * WORLD_RADIUS_METERS_AT_EQUATOR;
const RADIANS_PER_DEGREE: f64 = std::f64::consts::PI / 180.0;
const DEGREES_PER_RADIAN: f64 = 180.0 / std::f64::consts::PI;
/// Factor used to compute spherical polygon area in square meters.
pub const AREA_FACTOR: f64 = AVERAGE_WORLD_RADIUS_METERS * AVERAGE_WORLD_RADIUS_METERS / 2.0;

static MAX_LAT: LazyLock<f64> = LazyLock::new(|| get_world_lat(-0.1));
static MIN_LAT: LazyLock<f64> = LazyLock::new(|| get_world_lat(1.1));
const QUANTIZED_WORLD_SIZE: f64 = 2147483648.0; // 2^31
const HALF_QUANTIZED_WORLD_SIZE: f64 = QUANTIZED_WORLD_SIZE / 2.0;
const LOWER_32_BIT_MASK: u64 = (1u64 << 32) - 1;

/// Bounds of the world in world coordinates.
pub const WORLD_BOUNDS: Envelope = Envelope { min_x: 0.0, max_x: 1.0, min_y: 0.0, max_y: 1.0 };

/// Bounds of the world in latitude/longitude coordinates.
pub static WORLD_LAT_LON_BOUNDS: LazyLock<Envelope> = LazyLock::new(|| to_lat_lon_bounds(&WORLD_BOUNDS));

/// Number of tile pixels per tile edge in vector tile coordinates.
pub const TILE_EXTENT: f64 = 4096.0;
/// Scale factor of the tile precision model: geometries in tile pixel (0-256) coordinates are rounded to the nearest
/// `1 / TILE_PRECISION_SCALE`.
pub const TILE_PRECISION_SCALE: f64 = 4096.0 / 256.0;

/// Convert a latitude/longitude geometry to world coordinates.
pub fn lat_lon_to_world_coords(geom: &Geometry<f64>) -> Geometry<f64> {
    geom.map_coords(|c| Coord { x: get_world_x(c.x), y: get_world_y(c.y) })
}

/// Convert a world-coordinate geometry back to latitude/longitude.
pub fn world_to_lat_lon_coords(geom: &Geometry<f64>) -> Geometry<f64> {
    geom.map_coords(|c| Coord { x: get_world_lon(c.x), y: get_world_lat(c.y) })
}

/// Java's `toLatLonBoundsBounds`.
pub fn to_lat_lon_bounds(world_bounds: &Envelope) -> Envelope {
    Envelope::new(
        get_world_lon(world_bounds.min_x),
        get_world_lon(world_bounds.max_x),
        get_world_lat(world_bounds.min_y),
        get_world_lat(world_bounds.max_y),
    )
}

pub fn to_world_bounds(lon_lat_bounds: &Envelope) -> Envelope {
    Envelope::new(
        get_world_x(lon_lat_bounds.min_x),
        get_world_x(lon_lat_bounds.max_x),
        get_world_y(lon_lat_bounds.min_y),
        get_world_y(lon_lat_bounds.max_y),
    )
}

/// Longitude for a world x coordinate.
pub fn get_world_lon(x: f64) -> f64 {
    x * 360.0 - 180.0
}

/// Latitude for a world y coordinate.
pub fn get_world_lat(y: f64) -> f64 {
    let n = std::f64::consts::PI - 2.0 * std::f64::consts::PI * y;
    DEGREES_PER_RADIAN * (0.5 * (n.exp() - (-n).exp())).atan()
}

/// World x coordinate for a longitude.
pub fn get_world_x(longitude: f64) -> f64 {
    (longitude + 180.0) / 360.0
}

/// World y coordinate for a latitude, clamped to `-0.1..1.1`.
pub fn get_world_y(latitude: f64) -> f64 {
    if latitude <= *MIN_LAT {
        return 1.1;
    }
    if latitude >= *MAX_LAT {
        return -0.1;
    }
    let sin = (latitude * RADIANS_PER_DEGREE).sin();
    0.5 - 0.25 * ((1.0 + sin) / (1.0 - sin)).ln() / std::f64::consts::PI
}

/// Pack a longitude/latitude into a single 64-bit value with 31 bits of precision per axis.
pub fn encode_flat_location(lon: f64, lat: f64) -> u64 {
    let world_x = get_world_x(lon) + 1.0;
    let world_y = get_world_y(lat) + 1.0;
    let x = (world_x * HALF_QUANTIZED_WORLD_SIZE) as i64 as u64;
    let y = (world_y * HALF_QUANTIZED_WORLD_SIZE) as i64 as u64;
    (x << 32) | (y & LOWER_32_BIT_MASK)
}

/// World y coordinate from a value packed by [`encode_flat_location`].
pub fn decode_world_y(encoded: u64) -> f64 {
    ((encoded & LOWER_32_BIT_MASK) as f64 / HALF_QUANTIZED_WORLD_SIZE) - 1.0
}

/// World x coordinate from a value packed by [`encode_flat_location`].
pub fn decode_world_x(encoded: u64) -> f64 {
    ((encoded >> 32) as f64 / HALF_QUANTIZED_WORLD_SIZE) - 1.0
}

pub fn get_zoom_from_lon_lat_bounds(envelope: &Envelope) -> f64 {
    get_zoom_from_world_bounds(&to_world_bounds(envelope))
}

pub fn get_zoom_from_world_bounds(world_bounds: &Envelope) -> f64 {
    let max_edge = world_bounds.width().max(world_bounds.height());
    (-max_edge.ln() / 2f64.ln()).max(0.0)
}

pub fn meters_per_pixel_at_equator(zoom: u32) -> f64 {
    WORLD_CIRCUMFERENCE_METERS / 2f64.powf(zoom as f64 + 8.0)
}

pub fn meters_to_pixel_at_equator(zoom: u32, meters: f64) -> f64 {
    meters / meters_per_pixel_at_equator(zoom)
}

/// Minimum zoom level at which a feature of `world_geometry_size` (in world units) is at least `min_pixel_size`
/// pixels.
pub fn min_zoom_for_pixel_size(world_geometry_size: f64, min_pixel_size: f64) -> u32 {
    let world_pixels = world_geometry_size * 256.0;
    let zoom = (min_pixel_size / world_pixels).ln() / 2f64.ln();
    (zoom.ceil().max(0.0) as u32).min(crate::config::planetiler_config::MAX_MAXZOOM)
}

/// Format an envelope as `Envelope(minX,minY,maxX,maxY)` like Java's `GeoUtils.envelopeToString`.
pub fn envelope_to_string(envelope: &Envelope) -> String {
    format!("Envelope({:?},{:?},{:?},{:?})", envelope.min_x, envelope.min_y, envelope.max_x, envelope.max_y)
}

/// Rounding precision for 256x256px tiles encoded using 4096 values.
pub const TILE_PRECISION: PrecisionModel = PrecisionModel::Fixed { scale: TILE_PRECISION_SCALE };

/// An empty geometry collection (Java's `EMPTY_GEOMETRY`).
pub fn empty_geometry() -> Geometry<f64> {
    geometry_transformer::empty_geometry()
}

/// An empty line string (Java's `EMPTY_LINE`).
pub fn empty_line() -> LineString<f64> {
    LineString(vec![])
}

/// An empty polygon (Java's `EMPTY_POLYGON`).
pub fn empty_polygon() -> Polygon<f64> {
    geometry_transformer::empty_polygon()
}

/// Java's `EMPTY_POINT`: geo has no empty point type, so this is an empty multipoint.
pub fn empty_point() -> Geometry<f64> {
    Geometry::MultiPoint(MultiPoint(vec![]))
}

pub fn point(x: f64, y: f64) -> Point<f64> {
    Point::new(x, y)
}

pub fn create_multi_line_string(line_strings: Vec<LineString<f64>>) -> MultiLineString<f64> {
    MultiLineString(line_strings)
}

pub fn create_multi_polygon(polygons: Vec<Polygon<f64>>) -> MultiPolygon<f64> {
    MultiPolygon(polygons)
}

pub fn create_multi_point(points: Vec<Point<f64>>) -> MultiPoint<f64> {
    MultiPoint(points)
}

pub fn create_geometry_collection(geoms: Vec<Geometry<f64>>) -> Geometry<f64> {
    Geometry::GeometryCollection(GeometryCollection(geoms))
}

pub fn create_polygon(exterior: LineString<f64>, rings: Vec<LineString<f64>>) -> Polygon<f64> {
    Polygon::new(exterior, rings)
}

/// A single line string if there is only one, otherwise a multilinestring.
pub fn combine_line_strings(mut line_strings: Vec<LineString<f64>>) -> Geometry<f64> {
    if line_strings.len() == 1 {
        Geometry::LineString(line_strings.pop().unwrap())
    } else {
        Geometry::MultiLineString(MultiLineString(line_strings))
    }
}

/// A single polygon if there is only one, otherwise a multipolygon.
pub fn combine_polygons(mut polys: Vec<Polygon<f64>>) -> Geometry<f64> {
    if polys.len() == 1 { Geometry::Polygon(polys.pop().unwrap()) } else { Geometry::MultiPolygon(MultiPolygon(polys)) }
}

/// A single point if there is only one, otherwise a multipoint.
pub fn combine_points(mut points: Vec<Point<f64>>) -> Geometry<f64> {
    if points.len() == 1 { Geometry::Point(points.pop().unwrap()) } else { Geometry::MultiPoint(MultiPoint(points)) }
}

/// Combines multiple geometries into one geometry collection, flattening one level of nested collections (including
/// multi-geometries, which are collections in JTS).
pub fn combine(geometries: Vec<Geometry<f64>>) -> Geometry<f64> {
    let mut inner = Vec::with_capacity(geometries.len());
    for geom in geometries {
        match geom {
            Geometry::GeometryCollection(gc) => inner.extend(gc.0),
            Geometry::MultiPolygon(mp) => inner.extend(mp.0.into_iter().map(Geometry::Polygon)),
            Geometry::MultiLineString(ml) => inner.extend(ml.0.into_iter().map(Geometry::LineString)),
            Geometry::MultiPoint(mp) => inner.extend(mp.0.into_iter().map(Geometry::Point)),
            other => inner.push(other),
        }
    }
    if inner.len() == 1 { inner.pop().unwrap() } else { create_geometry_collection(inner) }
}

/// Coordinates from a list of `x, y, x, y, ...` values.
pub fn coordinate_sequence(coords: &[f64]) -> Vec<Coord<f64>> {
    coords.as_chunks::<2>().0.iter().map(|c| Coord { x: c[0], y: c[1] }).collect()
}

/// Returns true if `geom` is valid according to JTS rules.
pub fn is_valid(geom: &Geometry<f64>) -> bool {
    crate::geo::is_valid::is_valid(geom)
}

/// Attempt to fix any self-intersections or overlaps in `geom` (JTS `geom.buffer(0)`).
pub fn fix_polygon(geom: &Geometry<f64>) -> GeometryResult<Geometry<f64>> {
    polygon_overlay::buffer_zero(geom).map_err(|e| {
        GeometryException::new("fix_polygon_topology_error", format!("robustness error fixing polygon: {e}"))
    })
}

/// More aggressive fix for self-intersections than [`fix_polygon`] that expands then contracts the shape by
/// `buffer`.
pub fn fix_polygon_with_buffer(geom: &Geometry<f64>, buffer: f64) -> GeometryResult<Geometry<f64>> {
    use geo::Buffer;
    let err = |e: GeometryException| {
        GeometryException::new("fix_polygon_buffer_topology_error", format!("robustness error fixing polygon: {e}"))
    };
    let polys: Vec<Polygon<f64>> = polygon_overlay::polygons_of(geom).into_iter().cloned().collect();
    if polys.is_empty() {
        return Ok(Geometry::Polygon(empty_polygon()));
    }
    let expanded = MultiPolygon(polys).buffer(buffer);
    let contracted = expanded.buffer(-buffer);
    // normalize ring orientation/validity the same way as the other overlay results
    polygon_overlay::buffer_zero(&Geometry::MultiPolygon(contracted)).map_err(err)
}

/// Rounds coordinates to `precision` without removing collapsed polygons (unlike `GeometryPrecisionReducer`
/// `reducePointwise`, does not remove duplicate points... except consecutive ones). Rings that end up with fewer than
/// 4 points are removed.
struct PointwiseRounder {
    precision: PrecisionModel,
}

impl GeometryTransformer for PointwiseRounder {
    fn transform_coordinates(&mut self, coords: &[Coord<f64>], _is_ring: bool) -> Option<Vec<Coord<f64>>> {
        if coords.len() < 4 {
            return None;
        }
        let mut result: Vec<Coord<f64>> = Vec::with_capacity(coords.len());
        let (mut last_x, mut last_y) = (f64::MIN_POSITIVE, f64::MIN_POSITIVE);
        for c in coords {
            let x = self.precision.make_precise(c.x);
            let y = self.precision.make_precise(c.y);
            if x != last_x || y != last_y {
                result.push(Coord { x, y });
            }
            last_x = x;
            last_y = y;
        }
        if result.len() < 4 { None } else { Some(result) }
    }
}

/// Java's `OrientationFixer`: outer rings clockwise, inner rings counter-clockwise (JTS `Orientation.isCCW` sense).
fn fix_orientation(geom: Geometry<f64>) -> Geometry<f64> {
    fn fix_poly(mut p: Polygon<f64>) -> Polygon<f64> {
        p.exterior_mut(|r| {
            if r.is_ccw() {
                r.0.reverse();
            }
        });
        p.interiors_mut(|holes| {
            for h in holes {
                if !h.is_ccw() {
                    h.0.reverse();
                }
            }
        });
        p
    }
    match geom {
        Geometry::Polygon(p) => Geometry::Polygon(fix_poly(p)),
        Geometry::MultiPolygon(mp) => Geometry::MultiPolygon(MultiPolygon(mp.0.into_iter().map(fix_poly).collect())),
        Geometry::GeometryCollection(gc) => {
            Geometry::GeometryCollection(GeometryCollection(gc.0.into_iter().map(fix_orientation).collect()))
        }
        other => other,
    }
}

/// Returns a copy of `geom` with coordinates rounded to [`TILE_PRECISION`] that fixes any polygon
/// self-intersections or overlaps that may have caused.
pub fn snap_and_fix_polygon(geom: &Geometry<f64>, stats: &dyn Stats, stage: &str) -> GeometryResult<Geometry<f64>> {
    snap_and_fix_polygon_with_precision(geom, &TILE_PRECISION, stats, stage)
}

/// Returns a copy of `geom` with coordinates rounded to `tile_precision` that fixes any polygon self-intersections or
/// overlaps that may have caused.
///
/// The fast path rounds each coordinate and returns the result with shells oriented clockwise and holes
/// counter-clockwise if it is still valid. Otherwise the input is fixed (if needed, counted as
/// `{stage}_snap_fix_input`) and reduced to the precision grid with a re-noding overlay.
pub fn snap_and_fix_polygon_with_precision(
    geom: &Geometry<f64>,
    tile_precision: &PrecisionModel,
    stats: &dyn Stats,
    stage: &str,
) -> GeometryResult<Geometry<f64>> {
    let naive_snap = PointwiseRounder { precision: *tile_precision }.transform(geom);
    if is_valid(&naive_snap) {
        return Ok(fix_orientation(naive_snap));
    }
    let mut geom = std::borrow::Cow::Borrowed(geom);
    if !is_valid(&geom) {
        geom = std::borrow::Cow::Owned(fix_polygon(&geom)?);
        stats.data_error(&format!("{stage}_snap_fix_input"));
    }
    match polygon_overlay::reduce_precision(&geom, tile_precision) {
        Ok(result) => Ok(result),
        Err(_) => {
            // precision reduction fails if geometry is invalid, so attempt to fix it then try again
            let fixed = polygon_overlay::geometry_fixer(&geom)?;
            stats.data_error(&format!("{stage}_snap_fix_input2"));
            match polygon_overlay::reduce_precision(&fixed, tile_precision) {
                Ok(result) => Ok(result),
                Err(_) => {
                    // give it one last try but with more aggressive fixing, just in case (see issue #511)
                    let fixed = fix_polygon_with_buffer(&fixed, tile_precision.grid_size() / 2.0)?;
                    stats.data_error(&format!("{stage}_snap_fix_input3"));
                    polygon_overlay::reduce_precision(&fixed, tile_precision).map_err(|_| {
                        stats.data_error(&format!("{stage}_snap_fix_input3_failed"));
                        GeometryException::new("snap_third_time_failed", "Error reducing precision")
                    })
                }
            }
        }
    }
}

fn wrap_double(value: f64, max: f64) -> f64 {
    let mut value = value % max;
    if value < 0.0 {
        value += max;
    }
    value
}

fn long_pair(a: i32, b: i32) -> i64 {
    ((a as i64) << 32) | ((b as i64) & LOWER_32_BIT_MASK as i64)
}

/// Breaks the world up into a grid and returns an ID for the square that `coord` falls into.
///
/// `tiles_at_zoom` is the tile width of the world at this zoom level, `label_grid_tile_size` the tile width of each
/// grid square and `coord` is scaled to this zoom level.
pub fn label_grid_id(tiles_at_zoom: i32, label_grid_tile_size: f64, coord: Coord<f64>) -> i64 {
    long_pair(
        (wrap_double(coord.x, tiles_at_zoom as f64) / label_grid_tile_size).floor() as i32,
        (coord.y / label_grid_tile_size).floor() as i32,
    )
}

/// Returns line strings for every inner and outer ring contained in a polygon.
///
/// Fails if `geom` contains anything other than polygons or line strings.
pub fn polygon_to_line_string(geom: &Geometry<f64>) -> GeometryResult<Geometry<f64>> {
    let mut line_strings = Vec::new();
    get_line_strings(geom, &mut line_strings)?;
    match line_strings.len() {
        0 => Err(GeometryException::new("polygon_to_linestring_empty", "No line strings")),
        1 => Ok(Geometry::LineString(line_strings.pop().unwrap())),
        _ => Ok(Geometry::MultiLineString(MultiLineString(line_strings))),
    }
}

fn get_line_strings(input: &Geometry<f64>, output: &mut Vec<LineString<f64>>) -> GeometryResult<()> {
    match input {
        Geometry::LineString(ls) => output.push(ls.clone()),
        Geometry::Line(l) => output.push((*l).into()),
        Geometry::Polygon(p) => {
            output.push(p.exterior().clone());
            output.extend(p.interiors().iter().cloned());
        }
        Geometry::Rect(r) => get_line_strings(&Geometry::Polygon(r.to_polygon()), output)?,
        Geometry::Triangle(t) => get_line_strings(&Geometry::Polygon(t.to_polygon()), output)?,
        Geometry::MultiLineString(ml) => output.extend(ml.0.iter().cloned()),
        Geometry::MultiPolygon(mp) => {
            for p in &mp.0 {
                output.push(p.exterior().clone());
                output.extend(p.interiors().iter().cloned());
            }
        }
        Geometry::GeometryCollection(gc) => {
            for g in &gc.0 {
                get_line_strings(g, output)?;
            }
        }
        Geometry::MultiPoint(mp) if mp.0.is_empty() => {}
        Geometry::Point(_) | Geometry::MultiPoint(_) => {
            return Err(GeometryException::new(
                "get_line_strings_bad_type",
                format!("unrecognized geometry type: {:?}", GeometryType::type_of(input)),
            ));
        }
    }
    Ok(())
}

/// Returns a point approximately `ratio` of the way from start to end and `offset` units to the right.
pub fn point_along_offset(line_string: &LineString<f64>, ratio: f64, offset: f64) -> Point<f64> {
    let num_points = line_string.0.len() as i64;
    let middle = ((num_points as f64 * ratio) as i64).clamp(0, (num_points - 2).max(0)) as usize;
    let a = line_string.0[middle];
    let b = line_string.0[(middle + 1).min(line_string.0.len() - 1)];
    // JTS LineSegment.pointAlongOffset(0.5, offset)
    let segx = a.x + 0.5 * (b.x - a.x);
    let segy = a.y + 0.5 * (b.y - a.y);
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len = (dx * dx + dy * dy).sqrt();
    let (mut ux, mut uy) = (0.0, 0.0);
    if offset != 0.0 && len > 0.0 {
        ux = offset * dx / len;
        uy = offset * dy / len;
    }
    Point::new(segx - uy, segy + ux)
}

/// Returns `true` if the signed area of the triangle formed by 3 sequential points does not change sign anywhere
/// along `ring`, ignoring repeated and collinear points (and very small concavities relative to the overall shape).
pub fn is_convex(ring: &LineString<f64>) -> bool {
    let threshold = 1e-3;
    let min_points_to_check = 10usize;
    let seq = &ring.0;
    let size = seq.len();
    if size <= 3 {
        return false;
    }

    // ignore leading repeated points
    let (c0x, c0y) = (seq[0].x, seq[0].y);
    let (mut c1x, mut c1y) = (f64::NAN, f64::NAN);
    let mut i = 1;
    while i < size {
        c1x = seq[i].x;
        c1y = seq[i].y;
        if c1x != c0x || c1y != c0y {
            break;
        }
        i += 1;
    }

    let mut dx1 = c1x - c0x;
    let mut dy1 = c1y - c0y;
    let mut neg_z = 1e-20;
    let mut pos_z = 1e-20;

    // need to wrap around to make sure the triangle formed by last and first points does not change sign
    while i <= size + 1 {
        // first and last point should be the same, so skip index 0
        let idx = if i < size { i } else { i + 1 - size };
        let (c2x, c2y) = (seq[idx].x, seq[idx].y);
        let dx2 = c2x - c1x;
        let dy2 = c2y - c1y;
        let z = dx1 * dy2 - dy1 * dx2;
        let abs_z = z.abs();

        // look for sign changes in the triangles formed by sequential points but allow for rounding errors and
        // small concavities relative to the overall shape
        let mut extended_bounds = false;
        if z < 0.0 && abs_z > neg_z {
            neg_z = abs_z;
            extended_bounds = true;
        } else if z > 0.0 && abs_z > pos_z {
            pos_z = abs_z;
            extended_bounds = true;
        }

        if i == min_points_to_check || (i > min_points_to_check && extended_bounds) {
            let ratio = if neg_z < pos_z { neg_z / pos_z } else { pos_z / neg_z };
            if ratio > threshold {
                return false;
            }
        }

        c1x = c2x;
        c1y = c2y;
        dx1 = dx2;
        dy1 = dy2;
        i += 1;
    }
    (if neg_z < pos_z { neg_z / pos_z } else { pos_z / neg_z }) < threshold
}

/// Unsigned area of a ring (JTS `Area.ofRing`).
pub fn ring_area(ring: &[Coord<f64>]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let x0 = ring[0].x;
    let mut sum = 0.0;
    for i in 1..ring.len() - 1 {
        let x = ring[i].x - x0;
        let y1 = ring[i + 1].y;
        let y2 = ring[i - 1].y;
        sum += x * (y2 - y1);
    }
    (sum / 2.0).abs()
}

/// If `geometry` is a multipolygon, returns a copy with polygons sorted by descending area of the outer shell,
/// otherwise returns the input geometry.
pub fn sort_polygons_by_area_descending(geometry: Geometry<f64>) -> Geometry<f64> {
    match geometry {
        Geometry::MultiPolygon(mp) => {
            let mut with_area: Vec<(f64, Polygon<f64>)> =
                mp.0.into_iter().map(|p| (ring_area(&p.exterior().0), p)).collect();
            with_area.sort_by(|a, b| b.0.total_cmp(&a.0));
            Geometry::MultiPolygon(MultiPolygon(with_area.into_iter().map(|(_, p)| p).collect()))
        }
        other => other,
    }
}

/// Euclidean length of a line string.
pub fn line_length(line: &LineString<f64>) -> f64 {
    line.0.windows(2).map(|w| (w[1].x - w[0].x).hypot(w[1].y - w[0].y)).sum()
}

/// JTS `Geometry.getLength()`: length of lines and perimeter of polygons.
pub fn length(geom: &Geometry<f64>) -> f64 {
    match geom {
        Geometry::Point(_) | Geometry::MultiPoint(_) => 0.0,
        Geometry::Line(l) => (l.end.x - l.start.x).hypot(l.end.y - l.start.y),
        Geometry::LineString(ls) => line_length(ls),
        Geometry::MultiLineString(ml) => ml.0.iter().map(line_length).sum(),
        Geometry::Polygon(p) => line_length(p.exterior()) + p.interiors().iter().map(line_length).sum::<f64>(),
        Geometry::MultiPolygon(mp) => mp.0.iter().map(|p| length(&Geometry::Polygon(p.clone()))).sum(),
        Geometry::Rect(r) => 2.0 * (r.width() + r.height()),
        Geometry::Triangle(t) => length(&Geometry::Polygon(t.to_polygon())),
        Geometry::GeometryCollection(gc) => gc.0.iter().map(length).sum(),
    }
}

/// JTS `Geometry.getArea()`: unsigned area of the polygonal components.
pub fn area(geom: &Geometry<f64>) -> f64 {
    use geo::Area;
    geom.unsigned_area()
}

/// The longest line string in `multi_line_string`, or `None` if it is empty.
pub fn get_longest_line(multi_line_string: &MultiLineString<f64>) -> Option<&LineString<f64>> {
    let mut result = None;
    let mut max = -1.0;
    for ls in &multi_line_string.0 {
        let length = line_length(ls);
        if length > max {
            max = length;
            result = Some(ls);
        }
    }
    result
}

/// Parse a WKB-encoded geometry (Java's `wkbReader().read`).
pub fn read_wkb(bytes: &[u8]) -> GeometryResult<Geometry<f64>> {
    use geo_traits::to_geo::ToGeoGeometry;
    let wkb = wkb::reader::read_wkb(bytes)
        .map_err(|e| GeometryException::new("wkb_parse_error", format!("error parsing WKB: {e}")))?;
    Ok(wkb.try_to_geometry().unwrap_or_else(empty_geometry))
}

/// Encode a geometry as little-endian WKB.
pub fn write_wkb(geom: &Geometry<f64>) -> GeometryResult<Vec<u8>> {
    let mut out = Vec::new();
    wkb::writer::write_geometry(
        &mut out,
        geom,
        &wkb::writer::WriteOptions { endianness: wkb::Endianness::LittleEndian },
    )
    .map_err(|e| GeometryException::new("wkb_write_error", format!("error writing WKB: {e}")))?;
    Ok(out)
}

/// Parse a WKT geometry (Java's `wktReader().read`).
pub fn read_wkt(wkt: &str) -> GeometryResult<Geometry<f64>> {
    crate::geo::wkt::read_wkt(wkt)
}

/// Format a geometry as WKT.
pub fn to_wkt(geom: &Geometry<f64>) -> String {
    crate::geo::wkt::to_wkt(geom)
}

/// Returns the distance in meters between 2 lat/lon coordinates using the haversine formula.
pub fn meters_between(from_lon: f64, from_lat: f64, to_lon: f64, to_lat: f64) -> f64 {
    let sin_delta_lat = ((to_lat - from_lat) * RADIANS_PER_DEGREE / 2.0).sin();
    let sin_delta_lon = ((to_lon - from_lon) * RADIANS_PER_DEGREE / 2.0).sin();
    let a = sin_delta_lat * sin_delta_lat
        + sin_delta_lon * sin_delta_lon * (from_lat * RADIANS_PER_DEGREE).cos() * (to_lat * RADIANS_PER_DEGREE).cos();
    AVERAGE_WORLD_RADIUS_METERS * 2.0 * a.sqrt().asin()
}

/// Returns the sum of the length of all edges using [`meters_between`].
pub fn line_length_meters(sequence: &[Coord<f64>]) -> f64 {
    sequence.windows(2).map(|w| meters_between(w[0].x, w[0].y, w[1].x, w[1].y)).sum()
}

/// Returns the approximate area in meters of a ring in lat/lon degree coordinates.
///
/// See "Some Algorithms for Polygons on a Sphere", JPL Publication 07-03, Jet Propulsion Laboratory, Pasadena, CA,
/// June 2007 (<https://trs.jpl.nasa.gov/handle/2014/40409>).
pub fn ring_area_meters(ring: &[Coord<f64>]) -> f64 {
    if ring.len() < 2 {
        return 0.0;
    }
    let mut total = 0.0;
    let num_edges = ring.len() - 1;
    for i in 0..num_edges {
        let lower_x = ring[i].x * RADIANS_PER_DEGREE;
        let mid_y = ring[if i + 1 == num_edges { 0 } else { i + 1 }].y * RADIANS_PER_DEGREE;
        let upper_x = ring[if i + 2 >= num_edges { (i + 2) % num_edges } else { i + 2 }].x * RADIANS_PER_DEGREE;
        total += (upper_x - lower_x) * mid_y.sin();
    }
    total.abs() * AREA_FACTOR
}

/// Returns the approximate area in square meters of a polygon, or all polygons contained within a multigeometry,
/// using [`ring_area_meters`].
pub fn area_in_meters(lat_lon_geom: &Geometry<f64>) -> f64 {
    fn poly_area(p: &Polygon<f64>) -> f64 {
        ring_area_meters(&p.exterior().0) - p.interiors().iter().map(|h| ring_area_meters(&h.0)).sum::<f64>()
    }
    match lat_lon_geom {
        Geometry::Polygon(p) => poly_area(p),
        Geometry::MultiPolygon(mp) => mp.0.iter().map(poly_area).sum(),
        Geometry::Rect(r) => poly_area(&r.to_polygon()),
        Geometry::Triangle(t) => poly_area(&t.to_polygon()),
        Geometry::GeometryCollection(gc) => gc.0.iter().map(area_in_meters).sum(),
        _ => 0.0,
    }
}

/// Returns the approximate length in meters of a line, or all lines contained within a multigeometry, using
/// [`line_length_meters`].
pub fn length_in_meters(lat_lon_geom: &Geometry<f64>) -> f64 {
    match lat_lon_geom {
        Geometry::LineString(ls) => line_length_meters(&ls.0),
        Geometry::Line(l) => line_length_meters(&[l.start, l.end]),
        Geometry::MultiLineString(ml) => ml.0.iter().map(|l| line_length_meters(&l.0)).sum(),
        Geometry::GeometryCollection(gc) => gc.0.iter().map(length_in_meters).sum(),
        _ => 0.0,
    }
}

/// A transform that swaps the X/Y coordinates.
pub fn swap_xy_transform() -> MathTransform {
    MathTransform::swap_xy()
}

/// Creates a transform that maps coordinates from `source` to `dest` CRS. If `source` axis ordering does not match
/// `force_longitude_first` then it force-swaps x/y coordinates first.
pub fn find_math_transform(source: &Crs, dest: &Crs, force_longitude_first: Option<bool>) -> MathTransform {
    crs::find_math_transform(source, dest, force_longitude_first)
}

/// Decodes a CRS from an `EPSG:1234` code or WKT. Add a `:lon_first` / `:lat_first` suffix to set the axis order;
/// otherwise it defaults to the order of `base`. See [`crate::geo::crs`] for supported definitions.
pub fn decode_crs(crs: &str, base: Option<&Crs>) -> anyhow::Result<Crs> {
    crs::decode_crs(crs, base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;
    use crate::stats::stats::InMemoryStats;
    use approx::assert_relative_eq;
    use geo::{AffineOps, AffineTransform, Contains};

    #[test]
    fn projections() {
        assert_relative_eq!(0.5, get_world_x(0.0));
        assert_relative_eq!(0.5, get_world_y(0.0));
        assert_relative_eq!(0.0, get_world_x(-180.0));
        assert_relative_eq!(1.0, get_world_x(180.0));
        assert_relative_eq!(0.0, get_world_y(85.05112877980659), epsilon = 1e-9);
        for lat in [-80.0, -45.0, 0.0, 12.3, 60.0] {
            assert_relative_eq!(lat, get_world_lat(get_world_y(lat)), epsilon = 1e-9);
        }
        for lon in [-180.0, -12.0, 0.0, 99.0, 180.0] {
            assert_relative_eq!(lon, get_world_lon(get_world_x(lon)), epsilon = 1e-9);
        }
    }

    #[test]
    fn flat_location() {
        let encoded = encode_flat_location(-73.5, 45.5);
        assert_relative_eq!(get_world_x(-73.5), decode_world_x(encoded), epsilon = 1e-9);
        assert_relative_eq!(get_world_y(45.5), decode_world_y(encoded), epsilon = 1e-9);
    }

    #[test]
    fn min_zoom_for_pixel_size_test() {
        assert_eq!(0, min_zoom_for_pixel_size(1.0, 256.0));
        assert_eq!(1, min_zoom_for_pixel_size(0.5, 256.0));
        assert_eq!(16, min_zoom_for_pixel_size(1e-12, 256.0));
    }

    #[test]
    fn world_coords() {
        for (lat, lon, world_x, world_y) in [
            (0.0, 0.0, 0.5, 0.5),
            (0.0, -180.0, 0.0, 0.5),
            (0.0, 180.0, 1.0, 0.5),
            (0.0, 180.0 - 1e-7, 1.0, 0.5),
            (45.0, 0.0, 0.5, 0.359725),
            (-45.0, 0.0, 0.5, 1.0 - 0.359725),
            (86.0, -198.0, -0.05, -0.03391287),
            (-86.0, 198.0, 1.05, 1.03391287),
        ] {
            assert_relative_eq!(world_y, get_world_y(lat), epsilon = 1e-5);
            assert_relative_eq!(world_x, get_world_x(lon), epsilon = 1e-5);
            let encoded = encode_flat_location(lon, lat);
            assert_relative_eq!(world_y, decode_world_y(encoded), epsilon = 1e-5);
            assert_relative_eq!(world_x, decode_world_x(encoded), epsilon = 1e-5);

            let input: Geometry<f64> = new_point(lon, lat).into();
            let expected: Geometry<f64> = new_point(world_x, world_y).into();
            let actual = lat_lon_to_world_coords(&input);
            assert_eq!(round(&expected), round(&actual));
            assert_eq!(round(&input), round(&world_to_lat_lon_coords(&actual)));
        }
    }

    #[test]
    fn polygon_to_line_string_test() {
        let expected = Geometry::LineString(new_line_string(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]));
        assert_eq!(expected, polygon_to_line_string(&rectangle(0.0, 1.0).into()).unwrap());
        assert_eq!(expected, polygon_to_line_string(&new_multi_polygon(vec![rectangle(0.0, 1.0)]).into()).unwrap());
        assert_eq!(expected, polygon_to_line_string(&rectangle(0.0, 1.0).exterior().clone().into()).unwrap());
        assert!(polygon_to_line_string(&new_point(0.0, 0.0).into()).is_err());
    }

    #[test]
    fn complex_polygon_to_line_string() {
        assert_eq!(
            Geometry::MultiLineString(new_multi_line_string(vec![
                new_line_string(&[0.0, 0.0, 3.0, 0.0, 3.0, 3.0, 0.0, 3.0, 0.0, 0.0]),
                new_line_string(&[1.0, 1.0, 2.0, 1.0, 2.0, 2.0, 1.0, 2.0, 1.0, 1.0]),
            ])),
            polygon_to_line_string(
                &new_polygon_with_holes(rectangle_coord_list(0.0, 3.0), vec![rectangle_coord_list(1.0, 2.0)]).into()
            )
            .unwrap()
        );
    }

    #[test]
    fn meters_per_pixel() {
        for (zoom, meters) in [(0, 156543.0), (8, 611.0), (14, 9.0)] {
            assert!((meters - meters_per_pixel_at_equator(zoom)).abs() <= 1.0);
        }
    }

    fn assert_convex(expected: bool, coords: &[f64]) {
        let ring = new_linear_ring(coords);
        for rotation in [0.0, 90.0, 180.0, 270.0] {
            let rotated = ring.affine_transform(&AffineTransform::rotate(rotation, Coord { x: 0.0, y: 0.0 }));
            for flip in [false, true] {
                let flipped = if flip {
                    rotated.affine_transform(&AffineTransform::scale(-1.0, 1.0, Coord { x: 0.0, y: 0.0 }))
                } else {
                    rotated.clone()
                };
                for reverse in [false, true] {
                    let mut reversed = flipped.clone();
                    if reverse {
                        reversed.0.reverse();
                    }
                    for scale in [1.0, 1e-2, 1.0 / 2f64.powi(14) / 4096.0] {
                        let scaled =
                            reversed.affine_transform(&AffineTransform::scale(scale, scale, Coord { x: 0.0, y: 0.0 }));
                        assert_eq!(
                            expected,
                            is_convex(&scaled),
                            "{coords:?} rotation={rotation} flip={flip} reverse={reverse} scale={scale}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn is_convex_tests() {
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]);
        // barely convex
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.5, 0.5, 0.0, 0.0]);
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.4, 0.4, 0.0, 0.0]);
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.7, 0.7, 0.0, 0.0]);
        // double points
        assert_convex(true, &[0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]);
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]);
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]);
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 0.0]);
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
        // barely concave
        assert_convex(false, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.51, 0.5, 0.0, 0.0]);
        // allow very small concavity
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.5001, 0.5, 0.0, 0.0]);
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.5, 0.4999, 0.0, 0.0]);
        // 5 points concave
        assert_convex(false, &[0.0, 0.0, 0.5, 0.1, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]);
        assert_convex(false, &[0.0, 0.0, 1.0, 0.0, 0.9, 0.5, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]);
        assert_convex(false, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.5, 0.9, 0.0, 1.0, 0.0, 0.0]);
        assert_convex(false, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.1, 0.5, 0.0, 0.0]);
        // 5 points colinear
        assert_convex(true, &[0.0, 0.0, 0.5, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]);
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 1.0, 0.5, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]);
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.5, 1.0, 0.0, 1.0, 0.0, 0.0]);
        assert_convex(true, &[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.5, 0.0, 0.0]);
    }

    #[test]
    fn combine_tests() {
        assert_eq!(super::empty_geometry(), combine(vec![]));
        let l = |a: f64| Geometry::LineString(new_line_string(&[a, a, a + 1.0, a + 1.0]));
        assert_eq!(l(0.0), combine(vec![l(0.0)]));
        assert_eq!(create_geometry_collection(vec![l(0.0), l(2.0)]), combine(vec![l(0.0), l(2.0)]));
        assert_eq!(
            create_geometry_collection(vec![l(0.0), l(2.0), l(4.0)]),
            combine(vec![combine(vec![l(0.0), l(2.0)]), l(4.0)])
        );
    }

    fn is_polygonal(g: &Geometry<f64>) -> bool {
        matches!(g, Geometry::Polygon(_) | Geometry::MultiPolygon(_))
    }

    /// JTS `Orientation.isCCWArea(result.getCoordinates())` for the concatenated coordinates.
    fn is_ccw_area(g: &Geometry<f64>) -> bool {
        use geo::CoordsIter;
        let coords: Vec<Coord<f64>> = g.coords_iter().collect();
        let mut area = 0.0;
        for i in 0..coords.len().saturating_sub(1) {
            area += coords[i].x * coords[i + 1].y - coords[i + 1].x * coords[i].y;
        }
        area > 0.0
    }

    #[test]
    fn snap_and_fix_issue_511() {
        let input = read_wkt("MULTIPOLYGON (((198.83750000000003 46.07500000000004, 199.0625 46.375, 199.4375 46.0625, 199.5 46.43750000000001, 199.5625 46, 199.3125 45.5, 198.8912037037037 46.101851851851876, 198.83750000000003 46.07500000000004)), ((198.43750000000003 46.49999999999999, 198.5625 46.43750000000001, 198.6875 46.25, 198.1875 46.25, 198.43750000000003 46.49999999999999)), ((198.6875 46.25, 198.81249999999997 46.062500000000014, 198.6875 46.00000000000002, 198.6875 46.25)), ((196.55199579831933 46.29359243697479, 196.52255639097743 46.941259398496236, 196.5225563909774 46.941259398496236, 196.49999999999997 47.43750000000001, 196.875 47.125, 197 47.5625, 197.47880544905414 46.97729334004497, 197.51505401161464 46.998359569801956, 197.25 47.6875, 198.0625 47.6875, 198.5 46.625, 198.34375 46.546875, 198.34375000000003 46.54687499999999, 197.875 46.3125, 197.875 46.25, 197.875 46.0625, 197.82894736842107 46.20065789473683, 197.25 46.56250000000001, 197.3125 46.125, 196.9375 46.1875, 196.9375 46.21527777777778, 196.73250000000002 46.26083333333334, 196.5625 46.0625, 196.55199579831933 46.29359243697479)), ((196.35213414634146 45.8170731707317, 197.3402027027027 45.93108108108108, 197.875 45.99278846153846, 197.875 45.93750000000002, 197.93749999999997 45.99999999999999, 197.9375 46, 197.90625 45.96874999999999, 197.90625 45.96875, 196.75000000000006 44.81250000000007, 197.1875 45.4375, 196.3125 45.8125, 196.35213414634146 45.8170731707317)), ((195.875 46.124999999999986, 195.8125 46.5625, 196.5 46.31250000000001, 195.9375 46.4375, 195.875 46.124999999999986)), ((196.49999999999997 46.93749999999999, 196.125 46.875, 196.3125 47.125, 196.49999999999997 46.93749999999999)))").unwrap();
        let result = snap_and_fix_polygon(&input, &InMemoryStats::new(), "test").unwrap();
        assert!(is_valid(&result));
        assert!(is_polygonal(&result));
        // JTS's precision reducer fails on this input and falls back to buffer/unbuffer, giving 3.06640625. The
        // integer overlay re-noding succeeds directly (3.15625; the unsnapped input area is 3.1107), so only check
        // that the result is close.
        assert!((3.06640625 - area(&result)).abs() < 0.1, "{}", area(&result));
    }

    #[test]
    fn snap_and_fix_issue_546() {
        let orig = read_wkt("POLYGON((0 0, 2 0, 2 1, 0 1, 0 0), (1.19053596444428 0.9029693332377, 1.06439281777784 0.92184484124482, 1.42787640888855 0.8257737683016, 0.94579086222257 0.5617504680322, 1.19053596444428 0.9029693332377))").unwrap();
        let result = snap_and_fix_polygon(&orig, &InMemoryStats::new(), "test").unwrap();
        assert!(is_polygonal(&result));
        assert!(is_valid(&result));
        assert!(!result.contains(&new_point(1.14602002962965, 0.76978969252622)));
    }

    #[test]
    fn snap_and_fix_issue_546_2() {
        let orig = read_wkt("POLYGON((1.19053596444428 0.9029693332377, 1.06439281777784 0.92184484124482, 1.42787640888855 0.8257737683016, 0.94579086222257 0.5617504680322, 1.19053596444428 0.9029693332377))").unwrap();
        let result = snap_and_fix_polygon(&orig, &InMemoryStats::new(), "test").unwrap();
        assert!(is_polygonal(&result));
        assert!(is_valid(&result));
        assert!(!is_ccw_area(&result));
    }

    #[test]
    fn snap_and_fix_remains_polygon() {
        for wkt in [
            "POLYGON ((178.80078125 -2.232421875, 177.3359375 -2.48828125, 177.474609375 -3.828125, 177.6048093077957 -4, 181.6829622310952 -4, 181.51171875 -3.9296875, 179.58984375 -3.44140625, 178.80078125 -2.232421875), (178.9876130756579 -4, 178.830078125 -3.990234375, 178.8322088068182 -4, 178.9876130756579 -4))",
            "MULTIPOLYGON (((40.53515625 259.98046875, 40.55303296657843 260, 40.5148361773021 260, 40.53515625 259.98046875)), ((38.84765625 254.9375, 36.3359375 252.28125, 39.53515625 248.98046875, 43.9375 252.98046875, 41.546875 255.23046875, 40.11328125 256.28125, 38.84765625 254.9375)))",
        ] {
            let orig = read_wkt(wkt).unwrap();
            let result = snap_and_fix_polygon(&orig, &InMemoryStats::new(), "test").unwrap();
            assert!(is_polygonal(&result), "{result:?}");
            assert!(is_valid(&result));
            assert!(!is_ccw_area(&result));
        }
    }

    #[test]
    fn snap_and_fix_valid_rectangle_unchanged() {
        let stats = InMemoryStats::new();
        let result = snap_and_fix_polygon(&rectangle(0.01, 10.02).into(), &stats, "test").unwrap();
        // shell oriented clockwise
        assert_eq!(Geometry::Polygon(new_polygon(&[0.0, 0.0, 0.0, 10.0, 10.0, 10.0, 10.0, 0.0, 0.0, 0.0])), result);
        assert!(stats.data_errors().is_empty());
    }

    #[test]
    fn min_zoom_for_pixel_size_cases() {
        for (size, px, expected) in [
            (1.0, 0.0, 0),
            (1.0, 10.0, 0),
            (1.0, 255.0, 0),
            (0.5, 0.0, 0),
            (0.5, 128.0, 0),
            (0.5, 129.0, 1),
            (0.5, 256.0, 1),
            (0.25, 0.0, 0),
            (0.25, 128.0, 1),
            (0.25, 129.0, 2),
            (0.25, 256.0, 2),
        ] {
            assert_eq!(expected, min_zoom_for_pixel_size(size, px), "{size} {px}");
        }
        assert_eq!(10, min_zoom_for_pixel_size(3.1 / (256u64 << 10) as f64, 3.0));
        assert_eq!(9, min_zoom_for_pixel_size(6.1 / (256u64 << 10) as f64, 3.0));
    }

    #[test]
    fn longest_line() {
        let line1 = new_line_string(&[0.0, 0.0, 1.0, 1.0]);
        let line2 = new_line_string(&[0.0, 0.0, 2.0, 2.0]);
        assert_eq!(None, get_longest_line(&new_multi_line_string(vec![])));
        assert_eq!(Some(&line1), get_longest_line(&new_multi_line_string(vec![line1.clone()])));
        assert_eq!(Some(&line2), get_longest_line(&new_multi_line_string(vec![line1, line2.clone()])));
    }

    #[test]
    fn area_in_meters_test() {
        assert_eq!(0.0, area_in_meters(&new_line_string(&[0.0, 0.0, 1.0, 1.0]).into()));
        assert_eq!(0.0, area_in_meters(&new_point(0.0, 1.0).into()));
        assert!((111_194.0 - area_in_meters(&rectangle(-0.5, 0.5).into()).sqrt()).abs() < 1.0);
        let with_hole: Geometry<f64> =
            new_polygon_with_holes(rectangle_coord_list(0.0, 3.0), vec![rectangle_coord_list(1.0, 2.0)]).into();
        assert!(
            (area_in_meters(&rectangle(0.0, 3.0).into())
                - area_in_meters(&rectangle(1.0, 2.0).into())
                - area_in_meters(&with_hole))
            .abs()
                < 1.0
        );
        assert!((96_963.0 - area_in_meters(&rectangle(40.0, 41.0).into()).sqrt()).abs() < 1.0);
        assert!((10_387.0 - area_in_meters(&rectangle(89.0, 90.0).into()).sqrt()).abs() < 1.0);
        assert!((10_387.0 - area_in_meters(&rectangle(-89.0, -90.0).into()).sqrt()).abs() < 1.0);
        let world: Geometry<f64> =
            new_polygon(&[-180.0, -90.0, 180.0, -90.0, 180.0, 90.0, -180.0, 90.0, -180.0, -90.0]).into();
        assert!((5.1e14 - area_in_meters(&world)).abs() < 1e11);
    }

    #[test]
    fn length_in_meters_test() {
        assert_eq!(0.0, length_in_meters(&rectangle(0.0, 1.0).into()));
        assert_eq!(0.0, length_in_meters(&new_point(0.0, 0.0).into()));
        let l1: Geometry<f64> = new_line_string(&[-75.343, 39.984, -75.534, 39.123]).into();
        assert!((97_129.0 - length_in_meters(&l1)).abs() < 1.0);
        let l3: Geometry<f64> =
            new_line_string(&[-75.343, 39.984, -75.534, 39.123, -75.343, 39.984, -75.534, 39.123]).into();
        assert!((97_129.0 * 3.0 - length_in_meters(&l3)).abs() < 1.0);
        let l: Geometry<f64> = new_line_string(&[47.234, 24.235, 47.234 + 0.1, 24.235 - 0.1]).into();
        assert!((15051.0 - length_in_meters(&l)).abs() < 1.0);
    }

    #[test]
    fn envelope_to_string_test() {
        let mut env = Envelope::null();
        assert_eq!("Envelope(0.0,0.0,-1.0,-1.0)", envelope_to_string(&env));
        env.expand_to_include(0.0, 1.0);
        env.expand_to_include(2.0, 3.0);
        assert_eq!("Envelope(0.0,1.0,2.0,3.0)", envelope_to_string(&env));
    }

    #[test]
    fn label_grid_id_test() {
        assert_eq!(0, label_grid_id(4, 1.0, Coord { x: 0.5, y: 0.5 }));
        assert_eq!((1i64 << 32) | 2, label_grid_id(4, 1.0, Coord { x: 1.5, y: 2.5 }));
        // wraps x
        assert_eq!((1i64 << 32) | 2, label_grid_id(4, 1.0, Coord { x: 5.5, y: 2.5 }));
        assert_eq!((3i64 << 32) | 2, label_grid_id(4, 1.0, Coord { x: -0.5, y: 2.5 }));
    }

    #[test]
    fn wkb_round_trip() {
        let geom: Geometry<f64> =
            new_polygon_with_holes(rectangle_coord_list(0.0, 3.0), vec![rectangle_coord_list(1.0, 2.0)]).into();
        assert_eq!(geom, read_wkb(&write_wkb(&geom).unwrap()).unwrap());
    }

    #[test]
    fn sort_polygons_by_area() {
        let mp: Geometry<f64> =
            new_multi_polygon(vec![rectangle(0.0, 1.0), rectangle(2.0, 5.0), rectangle(6.0, 8.0)]).into();
        assert_eq!(
            Geometry::MultiPolygon(new_multi_polygon(vec![
                rectangle(2.0, 5.0),
                rectangle(6.0, 8.0),
                rectangle(0.0, 1.0)
            ])),
            sort_polygons_by_area_descending(mp)
        );
    }

    #[test]
    fn point_along_offset_test() {
        let line = new_line_string(&[0.0, 0.0, 10.0, 0.0]);
        assert_eq!(new_point(5.0, 0.0), point_along_offset(&line, 0.5, 0.0));
        assert_eq!(new_point(5.0, 1.0), point_along_offset(&line, 0.5, 1.0));
    }
}
