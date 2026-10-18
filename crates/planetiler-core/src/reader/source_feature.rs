//! Port of `reader/SourceFeature.java`: base type for input features read from a data source.
//!
//! # Design
//!
//! Java's abstract `SourceFeature` class becomes the [`SourceFeature`] trait plus a [`SourceFeatureBase`] struct that
//! holds the common fields (tags, source, source layer, relation memberships, id and the [`GeometryCache`] used by the
//! memoized [`WithGeometry`] methods). Each reader defines its own feature type that owns a `SourceFeatureBase` and
//! implements:
//!
//! - [`WithGeometryType`] (`is_point`, `can_be_polygon`, `can_be_line`),
//! - [`WithGeometry`] (`world_geometry`, `lat_lon_geometry` - computed lazily, e.g. from OSM node locations - and
//!   `geometry_cache`, optionally `compute_line` / `compute_polygon`),
//! - [`SourceFeature`] (`base`, and optionally `vector_tile_feature_id`, `original_element`, `is_split_way`, ...),
//!
//! and calls [`impl_source_feature_traits!`](crate::impl_source_feature_traits) to derive [`WithTags`] and
//! [`ExpressionInput`] from the base. Profiles receive `&dyn SourceFeature`; generic helpers that can't live on a
//! trait object (like [`SourceFeatureExt::relation_info`]) are on the [`SourceFeatureExt`] extension trait which is
//! implemented for every source feature including `dyn SourceFeature`.
//!
//! [`SimpleFeature`](crate::reader::simple_feature::SimpleFeature) is the implementation used when the geometry is
//! known up front (shapefile, geopackage, natural earth, geojson, parquet readers, and tests).

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crate::expression::expression::ExpressionInput;
use crate::geo::with_geometry::{GeometryCache, WithGeometry};
use crate::reader::osm::osm_element::OsmElement;
use crate::reader::osm::osm_relation_info::{OsmRelationInfo, RelationMember};
use crate::value::Tags;

/// Common fields of every [`SourceFeature`].
#[derive(Default)]
pub struct SourceFeatureBase {
    /// Key/value pairs associated with this element.
    pub tags: Tags,
    /// Source name that profiles use to distinguish between elements from different data sources.
    pub source: Option<Arc<str>>,
    /// Layer name within `source` that profiles use to distinguish between different kinds of elements in a source.
    pub source_layer: Option<Arc<str>>,
    /// Relations that this element is contained within.
    pub relation_infos: Vec<RelationMember<dyn OsmRelationInfo>>,
    /// Numeric ID of this feature within this source (i.e. an OSM element ID).
    pub id: i64,
    /// Memoized derived geometries for the [`WithGeometry`] provided methods.
    pub geometry_cache: GeometryCache,
}

impl SourceFeatureBase {
    pub fn new(
        tags: Tags,
        source: Option<Arc<str>>,
        source_layer: Option<Arc<str>>,
        relation_infos: Vec<RelationMember<dyn OsmRelationInfo>>,
        id: i64,
    ) -> SourceFeatureBase {
        SourceFeatureBase { tags, source, source_layer, relation_infos, id, geometry_cache: GeometryCache::new() }
    }
}

impl fmt::Debug for SourceFeatureBase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Feature[source={}, source layer={}, id={}, tags={:?}]",
            self.source.as_deref().unwrap_or("null"),
            self.source_layer.as_deref().unwrap_or("null"),
            self.id,
            self.tags
        )
    }
}

/// An input feature read from a data source. See the [module docs](self) for how to implement it.
///
/// All geometries except [`WithGeometry::lat_lon_geometry`] are in world web mercator coordinates where `(0,0)` is
/// the northwest corner and `(1,1)` is the southeast corner of the planet.
///
/// The source name and source layer are available through [`ExpressionInput::source`] and
/// [`ExpressionInput::source_layer`].
pub trait SourceFeature: WithGeometry + ExpressionInput + fmt::Debug {
    /// The common fields of this feature.
    fn base(&self) -> &SourceFeatureBase;

    /// Returns the ID for this element from the input data source (i.e. OSM element ID).
    fn id(&self) -> i64 {
        self.base().id
    }

    /// The ID to use for output vector tile features. By default, the feature id is taken as-is from the input data
    /// source id times `multiplier` (wrapping on overflow like Java's `long` arithmetic, since some sources such as
    /// parquet use 64-bit hashes as IDs).
    fn vector_tile_feature_id(&self, multiplier: i64, _renumber: bool) -> i64 {
        multiplier.wrapping_mul(self.id())
    }

    /// Returns all OSM relations this element belongs to (including super-relations), type-erased.
    fn relation_infos(&self) -> &[RelationMember<dyn OsmRelationInfo>] {
        &self.base().relation_infos
    }

    /// Returns true if this element has any OSM relation info.
    fn has_relation_info(&self) -> bool {
        !self.base().relation_infos.is_empty()
    }

    /// For OSM features, the original OSM element this came from (Java `OsmSourceFeature.originalElement()`).
    fn original_element(&self) -> Option<&OsmElement> {
        None
    }

    /// True if this is the full version of an OSM way that also got split at intersections (Java `FullWay`).
    fn is_full_way(&self) -> bool {
        false
    }

    /// True if this is one segment of an OSM way that got split at intersections (Java `SplitWay`).
    fn is_split_way(&self) -> bool {
        false
    }

    /// Upcast to `&dyn Any` so profiles can downcast to a concrete reader feature type.
    fn as_any_feature(&self) -> &dyn Any;
}

/// Generic helpers for any [`SourceFeature`] (including `dyn SourceFeature`).
pub trait SourceFeatureExt {
    /// Returns the OSM relations with info type `T` that this element belongs to directly, along with the role that
    /// this element is tagged with in that relation.
    fn relation_info<T: OsmRelationInfo>(&self) -> Vec<RelationMember<T>> {
        self.relation_info_with_super(false)
    }

    /// Returns the OSM relations with info type `T` that this element belongs to, including super-relations when
    /// `include_super_relations` is true.
    fn relation_info_with_super<T: OsmRelationInfo>(&self, include_super_relations: bool) -> Vec<RelationMember<T>>;
}

impl<S: SourceFeature + ?Sized> SourceFeatureExt for S {
    fn relation_info_with_super<T: OsmRelationInfo>(&self, include_super_relations: bool) -> Vec<RelationMember<T>> {
        self.relation_infos()
            .iter()
            .filter(|info| include_super_relations || !info.is_super_relation())
            .filter_map(RelationMember::downcast::<T>)
            .collect()
    }
}

/// Implements [`WithTags`] and [`ExpressionInput`] for a [`SourceFeature`] implementation from its
/// [`SourceFeature::base`].
#[macro_export]
macro_rules! impl_source_feature_traits {
    ($t:ty) => {
        impl $crate::reader::with_tags::WithTags for $t {
            fn tags(&self) -> &$crate::value::Tags {
                &$crate::reader::source_feature::SourceFeature::base(self).tags
            }
        }

        impl $crate::expression::expression::ExpressionInput for $t {
            fn as_with_geometry_type(&self) -> Option<&dyn $crate::reader::with_geometry_type::WithGeometryType> {
                Some(self)
            }

            fn source(&self) -> Option<&str> {
                $crate::reader::source_feature::SourceFeature::base(self).source.as_deref()
            }

            fn source_layer(&self) -> Option<&str> {
                $crate::reader::source_feature::SourceFeature::base(self).source_layer.as_deref()
            }

            fn as_any(&self) -> Option<&dyn std::any::Any> {
                Some(self)
            }
        }
    };
}

/// Helper for writing `Display` of source features like Java's `toString`.
pub fn describe(feature: &dyn SourceFeature) -> String {
    format!("{:?}", feature.base())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::new_point;
    use crate::reader::simple_feature::SimpleFeature;
    use crate::tags;

    #[derive(Debug, PartialEq)]
    struct Route(i64);
    impl OsmRelationInfo for Route {
        fn id(&self) -> i64 {
            self.0
        }
    }

    #[derive(Debug)]
    struct Other(i64);
    impl OsmRelationInfo for Other {
        fn id(&self) -> i64 {
            self.0
        }
    }

    #[test]
    fn relation_info() {
        let feature = SimpleFeature::create_fake_osm_feature(
            new_point(0.0, 0.0),
            tags! {"a" => "b"},
            "osm",
            "layer",
            5,
            vec![
                RelationMember::new("r1", Arc::new(Route(1))).erase(),
                RelationMember::new("r2", Arc::new(Other(2))).erase(),
                RelationMember::with_path("r3", Arc::new(Route(3)), vec![4]).erase(),
            ],
            None,
        );
        let dyn_feature: &dyn SourceFeature = &feature;
        assert!(dyn_feature.has_relation_info());
        let routes = dyn_feature.relation_info::<Route>();
        assert_eq!(1, routes.len());
        assert_eq!("r1", routes[0].role());
        assert_eq!(&Route(1), routes[0].relation());
        let with_super = dyn_feature.relation_info_with_super::<Route>(true);
        assert_eq!(vec![1, 3], with_super.iter().map(|r| r.relation().id()).collect::<Vec<_>>());
        assert!(with_super[1].is_super_relation());
        assert_eq!(1, dyn_feature.relation_info::<Other>().len());
        assert_eq!(Some("osm"), dyn_feature.source());
        assert_eq!(Some("layer"), dyn_feature.source_layer());
        assert_eq!(5, dyn_feature.id());
        assert_eq!(50, dyn_feature.vector_tile_feature_id(10, false));
        assert!(dyn_feature.as_any_feature().downcast_ref::<SimpleFeature>().is_some());
        assert!(dyn_feature.as_with_geometry_type().unwrap().is_point());

        let plain = SimpleFeature::from_world_geometry(new_point(0.5, 0.5));
        let plain: &dyn SourceFeature = &plain;
        assert!(!plain.has_relation_info());
        assert!(plain.relation_info::<Route>().is_empty());
        assert!(plain.original_element().is_none());
        assert_eq!(None, plain.source());
    }
}
