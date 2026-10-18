//! Port of the `configschema` package: serde models of a YAML schema config file.
//!
//! Java `Object` fields (that can hold a constant, an expression string, a match structure, ...) are deserialized
//! into [`Value`]s (with mapping keys converted to strings, like Jackson does), and YAML `null` is treated the same as
//! a missing field. Like Jackson, unknown fields are rejected.

use std::path::Path;

use indexmap::IndexMap;
use planetiler_core::geo::geometry_type::GeometryType;
use planetiler_core::util::yaml;
use planetiler_core::value::{Tags, Value};
use serde::{Deserialize, Deserializer};

/// Default attribution when a schema does not set one.
pub const DEFAULT_ATTRIBUTION: &str =
    r#"<a href="https://www.openstreetmap.org/copyright" target="_blank">&copy; OpenStreetMap contributors</a>"#;

/// Deserialization helpers for lenient Jackson-like parsing.
pub(crate) mod de {
    use super::*;

    fn to_value(yaml_value: serde_yaml::Value) -> Option<Value> {
        match yaml::to_value(&yaml_value) {
            Value::Null => None,
            other => Some(other),
        }
    }

    /// An arbitrary value (`null` becomes `None`).
    pub fn opt_value<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
        Ok(to_value(serde_yaml::Value::deserialize(d)?))
    }

    /// A map of string keys to arbitrary values.
    pub fn opt_tags<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Tags>, D::Error> {
        match to_value(serde_yaml::Value::deserialize(d)?) {
            None => Ok(None),
            Some(Value::Map(map)) => Ok(Some((*map).clone())),
            Some(other) => Err(serde::de::Error::custom(format!("expected a map, got: {other}"))),
        }
    }

    /// A scalar coerced to a string.
    pub fn opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        match to_value(serde_yaml::Value::deserialize(d)?) {
            None => Ok(None),
            Some(v @ (Value::Map(_) | Value::List(_))) => {
                Err(serde::de::Error::custom(format!("expected a string, got: {v}")))
            }
            Some(other) => Ok(Some(other.to_string())),
        }
    }

    /// A single string or a list of strings (Jackson `ACCEPT_SINGLE_VALUE_AS_ARRAY`).
    pub fn string_or_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        match to_value(serde_yaml::Value::deserialize(d)?) {
            None => Ok(Vec::new()),
            Some(Value::List(items)) => Ok(items.iter().filter(|v| !v.is_null()).map(|v| v.to_string()).collect()),
            Some(v @ Value::Map(_)) => Err(serde::de::Error::custom(format!("expected a string or list, got: {v}"))),
            Some(other) => Ok(vec![other.to_string()]),
        }
    }

    /// A map of values (keys as strings) to zoom levels.
    pub fn opt_zoom_map<'de, D: Deserializer<'de>>(d: D) -> Result<Option<IndexMap<String, i32>>, D::Error> {
        match to_value(serde_yaml::Value::deserialize(d)?) {
            None => Ok(None),
            Some(Value::Map(map)) => map
                .iter()
                .map(|(k, v)| {
                    let zoom = planetiler_core::util::parse::parse_int_or_null(v)
                        .ok_or_else(|| serde::de::Error::custom(format!("expected an integer, got: {v}")))?;
                    Ok((k.clone(), zoom))
                })
                .collect::<Result<_, _>>()
                .map(Some),
            Some(other) => Err(serde::de::Error::custom(format!("expected a map, got: {other}"))),
        }
    }
}

/// Type of a data source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataSourceType {
    Osm,
    Shapefile,
    Geopackage,
    Geojson,
}

/// A data source definition (Java `DataSource`). `url`, `local_path` and `projection` may be expressions.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataSource {
    #[serde(rename = "type")]
    pub source_type: DataSourceType,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub url: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub local_path: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub projection: Option<Value>,
}

/// The geometry type of an output feature, and how to construct it from the input feature (Java `FeatureGeometry`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureGeometry {
    #[default]
    Any,
    Point,
    Line,
    Polygon,
    PolygonCentroid,
    LineCentroid,
    LineMidpoint,
    Centroid,
    PolygonCentroidIfConvex,
    PolygonPointOnSurface,
    PointOnLine,
    InnermostPoint,
    SplitLine,
}

impl FeatureGeometry {
    /// The type of input geometry this output can be built from.
    pub fn geometry_type(&self) -> GeometryType {
        use FeatureGeometry::*;
        match self {
            Any | Centroid | InnermostPoint => GeometryType::Unknown,
            Point => GeometryType::Point,
            Line | LineCentroid | LineMidpoint | PointOnLine | SplitLine => GeometryType::Line,
            Polygon | PolygonCentroid | PolygonCentroidIfConvex | PolygonPointOnSurface => GeometryType::Polygon,
        }
    }

    /// A test for whether a source feature is of the correct geometry to be included in the tile.
    pub fn feature_test(&self) -> planetiler_core::expression::expression::Expression {
        use planetiler_core::expression::expression::{Expression, match_geometry_type};
        match self.geometry_type() {
            GeometryType::Unknown => Expression::TRUE,
            other => match_geometry_type(other),
        }
    }

    /// Creates the output feature from `features` in `layer`.
    pub fn new_feature<'c, 'a>(
        &self,
        features: &'c mut planetiler_core::feature_collector::FeatureCollector<'a>,
        layer: &str,
    ) -> &'c mut planetiler_core::feature_collector::Feature<'a> {
        use FeatureGeometry::*;
        match self {
            Any => features.any_geometry(layer),
            Point => features.point(layer),
            Line => features.line(layer),
            Polygon => features.polygon(layer),
            PolygonCentroid | LineCentroid | Centroid => features.centroid(layer),
            LineMidpoint => features.line_midpoint(layer),
            PolygonCentroidIfConvex => features.centroid_if_convex(layer),
            PolygonPointOnSurface | PointOnLine => features.point_on_surface(layer),
            InnermostPoint => features.innermost_point(layer),
            SplitLine => features.split_line(layer),
        }
    }
}

/// Configuration for an attribute of an output feature (Java `AttributeDefinition`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttributeDefinition {
    #[serde(default)]
    pub key: String,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub include_when: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub exclude_when: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub min_zoom: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_zoom_map")]
    pub min_zoom_by_value: Option<IndexMap<String, i32>>,
    #[serde(default)]
    pub min_tile_cover_size: Option<f64>,
    #[serde(default, rename = "else", deserialize_with = "de::opt_value")]
    pub fallback: Option<Value>,
    /// Pass-through to value expression.
    #[serde(default, deserialize_with = "de::opt_value")]
    pub value: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_string")]
    pub tag_value: Option<String>,
    #[serde(default, deserialize_with = "de::opt_string")]
    pub arg_value: Option<String>,
    #[serde(default, rename = "type", deserialize_with = "de::opt_value")]
    pub attr_type: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub coalesce: Option<Value>,
}

/// `point_label_grid.pixel_size`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointLabelGridPixelSize {
    #[serde(default, rename = "maxzoom")]
    pub max_zoom: i32,
    #[serde(default)]
    pub value: f64,
}

/// `point_label_grid.limit`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointLabelGridLimit {
    #[serde(default, rename = "maxzoom")]
    pub max_zoom: i32,
    #[serde(default)]
    pub value: i32,
}

/// Label grid configuration for points.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointLabelGrid {
    #[serde(default)]
    pub pixel_size: Option<PointLabelGridPixelSize>,
    #[serde(default)]
    pub limit: Option<PointLabelGridLimit>,
}

/// A feature in an output layer, and how to generate it from input features (Java `FeatureItem`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureItem {
    #[serde(default, deserialize_with = "de::string_or_list")]
    pub source: Vec<String>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub id: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub min_zoom: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub max_zoom: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub min_size: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub min_size_at_max_zoom: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub tolerance: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub tolerance_at_max_zoom: Option<Value>,
    #[serde(default)]
    pub point_label_grid: Option<PointLabelGrid>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub sort_key: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub sort_key_descending: Option<Value>,
    #[serde(default, deserialize_with = "opt_geometry")]
    pub geometry: FeatureGeometry,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub include_when: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub exclude_when: Option<Value>,
    #[serde(default, deserialize_with = "opt_list")]
    pub attributes: Vec<AttributeDefinition>,
}

fn opt_geometry<'de, D: Deserializer<'de>>(d: D) -> Result<FeatureGeometry, D::Error> {
    Ok(Option::<FeatureGeometry>::deserialize(d)?.unwrap_or_default())
}

fn opt_list<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

/// `tile_post_process.merge_line_strings`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeLineStrings {
    #[serde(default)]
    pub min_length: Option<f64>,
    #[serde(default)]
    pub min_length_at_max_zoom: Option<f64>,
    #[serde(default)]
    pub tolerance: Option<f64>,
    #[serde(default)]
    pub tolerance_at_max_zoom: Option<f64>,
    #[serde(default)]
    pub buffer: Option<f64>,
}

/// `tile_post_process.merge_polygons`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergePolygons {
    #[serde(default)]
    pub min_area: Option<f64>,
    #[serde(default)]
    pub min_area_at_max_zoom: Option<f64>,
    #[serde(default)]
    pub tolerance: Option<f64>,
    #[serde(default)]
    pub tolerance_at_max_zoom: Option<f64>,
}

/// Post-processing to apply to features in a layer of each tile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostProcess {
    #[serde(default)]
    pub merge_line_strings: Option<MergeLineStrings>,
    #[serde(default)]
    pub merge_polygons: Option<MergePolygons>,
}

/// An output vector tile layer (Java `FeatureLayer`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureLayer {
    #[serde(default)]
    pub id: String,
    #[serde(default, deserialize_with = "opt_list")]
    pub features: Vec<FeatureItem>,
    #[serde(default)]
    pub buffer: Option<f64>,
    #[serde(default, rename = "tile_post_process")]
    pub post_process: Option<PostProcess>,
}

/// Overrides the default zoom range for features which contain specific tag combinations (Java `ZoomOverride`,
/// unused by the schema).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ZoomOverride {
    pub min: Option<i32>,
    pub max: Option<i32>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub tag: Option<Value>,
}

/// An object representation of a vector tile server schema, mapped from a schema YAML file.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaConfig {
    #[serde(default, deserialize_with = "de::opt_string")]
    pub schema_name: Option<String>,
    #[serde(default, deserialize_with = "de::opt_string")]
    pub schema_description: Option<String>,
    #[serde(default, deserialize_with = "de::opt_string")]
    attribution: Option<String>,
    #[serde(default, deserialize_with = "de::opt_string")]
    pub version: Option<String>,
    #[serde(default)]
    is_overlay: Option<bool>,
    #[serde(default, deserialize_with = "opt_map")]
    pub sources: IndexMap<String, DataSource>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub definitions: Option<Value>,
    #[serde(default, rename = "tag_mappings", deserialize_with = "de::opt_tags")]
    pub input_mappings: Option<Tags>,
    #[serde(default)]
    pub layers: Option<Vec<FeatureLayer>>,
    #[serde(default, deserialize_with = "de::opt_value")]
    pub examples: Option<Value>,
    #[serde(default, deserialize_with = "de::opt_tags")]
    args: Option<Tags>,
}

fn opt_map<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<IndexMap<String, T>, D::Error> {
    Ok(Option::<IndexMap<String, T>>::deserialize(d)?.unwrap_or_default())
}

impl SchemaConfig {
    /// The attribution, defaulting to [`DEFAULT_ATTRIBUTION`].
    pub fn attribution(&self) -> &str {
        self.attribution.as_deref().unwrap_or(DEFAULT_ATTRIBUTION)
    }

    /// Argument definitions from the schema (empty if missing).
    pub fn args(&self) -> &Tags {
        static EMPTY: std::sync::LazyLock<Tags> = std::sync::LazyLock::new(Tags::default);
        self.args.as_ref().unwrap_or(&EMPTY)
    }

    pub fn is_overlay(&self) -> bool {
        self.is_overlay.unwrap_or(false)
    }

    /// Loads a schema from a YAML file (anchors, aliases and `<<` merge keys are supported).
    pub fn load(path: &Path) -> anyhow::Result<SchemaConfig> {
        yaml::load_file(path)
    }

    /// Loads a schema from a YAML string.
    pub fn load_str(string: &str) -> anyhow::Result<SchemaConfig> {
        yaml::load(string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_minimal() {
        let schema = SchemaConfig::load_str(
            r#"
sources:
  osm:
    type: osm
    url: geofabrik:rhode-island
layers:
- id: testLayer
  features:
  - source: osm
    geometry: point
    attributes:
    - key: a
      min_zoom_by_value:
        4: 9
        x: 3
"#,
        )
        .unwrap();
        assert_eq!(DEFAULT_ATTRIBUTION, schema.attribution());
        assert!(!schema.is_overlay());
        assert!(schema.args().is_empty());
        let layers = schema.layers.unwrap();
        let feature = &layers[0].features[0];
        assert_eq!(vec!["osm".to_string()], feature.source);
        assert_eq!(FeatureGeometry::Point, feature.geometry);
        let by_value = feature.attributes[0].min_zoom_by_value.as_ref().unwrap();
        assert_eq!(Some(&9), by_value.get("4"));
        assert_eq!(Some(&3), by_value.get("x"));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(
            SchemaConfig::load_str("layers:\n- id: a\n  features:\n  - geometry: smurf\n").is_err(),
            "bad geometry"
        );
        assert!(
            SchemaConfig::load_str("layers:\n- id: a\n  tile_post_process:\n    merge_everything: {}\n").is_err(),
            "bad post process"
        );
    }
}
