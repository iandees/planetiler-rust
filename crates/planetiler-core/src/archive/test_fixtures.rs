//! Shared test fixtures for archive tests (port of the metadata constants in Java `TestUtils`).

use std::collections::BTreeMap;

use crate::archive::readable_tile_archive::ReadableTileArchive;
use crate::archive::tile::Tile;
use crate::archive::tile_archive_metadata::*;
use crate::archive::tile_compression::TileCompression;
use crate::archive::tile_format::TileFormat;
use crate::geo::envelope::Envelope;

pub fn max_metadata_deserialized() -> TileArchiveMetadata {
    TileArchiveMetadata::new(
        Some("name".into()),
        Some("description".into()),
        Some("attribution".into()),
        Some("version".into()),
        Some("type".into()),
        Some(TileFormat::Mvt),
        Some(Envelope::new(0.0, 1.0, 2.0, 3.0)),
        Some(Coordinate::new(1.3, 3.7, 1.0)),
        Some(2),
        Some(3),
        Some(TileArchiveMetadataJson::new(vec![
            VectorLayer {
                id: "vl0".into(),
                fields: BTreeMap::from([
                    ("1".into(), FieldType::Boolean),
                    ("2".into(), FieldType::Number),
                    ("3".into(), FieldType::String),
                ]),
                description: Some("description".into()),
                minzoom: Some(1),
                maxzoom: Some(2),
            },
            VectorLayer::new("vl1", BTreeMap::new()),
        ])),
        BTreeMap::from([("a".into(), "b".into()), ("c".into(), "d".into())]),
        Some(TileCompression::Gzip),
    )
}

pub const MAX_METADATA_SERIALIZED: &str = concat!(
    r#"{"name":"name","description":"description","attribution":"attribution","version":"version","type":"type","#,
    r#""format":"pbf","minzoom":"2","maxzoom":"3","compression":"gzip","bounds":"0,2,1,3","center":"1.3,3.7,1","#,
    r#""json":"{\"vector_layers\":[{\"id\":\"vl0\",\"fields\":{\"1\":\"Boolean\",\"2\":\"Number\",\"3\":\"String\"},"#,
    r#"\"description\":\"description\",\"minzoom\":1,\"maxzoom\":2},{\"id\":\"vl1\",\"fields\":{}}]}","a":"b","c":"d"}"#
);

pub fn min_metadata_deserialized() -> TileArchiveMetadata {
    TileArchiveMetadata::default()
}

pub const MIN_METADATA_SERIALIZED: &str = "{}";

/// Compares two JSON strings semantically, also comparing the nested escaped `json` value semantically.
pub fn assert_same_json(expected: &str, actual: &str) {
    fn normalize(s: &str) -> serde_json::Value {
        let mut v: serde_json::Value = serde_json::from_str(s).unwrap_or_else(|e| panic!("invalid json {s}: {e}"));
        if let Some(obj) = v.as_object_mut()
            && let Some(serde_json::Value::String(json)) = obj.get("json").cloned()
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json)
        {
            obj.insert("json".into(), parsed);
        }
        v
    }
    assert_eq!(normalize(expected), normalize(actual));
}

pub fn get_tiles(archive: &dyn ReadableTileArchive) -> std::collections::BTreeSet<Tile> {
    archive.get_all_tiles().unwrap().map(|t| t.unwrap()).collect()
}
