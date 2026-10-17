//! Port of `geo/GeoUtils.java`.
//!
//! "World" coordinates are web mercator scaled to `0..1` with `(0, 0)` in the top-left (north-west) corner.

use std::sync::LazyLock;

use geo::MapCoords;
use geo_types::{Coord, Geometry};

use crate::geo::envelope::Envelope;

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

/// Format an envelope as `minX,minY,maxX,maxY` for logging.
pub fn envelope_to_string(envelope: &Envelope) -> String {
    format!("{:.5},{:.5},{:.5},{:.5}", envelope.min_x, envelope.min_y, envelope.max_x, envelope.max_y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

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
}
