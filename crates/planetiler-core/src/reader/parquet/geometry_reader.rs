//! Port of `reader/parquet/GeometryReader.java`: decodes geometries from a parquet record based on the
//! [`GeoParquetMetadata`] provided.

use std::collections::HashMap;

use anyhow::{Result, bail};
use geo_types::Geometry;

use crate::geo::geo_utils;
use crate::geo::geometry_exception::{GeometryException, GeometryResult};
use crate::geo::geometry_type::GeometryType;
use crate::reader::parquet::geo_arrow::{self, GeoArrowEncoding};
use crate::reader::parquet::geo_parquet_metadata::GeoParquetMetadata;
use crate::value::{Tags, Value};

/// How geometries in a column are encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryEncoding {
    Wkb,
    Wkt,
    GeoArrow(GeoArrowEncoding),
}

impl GeometryEncoding {
    pub fn parse(encoding: &str) -> Result<GeometryEncoding> {
        Ok(match encoding {
            "WKB" => GeometryEncoding::Wkb,
            "WKT" => GeometryEncoding::Wkt,
            other => match GeoArrowEncoding::parse(other) {
                Some(e) => GeometryEncoding::GeoArrow(e),
                None => bail!("Unhandled type: {other}"),
            },
        })
    }
}

/// Decodes geometry columns of a geoparquet file.
#[derive(Clone, Debug)]
pub struct GeometryReader {
    converters: HashMap<String, GeometryEncoding>,
    /// Name of the primary geometry column.
    pub geometry_column: String,
}

impl GeometryReader {
    pub fn new(geoparquet: &GeoParquetMetadata) -> Result<GeometryReader> {
        let mut converters = HashMap::new();
        for (column, info) in &geoparquet.columns {
            converters.insert(column.clone(), GeometryEncoding::parse(&info.encoding)?);
        }
        Ok(GeometryReader { converters, geometry_column: geoparquet.primary_column.clone() })
    }

    /// Encoding of `column`, if it is a geometry column.
    pub fn encoding(&self, column: &str) -> Option<GeometryEncoding> {
        self.converters.get(column).copied()
    }

    /// Encoding of the primary geometry column.
    pub fn primary_encoding(&self) -> Option<GeometryEncoding> {
        self.encoding(&self.geometry_column)
    }

    pub fn read_primary_geometry(&self, tags: &Tags) -> GeometryResult<Geometry<f64>> {
        self.read_geometry(tags, &self.geometry_column)
    }

    pub fn read_geometry(&self, tags: &Tags, column: &str) -> GeometryResult<Geometry<f64>> {
        self.parse_geometry(tags.get(column).unwrap_or(&Value::Null), column)
    }

    pub fn parse_geometry(&self, value: &Value, column: &str) -> GeometryResult<Geometry<f64>> {
        let converter = self.converters.get(column);
        if value.is_null() {
            return Err(GeometryException::new(
                "no_parquet_column",
                format!("Missing geometry column column {column}"),
            ));
        }
        let Some(converter) = converter else {
            return Err(GeometryException::new("no_converter", format!("No geometry converter for {column}")));
        };
        let error = |e: String| GeometryException::new("error_reading", format!("Error reading {column}: {e}"));
        match (converter, value) {
            (GeometryEncoding::Wkb, Value::Bytes(bytes)) => geo_utils::read_wkb(bytes),
            (GeometryEncoding::Wkt, Value::String(s)) => geo_utils::read_wkt(s),
            (GeometryEncoding::GeoArrow(e), value) => {
                geo_arrow::from_value(*e, value).map_err(|e| error(e.to_string()))
            }
            (_, other) => Err(error(format!("unexpected value {other}"))),
        }
    }

    pub fn sniff_geometry_type(&self, value: &Value, column: &str) -> GeometryType {
        match (self.converters.get(column), value) {
            (Some(GeometryEncoding::Wkb), Value::Bytes(bytes)) => GeometryType::from_wkb(bytes),
            (Some(GeometryEncoding::Wkt), Value::String(s)) => GeometryType::from_wkt(s),
            (Some(GeometryEncoding::GeoArrow(e)), _) => geo_arrow_type(*e),
            _ => GeometryType::Unknown,
        }
    }
}

/// Geometry type of a geoarrow encoding.
pub fn geo_arrow_type(encoding: GeoArrowEncoding) -> GeometryType {
    match encoding {
        GeoArrowEncoding::Point | GeoArrowEncoding::MultiPoint => GeometryType::Point,
        GeoArrowEncoding::LineString | GeoArrowEncoding::MultiLineString => GeometryType::Line,
        GeoArrowEncoding::Polygon | GeoArrowEncoding::MultiPolygon => GeometryType::Polygon,
    }
}
