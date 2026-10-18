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

const BATCH_SIZE: usize = 1_000;

/// `TileSizeStats.main`: the `stats` CLI command that reads every tile in `--input` and writes a layerstats file to
/// `--output` (default `<input>.layerstats.tsv.gz`), then prints a size summary.
///
/// Batches of tiles are decoded in parallel with rayon; rows are written in archive order like Java.
pub fn main<S: AsRef<str>>(args: &[S]) -> anyhow::Result<()> {
    use rayon::prelude::*;

    use crate::archive::tile_archive_config::TileArchiveConfig;
    use crate::archive::tile_archives;
    use crate::config::arguments::Arguments;
    use crate::config::planetiler_config::PlanetilerConfig;
    use crate::util::tile_weights::TileWeights;
    use crate::util::tileset_summary_statistics::TilesetSummaryStatistics;

    let arguments = Arguments::from_args_or_config_file(args)?;
    let config = PlanetilerConfig::from(&arguments)?;
    let stats = crate::stats::stats::in_memory();
    let download = arguments.get_boolean("download_osm_tile_weights|download", "download OSM tile weights file", false);
    if download && !config.tile_weights.exists() {
        crate::util::top_osm_tiles::download_precomputed(&config);
    }
    let tile_stats =
        TilesetSummaryStatistics::with_weights(std::sync::Arc::new(TileWeights::read_from_file(&config.tile_weights)));
    let input_string = arguments.get_required_string("input", "input file")?;
    let input = TileArchiveConfig::from(&input_string)?;
    let output = match input.get_local_path() {
        Some(local) => arguments.file("output", "output file", get_default_layerstats_path(&local)),
        None => arguments.required_file("output", "output file")?,
    };
    let timer = stats.start_stage("tilestats");
    let reader = tile_archives::new_reader(&input, &config)?;
    let updater = tile_stats.thread_local_updater();
    let serializer = new_thread_local_serializer();
    let mut writer = new_writer(&output)?;
    writer.write_all(header_row().as_bytes())?;
    let mut tiles = reader.get_all_tiles()?;
    let mut count = 0u64;
    loop {
        let batch: Vec<_> = tiles.by_ref().take(BATCH_SIZE).collect::<anyhow::Result<Vec<_>>>()?;
        if batch.is_empty() {
            break;
        }
        count += batch.len() as u64;
        let results: Vec<(TileCoord, i64, Vec<LayerStats>)> = batch
            .par_iter()
            .map(|tile| -> anyhow::Result<_> {
                let unzipped = crate::util::gzip::gunzip(&tile.bytes)?;
                let decoded = pb::Tile::decode(unzipped.as_slice())?;
                Ok((tile.coord, tile.bytes.len() as i64, compute_tile_stats(&decoded)))
            })
            .collect::<anyhow::Result<_>>()?;
        for (coord, archived, layer_stats) in results {
            updater.record_tile(coord, archived, &layer_stats);
            serializer.write_output_rows(&mut writer, coord, archived, &layer_stats)?;
        }
    }
    writer.finish()?.flush()?;
    tracing::info!("processed {count} tiles");
    timer.stop();
    tile_stats.print_stats(&config.debug_url_pattern);
    stats.print_summary();
    Ok(())
}

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

/// Returns the size and statistics for each layer of the MLT tile `output` encoded from `input` (Java
/// `computeMltTileStats`), in `input` layer order. Key and value counts come from `vtile` (0 without one).
pub fn compute_mlt_tile_stats(
    vtile: Option<&vector_tile::VectorTile>,
    input: &crate::mlt::MapboxVectorTile,
    output: &[u8],
) -> anyhow::Result<Vec<LayerStats>> {
    use crate::mlt::decoder::{consume_column, layer_blocks};
    use crate::mlt::encoding::varint_size;
    use crate::mlt::metadata::FeatureTable;

    let mut encoded_layer_sizes: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut encoded_layer_attribute_sizes: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for block in layer_blocks(output) {
        let (tag, body) = block?;
        if tag == 1 {
            let length = varint_size(tag) + body.len();
            let mut offset = 0;
            let (metadata, _) = FeatureTable::parse_embedded(body, &mut offset)?;
            let tile = &body[offset..];
            let mut tile_offset = 0;
            let mut attr_bytes = 0;
            for column in &metadata.columns {
                attr_bytes += consume_column(column, tile, &mut tile_offset)?;
            }
            encoded_layer_sizes.insert(metadata.name.clone(), length as i64);
            encoded_layer_attribute_sizes.insert(metadata.name, attr_bytes as i64);
        }
    }
    Ok(input
        .layers
        .iter()
        .map(|layer| LayerStats {
            layer: layer.name.clone(),
            layer_bytes: encoded_layer_sizes.get(&layer.name).copied().unwrap_or(-1),
            layer_features: layer.features.len() as i64,
            layer_geometries: layer.features.iter().map(|f| count_mlt_geometries(&f.geometry)).sum::<usize>() as i64,
            layer_attr_bytes: encoded_layer_attribute_sizes.get(&layer.name).copied().unwrap_or(-1),
            layer_attr_keys: vtile.and_then(|t| t.get_num_keys(&layer.name)).unwrap_or(0) as i64,
            layer_attr_values: vtile.and_then(|t| t.get_num_values(&layer.name)).unwrap_or(0) as i64,
        })
        .collect())
}

/// Number of geometries in `geometry`, counting each part of a multi-geometry or collection (JTS
/// `GeometryCollection`s).
fn count_mlt_geometries(geometry: &geo_types::Geometry<f64>) -> usize {
    use geo_types::Geometry;
    match geometry {
        Geometry::MultiPoint(g) => g.0.len(),
        Geometry::MultiLineString(g) => g.0.len(),
        Geometry::MultiPolygon(g) => g.0.len(),
        Geometry::GeometryCollection(g) => g.0.iter().map(count_mlt_geometries).sum(),
        _ => 1,
    }
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
                .add_layer_features(
                    "b",
                    &[Feature::new("b", 1, encode_geometry(&new_point(0.0, 0.0)), Tags::default())],
                )
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

    fn two_layer_tile() -> VectorTile {
        let mut tile = VectorTile::new();
        tile.add_layer_features("b", &[Feature::new("b", 1, encode_geometry(&new_point(0.0, 0.0)), Tags::default())])
            .add_layer_features(
                "a",
                &[
                    Feature::new(
                        "a",
                        1,
                        encode_geometry(&new_point(0.0, 0.0)),
                        tags! {"key1" => "value1", "key2" => 2},
                    ),
                    Feature::new(
                        "a",
                        2,
                        encode_geometry(&new_point(1.0, 1.0)),
                        tags! {"key1" => "value1", "key2" => 3},
                    ),
                ],
            );
        tile
    }

    #[test]
    fn compute_stats_2_features_mlt() {
        use crate::mlt::{ConversionConfig, TypeMismatchPolicy, convert_mvt, create_tileset_metadata};
        let vector_tile = two_layer_tile();
        let mlt_input = vector_tile.to_mlt_input(None);
        let tileset_metadata = create_tileset_metadata(&mlt_input, TypeMismatchPolicy::Fail, &[], true).unwrap();
        let config =
            ConversionConfig { include_ids: true, use_fsst: false, use_fast_pfor: false, ..Default::default() };
        let mlt = convert_mvt(&mlt_input, &tileset_metadata, &config).unwrap();
        let stats = compute_mlt_tile_stats(Some(&vector_tile), &mlt_input, &mlt).unwrap();
        assert_eq!(2, stats.len());
        let entry1 = &stats[0];
        assert_eq!("a", entry1.layer);
        assert_eq!(2, entry1.layer_features);
        assert_eq!(85, entry1.layer_bytes);
        assert_eq!(41, entry1.layer_attr_bytes);
        assert_eq!(2, entry1.layer_attr_keys);
        assert_eq!(3, entry1.layer_attr_values);
        let entry2 = &stats[1];
        assert_eq!("b", entry2.layer);
        assert_eq!(1, entry2.layer_features);

        let formatted = new_thread_local_serializer().format_output_rows(TileCoord::of_xyz(1, 2, 3), 999, &stats);
        assert_eq!(
            "z\tx\ty\thilbert\tarchived_tile_bytes\tlayer\tlayer_bytes\tlayer_features\tlayer_geometries\tlayer_attr_bytes\tlayer_attr_keys\tlayer_attr_values\n\
             3\t1\t2\t34\t999\ta\t85\t2\t2\t41\t2\t3\n\
             3\t1\t2\t34\t999\tb\t25\t1\t1\t0\t0\t0",
            (header_row() + &formatted.join("")).trim()
        );
    }

    #[test]
    fn issue1470_compute_mlt_fast_pfor_stats() {
        let bytes = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources/fastpfor.mlt")).unwrap();
        let mlt = crate::mlt::decoder::decode(&bytes).unwrap();
        let stats = compute_mlt_tile_stats(None, &mlt, &bytes).unwrap();
        assert_eq!(mlt.layers.len(), stats.len());
        assert!(stats.iter().all(|s| s.layer_bytes > 0 && s.layer_attr_bytes >= 0));
    }

    #[test]
    fn compute_stats_2_features_nested() {
        use crate::mlt::{
            ColumnMapping, ConversionConfig, FeatureTableOptimizations, TypeMismatchPolicy, convert_mvt,
            create_tileset_metadata,
        };
        let mut vector_tile = VectorTile::new();
        vector_tile.add_layer_features(
            "a",
            &[
                Feature::new(
                    "a",
                    1,
                    encode_geometry(&new_point(0.0, 0.0)),
                    tags! {"key:1" => "value1", "key:2" => "value2"},
                ),
                Feature::new(
                    "a",
                    2,
                    encode_geometry(&new_point(1.0, 1.0)),
                    tags! {"key:1" => "value2", "key:2" => "value1"},
                ),
            ],
        );
        let mlt_input = vector_tile.to_mlt_input(None);
        let mappings = vec![ColumnMapping::of_prefix("key", ":", true)];
        let tileset_metadata = create_tileset_metadata(&mlt_input, TypeMismatchPolicy::Fail, &mappings, true).unwrap();
        let config = ConversionConfig {
            include_ids: true,
            use_fsst: true,
            use_fast_pfor: false,
            optimizations: [(
                "a".to_string(),
                FeatureTableOptimizations {
                    allow_sorting: false,
                    allow_id_regeneration: false,
                    column_mappings: mappings.clone(),
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let mlt = convert_mvt(&mlt_input, &tileset_metadata, &config).unwrap();
        let stats = compute_mlt_tile_stats(Some(&vector_tile), &mlt_input, &mlt).unwrap();
        assert_eq!(1, stats.len());
        let entry1 = &stats[0];
        assert_eq!("a", entry1.layer);
        assert_eq!(2, entry1.layer_features);
        assert_eq!(95, entry1.layer_bytes);
        assert_eq!(50, entry1.layer_attr_bytes);
        assert_eq!(2, entry1.layer_attr_keys);
        assert_eq!(2, entry1.layer_attr_values);

        let formatted = new_thread_local_serializer().format_output_rows(TileCoord::of_xyz(1, 2, 3), 999, &stats);
        assert_eq!(
            "z\tx\ty\thilbert\tarchived_tile_bytes\tlayer\tlayer_bytes\tlayer_features\tlayer_geometries\tlayer_attr_bytes\tlayer_attr_keys\tlayer_attr_values\n\
             3\t1\t2\t34\t999\ta\t95\t2\t2\t50\t2\t2",
            (header_row() + &formatted.join("")).trim()
        );
        // and it decodes back to the flat properties
        let decoded = crate::mlt::decoder::decode(&mlt).unwrap();
        assert_eq!(tags! {"key:1" => "value1", "key:2" => "value2"}, decoded.layers[0].features[0].properties);
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
