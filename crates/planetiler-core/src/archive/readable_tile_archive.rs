//! Port of `archive/ReadableTileArchive.java`.
//!
//! Read API for on-disk representation of a tileset in a portable format. Example: MBTiles, a sqlite-based archive
//! format. See [`WriteableTileArchive`](crate::archive::writeable_tile_archive::WriteableTileArchive) for the write
//! API.

use crate::archive::tile::Tile;
use crate::archive::tile_archive_metadata::TileArchiveMetadata;
use crate::geo::tile_coord::TileCoord;

/// Iterator over items read from an archive that may fail.
pub type ArchiveIter<'a, T> = Box<dyn Iterator<Item = anyhow::Result<T>> + 'a>;

pub trait ReadableTileArchive: Send + Sync {
    /// Returns the raw tile data at `x, y, z` or `None` if not found.
    fn get_tile(&self, x: u32, y: u32, z: u32) -> anyhow::Result<Option<Vec<u8>>>;

    /// Returns the raw tile data at `coord` or `None` if not found.
    fn get_tile_coord(&self, coord: &TileCoord) -> anyhow::Result<Option<Vec<u8>>> {
        self.get_tile(coord.x, coord.y, coord.z)
    }

    /// Returns an iterator over the coordinates of tiles in this archive.
    ///
    /// The order should respect the `tile_order()` of the corresponding writer.
    fn get_all_tile_coords(&self) -> anyhow::Result<ArchiveIter<'_, TileCoord>>;

    /// Returns an iterator over all tiles in this archive.
    fn get_all_tiles(&self) -> anyhow::Result<ArchiveIter<'_, Tile>> {
        let coords = self.get_all_tile_coords()?;
        Ok(Box::new(coords.map(move |coord| {
            let coord = coord?;
            let bytes = self.get_tile_coord(&coord)?.unwrap_or_default();
            Ok(Tile::new(coord, bytes))
        })))
    }

    /// Returns the metadata stored in this archive, or `None` if there is none.
    fn metadata(&self) -> anyhow::Result<Option<TileArchiveMetadata>>;
}
