//! Port of `files/ReadableFilesArchive.java`: reads tiles from a folder structure (e.g. `BASEPATH/{z}/{x}/{y}.pbf`).
//! Counterpart to [`WriteableFilesArchive`](crate::files::writeable_files_archive::WriteableFilesArchive).
//!
//! Supported arguments:
//! - `(files_)tile_scheme`: the tile scheme e.g. `{x}/{y}/{z}.pbf`. The default is `{z}/{x}/{y}.pbf`. See
//!   [`TileSchemeEncoding`] for more details.
//! - `(files_)metadata_path`: the path the meta data should be read from. The default is `BASEPATH/metadata.json`.
//!   `"none"` can be used to suppress reading metadata.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use tracing::info;

use crate::archive::readable_tile_archive::{ArchiveIter, ReadableTileArchive};
use crate::archive::tile_archive_config::ArchiveOptions;
use crate::archive::tile_archive_metadata::TileArchiveMetadata;
use crate::archive::tile_archive_metadata_de_ser;
use crate::files::files_archive_utils;
use crate::files::tile_scheme_encoding::{TileSchemeDecoder, TileSchemeEncoding};
use crate::geo::tile_coord::TileCoord;

pub struct ReadableFilesArchive {
    base_path: PathBuf,
    metadata_path: Option<PathBuf>,
    encoding: TileSchemeEncoding,
    decoder: TileSchemeDecoder,
    search_depth: usize,
}

fn find_files(dir: &Path, depth_remaining: usize, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if depth_remaining == 0 {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            find_files(&path, depth_remaining - 1, out)?;
        } else if file_type.is_file() || (file_type.is_symlink() && path.is_file()) {
            out.push(path);
        }
    }
    Ok(())
}

impl ReadableFilesArchive {
    pub fn new_reader(base_path: &Path, options: &ArchiveOptions) -> anyhow::Result<ReadableFilesArchive> {
        let path_and_scheme = files_archive_utils::base_path_with_tile_scheme_encoding(options, base_path)?;
        let base_path = path_and_scheme.base_path;
        info!("using {} as base files archive path", base_path.display());
        if !base_path.is_dir() {
            bail!("require \"{}\" to be an existing directory", base_path.display());
        }
        let metadata_path = files_archive_utils::metadata_path(&base_path, options);
        let encoding = path_and_scheme.tile_scheme_encoding;
        Ok(ReadableFilesArchive {
            base_path,
            metadata_path,
            decoder: encoding.decoder(),
            search_depth: encoding.search_depth(),
            encoding,
        })
    }
}

impl ReadableTileArchive for ReadableFilesArchive {
    fn get_tile(&self, x: u32, y: u32, z: u32) -> anyhow::Result<Option<Vec<u8>>> {
        let absolute = self.encoding.encode(&TileCoord::of_xyz(x, y, z));
        if !absolute.exists() {
            return Ok(None);
        }
        Ok(Some(std::fs::read(&absolute).with_context(|| format!("failed to read {}", absolute.display()))?))
    }

    fn get_all_tile_coords(&self) -> anyhow::Result<ArchiveIter<'_, TileCoord>> {
        let mut files = Vec::new();
        find_files(&self.base_path, self.search_depth, &mut files)?;
        Ok(Box::new(files.into_iter().filter_map(move |p| self.decoder.decode(&p)).map(Ok)))
    }

    fn metadata(&self) -> anyhow::Result<Option<TileArchiveMetadata>> {
        match &self.metadata_path {
            Some(p) if p.exists() => {
                let s = std::fs::read_to_string(p)?;
                Ok(Some(tile_archive_metadata_de_ser::from_json_str(&s, false)?))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::test_fixtures::*;
    use crate::archive::tile::Tile;
    use crate::files::files_archive_utils::{OPTION_METADATA_PATH, OPTION_TILE_SCHEME};

    fn write(path: &Path, data: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn test_read() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        let files = [
            "0/0/0.pbf",
            "1/2/3.pbf",
            // invalid
            "9/9",
            "9/x",
            "9/8/9",
            "9/8/9.",
            "9/8/x.pbf",
            "9/b/1.pbf",
            "a/8/1.pbf",
            "9/7.pbf",
            "8.pbf",
        ];
        for (i, f) in files.iter().enumerate() {
            write(&tiles_dir.join(f), &[i as u8]);
        }
        let reader = ReadableFilesArchive::new_reader(&tiles_dir, &ArchiveOptions::empty()).unwrap();
        let mut tiles: Vec<Tile> = reader.get_all_tiles().unwrap().map(|t| t.unwrap()).collect();
        tiles.sort();
        assert_eq!(
            vec![Tile::new(TileCoord::of_xyz(0, 0, 0), vec![0]), Tile::new(TileCoord::of_xyz(2, 3, 1), vec![1])],
            tiles
        );
    }

    #[test]
    fn test_get_tile_not_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        std::fs::create_dir_all(&tiles_dir).unwrap();
        let reader = ReadableFilesArchive::new_reader(&tiles_dir, &ArchiveOptions::empty()).unwrap();
        assert_eq!(None, reader.get_tile(0, 0, 0).unwrap());
    }

    #[test]
    fn test_fails_to_read_tile_from_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        std::fs::create_dir_all(tiles_dir.join("0/0/0.pbf")).unwrap();
        let reader = ReadableFilesArchive::new_reader(&tiles_dir, &ArchiveOptions::empty()).unwrap();
        assert!(reader.get_tile(0, 0, 0).is_err());
    }

    #[test]
    fn test_requires_existing_path() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ReadableFilesArchive::new_reader(&tmp.path().join("tiles"), &ArchiveOptions::empty()).is_err());
    }

    #[test]
    fn test_read_custom_scheme() {
        for (scheme, file) in [
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
            write(&tiles_dir.join(file), &[1]);
            let options = ArchiveOptions::of_pairs(&[(OPTION_TILE_SCHEME, scheme)]);
            let archive = ReadableFilesArchive::new_reader(&tiles_dir, &options).unwrap();
            let coords: Vec<TileCoord> = archive.get_all_tile_coords().unwrap().map(|c| c.unwrap()).collect();
            assert_eq!(vec![TileCoord::of_xyz(1, 2, 3)], coords, "{scheme}");
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
            let shortcut_path = test_base.join(shortcut);
            let actual_path = test_base.join(actual);
            write(&test_base.join(file), &[1]);
            std::fs::write(actual_path.join("metadata.json"), MAX_METADATA_SERIALIZED).unwrap();
            let archive = ReadableFilesArchive::new_reader(&shortcut_path, &ArchiveOptions::empty()).unwrap();
            let coords: Vec<TileCoord> = archive.get_all_tile_coords().unwrap().map(|c| c.unwrap()).collect();
            assert_eq!(vec![TileCoord::of_xyz(1, 2, 3)], coords, "{shortcut}");
            assert!(archive.metadata().unwrap().is_some());
        }
    }

    #[test]
    fn test_has_no_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        std::fs::create_dir_all(&tiles_dir).unwrap();
        let reader = ReadableFilesArchive::new_reader(&tiles_dir, &ArchiveOptions::empty()).unwrap();
        assert_eq!(None, reader.metadata().unwrap());
    }

    fn test_metadata(base_path: &Path, options: ArchiveOptions, metadata_path: &Path) {
        let reader = ReadableFilesArchive::new_reader(base_path, &options).unwrap();
        assert_eq!(None, reader.metadata().unwrap());
        std::fs::write(metadata_path, MAX_METADATA_SERIALIZED).unwrap();
        assert_eq!(Some(max_metadata_deserialized()), reader.metadata().unwrap());
    }

    #[test]
    fn test_metadata_default() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        std::fs::create_dir_all(&tiles_dir).unwrap();
        test_metadata(&tiles_dir, ArchiveOptions::empty(), &tiles_dir.join("metadata.json"));
    }

    #[test]
    fn test_metadata_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        std::fs::create_dir_all(&tiles_dir).unwrap();
        test_metadata(&tiles_dir, ArchiveOptions::of_pairs(&[(OPTION_METADATA_PATH, "x.y")]), &tiles_dir.join("x.y"));
    }

    #[test]
    fn test_metadata_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        std::fs::create_dir_all(&tiles_dir).unwrap();
        std::fs::create_dir_all(tmp.path().join("abs")).unwrap();
        let p = tmp.path().join("abs").join("x.y");
        test_metadata(&tiles_dir, ArchiveOptions::of_pairs(&[(OPTION_METADATA_PATH, &p.to_string_lossy())]), &p);
    }

    #[test]
    fn test_metadata_none() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        std::fs::create_dir_all(&tiles_dir).unwrap();
        let options = ArchiveOptions::of_pairs(&[(OPTION_METADATA_PATH, "none")]);
        let reader = ReadableFilesArchive::new_reader(&tiles_dir, &options).unwrap();
        assert_eq!(None, reader.metadata().unwrap());
        std::fs::write(tiles_dir.join("none"), MAX_METADATA_SERIALIZED).unwrap();
        assert_eq!(None, reader.metadata().unwrap());
    }
}
