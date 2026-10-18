//! Port of `archive/TileArchiveMetadata.java`.
//!
//! Metadata associated with a tile archive. The default (de-)serialization corresponds to the
//! [mbtiles spec](https://github.com/mapbox/mbtiles-spec/blob/master/1.3/spec.md#metadata), so each value is a
//! string. See [`crate::archive::tile_archive_metadata_de_ser`] for the (de)serialization logic.

use std::collections::BTreeMap;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::archive::tile_archive_metadata_de_ser as de_ser;
use crate::archive::tile_compression::TileCompression;
use crate::archive::tile_format::TileFormat;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::geo::envelope::Envelope;
use crate::profile::Profile;
use crate::util::build_info::BuildInfo;

pub const NAME_KEY: &str = "name";
pub const DESCRIPTION_KEY: &str = "description";
pub const ATTRIBUTION_KEY: &str = "attribution";
pub const VERSION_KEY: &str = "version";
pub const TYPE_KEY: &str = "type";
pub const FORMAT_KEY: &str = "format";
pub const BOUNDS_KEY: &str = "bounds";
pub const CENTER_KEY: &str = "center";
pub const ZOOM_KEY: &str = "zoom";
pub const MINZOOM_KEY: &str = "minzoom";
pub const MAXZOOM_KEY: &str = "maxzoom";
pub const VECTOR_LAYERS_KEY: &str = "vector_layers";
pub const COMPRESSION_KEY: &str = "compression";
pub const JSON_KEY: &str = "json";

/// Equivalent of JTS `Coordinate` as used for the tileset center: `z` is `NaN` when absent (JTS `CoordinateXY`).
#[derive(Clone, Copy, Debug)]
pub struct Coordinate {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Coordinate {
    pub fn new(x: f64, y: f64, z: f64) -> Coordinate {
        Coordinate { x, y, z }
    }

    pub fn xy(x: f64, y: f64) -> Coordinate {
        Coordinate { x, y, z: f64::NAN }
    }
}

/// Compares x, y and z (treating two `NaN` z values as equal), like JTS `equals3D`.
impl PartialEq for Coordinate {
    fn eq(&self, other: &Self) -> bool {
        self.x == other.x && self.y == other.y && (self.z == other.z || (self.z.is_nan() && other.z.is_nan()))
    }
}

/// Port of `LayerAttrStats.FieldType` (re-exported as `util::layer_attr_stats::FieldType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FieldType {
    #[serde(rename = "Number")]
    Number,
    #[serde(rename = "Boolean")]
    Boolean,
    #[serde(rename = "String")]
    String,
}

impl FieldType {
    /// Per the MBTiles spec: attributes whose type varies between features SHOULD be listed as "String".
    pub fn merge(old_value: FieldType, new_value: FieldType) -> FieldType {
        if old_value != new_value { FieldType::String } else { new_value }
    }

    /// The field type of an attribute value, or `None` for null values.
    pub fn of(value: &crate::value::Value) -> Option<FieldType> {
        use crate::value::Value;
        match value {
            Value::Null => None,
            Value::Int(_) | Value::Float(_) | Value::Double(_) => Some(FieldType::Number),
            Value::Bool(_) => Some(FieldType::Boolean),
            _ => Some(FieldType::String),
        }
    }
}

/// Port of `LayerAttrStats.VectorLayer`: an entry of the mbtiles `vector_layers` metadata.
///
/// Intentionally separate from `util::layer_attr_stats::VectorLayer`, which keeps fields in insertion order while
/// stats are collected; this one keeps them sorted, as written to archive metadata. Convert with `From`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorLayer {
    pub id: String,
    #[serde(default)]
    pub fields: BTreeMap<String, FieldType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minzoom: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maxzoom: Option<i32>,
}

impl VectorLayer {
    pub fn new(id: impl Into<String>, fields: BTreeMap<String, FieldType>) -> VectorLayer {
        VectorLayer { id: id.into(), fields, description: None, minzoom: None, maxzoom: None }
    }

    pub fn with_zooms(
        id: impl Into<String>,
        fields: BTreeMap<String, FieldType>,
        minzoom: i32,
        maxzoom: i32,
    ) -> VectorLayer {
        VectorLayer { id: id.into(), fields, description: None, minzoom: Some(minzoom), maxzoom: Some(maxzoom) }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> VectorLayer {
        self.description = Some(description.into());
        self
    }

    pub fn with_minzoom(mut self, minzoom: i32) -> VectorLayer {
        self.minzoom = Some(minzoom);
        self
    }

    pub fn with_maxzoom(mut self, maxzoom: i32) -> VectorLayer {
        self.maxzoom = Some(maxzoom);
        self
    }
}

/// Data contained in the `json` row of the mbtiles metadata table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TileArchiveMetadataJson {
    #[serde(rename = "vector_layers", default)]
    pub vector_layers: Vec<VectorLayer>,
}

impl TileArchiveMetadataJson {
    pub fn new(vector_layers: Vec<VectorLayer>) -> TileArchiveMetadataJson {
        TileArchiveMetadataJson { vector_layers }
    }

    pub fn create(vector_layers: Option<Vec<VectorLayer>>) -> Option<TileArchiveMetadataJson> {
        vector_layers.map(TileArchiveMetadataJson::new)
    }

    pub fn with_layers(&self, vector_layers: Vec<VectorLayer>) -> TileArchiveMetadataJson {
        TileArchiveMetadataJson::new(vector_layers)
    }
}

/// Metadata associated with a tile archive.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct TileArchiveMetadata {
    pub name: Option<String>,
    pub description: Option<String>,
    pub attribution: Option<String>,
    pub version: Option<String>,
    pub type_: Option<String>,
    pub format: Option<TileFormat>,
    pub bounds: Option<Envelope>,
    pub center: Option<Coordinate>,
    pub minzoom: Option<i32>,
    pub maxzoom: Option<i32>,
    pub json: Option<TileArchiveMetadataJson>,
    pub others: BTreeMap<String, String>,
    pub tile_compression: Option<TileCompression>,
}

impl TileArchiveMetadata {
    /// Constructor with every field, in the same order as the Java record.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: Option<String>,
        description: Option<String>,
        attribution: Option<String>,
        version: Option<String>,
        type_: Option<String>,
        format: Option<TileFormat>,
        bounds: Option<Envelope>,
        center: Option<Coordinate>,
        minzoom: Option<i32>,
        maxzoom: Option<i32>,
        json: Option<TileArchiveMetadataJson>,
        others: BTreeMap<String, String>,
        tile_compression: Option<TileCompression>,
    ) -> TileArchiveMetadata {
        TileArchiveMetadata {
            name,
            description,
            attribution,
            version,
            type_,
            format,
            bounds,
            center,
            minzoom,
            maxzoom,
            json,
            others,
            tile_compression,
        }
    }

    /// Metadata for an archive generated by `profile` with `config` (Java
    /// `new TileArchiveMetadata(Profile, PlanetilerConfig)`).
    pub fn from_profile(profile: &dyn Profile, config: &PlanetilerConfig) -> TileArchiveMetadata {
        Self::from_profile_with_layers(profile, config, None)
    }

    /// Metadata for an archive generated by `profile` with `config` and `vector_layers` stats (Java
    /// `new TileArchiveMetadata(Profile, PlanetilerConfig, List<VectorLayer>)`).
    ///
    /// Name, description, attribution, version and type can be overridden with `archive_<key>` (or deprecated
    /// `mbtiles_<key>`) arguments.
    pub fn from_profile_with_layers(
        profile: &dyn Profile,
        config: &PlanetilerConfig,
        vector_layers: Option<Vec<VectorLayer>>,
    ) -> TileArchiveMetadata {
        let get_string = |key: &str, fallback: Option<String>| -> Option<String> {
            let arg = format!("archive_{key}|mbtiles_{key}");
            let description = format!("'{key}' attribute for tileset metadata");
            match fallback {
                Some(fallback) => Some(config.arguments.get_string(&arg, &description, &fallback)),
                None => config.arguments.get_string_opt(&arg, &description),
            }
        };
        let lat_lon = config.bounds.lat_lon();
        let center = lat_lon
            .centre()
            .map(|c| Coordinate::new(c.x, c.y, crate::geo::geo_utils::get_zoom_from_lon_lat_bounds(&lat_lon)));
        let mut others = BTreeMap::new();
        let build_info = BuildInfo::get();
        if let Some(version) = &build_info.version {
            others.insert("planetiler:version".to_string(), version.clone());
        }
        if let Some(githash) = &build_info.githash {
            others.insert("planetiler:githash".to_string(), githash.clone());
        }
        if let Some(build_time) = build_info.build_time_string() {
            others.insert("planetiler:buildtime".to_string(), build_time);
        }
        others.extend(profile.extra_archive_metadata());
        TileArchiveMetadata {
            name: get_string(NAME_KEY, Some(profile.name())),
            description: get_string(DESCRIPTION_KEY, profile.description()),
            attribution: get_string(ATTRIBUTION_KEY, profile.attribution()),
            version: get_string(VERSION_KEY, profile.version()),
            type_: get_string(TYPE_KEY, Some(if profile.is_overlay() { "overlay" } else { "baselayer" }.to_string())),
            format: Some(config.tile_format),
            bounds: Some(lat_lon),
            center,
            minzoom: Some(config.minzoom as i32),
            maxzoom: Some(config.maxzoom as i32),
            json: TileArchiveMetadataJson::create(vector_layers),
            others,
            tile_compression: Some(config.tile_compression),
        }
    }

    /// Approximate zoom level for the center, or `None` if missing.
    pub fn zoom(&self) -> Option<f64> {
        let z = self.center?.z;
        if z.is_nan() { None } else { Some(z) }
    }

    /// The vector layers stored in [`TileArchiveMetadata::json`].
    pub fn vector_layers(&self) -> Option<&Vec<VectorLayer>> {
        self.json.as_ref().map(|j| &j.vector_layers)
    }

    /// Sets an extra metadata entry in [`TileArchiveMetadata::others`].
    pub fn set_extra_metadata(&mut self, key: impl Into<String>, value: impl ToString) -> &mut Self {
        self.others.insert(key.into(), value.to_string());
        self
    }

    /// Returns a map with all key-value pairs from this metadata entry, including `others` hoisted to top-level keys.
    ///
    /// Unlike the mbtiles serialization, this omits `json`, includes `vector_layers` as a JSON string and `zoom`, and
    /// only includes the x/y of `center`.
    pub fn to_map(&self) -> IndexMap<String, String> {
        de_ser::to_internal_map(self)
    }

    /// Returns a copy of this instance with `json` set to `layer_stats`.
    pub fn with_layer_stats(&self, layer_stats: Vec<VectorLayer>) -> TileArchiveMetadata {
        self.with_json(Some(match &self.json {
            None => TileArchiveMetadataJson::new(layer_stats),
            Some(json) => json.with_layers(layer_stats),
        }))
    }

    /// Returns a copy of this instance with `json` replaced.
    pub fn with_json(&self, json: Option<TileArchiveMetadataJson>) -> TileArchiveMetadata {
        TileArchiveMetadata { json, ..self.clone() }
    }
}

impl Serialize for TileArchiveMetadata {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        de_ser::to_mbtiles_map(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TileArchiveMetadata {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = IndexMap::<String, serde_json::Value>::deserialize(deserializer)?;
        de_ser::from_json_map(&map, false).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::arguments::Arguments;
    use crate::feature_collector::FeatureCollector;
    use crate::geo::geo_utils;
    use crate::profile::NullProfile;
    use crate::reader::source_feature::SourceFeature;

    fn config(pairs: &[(&str, &str)]) -> PlanetilerConfig {
        PlanetilerConfig::from(&Arguments::of_pairs(pairs.iter().copied())).unwrap()
    }

    #[test]
    fn test_add_metadata_world_bounds() {
        let bounds = *geo_utils::WORLD_LAT_LON_BOUNDS;
        let arg = format!("{},{},{},{}", bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y);
        let metadata = TileArchiveMetadata::from_profile(&NullProfile, &config(&[("bounds", &arg)]));
        assert_eq!(Some(bounds), metadata.bounds);
        let center = metadata.center.unwrap();
        assert_eq!((0.0, 0.0), (center.x, center.y));
        assert_eq!(Some(0.0), metadata.zoom());
        assert_eq!(Some("Null"), metadata.name.as_deref());
        assert_eq!(Some("baselayer"), metadata.type_.as_deref());
    }

    #[test]
    fn test_add_metadata_small_bounds() {
        let bounds = Envelope::new(-73.6632, -69.7598, 41.1274, 43.0185);
        let metadata = TileArchiveMetadata::from_profile(
            &NullProfile,
            &config(&[("bounds", "-73.6632,41.1274,-69.7598,43.0185")]),
        );
        assert_eq!(Some(bounds), metadata.bounds);
        let center = metadata.center.unwrap();
        assert!((-71.7115 - center.x).abs() < 1e-5);
        assert!((42.07295 - center.y).abs() < 1e-5);
        assert_eq!(7.0, metadata.zoom().unwrap().ceil());
    }

    #[test]
    fn test_add_extra_metadata() {
        struct TestingProfile;
        impl Profile for TestingProfile {
            fn process_feature(&self, _source_feature: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {}
            fn name(&self) -> String {
                "My Name".to_string()
            }
            fn extra_archive_metadata(&self) -> BTreeMap<String, String> {
                BTreeMap::from([("FooVersion".to_string(), "2.0".to_string())])
            }
        }
        let metadata = TileArchiveMetadata::from_profile(
            &TestingProfile,
            &config(&[("bounds", "-73.6632,41.1274,-69.7598,43.0185")]),
        );
        let map = metadata.to_map();
        assert_eq!("My Name", map["name"]);
        assert_eq!("2.0", map["FooVersion"]);
        assert_eq!(Some(env!("CARGO_PKG_VERSION")), map.get("planetiler:version").map(String::as_str));
    }

    #[test]
    fn test_metadata_overrides_from_arguments() {
        let metadata = TileArchiveMetadata::from_profile(
            &NullProfile,
            &config(&[("archive_name", "override"), ("mbtiles_description", "desc"), ("tile-format", "mlt")]),
        );
        assert_eq!(Some("override"), metadata.name.as_deref());
        assert_eq!(Some("desc"), metadata.description.as_deref());
        assert_eq!(None, metadata.attribution);
        assert_eq!(Some(TileFormat::Mlt), metadata.format);
        assert_eq!("application/vnd.maplibre-tile", metadata.to_map()["format"]);
    }
}
