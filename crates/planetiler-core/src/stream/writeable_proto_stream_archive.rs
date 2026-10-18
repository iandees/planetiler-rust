//! Port of `stream/WriteableProtoStreamArchive.java`: writes protobuf-serialized tile data as well as meta data into
//! file(s). The messages are of type [`proto::Entry`] and are length-delimited.
//!
//! Custom plugins/integrations should prefer to use this format since - given it's binary - it's the fastest to write
//! and read, and once setup, it should also be the simplest to use since models and the code to parse it are
//! generated. It's also the most stable and straightforward format in regards to schema evolution.

use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::bail;
use prost::Message;

use crate::archive::tile_archive_metadata::{Coordinate, FieldType, TileArchiveMetadata, VectorLayer};
use crate::archive::tile_compression::TileCompression;
use crate::archive::tile_encoding_result::TileEncodingResult;
use crate::archive::writeable_tile_archive::{TileWriter, WriteableTileArchive};
use crate::geo::envelope::Envelope;
use crate::geo::tile_order::TileOrder;
use crate::proto::stream_archive as proto;
use crate::stream::stream_archive_config::StreamArchiveConfig;
use crate::stream::writeable_stream_archive::StreamArchiveBase;

pub struct WriteableProtoStreamArchive {
    base: StreamArchiveBase,
}

impl WriteableProtoStreamArchive {
    pub fn new_write_to_file(path: &Path, config: StreamArchiveConfig) -> anyhow::Result<WriteableProtoStreamArchive> {
        Ok(WriteableProtoStreamArchive { base: StreamArchiveBase::new(path, config)? })
    }

    fn write_entry(&self, entry: &proto::Entry) -> anyhow::Result<()> {
        let mut out = self.base.primary_output_stream();
        out.write_all(&entry.encode_length_delimited_to_vec())?;
        out.flush()?;
        Ok(())
    }
}

/// Converts archive metadata to its protobuf representation.
pub fn to_export_data(metadata: &TileArchiveMetadata) -> anyhow::Result<proto::Metadata> {
    let mut result = proto::Metadata {
        name: metadata.name.clone().unwrap_or_default(),
        description: metadata.description.clone().unwrap_or_default(),
        attribution: metadata.attribution.clone().unwrap_or_default(),
        version: metadata.version.clone().unwrap_or_default(),
        r#type: metadata.type_.clone().unwrap_or_default(),
        format: metadata.format.and_then(|f| f.mbtiles()).unwrap_or_default().to_string(),
        bounds: metadata.bounds.as_ref().map(envelope_to_export_data),
        center: metadata.center.as_ref().map(coordinate_to_export_data),
        min_zoom: metadata.minzoom.unwrap_or_default(),
        max_zoom: metadata.maxzoom.unwrap_or_default(),
        vector_layers: metadata
            .vector_layers()
            .map(|l| l.iter().map(vector_layer_to_export_data).collect())
            .unwrap_or_default(),
        others: metadata.others.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        tile_compression: 0,
    };
    result.set_tile_compression(match metadata.tile_compression {
        Some(TileCompression::Gzip) => proto::TileCompression::Gzip,
        Some(TileCompression::None) => proto::TileCompression::None,
        Some(TileCompression::Unknown) => bail!("should not produce \"UNKNOWN\" compression"),
        None => bail!("tile compression is required"),
    });
    Ok(result)
}

fn envelope_to_export_data(envelope: &Envelope) -> proto::Envelope {
    proto::Envelope { min_x: envelope.min_x, max_x: envelope.max_x, min_y: envelope.min_y, max_y: envelope.max_y }
}

fn coordinate_to_export_data(coord: &Coordinate) -> proto::Coordinate {
    proto::Coordinate { x: coord.x, y: coord.y, z: coord.z }
}

fn vector_layer_to_export_data(vector_layer: &VectorLayer) -> proto::VectorLayer {
    proto::VectorLayer {
        id: vector_layer.id.clone(),
        fields: vector_layer
            .fields
            .iter()
            .map(|(k, v)| {
                let t = match v {
                    FieldType::Number => proto::vector_layer::FieldType::Number,
                    FieldType::Boolean => proto::vector_layer::FieldType::Boolean,
                    FieldType::String => proto::vector_layer::FieldType::String,
                };
                (k.clone(), t as i32)
            })
            .collect(),
        description: vector_layer.description.clone().unwrap_or_default(),
        min_zoom: vector_layer.minzoom.unwrap_or_default(),
        max_zoom: vector_layer.maxzoom.unwrap_or_default(),
    }
}

impl WriteableTileArchive for WriteableProtoStreamArchive {
    fn deduplicates(&self) -> bool {
        false
    }

    fn tile_order(&self) -> TileOrder {
        TileOrder::Tms
    }

    fn initialize(&mut self) -> anyhow::Result<()> {
        self.write_entry(&proto::Entry::default())
    }

    fn new_tile_writer(&self) -> anyhow::Result<Box<dyn TileWriter + Send + '_>> {
        Ok(Box::new(ProtoTileArchiveWriter { out: BufWriter::new(self.base.next_output_stream()?), buf: Vec::new() }))
    }

    fn finish(&mut self, metadata: &TileArchiveMetadata) -> anyhow::Result<()> {
        let entry = proto::Entry {
            entry: Some(proto::entry::Entry::Finish(proto::FinishEntry { metadata: Some(to_export_data(metadata)?) })),
        };
        self.write_entry(&entry)
    }

    fn bytes_written(&self) -> u64 {
        self.base.bytes_written()
    }

    fn close(&mut self) -> anyhow::Result<()> {
        self.base.close()
    }
}

struct ProtoTileArchiveWriter {
    out: BufWriter<Box<dyn Write + Send>>,
    buf: Vec<u8>,
}

impl TileWriter for ProtoTileArchiveWriter {
    fn write(&mut self, encoding_result: TileEncodingResult) -> anyhow::Result<()> {
        let coord = encoding_result.coord;
        let entry = proto::Entry {
            entry: Some(proto::entry::Entry::Tile(proto::TileEntry {
                x: coord.x as i32,
                y: coord.y as i32,
                z: coord.z as i32,
                encoded_data: encoding_result.tile_data,
            })),
        };
        self.buf.clear();
        entry.encode_length_delimited(&mut self.buf)?;
        self.out.write_all(&self.buf)?;
        Ok(())
    }

    fn close(&mut self) -> anyhow::Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::test_fixtures::max_metadata_deserialized;
    use crate::geo::tile_coord::TileCoord;
    use std::collections::HashMap;

    fn max_metadata_out() -> proto::Metadata {
        let mut m = proto::Metadata {
            name: "name".into(),
            description: "description".into(),
            attribution: "attribution".into(),
            version: "version".into(),
            r#type: "type".into(),
            format: "pbf".into(),
            bounds: Some(proto::Envelope { min_x: 0.0, max_x: 1.0, min_y: 2.0, max_y: 3.0 }),
            center: Some(proto::Coordinate { x: 1.3, y: 3.7, z: 1.0 }),
            min_zoom: 2,
            max_zoom: 3,
            vector_layers: vec![
                proto::VectorLayer {
                    id: "vl0".into(),
                    description: "description".into(),
                    min_zoom: 1,
                    max_zoom: 2,
                    fields: HashMap::from([
                        ("1".into(), proto::vector_layer::FieldType::Boolean as i32),
                        ("2".into(), proto::vector_layer::FieldType::Number as i32),
                        ("3".into(), proto::vector_layer::FieldType::String as i32),
                    ]),
                },
                proto::VectorLayer { id: "vl1".into(), ..Default::default() },
            ],
            others: HashMap::from([("a".into(), "b".into()), ("c".into(), "d".into())]),
            tile_compression: 0,
        };
        m.set_tile_compression(proto::TileCompression::Gzip);
        m
    }

    fn min_metadata_in() -> TileArchiveMetadata {
        TileArchiveMetadata { tile_compression: Some(TileCompression::None), ..Default::default() }
    }

    fn min_metadata_out() -> proto::Metadata {
        let mut m = proto::Metadata::default();
        m.set_tile_compression(proto::TileCompression::None);
        m
    }

    fn read_all_entries(path: &Path) -> Vec<proto::Entry> {
        let bytes = std::fs::read(path).unwrap();
        let mut buf = bytes.as_slice();
        let mut result = Vec::new();
        while !buf.is_empty() {
            result.push(proto::Entry::decode_length_delimited(&mut buf).unwrap());
        }
        result
    }

    fn to_entry(r: &TileEncodingResult) -> proto::Entry {
        proto::Entry {
            entry: Some(proto::entry::Entry::Tile(proto::TileEntry {
                x: r.coord.x as i32,
                y: r.coord.y as i32,
                z: r.coord.z as i32,
                encoded_data: r.tile_data.clone(),
            })),
        }
    }

    fn wrap_init() -> proto::Entry {
        proto::Entry::default()
    }

    fn wrap_finish(metadata: proto::Metadata) -> proto::Entry {
        proto::Entry { entry: Some(proto::entry::Entry::Finish(proto::FinishEntry { metadata: Some(metadata) })) }
    }

    fn tile(x: u32, y: u32, z: u32, data: u8, hash: Option<u64>) -> TileEncodingResult {
        TileEncodingResult::new(TileCoord::of_xyz(x, y, z), vec![data], hash)
    }

    #[test]
    fn test_write_single_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("out.proto");
        let tile0 = tile(0, 0, 0, 0, None);
        let tile1 = tile(1, 2, 3, 1, Some(1));
        let mut archive =
            WriteableProtoStreamArchive::new_write_to_file(&file, StreamArchiveConfig::default()).unwrap();
        archive.initialize().unwrap();
        {
            let mut w = archive.new_tile_writer().unwrap();
            w.write(tile0.clone()).unwrap();
            w.write(tile1.clone()).unwrap();
            w.close().unwrap();
        }
        archive.finish(&min_metadata_in()).unwrap();
        archive.close().unwrap();
        assert_eq!(
            vec![wrap_init(), to_entry(&tile0), to_entry(&tile1), wrap_finish(min_metadata_out())],
            read_all_entries(&file)
        );
    }

    #[test]
    fn test_write_to_multiple_files() {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("out.proto");
        let secondary = tmp.path().join("out.proto1");
        let tertiary = tmp.path().join("out.proto2");
        let tiles = [
            tile(11, 12, 1, 0, None),
            tile(21, 22, 2, 1, None),
            tile(31, 32, 3, 2, None),
            tile(41, 42, 4, 3, None),
            tile(51, 52, 5, 4, None),
        ];
        let mut archive =
            WriteableProtoStreamArchive::new_write_to_file(&primary, StreamArchiveConfig::default()).unwrap();
        archive.initialize().unwrap();
        for group in [&tiles[0..2], &tiles[2..4], &tiles[4..5]] {
            let mut w = archive.new_tile_writer().unwrap();
            for t in group {
                w.write(t.clone()).unwrap();
            }
            w.close().unwrap();
        }
        archive.finish(&max_metadata_deserialized()).unwrap();
        archive.close().unwrap();
        assert_eq!(
            vec![wrap_init(), to_entry(&tiles[0]), to_entry(&tiles[1]), wrap_finish(max_metadata_out())],
            read_all_entries(&primary)
        );
        assert_eq!(vec![to_entry(&tiles[2]), to_entry(&tiles[3])], read_all_entries(&secondary));
        assert_eq!(vec![to_entry(&tiles[4])], read_all_entries(&tertiary));
    }
}
