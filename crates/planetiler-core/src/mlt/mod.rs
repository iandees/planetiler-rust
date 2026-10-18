//! MapLibre Tile (MLT) encoding: a port of the parts of the Java MLT library (`org.maplibre:mlt:0.0.10`, Apache-2.0,
//! [maplibre-tile-spec](https://github.com/maplibre/maplibre-tile-spec) `java/` at tag `java-v0.0.10`) that
//! Planetiler uses, so that Rust produces the same bytes as Java Planetiler.
//!
//! The encoder (`MltConverter.createTilesetMetadata` + `MltConverter.convertMvt`) lives in [`converter`], with the
//! stream encoders in [`integer_encoder`], [`string_encoder`], [`property_encoder`] and [`geometry_encoder`], and
//! the third-party codecs the Java library pulls in ported alongside: [`fastpfor`] (JavaFastPFOR 0.3.10), [`fsst`]
//! (the Java FSST implementation) and [`earcut`] (earcut4j 3.0.0). [`decoder`] decodes the subset of MLT these
//! encoders produce, which is used for tests, `compare` and tile size stats.
//!
//! [`encode_tile`] is the MLT branch of Java `TileArchiveWriter` (shared dictionary column detection plus the
//! conversion config built from the `mlt_*` options).
//!
//! See PORTING.md ("MLT output") for why this is a port rather than a use of the `mlt-core` crate.

pub mod converter;
pub mod decoder;
pub mod earcut;
pub mod encoding;
pub mod fastpfor;
pub mod fsst;
pub mod geometry_encoder;
pub mod integer_encoder;
pub mod metadata;
pub mod property_encoder;
pub mod string_encoder;

use std::collections::{HashMap, HashSet};

use geo_types::Geometry;

pub use converter::{
    ColumnMapping, ConversionConfig, FeatureTableOptimizations, TypeMismatchPolicy, convert_mvt,
    create_tileset_metadata,
};

use crate::value::{Tags, Value};

/// `org.maplibre.mlt.data.Feature`: geometry in tile extent coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct Feature {
    /// `None` if the feature has no ID.
    pub id: Option<i64>,
    pub geometry: Geometry<f64>,
    pub properties: Tags,
}

/// `org.maplibre.mlt.data.Layer`.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub name: String,
    pub features: Vec<Feature>,
    pub extent: u32,
}

/// `org.maplibre.mlt.converter.mvt.MapboxVectorTile`: the input of the MLT encoder.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MapboxVectorTile {
    pub layers: Vec<Layer>,
}

/// The `mlt_*` options of [`PlanetilerConfig`](crate::config::planetiler_config::PlanetilerConfig).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MltOptions {
    pub fsst: bool,
    pub fast_pfor: bool,
    pub tessellate_polygons: bool,
    pub polygon_outline: bool,
    pub reorder_features: bool,
    pub shared_dictionaries: bool,
}

impl MltOptions {
    pub fn from_config(config: &crate::config::planetiler_config::PlanetilerConfig) -> MltOptions {
        MltOptions {
            fsst: config.mlt_fsst,
            fast_pfor: config.mlt_fast_pfor,
            tessellate_polygons: config.mlt_tessellate_polygons,
            polygon_outline: config.mlt_polygon_outline,
            reorder_features: config.mlt_reorder_feature,
            shared_dictionaries: config.mlt_shared_dictionaries,
        }
    }
}

/// `TileArchiveWriter.findStringColumns`: when the tile has enough repeated string values for a shared dictionary to
/// pay off, returns the properties that only ever hold strings, overall and per layer.
fn find_string_columns(input: &MapboxVectorTile) -> (HashSet<&str>, HashMap<&str, HashSet<&str>>) {
    let mut value_counts: HashMap<&str, u32> = HashMap::new();
    let mut string_columns: HashSet<&str> = HashSet::new();
    let mut string_columns_by_layer: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut not_string_columns: HashSet<&str> = HashSet::new();
    let mut num_repeats = 0;
    let mut have_enough_repeated_values = false;
    for layer in &input.layers {
        for feature in &layer.features {
            for (key, value) in &feature.properties {
                if let Value::String(value) = value {
                    if !have_enough_repeated_values {
                        let count = value_counts.entry(value).or_insert(0);
                        *count += 1;
                        if *count > 1 {
                            num_repeats += if *count == 2 { 2 } else { 1 };
                            if num_repeats >= 50 {
                                have_enough_repeated_values = true;
                            }
                        }
                    }
                    string_columns.insert(key);
                    string_columns_by_layer.entry(&layer.name).or_default().insert(key);
                } else {
                    not_string_columns.insert(key);
                }
            }
        }
    }
    // you need enough repeated values for the overhead of the offsets and indices to be worth it
    // testing showed that you need >~50 shared strings for deduplication benefit to outweigh shared dictionary cost
    if !have_enough_repeated_values {
        string_columns.clear();
        string_columns_by_layer.clear();
    } else {
        string_columns.retain(|c| !not_string_columns.contains(c));
        for columns in string_columns_by_layer.values_mut() {
            columns.retain(|c| !not_string_columns.contains(c));
        }
    }
    (string_columns, string_columns_by_layer)
}

/// Encodes `input` the way Java `TileArchiveWriter` does for `--tile-format=mlt`.
pub fn encode_tile(input: &MapboxVectorTile, include_ids: bool, options: &MltOptions) -> anyhow::Result<Vec<u8>> {
    let (string_columns, string_columns_by_layer) =
        if options.shared_dictionaries { find_string_columns(input) } else { (HashSet::new(), HashMap::new()) };
    let column_mappings: Vec<ColumnMapping> = if string_columns.is_empty() {
        Vec::new()
    } else {
        vec![ColumnMapping::of_columns(string_columns.iter().map(|s| s.to_string()), true)]
    };
    let tileset_metadata = create_tileset_metadata(input, TypeMismatchPolicy::Coerce, &column_mappings, include_ids)?;
    let optimizations = input
        .layers
        .iter()
        .map(|layer| {
            let layer_string_columns = string_columns_by_layer.get(layer.name.as_str());
            let mappings = match layer_string_columns {
                Some(columns) if !columns.is_empty() => {
                    vec![ColumnMapping::of_columns(columns.iter().map(|s| s.to_string()), true)]
                }
                _ => Vec::new(),
            };
            (
                layer.name.clone(),
                FeatureTableOptimizations {
                    allow_sorting: options.reorder_features,
                    allow_id_regeneration: !include_ids,
                    column_mappings: mappings,
                },
            )
        })
        .collect();
    let config = ConversionConfig {
        include_ids,
        use_fast_pfor: options.fast_pfor,
        use_fsst: options.fsst,
        mismatch_policy: TypeMismatchPolicy::Coerce,
        optimizations,
        pre_tessellate_polygons: options.tessellate_polygons,
        use_morton_encoding: true,
        outline_feature_table_names: if options.polygon_outline { vec!["ALL".to_string()] } else { Vec::new() },
    };
    convert_mvt(input, &tileset_metadata, &config)
}

#[cfg(test)]
mod tests;
