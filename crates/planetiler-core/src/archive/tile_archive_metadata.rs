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
use crate::geo::envelope::Envelope;

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

/// Port of `LayerAttrStats.FieldType`.
///
/// TODO(port): `util/layer_attr_stats.rs` should re-use (or re-export) this type.
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
}

/// Port of `LayerAttrStats.VectorLayer`: an entry of the mbtiles `vector_layers` metadata.
///
/// TODO(port): `util/layer_attr_stats.rs` should re-use (or re-export) this type.
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

    // TODO(port): `from_profile(profile, config)` / `from_profile_with_layers(profile, config, vector_layers)`
    // equivalent of `new TileArchiveMetadata(Profile, PlanetilerConfig[, List<VectorLayer>])` once `Profile`,
    // `Arguments.getString` and `BuildInfo` are ported. Logic:
    //   name/description/attribution/version: config arg "archive_<key>|mbtiles_<key>" falling back to profile value
    //   type: arg or (profile.is_overlay() ? "overlay" : "baselayer")
    //   format = config.tile_format, bounds = config.bounds.lat_lon(),
    //   center = Coordinate(bounds.centre.x, bounds.centre.y, geo_utils::get_zoom_from_lon_lat_bounds(bounds)),
    //   minzoom/maxzoom from config, json = vector_layers.map(TileArchiveMetadataJson::new),
    //   others = build info ("planetiler:version", "planetiler:githash", "planetiler:buildtime") merged with
    //   profile.extra_archive_metadata(), tile_compression = config.tile_compression.

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
