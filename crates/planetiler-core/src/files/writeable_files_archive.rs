//! Port of `files/WriteableFilesArchive.java`: writes tiles as separate files. The default tile scheme is
//! `z/x/y.pbf`.
//!
//! Supported arguments:
//! - `(files_)tile_scheme`: the tile scheme e.g. `{x}/{y}/{z}.pbf`. The default is `{z}/{x}/{y}.pbf`. See
//!   [`TileSchemeEncoding`] for more details.
//! - `(files_)metadata_path`: the path the meta data should be written to. The default is `BASEPATH/metadata.json`.
//!   `"none"` can be used to suppress writing metadata.
//!
//! Usages:
//! ```text
//! --output=/path/to/tiles/ --files_tile_scheme={z}/{x}/{y}.pbf --files_metadata_path=/some/other/path/metadata.json
//! --output=/path/to/tiles/{z}/{x}/{y}.pbf
//! --output=/path/to/tiles?format=files&tile_scheme={z}/{x}/{y}.pbf
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, bail};
use tracing::info;

use crate::archive::tile_archive_config::ArchiveOptions;
use crate::archive::tile_archive_metadata::TileArchiveMetadata;
use crate::archive::tile_archive_metadata_de_ser;
use crate::archive::tile_encoding_result::TileEncodingResult;
use crate::archive::writeable_tile_archive::{TileWriter, WriteableTileArchive};
use crate::files::files_archive_utils;
use crate::files::tile_scheme_encoding::TileSchemeEncoding;
use crate::geo::tile_order::TileOrder;

pub struct WriteableFilesArchive {
    bytes_written: Arc<AtomicU64>,
    base_path: PathBuf,
    metadata_path: Option<PathBuf>,
    encoding: TileSchemeEncoding,
    tile_order: TileOrder,
}

fn create_validate_directory(p: &Path) -> anyhow::Result<()> {
    if !p.exists() {
        std::fs::create_dir_all(p).with_context(|| format!("failed to create {}", p.display()))?;
    }
    if !p.is_dir() {
        bail!("require \"{}\" to be a directory", p.display());
    }
    Ok(())
}

impl WriteableFilesArchive {
    pub fn new_writer(
        base_path: &Path,
        options: &ArchiveOptions,
        overwrite_metadata: bool,
    ) -> anyhow::Result<WriteableFilesArchive> {
        let path_and_scheme = files_archive_utils::base_path_with_tile_scheme_encoding(options, base_path)?;
        let base_path = path_and_scheme.base_path;
        info!("using {} as base files archive path", base_path.display());
        create_validate_directory(&base_path)?;
        let metadata_path = files_archive_utils::metadata_path(&base_path, options);
        if let Some(p) = &metadata_path
            && p.exists()
        {
            if !overwrite_metadata {
                bail!("{} already exists", p.display());
            } else if !p.is_file() {
                bail!("require {} to be a regular file", p.display());
            }
        }
        let encoding = path_and_scheme.tile_scheme_encoding;
        Ok(WriteableFilesArchive {
            bytes_written: Arc::new(AtomicU64::new(0)),
            base_path,
            metadata_path,
            tile_order: encoding.preferred_tile_order(),
            encoding,
        })
    }
}

impl WriteableTileArchive for WriteableFilesArchive {
    fn deduplicates(&self) -> bool {
        false
    }

    fn tile_order(&self) -> TileOrder {
        self.tile_order
    }

    fn new_tile_writer(&self) -> anyhow::Result<Box<dyn TileWriter + Send + '_>> {
        Ok(Box::new(TileFilesWriter {
            encoding: &self.encoding,
            last_checked_folder: self.base_path.clone(),
            bytes_written: &self.bytes_written,
        }))
    }

    fn finish(&mut self, metadata: &TileArchiveMetadata) -> anyhow::Result<()> {
        if let Some(p) = &self.metadata_path {
            let json = tile_archive_metadata_de_ser::to_json_string(metadata);
            std::fs::write(p, &json).with_context(|| format!("failed to write {}", p.display()))?;
            self.bytes_written.fetch_add(json.len() as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }
}

struct TileFilesWriter<'a> {
    encoding: &'a TileSchemeEncoding,
    last_checked_folder: PathBuf,
    bytes_written: &'a AtomicU64,
}

impl TileWriter for TileFilesWriter<'_> {
    fn write(&mut self, encoding_result: TileEncodingResult) -> anyhow::Result<()> {
        let data = &encoding_result.tile_data;
        let file = self.encoding.encode(&encoding_result.coord);
        let folder = file.parent().map(Path::to_path_buf).unwrap_or_default();
        // tiny optimization in order to avoid too many unnecessary "folder-exists-checks" (I/O)
        // only effective when the tileScheme is z/x/y but doesn't really harm otherwise
        if self.last_checked_folder != folder && !folder.exists() {
            std::fs::create_dir_all(&folder).with_context(|| format!("failed to create {}", folder.display()))?;
        }
        self.last_checked_folder = folder;
        std::fs::write(&file, data).with_context(|| format!("failed to write {}", file.display()))?;
        self.bytes_written.fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn close(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::test_fixtures::*;
    use crate::files::files_archive_utils::{OPTION_METADATA_PATH, OPTION_TILE_SCHEME};
    use crate::geo::tile_coord::TileCoord;

    fn tile(x: u32, y: u32, z: u32, data: u8, hash: Option<u64>) -> TileEncodingResult {
        TileEncodingResult::new(TileCoord::of_xyz(x, y, z), vec![data], hash)
    }

    fn list_files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        fn rec(dir: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() { rec(&p, out) } else { out.push(p) }
            }
        }
        rec(dir, &mut out);
        out.sort();
        out
    }

    #[test]
    fn test_write() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        let mut archive = WriteableFilesArchive::new_writer(&tiles_dir, &ArchiveOptions::empty(), false).unwrap();
        archive.initialize().unwrap();
        {
            let mut writer = archive.new_tile_writer().unwrap();
            writer.write(tile(0, 0, 0, 0, None)).unwrap();
            writer.write(tile(1, 2, 3, 1, Some(1))).unwrap();
            writer.write(tile(1, 3, 3, 2, Some(2))).unwrap();
            writer.write(tile(1, 3, 4, 3, Some(3))).unwrap();
            writer.close().unwrap();
        }
        archive.finish(&max_metadata_deserialized()).unwrap();
        let files = list_files(&tiles_dir);
        let relative: Vec<PathBuf> = files.iter().map(|f| f.strip_prefix(&tiles_dir).unwrap().to_path_buf()).collect();
        assert_eq!(
            ["0/0/0.pbf", "3/1/2.pbf", "3/1/3.pbf", "4/1/3.pbf", "metadata.json"].map(PathBuf::from).to_vec(),
            relative
        );
        for (i, file) in files.iter().take(4).enumerate() {
            assert_eq!(vec![i as u8], std::fs::read(file).unwrap());
        }
        assert_same_json(MAX_METADATA_SERIALIZED, &std::fs::read_to_string(&files[4]).unwrap());
    }

    #[test]
    fn test_write_custom_scheme() {
        for (scheme, expected) in [
            ("{z}/{x}/{y}.pbf", "3/1/2.pbf"),
            ("{x}/{y}/{z}.pbf", "1/2/3.pbf"),
            ("{x}-{y}-{z}.pbf", "1-2-3.pbf"),
            ("{x}/a/{y}/b{z}.pbf", "1/a/2/b3.pbf"),
            ("{z}/{x}/{y}.pbf.gz", "3/1/2.pbf.gz"),
            ("{z}/{xs}/{ys}.pbf", "3/000/001/000/002.pbf"),
            ("{z}/{x}/{ys}.pbf", "3/1/000/002.pbf"),
            ("{z}/{xs}/{y}.pbf", "3/000/001/2.pbf"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let tiles_dir = tmp.path().join("tiles");
            let options = ArchiveOptions::of_pairs(&[(OPTION_TILE_SCHEME, scheme)]);
            let archive = WriteableFilesArchive::new_writer(&tiles_dir, &options, false).unwrap();
            let mut writer = archive.new_tile_writer().unwrap();
            writer.write(tile(1, 2, 3, 1, None)).unwrap();
            writer.close().unwrap();
            assert!(tiles_dir.join(expected).exists(), "{scheme}");
        }
    }

    #[test]
    fn test_tile_scheme_from_base_path() {
        for (shortcut, actual, file) in [
            ("{z}/{x}/{y}.pbf", "", "3/1/2.pbf"),
            ("tiles/{z}/{x}/{y}.pbf", "tiles", "tiles/3/1/2.pbf"),
            ("tiles/z{z}/{x}/{y}.pbf", "tiles", "tiles/z3/1/2.pbf"),
            ("z{z}/x{x}/y{y}.pbf", "", "z3/x1/y2.pbf"),
            ("tiles/tile-{z}-{x}-{y}.pbf", "tiles", "tiles/tile-3-1-2.pbf"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let test_base = tmp.path().join("tiles");
            let mut archive =
                WriteableFilesArchive::new_writer(&test_base.join(shortcut), &ArchiveOptions::empty(), false).unwrap();
            {
                let mut writer = archive.new_tile_writer().unwrap();
                writer.write(tile(1, 2, 3, 1, None)).unwrap();
                writer.close().unwrap();
            }
            archive.finish(&max_metadata_deserialized()).unwrap();
            assert!(test_base.join(file).exists());
            assert!(test_base.join(actual).join("metadata.json").exists());
        }
    }

    fn test_metadata_write(options: ArchiveOptions, output: &Path, metadata_path: &Path) {
        let mut archive = WriteableFilesArchive::new_writer(output, &options, false).unwrap();
        archive.initialize().unwrap();
        archive.finish(&max_metadata_deserialized()).unwrap();
        assert!(metadata_path.exists());
        assert_same_json(MAX_METADATA_SERIALIZED, &std::fs::read_to_string(metadata_path).unwrap());
    }

    #[test]
    fn test_metadata_write_default() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        test_metadata_write(ArchiveOptions::empty(), &tiles_dir, &tiles_dir.join("metadata.json"));
    }

    #[test]
    fn test_metadata_write_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        test_metadata_write(
            ArchiveOptions::of_pairs(&[(OPTION_METADATA_PATH, "x.y")]),
            &tiles_dir,
            &tiles_dir.join("x.y"),
        );
    }

    #[test]
    fn test_metadata_write_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        std::fs::create_dir(tmp.path().join("abs")).unwrap();
        let p = tmp.path().join("abs").join("abc.json");
        test_metadata_write(ArchiveOptions::of_pairs(&[(OPTION_METADATA_PATH, &p.to_string_lossy())]), &tiles_dir, &p);
    }

    #[test]
    fn test_metadata_write_none() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        let options = ArchiveOptions::of_pairs(&[(OPTION_METADATA_PATH, "none")]);
        let mut archive = WriteableFilesArchive::new_writer(&tiles_dir, &options, false).unwrap();
        archive.initialize().unwrap();
        archive.finish(&max_metadata_deserialized()).unwrap();
        assert!(list_files(&tiles_dir).is_empty());
    }

    #[test]
    fn test_metadata_fails_if_not_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        let options = ArchiveOptions::of_pairs(&[(OPTION_METADATA_PATH, &tiles_dir.to_string_lossy())]);
        assert!(WriteableFilesArchive::new_writer(&tiles_dir, &options, false).is_err());
    }

    #[test]
    fn test_metadata_overwrite_off() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        std::fs::create_dir(&tiles_dir).unwrap();
        std::fs::write(tiles_dir.join("metadata.json"), "something").unwrap();
        assert!(WriteableFilesArchive::new_writer(&tiles_dir, &ArchiveOptions::empty(), false).is_err());
    }

    #[test]
    fn test_metadata_overwrite_on() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        let metadata_path = tiles_dir.join("metadata.json");
        std::fs::create_dir(&tiles_dir).unwrap();
        std::fs::write(&metadata_path, "something").unwrap();
        let mut archive = WriteableFilesArchive::new_writer(&tiles_dir, &ArchiveOptions::empty(), true).unwrap();
        archive.initialize().unwrap();
        archive.finish(&max_metadata_deserialized()).unwrap();
        assert_same_json(MAX_METADATA_SERIALIZED, &std::fs::read_to_string(&metadata_path).unwrap());
    }

    #[test]
    fn test_creates_path_if_not_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        let archive = WriteableFilesArchive::new_writer(&tiles_dir, &ArchiveOptions::empty(), false).unwrap();
        let mut writer = archive.new_tile_writer().unwrap();
        writer.write(tile(0, 0, 0, 0, None)).unwrap();
        writer.close().unwrap();
        assert!(tiles_dir.join("0/0/0.pbf").is_file());
    }

    #[test]
    fn test_fails_if_base_path_is_no_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        std::fs::write(&tiles_dir, b"").unwrap();
        assert!(WriteableFilesArchive::new_writer(&tiles_dir, &ArchiveOptions::empty(), false).is_err());
    }

    #[test]
    fn test_fails_if_tile_exists_as_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        std::fs::create_dir_all(tiles_dir.join("0/0/0.pbf")).unwrap();
        let archive = WriteableFilesArchive::new_writer(&tiles_dir, &ArchiveOptions::empty(), false).unwrap();
        let mut writer = archive.new_tile_writer().unwrap();
        assert!(writer.write(tile(0, 0, 0, 0, None)).is_err());
    }

    #[test]
    fn test_fails_if_dir_exists_as_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        std::fs::create_dir_all(&tiles_dir).unwrap();
        std::fs::write(tiles_dir.join("0"), b"").unwrap();
        let archive = WriteableFilesArchive::new_writer(&tiles_dir, &ArchiveOptions::empty(), false).unwrap();
        let mut writer = archive.new_tile_writer().unwrap();
        assert!(writer.write(tile(0, 0, 0, 0, None)).is_err());
    }

    #[test]
    fn test_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        std::fs::create_dir_all(&tiles_dir).unwrap();
        let archive = WriteableFilesArchive::new_writer(&tiles_dir, &ArchiveOptions::empty(), false).unwrap();
        assert!(!archive.deduplicates());
        assert_eq!(TileOrder::Tms, archive.tile_order());
    }
}
