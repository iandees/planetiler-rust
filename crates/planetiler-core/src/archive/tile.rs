//! Port of `archive/Tile.java`.

use std::cmp::Ordering;
use std::fmt;

use crate::geo::tile_coord::TileCoord;

/// A tile stored in an archive with coordinate `coord` and archived `bytes`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Tile {
    pub coord: TileCoord,
    pub bytes: Vec<u8>,
}

impl Tile {
    pub fn new(coord: TileCoord, bytes: Vec<u8>) -> Tile {
        Tile { coord, bytes }
    }
}

impl fmt::Debug for Tile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tile{{coord={}, data=byte[{}]}}", self.coord, self.bytes.len())
    }
}

impl PartialOrd for Tile {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Orders by coordinate (then bytes, to stay consistent with `Eq`).
impl Ord for Tile {
    fn cmp(&self, other: &Self) -> Ordering {
        self.coord.cmp(&other.coord).then_with(|| self.bytes.cmp(&other.bytes))
    }
}
