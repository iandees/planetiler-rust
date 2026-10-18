//! Port of `pmtiles/ReadablePmtiles.java`.

use std::fs::File;
use std::path::Path;

use crate::archive::readable_tile_archive::{ArchiveIter, ReadableTileArchive};
use crate::archive::tile::Tile;
use crate::archive::tile_archive_metadata::{self as md, Coordinate, TileArchiveMetadata, TileArchiveMetadataJson};
use crate::archive::tile_compression::TileCompression;
use crate::archive::tile_format::TileFormat;
use crate::geo::tile_coord::TileCoord;
use crate::pmtiles::pmtiles::{self, Compression, Entry, HEADER_LEN, Header, JsonMetadata, TileType};
use crate::pmtiles::writeable_pmtiles::SeekableInMemoryByteChannel;
use crate::util::gzip::gunzip;

/// Random-access byte source for a PMTiles archive.
pub trait ByteSource: Send + Sync {
    /// Reads `length` bytes at `offset`; bytes past the end of the source are zero-filled.
    fn read_at(&self, offset: u64, length: usize) -> std::io::Result<Vec<u8>>;
}

impl ByteSource for File {
    fn read_at(&self, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        let mut buf = vec![0u8; length];
        let mut read = 0;
        while read < length {
            #[cfg(unix)]
            let n = std::os::unix::fs::FileExt::read_at(self, &mut buf[read..], offset + read as u64)?;
            #[cfg(windows)]
            let n = std::os::windows::fs::FileExt::seek_read(self, &mut buf[read..], offset + read as u64)?;
            if n == 0 {
                break;
            }
            read += n;
        }
        Ok(buf)
    }
}

impl ByteSource for SeekableInMemoryByteChannel {
    fn read_at(&self, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        Ok(SeekableInMemoryByteChannel::read_at(self, offset, length))
    }
}

impl ByteSource for Vec<u8> {
    fn read_at(&self, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        let mut out = vec![0u8; length];
        let start = (offset as usize).min(self.len());
        let end = (start + length).min(self.len());
        out[..end - start].copy_from_slice(&self[start..end]);
        Ok(out)
    }
}

pub struct ReadablePmtiles {
    source: Box<dyn ByteSource>,
    header: Header,
}

/// Finds the relevant entry for a tile_id in a list of entries.
///
/// If there is an exact match for tile_id, return that. Else if the tile_id matches an entry's tile_id + run_length,
/// return that. Else if the preceding entry is a directory (run_length = 0), return that. Else return None.
pub fn find_tile(entries: &[Entry], tile_id: u64) -> Option<&Entry> {
    let mut m: i64 = 0;
    let mut n: i64 = entries.len() as i64 - 1;
    while m <= n {
        let k = ((n + m) >> 1) as usize;
        let entry = &entries[k];
        if tile_id > entry.tile_id {
            m = k as i64 + 1;
        } else if tile_id < entry.tile_id {
            n = k as i64 - 1;
        } else {
            return Some(entry);
        }
    }
    if n >= 0 {
        let entry = &entries[n as usize];
        if entry.run_length == 0 || tile_id - entry.tile_id < entry.run_length as u64 {
            return Some(entry);
        }
    }
    None
}

impl ReadablePmtiles {
    pub fn new(source: Box<dyn ByteSource>) -> anyhow::Result<ReadablePmtiles> {
        let header = Header::from_bytes(&source.read_at(0, HEADER_LEN)?)?;
        Ok(ReadablePmtiles { source, header })
    }

    pub fn new_read_from_file(path: &Path) -> anyhow::Result<ReadablePmtiles> {
        Self::new(Box::new(File::open(path)?))
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    fn get_bytes(&self, start: u64, length: usize) -> std::io::Result<Vec<u8>> {
        self.source.read_at(start, length)
    }

    fn decompress_internal(&self, bytes: Vec<u8>) -> std::io::Result<Vec<u8>> {
        if self.header.internal_compression == Compression::Gzip { gunzip(&bytes) } else { Ok(bytes) }
    }

    fn read_dir(&self, offset: u64, length: usize) -> anyhow::Result<Vec<Entry>> {
        let bytes = self.decompress_internal(self.get_bytes(offset, length)?)?;
        Ok(pmtiles::directory_from_bytes(&bytes)?)
    }

    pub fn get_json_metadata(&self) -> anyhow::Result<JsonMetadata> {
        let buf = self.get_bytes(self.header.json_metadata_offset, self.header.json_metadata_length as usize)?;
        JsonMetadata::from_bytes(&self.decompress_internal(buf)?)
    }

    fn root_dir(&self) -> anyhow::Result<Vec<Entry>> {
        self.read_dir(self.header.root_dir_offset, self.header.root_dir_length as usize)
    }

    /// Depth-first iterator over run-length entries (expanding leaf directories lazily).
    fn tile_entries(&self) -> anyhow::Result<impl Iterator<Item = anyhow::Result<Entry>> + '_> {
        let mut stack: Vec<std::vec::IntoIter<Entry>> = vec![self.root_dir()?.into_iter()];
        Ok(std::iter::from_fn(move || {
            loop {
                let top = stack.last_mut()?;
                match top.next() {
                    None => {
                        stack.pop();
                    }
                    Some(entry) if entry.run_length == 0 => {
                        match self.read_dir(self.header.leaf_directories_offset + entry.offset, entry.length as usize) {
                            Ok(dir) => stack.push(dir.into_iter()),
                            Err(e) => {
                                stack.clear();
                                return Some(Err(e));
                            }
                        }
                    }
                    Some(entry) => return Some(Ok(entry)),
                }
            }
        }))
    }
}

impl ReadableTileArchive for ReadablePmtiles {
    fn get_tile(&self, x: u32, y: u32, z: u32) -> anyhow::Result<Option<Vec<u8>>> {
        let tile_id = TileCoord::of_xyz(x, y, z).hilbert_encoded();
        let mut dir_offset = self.header.root_dir_offset;
        let mut dir_length = self.header.root_dir_length as usize;
        for _depth in 0..=3 {
            let dir = self.read_dir(dir_offset, dir_length)?;
            match find_tile(&dir, tile_id) {
                Some(entry) if entry.run_length > 0 => {
                    return Ok(Some(
                        self.get_bytes(self.header.tile_data_offset + entry.offset, entry.length as usize)?,
                    ));
                }
                Some(entry) => {
                    dir_offset = self.header.leaf_directories_offset + entry.offset;
                    dir_length = entry.length as usize;
                }
                None => return Ok(None),
            }
        }
        Ok(None)
    }

    fn get_all_tile_coords(&self) -> anyhow::Result<ArchiveIter<'_, TileCoord>> {
        Ok(Box::new(self.tile_entries()?.flat_map(|entry| -> Box<dyn Iterator<Item = anyhow::Result<TileCoord>>> {
            match entry {
                Ok(entry) => Box::new(
                    (entry.tile_id..entry.tile_id + entry.run_length as u64)
                        .map(|id| Ok(TileCoord::hilbert_decode(id))),
                ),
                Err(e) => Box::new(std::iter::once(Err(e))),
            }
        })))
    }

    fn get_all_tiles(&self) -> anyhow::Result<ArchiveIter<'_, Tile>> {
        Ok(Box::new(self.tile_entries()?.flat_map(move |entry| -> Box<dyn Iterator<Item = anyhow::Result<Tile>>> {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => return Box::new(std::iter::once(Err(e))),
            };
            match self.get_bytes(self.header.tile_data_offset + entry.offset, entry.length as usize) {
                Ok(data) => Box::new(
                    (entry.tile_id..entry.tile_id + entry.run_length as u64)
                        .map(move |id| Ok(Tile::new(TileCoord::hilbert_decode(id), data.clone()))),
                ),
                Err(e) => Box::new(std::iter::once(Err(e.into()))),
            }
        })))
    }

    fn metadata(&self) -> anyhow::Result<Option<TileArchiveMetadata>> {
        let tile_compression = match self.header.tile_compression {
            Compression::Gzip => TileCompression::Gzip,
            Compression::None => TileCompression::None,
            Compression::Unknown => TileCompression::Unknown,
        };
        let format = match self.header.tile_type {
            TileType::Mvt => Some(TileFormat::Mvt),
            TileType::Mlt => Some(TileFormat::Mlt),
            TileType::Unknown => None,
        };
        let json_metadata = self.get_json_metadata()?;
        let mut map = json_metadata.other_metadata_strings();
        let center = self.header.center();
        Ok(Some(TileArchiveMetadata::new(
            map.remove(md::NAME_KEY),
            map.remove(md::DESCRIPTION_KEY),
            map.remove(md::ATTRIBUTION_KEY),
            map.remove(md::VERSION_KEY),
            map.remove(md::TYPE_KEY),
            format,
            Some(self.header.bounds()),
            Some(Coordinate::new(center.x, center.y, self.header.center_zoom as f64)),
            Some(self.header.min_zoom as i32),
            Some(self.header.max_zoom as i32),
            TileArchiveMetadataJson::create(json_metadata.vector_layers),
            map,
            Some(tile_compression),
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::archive::tile_archive_metadata::VectorLayer;
    use crate::archive::tile_encoding_result::TileEncodingResult;
    use crate::archive::writeable_tile_archive::WriteableTileArchive;
    use crate::geo::envelope::Envelope;
    use crate::pmtiles::writeable_pmtiles::WriteablePmtiles;

    /// Equivalent of `new TileArchiveMetadata(new Profile.NullProfile(), PlanetilerConfig.defaults())`.
    fn null_profile_metadata() -> TileArchiveMetadata {
        let bounds = *crate::geo::geo_utils::WORLD_LAT_LON_BOUNDS;
        let c = bounds.centre().unwrap();
        TileArchiveMetadata {
            name: Some("Null".into()),
            type_: Some("baselayer".into()),
            format: Some(TileFormat::Mvt),
            bounds: Some(bounds),
            center: Some(Coordinate::new(c.x, c.y, crate::geo::geo_utils::get_zoom_from_lon_lat_bounds(&bounds))),
            minzoom: Some(0),
            maxzoom: Some(14),
            tile_compression: Some(TileCompression::Gzip),
            ..Default::default()
        }
    }

    fn tile_result(coord: TileCoord, data: &[u8], hash: Option<u64>) -> TileEncodingResult {
        TileEncodingResult::new(coord, data.to_vec(), hash)
    }

    fn reader(bytes: &SeekableInMemoryByteChannel) -> ReadablePmtiles {
        ReadablePmtiles::new(Box::new(bytes.clone())).unwrap()
    }

    fn coords(reader: &ReadablePmtiles) -> BTreeSet<TileCoord> {
        reader.get_all_tile_coords().unwrap().map(|c| c.unwrap()).collect()
    }

    fn tiles(reader: &ReadablePmtiles) -> BTreeSet<Tile> {
        reader.get_all_tiles().unwrap().map(|c| c.unwrap()).collect()
    }

    #[test]
    fn test_write_pmtiles_single_entry() {
        let bytes = SeekableInMemoryByteChannel::new();
        let mut archive = WriteablePmtiles::new_write_to_memory(bytes.clone()).unwrap();
        archive.initialize().unwrap();
        {
            let mut writer = archive.new_tile_writer().unwrap();
            writer.write(tile_result(TileCoord::of_xyz(0, 0, 1), &[0xa, 0x2], None)).unwrap();
            writer.close().unwrap();
        }
        archive.finish(&null_profile_metadata()).unwrap();
        let reader = reader(&bytes);
        let header = reader.header();
        assert_eq!(1, header.num_addressed_tiles);
        assert_eq!(1, header.num_tile_contents);
        assert_eq!(1, header.num_tile_entries);
        assert_eq!(Some(vec![0xa, 0x2]), reader.get_tile(0, 0, 1).unwrap());
        assert_eq!(None, reader.get_tile(0, 0, 0).unwrap());
        assert_eq!(None, reader.get_tile(0, 0, 2).unwrap());
        assert_eq!(BTreeSet::from([TileCoord::of_xyz(0, 0, 1)]), coords(&reader));
        assert_eq!(BTreeSet::from([Tile::new(TileCoord::of_xyz(0, 0, 1), vec![0xa, 0x2])]), tiles(&reader));
    }

    fn round_trip_metadata(input: TileArchiveMetadata, output: TileArchiveMetadata) {
        let bytes = SeekableInMemoryByteChannel::new();
        let mut archive = WriteablePmtiles::new_write_to_memory(bytes.clone()).unwrap();
        archive.initialize().unwrap();
        {
            let mut writer = archive.new_tile_writer().unwrap();
            writer.write(tile_result(TileCoord::of_xyz(0, 0, 0), &[0xa, 0x2], None)).unwrap();
            writer.close().unwrap();
        }
        archive.finish(&input).unwrap();
        let reader = reader(&bytes);
        assert_eq!(Some(vec![0xa, 0x2]), reader.get_tile(0, 0, 0).unwrap());
        assert_eq!(Some(output), reader.metadata().unwrap());
    }

    #[test]
    fn test_roundtrip_metadata() {
        let metadata = TileArchiveMetadata::new(
            Some("MyName".into()),
            Some("MyDescription".into()),
            Some("MyAttribution".into()),
            Some("MyVersion".into()),
            Some("baselayer".into()),
            Some(TileFormat::Mvt),
            Some(Envelope::new(1.1, 2.2, 3.3, 4.4)),
            Some(Coordinate::new(5.5, 6.6, 7.0)),
            Some(8),
            Some(9),
            Some(TileArchiveMetadataJson::new(vec![VectorLayer::new("MyLayer", BTreeMap::new())])),
            BTreeMap::from([("other key".into(), "other value".into())]),
            Some(TileCompression::Gzip),
        );
        round_trip_metadata(metadata.clone(), metadata);
    }

    #[test]
    fn test_roundtrip_metadata_minimal() {
        for compression in [TileCompression::Gzip, TileCompression::None] {
            round_trip_metadata(
                TileArchiveMetadata { tile_compression: Some(compression), ..Default::default() },
                TileArchiveMetadata {
                    bounds: Some(Envelope::new(-180.0, 180.0, -85.0511287, 85.0511287)),
                    center: Some(Coordinate::new(0.0, 0.0, 0.0)),
                    minzoom: Some(0),
                    maxzoom: Some(16),
                    tile_compression: Some(compression),
                    ..Default::default()
                },
            );
        }
    }

    #[test]
    fn test_read_pmtiles_from_tippecanoe() {
        // '{"type":"Polygon","coordinates":[[[0,0],[0,1],[1,1],[1,0],[0,0]]]}' | ./tippecanoe -zg -o box1degree.pmtiles
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources/box1degree.pmtiles");
        let reader = ReadablePmtiles::new_read_from_file(&path).unwrap();
        assert!(reader.header().max_zoom <= 15);
        assert!(reader.get_tile(0, 0, 0).unwrap().is_some());
        assert!(reader.metadata().unwrap().is_some());
    }

    #[test]
    fn test_write_pmtiles_duplication() {
        let bytes = SeekableInMemoryByteChannel::new();
        let mut archive = WriteablePmtiles::new_write_to_memory(bytes.clone()).unwrap();
        archive.initialize().unwrap();
        {
            let mut writer = archive.new_tile_writer().unwrap();
            for z in 0..3 {
                writer.write(tile_result(TileCoord::of_xyz(0, 0, z), &[0xa, 0x2], Some(42))).unwrap();
            }
            writer.close().unwrap();
        }
        archive.finish(&null_profile_metadata()).unwrap();
        let reader = reader(&bytes);
        let header = reader.header();
        assert_eq!(3, header.num_addressed_tiles);
        assert_eq!(1, header.num_tile_contents);
        assert_eq!(2, header.num_tile_entries); // z0 and z1 are contiguous
        for z in 0..3 {
            assert_eq!(Some(vec![0xa, 0x2]), reader.get_tile(0, 0, z).unwrap());
        }
        let expected: BTreeSet<TileCoord> = (0..3).map(|z| TileCoord::of_xyz(0, 0, z)).collect();
        assert_eq!(expected, coords(&reader));
        let expected: BTreeSet<Tile> = (0..3).map(|z| Tile::new(TileCoord::of_xyz(0, 0, z), vec![0xa, 0x2])).collect();
        assert_eq!(expected, tiles(&reader));
    }

    #[test]
    fn test_write_pmtiles_unclustered() {
        let bytes = SeekableInMemoryByteChannel::new();
        let mut archive = WriteablePmtiles::new_write_to_memory(bytes.clone()).unwrap();
        archive.initialize().unwrap();
        {
            let mut writer = archive.new_tile_writer().unwrap();
            writer.write(tile_result(TileCoord::of_xyz(0, 0, 1), &[0xa, 0x2], Some(42))).unwrap();
            writer.write(tile_result(TileCoord::of_xyz(0, 0, 0), &[0xa, 0x2], Some(42))).unwrap();
            writer.close().unwrap();
        }
        archive.finish(&null_profile_metadata()).unwrap();
        let reader = reader(&bytes);
        let header = reader.header();
        assert_eq!(2, header.num_addressed_tiles);
        assert_eq!(1, header.num_tile_contents);
        assert_eq!(2, header.num_tile_entries);
        assert!(!header.clustered);
        assert_eq!(Some(vec![0xa, 0x2]), reader.get_tile(0, 0, 0).unwrap());
        assert_eq!(Some(vec![0xa, 0x2]), reader.get_tile(0, 0, 1).unwrap());
        assert_eq!(BTreeSet::from([TileCoord::of_xyz(0, 0, 0), TileCoord::of_xyz(0, 0, 1)]), coords(&reader));
        assert_eq!(2, tiles(&reader).len());
    }

    #[test]
    fn test_write_pmtiles_leaf_directories() {
        let bytes = SeekableInMemoryByteChannel::new();
        let mut archive = WriteablePmtiles::new_write_to_memory(bytes.clone()).unwrap();
        archive.initialize().unwrap();
        const ENTRIES: u64 = 20000;
        let mut expected_coords = BTreeSet::new();
        let mut expected_tiles = BTreeSet::new();
        {
            let mut writer = archive.new_tile_writer().unwrap();
            for i in 0..ENTRIES {
                let coord = TileCoord::hilbert_decode(i);
                let data = (i as i32).to_be_bytes().to_vec();
                expected_coords.insert(coord);
                expected_tiles.insert(Tile::new(coord, data.clone()));
                writer.write(TileEncodingResult::new(coord, data, None)).unwrap();
            }
            writer.close().unwrap();
        }
        archive.finish(&null_profile_metadata()).unwrap();
        let reader = reader(&bytes);
        let header = reader.header();
        assert_eq!(ENTRIES, header.num_addressed_tiles);
        assert_eq!(ENTRIES, header.num_tile_contents);
        assert_eq!(ENTRIES, header.num_tile_entries);
        assert!(header.leaf_directories_length > 0);
        for i in 0..ENTRIES {
            let coord = TileCoord::hilbert_decode(i);
            assert_eq!(
                Some((i as i32).to_be_bytes().to_vec()),
                reader.get_tile(coord.x, coord.y, coord.z).unwrap(),
                "tileCoord={coord} did not match"
            );
        }
        assert_eq!(expected_coords, coords(&reader));
        assert_eq!(expected_tiles, tiles(&reader));
    }

    #[test]
    fn test_find_tile() {
        let entries = vec![Entry::new(0, 0, 1, 2), Entry::new(5, 1, 1, 0), Entry::new(10, 2, 1, 1)];
        assert_eq!(Some(&entries[0]), find_tile(&entries, 1));
        assert_eq!(None, find_tile(&entries, 2));
        assert_eq!(Some(&entries[1]), find_tile(&entries, 7));
        assert_eq!(Some(&entries[2]), find_tile(&entries, 10));
        assert_eq!(None, find_tile(&entries, 11));
    }
}
