//! Port of `geo/TileOrder.java`.
//!
//! Controls the sort order of `FeatureGroup`, which determines the ordering of
//! [`TileEncodingResult`](crate::archive::tile_encoding_result::TileEncodingResult)s when written to a
//! [`TileWriter`](crate::archive::writeable_tile_archive::TileWriter).

use std::fmt;

use crate::geo::tile_coord::TileCoord;
use crate::geo::tile_extents::TileExtents;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TileOrder {
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
}

impl fmt::Display for TileOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TileOrder::Tms => "TMS",
            TileOrder::Hilbert => "HILBERT",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for order in [TileOrder::Tms, TileOrder::Hilbert] {
            for i in 0..1000 {
                assert_eq!(i, order.encode(&order.decode(i)));
            }
        }
    }
}
