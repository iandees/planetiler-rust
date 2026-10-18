//! Port of `reader/geojson/GeoJson.java`: a streaming geojson parser that can handle arbitrarily large regular or
//! [newline-delimited](https://stevage.github.io/ndgeojson/) geojson files without loading the whole thing in memory.
//!
//! This emits every top-level feature or feature contained within a `FeatureCollection`. Invalid json syntax will
//! return an error, but unexpected geojson objects will just log a warning and emit empty geometries.
//!
//! See the [GeoJSON specification (RFC 7946)](https://datatracker.ietf.org/doc/html/rfc7946).

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::reader::geojson::geo_json_feature::GeoJsonFeature;
use crate::reader::geojson::geo_json_feature_counter;
use crate::reader::geojson::geo_json_feature_iterator::GeoJsonFeatureIterator;

type InputSupplier = Arc<dyn Fn() -> std::io::Result<Box<dyn Read + Send>> + Send + Sync>;

/// A geojson document that can be iterated over (multiple times) and counted.
#[derive(Clone)]
pub struct GeoJson {
    input: InputSupplier,
    name: Option<String>,
}

impl GeoJson {
    /// Reads geojson from a file.
    pub fn from_path(path: &Path) -> GeoJson {
        let p: PathBuf = path.to_path_buf();
        GeoJson {
            input: Arc::new(move || {
                Ok(Box::new(BufReader::with_capacity(1 << 16, std::fs::File::open(&p)?)) as Box<dyn Read + Send>)
            }),
            name: Some(path.display().to_string()),
        }
    }

    /// Reads geojson from bytes in memory.
    pub fn from_bytes(json: impl Into<Arc<[u8]>>) -> GeoJson {
        let bytes: Arc<[u8]> = json.into();
        GeoJson {
            input: Arc::new(move || Ok(Box::new(std::io::Cursor::new(bytes.clone())) as Box<dyn Read + Send>)),
            name: None,
        }
    }

    /// Reads geojson from a string.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(json: &str) -> GeoJson {
        Self::from_bytes(json.as_bytes())
    }

    /// Reads geojson from a stream that is opened each time this document is iterated over or counted.
    pub fn from_supplier(
        supplier: impl Fn() -> std::io::Result<Box<dyn Read + Send>> + Send + Sync + 'static,
    ) -> GeoJson {
        GeoJson { input: Arc::new(supplier), name: None }
    }

    /// Returns a streaming iterator over the features in this document.
    pub fn iter(&self) -> Result<GeoJsonFeatureIterator<Box<dyn Read + Send>>> {
        let input = (self.input)().context("Unable to read geojson file")?;
        Ok(GeoJsonFeatureIterator::new(input, self.name.clone()))
    }

    /// Reads all features into a vector.
    pub fn features(&self) -> Result<Vec<GeoJsonFeature>> {
        self.iter()?.collect()
    }

    /// Returns the number of geojson features in this document (0 if it can't be read).
    pub fn count(&self) -> u64 {
        info!("Counting geojson features in {}", self.name.as_deref().unwrap_or("input"));
        match (self.input)().map_err(anyhow::Error::from).and_then(geo_json_feature_counter::count) {
            Ok(count) => count,
            Err(e) => {
                warn!("Unable to feature count: {e:#}");
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use geo_types::Geometry;

    use super::*;
    use crate::geo::geo_utils;
    use crate::geo::test_utils::*;
    use crate::tags;
    use crate::value::{Tags, Value};

    fn feature(geometry: impl Into<Geometry<f64>>, tags: Tags) -> GeoJsonFeature {
        GeoJsonFeature { geometry: geometry.into(), tags }
    }

    fn test_parse(json: &str, expected: Vec<GeoJsonFeature>) {
        let n = expected.len() as u64;
        test_parse_count(json, expected, n);
    }

    fn test_parse_count(json: &str, expected: Vec<GeoJsonFeature>, num_expected: u64) {
        let wrapper = GeoJson::from_str(json);
        assert_eq!(expected, wrapper.features().unwrap(), "{json}");
        assert_eq!(num_expected, wrapper.count(), "{json}");
    }

    #[test]
    fn point() {
        test_parse(
            r#"{"type": "Feature", "geometry": {"type": "Point", "coordinates": [1, 2]}}"#,
            vec![feature(new_point(1.0, 2.0), Tags::default())],
        );
    }

    #[test]
    fn line_string() {
        test_parse(
            r#"{"type": "Feature", "geometry": {"type": "LineString", "coordinates": [[1, 2], [3, 4]]}}"#,
            vec![feature(new_line_string(&[1.0, 2.0, 3.0, 4.0]), Tags::default())],
        );
    }

    #[test]
    fn polygon() {
        test_parse(
            r#"{"type": "Feature", "geometry": {"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 1], [0, 0]]]}}"#,
            vec![feature(new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]), Tags::default())],
        );
    }

    fn square_with_hole() -> geo_types::Polygon<f64> {
        new_polygon_with_holes(
            new_coordinate_list(&[0.0, 0.0, 3.0, 0.0, 3.0, 3.0, 0.0, 3.0, 0.0, 0.0]),
            vec![new_coordinate_list(&[1.0, 1.0, 2.0, 1.0, 2.0, 2.0, 1.0, 2.0, 1.0, 1.0])],
        )
    }

    #[test]
    fn polygon_with_hole() {
        test_parse(
            r#"{"type": "Feature", "geometry": {"type": "Polygon", "coordinates": [[[0, 0], [3, 0], [3, 3], [0, 3], [0, 0]], [[1, 1], [2, 1], [2, 2], [1, 2], [1, 1]]]}}"#,
            vec![feature(square_with_hole(), Tags::default())],
        );
    }

    #[test]
    fn multi_point() {
        test_parse(
            r#"{"type": "Feature", "geometry": {"type": "MultiPoint", "coordinates": [[1, 2], [3, 4]]}}"#,
            vec![feature(new_multi_point(vec![new_point(1.0, 2.0), new_point(3.0, 4.0)]), Tags::default())],
        );
    }

    #[test]
    fn multi_line_string() {
        test_parse(
            r#"{"type": "Feature", "geometry": {"type": "MultiLineString", "coordinates": [[[1, 2], [3, 4]], [[5, 6], [7, 8]]]}}"#,
            vec![feature(
                new_multi_line_string(vec![
                    new_line_string(&[1.0, 2.0, 3.0, 4.0]),
                    new_line_string(&[5.0, 6.0, 7.0, 8.0]),
                ]),
                Tags::default(),
            )],
        );
    }

    #[test]
    fn multi_polygon() {
        test_parse(
            r#"{"type": "Feature", "geometry": {"type": "MultiPolygon", "coordinates": [[[[0, 0], [1, 0], [1, 1], [0, 1], [0, 0]]], [[[0, 0], [2, 0], [2, 2], [0, 2], [0, 0]]]]}}"#,
            vec![feature(
                new_multi_polygon(vec![
                    new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]),
                    new_polygon(&[0.0, 0.0, 2.0, 0.0, 2.0, 2.0, 0.0, 2.0, 0.0, 0.0]),
                ]),
                Tags::default(),
            )],
        );
    }

    #[test]
    fn multi_polygon_with_holes() {
        test_parse(
            r#"{"type": "Feature", "geometry": {"type": "MultiPolygon", "coordinates": [[[[0, 0], [3, 0], [3, 3], [0, 3], [0, 0]], [[1, 1], [2, 1], [2, 2], [1, 2], [1, 1]]], [[[0, 0], [3, 0], [3, 3], [0, 3], [0, 0]], [[1, 1], [2, 1], [2, 2], [1, 2], [1, 1]]]]}}"#,
            vec![feature(new_multi_polygon(vec![square_with_hole(), square_with_hole()]), Tags::default())],
        );
    }

    #[test]
    fn empty_point() {
        test_parse(
            r#"{"type": "Feature", "geometry": {"type": "Point", "coordinates": []}}"#,
            vec![feature(geo_utils::empty_point(), Tags::default())],
        );
    }

    #[test]
    fn empty_line_string() {
        for coords in ["[]", "[[]]"] {
            test_parse(
                &format!(r#"{{"type": "Feature", "geometry": {{"type": "LineString", "coordinates": {coords}}}}}"#),
                vec![feature(geo_utils::empty_line(), Tags::default())],
            );
        }
    }

    #[test]
    fn empty_polygon() {
        for coords in ["[]", "[[]]", "[[[]]]"] {
            test_parse(
                &format!(r#"{{"type": "Feature", "geometry": {{"type": "Polygon", "coordinates": {coords}}}}}"#),
                vec![feature(geo_utils::empty_polygon(), Tags::default())],
            );
        }
    }

    #[test]
    fn point_with_bad_properties() {
        for properties in ["\"\"", "null", "true", "1", "[]"] {
            test_parse(
                &format!(
                    r#"{{
                      "type": "Feature",
                      "geometry": {{"type": "Point", "coordinates": [1, 2]}},
                      "properties": {properties}
                    }}"#
                ),
                vec![feature(new_point(1.0, 2.0), Tags::default())],
            );
        }
    }

    #[test]
    fn point_with_properties() {
        test_parse(
            r#"{
              "type": "Feature",
              "geometry": {"type": "Point", "coordinates": [1, 2]},
              "properties": {
                "string":  "val",
                "int": 1,
                "double": 1.2,
                "bool": true,
                "obj": {"nested": "value"},
                "list": [1,2,3],
                "geometry": [1,2]
              }
            }"#,
            vec![feature(
                new_point(1.0, 2.0),
                tags! {
                    "string" => "val",
                    "int" => 1,
                    "double" => 1.2,
                    "bool" => true,
                    "obj" => Value::map(tags! {"nested" => "value"}),
                    "list" => Value::list([Value::Int(1), Value::Int(2), Value::Int(3)]),
                    "geometry" => Value::list([Value::Int(1), Value::Int(2)]),
                },
            )],
        );
    }

    #[test]
    fn newline_delimited() {
        test_parse(
            r#"{"type": "Feature", "geometry": {"type": "Point", "coordinates": [1, 2]}}
{"type": "Feature", "geometry": {"type": "Point", "coordinates": [3, 4]}}
{"type": "Feature", "geometry": {"type": "Point", "coordinates": [5, 6]}}
"#,
            vec![
                feature(new_point(1.0, 2.0), Tags::default()),
                feature(new_point(3.0, 4.0), Tags::default()),
                feature(new_point(5.0, 6.0), Tags::default()),
            ],
        );
    }

    #[test]
    fn feature_collection() {
        test_parse(
            r#"{
              "type": "FeatureCollection",
              "features": [
                {"type": "Feature", "geometry": {"type": "Point", "coordinates": [1, 2]}},
                {"type": "Feature", "geometry": {"type": "Point", "coordinates": [3, 4]}},
                {"type": "Feature", "geometry": {"type": "Point", "coordinates": [5, 6]}}
              ]
            }"#,
            vec![
                feature(new_point(1.0, 2.0), Tags::default()),
                feature(new_point(3.0, 4.0), Tags::default()),
                feature(new_point(5.0, 6.0), Tags::default()),
            ],
        );
    }

    #[test]
    fn empty_feature_collection() {
        test_parse(r#"{"type": "FeatureCollection", "features": []}"#, vec![]);
    }

    #[test]
    fn parse_bad_features() {
        test_parse_count(
            r#"{"type": "Garbage", "geometry": {"type": "Point", "coordinates": [1, 2]}}
{"type": "Feature", "geometry": {"type": "Point", "coordinates": [3, 4], "other2": "value"}, "other": "value"}
{"type": "Feature", "geometry": {"type": "Garbage", "coordinates": [5, 6]}}
{"type": "FeatureCollection", "features": {}}
"#,
            vec![
                feature(new_point(1.0, 2.0), Tags::default()),
                feature(new_point(3.0, 4.0), Tags::default()),
                feature(geo_utils::empty_geometry(), Tags::default()),
            ],
            3,
        );
    }

    #[test]
    fn bad_geometries() {
        for (json, expected) in [
            (r#"{"type": "Feature", "geometry": {"type": "Point", "coordinates": [[1, 2]]}}"#, "POINT EMPTY"),
            (r#"{"type": "Feature", "geometry": {"type": "Point", "coordinates": [[1]]}}"#, "POINT EMPTY"),
            (r#"{"type": "Feature", "geometry": {"type": "Point", "coordinates": {}}}"#, "GEOMETRYCOLLECTION EMPTY"),
            (r#"{"type": "Feature", "geometry": {"type": "Point", "coordinates": 1}}"#, "GEOMETRYCOLLECTION EMPTY"),
            (r#"{"type": "Feature", "geometry": {"type": "Point", "coordinates": "1"}}"#, "GEOMETRYCOLLECTION EMPTY"),
            (r#"{"type": "Feature", "geometry": {"type": 1, "coordinates": [1,1]}}"#, "GEOMETRYCOLLECTION EMPTY"),
            (r#"{"type": "Feature", "geometry": {"type": [], "coordinates": [1,1]}}"#, "GEOMETRYCOLLECTION EMPTY"),
            (r#"{"type": "Feature", "geometry": {"type": {}, "coordinates": [1,1]}}"#, "GEOMETRYCOLLECTION EMPTY"),
            (r#"{"type": "Feature", "geometry": []}"#, "GEOMETRYCOLLECTION EMPTY"),
            (r#"{"type": "Feature", "geometry": null}"#, "GEOMETRYCOLLECTION EMPTY"),
            (r#"{"type": "Feature", "geometry": "1"}"#, "GEOMETRYCOLLECTION EMPTY"),
            (r#"{"type": "Feature", "geometry": {"type": "Point", "coordinates": [1, {}]}}"#, "POINT EMPTY"),
            (r#"{"type": "Feature", "geometry": {"type": "MultiPoint", "coordinates": [1, 2]}}"#, "MULTIPOINT EMPTY"),
            (
                r#"{"type": "Feature", "geometry": {"type": "MultiPoint", "coordinates": [[1, 2], {}]}}"#,
                "MULTIPOINT ((1 2))",
            ),
            (
                r#"{"type": "Feature", "geometry": {"type": "MultiPoint", "coordinates": [{}, [1, 2]]}}"#,
                "MULTIPOINT ((1 2))",
            ),
            (r#"{"type": "Feature", "geometry": {"type": "LineString", "coordinates": [1, 2]}}"#, "LINESTRING EMPTY"),
            (r#"{"type": "Feature", "geometry": {"type": "Polygon", "coordinates": [1, 2]}}"#, "POLYGON EMPTY"),
            (r#"{"type": "Feature", "geometry": {"type": "Polygon", "coordinates": [[[1, 2]]]}}"#, "POLYGON EMPTY"),
            (
                r#"{"type": "Feature", "geometry": {"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 1]]]}}"#,
                "POLYGON EMPTY",
            ),
        ] {
            let expected = geo_utils::read_wkt(expected).unwrap();
            test_parse(json, vec![feature(expected, Tags::default())]);
        }
    }

    #[test]
    fn really_bad_geometries() {
        for json in ["{}", "[]", "\"string\"", "1", "null", "false"] {
            test_parse(json, vec![]);
        }
    }

    #[test]
    fn syntax_error() {
        let result =
            GeoJson::from_str(r#"{"type": "Feature", "geometry": {"type": "Point", "coordinates": [1, 2]}"#).features();
        assert!(result.is_err());
    }
}
