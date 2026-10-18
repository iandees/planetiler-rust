//! Port of `stream/WriteableCsvArchive.java`: writes tile data into a CSV file (or pipe), one `x,y,z,encoded data`
//! line per tile.
//!
//! A simple (not very efficient) upload to S3 using minio client could look as follows:
//!
//! ```text
//! mkfifo /tmp/data/output.csv
//! # now run planetiler with the options --append --output=/tmp/data/output.csv
//!
//! # ... and start a script to upload data
//! #! /bin/bash
//! while IFS="," read -r x y z encoded
//! do
//!   echo "pushing tile z=$z x=$x y=$y"
//!   echo $encoded | base64 -d | aws s3 cp - s3://BUCKET/map/$z/$x/$y.pbf --content-type=application/x-protobuf --content-encoding=gzip
//! done < "${1:-/dev/stdin}"
//! ```
//!
//! See [`StreamArchiveBase`] for how to write to multiple files. This can be used to parallelize uploads.

use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::bail;

use crate::archive::tile_archive_config::Format;
use crate::archive::tile_encoding_result::TileEncodingResult;
use crate::archive::writeable_tile_archive::{TileWriter, WriteableTileArchive};
use crate::geo::tile_order::TileOrder;
use crate::stream::stream_archive_config::StreamArchiveConfig;
use crate::stream::stream_archive_utils::{self, base64_encode, hex_encode};
use crate::stream::writeable_stream_archive::StreamArchiveBase;

pub const OPTION_COLUMN_SEPARATOR: &str = "column_separator";
pub const OPTION_LINE_SEPARATOR: &str = "line_separator";
pub const OPTION_BINARY_ENCODING: &str = "binary_encoding";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BinaryEncoding {
    Base64,
    Hex,
}

impl BinaryEncoding {
    fn ids() -> &'static [&'static str] {
        &["base64", "hex"]
    }

    fn from_id(id: &str) -> anyhow::Result<BinaryEncoding> {
        match id {
            "base64" => Ok(BinaryEncoding::Base64),
            "hex" => Ok(BinaryEncoding::Hex),
            other => bail!("unexpected binary encoding - expected one of {:?} but got {other}", Self::ids()),
        }
    }

    fn encode(&self, data: &[u8]) -> String {
        match self {
            BinaryEncoding::Base64 => base64_encode(data),
            BinaryEncoding::Hex => hex_encode(data),
        }
    }
}

pub struct WriteableCsvArchive {
    base: StreamArchiveBase,
    column_separator: String,
    line_separator: String,
    binary_encoding: BinaryEncoding,
}

impl WriteableCsvArchive {
    pub fn new_write_to_file(
        format: Format,
        path: &Path,
        config: StreamArchiveConfig,
    ) -> anyhow::Result<WriteableCsvArchive> {
        let default_column_separator = match format {
            Format::Csv => "','",
            Format::Tsv => "'\\t'",
            other => bail!("supported formats are csv and tsv but got {}", other.id()),
        };
        let options = &config.more_options;
        let column_separator = stream_archive_utils::get_escaped_string(
            options,
            format,
            OPTION_COLUMN_SEPARATOR,
            "column separator",
            default_column_separator,
            &[",", " "],
        );
        let line_separator = stream_archive_utils::get_escaped_string(
            options,
            format,
            OPTION_LINE_SEPARATOR,
            "line separator",
            "'\\n'",
            &["\n", "\r\n"],
        );
        let binary_encoding = BinaryEncoding::from_id(&options.get_string(
            OPTION_BINARY_ENCODING,
            &format!("binary (tile) data encoding - one of {:?}", BinaryEncoding::ids()),
            "base64",
        ))?;
        Ok(WriteableCsvArchive {
            base: StreamArchiveBase::new(path, config)?,
            column_separator,
            line_separator,
            binary_encoding,
        })
    }
}

impl WriteableTileArchive for WriteableCsvArchive {
    fn deduplicates(&self) -> bool {
        false
    }

    fn tile_order(&self) -> TileOrder {
        TileOrder::Tms
    }

    fn new_tile_writer(&self) -> anyhow::Result<Box<dyn TileWriter + Send + '_>> {
        Ok(Box::new(CsvTileWriter {
            writer: BufWriter::new(self.base.next_output_stream()?),
            column_separator: &self.column_separator,
            line_separator: &self.line_separator,
            encoding: self.binary_encoding,
        }))
    }

    fn bytes_written(&self) -> u64 {
        self.base.bytes_written()
    }

    fn close(&mut self) -> anyhow::Result<()> {
        self.base.close()
    }
}

struct CsvTileWriter<'a> {
    writer: BufWriter<Box<dyn Write + Send>>,
    column_separator: &'a str,
    line_separator: &'a str,
    encoding: BinaryEncoding,
}

impl TileWriter for CsvTileWriter<'_> {
    fn write(&mut self, encoding_result: TileEncodingResult) -> anyhow::Result<()> {
        let coord = encoding_result.coord;
        let encoded = self.encoding.encode(&encoding_result.tile_data);
        let sep = self.column_separator;
        // x | y | z | encoded data
        write!(self.writer, "{}{sep}{}{sep}{}{sep}{encoded}{}", coord.x, coord.y, coord.z, self.line_separator)?;
        Ok(())
    }

    fn close(&mut self) -> anyhow::Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::tile_archive_metadata::TileArchiveMetadata;
    use crate::config::arguments::Arguments;
    use crate::geo::tile_coord::TileCoord;

    fn tile(x: u32, y: u32, z: u32, data: &[u8]) -> TileEncodingResult {
        TileEncodingResult::new(TileCoord::of_xyz(x, y, z), data.to_vec(), None)
    }

    fn default_metadata() -> TileArchiveMetadata {
        TileArchiveMetadata { name: Some("start".into()), ..Default::default() }
    }

    fn list(dir: &Path) -> std::collections::BTreeSet<std::path::PathBuf> {
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect()
    }

    #[test]
    fn test_write_to_single_file() {
        for format in [Format::Csv, Format::Tsv] {
            let tmp = tempfile::tempdir().unwrap();
            let file = tmp.path().join("out.csv");
            let mut archive =
                WriteableCsvArchive::new_write_to_file(format, &file, StreamArchiveConfig::default()).unwrap();
            archive.initialize().unwrap();
            {
                let mut w = archive.new_tile_writer().unwrap();
                w.write(tile(0, 0, 0, &[0])).unwrap();
                w.write(TileEncodingResult::new(TileCoord::of_xyz(1, 2, 3), vec![1], Some(1))).unwrap();
                w.close().unwrap();
            }
            archive.finish(&default_metadata()).unwrap();
            archive.close().unwrap();
            let expected = match format {
                Format::Csv => "0,0,0,AA==\n1,2,3,AQ==\n",
                _ => "0\t0\t0\tAA==\n1\t2\t3\tAQ==\n",
            };
            assert_eq!(expected, std::fs::read_to_string(&file).unwrap());
            assert_eq!(std::collections::BTreeSet::from([file]), list(tmp.path()));
        }
    }

    #[test]
    fn test_write_to_multiple_files() {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("out.csv");
        let secondary = tmp.path().join("out.csv1");
        let tertiary = tmp.path().join("out.csv2");
        let mut archive =
            WriteableCsvArchive::new_write_to_file(Format::Csv, &primary, StreamArchiveConfig::default()).unwrap();
        archive.initialize().unwrap();
        {
            let mut w = archive.new_tile_writer().unwrap();
            w.write(tile(11, 12, 1, &[0])).unwrap();
            w.write(tile(21, 22, 2, &[1])).unwrap();
            w.close().unwrap();
            let mut w = archive.new_tile_writer().unwrap();
            w.write(tile(31, 32, 3, &[2])).unwrap();
            w.write(tile(41, 42, 4, &[3])).unwrap();
            w.close().unwrap();
            let mut w = archive.new_tile_writer().unwrap();
            w.write(tile(51, 55, 5, &[4])).unwrap();
            w.close().unwrap();
        }
        archive.finish(&default_metadata()).unwrap();
        archive.close().unwrap();
        assert_eq!("11,12,1,AA==\n21,22,2,AQ==\n", std::fs::read_to_string(&primary).unwrap());
        assert_eq!("31,32,3,Ag==\n41,42,4,Aw==\n", std::fs::read_to_string(&secondary).unwrap());
        assert_eq!("51,55,5,BA==\n", std::fs::read_to_string(&tertiary).unwrap());
        assert_eq!(std::collections::BTreeSet::from([primary, secondary, tertiary]), list(tmp.path()));
    }

    fn test_tile_options(options: &[(&str, &str)], expected: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("out.csv");
        let config = StreamArchiveConfig::new(false, Arguments::of_pairs(options.iter().copied()));
        let mut archive = WriteableCsvArchive::new_write_to_file(Format::Csv, &file, config).unwrap();
        archive.initialize().unwrap();
        {
            let mut w = archive.new_tile_writer().unwrap();
            w.write(tile(0, 0, 0, &[0, 1])).unwrap();
            w.write(tile(1, 1, 1, &[2, 3])).unwrap();
            w.close().unwrap();
        }
        archive.finish(&default_metadata()).unwrap();
        archive.close().unwrap();
        assert_eq!(expected, std::fs::read_to_string(&file).unwrap());
        assert_eq!(std::collections::BTreeSet::from([file]), list(tmp.path()));
    }

    #[test]
    fn test_column_separator() {
        test_tile_options(&[(OPTION_COLUMN_SEPARATOR, "' '")], "0 0 0 AAE=\n1 1 1 AgM=\n");
    }

    #[test]
    fn test_line_separator() {
        test_tile_options(&[(OPTION_LINE_SEPARATOR, "'\\r'")], "0,0,0,AAE=\r1,1,1,AgM=\r");
    }

    #[test]
    fn test_hex_encoding() {
        test_tile_options(&[(OPTION_BINARY_ENCODING, "hex")], "0,0,0,0001\n1,1,1,0203\n");
    }
}
