/*
Copyright (c) 2021, MapTiler.com & OpenMapTiles contributors.
All rights reserved.

Code license: BSD 3-Clause License

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

* Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.

* Redistributions in binary form must reproduce the above copyright notice,
  this list of conditions and the following disclaimer in the documentation
  and/or other materials provided with the distribution.

* Neither the name of the copyright holder nor the names of its
  contributors may be used to endorse or promote products derived from
  this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Design license: CC-BY 4.0

See https://github.com/openmaptiles/openmaptiles/blob/master/LICENSE.md for details on usage
*/
//! Port of `layers/Transportation.java`: defines the logic for generating map elements for roads, shipways,
//! railroads, and paths in the `transportation` layer from source features.
//!
//! This is ported from the
//! [OpenMapTiles transportation sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/transportation).
//!
//! Generates the shape for roads, trails, ferries, railways with detailed attributes for rendering, but not any
//! names. The `transportation_name` layer includes names, but less detailed attributes.
//!
//! Differences from Java:
//! - `highwayClass` (Java builds a 3-entry tag map per call and evaluates the class `MultiExpression`) uses a lookup
//!   table precomputed from the class field mapping over the finite set of values it mentions, so it never allocates.
//! - Internal callers compute the route relations of a way once and reuse them for the min zoom / class logic.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, LazyLock, OnceLock};

use geo::{BoundingRect, Buffer, CoordsIter, Intersects};
use geo_types::{Coord, Line, Rect};
use geo_types::{Geometry, MultiPolygon};
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::expression::expression::Expression;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::forwarding_profile::{Handler, LayerPostProcessor, OsmRelationPreprocessor};
use planetiler_core::geo::geo_utils;
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::reader::osm::osm_element::Relation;
use planetiler_core::reader::osm::osm_relation_info::OsmRelationInfo;
use planetiler_core::reader::source_feature::{SourceFeature, SourceFeatureExt};
use planetiler_core::reader::with_tags::WithTags;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::parse;
use planetiler_core::util::translations::Translations;
use planetiler_core::util::zoom_function::{MeterToPixelThresholds, ZoomFunction, meter_thresholds};
use planetiler_core::value::{Tags, Value};
use planetiler_core::vector_tile;
use regex::Regex;
use rstar::primitives::GeomWithData;
use rstar::{AABB, RTree};

use crate::feature_merge;
use crate::generated::open_map_tiles_schema::transportation::*;
use crate::generated::tables::*;
use crate::layer::Layer;
use crate::open_map_tiles_profile::NaturalEarthProcessor;
use crate::util::utils::{brunnel_with_ford, null_if_empty, null_if_int, null_if_long, null_or_empty};

static GREAT_BRITAIN_REF_NETWORK_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new("^[ABM][0-9ABM()]+").unwrap());
static IRELAND_REF_NETWORK_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new("^[MNRL][0-9]+").unwrap());

const RAILWAY_RAIL_VALUES: &[&str] = &[
    field_values::SUBCLASS_RAIL,
    field_values::SUBCLASS_NARROW_GAUGE,
    field_values::SUBCLASS_PRESERVED,
    field_values::SUBCLASS_FUNICULAR,
];
const RAILWAY_TRANSIT_VALUES: &[&str] = &[
    field_values::SUBCLASS_SUBWAY,
    field_values::SUBCLASS_LIGHT_RAIL,
    field_values::SUBCLASS_MONORAIL,
    field_values::SUBCLASS_TRAM,
];
const SERVICE_VALUES: &[&str] = &[
    field_values::SERVICE_SPUR,
    field_values::SERVICE_YARD,
    field_values::SERVICE_SIDING,
    field_values::SERVICE_CROSSOVER,
    field_values::SERVICE_DRIVEWAY,
    field_values::SERVICE_ALLEY,
    field_values::SERVICE_PARKING_AISLE,
];
const SURFACE_UNPAVED_VALUES: &[&str] = &[
    "unpaved",
    "compacted",
    "dirt",
    "earth",
    "fine_gravel",
    "grass",
    "grass_paver",
    "gravel",
    "gravel_turf",
    "ground",
    "ice",
    "mud",
    "pebblestone",
    "salt",
    "sand",
    "snow",
    "woodchips",
    "rock",
    "clay",
    "laterite",
];
const SURFACE_PAVED_VALUES: &[&str] = &[
    "paved",
    "asphalt",
    "cobblestone",
    "concrete",
    "concrete:lanes",
    "concrete:plates",
    "metal",
    "paving_stones",
    "sett",
    "unhewn_cobblestone",
    "wood",
    "grade1",
];
const ACCESS_NO_VALUES: &[&str] = &["private", "no"];
// ... and also Z4_MOTORWAY_NY_NETWORK, except those in Z5_MOTORWAYS_BY_NETWORK:
const Z5_TRUNK_BY_NETWORK: &[RouteNetwork] = &[
    RouteNetwork::CaTranscanada,
    RouteNetwork::CaProvincialArterial,
    RouteNetwork::UsInterstate,
    RouteNetwork::UsHighway,
    RouteNetwork::GbMotorway,
    RouteNetwork::GbTrunk,
    RouteNetwork::IeMotorway,
    RouteNetwork::IeNational,
    RouteNetwork::ERoad,
    RouteNetwork::ARoad,
];
const Z5_MOTORWAYS_BY_NETWORK: &[RouteNetwork] = &[RouteNetwork::GbTrunk, RouteNetwork::UsHighway];
const CA_AB_PRIMARY_AS_ARTERIAL_BY_REF: &[&str] = &["2", "3", "4"];
const CA_BC_AS_ARTERIAL_BY_REF: &[&str] = &["3", "5", "99"];
static MIN_LENGTH: LazyLock<MeterToPixelThresholds> =
    LazyLock::new(|| meter_thresholds().put(7, 50.0).put(6, 100.0).put(5, 500.0).put(4, 1_000.0));
const LIMIT_MERGE_TAG: &str = "__limit_merge";

/// Java `String.length()` (UTF-16 code units).
#[inline]
pub(crate) fn java_length(s: &str) -> usize {
    if s.is_ascii() { s.len() } else { s.encode_utf16().count() }
}

/// Route network types inferred from route relations (or ref naming conventions in GB and Ireland).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RouteNetwork {
    UsInterstate,
    UsHighway,
    UsState,
    CaTranscanada,
    CaProvincialArterial,
    CaProvincial,
    GbMotorway,
    GbTrunk,
    GbPrimary,
    IeMotorway,
    IeNational,
    IeRegional,
    ERoad,
    ARoad,
}

impl RouteNetwork {
    /// The value for the `network` attribute.
    pub fn name(self) -> &'static str {
        match self {
            RouteNetwork::UsInterstate => "us-interstate",
            RouteNetwork::UsHighway => "us-highway",
            RouteNetwork::UsState => "us-state",
            RouteNetwork::CaTranscanada => "ca-transcanada",
            RouteNetwork::CaProvincialArterial => "ca-provincial-arterial",
            RouteNetwork::CaProvincial => "ca-provincial",
            RouteNetwork::GbMotorway => "gb-motorway",
            RouteNetwork::GbTrunk => "gb-trunk",
            RouteNetwork::GbPrimary => "gb-primary",
            RouteNetwork::IeMotorway => "ie-motorway",
            RouteNetwork::IeNational => "ie-national",
            RouteNetwork::IeRegional => "ie-regional",
            RouteNetwork::ERoad => "e-road",
            RouteNetwork::ARoad => "a-road",
        }
    }

    /// The synthetic route `network` used for networks inferred from ref naming conventions.
    pub fn network(self) -> Option<&'static str> {
        match self {
            RouteNetwork::GbMotorway => Some("omt-gb-motorway"),
            RouteNetwork::GbTrunk => Some("omt-gb-trunk"),
            RouteNetwork::GbPrimary => Some("omt-gb-primary"),
            RouteNetwork::IeMotorway => Some("omt-ie-motorway"),
            RouteNetwork::IeNational => Some("omt-ie-national"),
            RouteNetwork::IeRegional => Some("omt-ie-regional"),
            _ => None,
        }
    }
}

/// Information extracted from route relations to use when processing ways in that relation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteRelation {
    pub r#ref: String,
    pub network: Option<String>,
    pub name: Option<String>,
    pub colour: Option<String>,
    pub network_type: Option<RouteNetwork>,
    pub rank: i8,
    pub id: i64,
}

impl RouteRelation {
    pub fn int_rank(&self) -> i32 {
        self.rank as i32
    }
}

impl OsmRelationInfo for RouteRelation {
    fn id(&self) -> i64 {
        self.id
    }

    fn estimate_memory_usage_bytes(&self) -> u64 {
        fn s(v: &Option<String>) -> u64 {
            v.as_ref().map_or(0, |s| 24 + s.len() as u64)
        }
        16 + 1
            + 8
            + 24
            + self.r#ref.len() as u64
            + 8
            + s(&self.network)
            + 8
            + s(&self.name)
            + 8
            + s(&self.colour)
            + 8
            + 8
    }
}

// ORDER BY network_type, network, LENGTH(ref), ref
impl Ord for RouteRelation {
    fn cmp(&self, other: &Self) -> Ordering {
        let nt = |r: &RouteRelation| r.network_type.map_or(i32::MAX, |n| n as i32);
        nt(self)
            .cmp(&nt(other))
            .then_with(|| self.network.as_deref().unwrap_or("").cmp(other.network.as_deref().unwrap_or("")))
            .then_with(|| java_length(&self.r#ref).cmp(&java_length(&other.r#ref)))
            .then_with(|| self.r#ref.cmp(&other.r#ref))
    }
}

impl PartialOrd for RouteRelation {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Min zoom and optional per-zoom class override for a highway.
#[derive(Clone, Debug)]
pub struct MinZoomAndNewClass {
    pub minzoom: u32,
    /// `(class at z<=5, class above z5)`, Java `ZoomFunction<String> classOverride`.
    pub class_override: Option<(&'static str, &'static str)>,
}

/*
 * Highway class lookup table
 */

const CLASS_KEYS: [&str; 3] = ["highway", "public_transport", "construction"];
const OTHER_VALUE: &str = "\u{1}__other__";

/// Precomputed result of the class field mapping for every combination of the (finite) values it mentions for
/// `highway`, `public_transport` and `construction`. Index 0 is "missing or empty", 1 is "any other value".
struct HighwayClassTable {
    values: [HashMap<&'static str, usize>; 3],
    dims: [usize; 3],
    table: Vec<Option<&'static str>>,
}

fn collect_values(expression: &'static Expression, out: &mut [Vec<&'static str>; 3]) {
    match expression {
        Expression::And(children) | Expression::Or(children) => {
            children.iter().for_each(|c| collect_values(c, out));
        }
        Expression::Not(child) => collect_values(child, out),
        Expression::MatchAny(any) => {
            let field = any.field.as_deref().expect("class mapping must match on fields");
            let idx = CLASS_KEYS.iter().position(|k| *k == field).expect("unexpected class mapping key");
            assert!(any.pattern.is_none(), "class mapping must not use wildcards");
            for value in &any.values {
                if let Value::String(s) = value
                    && !s.is_empty()
                    && !out[idx].contains(&&**s)
                {
                    out[idx].push(s);
                }
            }
        }
        Expression::MatchField(field) => {
            assert!(CLASS_KEYS.contains(&&**field), "unexpected class mapping key");
        }
        Expression::Constant(_) => {}
        other => panic!("unsupported class mapping expression {other:?}"),
    }
}

impl HighwayClassTable {
    fn new() -> HighwayClassTable {
        let mapping: &'static planetiler_core::expression::multi_expression::MultiExpression<&'static str> =
            &field_mappings::CLASS;
        let mut mentioned: [Vec<&'static str>; 3] = Default::default();
        for e in &mapping.expressions {
            collect_values(&e.expression, &mut mentioned);
        }
        let index = mapping.index();
        let mut values: [HashMap<&'static str, usize>; 3] = Default::default();
        let mut domains: [Vec<Option<&'static str>>; 3] = Default::default();
        for i in 0..3 {
            domains[i] = vec![None, Some(OTHER_VALUE)];
            for (j, v) in mentioned[i].iter().enumerate() {
                values[i].insert(*v, j + 2);
                domains[i].push(Some(*v));
            }
        }
        let dims = [domains[0].len(), domains[1].len(), domains[2].len()];
        let mut table = Vec::with_capacity(dims[0] * dims[1] * dims[2]);
        for a in &domains[0] {
            for b in &domains[1] {
                for c in &domains[2] {
                    let mut tags = Tags::default();
                    for (k, v) in CLASS_KEYS.iter().zip([a, b, c]) {
                        if let Some(v) = v {
                            tags.insert((*k).to_string(), Value::from(*v));
                        }
                    }
                    table.push(index.get_first(&tags).copied());
                }
            }
        }
        HighwayClassTable { values, dims, table }
    }

    #[inline]
    fn idx(&self, key: usize, value: Option<&str>) -> usize {
        match value {
            None | Some("") => 0,
            Some(v) => self.values[key].get(v).copied().unwrap_or(1),
        }
    }

    #[inline]
    fn get(
        &self,
        highway: Option<&str>,
        public_transport: Option<&str>,
        construction: Option<&str>,
    ) -> Option<&'static str> {
        let i = self.idx(0, highway);
        let j = self.idx(1, public_transport);
        let k = self.idx(2, construction);
        self.table[(i * self.dims[1] + j) * self.dims[2] + k]
    }
}

static CLASS_TABLE: LazyLock<HighwayClassTable> = LazyLock::new(HighwayClassTable::new);

/*
 * Static helpers
 */

/// Returns a value for `surface` tag constrained to a small set of known values from raw OSM data.
fn surface(value: Option<&str>) -> Option<&'static str> {
    let value = value?;
    if SURFACE_PAVED_VALUES.contains(&value) {
        Some(field_values::SURFACE_PAVED)
    } else if SURFACE_UNPAVED_VALUES.contains(&value) {
        Some(field_values::SURFACE_UNPAVED)
    } else {
        None
    }
}

/// Returns a value for `access` tag constrained to a small set of known values from raw OSM data.
fn access(value: Option<&str>) -> Option<&'static str> {
    value.filter(|v| ACCESS_NO_VALUES.contains(v)).map(|_| "no")
}

/// Returns a value for `service` tag constrained to a small set of known values from raw OSM data.
fn service(value: Option<&str>) -> Option<&'static str> {
    let value = value?;
    SERVICE_VALUES.iter().find(|v| **v == value).copied()
}

/// Returns a value for `official` field based on OSM `operator` and `informal` tags. Only applies to
/// highway=path/footway/cycleway/bridleway.
fn official(highway: Option<&str>, informal: Option<&str>, operator: Option<&str>) -> Option<i32> {
    if !matches!(highway, Some("path" | "footway" | "cycleway" | "bridleway")) {
        return None;
    }
    if informal == Some("yes") {
        return Some(0);
    }
    if informal == Some("no") || !null_or_empty(operator) {
        return Some(1);
    }
    None
}

fn railway_class(value: Option<&str>) -> Option<&'static str> {
    let value = value?;
    if RAILWAY_RAIL_VALUES.contains(&value) {
        Some("rail")
    } else if RAILWAY_TRANSIT_VALUES.contains(&value) {
        Some("transit")
    } else {
        None
    }
}

/// The `class` of a highway from its `highway`, `public_transport`, `construction` and `man_made` tags.
pub fn highway_class(
    highway: Option<&str>,
    public_transport: Option<&str>,
    construction: Option<&str>,
    man_made: Option<&str>,
) -> Option<&'static str> {
    if !null_or_empty(highway) || !null_or_empty(public_transport) {
        CLASS_TABLE.get(highway, public_transport, construction)
    } else {
        match man_made {
            Some("bridge") => Some("bridge"),
            Some("pier") => Some("pier"),
            _ => None,
        }
    }
}

/// The `subclass` of a highway (only for `path`).
pub fn highway_subclass<'s>(
    highway_class: &str,
    public_transport: Option<&'s str>,
    highway: Option<&'s str>,
) -> Option<&'s str> {
    if highway_class == field_values::CLASS_PATH { null_if_empty(public_transport).or(highway) } else { None }
}

pub fn is_footway_or_steps(highway: Option<&str>) -> bool {
    matches!(highway, Some("footway" | "steps"))
}

pub fn is_link(highway: Option<&str>) -> bool {
    highway.is_some_and(|h| h.ends_with("_link"))
}

fn is_residential_or_unclassified(highway: Option<&str>) -> bool {
    matches!(highway, Some("residential" | "unclassified"))
}

fn network_types(route_relations: &[Arc<RouteRelation>]) -> impl Iterator<Item = RouteNetwork> + '_ {
    route_relations.iter().filter_map(|r| r.network_type)
}

fn is_trunk_for_z5(highway: Option<&str>, route_relations: &[Arc<RouteRelation>]) -> bool {
    // Allow trunk roads that are part of a nation's most important route network to show at z5
    highway == Some("trunk") && network_types(route_relations).any(|nt| Z5_TRUNK_BY_NETWORK.contains(&nt))
}

fn is_motorway_with_network_for_z4(route_relations: &[Arc<RouteRelation>]) -> bool {
    // All roads in network included in osm_national_network except gb-trunk and us-highway
    network_types(route_relations)
        .filter(|nt| !Z5_MOTORWAYS_BY_NETWORK.contains(nt))
        .any(|nt| Z5_TRUNK_BY_NETWORK.contains(&nt))
}

fn is_motorway_wo_network_for_z4(route_relations: &[Arc<RouteRelation>]) -> bool {
    // All motorways without network (e.g. EU, Asia, South America)
    network_types(route_relations).next().is_none()
}

fn is_motorway_for_z4(route_relations: &[Arc<RouteRelation>]) -> bool {
    is_motorway_wo_network_for_z4(route_relations) || is_motorway_with_network_for_z4(route_relations)
}

fn is_driveway_or_parking_aisle(service: Option<&str>) -> bool {
    matches!(service, Some(field_values::SERVICE_PARKING_AISLE | field_values::SERVICE_DRIVEWAY))
}

fn is_bridge_or_pier(man_made: Option<&str>) -> bool {
    matches!(man_made, Some("bridge" | "pier"))
}

/// `class.replace(base, "motorway")` for trunk classes.
fn trunk_to_motorway(class: &'static str) -> &'static str {
    match class {
        field_values::CLASS_TRUNK => field_values::CLASS_MOTORWAY,
        field_values::CLASS_TRUNK_CONSTRUCTION => field_values::CLASS_MOTORWAY_CONSTRUCTION,
        other => other,
    }
}

fn base_class(class: &str) -> &str {
    class.strip_suffix("_construction").unwrap_or(class)
}

/// A polygon prepared for fast repeated intersection tests (Java `PreparedGeometry`; geo's `PreparedGeometry` is not
/// `Send`/`Sync`): an R-tree of its edges for segment crossings plus R-tree-backed ray casting for containment.
struct Prepared {
    bounds: Option<Rect<f64>>,
    edges: RTree<GeomWithData<rstar::primitives::Line<[f64; 2]>, ()>>,
}

impl Prepared {
    fn new(polygon: &MultiPolygon<f64>) -> Prepared {
        let mut edges = Vec::new();
        for poly in &polygon.0 {
            for ring in std::iter::once(poly.exterior()).chain(poly.interiors()) {
                for line in ring.lines() {
                    edges.push(GeomWithData::new(
                        rstar::primitives::Line::new([line.start.x, line.start.y], [line.end.x, line.end.y]),
                        (),
                    ));
                }
            }
        }
        Prepared { bounds: polygon.bounding_rect(), edges: RTree::bulk_load(edges) }
    }

    fn contains_point(&self, p: Coord<f64>) -> bool {
        let Some(bounds) = self.bounds else {
            return false;
        };
        // count crossings of a ray from p towards +x
        let ray = AABB::from_corners([p.x, p.y], [bounds.max().x, p.y]);
        let mut inside = false;
        for edge in self.edges.locate_in_envelope_intersecting(ray) {
            let [ax, ay] = edge.geom().from;
            let [bx, by] = edge.geom().to;
            if (ay > p.y) != (by > p.y) {
                let x = ax + (p.y - ay) * (bx - ax) / (by - ay);
                if p.x < x {
                    inside = !inside;
                }
            }
        }
        inside
    }

    fn intersects_line(&self, line: Line<f64>) -> bool {
        let envelope = AABB::from_corners([line.start.x, line.start.y], [line.end.x, line.end.y]);
        self.edges.locate_in_envelope_intersecting(envelope).any(|edge| {
            let [ax, ay] = edge.geom().from;
            let [bx, by] = edge.geom().to;
            Line::new(Coord { x: ax, y: ay }, Coord { x: bx, y: by }).intersects(&line)
        })
    }

    fn intersects(&self, geometry: &Geometry<f64>) -> bool {
        let (Some(bounds), Some(other)) = (self.bounds, geometry.bounding_rect()) else {
            return false;
        };
        if !bounds.intersects(&other) {
            return false;
        }
        let lines_intersect = match geometry {
            Geometry::LineString(l) => l.lines().any(|line| self.intersects_line(line)),
            Geometry::MultiLineString(m) => m.iter().flat_map(|l| l.lines()).any(|line| self.intersects_line(line)),
            Geometry::Polygon(p) => p.exterior().lines().any(|line| self.intersects_line(line)),
            Geometry::MultiPolygon(m) => {
                m.iter().flat_map(|p| p.exterior().lines()).any(|line| self.intersects_line(line))
            }
            _ => false,
        };
        lines_intersect || geometry.coords_iter().next().is_some_and(|c| self.contains_point(c))
    }
}

/// The `transportation` layer. See the [module docs](self).
pub struct Transportation {
    config: Arc<PlanetilerConfig>,
    stats: StatsRef,
    z13_paths: bool,
    logged_no_gb: AtomicBool,
    logged_no_ireland: AtomicBool,
    great_britain: OnceLock<Prepared>,
    ireland: OnceLock<Prepared>,
}

fn prepare_country(feature: &dyn SourceFeature) -> GeometryResult<Prepared> {
    let polygon = feature.polygon()?;
    let multi = match polygon {
        Geometry::Polygon(p) => MultiPolygon(vec![p.clone()]),
        Geometry::MultiPolygon(m) => m.clone(),
        _ => MultiPolygon(vec![]),
    };
    let buffered = multi.buffer(geo_utils::meters_to_pixel_at_equator(0, 10_000.0) / 256.0);
    Ok(Prepared::new(&buffered))
}

impl Transportation {
    pub fn new(_translations: &Arc<Translations>, config: &Arc<PlanetilerConfig>, stats: &StatsRef) -> Transportation {
        let z13_paths = config.arguments.get_boolean(
            "transportation_z13_paths",
            "transportation(_name) layer: show all paths on z13",
            false,
        );
        Transportation {
            config: config.clone(),
            stats: stats.clone(),
            z13_paths,
            logged_no_gb: AtomicBool::new(false),
            logged_no_ireland: AtomicBool::new(false),
            great_britain: OnceLock::new(),
            ireland: OnceLock::new(),
        }
    }

    fn minzoom_for_class(&self, class: &str) -> u32 {
        match class {
            field_values::CLASS_PATH => {
                if self.z13_paths {
                    13
                } else {
                    14
                }
            }
            field_values::CLASS_TRACK => 14,
            field_values::CLASS_SERVICE => 13,
            field_values::CLASS_MINOR => 13,
            field_values::CLASS_RACEWAY => 12,
            field_values::CLASS_TERTIARY => 11,
            field_values::CLASS_BUSWAY => 11,
            field_values::CLASS_BUS_GUIDEWAY => 11,
            field_values::CLASS_SECONDARY => 9,
            field_values::CLASS_PRIMARY => 7,
            field_values::CLASS_TRUNK => 6,
            field_values::CLASS_MOTORWAY => 4,
            _ => u32::MAX,
        }
    }

    fn intersects(&self, prepared: &Prepared, element: &OsmHighwayLinestring<'_>, stat: &str, msg: &str) -> bool {
        match element.source().world_geometry() {
            Ok(geometry) => prepared.intersects(geometry),
            Err(e) => {
                e.log_with_stats(&*self.stats, stat, &format!("{msg}: {}", element.source().id()));
                false
            }
        }
    }

    /// Returns the route relations of a highway (sorted), including relations inferred from ref naming conventions
    /// in Great Britain and Ireland.
    pub fn get_route_relations(&self, element: &OsmHighwayLinestring<'_>) -> Vec<Arc<RouteRelation>> {
        let r#ref = element.r#ref();
        let relations = element.source().relation_info::<RouteRelation>();
        let mut result: Vec<Arc<RouteRelation>> = Vec::with_capacity(relations.len() + 1);
        for member in relations {
            // avoid duplicates - list should be very small and usually only one
            if !result.iter().any(|r| **r == *member.relation) {
                result.push(member.relation);
            }
        }
        if let Some(r#ref) = r#ref.as_deref() {
            // GB doesn't use regular relations like everywhere else, so if we are
            // in GB then use a naming convention instead.
            if let Some(m) = GREAT_BRITAIN_REF_NETWORK_PATTERN.find(r#ref) {
                match self.great_britain.get() {
                    None => {
                        if !self.logged_no_gb.swap(true, AtomicOrdering::Relaxed) {
                            tracing::warn!("No GB polygon for inferring route network types");
                        }
                    }
                    Some(gb) => {
                        if self.intersects(
                            gb,
                            element,
                            "omt_transportation_name_gb_test",
                            "Unable to test highway against GB route network",
                        ) {
                            let network_type = match element.highway().as_deref() {
                                Some("motorway") => Some(RouteNetwork::GbMotorway),
                                Some("trunk") => Some(RouteNetwork::GbTrunk),
                                Some("primary" | "secondary") => Some(RouteNetwork::GbPrimary),
                                _ => None,
                            };
                            result.push(Arc::new(RouteRelation {
                                r#ref: m.as_str().to_string(),
                                network: network_type.and_then(RouteNetwork::network).map(String::from),
                                name: None,
                                colour: None,
                                network_type,
                                rank: -1,
                                id: 0,
                            }));
                        }
                    }
                }
            }
            // Similarly Ireland.
            if let Some(m) = IRELAND_REF_NETWORK_PATTERN.find(r#ref) {
                match self.ireland.get() {
                    None => {
                        if !self.logged_no_ireland.swap(true, AtomicOrdering::Relaxed) {
                            tracing::warn!("No IE polygon for inferring route network types");
                        }
                    }
                    Some(ie) => {
                        if self.intersects(
                            ie,
                            element,
                            "omt_transportation_name_ie_test",
                            "Unable to test highway against IE route network",
                        ) {
                            let network_type = match element.highway().as_deref() {
                                Some("motorway") => RouteNetwork::IeMotorway,
                                Some("trunk" | "primary") => RouteNetwork::IeNational,
                                _ => RouteNetwork::IeRegional,
                            };
                            result.push(Arc::new(RouteRelation {
                                r#ref: m.as_str().to_string(),
                                network: network_type.network().map(String::from),
                                name: None,
                                colour: None,
                                network_type: Some(network_type),
                                rank: -1,
                                id: 0,
                            }));
                        }
                    }
                }
            }
        }
        result.sort();
        result
    }

    /// The first route relation of a highway (see [`Transportation::get_route_relations`]).
    pub fn get_route_relation(&self, element: &OsmHighwayLinestring<'_>) -> Option<Arc<RouteRelation>> {
        self.get_route_relations(element).into_iter().next()
    }

    fn is_trunk_z5_mergeable_length(&self, element: &OsmHighwayLinestring<'_>) -> bool {
        match element.source().length() {
            Ok(length) => length < geo_utils::meters_to_pixel_at_equator(0, 500.0),
            Err(e) => {
                e.log_with_stats(
                    &*self.stats,
                    "omt_transportation_trunk_length",
                    &format!("Unable to get feature length for trunk upgrade: {}", element.source().id()),
                );
                false
            }
        }
    }

    /// Returns the min zoom and an optional class override for a highway.
    pub fn get_minzoom_and_class(
        &self,
        element: &OsmHighwayLinestring<'_>,
        highway_class: &'static str,
    ) -> MinZoomAndNewClass {
        let relations = self.get_route_relations(element);
        self.get_minzoom_and_class_with_relations(element, highway_class, &relations)
    }

    /// Same as [`Transportation::get_minzoom_and_class`] with precomputed route relations.
    pub fn get_minzoom_and_class_with_relations(
        &self,
        element: &OsmHighwayLinestring<'_>,
        highway_class: &'static str,
        route_relations: &[Arc<RouteRelation>],
    ) -> MinZoomAndNewClass {
        let mut highway_class_override = None;
        let route_rank = route_relations.iter().map(|r| r.int_rank()).fold(3, i32::min);
        let highway = element.highway();
        let highway = highway.as_deref();
        let construction = element.construction();

        let mut minzoom =
            if element.man_made().as_deref() == Some("pier") {
                13
            } else if is_residential_or_unclassified(highway) {
                12
            } else {
                let base = base_class(highway_class);
                match base {
                    field_values::CLASS_SERVICE => {
                        if is_driveway_or_parking_aisle(service(element.service().as_deref())) { 14 } else { 13 }
                    }
                    field_values::CLASS_TRACK | field_values::CLASS_PATH => {
                        if route_rank == 1 {
                            12
                        } else if self.z13_paths
                            || !null_or_empty(element.name().as_deref())
                            || route_rank <= 2
                            || !null_or_empty(element.sac_scale().as_deref())
                        {
                            13
                        } else {
                            14
                        }
                    }
                    field_values::CLASS_TRUNK => {
                        let mut z5trunk = is_trunk_for_z5(highway, route_relations);

                        // Allow small trunk segments to be processed at z5 so they can merge with surrounding motorways
                        if self.is_trunk_z5_mergeable_length(element) {
                            z5trunk = true;
                            highway_class_override = Some((trunk_to_motorway(highway_class), highway_class));
                        }

                        // and if it is good for Z5, it may be good also for Z4 (see CLASS_MOTORWAY bellow):
                        let mut clazz = field_values::CLASS_TRUNK;
                        if z5trunk && is_motorway_with_network_for_z4(route_relations) {
                            clazz = field_values::CLASS_MOTORWAY;
                            z5trunk = false;
                            highway_class_override = None;
                        }
                        if z5trunk { 5 } else { self.minzoom_for_class(clazz) }
                    }
                    field_values::CLASS_MOTORWAY => {
                        if is_motorway_for_z4(route_relations) {
                            self.minzoom_for_class(field_values::CLASS_MOTORWAY)
                        } else {
                            5
                        }
                    }
                    other => self.minzoom_for_class(other),
                }
            };

        if is_link(highway) || is_link(construction.as_deref()) {
            minzoom = minzoom.max(9);
        }
        MinZoomAndNewClass { minzoom, class_override: highway_class_override }
    }

    fn is_pier_polygon(&self, element: &OsmHighwayLinestring<'_>) -> bool {
        if element.man_made().as_deref() == Some("pier") {
            match element.source().world_geometry() {
                // ignore this because it's a polygon
                Ok(Geometry::LineString(line)) => return line.is_closed(),
                Ok(_) => {}
                Err(e) => {
                    e.log_with_stats(
                        &*self.stats,
                        "omt_transportation_pier",
                        &format!("Unable to decode pier geometry for {}", element.source().id()),
                    );
                    return true;
                }
            }
        }
        false
    }
}

impl NaturalEarthProcessor for Transportation {
    fn process_natural_earth(&self, table: &str, feature: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {
        if table != "ne_10m_admin_0_countries" {
            return;
        }
        // multiple threads call this method concurrently, GB (or IE) polygon *should* only be found once
        let (target, name) = if feature.has_tag_str("iso_a2", "GB") {
            (&self.great_britain, "Great Britain")
        } else if feature.has_tag_str("iso_a2", "IE") {
            (&self.ireland, "Ireland")
        } else {
            return;
        };
        match prepare_country(feature) {
            Ok(prepared) => {
                let _ = target.set(prepared);
            }
            Err(e) => tracing::error!("Failed to get {name} Polygon: {e}"),
        }
    }
}

impl OsmRelationPreprocessor for Transportation {
    fn preprocess_osm_relation(&self, relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
        let str_tag = |key: &str| relation.get_tag(key).and_then(Value::as_str);
        if !matches!(str_tag("route"), Some("road" | "hiking")) {
            return None;
        }
        let network = str_tag("network");
        let r#ref = str_tag("ref");
        let name = null_if_empty(str_tag("name"));
        let colour = null_if_empty(str_tag("colour")).or(null_if_empty(str_tag("ref:colour")));

        let network_type = match (network, r#ref) {
            (Some("US:I"), _) => Some(RouteNetwork::UsInterstate),
            (Some("US:US"), _) => Some(RouteNetwork::UsHighway),
            (Some(n), _) if n.len() == 5 && n.starts_with("US:") => Some(RouteNetwork::UsState),
            (Some(n), _) if n.starts_with("CA:transcanada") => Some(RouteNetwork::CaTranscanada),
            (Some("CA:QC:A"), _) => Some(RouteNetwork::CaProvincialArterial),
            (Some("CA:ON:primary"), r) => {
                if r.is_some_and(|r| r.len() == 3 && r.starts_with('4')) || r == Some("QEW") {
                    Some(RouteNetwork::CaProvincialArterial)
                } else {
                    Some(RouteNetwork::CaProvincial)
                }
            }
            (Some("CA:MB:PTH"), Some("75")) => Some(RouteNetwork::CaProvincialArterial),
            (Some("CA:AB:primary"), Some(r)) if CA_AB_PRIMARY_AS_ARTERIAL_BY_REF.contains(&r) => {
                Some(RouteNetwork::CaProvincialArterial)
            }
            (Some("CA:BC"), Some(r)) if CA_BC_AS_ARTERIAL_BY_REF.contains(&r) => {
                Some(RouteNetwork::CaProvincialArterial)
            }
            // in SQL: LIKE 'CA:__' OR network LIKE 'CA:__:%'
            (Some(n), _)
                if n.starts_with("CA:")
                    && (java_length(n) == 5 || (java_length(n) >= 6 && n.chars().nth(5) == Some(':'))) =>
            {
                Some(RouteNetwork::CaProvincial)
            }
            _ => None,
        };

        let rank = match network.unwrap_or("") {
            "iwn" | "nwn" | "rwn" => 1,
            "lwn" => 2,
            _ => {
                if relation.has_tag("osmc:symbol") || relation.has_tag("colour") {
                    2
                } else {
                    3
                }
            }
        };

        if network.is_some() || rank < 3 {
            Some(vec![Arc::new(RouteRelation {
                r#ref: r#ref.unwrap_or("").to_string(),
                network: network.map(String::from),
                name: name.map(String::from),
                colour: colour.map(String::from),
                network_type,
                rank,
                id: relation.id,
            })])
        } else {
            None
        }
    }
}

impl OsmHighwayLinestringHandler for Transportation {
    fn process_osm_highway_linestring(&self, element: &OsmHighwayLinestring<'_>, features: &mut FeatureCollector<'_>) {
        if element.is_area() {
            return;
        }

        let route_relations = self.get_route_relations(element);
        let network_type = route_relations.first().and_then(|r| r.network_type);

        let highway = element.highway();
        let highway = highway.as_deref();
        let public_transport = element.public_transport();
        let public_transport = public_transport.as_deref();
        let Some(highway_class) =
            highway_class(highway, public_transport, element.construction().as_deref(), element.man_made().as_deref())
        else {
            return;
        };
        let service = service(element.service().as_deref());
        if self.is_pier_polygon(element) {
            return;
        }
        let min_zoom_and_new_class =
            self.get_minzoom_and_class_with_relations(element, highway_class, &route_relations);
        let minzoom = min_zoom_and_new_class.minzoom;

        if minzoom > self.config.maxzoom {
            return;
        }

        let highway_ramp = is_link(highway);
        let is_ramp = element.is_ramp();
        let ramp_above_z12 = if highway_ramp || is_ramp { Some(1) } else { None };
        let ramp_below_z12 = if highway_ramp { Some(1) } else { None };

        let expressway = element.expressway() && highway != Some("motorway") && !(is_ramp || highway_ramp);

        let surface_value = element.surface().or_else(|| element.tracktype());
        let feature = features.line(LAYER_NAME);
        feature.set_buffer_pixels(BUFFER_SIZE);
        // main attributes at all zoom levels (used for grouping <= z8)
        match min_zoom_and_new_class.class_override {
            Some((low, high)) => {
                feature.set_attr(fields::CLASS, ZoomFunction::new(move |z| Some(if z <= 5 { low } else { high })))
            }
            None => feature.set_attr(fields::CLASS, highway_class),
        }
        .set_attr(fields::SUBCLASS, highway_subclass(highway_class, public_transport, highway))
        .set_attr(fields::NETWORK, network_type.map(RouteNetwork::name))
        .set_attr_with_min_size_range(
            fields::BRUNNEL,
            brunnel_with_ford(element.is_bridge(), element.is_tunnel(), element.is_ford()),
            4.0,
            4,
            12,
        )
        // z8+
        .set_attr_with_minzoom(fields::EXPRESSWAY, if expressway { Some(1) } else { None }, 8)
        // z9+
        .set_attr_with_min_size_range(fields::LAYER, null_if_long(element.layer(), 0), 4.0, 9, 12)
        .set_attr_with_minzoom(fields::BICYCLE, null_if_empty(element.bicycle().as_deref()), 9)
        .set_attr_with_minzoom(fields::FOOT, null_if_empty(element.foot().as_deref()), 9)
        .set_attr_with_minzoom(fields::HORSE, null_if_empty(element.horse().as_deref()), 9)
        .set_attr_with_minzoom(fields::MTB_SCALE, null_if_empty(element.mtb_scale().as_deref()), 9)
        .set_attr_with_minzoom(
            fields::OFFICIAL,
            official(highway, element.informal().as_deref(), element.operator().as_deref()),
            9,
        )
        .set_attr_with_minzoom(fields::ACCESS, access(element.access().as_deref()), 9)
        .set_attr_with_minzoom(fields::TOLL, if element.toll() { Some(1) } else { None }, 9);
        // sometimes z9+, sometimes z12+
        if minzoom >= 12 {
            feature.set_attr(fields::RAMP, ramp_above_z12);
        } else {
            feature.set_attr(
                fields::RAMP,
                ZoomFunction::new(move |z| {
                    if z < 9 {
                        None
                    } else if z >= 12 {
                        ramp_above_z12
                    } else {
                        ramp_below_z12
                    }
                }),
            );
        }
        // z12+
        feature
            .set_attr_with_minzoom(fields::SERVICE, service, 12)
            .set_attr_with_minzoom(fields::ONEWAY, null_if_int(element.is_oneway(), 0), 12)
            .set_attr_with_minzoom(fields::SURFACE, surface(surface_value.as_deref()), 12)
            .set_min_pixel_size(0.0) // merge during post-processing, then limit by size
            .set_sort_key(element.z_order())
            .set_min_zoom(minzoom);

        if is_footway_or_steps(highway) {
            feature
                .set_attr(fields::LEVEL, element.source().get_tag("level").and_then(parse::parse_long_or_null))
                .set_attr(fields::INDOOR, if element.indoor() { Some(1) } else { None });
        }
    }
}

impl OsmRailwayLinestringHandler for Transportation {
    fn process_osm_railway_linestring(&self, element: &OsmRailwayLinestring<'_>, features: &mut FeatureCollector<'_>) {
        let railway = element.railway();
        let railway = railway.as_deref();
        if let Some(clazz) = railway_class(railway) {
            let service_tag = element.service();
            let service_tag = null_if_empty(service_tag.as_deref());
            let minzoom = if service_tag.is_some() {
                14
            } else if railway == Some(field_values::SUBCLASS_RAIL) {
                if element.usage().as_deref() == Some("main") { 8 } else { 10 }
            } else if railway == Some(field_values::SUBCLASS_NARROW_GAUGE) {
                10
            } else if railway == Some(field_values::SUBCLASS_LIGHT_RAIL) {
                11
            } else {
                14
            };
            features
                .line(LAYER_NAME)
                .set_buffer_pixels(BUFFER_SIZE)
                .set_attr(fields::CLASS, clazz)
                .set_attr(fields::SUBCLASS, railway)
                .set_attr(fields::SERVICE, service(service_tag))
                .set_attr(fields::RAMP, if element.is_ramp() { Some(1i64) } else { None })
                .set_attr_with_minzoom(
                    fields::BRUNNEL,
                    brunnel_with_ford(element.is_bridge(), element.is_tunnel(), element.is_ford()),
                    10,
                )
                .set_attr_with_minzoom(fields::LAYER, null_if_long(element.layer(), 0), 9)
                .set_sort_key(element.z_order())
                .set_min_pixel_size(0.0) // merge during post-processing, then limit by size
                .set_min_zoom(minzoom);
        }
    }
}

impl OsmAerialwayLinestringHandler for Transportation {
    fn process_osm_aerialway_linestring(
        &self,
        element: &OsmAerialwayLinestring<'_>,
        features: &mut FeatureCollector<'_>,
    ) {
        features
            .line(LAYER_NAME)
            .set_buffer_pixels(BUFFER_SIZE)
            .set_attr(fields::CLASS, "aerialway")
            .set_attr(fields::SUBCLASS, element.aerialway().as_deref())
            .set_attr(fields::SERVICE, service(element.service().as_deref()))
            .set_attr(fields::ONEWAY, null_if_int(element.is_oneway(), 0))
            .set_attr(fields::RAMP, if element.is_ramp() { Some(1i64) } else { None })
            .set_attr(fields::BRUNNEL, brunnel_with_ford(element.is_bridge(), element.is_tunnel(), element.is_ford()))
            .set_attr(fields::LAYER, null_if_long(element.layer(), 0))
            .set_sort_key(element.z_order())
            .set_min_pixel_size(0.0) // merge during post-processing, then limit by size
            .set_min_zoom(12);
    }
}

impl OsmShipwayLinestringHandler for Transportation {
    fn process_osm_shipway_linestring(&self, element: &OsmShipwayLinestring<'_>, features: &mut FeatureCollector<'_>) {
        features
            .line(LAYER_NAME)
            .set_buffer_pixels(BUFFER_SIZE)
            .set_attr(fields::CLASS, element.shipway().as_deref()) // "ferry"
            // no subclass
            .set_attr(fields::SERVICE, service(element.service().as_deref()))
            .set_attr(fields::RAMP, if element.is_ramp() { Some(1i64) } else { None })
            .set_attr(fields::BRUNNEL, brunnel_with_ford(element.is_bridge(), element.is_tunnel(), element.is_ford()))
            .set_attr(fields::LAYER, null_if_long(element.layer(), 0))
            .set_sort_key(element.z_order())
            .set_min_pixel_size(0.0) // merge during post-processing, then limit by size
            .set_min_zoom(4)
            .set_min_pixel_size_below_zoom(10, 32.0); // `sql_filter: ST_Length(...)` used in OpenMapTiles translates to 32px
    }
}

impl OsmHighwayPolygonHandler for Transportation {
    fn process_osm_highway_polygon(&self, element: &OsmHighwayPolygon<'_>, features: &mut FeatureCollector<'_>) {
        let man_made = element.man_made();
        let man_made = man_made.as_deref();
        let layer = element.layer();
        if is_bridge_or_pier(man_made)
            // only allow closed ways where area=yes, and multipolygons
            // and ignore underground pedestrian areas
            || (!element.source().can_be_line() && layer >= 0)
        {
            let highway = element.highway();
            let public_transport = element.public_transport();
            if let Some(highway_class) = highway_class(highway.as_deref(), public_transport.as_deref(), None, man_made)
            {
                features
                    .polygon(LAYER_NAME)
                    .set_buffer_pixels(BUFFER_SIZE)
                    .set_attr(fields::CLASS, highway_class)
                    .set_attr(
                        fields::SUBCLASS,
                        highway_subclass(highway_class, public_transport.as_deref(), highway.as_deref()),
                    )
                    .set_attr(fields::BRUNNEL, brunnel_with_ford(man_made == Some("bridge"), false, false))
                    .set_attr(fields::LAYER, null_if_long(layer, 0))
                    .set_sort_key(element.z_order())
                    .set_min_zoom(13);
            }
        }
    }
}

impl LayerPostProcessor for Transportation {
    fn post_process(&self, zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        let tolerance = self.config.tolerance(zoom);
        let min_length = MIN_LENGTH.apply(zoom).unwrap_or(0.0);

        // don't merge road segments with oneway tag
        // TODO merge preserving oneway instead ignoring
        let mut oneway_id = 1i64;
        for item in items.iter_mut() {
            let oneway = match item.tags.get(fields::ONEWAY) {
                Some(Value::Int(i)) => Some(*i),
                Some(Value::Float(f)) => Some(*f as i64),
                Some(Value::Double(d)) => Some(*d as i64),
                _ => None,
            };
            if matches!(oneway, Some(-1 | 1)) {
                item.tags.insert(LIMIT_MERGE_TAG.to_string(), Value::Int(oneway_id));
                oneway_id += 1;
            }
        }

        let mut merged = feature_merge::merge_line_strings(std::mem::take(items), min_length, tolerance, BUFFER_SIZE);

        for item in &mut merged {
            item.tags.shift_remove(LIMIT_MERGE_TAG);
        }
        *items = merged;
        Ok(())
    }
}

impl Handler for Transportation {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }

    fn as_osm_relation_preprocessor(&self) -> Option<&dyn OsmRelationPreprocessor> {
        Some(self)
    }
}

impl RowHandlers for Transportation {
    fn as_osm_aerialway_linestring_handler(&self) -> Option<&dyn OsmAerialwayLinestringHandler> {
        Some(self)
    }

    fn as_osm_highway_linestring_handler(&self) -> Option<&dyn OsmHighwayLinestringHandler> {
        Some(self)
    }

    fn as_osm_railway_linestring_handler(&self) -> Option<&dyn OsmRailwayLinestringHandler> {
        Some(self)
    }

    fn as_osm_shipway_linestring_handler(&self) -> Option<&dyn OsmShipwayLinestringHandler> {
        Some(self)
    }

    fn as_osm_highway_polygon_handler(&self) -> Option<&dyn OsmHighwayPolygonHandler> {
        Some(self)
    }
}

impl Layer for Transportation {
    fn as_natural_earth_processor(&self) -> Option<&dyn NaturalEarthProcessor> {
        Some(self)
    }

    fn ignore_wikidata(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "transportation_tests.rs"]
mod tests;
