//! Port of `validator/SchemaSpecification.java`: a model of example input source features and expected output vector
//! tile features that a schema should produce.
//!
//! Executed by an implementation of
//! [`BaseSchemaValidator`](crate::validator::base_schema_validator::BaseSchemaValidator), or directly with
//! [`base_schema_validator::validate`](crate::validator::base_schema_validator::validate).
//!
//! ```yaml
//! examples:
//! - name: water polygon
//!   input:
//!     source: osm
//!     geometry: polygon        # "point", "line", "polygon" or a WKT string (lat/lon)
//!     tags:
//!       natural: water
//!   output:                    # a single feature or a list of features
//!   - layer: water
//!     geometry: polygon
//!     min_zoom: 0
//!     min_size: 10
//!     at_zoom: 14              # zoom to evaluate attributes at (default 16)
//!     allow_extra_tags: false  # default true
//!     tags:
//!       natural: water
//! ```

use std::path::Path;

use indexmap::IndexMap;
use serde::Deserialize;
use serde::de::{self, Deserializer};

use crate::config::planetiler_config::MAX_MAXZOOM;
use crate::geo::geometry_type::GeometryType;
use crate::util::yaml;
use crate::value::{Tags, Value};

/// A set of examples (test cases) for a schema.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct SchemaSpecification {
    #[serde(default, deserialize_with = "null_as_default")]
    pub examples: Vec<Example>,
}

impl SchemaSpecification {
    /// Reads and parses a specification from the YAML file at `path`.
    pub fn load(path: &Path) -> anyhow::Result<SchemaSpecification> {
        yaml::load_file(path)
    }

    /// Parses a specification from a YAML string.
    pub fn load_str(string: &str) -> anyhow::Result<SchemaSpecification> {
        yaml::load(string)
    }
}

/// An individual test case.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Example {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input: InputFeature,
    /// Expected output features (a single feature in the YAML is accepted as a one-element list).
    #[serde(default, deserialize_with = "one_or_many")]
    pub output: Vec<OutputFeature>,
}

/// Description of an input feature from a source that the schema will process.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct InputFeature {
    #[serde(default)]
    pub source: Option<String>,
    /// `point`, `line`, `polygon` or a WKT string in latitude/longitude coordinates.
    #[serde(default)]
    pub geometry: String,
    /// Source feature tags; scalar values are converted to strings (like Java's `Map<String, String>`).
    #[serde(default, deserialize_with = "string_tags")]
    pub tags: IndexMap<String, String>,
}

/// Description of an expected vector tile feature that the schema should produce.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct OutputFeature {
    #[serde(default)]
    pub layer: Option<String>,
    #[serde(default, deserialize_with = "geometry_type")]
    pub geometry: Option<GeometryType>,
    #[serde(default)]
    pub min_zoom: Option<u32>,
    #[serde(default)]
    pub max_zoom: Option<u32>,
    #[serde(default)]
    pub min_size: Option<f64>,
    #[serde(default, rename = "at_zoom")]
    pub at_zoom_opt: Option<u32>,
    #[serde(default, rename = "allow_extra_tags")]
    pub allow_extra_tags_opt: Option<bool>,
    /// Expected attributes; a `null` value means the attribute must be absent.
    #[serde(default, deserialize_with = "value_tags")]
    pub tags: Tags,
}

impl OutputFeature {
    /// Zoom level to evaluate attributes at, defaulting to [`MAX_MAXZOOM`].
    pub fn at_zoom(&self) -> u32 {
        self.at_zoom_opt.unwrap_or(MAX_MAXZOOM)
    }

    /// Whether the actual feature may have attributes not listed in [`OutputFeature::tags`] (default `true`).
    pub fn allow_extra_tags(&self) -> bool {
        self.allow_extra_tags_opt.unwrap_or(true)
    }
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

fn one_or_many<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<OutputFeature>, D::Error> {
    Ok(match Option::<serde_yaml::Value>::deserialize(deserializer)? {
        None | Some(serde_yaml::Value::Null) => Vec::new(),
        Some(list @ serde_yaml::Value::Sequence(_)) => serde_yaml::from_value(list).map_err(de::Error::custom)?,
        Some(one) => vec![serde_yaml::from_value(one).map_err(de::Error::custom)?],
    })
}

fn geometry_type<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<GeometryType>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|s| match s.as_str() {
            "point" => Ok(GeometryType::Point),
            "line" => Ok(GeometryType::Line),
            "polygon" => Ok(GeometryType::Polygon),
            "UNKNOWN" => Ok(GeometryType::Unknown),
            other => Err(de::Error::unknown_variant(other, &["point", "line", "polygon", "UNKNOWN"])),
        })
        .transpose()
}

fn value_tags<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Tags, D::Error> {
    let map = Option::<IndexMap<String, serde_yaml::Value>>::deserialize(deserializer)?.unwrap_or_default();
    Ok(map.into_iter().map(|(k, v)| (k, yaml::to_value(&v))).collect())
}

fn string_tags<'de, D: Deserializer<'de>>(deserializer: D) -> Result<IndexMap<String, String>, D::Error> {
    let map = Option::<IndexMap<String, serde_yaml::Value>>::deserialize(deserializer)?.unwrap_or_default();
    map.into_iter()
        .filter_map(|(k, v)| match yaml::to_value(&v) {
            Value::Null => None,
            v @ (Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Double(_) | Value::String(_)) => {
                Some(Ok((k, v.to_string())))
            }
            other => Some(Err(de::Error::custom(format!("expected a scalar value for tag {k:?}, got {other}")))),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags;

    #[test]
    fn parse_full_spec() {
        let spec = SchemaSpecification::load_str(
            r#"
examples:
- name: test output
  extra: ignored
  input:
    source: osm
    geometry: polygon
    tags:
      natural: water
      num: 1
  output:
  - layer: water
    geometry: polygon
    min_zoom: 1
    max_zoom: 14
    min_size: 10
    at_zoom: 12
    allow_extra_tags: false
    tags:
      natural: water
      other: null
      n: 1
"#,
        )
        .unwrap();
        assert_eq!(1, spec.examples.len());
        let example = &spec.examples[0];
        assert_eq!("test output", example.name);
        assert_eq!(Some("osm"), example.input.source.as_deref());
        assert_eq!("polygon", example.input.geometry);
        assert_eq!(Some(&"1".to_string()), example.input.tags.get("num"));
        let out = &example.output[0];
        assert_eq!(Some("water"), out.layer.as_deref());
        assert_eq!(Some(GeometryType::Polygon), out.geometry);
        assert_eq!(Some(1), out.min_zoom);
        assert_eq!(Some(14), out.max_zoom);
        assert_eq!(Some(10.0), out.min_size);
        assert_eq!(12, out.at_zoom());
        assert!(!out.allow_extra_tags());
        assert_eq!(tags! {"natural" => "water", "other" => Value::Null, "n" => 1}, out.tags);
    }

    #[test]
    fn defaults_and_single_output() {
        let spec = SchemaSpecification::load_str(
            r#"
examples:
- name: a
  input:
    geometry: point
  output:
    layer: l
- name: b
  input:
    geometry: line
  output:
"#,
        )
        .unwrap();
        assert_eq!(1, spec.examples[0].output.len());
        let out = &spec.examples[0].output[0];
        assert_eq!(MAX_MAXZOOM, out.at_zoom());
        assert!(out.allow_extra_tags());
        assert!(out.tags.is_empty());
        assert!(spec.examples[1].output.is_empty());
    }

    #[test]
    fn bad_geometry_type() {
        assert!(SchemaSpecification::load_str("examples: [{output: {geometry: blob}}]").is_err());
    }
}
