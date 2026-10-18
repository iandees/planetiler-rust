//! Port of `pmtiles/WriteablePmtiles.java`.
//!
//! PMTiles is a single-file tile archive format designed for efficient access on cloud storage. Tile data is written
//! after a 16KB initial section reserved for the header and root directory; tiles with identical hashes are
//! deduplicated, consecutive identical tiles are run-length encoded, and leaf directories are created when the root
//! directory would not fit in the initial section.
//!
//! See the [PMTiles Specification](https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md).

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::{self, BufWriter, Cursor, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tracing::{error, info};

use crate::archive::tile_archive_metadata::{self as md, TileArchiveMetadata};
use crate::archive::tile_compression::TileCompression;
use crate::archive::tile_encoding_result::TileEncodingResult;
use crate::archive::tile_format::TileFormat;
use crate::archive::writeable_tile_archive::{TileWriter, WriteableTileArchive};
use crate::config::planetiler_config::MAX_MAXZOOM;
use crate::geo::geo_utils;
use crate::geo::tile_order::TileOrder;
use crate::pmtiles::pmtiles::{self, Compression, Entry, HEADER_LEN, Header, JsonMetadata, TileType, gzip};

pub const INIT_SECTION: usize = 16384;

/// Output sink for a PMTiles archive.
pub trait PmtilesOutput: Write + Seek + Send {}

impl<T: Write + Seek + Send> PmtilesOutput for T {}

/// An in-memory seekable byte buffer that can be shared between a writer and a reader (Java
/// `SeekableInMemoryByteChannel`).
#[derive(Clone, Default)]
pub struct SeekableInMemoryByteChannel {
    inner: Arc<Mutex<Cursor<Vec<u8>>>>,
}

impl SeekableInMemoryByteChannel {
    pub fn new() -> SeekableInMemoryByteChannel {
        Self::default()
    }

    /// A copy of the current contents.
    pub fn bytes(&self) -> Vec<u8> {
        self.inner.lock().get_ref().clone()
    }

    pub fn read_at(&self, offset: u64, length: usize) -> Vec<u8> {
        let guard = self.inner.lock();
        let data = guard.get_ref();
        let mut out = vec![0u8; length];
        let start = (offset as usize).min(data.len());
        let end = (start + length).min(data.len());
        out[..end - start].copy_from_slice(&data[start..end]);
        out
    }
}

impl Write for SeekableInMemoryByteChannel {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.lock().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for SeekableInMemoryByteChannel {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.lock().seek(pos)
    }
}

/// Serialized root and leaf directories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directories {
    pub root: Vec<u8>,
    pub leaves: Vec<u8>,
    pub num_leaves: usize,
    pub leaf_size: usize,
    pub num_attempts: usize,
}

fn make_directories_with_leaves(
    sub_entries: &[Entry],
    leaf_size: usize,
    attempt_num: usize,
) -> io::Result<Directories> {
    info!("Building directories with {leaf_size} entries per leaf, attempt {attempt_num}...");
    let mut root_entries = Vec::new();
    let mut leaves = Vec::new();
    let mut num_leaves = 0;
    for chunk in sub_entries.chunks(leaf_size) {
        num_leaves += 1;
        let leaf_bytes = gzip(&pmtiles::directory_to_bytes(chunk))?;
        root_entries.push(Entry::new(chunk[0].tile_id, leaves.len() as u64, leaf_bytes.len() as u32, 0));
        leaves.extend_from_slice(&leaf_bytes);
    }
    let root = gzip(&pmtiles::directory_to_bytes(&root_entries))?;
    info!("Built directories with {} leaves, {}B root directory", root_entries.len(), root.len());
    Ok(Directories { root, leaves, num_leaves, leaf_size, num_attempts: attempt_num })
}

/// Serialize all entries into bytes, choosing the # of leaf directories to ensure the header+root fits in 16 KB.
pub fn make_directories(entries: &[Entry]) -> io::Result<Directories> {
    let max_entries_root_only = 16384;
    let mut attempt_num = 1;
    if entries.len() < max_entries_root_only {
        let test_bytes = gzip(&pmtiles::directory_to_bytes(entries))?;
        if test_bytes.len() < INIT_SECTION - HEADER_LEN {
            return Ok(Directories { root: test_bytes, leaves: vec![], num_leaves: 0, leaf_size: 0, num_attempts: 1 });
        }
    }
    let estimated_leaf_size = entries.len() as f64 / 3_500.0;
    let mut leaf_size = estimated_leaf_size.max(4096.0) as usize;
    loop {
        let temp = make_directories_with_leaves(entries, leaf_size, attempt_num)?;
        attempt_num += 1;
        if temp.root.len() < INIT_SECTION - HEADER_LEN {
            return Ok(temp);
        }
        leaf_size = (leaf_size as f64 * 1.2) as usize;
    }
}

struct State {
    out: Box<dyn PmtilesOutput>,
    hash_to_offset: HashMap<u64, u64>,
    entries: Vec<Entry>,
    current_offset: u64,
    num_unhashed_tiles: u64,
    num_addressed_tiles: u64,
    is_clustered: bool,
}

pub struct WriteablePmtiles {
    state: Mutex<State>,
    path: Option<PathBuf>,
}

impl WriteablePmtiles {
    fn new(mut out: Box<dyn PmtilesOutput>, path: Option<PathBuf>) -> io::Result<WriteablePmtiles> {
        out.write_all(&[0u8; INIT_SECTION])?;
        Ok(WriteablePmtiles {
            state: Mutex::new(State {
                out,
                hash_to_offset: HashMap::new(),
                entries: Vec::new(),
                current_offset: 0,
                num_unhashed_tiles: 0,
                num_addressed_tiles: 0,
                is_clustered: true,
            }),
            path,
        })
    }

    pub fn new_write_to_file(path: &Path) -> anyhow::Result<WriteablePmtiles> {
        let file = OpenOptions::new().create(true).truncate(true).read(true).write(true).open(path)?;
        Ok(Self::new(Box::new(BufWriter::with_capacity(1 << 20, file)), Some(path.to_path_buf()))?)
    }

    pub fn new_write_to_memory(bytes: SeekableInMemoryByteChannel) -> anyhow::Result<WriteablePmtiles> {
        Ok(Self::new(Box::new(bytes), None)?)
    }

    fn do_finish(&self, metadata: &TileArchiveMetadata) -> io::Result<()> {
        let mut guard = self.state.lock();
        let state = &mut *guard;
        if !state.is_clustered {
            info!("Tile data was not written in order, sorting entries...");
            state.entries.sort();
            info!("Done sorting.");
        }
        let directories = make_directories(&state.entries)?;
        // use a sorted map to ensure consistent output between runs
        let mut other_metadata: BTreeMap<String, String> = metadata.to_map().into_iter().collect();
        // exclude keys included in top-level header
        for key in [
            md::CENTER_KEY,
            md::ZOOM_KEY,
            md::BOUNDS_KEY,
            md::FORMAT_KEY,
            md::MINZOOM_KEY,
            md::MAXZOOM_KEY,
            md::VECTOR_LAYERS_KEY,
            md::COMPRESSION_KEY,
        ] {
            other_metadata.remove(key);
        }
        let json_bytes = gzip(&JsonMetadata::new(metadata.vector_layers().cloned(), other_metadata).to_bytes())?;

        let output_format = match metadata.format {
            Some(TileFormat::Mlt) => TileType::Mlt,
            Some(TileFormat::Mvt) => TileType::Mvt,
            _ => TileType::Unknown,
        };
        let bounds = metadata.bounds.unwrap_or(*geo_utils::WORLD_LAT_LON_BOUNDS);
        let center = metadata.center.unwrap_or_else(|| {
            let c = bounds.centre().unwrap_or_default();
            md::Coordinate::xy(c.x, c.y)
        });
        let zoom = metadata.zoom().unwrap_or_else(|| geo_utils::get_zoom_from_lon_lat_bounds(&bounds)).ceil() as i32;
        let minzoom = metadata.minzoom.unwrap_or(0);
        let maxzoom = metadata.maxzoom.unwrap_or(MAX_MAXZOOM as i32);
        let tile_compression = match metadata.tile_compression {
            Some(TileCompression::Gzip) => Compression::Gzip,
            Some(TileCompression::None) => Compression::None,
            _ => Compression::Unknown,
        };
        let e7 = |v: f64| (v * 10_000_000.0) as i32;
        let num_tile_contents = state.hash_to_offset.len() as u64 + state.num_unhashed_tiles;
        let header = Header {
            spec_version: 3,
            root_dir_offset: HEADER_LEN as u64,
            root_dir_length: directories.root.len() as u64,
            json_metadata_offset: INIT_SECTION as u64 + state.current_offset,
            json_metadata_length: json_bytes.len() as u64,
            leaf_directories_offset: INIT_SECTION as u64 + state.current_offset + json_bytes.len() as u64,
            leaf_directories_length: directories.leaves.len() as u64,
            tile_data_offset: INIT_SECTION as u64,
            tile_data_length: state.current_offset,
            num_addressed_tiles: state.num_addressed_tiles,
            num_tile_entries: state.entries.len() as u64,
            num_tile_contents,
            clustered: state.is_clustered,
            internal_compression: Compression::Gzip,
            tile_compression,
            tile_type: output_format,
            min_zoom: minzoom as u8,
            max_zoom: maxzoom as u8,
            min_lon_e7: e7(bounds.min_x),
            min_lat_e7: e7(bounds.min_y),
            max_lon_e7: e7(bounds.max_x),
            max_lat_e7: e7(bounds.max_y),
            center_zoom: zoom as u8,
            center_lon_e7: e7(center.x),
            center_lat_e7: e7(center.y),
        };

        info!("Writing metadata and leaf directories...");
        state.out.write_all(&json_bytes)?;
        state.out.write_all(&directories.leaves)?;

        info!("Writing header...");
        state.out.seek(SeekFrom::Start(0))?;
        state.out.write_all(&header.to_bytes())?;
        state.out.write_all(&directories.root)?;
        state.out.flush()?;

        info!("# addressed tiles: {}", state.num_addressed_tiles);
        info!("# of tile entries: {}", state.entries.len());
        info!("# of tile contents: {num_tile_contents}");
        info!("Root directory: {}B", directories.root.len());
        info!("# leaves: {}", directories.num_leaves);
        if let Some(avg) = directories.leaves.len().checked_div(directories.num_leaves) {
            info!("Leaf directories: {}B", directories.leaves.len());
            info!("Avg leaf size: {avg}B");
        }
        let total = directories.root.len() + directories.leaves.len();
        info!("Total dir bytes: {total}B");
        info!("Average bytes per addressed tile: {}", total as f64 / state.num_addressed_tiles as f64);
        Ok(())
    }
}

impl WriteableTileArchive for WriteablePmtiles {
    fn deduplicates(&self) -> bool {
        true
    }

    fn tile_order(&self) -> TileOrder {
        TileOrder::Hilbert
    }

    fn new_tile_writer(&self) -> anyhow::Result<Box<dyn TileWriter + Send + '_>> {
        Ok(Box::new(DeduplicatingTileWriter { archive: self, last_entry: None }))
    }

    fn finish(&mut self, metadata: &TileArchiveMetadata) -> anyhow::Result<()> {
        if let Err(e) = self.do_finish(metadata) {
            error!("{e}");
            return Err(e.into());
        }
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.path.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len()).unwrap_or(0)
    }

    fn close(&mut self) -> anyhow::Result<()> {
        self.state.lock().out.flush()?;
        Ok(())
    }
}

struct DeduplicatingTileWriter<'a> {
    archive: &'a WriteablePmtiles,
    /// index of the last entry in `entries`
    last_entry: Option<usize>,
}

impl TileWriter for DeduplicatingTileWriter<'_> {
    fn write(&mut self, encoding_result: TileEncodingResult) -> anyhow::Result<()> {
        let mut guard = self.archive.state.lock();
        let state = &mut *guard;
        state.num_addressed_tiles += 1;
        let data = encoding_result.tile_data;
        let tile_id = encoding_result.coord.hilbert_encoded();
        let last = self.last_entry.map(|i| state.entries[i]);

        if let Some(last) = last {
            if tile_id < last.tile_id {
                state.is_clustered = false;
            } else if tile_id == last.tile_id {
                error!("Duplicate tile detected in writer");
            }
        }

        let (offset, write_tile_data) = match encoding_result.tile_data_hash {
            Some(hash) => match state.hash_to_offset.get(&hash) {
                Some(&offset) => {
                    if let Some(last) = last
                        && last.tile_id + last.run_length as u64 == tile_id
                        && last.offset == offset
                    {
                        state.entries[self.last_entry.expect("last entry")].run_length += 1;
                        return Ok(());
                    }
                    (offset, false)
                }
                None => {
                    state.hash_to_offset.insert(hash, state.current_offset);
                    (state.current_offset, true)
                }
            },
            None => {
                state.num_unhashed_tiles += 1;
                (state.current_offset, true)
            }
        };

        state.entries.push(Entry::new(tile_id, offset, data.len() as u32, 1));
        self.last_entry = Some(state.entries.len() - 1);

        if write_tile_data {
            state.out.write_all(&data)?;
            state.current_offset += data.len() as u64;
        }
        Ok(())
    }

    fn close(&mut self) -> anyhow::Result<()> {
        // no cleanup needed.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_directories_root_only() {
        let entries: Vec<Entry> = (0..1000).map(|i| Entry::new(i, i * 100, 100, 1)).collect();
        assert_eq!(1, make_directories(&entries).unwrap().num_attempts);
    }

    #[test]
    fn test_build_directories_leaves_not_too_small() {
        let entries: Vec<Entry> = (0..100_000).map(|i| Entry::new(i, i * 100, 100, 1)).collect();
        let result = make_directories(&entries).unwrap();
        assert!(result.leaf_size >= 4096, "entries in leaf: {}", result.leaf_size);
    }
}
