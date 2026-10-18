//! Port of `archive/WriteableTileArchive.java`.
//!
//! Write API for an on-disk representation of a tileset in a portable format. Example: MBTiles, a sqlite-based archive
//! format. See [`ReadableTileArchive`](crate::archive::readable_tile_archive::ReadableTileArchive) for the read API.
//!
//! Lifecycle (mirrors Java):
//! 1. [`WriteableTileArchive::initialize`] once,
//! 2. one or more [`WriteableTileArchive::new_tile_writer`] (one per tile-writer thread when the format supports
//!    concurrent writes), [`TileWriter::write`] each tile, then [`TileWriter::close`] each writer,
//! 3. [`WriteableTileArchive::finish`] with the final metadata,
//! 4. [`WriteableTileArchive::close`] (or drop).
//!
//! Archives are `Send + Sync` so that a progress logger can call [`WriteableTileArchive::bytes_written`] while tile
//! writers (which borrow the archive) write from other threads.

use crate::archive::tile_archive_metadata::TileArchiveMetadata;
use crate::archive::tile_encoding_result::TileEncodingResult;
use crate::geo::tile_order::TileOrder;

pub trait WriteableTileArchive: Send + Sync {
    /// Returns true if this tile archive deduplicates tiles with the same content.
    ///
    /// If false, then the [`TileWriter`] caller can skip computing tile hashes.
    fn deduplicates(&self) -> bool;

    /// The preferred insertion order for this archive, e.g. [`TileOrder::Tms`] or [`TileOrder::Hilbert`].
    fn tile_order(&self) -> TileOrder;

    /// Called before any tiles are written into a [`TileWriter`]. Implementations should set up any required state
    /// here.
    fn initialize(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Returns a new [`TileWriter`] that writes into this archive.
    fn new_tile_writer(&self) -> anyhow::Result<Box<dyn TileWriter + Send + '_>>;

    /// Called after all tiles are written into all [`TileWriter`]s. After this is called, the archive should be
    /// complete on disk.
    fn finish(&mut self, _metadata: &TileArchiveMetadata) -> anyhow::Result<()> {
        Ok(())
    }

    /// Number of bytes written so far.
    fn bytes_written(&self) -> u64;

    /// Release any resources (Java `close()`). Dropping the archive also releases resources, but this reports errors.
    fn close(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Writes tiles into a [`WriteableTileArchive`].
pub trait TileWriter {
    fn write(&mut self, encoding_result: TileEncodingResult) -> anyhow::Result<()>;

    /// Flushes any pending writes. Must be called once after the last [`TileWriter::write`].
    fn close(&mut self) -> anyhow::Result<()>;

    fn print_stats(&self) {}
}
