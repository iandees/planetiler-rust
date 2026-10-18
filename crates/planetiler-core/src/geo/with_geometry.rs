//! Port of `geo/WithGeometry.java`: wraps a geometry and provides cached accessors for derived geometries and
//! measurements.
//!
//! All geometries except [`WithGeometry::lat_lon_geometry`] are in world web mercator coordinates where `(0, 0)` is
//! the northwest corner and `(1, 1)` is the southeast corner of the planet.
//!
//! Implementors provide [`WithGeometry::world_geometry`], [`WithGeometry::lat_lon_geometry`], the geometry type
//! predicates (supertrait [`WithGeometryType`]) and a [`GeometryCache`] (returned from [`WithGeometry::geometry_cache`]) that the provided methods use
//! to memoize results. [`WithGeometry::compute_line`] and [`WithGeometry::compute_polygon`] can be overridden (Java's
//! protected `computeLine` / `computePolygon`).

use std::sync::OnceLock;

use geo_types::{Geometry, Point};
use parking_lot::Mutex;

use crate::geo::geo_utils;
use crate::geo::geometry_exception::{GeometryException, GeometryResult};
use crate::geo::interior_point;
use crate::geo::line_splitter::LineSplitter;
use crate::geo::maximum_inscribed_circle;
use crate::geo::unit;
pub use crate::reader::with_geometry_type::WithGeometryType;

/// Memoized results for the provided methods of [`WithGeometry`].
///
/// The memoized values are allocated on first use, so that creating (and moving) a source feature whose derived
/// geometries are never requested stays cheap (most OSM elements never need them).
#[derive(Default)]
pub struct GeometryCache(OnceLock<Box<CacheEntries>>);

#[derive(Default)]
struct CacheEntries {
    centroid: OnceLock<GeometryResult<Geometry<f64>>>,
    point_on_surface: OnceLock<GeometryResult<Geometry<f64>>>,
    centroid_if_convex: OnceLock<GeometryResult<Geometry<f64>>>,
    innermost_point: Mutex<Option<(f64, Geometry<f64>)>>,
    line_midpoint: OnceLock<GeometryResult<Geometry<f64>>>,
    linear_geometry: OnceLock<GeometryResult<Option<Geometry<f64>>>>,
    polygon_geometry: OnceLock<GeometryResult<Option<Geometry<f64>>>>,
    valid_polygon: OnceLock<GeometryResult<Geometry<f64>>>,
    area: OnceLock<GeometryResult<f64>>,
    length: OnceLock<GeometryResult<f64>>,
    area_meters: OnceLock<GeometryResult<f64>>,
    length_meters: OnceLock<GeometryResult<f64>>,
    line_splitter: OnceLock<GeometryResult<LineSplitter>>,
}

impl std::fmt::Debug for GeometryCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GeometryCache")
    }
}

impl GeometryCache {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    fn get(&self) -> &CacheEntries {
        self.0.get_or_init(Box::default)
    }
}

/// Get or compute a memoized fallible geometry.
pub fn cached(
    cell: &OnceLock<GeometryResult<Geometry<f64>>>,
    f: impl FnOnce() -> GeometryResult<Geometry<f64>>,
) -> GeometryResult<&Geometry<f64>> {
    cell.get_or_init(f).as_ref().map_err(Clone::clone)
}

fn cached_f64(cell: &OnceLock<GeometryResult<f64>>, f: impl FnOnce() -> GeometryResult<f64>) -> GeometryResult<f64> {
    cell.get_or_init(f).clone()
}

fn point_or_empty(c: Option<geo_types::Coord<f64>>) -> Geometry<f64> {
    c.map_or_else(geo_utils::empty_point, |c| Geometry::Point(Point(c)))
}

/// `Send + Sync` so borrowed features can be held by values that require it (e.g. CEL values in custommap) without
/// any `unsafe`; every implementation memoizes through `OnceLock`, which is `Sync`.
pub trait WithGeometry: WithGeometryType + Send + Sync {
    /// This geometry in world web mercator coordinates.
    fn world_geometry(&self) -> GeometryResult<&Geometry<f64>>;

    /// This geometry in latitude/longitude degree coordinates.
    fn lat_lon_geometry(&self) -> GeometryResult<&Geometry<f64>>;

    /// Storage for the memoized results of the provided methods.
    fn geometry_cache(&self) -> &GeometryCache;

    /// Computes this feature as a line string or multilinestring in world coordinates, or `None` if
    /// [`WithGeometry::world_geometry`] can be used as-is (avoids copying the geometry).
    fn compute_line(&self) -> GeometryResult<Option<Geometry<f64>>> {
        let world = self.world_geometry()?;
        match world {
            Geometry::LineString(_) | Geometry::MultiLineString(_) | Geometry::Line(_) => Ok(None),
            other => geo_utils::polygon_to_line_string(other).map(Some),
        }
    }

    /// Computes this feature as a polygon or multipolygon in world coordinates, or `None` if
    /// [`WithGeometry::world_geometry`] can be used as-is (avoids copying the geometry).
    fn compute_polygon(&self) -> GeometryResult<Option<Geometry<f64>>> {
        Ok(None)
    }

    /// Area of this feature in world web mercator coordinates where `1` means the area of the entire planet.
    fn area(&self) -> GeometryResult<f64> {
        cached_f64(&self.geometry_cache().get().area, || {
            if self.can_be_polygon() { Ok(geo_utils::area(self.polygon()?).abs()) } else { Ok(0.0) }
        })
    }

    /// Length of this feature in world web mercator coordinates where `1` means the circumference of the planet.
    fn length(&self) -> GeometryResult<f64> {
        cached_f64(&self.geometry_cache().get().length, || {
            if self.is_point() || self.can_be_polygon() || self.can_be_line() {
                Ok(geo_utils::length(self.world_geometry()?))
            } else {
                Ok(0.0)
            }
        })
    }

    /// The square root of [`WithGeometry::area`] if polygon or [`WithGeometry::length`] if a line string.
    fn size(&self) -> GeometryResult<f64> {
        if self.can_be_polygon() {
            Ok(self.area()?.abs().sqrt())
        } else if self.can_be_line() {
            self.length()
        } else {
            Ok(0.0)
        }
    }

    /// The approximate area of the geometry in square meters.
    fn area_meters(&self) -> GeometryResult<f64> {
        cached_f64(&self.geometry_cache().get().area_meters, || {
            if self.is_point() || self.can_be_polygon() || self.can_be_line() {
                Ok(geo_utils::area_in_meters(self.lat_lon_geometry()?))
            } else {
                Ok(0.0)
            }
        })
    }

    /// The approximate length of the geometry in meters.
    fn length_meters(&self) -> GeometryResult<f64> {
        cached_f64(&self.geometry_cache().get().length_meters, || {
            if self.is_point() || self.can_be_polygon() || self.can_be_line() {
                Ok(geo_utils::length_in_meters(self.lat_lon_geometry()?))
            } else {
                Ok(0.0)
            }
        })
    }

    /// The square root of [`WithGeometry::area_meters`] if polygon or [`WithGeometry::length_meters`] if a line.
    fn size_meters(&self) -> GeometryResult<f64> {
        if self.can_be_polygon() {
            Ok(self.area_meters()?.abs().sqrt())
        } else if self.can_be_line() {
            self.length_meters()
        } else {
            Ok(0.0)
        }
    }

    /// The length of this geometry in `unit` (Java's `length(Unit.Length)`).
    fn length_in(&self, unit: unit::Length) -> GeometryResult<f64> {
        unit.of(self)
    }

    /// The length of this geometry if it is a line or the square root of the area if it is a polygon in `unit`.
    fn size_in(&self, unit: unit::Length) -> GeometryResult<f64> {
        if self.can_be_polygon() {
            Ok(unit.from_base_unit(unit.base().area(self)?.sqrt()))
        } else {
            Ok(unit.from_base_unit(unit.base().length(self)?))
        }
    }

    /// The area of this geometry in `unit` (Java's `area(Unit.Area)`).
    fn area_in(&self, unit: unit::Area) -> GeometryResult<f64> {
        unit.of(self)
    }

    /// The centroid of this geometry in world coordinates.
    fn centroid(&self) -> GeometryResult<&Geometry<f64>> {
        cached(&self.geometry_cache().get().centroid, || {
            let geom = if self.can_be_polygon() {
                self.polygon()?
            } else if self.can_be_line() {
                self.line()?
            } else {
                self.world_geometry()?
            };
            Ok(point_or_empty(interior_point::centroid(geom)))
        })
    }

    /// A point on the surface of this geometry (JTS `getInteriorPoint`) in world coordinates.
    fn point_on_surface(&self) -> GeometryResult<&Geometry<f64>> {
        cached(&self.geometry_cache().get().point_on_surface, || {
            let geom = if self.can_be_polygon() {
                self.polygon()?
            } else if self.can_be_line() {
                self.line()?
            } else {
                self.world_geometry()?
            };
            Ok(point_or_empty(interior_point::interior_point(geom)))
        })
    }

    /// The center of the maximum inscribed circle of this geometry in world coordinates.
    ///
    /// `tolerance` is the precision for calculating the maximum inscribed circle: 0.01 means 1% of the square root of
    /// the area. Values between 0.05-0.1 are a good compromise of performance vs. precision.
    fn innermost_point(&self, tolerance: f64) -> GeometryResult<Geometry<f64>> {
        if self.can_be_polygon() {
            let cache = self.geometry_cache().get();
            if let Some((tol, point)) = &*cache.innermost_point.lock()
                && *tol == tolerance
            {
                return Ok(point.clone());
            }
            let polygon = self.polygon()?;
            let point = point_or_empty(maximum_inscribed_circle::get_center(polygon, self.area()?.sqrt() * tolerance));
            *cache.innermost_point.lock() = Some((tolerance, point.clone()));
            Ok(point)
        } else if self.can_be_line() {
            self.line_midpoint().cloned()
        } else {
            self.point_on_surface().cloned()
        }
    }

    /// The midpoint of this line, or the longest segment if it is a multilinestring.
    fn line_midpoint(&self) -> GeometryResult<&Geometry<f64>> {
        cached(&self.geometry_cache().get().line_midpoint, || self.point_along_line(0.5))
    }

    /// A point along this line where `ratio=0` is the start, `ratio=1` the end and `0.5` the midpoint. When this is
    /// a multilinestring, the longest segment is used.
    fn point_along_line(&self, ratio: f64) -> GeometryResult<Geometry<f64>> {
        let splitter = self.geometry_cache().get().line_splitter.get_or_init(|| {
            let line = self.line()?;
            match line {
                Geometry::MultiLineString(multi) => match geo_utils::get_longest_line(multi) {
                    Some(longest) => Ok(LineSplitter::new(longest.clone())),
                    None => Err(GeometryException::non_fatal("empty_multilinestring", "no lines")),
                },
                other => LineSplitter::from_geometry(other),
            }
        });
        let splitter = splitter.as_ref().map_err(Clone::clone)?;
        Ok(Geometry::Point(splitter.get_point(ratio)))
    }

    /// A point inside the geometry: the centroid if it is a convex polygon, otherwise
    /// [`WithGeometry::point_on_surface`].
    fn centroid_if_convex(&self) -> GeometryResult<&Geometry<f64>> {
        cached(&self.geometry_cache().get().centroid_if_convex, || {
            if !self.can_be_polygon() {
                return self.centroid().cloned();
            }
            match self.polygon()? {
                Geometry::Polygon(poly) if poly.interiors().is_empty() && geo_utils::is_convex(poly.exterior()) => {
                    self.centroid().cloned()
                }
                // multipolygon, polygon with holes, or concave polygon
                _ => self.point_on_surface().cloned(),
            }
        })
    }

    /// This feature as a line string or multilinestring in world coordinates.
    fn line(&self) -> GeometryResult<&Geometry<f64>> {
        if !self.can_be_line() {
            return Err(GeometryException::non_fatal("feature_not_line", "cannot be line"));
        }
        match self.geometry_cache().get().linear_geometry.get_or_init(|| self.compute_line()) {
            Ok(Some(line)) => Ok(line),
            Ok(None) => self.world_geometry(),
            Err(e) => Err(e.clone()),
        }
    }

    /// A partial line string from `start` to `end` where 0 is the beginning of the line and 1 is the end.
    ///
    /// Fails for multilinestrings.
    fn partial_line(&self, start: f64, end: f64) -> GeometryResult<Geometry<f64>> {
        let line = self.line()?;
        if start <= 0.0 && end >= 1.0 {
            return Ok(line.clone());
        }
        match line {
            Geometry::LineString(_) | Geometry::Line(_) => {
                let splitter =
                    self.geometry_cache().get().line_splitter.get_or_init(|| LineSplitter::from_geometry(line));
                let splitter = splitter.as_ref().map_err(Clone::clone)?;
                Ok(Geometry::LineString(splitter.get(start, end)))
            }
            _ => Err(GeometryException::non_fatal("partial_multilinestring", "cannot get partial of a multiline")),
        }
    }

    /// This feature as a polygon or multipolygon in world coordinates.
    fn polygon(&self) -> GeometryResult<&Geometry<f64>> {
        if !self.can_be_polygon() {
            return Err(GeometryException::non_fatal("feature_not_polygon", "cannot be polygon"));
        }
        match self.geometry_cache().get().polygon_geometry.get_or_init(|| self.compute_polygon()) {
            Ok(Some(polygon)) => Ok(polygon),
            Ok(None) => self.world_geometry(),
            Err(e) => Err(e.clone()),
        }
    }

    /// This feature as a valid polygon or multipolygon in world coordinates. Validating and fixing invalid polygons
    /// can be expensive, so use only if necessary (invalid polygons are also fixed at render time).
    fn validated_polygon(&self) -> GeometryResult<&Geometry<f64>> {
        if !self.can_be_polygon() {
            return Err(GeometryException::non_fatal("feature_not_polygon", "cannot be polygon"));
        }
        cached(&self.geometry_cache().get().valid_polygon, || {
            let polygon = self.polygon()?;
            if geo_utils::is_valid(polygon) { Ok(polygon.clone()) } else { geo_utils::fix_polygon(polygon) }
        })
    }
}

/// A [`WithGeometry`] wrapping a world web mercator geometry (Java's `WithGeometry.fromWorldGeometry`).
#[derive(Debug)]
pub struct FromWorld {
    world_geometry: Geometry<f64>,
    lat_lon_geometry: OnceLock<Geometry<f64>>,
    cache: GeometryCache,
}

/// Wraps a world web mercator geometry.
pub fn from_world_geometry(world_geometry: Geometry<f64>) -> FromWorld {
    FromWorld { world_geometry, lat_lon_geometry: OnceLock::new(), cache: GeometryCache::default() }
}

impl PartialEq for FromWorld {
    fn eq(&self, other: &Self) -> bool {
        self.world_geometry == other.world_geometry
    }
}

impl WithGeometry for FromWorld {
    fn world_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        Ok(&self.world_geometry)
    }

    fn lat_lon_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        Ok(self.lat_lon_geometry.get_or_init(|| geo_utils::world_to_lat_lon_coords(&self.world_geometry)))
    }

    fn geometry_cache(&self) -> &GeometryCache {
        &self.cache
    }
}

impl WithGeometryType for FromWorld {
    fn is_point(&self) -> bool {
        matches!(self.world_geometry, Geometry::Point(_) | Geometry::MultiPoint(_))
    }

    fn can_be_polygon(&self) -> bool {
        matches!(
            self.world_geometry,
            Geometry::Polygon(_) | Geometry::MultiPolygon(_) | Geometry::Rect(_) | Geometry::Triangle(_)
        )
    }

    fn can_be_line(&self) -> bool {
        matches!(self.world_geometry, Geometry::LineString(_) | Geometry::MultiLineString(_) | Geometry::Line(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;

    #[test]
    fn polygon_measurements() {
        let g = from_world_geometry(rectangle(0.0, 0.5).into());
        assert!(g.can_be_polygon());
        assert!(!g.can_be_line());
        assert_eq!(0.25, g.area().unwrap());
        assert_eq!(2.0, g.length().unwrap());
        assert_eq!(0.5, g.size().unwrap());
        assert_eq!(&Geometry::Point(new_point(0.25, 0.25)), g.centroid().unwrap());
        assert_eq!(&Geometry::Point(new_point(0.25, 0.25)), g.centroid_if_convex().unwrap());
        assert_eq!(Geometry::Point(new_point(0.25, 0.25)), g.innermost_point(0.01).unwrap());
        assert!(g.line().is_err());
        assert!(g.area_meters().unwrap() > 0.0);
    }

    #[test]
    fn line_measurements() {
        let g = from_world_geometry(new_line_string(&[0.0, 0.0, 1.0, 0.0]).into());
        assert!(g.can_be_line());
        assert_eq!(0.0, g.area().unwrap());
        assert_eq!(1.0, g.length().unwrap());
        assert_eq!(Geometry::Point(new_point(0.5, 0.0)), g.innermost_point(0.1).unwrap());
        assert_eq!(&Geometry::Point(new_point(0.5, 0.0)), g.line_midpoint().unwrap());
        assert_eq!(Geometry::Point(new_point(0.25, 0.0)), g.point_along_line(0.25).unwrap());
        assert_eq!(Geometry::LineString(new_line_string(&[0.25, 0.0, 0.5, 0.0])), g.partial_line(0.25, 0.5).unwrap());
        assert!(g.polygon().is_err());
    }

    #[test]
    fn multiline_partial_fails() {
        let g = from_world_geometry(
            new_multi_line_string(vec![new_line_string(&[0.0, 0.0, 1.0, 0.0]), new_line_string(&[0.0, 1.0, 3.0, 1.0])])
                .into(),
        );
        assert!(g.partial_line(0.1, 0.2).is_err());
        // longest line is used
        assert_eq!(Geometry::Point(new_point(1.5, 1.0)), g.point_along_line(0.5).unwrap());
    }

    #[test]
    fn concave_uses_point_on_surface() {
        let u: Geometry<f64> =
            new_polygon(&[0.0, 0.0, 3.0, 0.0, 3.0, 3.0, 2.0, 3.0, 2.0, 1.0, 1.0, 1.0, 1.0, 3.0, 0.0, 3.0, 0.0, 0.0])
                .into();
        let g = from_world_geometry(u.clone());
        assert_point_on_surface(&u, g.centroid_if_convex().unwrap());
    }

    #[test]
    fn validated_polygon_fixes() {
        let bowtie: Geometry<f64> = new_polygon(&[0.0, 0.0, 6.0, 3.0, 6.0, 0.0, 0.0, 1.0, 0.0, 0.0]).into();
        let g = from_world_geometry(bowtie);
        assert!(geo_utils::is_valid(g.validated_polygon().unwrap()));
    }

    #[test]
    fn point() {
        let g = from_world_geometry(new_point(0.5, 0.5).into());
        assert!(g.is_point());
        assert_eq!(0.0, g.size().unwrap());
        assert_eq!(Geometry::Point(new_point(0.5, 0.5)), g.innermost_point(0.1).unwrap());
    }
}
