//! Port of `util/TileSizeStats.java`.
//!
//! Utilities for extracting tile and layer size summaries from encoded vector tiles.
//!
//! [`compute_tile_stats`] extracts statistics about each layer in a tile and [`TsvSerializer`] formats them as rows
//! of a TSV file to write.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use prost::Message;

use crate::geo::tile_coord::TileCoord;
use crate::proto::vector_tile as pb;
use crate::vector_tile;

// TODO(port): `TileSizeStats.main` (the `stats` CLI command that reads an existing archive and writes a layerstats
// file) depends on TileArchives/WorkerPipeline/ProgressLoggers and is not ported yet.
// TODO(port): `computeMltTileStats` depends on the MLT decoder (org.maplibre.mlt) which is not ported yet.

const COLUMNS: [&str; 12] = [
    "z",
    "x",
    "y",
    "hilbert",
    "archived_tile_bytes",
    "layer",
    "layer_bytes",
    "layer_features",
    "layer_geometries",
    "layer_attr_bytes",
    "layer_attr_keys",
    "layer_attr_values",
];

/// Returns the default path that a layerstats file should go relative to an existing archive.
pub fn get_default_layerstats_path(archive: &Path) -> PathBuf {
    let file_name = archive.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    archive.with_file_name(format!("{file_name}.layerstats.tsv.gz"))
}

/// Returns a [`TsvSerializer`] that can be used by a single thread to convert to TSV rows.
pub fn new_thread_local_serializer() -> TsvSerializer {
    TsvSerializer
}

/// Opens a new gzip (level 1/fast) writer to `path`, creating a new one or replacing an existing file at that path.
pub fn new_writer(path: &Path) -> std::io::Result<GzEncoder<BufWriter<File>>> {
    let file = OpenOptions::new().write(true).create(true).truncate(true).open(path)?;
    Ok(GzEncoder::new(BufWriter::new(file), Compression::fast()))
}

/// Returns the header row for the output TSV file.
pub fn header_row() -> String {
    let mut result = COLUMNS.join("\t");
    result.push('\n');
    result
}

/// Returns the size and statistics for each layer in `proto`, sorted by layer name.
pub fn compute_tile_stats(proto: &pb::Tile) -> Vec<LayerStats> {
    let mut result: Vec<LayerStats> = Vec::with_capacity(proto.layers.len());
    for layer in &proto.layers {
        let mut attr_size = 0usize;
        for key in &layer.keys {
            attr_size += key.len();
        }
        for value in &layer.values {
            attr_size += value.encoded_len();
        }
        let geom_count: usize = layer.features.iter().map(vector_tile::count_geometries).sum();
        result.push(LayerStats {
            layer: layer.name.clone(),
            layer_bytes: layer.encoded_len() as i64,
            layer_features: layer.features.len() as i64,
            layer_geometries: geom_count as i64,
            layer_attr_bytes: attr_size as i64,
            layer_attr_keys: layer.keys.len() as i64,
            layer_attr_values: layer.values.len() as i64,
        });
    }
    result.sort();
    result
}

/// Quote a TSV field the way Jackson's CSV writer does when it contains special characters.
fn quote(field: &str) -> std::borrow::Cow<'_, str> {
    if field.contains(['\t', '"', '\n', '\r']) {
        std::borrow::Cow::Owned(format!("\"{}\"", field.replace('"', "\"\"")))
    } else {
        std::borrow::Cow::Borrowed(field)
    }
}

/// Formats [`LayerStats`] as rows of the layerstats TSV file.
pub struct TsvSerializer;

impl TsvSerializer {
    /// Returns the TSV rows to output for all the layers in a tile.
    pub fn format_output_rows(
        &self,
        tile_coord: TileCoord,
        archived_bytes: i64,
        layer_stats: &[LayerStats],
    ) -> Vec<String> {
        let hilbert = tile_coord.hilbert_encoded();
        layer_stats
            .iter()
            .map(|layer| {
                format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    tile_coord.z,
                    tile_coord.x,
                    tile_coord.y,
                    hilbert,
                    archived_bytes,
                    quote(&layer.layer),
                    layer.layer_bytes,
                    layer.layer_features,
                    layer.layer_geometries,
                    layer.layer_attr_bytes,
                    layer.layer_attr_keys,
                    layer.layer_attr_values
                )
            })
            .collect()
    }

    /// Write the rows for a tile to `writer`.
    pub fn write_output_rows(
        &self,
        writer: &mut impl Write,
        tile_coord: TileCoord,
        archived_bytes: i64,
        layer_stats: &[LayerStats],
    ) -> std::io::Result<()> {
        for row in self.format_output_rows(tile_coord, archived_bytes, layer_stats) {
            writer.write_all(row.as_bytes())?;
        }
        Ok(())
    }
}

/// Stats extracted from a layer in a vector tile. Ordered by layer name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LayerStats {
    pub layer: String,
    pub layer_bytes: i64,
    pub layer_features: i64,
    pub layer_geometries: i64,
    pub layer_attr_bytes: i64,
    pub layer_attr_keys: i64,
    pub layer_attr_values: i64,
}

impl LayerStats {
    pub fn new(
        layer: impl Into<String>,
        layer_bytes: i64,
        layer_features: i64,
        layer_geometries: i64,
        layer_attr_bytes: i64,
        layer_attr_keys: i64,
        layer_attr_values: i64,
    ) -> LayerStats {
        LayerStats {
            layer: layer.into(),
            layer_bytes,
            layer_features,
            layer_geometries,
            layer_attr_bytes,
            layer_attr_keys,
            layer_attr_values,
        }
    }
}

impl PartialOrd for LayerStats {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LayerStats {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Java compares only by layer; tie-break on remaining fields to stay consistent with Eq
        self.layer.cmp(&other.layer).then_with(|| {
            (
                self.layer_bytes,
                self.layer_features,
                self.layer_geometries,
                self.layer_attr_bytes,
                self.layer_attr_keys,
                self.layer_attr_values,
            )
                .cmp(&(
                    other.layer_bytes,
                    other.layer_features,
                    other.layer_geometries,
                    other.layer_attr_bytes,
                    other.layer_attr_keys,
                    other.layer_attr_values,
                ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags;
    use crate::value::Tags;
    use crate::vector_tile::{Feature, VectorTile, encode_geometry};
    use geo_types::{Geometry, Point};

    fn new_point(x: f64, y: f64) -> Geometry<f64> {
        Geometry::Point(Point::new(x, y))
    }

    #[test]
    fn compute_stats_empty() {
        assert_eq!(0, compute_tile_stats(&VectorTile::new().to_proto(true)).len());
    }

    #[test]
    fn compute_stats_one_feature() {
        let stats = compute_tile_stats(
            &VectorTile::new()
                .add_layer_features(
                    "layer",
                    &[Feature::new(
                        "layer",
                        1,
                        encode_geometry(&new_point(0.0, 0.0)),
                        tags! {"key1" => "value1", "key2" => 2},
                    )],
                )
                .to_proto(true),
        );
        assert_eq!(1, stats.len());
        let entry1 = &stats[0];
        assert_eq!("layer", entry1.layer);
        assert_eq!(1, entry1.layer_features);
        assert_eq!(55, entry1.layer_bytes);
        assert_eq!(18, entry1.layer_attr_bytes);
        assert_eq!(2, entry1.layer_attr_keys);
        assert_eq!(2, entry1.layer_attr_values);

        let formatted = new_thread_local_serializer().format_output_rows(TileCoord::of_xyz(1, 2, 3), 999, &stats);
        assert_eq!(
            "z\tx\ty\thilbert\tarchived_tile_bytes\tlayer\tlayer_bytes\tlayer_features\tlayer_geometries\tlayer_attr_bytes\tlayer_attr_keys\tlayer_attr_values\n\
             3\t1\t2\t34\t999\tlayer\t55\t1\t1\t18\t2\t2",
            (header_row() + &formatted.join("")).trim()
        );
    }

    #[test]
    fn compute_stats_2_features() {
        let stats = compute_tile_stats(
            &VectorTile::new()
                .add_layer_features("b", &[Feature::new("b", 1, encode_geometry(&new_point(0.0, 0.0)), Tags::new())])
                .add_layer_features(
                    "a",
                    &[
                        Feature::new(
                            "a",
                            1,
                            encode_geometry(&new_point(0.0, 0.0)),
                            tags! {"key1" => "value1", "key2" => 2},
                        ),
                        Feature::new("a", 2, encode_geometry(&new_point(1.0, 1.0)), tags! {"key1" => 2, "key2" => 3}),
                    ],
                )
                .to_proto(true),
        );
        assert_eq!(2, stats.len());
        let entry1 = &stats[0];
        assert_eq!("a", entry1.layer);
        assert_eq!(2, entry1.layer_features);
        assert_eq!(72, entry1.layer_bytes);
        assert_eq!(20, entry1.layer_attr_bytes);
        assert_eq!(2, entry1.layer_attr_keys);
        assert_eq!(3, entry1.layer_attr_values);
        let entry2 = &stats[1];
        assert_eq!("b", entry2.layer);
        assert_eq!(1, entry2.layer_features);

        let formatted = new_thread_local_serializer().format_output_rows(TileCoord::of_xyz(1, 2, 3), 999, &stats);
        assert_eq!(
            "z\tx\ty\thilbert\tarchived_tile_bytes\tlayer\tlayer_bytes\tlayer_features\tlayer_geometries\tlayer_attr_bytes\tlayer_attr_keys\tlayer_attr_values\n\
             3\t1\t2\t34\t999\ta\t72\t2\t2\t20\t2\t3\n\
             3\t1\t2\t34\t999\tb\t19\t1\t1\t0\t0\t0",
            (header_row() + &formatted.join("")).trim()
        );
    }

    #[test]
    fn default_layerstats_path() {
        assert_eq!(
            PathBuf::from("/a/b/out.pmtiles.layerstats.tsv.gz"),
            get_default_layerstats_path(Path::new("/a/b/out.pmtiles"))
        );
    }

    #[test]
    fn writer_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.tsv.gz");
        {
            let mut writer = new_writer(&path).unwrap();
            writer.write_all(header_row().as_bytes()).unwrap();
            new_thread_local_serializer()
                .write_output_rows(
                    &mut writer,
                    TileCoord::of_xyz(0, 0, 0),
                    10,
                    &[LayerStats::new("a", 1, 2, 3, 4, 5, 6)],
                )
                .unwrap();
            writer.finish().unwrap().flush().unwrap();
        }
        let mut out = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(File::open(&path).unwrap()), &mut out).unwrap();
        assert_eq!(header_row() + "0\t0\t0\t0\t10\ta\t1\t2\t3\t4\t5\t6\n", out);
    }
}
