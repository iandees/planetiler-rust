//! Port of `geo/GeometryType.java`.

use geo_types::Geometry;

use crate::proto::vector_tile::tile::GeomType;

/// The geometry type of a vector tile feature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum GeometryType {
    #[default]
    Unknown,
    Point,
    Line,
    Polygon,
}

impl GeometryType {
    /// Returns the type of a geometry, or the common type of all elements of a geometry collection, or
    /// [`GeometryType::Unknown`] if they differ.
    pub fn type_of(geom: &Geometry<f64>) -> GeometryType {
        match geom {
            Geometry::GeometryCollection(gc) if !gc.0.is_empty() => {
                let result = Self::type_of(&gc.0[0]);
                if gc.0[1..].iter().all(|g| Self::type_of(g) == result) { result } else { GeometryType::Unknown }
            }
            Geometry::Point(_) | Geometry::MultiPoint(_) => GeometryType::Point,
            Geometry::Line(_) | Geometry::LineString(_) | Geometry::MultiLineString(_) => GeometryType::Line,
            Geometry::Polygon(_) | Geometry::MultiPolygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => {
                GeometryType::Polygon
            }
            Geometry::GeometryCollection(_) => GeometryType::Unknown,
        }
    }

    pub fn from_protobuf(geom_type: GeomType) -> GeometryType {
        match geom_type {
            GeomType::Point => GeometryType::Point,
            GeomType::Linestring => GeometryType::Line,
            GeomType::Polygon => GeometryType::Polygon,
            GeomType::Unknown => GeometryType::Unknown,
        }
    }

    pub fn as_protobuf(&self) -> GeomType {
        match self {
            GeometryType::Point => GeomType::Point,
            GeometryType::Line => GeomType::Linestring,
            GeometryType::Polygon => GeomType::Polygon,
            GeometryType::Unknown => GeomType::Unknown,
        }
    }

    /// Same as [`GeometryType::from_protobuf`] using the integer protobuf value.
    pub fn from_byte(val: u8) -> GeometryType {
        GeomType::try_from(val as i32).map_or(GeometryType::Unknown, Self::from_protobuf)
    }

    /// The integer protobuf value.
    pub fn as_byte(&self) -> u8 {
        self.as_protobuf() as i32 as u8
    }

    /// Minimum number of points a geometry of this type needs to be valid.
    pub fn min_points(&self) -> usize {
        match self {
            GeometryType::Unknown => 0,
            GeometryType::Point => 1,
            GeometryType::Line => 2,
            GeometryType::Polygon => 4,
        }
    }

    /// The string used in `Expression.matchType` for this type.
    pub fn match_type_string(&self) -> &'static str {
        match self {
            GeometryType::Unknown => "unknown",
            GeometryType::Point => "point",
            GeometryType::Line => "linestring",
            GeometryType::Polygon => "polygon",
        }
    }

    /// Detect geometry type from the header of a WKB-encoded geometry.
    pub fn from_wkb(wkb: &[u8]) -> GeometryType {
        if wkb.len() < 5 {
            return GeometryType::Unknown;
        }
        let bytes = [wkb[1], wkb[2], wkb[3], wkb[4]];
        let geom_type = if wkb[0] == 1 { u32::from_le_bytes(bytes) } else { u32::from_be_bytes(bytes) };
        match geom_type {
            1 | 4 => GeometryType::Point,
            2 | 5 => GeometryType::Line,
            3 | 6 => GeometryType::Polygon,
            _ => GeometryType::Unknown,
        }
    }

    /// Detect geometry type from the prefix of a WKT-encoded geometry.
    pub fn from_wkt(wkt: &str) -> GeometryType {
        let lower = wkt.trim_start().to_ascii_lowercase();
        let rest = lower.strip_prefix("multi").unwrap_or(&lower);
        if rest.starts_with("point") {
            GeometryType::Point
        } else if rest.starts_with("line") {
            GeometryType::Line
        } else if rest.starts_with("polygon") {
            GeometryType::Polygon
        } else {
            GeometryType::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::{GeometryCollection, line_string, point, polygon};

    #[test]
    fn type_of() {
        assert_eq!(GeometryType::Point, GeometryType::type_of(&point!(x: 1.0, y: 2.0).into()));
        assert_eq!(GeometryType::Line, GeometryType::type_of(&line_string![(x: 0.0, y: 0.0), (x: 1.0, y: 1.0)].into()));
        let poly: Geometry<f64> =
            polygon![(x: 0.0, y: 0.0), (x: 1.0, y: 0.0), (x: 1.0, y: 1.0), (x: 0.0, y: 0.0)].into();
        assert_eq!(GeometryType::Polygon, GeometryType::type_of(&poly));
        let mixed = Geometry::GeometryCollection(GeometryCollection(vec![poly, point!(x: 1.0, y: 2.0).into()]));
        assert_eq!(GeometryType::Unknown, GeometryType::type_of(&mixed));
    }

    #[test]
    fn wkt() {
        assert_eq!(GeometryType::Polygon, GeometryType::from_wkt(" MULTIPOLYGON (((0 0, 1 1, 0 0)))"));
        assert_eq!(GeometryType::Line, GeometryType::from_wkt("LINESTRING (0 0, 1 1)"));
        assert_eq!(GeometryType::Unknown, GeometryType::from_wkt("GEOMETRYCOLLECTION EMPTY"));
    }

    #[test]
    fn bytes() {
        for t in [GeometryType::Point, GeometryType::Line, GeometryType::Polygon, GeometryType::Unknown] {
            assert_eq!(t, GeometryType::from_byte(t.as_byte()));
        }
    }
}
