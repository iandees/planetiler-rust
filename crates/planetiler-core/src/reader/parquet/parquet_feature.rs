//! Port of `reader/parquet/ParquetFeature.java`: a single record read from a geoparquet file.
//!
//! The geometry is decoded lazily on first access (WKB/WKT), or eagerly for geoarrow-encoded columns which are
//! decoded straight from arrow arrays. The primary geometry column is removed from the tags.
//!
//! `ParquetFeature` implements [`SourceFeature`] directly (like Java's `ParquetFeature extends SourceFeature`) rather
//! than being converted to a `SimpleFeature`: that keeps WKB/WKT decoding lazy, so features a profile ignores based
//! on their tags never have their geometry parsed, and avoids moving tags into a second struct. Dotted tag access
//! (`names.primary`) comes from the default [`WithTags`](crate::reader::with_tags::WithTags) methods.

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use geo_types::Geometry;
use parquet::schema::types::SchemaDescPtr;

use crate::geo::geo_utils;
use crate::geo::geometry_exception::GeometryResult;
use crate::geo::geometry_type::GeometryType;
use crate::geo::with_geometry::{GeometryCache, WithGeometry};
use crate::reader::parquet::geometry_reader::GeometryReader;
use crate::reader::simple_reader::ReaderFeature;
use crate::reader::source_feature::{SourceFeature, SourceFeatureBase};
use crate::reader::with_geometry_type::WithGeometryType;
use crate::value::{Tags, Value};

/// A single record read from a geoparquet file.
pub struct ParquetFeature {
    /// Tags (minus the geometry column), source, layer and id.
    base: SourceFeatureBase,
    raw_geometry: Value,
    geometry_reader: Arc<GeometryReader>,
    path: Arc<PathBuf>,
    schema: Option<SchemaDescPtr>,
    lat_lon: OnceLock<GeometryResult<Geometry<f64>>>,
    world: OnceLock<GeometryResult<Geometry<f64>>>,
    geometry_type: OnceLock<GeometryType>,
}

impl ParquetFeature {
    /// Creates a feature from `tags`, removing the primary geometry column from them.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: Arc<str>,
        source_layer: Arc<str>,
        id: i64,
        geometry_reader: Arc<GeometryReader>,
        mut tags: Tags,
        path: Arc<PathBuf>,
        schema: Option<SchemaDescPtr>,
    ) -> ParquetFeature {
        let raw_geometry = tags.shift_remove(&geometry_reader.geometry_column).unwrap_or(Value::Null);
        ParquetFeature {
            base: SourceFeatureBase::new(tags, Some(source), Some(source_layer), Vec::new(), id),
            raw_geometry,
            geometry_reader,
            path,
            schema,
            lat_lon: OnceLock::new(),
            world: OnceLock::new(),
            geometry_type: OnceLock::new(),
        }
    }

    /// Creates a feature with an already-decoded lat/lon geometry (`None` if the geometry column was null).
    pub(crate) fn with_geometry(self, geometry: Option<Geometry<f64>>) -> ParquetFeature {
        if let Some(geometry) = geometry {
            let _ = self.lat_lon.set(Ok(geometry));
        }
        self
    }

    pub fn source(&self) -> &str {
        self.base.source.as_deref().unwrap_or_default()
    }

    pub fn source_layer(&self) -> &str {
        self.base.source_layer.as_deref().unwrap_or_default()
    }

    pub fn id(&self) -> i64 {
        self.base.id
    }

    /// Returns the parquet file that this feature was read from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the schema of the parquet file that this feature was read from.
    pub fn parquet_schema(&self) -> Option<&SchemaDescPtr> {
        self.schema.as_ref()
    }

    /// The geometry in latitude/longitude coordinates.
    pub fn lat_lon_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        self.lat_lon
            .get_or_init(|| {
                self.geometry_reader.parse_geometry(&self.raw_geometry, &self.geometry_reader.geometry_column)
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// The geometry in world web mercator coordinates (0-1), with polygons sorted by area descending.
    pub fn world_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        self.world
            .get_or_init(|| {
                let lat_lon = self.lat_lon_geometry()?;
                Ok(geo_utils::sort_polygons_by_area_descending(geo_utils::lat_lon_to_world_coords(lat_lon)))
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    fn geometry_type(&self) -> GeometryType {
        *self.geometry_type.get_or_init(|| {
            let sniffed = if self.raw_geometry.is_null() && self.lat_lon.get().is_some() {
                match self.geometry_reader.primary_encoding() {
                    Some(crate::reader::parquet::geometry_reader::GeometryEncoding::GeoArrow(e)) => {
                        crate::reader::parquet::geometry_reader::geo_arrow_type(e)
                    }
                    _ => GeometryType::Unknown,
                }
            } else {
                self.geometry_reader.sniff_geometry_type(&self.raw_geometry, &self.geometry_reader.geometry_column)
            };
            if sniffed != GeometryType::Unknown {
                return sniffed;
            }
            match self.lat_lon_geometry() {
                Ok(Geometry::Point(_) | Geometry::MultiPoint(_)) => GeometryType::Point,
                Ok(Geometry::LineString(_) | Geometry::MultiLineString(_) | Geometry::Line(_)) => GeometryType::Line,
                Ok(Geometry::Polygon(_) | Geometry::MultiPolygon(_) | Geometry::Rect(_) | Geometry::Triangle(_)) => {
                    GeometryType::Polygon
                }
                _ => GeometryType::Unknown,
            }
        })
    }

    pub fn is_point(&self) -> bool {
        self.geometry_type() == GeometryType::Point
    }

    pub fn can_be_polygon(&self) -> bool {
        self.geometry_type() == GeometryType::Polygon
    }

    pub fn can_be_line(&self) -> bool {
        self.geometry_type() == GeometryType::Line
    }

    /// Convert to a plain [`ReaderFeature`], decoding the geometry (moves the geometry and tags out).
    pub fn into_reader_feature(mut self) -> GeometryResult<ReaderFeature> {
        self.lat_lon_geometry()?;
        let geometry = self.lat_lon.take().expect("geometry decoded").expect("geometry ok");
        let base = self.base;
        Ok(ReaderFeature { geometry, tags: base.tags, source_layer: base.source_layer, id: base.id })
    }
}

impl WithGeometryType for ParquetFeature {
    fn is_point(&self) -> bool {
        ParquetFeature::is_point(self)
    }

    fn can_be_polygon(&self) -> bool {
        ParquetFeature::can_be_polygon(self)
    }

    fn can_be_line(&self) -> bool {
        ParquetFeature::can_be_line(self)
    }
}

impl WithGeometry for ParquetFeature {
    fn world_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        ParquetFeature::world_geometry(self)
    }

    fn lat_lon_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        ParquetFeature::lat_lon_geometry(self)
    }

    fn geometry_cache(&self) -> &GeometryCache {
        &self.base.geometry_cache
    }
}

impl SourceFeature for ParquetFeature {
    fn base(&self) -> &SourceFeatureBase {
        &self.base
    }

    fn as_any_feature(&self) -> &dyn Any {
        self
    }
}

crate::impl_source_feature_traits!(ParquetFeature);

impl std::fmt::Debug for ParquetFeature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // like Java's toString(): just the tags
        write!(f, "{:?}", self.base.tags)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::reader::parquet::geo_parquet_metadata::{ColumnMetadata, GeoParquetMetadata};
    use crate::reader::with_tags::WithTags;
    use crate::tags;

    fn feature(tags: Tags) -> ParquetFeature {
        let metadata = GeoParquetMetadata {
            version: None,
            primary_column: "geometry".into(),
            columns: HashMap::from([("geometry".to_string(), ColumnMetadata::new("WKB"))]),
        };
        ParquetFeature::new(
            "overture".into(),
            "layer".into(),
            1,
            Arc::new(GeometryReader::new(&metadata).unwrap()),
            tags,
            Arc::new(PathBuf::new()),
            None,
        )
    }

    #[test]
    fn has_tag() {
        let feature = feature(tags! {"names" => Value::map(tags!{"primary" => "name"})});
        assert!(feature.has_tag("names.primary"));
        assert!(feature.has_tag("names[].primary"));
        assert!(feature.has_tag_str("names.primary", "name"));
        assert!(feature.has_tag_str("names[].primary", "name"));
        assert!(feature.has_tag_any("names[].primary", &["name", "name2"]));
        assert!(!feature.has_tag_str("names.primary", "not name"));
        assert!(!feature.has_tag_any("names.primary", &["not name", "not name 2"]));
        assert!(feature.has_tag_any("names.primary", &["name1", "name"]));
        assert!(feature.has_tag_any("names.primary", &["name", "name1"]));
    }

    #[test]
    fn geometry() {
        let wkb = geo_utils::write_wkb(&geo_types::Point::new(1.0, 2.0).into()).unwrap();
        let feature = feature(tags! {"geometry" => Value::Bytes(wkb.into()), "a" => 1});
        assert!(!feature.has_tag("geometry"));
        assert!(feature.is_point());
        assert!(!feature.can_be_line());
        assert_eq!(&Geometry::Point(geo_types::Point::new(1.0, 2.0)), feature.lat_lon_geometry().unwrap());
        let reader_feature = feature.into_reader_feature().unwrap();
        assert_eq!(Some("layer"), reader_feature.source_layer.as_deref());
        assert_eq!(1, reader_feature.tags.len());
    }

    #[test]
    fn missing_geometry() {
        let feature = feature(tags! {"a" => 1});
        assert!(feature.lat_lon_geometry().is_err());
        assert!(!feature.is_point());
    }
}
