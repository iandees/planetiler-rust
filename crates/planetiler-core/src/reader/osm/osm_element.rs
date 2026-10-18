//! Port of `reader/osm/OsmElement.java`: an input element read from OpenStreetMap data.
//!
//! See <https://wiki.openstreetmap.org/wiki/Elements>.

use std::sync::Arc;

use crate::geo::geo_utils;
use crate::reader::with_tags::WithTags;
use crate::value::Tags;

/// The type of an OSM element.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum OsmElementType {
    #[default]
    Other,
    Node,
    Way,
    Relation,
}

/// Metadata about an OSM element version.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Info {
    pub changeset: i64,
    /// Raw timestamp from the PBF file (seconds since epoch with the default date granularity).
    pub timestamp: i64,
    pub user_id: i32,
    pub version: i32,
    pub user: Arc<str>,
}

impl Info {
    const COST: usize = 2;

    pub fn new(changeset: i64, timestamp: i64, user_id: i32, version: i32, user: &str) -> Info {
        Info { changeset, timestamp, user_id, version, user: Arc::from(user) }
    }
}

fn info_cost(info: &Option<Info>) -> usize {
    if info.is_some() { Info::COST } else { 0 }
}

/// An un-handled element read from the .osm.pbf file (i.e. file header).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Other {
    pub id: i64,
    pub tags: Tags,
    pub info: Option<Info>,
}

/// A point on the earth's surface.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Node {
    pub id: i64,
    pub tags: Tags,
    pub lat: f64,
    pub lon: f64,
    pub info: Option<Info>,
}

impl Node {
    pub fn new(id: i64, lat: f64, lon: f64) -> Node {
        Node { id, tags: Tags::default(), lat, lon, info: None }
    }

    pub fn with_tags(id: i64, tags: Tags, lat: f64, lon: f64) -> Node {
        Node { id, tags, lat, lon, info: None }
    }

    /// Location packed into a single long, see [`geo_utils::encode_flat_location`].
    pub fn encoded_location(&self) -> u64 {
        geo_utils::encode_flat_location(self.lon, self.lat)
    }
}

/// An ordered list of 2-2,000 nodes that define a polyline.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Way {
    pub id: i64,
    pub tags: Tags,
    pub nodes: Vec<i64>,
    pub info: Option<Info>,
}

impl Way {
    pub fn new(id: i64) -> Way {
        Way { id, tags: Tags::default(), nodes: Vec::with_capacity(5), info: None }
    }

    pub fn with_tags(id: i64, tags: Tags, nodes: Vec<i64>) -> Way {
        Way { id, tags, nodes, info: None }
    }

    /// Returns a copy of this way from node `from_index` (inclusive) to `to_index` (exclusive).
    pub fn split(&self, from_index: usize, to_index: usize) -> Way {
        Way {
            id: self.id,
            tags: self.tags.clone(),
            nodes: self.nodes[from_index..to_index].to_vec(),
            info: self.info.clone(),
        }
    }

    /// Returns a copy of this way with new tags.
    pub fn with_new_tags(&self, tags: Tags) -> Way {
        Way { id: self.id, tags, nodes: self.nodes.clone(), info: self.info.clone() }
    }

    /// Returns true if the first node is the same as the last node.
    pub fn is_closed(&self) -> bool {
        !self.nodes.is_empty() && self.nodes[0] == self.nodes[self.nodes.len() - 1]
    }
}

/// A node, way, or relation contained in a relation with an optional "role" to clarify the purpose of each member.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Member {
    pub member_type: OsmElementType,
    pub ref_id: i64,
    pub role: Arc<str>,
}

impl Member {
    pub fn new(member_type: OsmElementType, ref_id: i64, role: &str) -> Member {
        Member { member_type, ref_id, role: Arc::from(role) }
    }
}

/// An ordered list of nodes, ways, and other relations.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Relation {
    pub id: i64,
    pub tags: Tags,
    pub members: Vec<Member>,
    pub info: Option<Info>,
}

impl Relation {
    pub fn new(id: i64) -> Relation {
        Relation { id, ..Default::default() }
    }

    pub fn with_tags(id: i64, tags: Tags, members: Vec<Member>) -> Relation {
        Relation { id, tags, members, info: None }
    }
}

/// An input element read from OpenStreetMap data.
#[derive(Clone, Debug, PartialEq)]
pub enum OsmElement {
    Other(Other),
    Node(Node),
    Way(Way),
    Relation(Relation),
}

impl OsmElement {
    /// OSM element ID.
    pub fn id(&self) -> i64 {
        match self {
            OsmElement::Other(e) => e.id,
            OsmElement::Node(e) => e.id,
            OsmElement::Way(e) => e.id,
            OsmElement::Relation(e) => e.id,
        }
    }

    pub fn info(&self) -> Option<&Info> {
        match self {
            OsmElement::Other(e) => e.info.as_ref(),
            OsmElement::Node(e) => e.info.as_ref(),
            OsmElement::Way(e) => e.info.as_ref(),
            OsmElement::Relation(e) => e.info.as_ref(),
        }
    }

    pub fn element_type(&self) -> OsmElementType {
        match self {
            OsmElement::Other(_) => OsmElementType::Other,
            OsmElement::Node(_) => OsmElementType::Node,
            OsmElement::Way(_) => OsmElementType::Way,
            OsmElement::Relation(_) => OsmElementType::Relation,
        }
    }

    /// Approximate relative cost of processing this element (used for weighting work between threads).
    pub fn cost(&self) -> usize {
        match self {
            OsmElement::Other(e) => 1 + e.tags.len() + info_cost(&e.info),
            OsmElement::Node(e) => 1 + e.tags.len() + info_cost(&e.info),
            OsmElement::Way(e) => 1 + e.tags.len() + e.nodes.len() + info_cost(&e.info),
            OsmElement::Relation(e) => 1 + e.tags.len() + e.members.len() * 3 + info_cost(&e.info),
        }
    }

    pub fn tags_mut(&mut self) -> &mut Tags {
        match self {
            OsmElement::Other(e) => &mut e.tags,
            OsmElement::Node(e) => &mut e.tags,
            OsmElement::Way(e) => &mut e.tags,
            OsmElement::Relation(e) => &mut e.tags,
        }
    }
}

/// Vector tile feature ID for an OSM element: `id * multiplier + type offset`.
pub fn vector_tile_feature_id(multiplier: i32, id: i64, element_type: OsmElementType) -> i64 {
    id.wrapping_mul(multiplier as i64).wrapping_add(match element_type {
        OsmElementType::Other => 0,
        OsmElementType::Node => 1,
        OsmElementType::Way => 2,
        OsmElementType::Relation => 3,
    })
}

macro_rules! impl_with_tags {
    ($($t:ty),*) => {
        $(impl WithTags for $t {
            fn tags(&self) -> &Tags {
                &self.tags
            }
        })*
    };
}

impl_with_tags!(Other, Node, Way, Relation);

impl WithTags for OsmElement {
    fn tags(&self) -> &Tags {
        match self {
            OsmElement::Other(e) => &e.tags,
            OsmElement::Node(e) => &e.tags,
            OsmElement::Way(e) => &e.tags,
            OsmElement::Relation(e) => &e.tags,
        }
    }
}

impl From<Node> for OsmElement {
    fn from(value: Node) -> Self {
        OsmElement::Node(value)
    }
}

impl From<Way> for OsmElement {
    fn from(value: Way) -> Self {
        OsmElement::Way(value)
    }
}

impl From<Relation> for OsmElement {
    fn from(value: Relation) -> Self {
        OsmElement::Relation(value)
    }
}

impl From<Other> for OsmElement {
    fn from(value: Other) -> Self {
        OsmElement::Other(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags;

    #[test]
    fn basics() {
        let way = Way::with_tags(1, tags! {"a" => "b"}, vec![1, 2, 3, 1]);
        assert!(way.is_closed());
        assert!(!way.split(0, 2).is_closed());
        assert_eq!(vec![2, 3], way.split(1, 3).nodes);
        let e: OsmElement = way.into();
        assert_eq!(OsmElementType::Way, e.element_type());
        assert_eq!(1 + 1 + 4, e.cost());
        assert!(e.has_tag_str("a", "b"));
        assert_eq!(12, vector_tile_feature_id(10, 1, OsmElementType::Way));
    }
}
