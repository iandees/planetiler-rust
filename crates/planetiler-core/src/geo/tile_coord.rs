//! Port of `geo/TileCoord.java`.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;

use geo_types::Coord;

use crate::config::planetiler_config::MAX_MAXZOOM;
use crate::geo::envelope::Envelope;
use crate::geo::geo_utils;
use crate::geo::tile_extents::TileExtents;
use crate::util::hilbert;

static ZOOM_START_INDEX: LazyLock<[u64; MAX_MAXZOOM as usize + 1]> = LazyLock::new(|| {
    let mut result = [0u64; MAX_MAXZOOM as usize + 1];
    let mut idx = 0u64;
    for (z, slot) in result.iter_mut().enumerate() {
        *slot = idx;
        idx += 1u64 << (2 * z);
    }
    result
});

fn start_index_for_zoom(z: u32) -> u64 {
    ZOOM_START_INDEX[z as usize]
}

fn zoom_for_index(idx: u64) -> u32 {
    for z in (0..=MAX_MAXZOOM).rev() {
        if ZOOM_START_INDEX[z as usize] <= idx {
            return z;
        }
    }
    panic!("Bad index: {idx}")
}

/// The coordinate of a slippy map tile.
///
/// `encoded` orders tiles by zoom, then x, then y in TMS order (y flipped) so that tiles are emitted in the order that
/// `mbtiles` and `pmtiles` clustered writes prefer.
#[derive(Clone, Copy, Debug)]
pub struct TileCoord {
    pub encoded: u64,
    pub x: u32,
    pub y: u32,
    pub z: u32,
}

impl TileCoord {
    pub fn of_xyz(x: u32, y: u32, z: u32) -> TileCoord {
        debug_assert!(z <= MAX_MAXZOOM);
        TileCoord { encoded: Self::encode(x, y, z), x, y, z }
    }

    pub fn decode(encoded: u64) -> TileCoord {
        let z = zoom_for_index(encoded);
        let xy = Self::tms_position_to_xy(z, encoded - start_index_for_zoom(z));
        TileCoord { encoded, x: (xy >> 32) as u32, y: xy as u32, z }
    }

    pub fn hilbert_decode(encoded: u64) -> TileCoord {
        let z = zoom_for_index(encoded);
        let xy = hilbert::hilbert_position_to_xy(z, encoded - start_index_for_zoom(z));
        TileCoord::of_xyz(hilbert::extract_x(xy), hilbert::extract_y(xy), z)
    }

    /// The tile containing a longitude/latitude at `zoom`.
    pub fn around_lng_lat(lng: f64, lat: f64, zoom: u32) -> TileCoord {
        let factor = (1u64 << zoom) as f64;
        let x = geo_utils::get_world_x(lng) * factor;
        let y = geo_utils::get_world_y(lat) * factor;
        TileCoord::of_xyz(x.floor() as u32, y.floor() as u32, zoom)
    }

    pub fn encode(x: u32, y: u32, z: u32) -> u64 {
        start_index_for_zoom(z).wrapping_add(Self::tms_xy_to_position(z, x, y))
    }

    /// Approximate fraction complete within a zoom level, using columns in `extents`.
    pub fn progress_on_level(&self, extents: &TileExtents) -> f64 {
        let zoom_bounds = extents.get_for_zoom(self.z);
        (self.x as f64 - zoom_bounds.min_x as f64) / (zoom_bounds.max_x as f64 - zoom_bounds.min_x as f64)
    }

    pub fn hilbert_progress_on_level(&self) -> f64 {
        hilbert::hilbert_xy_to_index(self.z, self.x, self.y) as f64 / (1u64 << (2 * self.z)) as f64
    }

    /// Latitude/longitude bounds of this tile (`min_y` is the southern edge).
    pub fn get_envelope(&self) -> Envelope {
        let world_width_at_zoom = 2f64.powi(self.z as i32);
        Envelope::new(
            geo_utils::get_world_lon(self.x as f64 / world_width_at_zoom),
            geo_utils::get_world_lon((self.x as f64 + 1.0) / world_width_at_zoom),
            geo_utils::get_world_lat((self.y as f64 + 1.0) / world_width_at_zoom),
            geo_utils::get_world_lat(self.y as f64 / world_width_at_zoom),
        )
    }

    /// Same as [`TileCoord::get_envelope`] (Java's `bounds()`; envelopes normalize min/max so they are identical).
    pub fn bounds(&self) -> Envelope {
        self.get_envelope()
    }

    /// Replace `{lat}`, `{lon}`, and `{z}` placeholders in a debug URL with the center of this tile.
    pub fn get_debug_url(&self, pattern: &str) -> String {
        let center = self.get_envelope().centre().expect("tile envelope is never null");
        let fmt5 = |v: f64| {
            let s = format!("{v:.5}");
            let s = s.trim_end_matches('0').trim_end_matches('.');
            if s == "-0" { "0".to_string() } else { s.to_string() }
        };
        pattern
            .replace("{latitude}", &fmt5(center.y))
            .replace("{lat}", &fmt5(center.y))
            .replace("{longitude}", &fmt5(center.x))
            .replace("{lon}", &fmt5(center.x))
            .replace("{zoom}", &format!("{}.5", self.z))
            .replace("{z}", &format!("{}.5", self.z))
    }

    /// Pixel coordinates (0-256) of a longitude/latitude within this tile.
    pub fn lng_lat_to_tile_coords(&self, lng: f64, lat: f64) -> Coord<f64> {
        let factor = (1u64 << self.z) as f64;
        let x = geo_utils::get_world_x(lng) * factor;
        let y = geo_utils::get_world_y(lat) * factor;
        Coord { x: (x - x.floor()) * 256.0, y: (y - y.floor()) * 256.0 }
    }

    pub fn hilbert_encoded(&self) -> u64 {
        start_index_for_zoom(self.z) + hilbert::hilbert_xy_to_index(self.z, self.x, self.y)
    }

    pub fn tms_position_to_xy(z: u32, pos: u64) -> u64 {
        if z == 0 {
            return 0;
        }
        let dim = 1u64 << z;
        let x = pos / dim;
        let y = dim - 1 - (pos % dim);
        (x << 32) | y
    }

    pub fn tms_xy_to_position(z: u32, x: u32, y: u32) -> u64 {
        let dim = 1u64 << z;
        // wrapping like Java's long arithmetic so out-of-range coordinates (used by some archive tests) don't panic
        (x as u64).wrapping_mul(dim).wrapping_add((dim - 1).wrapping_sub(y as u64))
    }

    pub fn parent(&self) -> TileCoord {
        TileCoord::of_xyz(self.x / 2, self.y / 2, self.z - 1)
    }
}

impl PartialEq for TileCoord {
    fn eq(&self, other: &Self) -> bool {
        self.encoded == other.encoded
    }
}

impl Eq for TileCoord {}

impl Hash for TileCoord {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.encoded.hash(state);
    }
}

impl PartialOrd for TileCoord {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TileCoord {
    fn cmp(&self, other: &Self) -> Ordering {
        self.encoded.cmp(&other.encoded)
    }
}

impl fmt::Display for TileCoord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{x={} y={} z={}}}", self.x, self.y, self.z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode() {
        for z in 0..=MAX_MAXZOOM {
            let max = (1u32 << z) - 1;
            for (x, y) in [(0, 0), (max, 0), (0, max), (max, max), (max / 2, max / 3)] {
                let c = TileCoord::of_xyz(x, y, z);
                let d = TileCoord::decode(c.encoded);
                assert_eq!((x, y, z), (d.x, d.y, d.z));
                let h = TileCoord::hilbert_decode(c.hilbert_encoded());
                assert_eq!((x, y, z), (h.x, h.y, h.z));
            }
        }
    }

    #[test]
    fn ordering() {
        // tms order: z, then x, then y descending
        let a = TileCoord::of_xyz(0, 1, 1);
        let b = TileCoord::of_xyz(0, 0, 1);
        let c = TileCoord::of_xyz(1, 1, 1);
        assert!(TileCoord::of_xyz(0, 0, 0) < a);
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn around_lng_lat() {
        let t = TileCoord::around_lng_lat(0.1, 0.1, 1);
        assert_eq!((1, 0, 1), (t.x, t.y, t.z));
        let t = TileCoord::around_lng_lat(-179.0, -80.0, 2);
        assert_eq!((0, 3, 2), (t.x, t.y, t.z));
    }

    #[test]
    fn debug_url() {
        assert_eq!("0.5/0/0", TileCoord::of_xyz(0, 0, 0).get_debug_url("{z}/{lat}/{lon}"));
    }
}
