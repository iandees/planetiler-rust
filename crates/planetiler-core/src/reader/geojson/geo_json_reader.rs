//! Port of `reader/geojson/GeoJsonReader.java`: reads features from the vector geometries contained in a GeoJSON
//! file, which can contain [newline-delimited](https://stevage.github.io/ndgeojson/) features or features in a
//! feature collection.
//!
//! The source layer is the file name without its extension and feature IDs start at 0 (like Java).

use std::path::Path;

use anyhow::Result;
use parking_lot::Mutex;

use crate::reader::geojson::geo_json::GeoJson;
use crate::reader::simple_reader::{FeatureSink, ReaderFeature, SimpleReader};

/// Reads features from a geojson file.
pub struct GeoJsonReader {
    source_name: String,
    layer: String,
    file: GeoJson,
    count: Mutex<Option<u64>>,
}

impl GeoJsonReader {
    pub fn new(source_name: &str, input: &Path) -> GeoJsonReader {
        let file_name = input.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // remove file extension
        let layer = match file_name.rfind('.') {
            Some(idx) if idx + 1 < file_name.len() => file_name[..idx].to_string(),
            _ => file_name,
        };
        GeoJsonReader {
            source_name: source_name.to_string(),
            layer,
            file: GeoJson::from_path(input),
            count: Mutex::new(None),
        }
    }

    /// Name of the layer features are emitted to.
    pub fn layer(&self) -> &str {
        &self.layer
    }
}

impl SimpleReader for GeoJsonReader {
    fn source_name(&self) -> &str {
        &self.source_name
    }

    fn feature_count(&self) -> Result<u64> {
        let mut count = self.count.lock();
        Ok(*count.get_or_insert_with(|| self.file.count()))
    }

    fn read_features(&self, next: &mut FeatureSink<'_>) -> Result<()> {
        let layer = Some(self.layer.clone());
        for (id, feature) in self.file.iter()?.enumerate() {
            let feature = feature?;
            next(ReaderFeature {
                geometry: feature.geometry,
                tags: feature.tags,
                source_layer: layer.clone(),
                id: id as i64,
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use geo::Centroid;

    use super::*;
    use crate::value::Value;

    fn read_file(name: &str, layer: &str, expected_features: usize) -> (Vec<ReaderFeature>, Vec<String>) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources").join(name);
        let reader = GeoJsonReader::new("test", &path);
        assert_eq!(expected_features as u64, reader.feature_count().unwrap());
        let features = reader.read_all().unwrap();
        let mut names = vec![];
        for f in &features {
            assert!(matches!(f.get_tag("name"), Some(Value::String(_))));
            assert_eq!(Some(layer), f.source_layer.as_deref());
            names.push(f.get_tag("name").unwrap().to_string_lossy().to_string());
        }
        assert_eq!(expected_features, features.len());
        assert_eq!(
            (0..expected_features as i64).collect::<Vec<_>>(),
            features.iter().map(|f| f.id).collect::<Vec<_>>()
        );
        (features, names)
    }

    fn centroid(features: &[ReaderFeature]) -> geo_types::Point<f64> {
        geo_types::GeometryCollection(features.iter().map(|f| f.geometry.clone()).collect()).centroid().unwrap()
    }

    #[test]
    fn read_feature_collection() {
        let (features, names) = read_file("featurecollection.geojson", "featurecollection", 3);
        for name in ["line", "point", "polygon"] {
            assert!(names.contains(&name.to_string()));
        }
        let c = centroid(&features);
        assert!((c.x() - 100.5).abs() < 1e-5 && (c.y() - 0.5).abs() < 1e-5, "{c:?}");
    }

    #[test]
    fn read_newline_delimited() {
        let (features, names) = read_file("newlines.geojson", "newlines", 3);
        for name in ["line", "point", "polygon"] {
            assert!(names.contains(&name.to_string()));
        }
        let c = centroid(&features);
        assert!((c.x() - 100.5).abs() < 1e-5 && (c.y() - 0.5).abs() < 1e-5, "{c:?}");
    }

    #[test]
    fn read_feature() {
        let (features, _) = read_file("feature.geojson", "feature", 1);
        let c = centroid(&features);
        assert!((c.x() - 102.0).abs() < 1e-5 && (c.y() - 0.5).abs() < 1e-5, "{c:?}");
    }
}
