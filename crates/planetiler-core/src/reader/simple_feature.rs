//! Port of `reader/SimpleFeature.java`: an input feature with geometry and tags known at creation-time.

use std::any::Any;
use std::fmt;
use std::sync::{Arc, OnceLock};

use geo_types::{Geometry, Polygon};

use crate::geo::geo_utils;
use crate::geo::geometry_exception::GeometryResult;
use crate::geo::geometry_type::GeometryType;
use crate::geo::with_geometry::{GeometryCache, WithGeometry};
use crate::reader::osm::osm_element::{Info, Node, OsmElement, Way};
use crate::reader::osm::osm_relation_info::{OsmRelationInfo, RelationMember};
use crate::reader::source_feature::{SourceFeature, SourceFeatureBase};
use crate::reader::with_geometry_type::WithGeometryType;
use crate::value::Tags;
use crate::vector_tile::NO_FEATURE_ID;

/// Conversion into an optional shared source/layer name (`&str`, `String`, `Arc<str>`, or `Option` of those).
pub trait IntoName {
    fn into_name(self) -> Option<Arc<str>>;
}

impl IntoName for &str {
    fn into_name(self) -> Option<Arc<str>> {
        Some(Arc::from(self))
    }
}

impl IntoName for String {
    fn into_name(self) -> Option<Arc<str>> {
        Some(Arc::from(self))
    }
}

impl IntoName for Arc<str> {
    fn into_name(self) -> Option<Arc<str>> {
        Some(self)
    }
}

impl IntoName for &Arc<str> {
    fn into_name(self) -> Option<Arc<str>> {
        Some(self.clone())
    }
}

impl<T: IntoName> IntoName for Option<T> {
    fn into_name(self) -> Option<Arc<str>> {
        self.and_then(IntoName::into_name)
    }
}

/// Extra state for fake OSM features created with [`SimpleFeature::create_fake_osm_feature`].
struct OsmExtra {
    area: Option<String>,
    element: OsmElement,
}

/// An input feature read from a data source with geometry and tags known at creation-time.
pub struct SimpleFeature {
    base: SourceFeatureBase,
    /// The geometry type of whichever geometry was provided at construction.
    geometry_type: GeometryType,
    /// True if the geometry was provided in lat/lon coordinates.
    from_lat_lon: bool,
    lat_lon_geometry: OnceLock<Geometry<f64>>,
    world_geometry: OnceLock<Geometry<f64>>,
    osm: Option<OsmExtra>,
}

impl SimpleFeature {
    fn new(
        lat_lon_geometry: Option<Geometry<f64>>,
        world_geometry: Option<Geometry<f64>>,
        base: SourceFeatureBase,
        osm: Option<OsmExtra>,
    ) -> SimpleFeature {
        debug_assert!(
            lat_lon_geometry.is_none() || world_geometry.is_none(),
            "Cannot specify both a world and lat/lon geometry"
        );
        let from_lat_lon = lat_lon_geometry.is_some();
        let geometry_type =
            lat_lon_geometry.as_ref().or(world_geometry.as_ref()).map_or(GeometryType::Unknown, GeometryType::type_of);
        let lat_lon = OnceLock::new();
        if let Some(g) = lat_lon_geometry {
            let _ = lat_lon.set(g);
        }
        let world = OnceLock::new();
        if let Some(g) = world_geometry {
            // we expect outer polygons to appear before inner ones, so process ones with larger areas first
            let _ = world.set(geo_utils::sort_polygons_by_area_descending(g));
        }
        SimpleFeature { base, geometry_type, from_lat_lon, lat_lon_geometry: lat_lon, world_geometry: world, osm }
    }

    /// Returns a new feature from its geometry in latitude/longitude coordinates (converted to world web mercator on
    /// read), tags, and source information.
    pub fn create(
        lat_lon_geometry: impl Into<Geometry<f64>>,
        tags: Tags,
        source: impl IntoName,
        source_layer: impl IntoName,
        id: i64,
    ) -> SimpleFeature {
        Self::new(
            Some(lat_lon_geometry.into()),
            None,
            SourceFeatureBase::new(tags, source.into_name(), source_layer.into_name(), Vec::new(), id),
            None,
        )
    }

    /// Returns a new feature from a world web mercator geometry, tags, and source information.
    pub fn create_world(
        world_geometry: impl Into<Geometry<f64>>,
        tags: Tags,
        source: impl IntoName,
        source_layer: impl IntoName,
        id: i64,
    ) -> SimpleFeature {
        Self::new(
            None,
            Some(world_geometry.into()),
            SourceFeatureBase::new(tags, source.into_name(), source_layer.into_name(), Vec::new(), id),
            None,
        )
    }

    /// Returns a new feature with no tags and a geometry specified in latitude/longitude coordinates.
    pub fn from_lat_lon_geometry_with_id(lat_lon_geometry: impl Into<Geometry<f64>>, id: i64) -> SimpleFeature {
        Self::create_tags_with_id(lat_lon_geometry, Tags::default(), id)
    }

    /// Alias for [`SimpleFeature::from_lat_lon_geometry_with_id`] with no ID set.
    pub fn from_lat_lon_geometry(lat_lon_geometry: impl Into<Geometry<f64>>) -> SimpleFeature {
        Self::from_lat_lon_geometry_with_id(lat_lon_geometry, NO_FEATURE_ID)
    }

    /// Returns a new feature with no tags and a geometry specified in world web mercator coordinates where `(0,0)` is
    /// the northwest and `(1,1)` is the southeast corner of the planet.
    pub fn from_world_geometry_with_id(world_geometry: impl Into<Geometry<f64>>, id: i64) -> SimpleFeature {
        Self::new(
            None,
            Some(world_geometry.into()),
            SourceFeatureBase::new(Tags::default(), None, None, Vec::new(), id),
            None,
        )
    }

    /// Alias for [`SimpleFeature::from_world_geometry_with_id`] with no ID set.
    pub fn from_world_geometry(world_geometry: impl Into<Geometry<f64>>) -> SimpleFeature {
        Self::from_world_geometry_with_id(world_geometry, NO_FEATURE_ID)
    }

    /// Returns a new feature without source information if you need a `SimpleFeature` but don't plan on passing it
    /// to a profile.
    pub fn create_tags_with_id(lat_lon_geometry: impl Into<Geometry<f64>>, tags: Tags, id: i64) -> SimpleFeature {
        Self::new(Some(lat_lon_geometry.into()), None, SourceFeatureBase::new(tags, None, None, Vec::new(), id), None)
    }

    /// Alias for [`SimpleFeature::create_tags_with_id`] with no ID set.
    pub fn create_tags(lat_lon_geometry: impl Into<Geometry<f64>>, tags: Tags) -> SimpleFeature {
        Self::create_tags_with_id(lat_lon_geometry, tags, NO_FEATURE_ID)
    }

    /// Returns a new feature with OSM relation info and optional metadata that behaves like an OSM element (closed
    /// rings can be lines or polygons depending on the `area` tag). Useful for setting up inputs for OSM unit tests.
    pub fn create_fake_osm_feature(
        lat_lon_geometry: impl Into<Geometry<f64>>,
        tags: Tags,
        source: impl IntoName,
        source_layer: impl IntoName,
        id: i64,
        relations: Vec<RelationMember<dyn OsmRelationInfo>>,
        info: Option<Info>,
    ) -> SimpleFeature {
        let geom = lat_lon_geometry.into();
        let area = tags.get("area").map(|v| v.to_string_lossy().into_owned());
        let element = if GeometryType::type_of(&geom) == GeometryType::Point {
            let (lon, lat) = match &geom {
                Geometry::Point(p) => (p.x(), p.y()),
                _ => (0.0, 0.0),
            };
            let mut node = Node::with_tags(id, tags.clone(), lat, lon);
            node.info = info;
            OsmElement::Node(node)
        } else {
            let mut way = Way::with_tags(id, tags.clone(), Vec::new());
            way.info = info;
            OsmElement::Way(way)
        };
        Self::new(
            Some(geom),
            None,
            SourceFeatureBase::new(tags, source.into_name(), source_layer.into_name(), relations, id),
            Some(OsmExtra { area, element }),
        )
    }

    /// Creates a feature from a fully-built [`SourceFeatureBase`] and a lat/lon geometry, for readers that build the
    /// base themselves.
    pub fn from_base_lat_lon(lat_lon_geometry: Geometry<f64>, base: SourceFeatureBase) -> SimpleFeature {
        Self::new(Some(lat_lon_geometry), None, base, None)
    }

    /// Creates a feature from a fully-built [`SourceFeatureBase`] and a world geometry.
    pub fn from_base_world(world_geometry: Geometry<f64>, base: SourceFeatureBase) -> SimpleFeature {
        Self::new(None, Some(world_geometry), base, None)
    }

    fn provided_geometry(&self) -> Option<&Geometry<f64>> {
        if self.from_lat_lon { self.lat_lon_geometry.get() } else { self.world_geometry.get() }
    }

    fn lat_lon(&self) -> &Geometry<f64> {
        self.lat_lon_geometry
            .get_or_init(|| geo_utils::world_to_lat_lon_coords(self.world_geometry.get().expect("world geometry")))
    }

    fn world(&self) -> &Geometry<f64> {
        self.world_geometry.get_or_init(|| {
            // we expect outer polygons to appear before inner ones, so process ones with larger areas first
            geo_utils::sort_polygons_by_area_descending(geo_utils::lat_lon_to_world_coords(
                self.lat_lon_geometry.get().expect("lat/lon geometry"),
            ))
        })
    }
}

impl WithGeometryType for SimpleFeature {
    fn is_point(&self) -> bool {
        self.geometry_type == GeometryType::Point
    }

    fn can_be_polygon(&self) -> bool {
        match &self.osm {
            None => self.geometry_type == GeometryType::Polygon,
            Some(osm) => match self.lat_lon() {
                Geometry::Polygon(_) | Geometry::MultiPolygon(_) => true,
                Geometry::LineString(line) => {
                    line.is_closed() && osm.area.as_deref() != Some("no") && line.0.len() >= 4
                }
                _ => false,
            },
        }
    }

    fn can_be_line(&self) -> bool {
        match &self.osm {
            None => self.geometry_type == GeometryType::Line,
            Some(osm) => match self.lat_lon() {
                Geometry::MultiLineString(_) => true,
                Geometry::LineString(line) => {
                    (!line.is_closed() || osm.area.as_deref() != Some("yes")) && line.0.len() >= 2
                }
                _ => false,
            },
        }
    }
}

impl WithGeometry for SimpleFeature {
    fn world_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        Ok(self.world())
    }

    fn lat_lon_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        Ok(self.lat_lon())
    }

    fn geometry_cache(&self) -> &GeometryCache {
        &self.base.geometry_cache
    }

    fn compute_polygon(&self) -> GeometryResult<Option<Geometry<f64>>> {
        match (&self.osm, self.world()) {
            (Some(_), Geometry::LineString(line)) => Ok(Some(Geometry::Polygon(Polygon::new(line.clone(), vec![])))),
            _ => Ok(None),
        }
    }
}

impl SourceFeature for SimpleFeature {
    fn base(&self) -> &SourceFeatureBase {
        &self.base
    }

    fn original_element(&self) -> Option<&OsmElement> {
        self.osm.as_ref().map(|o| &o.element)
    }

    fn as_any_feature(&self) -> &dyn Any {
        self
    }
}

crate::impl_source_feature_traits!(SimpleFeature);

impl PartialEq for SimpleFeature {
    fn eq(&self, other: &Self) -> bool {
        self.from_lat_lon == other.from_lat_lon
            && self.provided_geometry() == other.provided_geometry()
            && self.base.tags == other.base.tags
            && self.osm.as_ref().map(|o| (&o.area, o.element.info()))
                == other.osm.as_ref().map(|o| (&o.area, o.element.info()))
    }
}

impl fmt::Debug for SimpleFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Feature[source={}, source layer={}, id={}, geometry type={:?}, tags={:?}]",
            self.base.source.as_deref().unwrap_or("null"),
            self.base.source_layer.as_deref().unwrap_or("null"),
            self.base.id,
            self.geometry_type,
            self.base.tags
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;
    use crate::reader::with_tags::WithTags;
    use crate::tags;
    use geo_types::GeometryCollection;

    fn gc(geoms: Vec<Geometry<f64>>) -> Geometry<f64> {
        Geometry::GeometryCollection(GeometryCollection(geoms))
    }

    #[test]
    fn test_fixes_multipolygon_ordering() {
        let outer1 = world_rectangle(0.1, 0.9);
        let inner1 = world_rectangle(0.2, 0.8);
        let outer2 = world_rectangle(0.3, 0.7);
        let inner2 = world_rectangle(0.4, 0.6);
        let feature = SimpleFeature::from_lat_lon_geometry(new_multi_polygon(vec![
            new_polygon_with_holes(outer2, vec![inner2]),
            new_polygon_with_holes(outer1, vec![inner1]),
        ]));
        let Geometry::MultiPolygon(mp) = feature.world_geometry().unwrap() else { panic!() };
        assert_eq!(2, mp.0.len());
        assert_same_normalized_feature(
            &round(
                &new_polygon_with_holes(rectangle_coord_list(0.1, 0.9), vec![rectangle_coord_list(0.2, 0.8)]).into(),
            ),
            &round(&mp.0[0].clone().into()),
        );
        assert_same_normalized_feature(
            &round(
                &new_polygon_with_holes(rectangle_coord_list(0.3, 0.7), vec![rectangle_coord_list(0.4, 0.6)]).into(),
            ),
            &round(&mp.0[1].clone().into()),
        );
    }

    #[test]
    fn test_from_world_geom() {
        let world: Geometry<f64> = new_line_string(&[0.5, 0.5, 0.6, 0.7]).into();
        let feature = SimpleFeature::from_world_geometry(world.clone());
        assert_same_normalized_feature(&world, feature.world_geometry().unwrap());
        assert_same_normalized_feature(
            &world,
            &round(&geo_utils::lat_lon_to_world_coords(feature.lat_lon_geometry().unwrap())),
        );
    }

    fn check_types(geom: Geometry<f64>, line: bool, polygon: bool, point: bool) {
        let world = SimpleFeature::from_world_geometry(geom.clone());
        let lat_lon = SimpleFeature::from_lat_lon_geometry(geom);
        for f in [&world, &lat_lon] {
            assert_eq!(line, f.can_be_line());
            assert_eq!(polygon, f.can_be_polygon());
            assert_eq!(point, f.is_point());
        }
    }

    #[test]
    fn test_is_line() {
        let line: Geometry<f64> = new_line_string(&[0.0, 0.0, 1.0, 1.0]).into();
        check_types(line.clone(), true, false, false);
        check_types(gc(vec![line]), true, false, false);
    }

    #[test]
    fn test_is_polygon() {
        let polygon: Geometry<f64> = new_polygon(&[0.0, 0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0]).into();
        check_types(polygon.clone(), false, true, false);
        check_types(gc(vec![polygon]), false, true, false);
    }

    #[test]
    fn test_is_point() {
        let point: Geometry<f64> = new_point(0.0, 0.0).into();
        check_types(point.clone(), false, false, true);
        check_types(gc(vec![point]), false, false, true);
    }

    #[test]
    fn test_mixed_collections() {
        let point: Geometry<f64> = new_point(0.0, 0.0).into();
        let line: Geometry<f64> = new_line_string(&[0.0, 0.0, 1.0, 1.0]).into();
        let polygon: Geometry<f64> = new_polygon(&[0.0, 0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0]).into();
        for g in [
            gc(vec![point.clone(), line.clone()]),
            gc(vec![line.clone(), polygon.clone()]),
            gc(vec![point.clone(), line.clone(), polygon.clone()]),
            gc(vec![gc(vec![point]), gc(vec![line]), gc(vec![polygon])]),
        ] {
            check_types(g, false, false, false);
        }
    }

    #[test]
    fn test_collections_centroid() {
        use crate::geo::interior_point::centroid;
        let point1: Geometry<f64> = new_point(0.0, 0.0).into();
        let point2: Geometry<f64> = new_point(1.0, 1.0).into();
        let line1: Geometry<f64> = new_line_string(&[0.0, 0.0, 1.0, 1.0]).into();
        let line2: Geometry<f64> = new_line_string(&[1.0, 1.0, 0.0, 0.0]).into();
        let polygon1: Geometry<f64> = new_polygon(&[0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0]).into();
        let polygon2: Geometry<f64> = new_polygon(&[0.25, 0.25, 0.25, 0.75, 0.75, 0.75, 0.75, 0.25, 0.25, 0.25]).into();
        let points1 = gc(vec![point1.clone()]);
        let points2 = gc(vec![point1, point2]);
        let lines1 = gc(vec![line1.clone()]);
        let lines2 = gc(vec![line1, line2]);
        let polys1 = gc(vec![polygon1.clone()]);
        let polys2 = gc(vec![polygon1, polygon2]);
        let cases = [
            (points1.clone(), 0.0, 0.0),
            (points2.clone(), 0.5, 0.5),
            (gc(vec![points1, points2]), 1.0 / 3.0, 1.0 / 3.0),
            (lines1.clone(), 0.5, 0.5),
            (lines2.clone(), 0.5, 0.5),
            (gc(vec![lines1, lines2]), 0.5, 0.5),
            (polys1.clone(), 0.5, 0.5),
            (polys2.clone(), 0.5, 0.5),
            (gc(vec![polys1, polys2]), 0.5, 0.5),
        ];
        for (g, x, y) in cases {
            let c = centroid(&g).unwrap();
            assert!((c.x - x).abs() < 1e-5 && (c.y - y).abs() < 1e-5, "{g:?} {c:?}");
        }
    }

    #[test]
    fn test_fake_osm_feature() {
        let closed: Geometry<f64> = new_line_string(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0]).into();
        let both =
            SimpleFeature::create_fake_osm_feature(closed.clone(), tags! {}, "osm", None::<&str>, 1, vec![], None);
        assert!(both.can_be_line() && both.can_be_polygon());
        assert!(matches!(both.polygon().unwrap(), Geometry::Polygon(_)));
        let area = SimpleFeature::create_fake_osm_feature(
            closed.clone(),
            tags! {"area" => "yes"},
            "osm",
            None::<&str>,
            1,
            vec![],
            None,
        );
        assert!(!area.can_be_line() && area.can_be_polygon());
        let not_area = SimpleFeature::create_fake_osm_feature(
            closed,
            tags! {"area" => "no"},
            "osm",
            None::<&str>,
            1,
            vec![],
            None,
        );
        assert!(not_area.can_be_line() && !not_area.can_be_polygon());
        assert_eq!(Some("osm"), crate::expression::expression::ExpressionInput::source(&not_area));
        assert!(not_area.has_tag_str("area", "no"));
        assert!(matches!(not_area.original_element(), Some(OsmElement::Way(_))));
    }
}
