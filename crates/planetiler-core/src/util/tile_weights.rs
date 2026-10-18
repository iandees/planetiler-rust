//! Port of `util/TileWeights.java`: holds tile weights to compute weighted average tile sizes.
//!
//! [`crate::util::top_osm_tiles`] can be used to get tile weights from 90 days of openstreetmap.org tile traffic.

use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use crate::config::planetiler_config::{MAX_MAXZOOM, MIN_MINZOOM};
use crate::geo::tile_coord::TileCoord;

/// Top 50k tiles from openstreetmap.org tile traffic, used when no tile weights file is available.
const TOP_50K_OSM_TILES: &str = include_str!("../../resources/top_50k_osm_tiles.tsv");

/// Weights (i.e. number of requests) for each tile.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct TileWeights {
    by_zoom: HashMap<u32, i64>,
    weights: HashMap<TileCoord, i64>,
}

impl TileWeights {
    pub fn new() -> TileWeights {
        TileWeights::default()
    }

    /// Returns the weight of `coord`, or 0 if missing.
    pub fn get_weight(&self, coord: TileCoord) -> i64 {
        self.weights.get(&coord).copied().unwrap_or(0)
    }

    /// Returns the sum of all tile weights at a specific zoom.
    pub fn get_zoom_weight(&self, zoom: u32) -> i64 {
        self.by_zoom.get(&zoom).copied().unwrap_or(0)
    }

    /// Adds `weight` to the current weight for `coord` and returns this modified instance.
    pub fn put(&mut self, coord: TileCoord, weight: i64) -> &mut Self {
        *self.weights.entry(coord).or_insert(0) += weight;
        *self.by_zoom.entry(coord.z).or_insert(0) += weight;
        self
    }

    /// Returns true if there are no tiles with positive weight.
    pub fn is_empty(&self) -> bool {
        !self.by_zoom.values().any(|w| *w > 0)
    }

    /// Writes tile weights to a gzipped TSV file with `z, x, y, loads` columns.
    pub fn write_to_file(&self, path: &Path) -> std::io::Result<()> {
        let file = std::fs::File::create(path)?;
        let mut output = GzEncoder::new(std::io::BufWriter::new(file), Compression::default());
        writeln!(output, "z\tx\ty\tloads")?;
        let mut sorted: Vec<_> = self.weights.iter().collect();
        sorted.sort_by_key(|(coord, _)| coord.encoded);
        for (coord, loads) in sorted {
            writeln!(output, "{}\t{}\t{}\t{loads}", coord.z, coord.x, coord.y)?;
        }
        output.finish()?.flush()
    }

    /// Loads tile weights from a gzipped TSV file with `z, x, y, loads` columns.
    ///
    /// Duplicate entries are added together. If the file is missing or invalid, falls back to an embedded file with
    /// the top 50k OSM tiles.
    pub fn read_from_file(path: &Path) -> TileWeights {
        let from_file = std::fs::File::open(path)
            .map_err(anyhow::Error::from)
            .and_then(|f| Self::read_from(GzDecoder::new(std::io::BufReader::new(f))));
        match from_file {
            Ok(result) if !result.weights.is_empty() => return result,
            _ => tracing::info!("Unable to load tile weights from {}, falling back to top 100k", path.display()),
        }
        match Self::read_from(TOP_50K_OSM_TILES.as_bytes()) {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!("Unable to load top 100k tile weights, falling back to unweighted average: {e}");
                TileWeights::new()
            }
        }
    }

    fn read_from(input: impl Read) -> anyhow::Result<TileWeights> {
        let mut result = TileWeights::new();
        let mut lines = BufReader::new(input).lines();
        let Some(header) = lines.next().transpose()? else {
            return Ok(result);
        };
        let columns: Vec<String> = header.split('\t').map(|s| s.trim().to_string()).collect();
        let index = |name: &str| columns.iter().position(|c| c == name);
        let (zi, xi, yi, li) = (index("z"), index("x"), index("y"), index("loads"));
        for line in lines {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            let values: Vec<&str> = line.split('\t').collect();
            if values.len() > columns.len() {
                anyhow::bail!("too many columns in tile weights row: {line}");
            }
            let get = |i: Option<usize>| -> anyhow::Result<i64> {
                match i.and_then(|i| values.get(i)) {
                    Some(v) => Ok(v.trim().parse()?),
                    None => Ok(0),
                }
            };
            let (z, x, y, loads) = (get(zi)?, get(xi)?, get(yi)?, get(li)?);
            if z >= MIN_MINZOOM as i64 && z <= MAX_MAXZOOM as i64 {
                let size = 1i64 << z;
                let x = x.rem_euclid(size) as u32;
                let y = y.rem_euclid(size) as u32;
                result.put(TileCoord::of_xyz(x, y, z as u32), loads);
            }
        }
        Ok(result)
    }
}

impl crate::util::tileset_summary_statistics::TileWeightSource for TileWeights {
    fn get_weight(&self, coord: &TileCoord) -> i64 {
        TileWeights::get_weight(self, *coord)
    }

    fn get_zoom_weight(&self, zoom: u32) -> i64 {
        TileWeights::get_zoom_weight(self, zoom)
    }

    fn is_empty(&self) -> bool {
        TileWeights::is_empty(self)
    }
}

impl fmt::Debug for TileWeights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "TileWeights{{")?;
        for (coord, weight) in &self.weights {
            writeln!(f, "  {coord}: {weight}")?;
        }
        write!(f, "}}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gzip(s: &str) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(s.as_bytes()).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn weights() {
        let mut weights = TileWeights::new();
        assert_eq!(0, weights.get_weight(TileCoord::of_xyz(0, 0, 0)));
        assert_eq!(0, weights.get_zoom_weight(0));
        assert_eq!(0, weights.get_weight(TileCoord::of_xyz(0, 0, 1)));
        assert_eq!(0, weights.get_weight(TileCoord::of_xyz(1, 0, 1)));
        assert_eq!(0, weights.get_zoom_weight(1));
        assert!(weights.is_empty());

        weights.put(TileCoord::of_xyz(0, 0, 0), 1);
        weights.put(TileCoord::of_xyz(0, 0, 0), 2);
        weights.put(TileCoord::of_xyz(0, 0, 1), 3);
        weights.put(TileCoord::of_xyz(1, 0, 1), 4);

        assert!(!weights.is_empty());
        assert_eq!(3, weights.get_weight(TileCoord::of_xyz(0, 0, 0)));
        assert_eq!(3, weights.get_zoom_weight(0));
        assert_eq!(3, weights.get_weight(TileCoord::of_xyz(0, 0, 1)));
        assert_eq!(4, weights.get_weight(TileCoord::of_xyz(1, 0, 1)));
        assert_eq!(7, weights.get_zoom_weight(1));
    }

    #[test]
    fn write_to_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.tsv.gz");
        TileWeights::new().write_to_file(&file).unwrap();
        let read = TileWeights::read_from_file(&file);
        // when read is empty, fall back to embedded
        assert_eq!(191148782, read.get_weight(TileCoord::of_xyz(0, 0, 0)));
    }

    #[test]
    fn write_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.tsv.gz");
        TileWeights::new()
            .put(TileCoord::of_xyz(0, 0, 1), 1)
            .put(TileCoord::of_xyz(0, 0, 1), 1)
            .put(TileCoord::of_xyz(0, 0, 0), 1)
            .write_to_file(&file)
            .unwrap();
        let read = TileWeights::read_from_file(&file);
        let mut contents = String::new();
        GzDecoder::new(std::fs::File::open(&file).unwrap()).read_to_string(&mut contents).unwrap();
        assert_eq!("z\tx\ty\tloads\n0\t0\t0\t1\n1\t0\t0\t2\n", contents);
        assert_eq!(1, read.get_weight(TileCoord::of_xyz(0, 0, 0)));
        assert_eq!(2, read.get_weight(TileCoord::of_xyz(0, 0, 1)));
    }

    #[test]
    fn read_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.tsv.gz");
        let expected = 191148782;
        assert_eq!(expected, TileWeights::read_from_file(&file).get_weight(TileCoord::of_xyz(0, 0, 0)));

        std::fs::write(&file, gzip("garbage\n")).unwrap();
        assert_eq!(expected, TileWeights::read_from_file(&file).get_weight(TileCoord::of_xyz(0, 0, 0)));

        std::fs::write(&file, gzip("z\tx\ty\tloads\na\tb\tc\td\n")).unwrap();
        assert_eq!(expected, TileWeights::read_from_file(&file).get_weight(TileCoord::of_xyz(0, 0, 0)));

        std::fs::write(&file, gzip("z\tx\td\tloads\n1\t2\t3\t4\n")).unwrap();
        assert_eq!(0, TileWeights::read_from_file(&file).get_weight(TileCoord::of_xyz(0, 0, 0)));

        std::fs::write(&file, gzip("z\tx\ty\tloads\n-1\t2\t-3\t4\n1\t2\t5\t4\n")).unwrap();
        assert_eq!(4, TileWeights::read_from_file(&file).get_weight(TileCoord::of_xyz(0, 1, 1)));
    }
}
