//! Port of the test helper `stream/InMemoryStreamArchive.java`: reads the output of the csv/tsv, json and
//! proto/pbf stream archive writers back into memory so it can be inspected like any other
//! [`ReadableTileArchive`].

use std::io::BufRead;
use std::path::Path;

use anyhow::Context;
use prost::Message;
use serde::Deserialize;

use crate::archive::readable_tile_archive::{ArchiveIter, ReadableTileArchive};
use crate::archive::tile_archive_metadata::TileArchiveMetadata;
use crate::geo::tile_coord::TileCoord;
use crate::proto::stream_archive as proto;
use crate::stream::stream_archive_utils::base64_decode;

/// Tiles (and metadata, if the format has it) read from a stream archive.
pub struct InMemoryStreamArchive {
    tiles: Vec<(TileCoord, Vec<u8>)>,
    metadata: Option<TileArchiveMetadata>,
}

impl InMemoryStreamArchive {
    /// Reads a csv/tsv archive with `x`, `y`, `z` and base64-encoded tile data columns separated by
    /// `column_separator`. These archives have no metadata.
    pub fn from_csv(path: &Path, column_separator: &str) -> anyhow::Result<InMemoryStreamArchive> {
        let mut tiles = Vec::new();
        for line in std::io::BufReader::new(std::fs::File::open(path)?).lines() {
            let line = line?;
            let splits: Vec<&str> = line.split(column_separator).collect();
            anyhow::ensure!(splits.len() >= 4, "expected 4 columns in {line:?}");
            let coord = TileCoord::of_xyz(splits[0].parse()?, splits[1].parse()?, splits[2].parse()?);
            let data = base64_decode(splits[3]).with_context(|| format!("invalid base64 in {line:?}"))?;
            tiles.push((coord, data));
        }
        Ok(InMemoryStreamArchive { tiles, metadata: None })
    }

    /// Reads a length-delimited protobuf archive. Like Java, the metadata in the finish entry is not read back.
    pub fn from_protobuf(path: &Path) -> anyhow::Result<InMemoryStreamArchive> {
        let bytes = std::fs::read(path)?;
        let mut buf = bytes.as_slice();
        let mut tiles = Vec::new();
        while !buf.is_empty() {
            if let Some(proto::entry::Entry::Tile(tile)) = proto::Entry::decode_length_delimited(&mut buf)?.entry {
                tiles.push((TileCoord::of_xyz(tile.x as u32, tile.y as u32, tile.z as u32), tile.encoded_data));
            }
        }
        Ok(InMemoryStreamArchive { tiles, metadata: None })
    }

    /// Reads a json archive. Fails if there is no `finish` entry with metadata.
    pub fn from_json(path: &Path) -> anyhow::Result<InMemoryStreamArchive> {
        let reader = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut tiles = Vec::new();
        let mut metadata = None;
        for entry in serde_json::Deserializer::from_reader(reader).into_iter::<serde_json::Value>() {
            let entry = entry?;
            match entry.get("type").and_then(|t| t.as_str()) {
                Some("tile") => {
                    let int = |key: &str| -> anyhow::Result<u32> {
                        Ok(entry.get(key).and_then(|v| v.as_u64()).with_context(|| format!("missing {key}"))? as u32)
                    };
                    let coord = TileCoord::of_xyz(int("x")?, int("y")?, int("z")?);
                    let data = entry
                        .get("encodedData")
                        .and_then(|v| v.as_str())
                        .and_then(base64_decode)
                        .context("missing or invalid encodedData")?;
                    tiles.push((coord, data));
                }
                Some("finish") => {
                    let value = entry.get("metadata").cloned().context("finish entry without metadata")?;
                    metadata = Some(TileArchiveMetadata::deserialize(value)?);
                }
                _ => {}
            }
        }
        let metadata = metadata.context("json archive has no finish entry")?;
        Ok(InMemoryStreamArchive { tiles, metadata: Some(metadata) })
    }
}

impl ReadableTileArchive for InMemoryStreamArchive {
    fn get_tile(&self, x: u32, y: u32, z: u32) -> anyhow::Result<Option<Vec<u8>>> {
        let coord = TileCoord::of_xyz(x, y, z);
        Ok(self.tiles.iter().find(|(c, _)| *c == coord).map(|(_, data)| data.clone()))
    }

    fn get_all_tile_coords(&self) -> anyhow::Result<ArchiveIter<'_, TileCoord>> {
        Ok(Box::new(self.tiles.iter().map(|(coord, _)| Ok(*coord))))
    }

    fn metadata(&self) -> anyhow::Result<Option<TileArchiveMetadata>> {
        Ok(self.metadata.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::stream_archive_utils::base64_encode;

    #[test]
    fn reads_csv() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tsv");
        std::fs::write(&path, format!("0\t0\t0\t{}\n1\t2\t3\t{}\n", base64_encode(&[1, 2]), base64_encode(&[3])))
            .unwrap();
        let archive = InMemoryStreamArchive::from_csv(&path, "\t").unwrap();
        assert_eq!(Some(vec![3]), archive.get_tile(1, 2, 3).unwrap());
        assert_eq!(None, archive.get_tile(1, 1, 3).unwrap());
        let coords: Vec<TileCoord> = archive.get_all_tile_coords().unwrap().map(Result::unwrap).collect();
        assert_eq!(vec![TileCoord::of_xyz(0, 0, 0), TileCoord::of_xyz(1, 2, 3)], coords);
        assert!(archive.metadata().unwrap().is_none());
    }

    #[test]
    fn reads_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        std::fs::write(
            &path,
            "{\"type\":\"initialization\"}\n{\"type\":\"tile\",\"x\":1,\"y\":2,\"z\":3,\"encodedData\":\"AAE=\"}\n\
             {\"type\":\"finish\",\"metadata\":{\"name\":\"n\"}}\n",
        )
        .unwrap();
        let archive = InMemoryStreamArchive::from_json(&path).unwrap();
        assert_eq!(Some(vec![0, 1]), archive.get_tile(1, 2, 3).unwrap());
        assert_eq!(Some("n"), archive.metadata().unwrap().unwrap().name.as_deref());
    }

    #[test]
    fn reads_proto() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.proto");
        let mut bytes = Vec::new();
        proto::Entry::default().encode_length_delimited(&mut bytes).unwrap();
        proto::Entry {
            entry: Some(proto::entry::Entry::Tile(proto::TileEntry { x: 1, y: 2, z: 3, encoded_data: vec![9] })),
        }
        .encode_length_delimited(&mut bytes)
        .unwrap();
        std::fs::write(&path, bytes).unwrap();
        let archive = InMemoryStreamArchive::from_protobuf(&path).unwrap();
        assert_eq!(Some(vec![9]), archive.get_tile(1, 2, 3).unwrap());
        assert!(archive.metadata().unwrap().is_none());
    }
}
