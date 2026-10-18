//! Port of `geo/TileOrder.java`.

use crate::geo::tile_coord::TileCoord;
use crate::geo::tile_extents::TileExtents;

/// Controls the sort order of tiles in the feature group, which determines the order tiles are written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TileOrder {
    #[default]
    Tms,
    Hilbert,
}

impl TileOrder {
    pub fn encode(&self, coord: &TileCoord) -> u64 {
        match self {
            TileOrder::Tms => coord.encoded,
            TileOrder::Hilbert => coord.hilbert_encoded(),
        }
    }

    pub fn decode(&self, encoded: u64) -> TileCoord {
        match self {
            TileOrder::Tms => TileCoord::decode(encoded),
            TileOrder::Hilbert => TileCoord::hilbert_decode(encoded),
        }
    }

    pub fn progress_on_level(&self, coord: &TileCoord, extents: &TileExtents) -> f64 {
        match self {
            TileOrder::Tms => coord.progress_on_level(extents),
            TileOrder::Hilbert => coord.hilbert_progress_on_level(),
        }
    }

    /// Parse `tms` or `hilbert` (case-insensitive).
    pub fn from_name(name: &str) -> Option<TileOrder> {
        match name.to_ascii_lowercase().as_str() {
            "tms" => Some(TileOrder::Tms),
            "hilbert" => Some(TileOrder::Hilbert),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for order in [TileOrder::Tms, TileOrder::Hilbert] {
            let coord = TileCoord::of_xyz(3, 5, 4);
            let decoded = order.decode(order.encode(&coord));
            assert_eq!((coord.x, coord.y, coord.z), (decoded.x, decoded.y, decoded.z));
        }
    }
}
