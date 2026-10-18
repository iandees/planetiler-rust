//! Port of `archive/TileEncodingResult.java`.

use crate::geo::tile_coord::TileCoord;

/// An encoded tile ready to be written to a [`TileWriter`](crate::archive::writeable_tile_archive::TileWriter).
#[derive(Clone, Debug)]
pub struct TileEncodingResult {
    pub coord: TileCoord,
    pub tile_data: Vec<u8>,
    pub raw_tile_size: usize,
    /// Will always be empty in non-compact mode and might also be empty in compact mode.
    pub tile_data_hash: Option<u64>,
    pub layer_stats: Vec<String>,
}

impl TileEncodingResult {
    pub fn new(coord: TileCoord, tile_data: Vec<u8>, tile_data_hash: Option<u64>) -> TileEncodingResult {
        let raw_tile_size = tile_data.len();
        TileEncodingResult { coord, tile_data, raw_tile_size, tile_data_hash, layer_stats: Vec::new() }
    }

    pub fn with_stats(
        coord: TileCoord,
        tile_data: Vec<u8>,
        raw_tile_size: usize,
        tile_data_hash: Option<u64>,
        layer_stats: Vec<String>,
    ) -> TileEncodingResult {
        TileEncodingResult { coord, tile_data, raw_tile_size, tile_data_hash, layer_stats }
    }
}

/// Equality ignores `raw_tile_size` and `layer_stats` like the Java implementation.
impl PartialEq for TileEncodingResult {
    fn eq(&self, other: &Self) -> bool {
        self.coord == other.coord && self.tile_data == other.tile_data && self.tile_data_hash == other.tile_data_hash
    }
}

impl Eq for TileEncodingResult {}
