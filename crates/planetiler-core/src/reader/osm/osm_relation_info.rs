//! Port of `reader/osm/OsmRelationInfo.java` and `OsmReader.RelationMember`.
//!
//! Profiles define their own types implementing [`OsmRelationInfo`] (returned from
//! [`Profile::preprocess_osm_relation`](crate::profile::Profile::preprocess_osm_relation)) and later get them back,
//! downcast to the concrete type, through
//! [`SourceFeatureExt::relation_info`](crate::reader::source_feature::SourceFeatureExt::relation_info).

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crate::reader::osm::osm_element::{self, OsmElementType};

/// A user-defined type containing information about a relation that will be relevant during subsequent way
/// processing. This is stored in-memory by the OSM reader so keep it as compact as possible.
pub trait OsmRelationInfo: Any + Send + Sync + fmt::Debug {
    /// Returns the OSM ID of this relation.
    fn id(&self) -> i64;

    /// Estimated number of bytes this uses in memory.
    fn estimate_memory_usage_bytes(&self) -> u64 {
        0
    }

    /// Vector tile feature ID for this relation.
    fn vector_tile_feature_id(&self, multiplier: i32) -> i64 {
        osm_element::vector_tile_feature_id(multiplier, self.id(), OsmElementType::Relation)
    }
}

impl dyn OsmRelationInfo {
    /// Returns this relation info as `T` if that is its concrete type.
    pub fn downcast_ref<T: OsmRelationInfo>(&self) -> Option<&T> {
        (self as &dyn Any).downcast_ref::<T>()
    }

    /// Returns true if the concrete type of this relation info is `T`.
    pub fn is<T: OsmRelationInfo>(&self) -> bool {
        (self as &dyn Any).is::<T>()
    }
}

/// A relation (with info type `T`) that an element belongs to, along with the role of the element in that relation
/// (Java `OsmReader.RelationMember`).
///
/// `parent_relation_path` contains the IDs of intermediate relations when the element is contained in `relation`
/// through a super-relation.
pub struct RelationMember<T: ?Sized> {
    pub role: Arc<str>,
    pub relation: Arc<T>,
    pub parent_relation_path: Vec<i64>,
}

impl<T: ?Sized> RelationMember<T> {
    /// A direct membership of an element in `relation` with `role`.
    pub fn new(role: impl Into<Arc<str>>, relation: Arc<T>) -> RelationMember<T> {
        RelationMember { role: role.into(), relation, parent_relation_path: Vec::new() }
    }

    /// A membership through super-relations listed in `parent_relation_path`.
    pub fn with_path(role: impl Into<Arc<str>>, relation: Arc<T>, parent_relation_path: Vec<i64>) -> RelationMember<T> {
        RelationMember { role: role.into(), relation, parent_relation_path }
    }

    /// The role this element has in the relation.
    pub fn role(&self) -> &str {
        &self.role
    }

    /// The relation info.
    pub fn relation(&self) -> &T {
        &self.relation
    }

    /// Returns `true` if this is from a super-relation that contains a relation this element belongs to.
    pub fn is_super_relation(&self) -> bool {
        !self.parent_relation_path.is_empty()
    }
}

impl<T: OsmRelationInfo> RelationMember<T> {
    /// Converts to a type-erased member that can be stored on a source feature.
    pub fn erase(self) -> RelationMember<dyn OsmRelationInfo> {
        RelationMember { role: self.role, relation: self.relation, parent_relation_path: self.parent_relation_path }
    }
}

impl RelationMember<dyn OsmRelationInfo> {
    /// Returns this member typed as `T` if the relation info has that concrete type.
    pub fn downcast<T: OsmRelationInfo>(&self) -> Option<RelationMember<T>> {
        let any: Arc<dyn Any + Send + Sync> = self.relation.clone();
        any.downcast::<T>().ok().map(|relation| RelationMember {
            role: self.role.clone(),
            relation,
            parent_relation_path: self.parent_relation_path.clone(),
        })
    }
}

impl<T: ?Sized> Clone for RelationMember<T> {
    fn clone(&self) -> Self {
        RelationMember {
            role: self.role.clone(),
            relation: self.relation.clone(),
            parent_relation_path: self.parent_relation_path.clone(),
        }
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RelationMember<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelationMember")
            .field("role", &self.role)
            .field("relation", &self.relation)
            .field("parent_relation_path", &self.parent_relation_path)
            .finish()
    }
}

impl<T: ?Sized + PartialEq> PartialEq for RelationMember<T> {
    fn eq(&self, other: &Self) -> bool {
        self.role == other.role
            && *self.relation == *other.relation
            && self.parent_relation_path == other.parent_relation_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn downcast() {
        let member = RelationMember::new("role", Arc::new(Route(1))).erase();
        assert!(member.relation.is::<Route>());
        assert!(!member.relation.is::<Other>());
        assert_eq!(Some(&Route(1)), member.relation.downcast_ref::<Route>());
        let typed = member.downcast::<Route>().unwrap();
        assert_eq!("role", typed.role());
        assert_eq!(1, typed.relation().id());
        assert!(member.downcast::<Other>().is_none());
        assert_eq!(13, Route(1).vector_tile_feature_id(10));
    }
}
