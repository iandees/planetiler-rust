//! Port of `archive/TileArchiveMetadataDeSer.java`: everything related to (de-)serialization of
//! [`TileArchiveMetadata`].
//!
//! Java uses two Jackson mappers:
//! - the "mbtiles" mapper ([`to_mbtiles_map`] / [`from_string_map`] / [`from_json_str`]) which serializes every value
//!   as a string like the mbtiles metadata table, with `json` holding the escaped `{"vector_layers": [...]}`;
//! - the "internal map" mapper ([`to_internal_map`]) used by [`TileArchiveMetadata::to_map`].
//!
//! `strict` deserialization corresponds to Java's `StrictDeserializationMixin`: malformed `bounds`/`center` values are
//! errors instead of being silently dropped.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use indexmap::IndexMap;
use serde_json::Value;

use crate::archive::tile_archive_metadata::*;
use crate::archive::tile_compression::TileCompression;
use crate::archive::tile_format::TileFormat;
use crate::geo::envelope::Envelope;

/// Port of `Format.joinCoordinates`: rounds to 5 decimals and joins with a comma.
pub fn join_coordinates(items: &[f64]) -> String {
    items.iter().map(|d| format_lat_lon(*d)).collect::<Vec<_>>().join(",")
}

fn format_lat_lon(d: f64) -> String {
    if d.is_nan() {
        return "NaN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "∞".into() } else { "-∞".into() };
    }
    let s = format!("{d:.5}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { s.as_str() };
    s.to_string()
}

/// Formats a double like Java's `Double.toString` for typical values (always includes a decimal point).
pub fn java_double_to_string(d: f64) -> String {
    if d.is_nan() {
        return "NaN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    let abs = d.abs();
    if abs != 0.0 && !(1e-3..1e7).contains(&abs) {
        // scientific notation like Java "1.0E7"
        let s = format!("{d:e}");
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let mantissa = if mantissa.contains('.') { mantissa.to_string() } else { format!("{mantissa}.0") };
        return format!("{mantissa}E{exp}");
    }
    let s = format!("{d}");
    if s.contains('.') { s } else { format!("{s}.0") }
}

fn format_center(c: &Coordinate, include_z: bool) -> String {
    if !include_z || c.z.is_nan() { join_coordinates(&[c.x, c.y]) } else { join_coordinates(&[c.x, c.y, c.z.ceil()]) }
}

fn format_bounds(b: &Envelope) -> String {
    join_coordinates(&[b.min_x, b.min_y, b.max_x, b.max_y])
}

/// Serialized `json` value: `{"vector_layers":[...]}`.
pub fn metadata_json_to_string(json: &TileArchiveMetadataJson) -> String {
    serde_json::to_string(json).expect("vector layers are always serializable")
}

fn put(map: &mut IndexMap<String, String>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        map.insert(key.to_string(), value);
    }
}

/// Serializes `metadata` to a map of strings like the mbtiles metadata table (Java `mbtilesMapper().convertValue`).
pub fn to_mbtiles_map(metadata: &TileArchiveMetadata) -> IndexMap<String, String> {
    let mut map = IndexMap::new();
    put(&mut map, NAME_KEY, metadata.name.clone());
    put(&mut map, DESCRIPTION_KEY, metadata.description.clone());
    put(&mut map, ATTRIBUTION_KEY, metadata.attribution.clone());
    put(&mut map, VERSION_KEY, metadata.version.clone());
    put(&mut map, TYPE_KEY, metadata.type_.clone());
    put(&mut map, FORMAT_KEY, metadata.format.map(format_to_string));
    put(&mut map, BOUNDS_KEY, metadata.bounds.as_ref().map(format_bounds));
    put(&mut map, CENTER_KEY, metadata.center.as_ref().map(|c| format_center(c, true)));
    put(&mut map, MINZOOM_KEY, metadata.minzoom.map(|z| z.to_string()));
    put(&mut map, MAXZOOM_KEY, metadata.maxzoom.map(|z| z.to_string()));
    put(&mut map, JSON_KEY, metadata.json.as_ref().map(metadata_json_to_string));
    put(&mut map, COMPRESSION_KEY, metadata.tile_compression.map(|c| c.id().to_string()));
    for (k, v) in &metadata.others {
        map.insert(k.clone(), v.clone());
    }
    map
}

/// Serializes `metadata` with the "internal map" rules (Java `TileArchiveMetadata.toMap()`).
pub fn to_internal_map(metadata: &TileArchiveMetadata) -> IndexMap<String, String> {
    let mut map = IndexMap::new();
    put(&mut map, NAME_KEY, metadata.name.clone());
    put(&mut map, DESCRIPTION_KEY, metadata.description.clone());
    put(&mut map, ATTRIBUTION_KEY, metadata.attribution.clone());
    put(&mut map, VERSION_KEY, metadata.version.clone());
    put(&mut map, TYPE_KEY, metadata.type_.clone());
    put(&mut map, FORMAT_KEY, metadata.format.map(format_to_string));
    put(&mut map, BOUNDS_KEY, metadata.bounds.as_ref().map(format_bounds));
    put(&mut map, CENTER_KEY, metadata.center.as_ref().map(|c| format_center(c, false)));
    put(&mut map, MINZOOM_KEY, metadata.minzoom.map(|z| z.to_string()));
    put(&mut map, MAXZOOM_KEY, metadata.maxzoom.map(|z| z.to_string()));
    put(&mut map, COMPRESSION_KEY, metadata.tile_compression.map(|c| c.id().to_string()));
    put(&mut map, ZOOM_KEY, metadata.zoom().map(java_double_to_string));
    put(
        &mut map,
        VECTOR_LAYERS_KEY,
        metadata.vector_layers().map(|l| serde_json::to_string(l).expect("vector layers are always serializable")),
    );
    for (k, v) in &metadata.others {
        map.insert(k.clone(), v.clone());
    }
    map
}

/// Serializes `metadata` to a JSON object string with the mbtiles rules (all values are strings).
pub fn to_json_string(metadata: &TileArchiveMetadata) -> String {
    serde_json::to_string(&to_mbtiles_map(metadata)).expect("string map is always serializable")
}

fn format_to_string(format: TileFormat) -> String {
    format.mbtiles().unwrap_or(format.id()).to_string()
}

fn double_list_from_comma_list(
    comma_list: &str,
    min_items: usize,
    max_items: usize,
    strict: bool,
) -> anyhow::Result<Option<Vec<f64>>> {
    let splits: Vec<&str> = comma_list.split(',').collect();
    if splits.len() < min_items {
        if strict {
            bail!("expected at least {min_items} doubles");
        }
        return Ok(None);
    } else if splits.len() > max_items && strict {
        bail!("expected at most {max_items} doubles");
    }
    let result = splits
        .iter()
        .take(max_items)
        .map(|s| s.trim().parse::<f64>().with_context(|| format!("invalid double '{s}'")))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Some(result))
}

fn parse_envelope(value: &str, strict: bool) -> anyhow::Result<Option<Envelope>> {
    Ok(double_list_from_comma_list(value, 4, 4, strict)?.map(|ds| Envelope::new(ds[0], ds[2], ds[1], ds[3])))
}

fn parse_center(value: &str, strict: bool) -> anyhow::Result<Option<Coordinate>> {
    Ok(double_list_from_comma_list(value, 2, 3, strict)?
        .map(|ds| if ds.len() == 2 { Coordinate::xy(ds[0], ds[1]) } else { Coordinate::new(ds[0], ds[1], ds[2]) }))
}

/// A raw metadata value: `None` = explicit null.
enum Raw<'a> {
    Str(String),
    Null,
    Json(&'a Value),
}

fn raw_of(value: &Value) -> Raw<'_> {
    match value {
        Value::Null => Raw::Null,
        Value::String(s) => Raw::Str(s.clone()),
        Value::Number(n) => Raw::Str(n.to_string()),
        Value::Bool(b) => Raw::Str(b.to_string()),
        other => Raw::Json(other),
    }
}

/// Deserializes metadata from a map of strings (e.g. the mbtiles metadata table).
pub fn from_string_map(map: &BTreeMap<String, String>, strict: bool) -> anyhow::Result<TileArchiveMetadata> {
    let json_map: IndexMap<String, Value> = map.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
    from_json_map(&json_map, strict)
}

/// Deserializes metadata from a JSON object string (mbtiles rules).
pub fn from_json_str(json: &str, strict: bool) -> anyhow::Result<TileArchiveMetadata> {
    let map: IndexMap<String, Value> = serde_json::from_str(json).context("invalid metadata JSON")?;
    from_json_map(&map, strict)
}

/// Deserializes metadata from parsed JSON object entries (mbtiles rules).
pub fn from_json_map(map: &IndexMap<String, Value>, strict: bool) -> anyhow::Result<TileArchiveMetadata> {
    let mut result = TileArchiveMetadata::default();
    // missing values of these get the Jackson "null value" of their deserializer
    let mut format = Some(TileFormat::Mvt);
    let mut compression = Some(TileCompression::Gzip);
    for (key, value) in map {
        let raw = raw_of(value);
        let string = || -> anyhow::Result<Option<String>> {
            match &raw {
                Raw::Str(s) => Ok(Some(s.clone())),
                Raw::Null => Ok(None),
                Raw::Json(v) => bail!("expected string for '{key}' but got {v}"),
            }
        };
        match key.as_str() {
            NAME_KEY => result.name = string()?,
            DESCRIPTION_KEY => result.description = string()?,
            ATTRIBUTION_KEY => result.attribution = string()?,
            VERSION_KEY => result.version = string()?,
            TYPE_KEY => result.type_ = string()?,
            FORMAT_KEY => {
                format = Some(string()?.and_then(|s| TileFormat::find_by_id(&s)).unwrap_or(TileFormat::Mvt));
            }
            COMPRESSION_KEY => {
                compression = Some(match string()? {
                    None => TileCompression::Gzip,
                    Some(s) => TileCompression::find_by_id(&s).unwrap_or(TileCompression::Unknown),
                });
            }
            BOUNDS_KEY => {
                result.bounds = match string()? {
                    Some(s) => parse_envelope(&s, strict).with_context(|| format!("invalid bounds '{s}'"))?,
                    None => None,
                }
            }
            CENTER_KEY => {
                result.center = match string()? {
                    Some(s) => parse_center(&s, strict).with_context(|| format!("invalid center '{s}'"))?,
                    None => None,
                }
            }
            MINZOOM_KEY | MAXZOOM_KEY => {
                let parsed = match string()? {
                    Some(s) => Some(s.trim().parse::<i32>().with_context(|| format!("invalid {key} '{s}'"))?),
                    None => None,
                };
                if key == MINZOOM_KEY {
                    result.minzoom = parsed;
                } else {
                    result.maxzoom = parsed;
                }
            }
            JSON_KEY => {
                result.json = match &raw {
                    Raw::Null => None,
                    Raw::Str(s) => Some(serde_json::from_str(s).context("invalid metadata json")?),
                    Raw::Json(v) => Some(serde_json::from_value((*v).clone()).context("invalid metadata json")?),
                }
            }
            // ignored properties in the mbtiles mapper
            ZOOM_KEY | VECTOR_LAYERS_KEY => {}
            _ => {
                if let Some(s) = string()? {
                    result.others.insert(key.clone(), s);
                }
            }
        }
    }
    result.format = format;
    result.tile_compression = compression;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    // produced by tilelive-copy or rather mbutil - just reduced $.json.vector_layers[]
    const SERIALIZED: &str = r#"
      {
          "attribution": "<a href=\"https://www.openmaptiles.org/\" target=\"_blank\">&copy; OpenMapTiles</a> <a href=\"https://www.openstreetmap.org/copyright\" target=\"_blank\">&copy; OpenStreetMap contributors</a>",
          "description": "A tileset showcasing all layers in OpenMapTiles. https://openmaptiles.org",
          "format": "pbf",
          "planetiler:version": "0.7-SNAPSHOT",
          "bounds": "7.40921,43.72335,7.44864,43.75169",
          "name": "OpenMapTiles",
          "planetiler:githash": "09c22c18268d9cc1371ed0b0af192e698abf54c7",
          "json": "{\"vector_layers\":[{\"fields\":{\"class\":\"String\",\"ref\":\"String\"},\"id\":\"aeroway\",\"maxzoom\":14,\"minzoom\":12}]}",
          "version": "3.14.0",
          "compression": "gzip",
          "minzoom": "0",
          "planetiler:osm:osmosisreplicationurl": "http://download.geofabrik.de/europe/monaco-updates",
          "maxzoom": "14",
          "planetiler:osm:osmosisreplicationseq": "3911",
          "type": "baselayer",
          "planetiler:buildtime": "2023-12-20T21:33:49.594Z",
          "planetiler:osm:osmosisreplicationtime": "2023-12-18T21:21:01Z",
          "center": "7.42892,43.73752,14"
      }
    "#;

    fn deserialized() -> TileArchiveMetadata {
        TileArchiveMetadata::new(
            Some("OpenMapTiles".into()),
            Some("A tileset showcasing all layers in OpenMapTiles. https://openmaptiles.org".into()),
            Some("<a href=\"https://www.openmaptiles.org/\" target=\"_blank\">&copy; OpenMapTiles</a> <a href=\"https://www.openstreetmap.org/copyright\" target=\"_blank\">&copy; OpenStreetMap contributors</a>".into()),
            Some("3.14.0".into()),
            Some("baselayer".into()),
            Some(TileFormat::Mvt),
            Some(Envelope::new(7.40921, 7.44864, 43.72335, 43.75169)),
            Some(Coordinate::new(7.42892, 43.73752, 14.0)),
            Some(0),
            Some(14),
            Some(TileArchiveMetadataJson::new(vec![VectorLayer::with_zooms(
                "aeroway",
                BTreeMap::from([("ref".into(), FieldType::String), ("class".into(), FieldType::String)]),
                12,
                14,
            )])),
            BTreeMap::from([
                ("planetiler:version".into(), "0.7-SNAPSHOT".into()),
                ("planetiler:githash".into(), "09c22c18268d9cc1371ed0b0af192e698abf54c7".into()),
                ("planetiler:osm:osmosisreplicationurl".into(), "http://download.geofabrik.de/europe/monaco-updates".into()),
                ("planetiler:osm:osmosisreplicationseq".into(), "3911".into()),
                ("planetiler:buildtime".into(), "2023-12-20T21:33:49.594Z".into()),
                ("planetiler:osm:osmosisreplicationtime".into(), "2023-12-18T21:21:01Z".into()),
            ]),
            Some(TileCompression::Gzip),
        )
    }

    #[test]
    fn test_deserialization() {
        assert_eq!(deserialized(), from_json_str(SERIALIZED, false).unwrap());
        // also via serde
        assert_eq!(deserialized(), serde_json::from_str::<TileArchiveMetadata>(SERIALIZED).unwrap());
    }

    #[test]
    fn test_serialization() {
        let mut o0: serde_json::Map<String, Value> = serde_json::from_str(SERIALIZED).unwrap();
        let mut o1: serde_json::Map<String, Value> =
            serde_json::from_str(&serde_json::to_string(&deserialized()).unwrap()).unwrap();
        let j0: Value = serde_json::from_str(o0.remove("json").unwrap().as_str().unwrap()).unwrap();
        let j1: Value = serde_json::from_str(o1.remove("json").unwrap().as_str().unwrap()).unwrap();
        assert_eq!(j0, j1);
        assert_eq!(o0, o1);
    }

    fn center(json: &str, strict: bool) -> anyhow::Result<Option<Coordinate>> {
        Ok(from_json_str(json, strict)?.center)
    }

    fn bounds(json: &str, strict: bool) -> anyhow::Result<Option<Envelope>> {
        Ok(from_json_str(json, strict)?.bounds)
    }

    #[test]
    fn test_center_deserialization() {
        assert_eq!(None, center(r#"{"center": null}"#, false).unwrap());
        assert_eq!(Some(Coordinate::xy(0.0, 1.1)), center(r#"{"center": "0.0,1.1"}"#, false).unwrap());
        assert_eq!(Some(Coordinate::new(0.0, 1.1, 14.0)), center(r#"{"center": "0.0,1.1,14"}"#, false).unwrap());
        let s3 = r#"{"center": "0.0,1.1,14,42"}"#;
        assert_eq!(Some(Coordinate::new(0.0, 1.1, 14.0)), center(s3, false).unwrap());
        assert!(center(s3, true).is_err());
        let s4 = r#"{"center": "0.0"}"#;
        assert_eq!(None, center(s4, false).unwrap());
        assert!(center(s4, true).is_err());
    }

    #[test]
    fn test_bounds_deserialization() {
        assert_eq!(None, bounds(r#"{"bounds": null}"#, false).unwrap());
        let expected = Some(Envelope::new(1.0, 3.0, 2.0, 4.0));
        assert_eq!(expected, bounds(r#"{"bounds": "1.0,2.0,3.0,4.0"}"#, false).unwrap());
        let s2 = r#"{"bounds": "1.0,2.0,3.0,4.0,5.0"}"#;
        assert_eq!(expected, bounds(s2, false).unwrap());
        assert!(bounds(s2, true).is_err());
        let s3 = r#"{"bounds": "1.0"}"#;
        assert_eq!(None, bounds(s3, false).unwrap());
        assert!(bounds(s3, true).is_err());
    }

    #[test]
    fn test_mlt_metadata() {
        let metadata = TileArchiveMetadata { format: Some(TileFormat::Mlt), ..Default::default() };
        let map = metadata.to_map();
        assert_eq!("application/vnd.maplibre-tile", map["format"]);
        let back: BTreeMap<String, String> = map.into_iter().collect();
        assert_eq!(Some(TileFormat::Mlt), from_string_map(&back, true).unwrap().format);
        assert_eq!(
            Some(TileFormat::Mlt),
            from_json_str(r#"{"format": "application/vnd.maplibre-tile"}"#, true).unwrap().format
        );
    }

    #[test]
    fn test_parse_unknown_format() {
        assert_eq!(Some(TileFormat::Mvt), from_json_str(r#"{"format": "garbage"}"#, true).unwrap().format);
    }

    #[test]
    fn test_to_map() {
        let bounds = Envelope::new(-73.6632, -69.7598, 41.1274, 43.0185);
        let centre = bounds.centre().unwrap();
        let zoom = crate::geo::geo_utils::get_zoom_from_lon_lat_bounds(&bounds);
        let metadata = TileArchiveMetadata {
            name: Some("Null".into()),
            type_: Some("baselayer".into()),
            format: Some(TileFormat::Mvt),
            bounds: Some(bounds),
            center: Some(Coordinate::new(centre.x, centre.y, zoom)),
            minzoom: Some(0),
            maxzoom: Some(14),
            tile_compression: Some(TileCompression::Gzip),
            ..Default::default()
        }
        .with_layer_stats(vec![VectorLayer::with_zooms(
            "aeroway",
            BTreeMap::from([("ref".into(), FieldType::String), ("class".into(), FieldType::String)]),
            12,
            14,
        )]);
        let mut map: BTreeMap<String, String> = metadata.to_map().into_iter().collect();
        let layers: Value = serde_json::from_str(&map.remove("vector_layers").unwrap()).unwrap();
        let expected_layers: Value = serde_json::from_str(
            r#"[{"id":"aeroway","fields":{"ref":"String","class":"String"},"minzoom":12,"maxzoom":14}]"#,
        )
        .unwrap();
        assert_eq!(expected_layers, layers);
        let expected: BTreeMap<String, String> = [
            ("name", "Null"),
            ("type", "baselayer"),
            ("format", "pbf"),
            ("zoom", "6.5271217861412305"),
            ("minzoom", "0"),
            ("maxzoom", "14"),
            ("bounds", "-73.6632,41.1274,-69.7598,43.0185"),
            ("center", "-71.7115,42.07295"),
            ("compression", "gzip"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(expected, map);
    }

    #[test]
    fn test_java_double() {
        assert_eq!("0.0", java_double_to_string(0.0));
        assert_eq!("6.5", java_double_to_string(6.5));
        assert_eq!("14.0", java_double_to_string(14.0));
    }
}
