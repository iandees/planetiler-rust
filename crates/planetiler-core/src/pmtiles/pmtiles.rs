//! Port of `pmtiles/Pmtiles.java`: PMTiles v3 header, directory and JSON metadata encoding.
//!
//! See the [PMTiles Specification](https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md).

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::archive::tile_archive_metadata::{Coordinate, VectorLayer};
use crate::geo::envelope::Envelope;

/// Error for malformed PMTiles archives (Java `FileFormatException`).
#[derive(Debug, Error)]
#[error("{0}")]
pub struct FileFormatException(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Compression {
    Unknown,
    None,
    Gzip,
}

impl Compression {
    pub fn value(&self) -> u8 {
        match self {
            Compression::Unknown => 0,
            Compression::None => 1,
            Compression::Gzip => 2,
        }
    }

    pub fn from_byte(b: u8) -> Compression {
        match b {
            1 => Compression::None,
            2 => Compression::Gzip,
            _ => Compression::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TileType {
    Unknown,
    Mvt,
    Mlt,
}

impl TileType {
    pub fn value(&self) -> u8 {
        match self {
            TileType::Unknown => 0,
            TileType::Mvt => 1,
            TileType::Mlt => 6,
        }
    }

    pub fn from_byte(b: u8) -> TileType {
        match b {
            1 => TileType::Mvt,
            6 => TileType::Mlt,
            _ => TileType::Unknown,
        }
    }
}

pub const HEADER_LEN: usize = 127;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub spec_version: u8,
    pub root_dir_offset: u64,
    pub root_dir_length: u64,
    pub json_metadata_offset: u64,
    pub json_metadata_length: u64,
    pub leaf_directories_offset: u64,
    pub leaf_directories_length: u64,
    pub tile_data_offset: u64,
    pub tile_data_length: u64,
    pub num_addressed_tiles: u64,
    pub num_tile_entries: u64,
    pub num_tile_contents: u64,
    pub clustered: bool,
    pub internal_compression: Compression,
    pub tile_compression: Compression,
    pub tile_type: TileType,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// Decimal longitude stored as a signed 32-bit integer by multiplying by 10,000,000.
    pub min_lon_e7: i32,
    pub min_lat_e7: i32,
    pub max_lon_e7: i32,
    pub max_lat_e7: i32,
    pub center_zoom: u8,
    pub center_lon_e7: i32,
    pub center_lat_e7: i32,
}

struct LeReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl LeReader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], FileFormatException> {
        if self.pos + N > self.bytes.len() {
            return Err(FileFormatException("Failed to read enough bytes for PMTiles header.".into()));
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.pos..self.pos + N]);
        self.pos += N;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, FileFormatException> {
        Ok(self.take::<1>()?[0])
    }

    fn u64(&mut self) -> Result<u64, FileFormatException> {
        Ok(u64::from_le_bytes(self.take()?))
    }

    fn i32(&mut self) -> Result<i32, FileFormatException> {
        Ok(i32::from_le_bytes(self.take()?))
    }
}

impl Header {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_LEN);
        buf.extend_from_slice(b"PMTiles");
        buf.push(self.spec_version);
        for v in [
            self.root_dir_offset,
            self.root_dir_length,
            self.json_metadata_offset,
            self.json_metadata_length,
            self.leaf_directories_offset,
            self.leaf_directories_length,
            self.tile_data_offset,
            self.tile_data_length,
            self.num_addressed_tiles,
            self.num_tile_entries,
            self.num_tile_contents,
        ] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf.push(u8::from(self.clustered));
        buf.push(self.internal_compression.value());
        buf.push(self.tile_compression.value());
        buf.push(self.tile_type.value());
        buf.push(self.min_zoom);
        buf.push(self.max_zoom);
        for v in [self.min_lon_e7, self.min_lat_e7, self.max_lon_e7, self.max_lat_e7] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf.push(self.center_zoom);
        buf.extend_from_slice(&self.center_lon_e7.to_le_bytes());
        buf.extend_from_slice(&self.center_lat_e7.to_le_bytes());
        debug_assert_eq!(HEADER_LEN, buf.len());
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Header, FileFormatException> {
        let mut r = LeReader { bytes, pos: 0 };
        let magic = r.take::<7>()?;
        if &magic != b"PMTiles" {
            return Err(FileFormatException("Incorrect magic number for PMTiles archive.".into()));
        }
        Ok(Header {
            spec_version: r.u8()?,
            root_dir_offset: r.u64()?,
            root_dir_length: r.u64()?,
            json_metadata_offset: r.u64()?,
            json_metadata_length: r.u64()?,
            leaf_directories_offset: r.u64()?,
            leaf_directories_length: r.u64()?,
            tile_data_offset: r.u64()?,
            tile_data_length: r.u64()?,
            num_addressed_tiles: r.u64()?,
            num_tile_entries: r.u64()?,
            num_tile_contents: r.u64()?,
            clustered: r.u8()? == 1,
            internal_compression: Compression::from_byte(r.u8()?),
            tile_compression: Compression::from_byte(r.u8()?),
            tile_type: TileType::from_byte(r.u8()?),
            min_zoom: r.u8()?,
            max_zoom: r.u8()?,
            min_lon_e7: r.i32()?,
            min_lat_e7: r.i32()?,
            max_lon_e7: r.i32()?,
            max_lat_e7: r.i32()?,
            center_zoom: r.u8()?,
            center_lon_e7: r.i32()?,
            center_lat_e7: r.i32()?,
        })
    }

    pub fn bounds(&self) -> Envelope {
        Envelope::new(
            self.min_lon_e7 as f64 / 1e7,
            self.max_lon_e7 as f64 / 1e7,
            self.min_lat_e7 as f64 / 1e7,
            self.max_lat_e7 as f64 / 1e7,
        )
    }

    pub fn center(&self) -> Coordinate {
        Coordinate::xy(self.center_lon_e7 as f64 / 1e7, self.center_lat_e7 as f64 / 1e7)
    }
}

/// A directory entry: either a run of `run_length` tiles starting at `tile_id`, or (when `run_length == 0`) a pointer
/// to a leaf directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

impl Entry {
    pub fn new(tile_id: u64, offset: u64, length: u32, run_length: u32) -> Entry {
        Entry { tile_id, offset, length, run_length }
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.tile_id.cmp(&other.tile_id)
    }
}

// TODO(port): switch to crate::util::var_int once ported.
fn put_var_long(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn get_var_long(bytes: &[u8], pos: &mut usize) -> Result<u64, FileFormatException> {
    let mut result = 0u64;
    let mut shift = 0;
    loop {
        let b = *bytes.get(*pos).ok_or_else(|| FileFormatException("Unexpected end of PMTiles directory".into()))?;
        *pos += 1;
        if shift >= 64 {
            return Err(FileFormatException("Varint too long".into()));
        }
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

/// Convert a directory of entries (sorted by ascending `tile_id`, non-empty) to uncompressed bytes.
pub fn directory_to_bytes(slice: &[Entry]) -> Vec<u8> {
    let mut dir = Vec::with_capacity(slice.len() * 4 + 8);
    put_var_long(slice.len() as u64, &mut dir);
    let mut last_id = 0;
    for entry in slice {
        put_var_long(entry.tile_id - last_id, &mut dir);
        last_id = entry.tile_id;
    }
    for entry in slice {
        put_var_long(entry.run_length as u64, &mut dir);
    }
    for entry in slice {
        put_var_long(entry.length as u64, &mut dir);
    }
    let mut last: Option<&Entry> = None;
    for entry in slice {
        match last {
            Some(l) if entry.offset == l.offset + l.length as u64 => put_var_long(0, &mut dir),
            _ => put_var_long(entry.offset + 1, &mut dir),
        }
        last = Some(entry);
    }
    dir
}

/// Convert the range `start..end` of a directory to uncompressed bytes.
pub fn directory_to_bytes_range(slice: &[Entry], start: usize, end: usize) -> Vec<u8> {
    directory_to_bytes(&slice[start..end])
}

/// Parse an uncompressed directory.
pub fn directory_from_bytes(bytes: &[u8]) -> Result<Vec<Entry>, FileFormatException> {
    let mut pos = 0;
    let num_entries = get_var_long(bytes, &mut pos)? as usize;
    let mut result = Vec::with_capacity(num_entries.min(bytes.len()));
    let mut last_id = 0;
    for _ in 0..num_entries {
        let tile_id = last_id + get_var_long(bytes, &mut pos)?;
        result.push(Entry::new(tile_id, 0, 0, 0));
        last_id = tile_id;
    }
    for entry in result.iter_mut() {
        entry.run_length = get_var_long(bytes, &mut pos)? as u32;
    }
    for entry in result.iter_mut() {
        entry.length = get_var_long(bytes, &mut pos)? as u32;
    }
    for i in 0..num_entries {
        let tmp = get_var_long(bytes, &mut pos)?;
        result[i].offset =
            if i > 0 && tmp == 0 { result[i - 1].offset + result[i - 1].length as u64 } else { tmp.wrapping_sub(1) };
    }
    Ok(result)
}

/// Arbitrary application-specific JSON metadata in the archive: name, attribution, created_at, planetiler build
/// SHA, vector_layers, etc.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct JsonMetadata {
    #[serde(rename = "vector_layers", default, skip_serializing_if = "Option::is_none")]
    pub vector_layers: Option<Vec<VectorLayer>>,
    #[serde(flatten)]
    pub other_metadata: BTreeMap<String, serde_json::Value>,
}

impl JsonMetadata {
    pub fn new(vector_layers: Option<Vec<VectorLayer>>, other_metadata: BTreeMap<String, String>) -> JsonMetadata {
        JsonMetadata {
            vector_layers,
            other_metadata: other_metadata.into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect(),
        }
    }

    /// Other metadata values as strings (non-string JSON values are serialized to JSON text).
    pub fn other_metadata_strings(&self) -> BTreeMap<String, String> {
        self.other_metadata
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
            .collect()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("metadata is always serializable")
    }

    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<JsonMetadata> {
        serde_json::from_slice(bytes)
            .map_err(|e| anyhow::anyhow!("Invalid metadata json: {} ({e})", String::from_utf8_lossy(bytes)))
    }
}

// TODO(port): switch to crate::util::gzip once ported.
pub(crate) fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes)?;
    encoder.finish()
}

pub(crate) fn gunzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(bytes).read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_header() {
        let header = Header {
            spec_version: 3,
            root_dir_offset: 1,
            root_dir_length: 2,
            json_metadata_offset: 3,
            json_metadata_length: 4,
            leaf_directories_offset: 5,
            leaf_directories_length: 6,
            tile_data_offset: 7,
            tile_data_length: 8,
            num_addressed_tiles: 9,
            num_tile_entries: 10,
            num_tile_contents: 11,
            clustered: true,
            internal_compression: Compression::Gzip,
            tile_compression: Compression::Gzip,
            tile_type: TileType::Mvt,
            min_zoom: 1,
            max_zoom: 3,
            min_lon_e7: -10_100_000,
            min_lat_e7: -20_200_000,
            max_lon_e7: 10_100_000,
            max_lat_e7: 20_200_000,
            center_zoom: 2,
            center_lon_e7: -5_500_000,
            center_lat_e7: -6_600_000,
        };
        let bytes = header.to_bytes();
        assert_eq!(HEADER_LEN, bytes.len());
        assert_eq!(header, Header::from_bytes(&bytes).unwrap());
    }

    #[test]
    fn test_bad_header() {
        assert!(Header::from_bytes(&[]).is_err());
        assert!(Header::from_bytes(&[0u8; 127]).is_err());
    }

    #[test]
    fn test_roundtrip_directory_minimal() {
        let input = vec![Entry::new(0, 0, 1, 1)];
        assert_eq!(input, directory_from_bytes(&directory_to_bytes(&input)).unwrap());
    }

    #[test]
    fn test_roundtrip_directory_simple() {
        // make sure there are cases of contiguous entries and non-contiguous entries.
        let input = vec![Entry::new(0, 0, 1, 0), Entry::new(1, 1, 1, 1), Entry::new(2, 3, 1, 1)];
        assert_eq!(input, directory_from_bytes(&directory_to_bytes(&input)).unwrap());
        assert_eq!(input, directory_from_bytes(&directory_to_bytes_range(&input, 0, input.len())).unwrap());
    }

    #[test]
    fn test_roundtrip_directory_slice() {
        let input = vec![Entry::new(0, 0, 1, 0), Entry::new(1, 1, 1, 1), Entry::new(2, 3, 1, 1)];
        assert_eq!(1, directory_from_bytes(&directory_to_bytes_range(&input, 1, 2)).unwrap().len());
    }

    #[test]
    fn test_directory_bytes() {
        // byte-for-byte check against the spec encoding
        let input = vec![Entry::new(0, 0, 1, 1), Entry::new(1, 1, 300, 1), Entry::new(5, 1000, 1, 2)];
        assert_eq!(vec![3, 0, 1, 4, 1, 1, 2, 1, 0xac, 0x02, 1, 1, 0, 0xe9, 0x07], directory_to_bytes(&input));
    }

    #[test]
    fn test_pmtiles_metadata_top_level_keys() {
        let metadata = JsonMetadata::new(Some(vec![]), BTreeMap::from([("testkey".into(), "testvalue".into())]));
        let node: serde_json::Value = serde_json::from_slice(&metadata.to_bytes()).unwrap();
        assert_eq!("testvalue", node["testkey"]);
        assert_eq!(serde_json::json!([]), node["vector_layers"]);
    }
}
