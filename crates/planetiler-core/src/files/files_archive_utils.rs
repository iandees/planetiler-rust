//! Port of `files/FilesArchiveUtils.java`.

use std::path::{Component, Path, PathBuf};

use crate::config::arguments::Arguments;
use crate::files::tile_scheme_encoding::{TileSchemeEncoding, X_TEMPLATE, Y_TEMPLATE, Z_TEMPLATE};

pub const OPTION_METADATA_PATH: &str = "metadata_path";
pub const OPTION_TILE_SCHEME: &str = "tile_scheme";

/// Path to the metadata file of a files archive at `base_path`, or `None` if disabled via `metadata_path=none`.
pub fn metadata_path(base_path: &Path, options: &Arguments) -> Option<PathBuf> {
    let raw =
        options.get_string(OPTION_METADATA_PATH, "path to the metadata - use \"none\" to disable", "metadata.json");
    if raw == "none" {
        return None;
    }
    let p = PathBuf::from(raw);
    if p.is_absolute() { Some(p) } else { Some(base_path.join(p)) }
}

pub fn tiles_scheme_encoding(
    options: &Arguments,
    base_path: &Path,
    default_tile_scheme: &str,
) -> anyhow::Result<TileSchemeEncoding> {
    let tile_scheme = options.get_string(
        OPTION_TILE_SCHEME,
        "the tile scheme (e.g. {z}/{x}/{y}.pbf, {x}/{y}/{z}.pbf) - instead of {x}/{y} {xs}/{ys} can be used which \
         splits the x/y into 2 directories each which ensures <1000 files per directory",
        default_tile_scheme,
    );
    TileSchemeEncoding::new(&tile_scheme, base_path)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BasePathWithTileSchemeEncoding {
    pub base_path: PathBuf,
    pub tile_scheme_encoding: TileSchemeEncoding,
}

/// Splits a path like `tiles/{z}/{x}/{y}.pbf` into the base path (`tiles`) and tile scheme, with the tile scheme
/// option taking precedence over the one in the path.
pub fn base_path_with_tile_scheme_encoding(
    options: &Arguments,
    base_path: &Path,
) -> anyhow::Result<BasePathWithTileSchemeEncoding> {
    let (base, scheme) = split_shortcut_path(base_path);
    let tile_scheme =
        scheme.unwrap_or_else(|| PathBuf::from(Z_TEMPLATE).join(X_TEMPLATE).join(format!("{Y_TEMPLATE}.pbf")));
    let encoding = tiles_scheme_encoding(options, &base, &tile_scheme.to_string_lossy())?;
    Ok(BasePathWithTileSchemeEncoding { base_path: base, tile_scheme_encoding: encoding })
}

/// Returns the base path of a files archive, removing any tile scheme template from it.
pub fn clean_base_path(base_path: &Path) -> PathBuf {
    split_shortcut_path(base_path).0
}

fn split_shortcut_path(base_path: &Path) -> (PathBuf, Option<PathBuf>) {
    let mut base_part = PathBuf::new();
    let mut tile_scheme_part: Option<PathBuf> = None;
    let mut remaining_is_tile_scheme = false;
    for component in base_path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => base_part.push(component.as_os_str()),
            _ => {
                let part = component.as_os_str();
                if !remaining_is_tile_scheme && part.to_string_lossy().contains('{') {
                    remaining_is_tile_scheme = true;
                }
                if remaining_is_tile_scheme {
                    tile_scheme_part = Some(match tile_scheme_part {
                        None => PathBuf::from(part),
                        Some(p) => p.join(part),
                    });
                } else {
                    base_part.push(part);
                }
            }
        }
    }
    match tile_scheme_part {
        // just in case: use the "original" basePath in case no tile scheme is included
        None => (base_path.to_path_buf(), None),
        Some(scheme) => (base_part, Some(scheme)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_path(input: &str, temp_dir: &Path) -> PathBuf {
        if input.is_empty() {
            PathBuf::from("")
        } else if let Some(rest) = input.strip_prefix('/') {
            temp_dir.join(rest)
        } else {
            PathBuf::from(input)
        }
    }

    #[test]
    fn test_base_path_with_tile_scheme_encoding() {
        let tmp = tempfile::tempdir().unwrap();
        for (shortcut, actual, scheme) in [
            ("{z}/{x}/{y}.pbf", "", "{z}/{x}/{y}.pbf"),
            ("", "", "{z}/{x}/{y}.pbf"),
            ("{x}/{y}/{z}.pbf", "", "{x}/{y}/{z}.pbf"),
            ("tiles/{z}/{x}/{y}.pbf", "tiles", "{z}/{x}/{y}.pbf"),
            ("tiles/z{z}/{x}/{y}.pbf", "tiles", "z{z}/{x}/{y}.pbf"),
            ("z{z}/x{x}/y{y}.pbf", "", "z{z}/x{x}/y{y}.pbf"),
            ("tiles/tile-{z}-{x}-{y}.pbf", "tiles", "tile-{z}-{x}-{y}.pbf"),
            ("/a", "/a", "{z}/{x}/{y}.pbf"),
            ("/", "/", "{z}/{x}/{y}.pbf"),
        ] {
            let shortcut_path = make_path(shortcut, tmp.path());
            let actual_path = make_path(actual, tmp.path());
            assert_eq!(
                BasePathWithTileSchemeEncoding {
                    base_path: actual_path.clone(),
                    tile_scheme_encoding: TileSchemeEncoding::new(scheme, &actual_path).unwrap(),
                },
                base_path_with_tile_scheme_encoding(&Arguments::of(), &shortcut_path).unwrap(),
                "{shortcut}"
            );
        }
    }

    #[test]
    fn test_prefers_arg_over_shortcut() {
        let base = PathBuf::from("");
        let shortcut_scheme = "{x}/{y}/{z}.pbf";
        let arg_scheme = "x{x}/y{y}/z{z}.pbf";
        let shortcut_path = base.join(shortcut_scheme);
        assert_eq!(
            BasePathWithTileSchemeEncoding {
                base_path: base.clone(),
                tile_scheme_encoding: TileSchemeEncoding::new(shortcut_scheme, &base).unwrap(),
            },
            base_path_with_tile_scheme_encoding(&Arguments::of(), &shortcut_path).unwrap()
        );
        assert_eq!(
            BasePathWithTileSchemeEncoding {
                base_path: base.clone(),
                tile_scheme_encoding: TileSchemeEncoding::new(arg_scheme, &base).unwrap(),
            },
            base_path_with_tile_scheme_encoding(
                &Arguments::of_pairs([(OPTION_TILE_SCHEME, arg_scheme)]),
                &shortcut_path
            )
            .unwrap()
        );
    }

    #[test]
    fn test_clean_base_path() {
        let tmp = tempfile::tempdir().unwrap();
        for (shortcut, actual) in [
            ("{z}/{x}/{y}.pbf", ""),
            ("", ""),
            ("{x}/{y}/{z}.pbf", ""),
            ("tiles/{z}/{x}/{y}.pbf", "tiles"),
            ("tiles/z{z}/{x}/{y}.pbf", "tiles"),
            ("z{z}/x{x}/y{y}.pbf", ""),
            ("tiles/tile-{z}-{x}-{y}.pbf", "tiles"),
            ("/a", "/a"),
            ("/", "/"),
        ] {
            assert_eq!(make_path(actual, tmp.path()), clean_base_path(&make_path(shortcut, tmp.path())), "{shortcut}");
        }
    }
}
