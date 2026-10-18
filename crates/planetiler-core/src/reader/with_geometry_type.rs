//! Port of `reader/WithGeometryType.java`.

/// Something attached to a geometry that can be matched using a `matchType` geometry type filter expression.
pub trait WithGeometryType {
    /// True if this feature can be interpreted as a point or multipoint.
    fn is_point(&self) -> bool;

    /// True if this feature can be interpreted as a polygon or multipolygon.
    ///
    /// A closed ring can either be a polygon or linestring, so return false to not allow this closed ring to be treated
    /// as a polygon.
    fn can_be_polygon(&self) -> bool;

    /// True if this feature can be interpreted as a linestring or multilinestring.
    ///
    /// A closed ring can either be a polygon or linestring, so return false to not allow this closed ring to be treated
    /// as a linestring.
    fn can_be_line(&self) -> bool;
}
