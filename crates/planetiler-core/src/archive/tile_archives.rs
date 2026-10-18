//! Port of `archive/TileArchives.java`: utilities for creating [`ReadableTileArchive`] and [`WriteableTileArchive`]
//! instances.

use std::path::Path;

use anyhow::{anyhow, bail};

use crate::archive::readable_tile_archive::ReadableTileArchive;
use crate::archive::tile_archive_config::{ArchiveOptions, Format, TileArchiveConfig};
use crate::archive::writeable_tile_archive::WriteableTileArchive;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::files::readable_files_archive::ReadableFilesArchive;
use crate::files::writeable_files_archive::WriteableFilesArchive;
use crate::mbtiles::mbtiles::{self, Mbtiles};
use crate::pmtiles::readable_pmtiles::ReadablePmtiles;
use crate::pmtiles::writeable_pmtiles::WriteablePmtiles;
use crate::stream::stream_archive_config::StreamArchiveConfig;
use crate::stream::writeable_csv_archive::WriteableCsvArchive;
use crate::stream::writeable_json_stream_archive::WriteableJsonStreamArchive;
use crate::stream::writeable_proto_stream_archive::WriteableProtoStreamArchive;

/// Returns a new [`WriteableTileArchive`] from the string definition in `archive` that will be parsed with
/// [`TileArchiveConfig`].
pub fn new_writer_from_str(archive: &str, config: &PlanetilerConfig) -> anyhow::Result<Box<dyn WriteableTileArchive>> {
    new_writer(&TileArchiveConfig::from(archive)?, config)
}

/// Returns a new [`ReadableTileArchive`] from the string definition in `archive` that will be parsed with
/// [`TileArchiveConfig`].
pub fn new_reader_from_str(archive: &str, config: &PlanetilerConfig) -> anyhow::Result<Box<dyn ReadableTileArchive>> {
    new_reader(&TileArchiveConfig::from(archive)?, config)
}

/// Alias for [`new_reader_from_str`].
pub fn new_reader_from_path(path: &Path, config: &PlanetilerConfig) -> anyhow::Result<Box<dyn ReadableTileArchive>> {
    new_reader_from_str(&path.to_string_lossy(), config)
}

/// Alias for [`new_writer_from_str`].
pub fn new_writer_from_path(path: &Path, config: &PlanetilerConfig) -> anyhow::Result<Box<dyn WriteableTileArchive>> {
    new_writer_from_str(&path.to_string_lossy(), config)
}

fn local_path(archive: &TileArchiveConfig) -> anyhow::Result<std::path::PathBuf> {
    archive.get_local_path().ok_or_else(|| anyhow!("{} is not a local path", archive.uri))
}

/// Returns a new [`WriteableTileArchive`] for `archive`.
pub fn new_writer(
    archive: &TileArchiveConfig,
    config: &PlanetilerConfig,
) -> anyhow::Result<Box<dyn WriteableTileArchive>> {
    let options = archive.apply_fallbacks(&config.arguments);
    let path = local_path(archive)?;
    Ok(match archive.format {
        // pass-through legacy arguments for fallback
        Format::Mbtiles => Box::new(Mbtiles::new_write_to_file_database(
            &path,
            &options.or_else(ArchiveOptions::from_arguments_subset(
                config.arguments.clone(),
                &[mbtiles::LEGACY_VACUUM_ANALYZE, mbtiles::LEGACY_COMPACT_DB, mbtiles::LEGACY_SKIP_INDEX_CREATION],
            )),
        )?),
        Format::Pmtiles => Box::new(WriteablePmtiles::new_write_to_file(&path)?),
        format @ (Format::Csv | Format::Tsv) => Box::new(WriteableCsvArchive::new_write_to_file(
            format,
            &path,
            StreamArchiveConfig::from_config(config, options),
        )?),
        Format::Proto | Format::Pbf => Box::new(WriteableProtoStreamArchive::new_write_to_file(
            &path,
            StreamArchiveConfig::from_config(config, options),
        )?),
        Format::Json => Box::new(WriteableJsonStreamArchive::new_write_to_file(
            &path,
            StreamArchiveConfig::from_config(config, options),
        )?),
        Format::Files => Box::new(WriteableFilesArchive::new_writer(&path, &options, config.force || config.append)?),
    })
}

/// Returns a new [`ReadableTileArchive`] for `archive`.
pub fn new_reader(
    archive: &TileArchiveConfig,
    config: &PlanetilerConfig,
) -> anyhow::Result<Box<dyn ReadableTileArchive>> {
    let options = archive.apply_fallbacks(&config.arguments);
    let path = local_path(archive)?;
    Ok(match archive.format {
        Format::Mbtiles => Box::new(Mbtiles::new_read_only_database_with_options(&path, &options)?),
        Format::Pmtiles => Box::new(ReadablePmtiles::new_read_from_file(&path)?),
        Format::Csv | Format::Tsv => bail!("reading CSV is not supported"),
        Format::Proto | Format::Pbf => bail!("reading PROTO is not supported"),
        Format::Json => bail!("reading JSON is not supported"),
        Format::Files => Box::new(ReadableFilesArchive::new_reader(&path, &options)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::tile_archive_metadata::TileArchiveMetadata;
    use crate::archive::tile_compression::TileCompression;
    use crate::archive::tile_encoding_result::TileEncodingResult;
    use crate::geo::tile_coord::TileCoord;

    #[test]
    fn test_round_trip_all_readable_formats() {
        let config = PlanetilerConfig::defaults();
        for name in ["out.mbtiles", "out.pmtiles", "out/", "out.mbtiles?compact=false"] {
            let tmp = tempfile::tempdir().unwrap();
            let archive = format!("{}/{name}", tmp.path().display());
            let mut writer = new_writer_from_str(&archive, &config).unwrap();
            writer.initialize().unwrap();
            {
                let mut w = writer.new_tile_writer().unwrap();
                w.write(TileEncodingResult::new(TileCoord::of_xyz(0, 0, 0), vec![1], Some(1))).unwrap();
                w.write(TileEncodingResult::new(TileCoord::of_xyz(0, 0, 1), vec![2], Some(2))).unwrap();
                w.close().unwrap();
            }
            let metadata = TileArchiveMetadata {
                name: Some("name".into()),
                tile_compression: Some(TileCompression::Gzip),
                ..Default::default()
            };
            writer.finish(&metadata).unwrap();
            writer.close().unwrap();
            drop(writer);
            let reader = new_reader_from_str(&archive, &config).unwrap();
            assert_eq!(Some(vec![1]), reader.get_tile(0, 0, 0).unwrap(), "{name}");
            assert_eq!(Some(vec![2]), reader.get_tile(0, 0, 1).unwrap(), "{name}");
            assert_eq!(2, reader.get_all_tiles().unwrap().count(), "{name}");
            assert_eq!(Some("name".to_string()), reader.metadata().unwrap().unwrap().name, "{name}");
        }
    }

    #[test]
    fn test_unsupported_readers() {
        let config = PlanetilerConfig::defaults();
        for name in ["out.csv", "out.proto", "out.json"] {
            assert!(new_reader_from_str(name, &config).is_err());
        }
    }
}
