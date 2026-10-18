//! Port of `org.maplibre.mlt.converter.MltConverter` (`createTilesetMetadata` and `convertMvt`) with its
//! configuration classes (`ConversionConfig`, `FeatureTableOptimizations`, `ColumnMapping`).

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use super::encoding::put_varint;
use super::geometry_encoder::{encode_geometry_column, encode_pretessellated_geometry_column};
use super::metadata::{Column, ColumnType, FeatureTable, ScalarType, TileSetMetadata};
use super::property_encoder::{encode_property_columns, encode_scalar_property_column};
use super::{MapboxVectorTile, MltFeature};
use crate::value::Value;

/// `ConversionConfig.TypeMismatchPolicy`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TypeMismatchPolicy {
    /// Coerce values to string on type mismatch.
    Coerce,
    /// Skip values that don't match the first type encountered.
    Elide,
    /// Return an error if a type mismatch is detected (default).
    #[default]
    Fail,
}

/// `ColumnMapping`: maps a set of flat MVT properties to one nested MLT column so its string fields share a
/// dictionary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnMapping {
    matcher: ColumnMatcher,
    pub use_shared_dictionary_encoding: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ColumnMatcher {
    /// Explicit column names.
    Names(HashSet<String>),
    /// A literal prefix, matching the prefix alone or `prefix + delimiter + suffix`.
    Prefix { prefix: String, delimiter: String },
}

impl ColumnMapping {
    /// Construct a mapping based on explicit column names.
    pub fn of_columns(names: impl IntoIterator<Item = String>, use_shared_dictionary_encoding: bool) -> ColumnMapping {
        ColumnMapping { matcher: ColumnMatcher::Names(names.into_iter().collect()), use_shared_dictionary_encoding }
    }

    /// Construct a mapping based on a common (literal) prefix and delimiter.
    pub fn of_prefix(prefix: &str, delimiter: &str, use_shared_dictionary_encoding: bool) -> ColumnMapping {
        ColumnMapping {
            matcher: ColumnMatcher::Prefix { prefix: prefix.to_string(), delimiter: delimiter.to_string() },
            use_shared_dictionary_encoding,
        }
    }

    /// `ColumnMapping.isMatch`.
    pub fn is_match(&self, property_name: &str) -> bool {
        match &self.matcher {
            ColumnMatcher::Names(names) if !names.is_empty() => names.contains(property_name),
            ColumnMatcher::Names(_) => false,
            ColumnMatcher::Prefix { prefix, delimiter } => match property_name.strip_prefix(prefix.as_str()) {
                Some(remainder) => remainder.is_empty() || remainder.starts_with(delimiter.as_str()),
                None => false,
            },
        }
    }
}

/// `FeatureTableOptimizations`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeatureTableOptimizations {
    /// The feature table can be sorted (otherwise the specified order has to be preserved).
    pub allow_sorting: bool,
    /// Ids can be reassigned to features.
    pub allow_id_regeneration: bool,
    /// Mappings of flat MVT properties to nested MLT columns sharing a dictionary.
    pub column_mappings: Vec<ColumnMapping>,
}

/// `ConversionConfig` (integer encodings are always `AUTO`, there is no layer filter).
#[derive(Clone, Debug, PartialEq)]
pub struct ConversionConfig {
    pub include_ids: bool,
    pub use_fast_pfor: bool,
    pub use_fsst: bool,
    pub mismatch_policy: TypeMismatchPolicy,
    pub optimizations: HashMap<String, FeatureTableOptimizations>,
    pub pre_tessellate_polygons: bool,
    pub use_morton_encoding: bool,
    /// Layer names for which to include polygon outlines in pre-tessellated geometry columns, or `ALL`.
    pub outline_feature_table_names: Vec<String>,
}

impl Default for ConversionConfig {
    /// `ConversionConfig.builder().build()` defaults.
    fn default() -> Self {
        ConversionConfig {
            include_ids: true,
            use_fast_pfor: false,
            use_fsst: false,
            mismatch_policy: TypeMismatchPolicy::Fail,
            optimizations: HashMap::new(),
            pre_tessellate_polygons: false,
            use_morton_encoding: true,
            outline_feature_table_names: Vec::new(),
        }
    }
}

/// Java `String.compareTo`: compares UTF-16 code units.
fn java_string_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    if a.is_ascii() && b.is_ascii() { a.cmp(b) } else { a.encode_utf16().cmp(b.encode_utf16()) }
}

/// `MltConverter.getScalarType`.
fn get_scalar_type(value: &Value) -> anyhow::Result<ScalarType> {
    Ok(match value {
        Value::Bool(_) => ScalarType::Boolean,
        Value::Int(i) => {
            if i32::try_from(*i).is_ok() {
                ScalarType::Int32
            } else {
                ScalarType::Int64
            }
        }
        Value::Float(_) => ScalarType::Float,
        Value::Double(_) => ScalarType::Double,
        Value::String(_) => ScalarType::String,
        other => anyhow::bail!("Specified data type currently not supported: {other:?}"),
    })
}

/// Columns by property name, in insertion order.
type ColumnSchemas = IndexMap<String, Column, crate::value::TagsHasher>;

/// `MltConverter.createTilesetMetadata`: infers the column schema of each layer. `column_mappings` apply to every
/// layer (Java `ColumnMappingConfig.of(Pattern.compile(".*"), columnMappings)`).
pub fn create_tileset_metadata<F: MltFeature>(
    tile: &MapboxVectorTile<F>,
    policy: TypeMismatchPolicy,
    column_mappings: &[ColumnMapping],
    include_id_if_present: bool,
) -> anyhow::Result<TileSetMetadata> {
    let mut tileset = TileSetMetadata::default();
    for layer in &tile.layers {
        let mut column_schemas: ColumnSchemas = IndexMap::with_hasher(crate::value::TagsHasher);
        // insertion ordered by the first time each mapping matched
        let mut complex_schemas: Vec<(usize, Vec<Column>)> = Vec::new();
        let mut has_id = false;
        let mut has_long_id = false;
        let mut has_null_id = false;
        let mut sorted: Vec<(&str, &Value)> = Vec::new();
        for (feature_index, feature) in layer.features.iter().enumerate() {
            sorted.clear();
            sorted.extend(feature.properties());
            sorted.sort_by(|a, b| java_string_cmp(a.0, b.0));
            for &(name, value) in &sorted {
                resolve_column_type(
                    name,
                    value,
                    &layer.name,
                    feature_index,
                    column_mappings,
                    &mut column_schemas,
                    &mut complex_schemas,
                    policy,
                )?;
            }
            if include_id_if_present {
                match feature.id() {
                    Some(id) => {
                        has_id = true;
                        if (!has_long_id && id > i32::MAX as i64) || id < i32::MIN as i64 {
                            has_long_id = true;
                        }
                    }
                    None => has_null_id = true,
                }
            }
        }

        for (_, mut children) in complex_schemas {
            let parent_name = resolve_complex_column_mapping(&mut children);
            // Each complex column scheme needs to have a unique entry name in the column map
            let mut i = 0;
            let key = loop {
                let key = format!("{parent_name}{i}");
                if !column_schemas.contains_key(&key) {
                    break key;
                }
                i += 1;
            };
            column_schemas
                .insert(key, Column { name: parent_name, nullable: false, column_type: ColumnType::Struct(children) });
        }

        let mut table = FeatureTable { name: layer.name.clone(), columns: Vec::new() };
        if has_id {
            table.columns.push(Column {
                name: String::new(),
                nullable: has_null_id,
                column_type: ColumnType::Id { long: has_long_id },
            });
        }
        // The `geometry` column is mandatory and has to be the first column after `ID`
        table.columns.push(Column { name: String::new(), nullable: false, column_type: ColumnType::Geometry });
        table.columns.extend(column_schemas.into_values());
        tileset.feature_tables.push(table);
    }
    Ok(tileset)
}

#[allow(clippy::too_many_arguments)]
fn resolve_column_type(
    name: &str,
    value: &Value,
    layer_name: &str,
    feature_index: usize,
    column_mappings: &[ColumnMapping],
    column_schemas: &mut ColumnSchemas,
    complex_schemas: &mut Vec<(usize, Vec<Column>)>,
    policy: TypeMismatchPolicy,
) -> anyhow::Result<()> {
    // MVT can only contain scalar types
    let scalar_type = get_scalar_type(value)?;
    // If this property already has a column, make sure the types match
    if let Some(previous) = column_schemas.get_mut(name)
        && let ColumnType::Scalar(prev) = previous.column_type
    {
        if prev == scalar_type {
            // Java puts an equal column again (columns are only created for properties without a mapping, and
            // always nullable)
            return Ok(());
        }
        use ScalarType::*;
        match (prev, scalar_type) {
            // Allow implicit upgrade from INT_32 to INT_64 and FLOAT to DOUBLE
            (Int32, Int64) => previous.column_type = ColumnType::Scalar(Int64),
            (Float, Double) => previous.column_type = ColumnType::Scalar(Double),
            (Int64, Int32) | (Double, Float) => {}
            _ if policy == TypeMismatchPolicy::Coerce => {
                previous.column_type = ColumnType::Scalar(String);
            }
            _ if policy == TypeMismatchPolicy::Elide => {}
            _ => anyhow::bail!(
                "Layer '{layer_name}' Feature index {feature_index} Property '{name}' has different type: {scalar_type:?} / {prev:?}"
            ),
        }
        return Ok(());
    }

    if let Some(mapping_index) = column_mappings.iter().position(|m| m.is_match(name)) {
        // A mapping exists for this property: create the parent type and add a child type entry.
        let children = match complex_schemas.iter().position(|(i, _)| *i == mapping_index) {
            Some(pos) => &mut complex_schemas[pos].1,
            None => {
                complex_schemas.push((mapping_index, Vec::new()));
                &mut complex_schemas.last_mut().expect("just pushed").1
            }
        };
        if !children.iter().any(|c| c.name == name) {
            children.push(Column::scalar(name, true, scalar_type));
        }
        return Ok(());
    }

    // no matching column mappings, create a plain scalar column
    column_schemas.insert(name.to_string(), Column::scalar(name, true, scalar_type));
    Ok(())
}

/// `resolveComplexColumnMapping`: strips the longest common prefix from the child names and returns it.
fn resolve_complex_column_mapping(children: &mut [Column]) -> String {
    let Some(first) = children.first() else { return String::new() };
    let mut prefix_len = first.name.len();
    for child in children.iter().skip(1) {
        let common = first
            .name
            .char_indices()
            .zip(child.name.chars())
            .find(|((_, a), b)| a != b)
            .map_or_else(|| first.name.len().min(child.name.len()), |((i, _), _)| i);
        // keep the prefix on a char boundary of both names
        let mut common = common.min(child.name.len());
        while !first.name.is_char_boundary(common) {
            common -= 1;
        }
        prefix_len = prefix_len.min(common);
    }
    let prefix = first.name[..prefix_len].to_string();
    if !prefix.is_empty() {
        for child in children.iter_mut() {
            child.name = child.name[prefix.len()..].to_string();
        }
    }
    prefix
}

/// A feature with a regenerated ID (its position).
struct RegeneratedId<'a, F> {
    id: i64,
    feature: &'a F,
}

impl<F: MltFeature> MltFeature for RegeneratedId<'_, F> {
    fn id(&self) -> Option<i64> {
        Some(self.id)
    }

    fn geometry(&self) -> &geo_types::Geometry<f64> {
        self.feature.geometry()
    }

    fn property(&self, name: &str) -> Option<&Value> {
        self.feature.property(name)
    }

    fn properties(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.feature.properties()
    }
}

struct BodyConfig<'a> {
    config: &'a ConversionConfig,
    layer_metadata: &'a FeatureTable,
    layer_name: &'a str,
    optimizations: Option<&'a FeatureTableOptimizations>,
    coerce: bool,
}

/// The ID column, the geometry column and the property columns of a layer.
fn encode_layer_body<F: MltFeature>(
    body: &mut Vec<u8>,
    c: &BodyConfig,
    sorted: &[&F],
    geometry_column: &super::geometry_encoder::EncodedGeometryColumn,
) -> anyhow::Result<()> {
    let config = c.config;
    let fast_pfor = config.use_fast_pfor;
    if config.include_ids {
        let id_metadata = c
            .layer_metadata
            .columns
            .iter()
            .find(|c| c.is_id())
            .ok_or_else(|| anyhow::anyhow!("No ID column in layer {}", c.layer_name))?;
        let ColumnType::Id { long } = id_metadata.column_type else { unreachable!("is_id") };
        // Write ID as a 32- or 64-bit scalar; the decoding assumes unsigned (no zigzag)
        let id_column =
            Column::scalar("id", id_metadata.nullable, if long { ScalarType::Uint64 } else { ScalarType::Uint32 });
        encode_scalar_property_column(body, &id_column, true, sorted, fast_pfor, config.use_fsst, c.coerce)?;
    }
    put_varint(body, geometry_column.num_streams);
    body.extend_from_slice(&geometry_column.encoded);
    let property_columns: Vec<&Column> =
        c.layer_metadata.columns.iter().filter(|c| !c.is_id() && !c.is_geometry()).collect();
    let column_mappings: &[ColumnMapping] = c.optimizations.map_or(&[], |o| &o.column_mappings);
    encode_property_columns(body, &property_columns, sorted, fast_pfor, config.use_fsst, c.coerce, column_mappings)
}

/// `MltConverter.convertMvt`: encodes `mvt` with the schema in `tileset_metadata`.
pub fn convert_mvt<F: MltFeature>(
    mvt: &MapboxVectorTile<F>,
    tileset_metadata: &TileSetMetadata,
    config: &ConversionConfig,
) -> anyhow::Result<Vec<u8>> {
    let mut meta_map: HashMap<&str, &FeatureTable> = HashMap::with_capacity(tileset_metadata.feature_tables.len());
    for table in &tileset_metadata.feature_tables {
        if meta_map.insert(&table.name, table).is_some() {
            anyhow::bail!("duplicate key {}", table.name);
        }
    }
    let fast_pfor = config.use_fast_pfor;
    let coerce = config.mismatch_policy == TypeMismatchPolicy::Coerce;
    let mut tile = Vec::new();
    for layer in &mvt.layers {
        let layer_metadata = *meta_map.get(layer.name.as_str()).ok_or_else(|| anyhow::anyhow!("Missing Metadata"))?;
        if layer.features.is_empty() {
            continue;
        }
        let optimizations = config.optimizations.get(&layer.name);
        let create_polygon_outline = config.outline_feature_table_names.iter().any(|n| n == &layer.name || n == "ALL");

        // sortFeaturesAndEncodeGeometryColumn
        let is_column_sortable = config.include_ids && optimizations.is_some_and(|o| o.allow_sorting);
        let allow_id_regeneration = optimizations.is_some_and(|o| o.allow_id_regeneration);
        let mut sorted: Vec<&F> = layer.features.iter().collect();
        if is_column_sortable && !allow_id_regeneration {
            sorted.sort_by_key(|f| (f.id().is_some(), f.id().unwrap_or(0)));
        }
        if is_column_sortable && allow_id_regeneration {
            anyhow::bail!("Sorting geometries with regenerated ids is not supported");
        }
        let geometries: Vec<&geo_types::Geometry<f64>> = sorted.iter().map(|f| f.geometry()).collect();
        let geometry_column = if config.pre_tessellate_polygons {
            encode_pretessellated_geometry_column(&geometries, fast_pfor, create_polygon_outline)?
        } else {
            encode_geometry_column(&geometries, fast_pfor, config.use_morton_encoding)?
        };
        let mut body = Vec::new();
        let body_config = BodyConfig { config, layer_metadata, layer_name: &layer.name, optimizations, coerce };
        if config.include_ids && allow_id_regeneration {
            let regenerated: Vec<RegeneratedId<F>> =
                sorted.iter().enumerate().map(|(i, &feature)| RegeneratedId { id: i as i64, feature }).collect();
            let regenerated: Vec<&RegeneratedId<F>> = regenerated.iter().collect();
            encode_layer_body(&mut body, &body_config, &regenerated, &geometry_column)?;
        } else {
            encode_layer_body(&mut body, &body_config, &sorted, &geometry_column)?;
        }

        let mut metadata_buffer = Vec::new();
        layer_metadata.encode_embedded(layer.extent, &mut metadata_buffer)?;
        let tag = 1u32;
        let tag_length = 1 + metadata_buffer.len() + body.len();
        put_varint(&mut tile, tag_length as u32);
        put_varint(&mut tile, tag);
        tile.extend_from_slice(&metadata_buffer);
        tile.extend_from_slice(&body);
    }
    Ok(tile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_mapping_matches() {
        let m = ColumnMapping::of_prefix("name", ":", true);
        assert!(m.is_match("name"));
        assert!(m.is_match("name:en"));
        assert!(!m.is_match("name_en"));
        assert!(!m.is_match("xname"));
        let m = ColumnMapping::of_columns(["a".to_string(), "b".to_string()], true);
        assert!(m.is_match("a"));
        assert!(!m.is_match("c"));
    }

    #[test]
    fn common_prefix() {
        let mut children =
            vec![Column::scalar("name", true, ScalarType::String), Column::scalar("name:en", true, ScalarType::String)];
        assert_eq!("name", resolve_complex_column_mapping(&mut children));
        assert_eq!("", children[0].name);
        assert_eq!(":en", children[1].name);
        let mut children =
            vec![Column::scalar("abc", true, ScalarType::String), Column::scalar("xyz", true, ScalarType::String)];
        assert_eq!("", resolve_complex_column_mapping(&mut children));
        assert_eq!("abc", children[0].name);
        let mut children = vec![Column::scalar("only", true, ScalarType::String)];
        assert_eq!("only", resolve_complex_column_mapping(&mut children));
        assert_eq!("", children[0].name);
    }
}
