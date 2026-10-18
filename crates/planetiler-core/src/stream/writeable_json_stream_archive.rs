//! Port of `stream/WriteableJsonStreamArchive.java`: writes JSON-serialized tile data as well as meta data into
//! file(s). The entries are separated by newline (by default):
//!
//! ```text
//! {"type":"initialization"}
//! {"type":"tile","x":0,"y":0,"z":0,"encodedData":"AA=="}
//! {"type":"finish","metadata":{...}}
//! ```

use std::io::{BufWriter, Write};
use std::path::Path;

use serde::Serialize;

use crate::archive::tile_archive_config::Format;
use crate::archive::tile_archive_metadata::TileArchiveMetadata;
use crate::archive::tile_encoding_result::TileEncodingResult;
use crate::archive::writeable_tile_archive::{TileWriter, WriteableTileArchive};
use crate::geo::tile_order::TileOrder;
use crate::stream::stream_archive_config::StreamArchiveConfig;
use crate::stream::stream_archive_utils::{self, base64_encode};
use crate::stream::writeable_stream_archive::StreamArchiveBase;

/// exposing meta data (non-tile data) might be useful for most use cases but complicates parsing for simple use
/// cases => allow to output tiles, only
const OPTION_WRITE_TILES_ONLY: &str = "tiles_only";
const OPTION_ROOT_VALUE_SEPARATOR: &str = "root_value_separator";

#[derive(Serialize)]
#[serde(tag = "type")]
enum Entry<'a> {
    #[serde(rename = "tile")]
    Tile {
        x: u32,
        y: u32,
        z: u32,
        #[serde(rename = "encodedData")]
        encoded_data: String,
    },
    #[serde(rename = "initialization")]
    Initialization,
    #[serde(rename = "finish")]
    Finish { metadata: &'a TileArchiveMetadata },
}

pub struct WriteableJsonStreamArchive {
    base: StreamArchiveBase,
    write_tiles_only: bool,
    root_value_separator: String,
}

impl WriteableJsonStreamArchive {
    pub fn new_write_to_file(path: &Path, config: StreamArchiveConfig) -> anyhow::Result<WriteableJsonStreamArchive> {
        let options = &config.more_options;
        let write_tiles_only = options.get_boolean(OPTION_WRITE_TILES_ONLY, "write tiles, only", false);
        let root_value_separator = stream_archive_utils::get_escaped_string(
            options,
            Format::Json,
            OPTION_ROOT_VALUE_SEPARATOR,
            "root value separator",
            "'\\n'",
            &["\n", " "],
        );
        Ok(WriteableJsonStreamArchive {
            base: StreamArchiveBase::new(path, config)?,
            write_tiles_only,
            root_value_separator,
        })
    }

    fn write_entry_flush(&self, entry: &Entry) -> anyhow::Result<()> {
        let mut out = self.base.primary_output_stream();
        let mut bytes = serde_json::to_vec(entry)?;
        bytes.extend_from_slice(self.root_value_separator.as_bytes());
        out.write_all(&bytes)?;
        out.flush()?;
        Ok(())
    }
}

impl WriteableTileArchive for WriteableJsonStreamArchive {
    fn deduplicates(&self) -> bool {
        false
    }

    fn tile_order(&self) -> TileOrder {
        TileOrder::Tms
    }

    fn initialize(&mut self) -> anyhow::Result<()> {
        if self.write_tiles_only {
            return Ok(());
        }
        self.write_entry_flush(&Entry::Initialization)
    }

    fn new_tile_writer(&self) -> anyhow::Result<Box<dyn TileWriter + Send + '_>> {
        Ok(Box::new(JsonTileWriter {
            writer: BufWriter::new(self.base.next_output_stream()?),
            root_value_separator: &self.root_value_separator,
            first: true,
        }))
    }

    fn finish(&mut self, metadata: &TileArchiveMetadata) -> anyhow::Result<()> {
        if self.write_tiles_only {
            return Ok(());
        }
        self.write_entry_flush(&Entry::Finish { metadata })
    }

    fn bytes_written(&self) -> u64 {
        self.base.bytes_written()
    }

    fn close(&mut self) -> anyhow::Result<()> {
        self.base.close()
    }
}

struct JsonTileWriter<'a> {
    writer: BufWriter<Box<dyn Write + Send>>,
    root_value_separator: &'a str,
    first: bool,
}

impl TileWriter for JsonTileWriter<'_> {
    fn write(&mut self, encoding_result: TileEncodingResult) -> anyhow::Result<()> {
        let coord = encoding_result.coord;
        if !self.first {
            self.writer.write_all(self.root_value_separator.as_bytes())?;
        }
        self.first = false;
        let entry =
            Entry::Tile { x: coord.x, y: coord.y, z: coord.z, encoded_data: base64_encode(&encoding_result.tile_data) };
        serde_json::to_writer(&mut self.writer, &entry)?;
        Ok(())
    }

    fn close(&mut self) -> anyhow::Result<()> {
        // jackson only handles separators between entries but does not append one to the last one
        self.writer.write_all(self.root_value_separator.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::test_fixtures::*;
    use crate::config::arguments::Arguments;
    use crate::geo::tile_coord::TileCoord;
    use serde_json::Value;

    fn tile(x: u32, y: u32, z: u32, data: u8) -> TileEncodingResult {
        TileEncodingResult::new(TileCoord::of_xyz(x, y, z), vec![data], None)
    }

    fn read_delimited(json: &str) -> Vec<Value> {
        serde_json::Deserializer::from_str(json).into_iter::<Value>().map(|v| v.unwrap()).collect()
    }

    fn assert_eq_delimited(expected: &str, actual: &str) {
        assert_eq!(read_delimited(expected), read_delimited(actual));
    }

    fn list(dir: &Path) -> std::collections::BTreeSet<std::path::PathBuf> {
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect()
    }

    #[test]
    fn test_write_to_single_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("out.json");
        let mut archive = WriteableJsonStreamArchive::new_write_to_file(&file, StreamArchiveConfig::default()).unwrap();
        archive.initialize().unwrap();
        {
            let mut w = archive.new_tile_writer().unwrap();
            w.write(tile(0, 0, 0, 0)).unwrap();
            w.write(TileEncodingResult::new(TileCoord::of_xyz(1, 2, 3), vec![1], Some(1))).unwrap();
            w.close().unwrap();
        }
        archive.finish(&min_metadata_deserialized()).unwrap();
        archive.close().unwrap();
        assert_eq_delimited(
            &format!(
                "{{\"type\":\"initialization\"}}\n{{\"type\":\"tile\",\"x\":0,\"y\":0,\"z\":0,\"encodedData\":\"AA==\"}}\n\
                 {{\"type\":\"tile\",\"x\":1,\"y\":2,\"z\":3,\"encodedData\":\"AQ==\"}}\n\
                 {{\"type\":\"finish\",\"metadata\":{MIN_METADATA_SERIALIZED}}}\n"
            ),
            &std::fs::read_to_string(&file).unwrap(),
        );
        assert_eq!(std::collections::BTreeSet::from([file]), list(tmp.path()));
    }

    #[test]
    fn test_write_to_multiple_files() {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("out.json");
        let secondary = tmp.path().join("out.json1");
        let tertiary = tmp.path().join("out.json2");
        let mut archive =
            WriteableJsonStreamArchive::new_write_to_file(&primary, StreamArchiveConfig::default()).unwrap();
        archive.initialize().unwrap();
        {
            let mut w = archive.new_tile_writer().unwrap();
            w.write(tile(11, 12, 1, 0)).unwrap();
            w.write(tile(21, 22, 2, 1)).unwrap();
            w.close().unwrap();
            let mut w = archive.new_tile_writer().unwrap();
            w.write(tile(31, 32, 3, 2)).unwrap();
            w.write(tile(41, 42, 4, 3)).unwrap();
            w.close().unwrap();
            let mut w = archive.new_tile_writer().unwrap();
            w.write(tile(51, 52, 5, 4)).unwrap();
            w.close().unwrap();
        }
        archive.finish(&max_metadata_deserialized()).unwrap();
        archive.close().unwrap();
        let primary_content = std::fs::read_to_string(&primary).unwrap();
        assert_eq_delimited(
            &format!(
                "{{\"type\":\"initialization\"}}\n{{\"type\":\"tile\",\"x\":11,\"y\":12,\"z\":1,\"encodedData\":\"AA==\"}}\n\
                 {{\"type\":\"tile\",\"x\":21,\"y\":22,\"z\":2,\"encodedData\":\"AQ==\"}}\n\
                 {{\"type\":\"finish\",\"metadata\":{MAX_METADATA_SERIALIZED}}}\n"
            ),
            &primary_content,
        );
        // the escaped json in metadata compares as a string; check it semantically too
        let finish = read_delimited(&primary_content).pop().unwrap();
        assert_same_json(MAX_METADATA_SERIALIZED, &finish["metadata"].to_string());
        assert_eq_delimited(
            "{\"type\":\"tile\",\"x\":31,\"y\":32,\"z\":3,\"encodedData\":\"Ag==\"}\n\
             {\"type\":\"tile\",\"x\":41,\"y\":42,\"z\":4,\"encodedData\":\"Aw==\"}\n",
            &std::fs::read_to_string(&secondary).unwrap(),
        );
        assert_eq_delimited(
            "{\"type\":\"tile\",\"x\":51,\"y\":52,\"z\":5,\"encodedData\":\"BA==\"}\n",
            &std::fs::read_to_string(&tertiary).unwrap(),
        );
        assert_eq!(std::collections::BTreeSet::from([primary, secondary, tertiary]), list(tmp.path()));
    }

    fn test_tile_options(options: &[(&str, &str)], expected: &str) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("out.json");
        let config = StreamArchiveConfig::new(false, Arguments::of_pairs(options.iter().copied()));
        let mut archive = WriteableJsonStreamArchive::new_write_to_file(&file, config).unwrap();
        archive.initialize().unwrap();
        {
            let mut w = archive.new_tile_writer().unwrap();
            w.write(tile(0, 0, 0, 0)).unwrap();
            w.write(tile(1, 2, 3, 1)).unwrap();
            w.close().unwrap();
        }
        archive.finish(&max_metadata_deserialized()).unwrap();
        archive.close().unwrap();
        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq_delimited(expected, &content);
        assert_eq!(std::collections::BTreeSet::from([file]), list(tmp.path()));
        content
    }

    #[test]
    fn test_tiles_only() {
        test_tile_options(
            &[("tiles_only", "true")],
            "{\"type\":\"tile\",\"x\":0,\"y\":0,\"z\":0,\"encodedData\":\"AA==\"}\n\
             {\"type\":\"tile\",\"x\":1,\"y\":2,\"z\":3,\"encodedData\":\"AQ==\"}\n",
        );
    }

    #[test]
    fn test_root_value_separator() {
        let expected = format!(
            "{{\"type\":\"initialization\"}} {{\"type\":\"tile\",\"x\":0,\"y\":0,\"z\":0,\"encodedData\":\"AA==\"}} \
             {{\"type\":\"tile\",\"x\":1,\"y\":2,\"z\":3,\"encodedData\":\"AQ==\"}} \
             {{\"type\":\"finish\",\"metadata\":{MAX_METADATA_SERIALIZED}}} "
        );
        let content = test_tile_options(&[("root_value_separator", "' '")], &expected);
        assert!(!content.contains('\n'));
    }
}
