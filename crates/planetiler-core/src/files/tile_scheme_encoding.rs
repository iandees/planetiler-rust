//! Port of `files/TileSchemeEncoding.java`: encoding and decoding of tile coordinates to a relative path.
//!
//! The tile scheme is a template string that supports the following templates: `{x}`, `{y}`, `{z}`, `{xs}`, `{ys}`.
//! `{xs}` and `{ys}` are "safe" options that split the x/y coordinate into two folders, ensuring that each folder has
//! less than 1000 children.
//!
//! | Tile Scheme          | Example Path            |
//! |----------------------|-------------------------|
//! | `{z}/{x}/{y}.pbf`    | `3/1/2.pbf`             |
//! | `{x}/{y}/{z}.pbf`    | `1/2/3.pbf`             |
//! | `{x}-{y}-{z}.pbf`    | `1-2-3.pbf`             |
//! | `{x}/a/{y}/b{z}.pbf` | `1/a/2/b3.pbf`          |
//! | `{z}/{x}/{y}.pbf.gz` | `3/1/2.pbf.gz`          |
//! | `{z}/{xs}/{ys}.pbf`  | `3/000/001/000/002.pbf` |
//! | `{z}/{x}/{ys}.pbf`   | `3/1/000/002.pbf`       |
//! | `{z}/{xs}/{y}.pbf`   | `3/000/001/2.pbf`       |

use std::fmt;
use std::path::{MAIN_SEPARATOR_STR, Path, PathBuf};

use anyhow::bail;
use regex::Regex;

use crate::geo::tile_coord::TileCoord;
use crate::geo::tile_order::TileOrder;

pub const X_TEMPLATE: &str = "{x}";
pub const X_SAFE_TEMPLATE: &str = "{xs}";
pub const Y_TEMPLATE: &str = "{y}";
pub const Y_SAFE_TEMPLATE: &str = "{ys}";
pub const Z_TEMPLATE: &str = "{z}";

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TileSchemeEncoding {
    tile_scheme: String,
    base_path: PathBuf,
}

fn count_matches(s: &str, pattern: &str) -> usize {
    s.matches(pattern).count()
}

fn to_absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
    }
}

fn split_safe(value: u32) -> String {
    let s = format!("{value:06}");
    format!("{}{}{}", &s[..3], MAIN_SEPARATOR_STR, &s[3..])
}

/// Decodes tile coordinates from paths, see [`TileSchemeEncoding::decoder`].
pub struct TileSchemeDecoder {
    pattern: Regex,
    x_safe: bool,
    y_safe: bool,
}

impl TileSchemeDecoder {
    pub fn decode(&self, path: &Path) -> Option<TileCoord> {
        let abs = to_absolute(path);
        let caps = self.pattern.captures(abs.to_str()?)?;
        let parse = |a: &str, b: Option<&str>| -> Option<u32> {
            match b {
                Some(b) => format!("{}{}", caps.name(a)?.as_str(), caps.name(b)?.as_str()).parse().ok(),
                None => caps.name(a)?.as_str().parse().ok(),
            }
        };
        let x = if self.x_safe { parse("x0", Some("x1"))? } else { parse("x", None)? };
        let y = if self.y_safe { parse("y0", Some("y1"))? } else { parse("y", None)? };
        let z = parse("z", None)?;
        if z > crate::config::planetiler_config::MAX_MAXZOOM {
            return None;
        }
        Some(TileCoord::of_xyz(x, y, z))
    }
}

impl TileSchemeEncoding {
    /// `tile_scheme` is the tile scheme to use e.g. `{z}/{x}/{y}.pbf`; `base_path` is the base path to append the
    /// generated relative tile path to.
    pub fn new(tile_scheme: &str, base_path: &Path) -> anyhow::Result<TileSchemeEncoding> {
        Ok(TileSchemeEncoding { tile_scheme: Self::validate(tile_scheme)?, base_path: base_path.to_path_buf() })
    }

    pub fn tile_scheme(&self) -> &str {
        &self.tile_scheme
    }

    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    /// Returns the absolute-or-relative (depending on `base_path`) path for `tile_coord`.
    pub fn encode(&self, tile_coord: &TileCoord) -> PathBuf {
        let mut p = self.tile_scheme.replace(Z_TEMPLATE, &tile_coord.z.to_string());
        if self.tile_scheme.contains(X_SAFE_TEMPLATE) {
            p = p.replace(X_SAFE_TEMPLATE, &split_safe(tile_coord.x));
        } else {
            p = p.replace(X_TEMPLATE, &tile_coord.x.to_string());
        }
        if self.tile_scheme.contains(Y_SAFE_TEMPLATE) {
            p = p.replace(Y_SAFE_TEMPLATE, &split_safe(tile_coord.y));
        } else {
            p = p.replace(Y_TEMPLATE, &tile_coord.y.to_string());
        }
        self.base_path.join(p)
    }

    /// Returns a decoder that parses tile coordinates from (absolute or relative) tile paths.
    pub fn decoder(&self) -> TileSchemeDecoder {
        let tmp_path = to_absolute(&self.base_path.join(&self.tile_scheme)).to_string_lossy().into_owned();
        let sep = regex::escape(MAIN_SEPARATOR_STR);
        let templates: [(&str, String); 5] = [
            (X_SAFE_TEMPLATE, format!(r"(?<x0>\d+){sep}(?<x1>\d+)")),
            (Y_SAFE_TEMPLATE, format!(r"(?<y0>\d+){sep}(?<y1>\d+)")),
            (X_TEMPLATE, r"(?<x>\d+)".to_string()),
            (Y_TEMPLATE, r"(?<y>\d+)".to_string()),
            (Z_TEMPLATE, r"(?<z>\d+)".to_string()),
        ];
        let mut pattern = String::from("^");
        let mut rest = tmp_path.as_str();
        'outer: while !rest.is_empty() {
            for (template, replacement) in &templates {
                if let Some(after) = rest.strip_prefix(template) {
                    pattern.push_str(replacement);
                    rest = after;
                    continue 'outer;
                }
            }
            let c = rest.chars().next().expect("non-empty");
            pattern.push_str(&regex::escape(&c.to_string()));
            rest = &rest[c.len_utf8()..];
        }
        pattern.push('$');
        TileSchemeDecoder {
            pattern: Regex::new(&pattern).expect("tile scheme pattern is valid"),
            x_safe: self.tile_scheme.contains(X_SAFE_TEMPLATE),
            y_safe: self.tile_scheme.contains(Y_SAFE_TEMPLATE),
        }
    }

    /// Maximum directory depth to search below `base_path` for tiles.
    pub fn search_depth(&self) -> usize {
        Path::new(&self.tile_scheme).components().count()
            + count_matches(&self.tile_scheme, X_SAFE_TEMPLATE)
            + count_matches(&self.tile_scheme, Y_SAFE_TEMPLATE)
    }

    pub fn preferred_tile_order(&self) -> TileOrder {
        // there's only TMS currently - but once there are more, this can be changed according to the scheme
        TileOrder::Tms
    }

    fn validate(tile_scheme: &str) -> anyhow::Result<String> {
        if Path::new(tile_scheme).is_absolute() {
            bail!("tile scheme is not allowed to be absolute");
        }
        if count_matches(tile_scheme, Z_TEMPLATE) != 1
            || count_matches(tile_scheme, X_TEMPLATE) + count_matches(tile_scheme, X_SAFE_TEMPLATE) != 1
            || count_matches(tile_scheme, Y_TEMPLATE) + count_matches(tile_scheme, Y_SAFE_TEMPLATE) != 1
        {
            bail!(
                "tile scheme must contain ('{X_TEMPLATE}' OR '{X_SAFE_TEMPLATE}') AND ('{Y_TEMPLATE}' OR \
                 '{Y_SAFE_TEMPLATE}' ) AND '{Z_TEMPLATE}'"
            );
        }
        if tile_scheme.contains("\\E") || tile_scheme.contains("\\Q") {
            bail!("regex quotes are not allowed");
        }
        Ok(tile_scheme.to_string())
    }
}

impl fmt::Debug for TileSchemeEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TileSchemeEncoding[tileScheme='{}', basePath={}]", self.tile_scheme, self.base_path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASES: &[(&str, &str)] = &[
        ("{z}/{x}/{y}.pbf", "3/1/2.pbf"),
        ("{x}/{y}/{z}.pbf", "1/2/3.pbf"),
        ("{x}-{y}-{z}.pbf", "1-2-3.pbf"),
        ("{x}/a/{y}/b{z}.pbf", "1/a/2/b3.pbf"),
        ("{z}/{x}/{y}.pbf.gz", "3/1/2.pbf.gz"),
        ("{z}/{xs}/{ys}.pbf", "3/000/001/000/002.pbf"),
        ("{z}/{x}/{ys}.pbf", "3/1/000/002.pbf"),
        ("{z}/{xs}/{y}.pbf", "3/000/001/2.pbf"),
    ];

    #[test]
    fn test_encoder() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        for (scheme, path) in CASES {
            assert_eq!(
                tiles_dir.join(path),
                TileSchemeEncoding::new(scheme, &tiles_dir).unwrap().encode(&TileCoord::of_xyz(1, 2, 3))
            );
        }
    }

    #[test]
    fn test_decoder() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        for (scheme, path) in CASES {
            assert_eq!(
                Some(TileCoord::of_xyz(1, 2, 3)),
                TileSchemeEncoding::new(scheme, &tiles_dir).unwrap().decoder().decode(&tiles_dir.join(path)),
                "{scheme}"
            );
        }
        for path in ["3/1/2.pb", "3/1/2", "a/1/2.pbf", "3/a/2.pbf", "3/1/a.pbf"] {
            assert_eq!(
                None,
                TileSchemeEncoding::new("{z}/{x}/{y}.pbf", &tiles_dir).unwrap().decoder().decode(&tiles_dir.join(path))
            );
        }
    }

    #[test]
    fn test_search_depth() {
        let tiles_dir = PathBuf::from("tiles");
        for (scheme, depth) in [
            ("{z}/{x}/{y}.pbf", 3),
            ("{x}/{y}/{z}.pbf", 3),
            ("{x}-{y}-{z}.pbf", 1),
            ("{x}/a/{y}/b{z}.pbf", 4),
            ("{z}/{x}/{y}.pbf.gz", 3),
            ("{z}/{xs}/{ys}.pbf", 5),
            ("{z}/{x}/{ys}.pbf", 4),
            ("{z}/{xs}/{y}.pbf", 4),
        ] {
            assert_eq!(depth, TileSchemeEncoding::new(scheme, &tiles_dir).unwrap().search_depth(), "{scheme}");
        }
    }

    #[test]
    fn test_invalid_schemes() {
        let tiles_dir = PathBuf::from("tiles");
        for scheme in [
            "1/2/3.pbf",
            "{x}/{y}.pbf",
            "{z}/{y}.pbf",
            "{z}/{x}",
            "{z}/{x}/1.pbf",
            "{z}/{x}/{y}/{xs}.pbf",
            "{z}/{x}/{y}/{ys}.pbf",
            "{z}/{z}/{x}/{y}.pbf",
            "{x}/{z}/{x}/{y}.pbf",
            "{y}/{z}/{x}/{y}.pbf",
            "{xs}/{z}/{xs}/{ys}.pbf",
            "{ys}/{z}/{xs}/{ys}.pbf",
            "{x}/\\Q/{y}/b{z}.pbf",
            "{x}/\\E/{y}/b{z}.pbf",
        ] {
            assert!(TileSchemeEncoding::new(scheme, &tiles_dir).is_err(), "{scheme}");
        }
    }

    #[test]
    fn test_invalid_absolute_tile_scheme() {
        let tmp = tempfile::tempdir().unwrap();
        let tiles_dir = tmp.path().join("tiles");
        let absolute = tiles_dir.join("{z}").join("{x}").join("{y}.pbf");
        assert!(TileSchemeEncoding::new(&absolute.to_string_lossy(), &tiles_dir).is_err());
    }

    #[test]
    fn test_preferred_tile_order() {
        let tiles_dir = PathBuf::from("tiles");
        for scheme in ["{z}/{x}/{y}.pbf", "{z}/{xs}/{ys}.pbf", "{x}/{y}/{z}.pbf"] {
            assert_eq!(TileOrder::Tms, TileSchemeEncoding::new(scheme, &tiles_dir).unwrap().preferred_tile_order());
        }
    }
}
