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
//! Port of `layers/TransportationName.java`: defines the logic for generating map elements for road, shipway, rail,
//! and path names in the `transportation_name` layer from source features.
//!
//! This is ported from the
//! [OpenMapTiles transportation_name sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/transportation_name).
//!
//! Generate road names from OSM data. Route networkType and ref are copied from relations that roads are a part of -
//! except in Great Britain which uses a naming convention instead of relations.
//!
//! The goal is to make name linestrings as long as possible to give clients the best chance of showing road names at
//! different zoom levels, so do not limit linestrings by length at process time and merge them at tile render-time.
//!
//! Any 3-way nodes and intersections break line merging so set the `transportation_name_limit_merge` argument to
//! true to add temporary "is link" and "relation" keys to prevent opposite directions of a divided highway or on/off
//! ramps from getting merged for main highways.
//!
//! Differences from Java: motorway junction node IDs collected during the node pass are frozen into a read-only set
//! the first time a way is preprocessed (OSM files list nodes before ways), so the way pass checks node membership
//! without locking.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::forwarding_profile::{Handler, LayerPostProcessor, OsmNodePreprocessor, OsmWayPreprocessor};
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::reader::osm::osm_element::{Node, Way};
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::reader::with_tags::WithTags;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::parse;
use planetiler_core::util::translations::Translations;
use planetiler_core::util::zoom_function::{MeterToPixelThresholds, ZoomFunction, meter_thresholds};
use planetiler_core::value::{Tags, Value};
use planetiler_core::vector_tile;

use crate::feature_merge;
use crate::generated::open_map_tiles_schema::transportation_name::*;
use crate::generated::tables::*;
use crate::layer::Layer;
use crate::layers::transportation::{
    Transportation, highway_class, highway_subclass, is_footway_or_steps, is_link, java_length,
};
use crate::util::omt_language_utils;
use crate::util::utils::{brunnel_with_ford, null_if_empty, null_if_long, null_or_empty};

// extra temp key used to group on/off-ramps separately from main highways
const LINK_TEMP_KEY: &str = "__islink";
const RELATION_ID_TEMP_KEY: &str = "__relid";

static MIN_LENGTH: LazyLock<MeterToPixelThresholds> = LazyLock::new(|| {
    meter_thresholds()
        .put(6, 20_000.0)
        .put(7, 20_000.0)
        .put(8, 14_000.0)
        .put(9, 8_000.0)
        .put(10, 4_000.0)
        .put(11, 2_000.0)
});
static MIN_LENGTH_FN: LazyLock<ZoomFunction<f64>> = LazyLock::new(|| MIN_LENGTH.clone().into_fn());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum HighwayClass {
    Motorway = 6,
    Trunk = 5,
    Primary = 4,
    Secondary = 3,
    Tertiary = 2,
    Unclassified = 1,
    Unknown = 0,
}

impl HighwayClass {
    fn from_highway(highway: &str) -> HighwayClass {
        match highway {
            "motorway" => HighwayClass::Motorway,
            "trunk" => HighwayClass::Trunk,
            "primary" => HighwayClass::Primary,
            "secondary" => HighwayClass::Secondary,
            "tertiary" => HighwayClass::Tertiary,
            "unclassified" => HighwayClass::Unclassified,
            _ => HighwayClass::Unknown,
        }
    }

    fn from_value(value: u8) -> HighwayClass {
        match value {
            6 => HighwayClass::Motorway,
            5 => HighwayClass::Trunk,
            4 => HighwayClass::Primary,
            3 => HighwayClass::Secondary,
            2 => HighwayClass::Tertiary,
            1 => HighwayClass::Unclassified,
            _ => HighwayClass::Unknown,
        }
    }

    fn highway_value(self) -> &'static str {
        match self {
            HighwayClass::Motorway => "motorway",
            HighwayClass::Trunk => "trunk",
            HighwayClass::Primary => "primary",
            HighwayClass::Secondary => "secondary",
            HighwayClass::Tertiary => "tertiary",
            HighwayClass::Unclassified => "unclassified",
            HighwayClass::Unknown => "",
        }
    }
}

/// `(network, ref, name, colour)` of a route, used to dedupe `route_<n>_*` attributes.
type RouteKey<'a> = (Option<&'a str>, &'a str, Option<&'a str>, Option<&'a str>);

/// The `transportation_name` layer. See the [module docs](self).
pub struct TransportationName {
    brunnel: bool,
    size_for_shield: bool,
    limit_merge: bool,
    minor_refs: bool,
    config: Arc<PlanetilerConfig>,
    translations: Arc<Translations>,
    transportation: OnceLock<Arc<Transportation>>,
    /// Motorway junction nodes collected during the node pass.
    motorway_junction_nodes: Mutex<HashSet<i64>>,
    /// [`Self::motorway_junction_nodes`] frozen when the way pass starts.
    motorway_junction_nodes_frozen: OnceLock<HashSet<i64>>,
    motorway_junction_highway_classes: Mutex<HashMap<i64, u8>>,
}

impl TransportationName {
    pub fn new(
        translations: &Arc<Translations>,
        config: &Arc<PlanetilerConfig>,
        _stats: &StatsRef,
    ) -> TransportationName {
        let args = &config.arguments;
        TransportationName {
            brunnel: args.get_boolean(
                "transportation_name_brunnel",
                "transportation_name layer: set to false to omit brunnel and help merge long highways",
                false,
            ),
            size_for_shield: args.get_boolean(
                "transportation_name_size_for_shield",
                "transportation_name layer: allow road names on shorter segments (ie. they will have a shield)",
                false,
            ),
            limit_merge: args.get_boolean(
                "transportation_name_limit_merge",
                "transportation_name layer: limit merge so we don't combine different relations to help merge long highways",
                false,
            ),
            minor_refs: args.get_boolean(
                "transportation_name_minor_refs",
                "transportation_name layer: include name and refs from minor road networks if not present on a way",
                false,
            ),
            config: config.clone(),
            translations: translations.clone(),
            transportation: OnceLock::new(),
            motorway_junction_nodes: Mutex::new(HashSet::new()),
            motorway_junction_nodes_frozen: OnceLock::new(),
            motorway_junction_highway_classes: Mutex::new(HashMap::new()),
        }
    }

    /// Gives this layer access to the transportation layer (route relations, minzoom/class logic).
    pub fn needs_transportation_layer(&self, transportation: Arc<Transportation>) {
        let _ = self.transportation.set(transportation);
    }

    fn names(&self, source: &dyn SourceFeature) -> Tags {
        omt_language_utils::get_names(source.tags(), Some(&self.translations))
    }

    /// Returns the minimum pixel length that a name will fit into.
    fn get_min_length_for_name(&self, attrs: &Tags) -> f64 {
        let r#ref = attrs.get(fields::REF).filter(|v| !v.is_null());
        let name = attrs.get(fields::NAME).filter(|v| !v.is_null()).or(r#ref);
        if self.size_for_shield && matches!(r#ref, Some(Value::String(_))) {
            6.0
        } else if let Some(Value::String(s)) = name {
            java_length(s) as f64 * 6.0
        } else {
            f64::MAX
        }
    }
}

impl OsmNodePreprocessor for TransportationName {
    fn preprocess_osm_node(&self, node: &Node) {
        if node.has_tag_str("highway", "motorway_junction") {
            self.motorway_junction_nodes.lock().unwrap().insert(node.id);
        }
    }
}

impl OsmWayPreprocessor for TransportationName {
    fn preprocess_osm_way(&self, way: &Way) {
        let Some(highway) = way.get_tag("highway").and_then(Value::as_str) else {
            return;
        };
        let cls = HighwayClass::from_highway(highway);
        if cls != HighwayClass::Unknown {
            let junctions = self
                .motorway_junction_nodes_frozen
                .get_or_init(|| std::mem::take(&mut *self.motorway_junction_nodes.lock().unwrap()));
            if junctions.is_empty() {
                return;
            }
            for node in &way.nodes {
                if junctions.contains(node) {
                    let mut classes = self.motorway_junction_highway_classes.lock().unwrap();
                    let old_value = classes.get(node).copied().unwrap_or(HighwayClass::Unknown as u8);
                    let new_value = cls as u8;
                    if new_value > old_value {
                        classes.insert(*node, new_value);
                    }
                }
            }
        }
    }
}

impl OsmHighwayPointHandler for TransportationName {
    fn process_osm_highway_point(&self, element: &OsmHighwayPoint<'_>, features: &mut FeatureCollector<'_>) {
        let id = element.source().id();
        let value = self.motorway_junction_highway_classes.lock().unwrap().get(&id).copied().unwrap_or(0);
        if value > 0 {
            let cls = HighwayClass::from_value(value);
            if cls != HighwayClass::Unknown {
                let subclass = field_values::SUBCLASS_JUNCTION;
                let r#ref = element.r#ref();
                features
                    .point(LAYER_NAME)
                    .set_buffer_pixels(BUFFER_SIZE)
                    .put_attrs(self.names(element.source()))
                    .set_attr(fields::REF, r#ref.as_deref())
                    .set_attr(fields::REF_LENGTH, r#ref.as_deref().map(|r| java_length(r) as i32))
                    .set_attr(fields::CLASS, highway_class(Some(cls.highway_value()), None, None, None))
                    .set_attr(fields::SUBCLASS, subclass)
                    .set_attr(fields::LAYER, null_if_long(element.layer(), 0))
                    .set_sort_key_descending(element.z_order())
                    .set_min_zoom(10);
            }
        }
    }
}

impl OsmHighwayLinestringHandler for TransportationName {
    fn process_osm_highway_linestring(&self, element: &OsmHighwayLinestring<'_>, features: &mut FeatureCollector<'_>) {
        let Some(transportation) = self.transportation.get() else {
            return;
        };
        let element_ref = element.r#ref();
        let mut r#ref: Option<&str> = element_ref.as_deref();
        let relations = transportation.get_route_relations(element);
        let first_relation_with_network = relations.iter().find(|rel| rel.network_type.is_some());

        if let Some(rel) = first_relation_with_network
            && !rel.r#ref.is_empty()
        {
            r#ref = Some(&rel.r#ref);
        }

        // if transportation_name_minor_refs flag set and we don't have a ref yet, then pull it from any network
        if null_or_empty(r#ref) && self.minor_refs && !relations.is_empty() {
            r#ref = relations.iter().map(|r| r.r#ref.as_str()).find(|r| !r.is_empty());
        }

        let name = element.name();
        let name = null_if_empty(name.as_deref());
        let r#ref = null_if_empty(r#ref);
        let highway = element.highway();
        let highway = null_if_empty(highway.as_deref());

        let highway_class = highway_class(
            element.highway().as_deref(),
            None,
            element.construction().as_deref(),
            element.man_made().as_deref(),
        );
        let (Some(highway_value), Some(highway_class)) = (highway, highway_class) else {
            return;
        };
        if element.is_area() || (name.is_none() && r#ref.is_none()) {
            return;
        }

        let link = is_link(Some(highway_value));
        let base_class = highway_class.strip_suffix("_construction").unwrap_or(highway_class);

        let mut minzoom = if base_class == field_values::CLASS_TRUNK {
            8
        } else if base_class == field_values::CLASS_MOTORWAY {
            6
        } else if link {
            13
        } else {
            12 // fallback - get from line minzoom, but floor at 12
        };

        // inherit min zoom threshold from visible road, and ensure we never show a label on a road that's not visible
        // yet.
        minzoom = minzoom
            .max(transportation.get_minzoom_and_class_with_relations(element, highway_class, &relations).minzoom);
        if minzoom > self.config.maxzoom {
            return;
        }

        let feature = features.line(LAYER_NAME);
        feature
            .set_buffer_pixels(BUFFER_SIZE)
            .set_buffer_pixel_overrides(Some(MIN_LENGTH_FN.clone()))
            // TODO abbreviate road names - can't port osml10n because it is AGPL
            .put_attrs(self.names(element.source()))
            .set_attr(fields::REF, r#ref)
            .set_attr(fields::REF_LENGTH, r#ref.map(|r| java_length(r) as i32))
            .set_attr(
                fields::NETWORK,
                match first_relation_with_network.and_then(|r| r.network_type) {
                    Some(network_type) => Some(network_type.name()),
                    None if !null_or_empty(r#ref) => Some("road"),
                    None => None,
                },
            )
            .set_attr(fields::CLASS, highway_class)
            .set_attr(fields::SUBCLASS, highway_subclass(highway_class, None, Some(highway_value)))
            .set_min_pixel_size(0.0)
            .set_sort_key(element.z_order())
            .set_min_zoom(minzoom);

        // populate route_1_<something>, route_2_<something>, ... route_n_<something> tags and remove duplicates
        let mut routes: Vec<RouteKey<'_>> = Vec::with_capacity(relations.len());
        for route in &relations {
            let key = (route.network.as_deref(), route.r#ref.as_str(), route.name.as_deref(), route.colour.as_deref());
            if !routes.contains(&key) {
                routes.push(key);
                let n = routes.len();
                feature.set_attr(format!("route_{n}_network"), route.network.as_deref());
                feature.set_attr(format!("route_{n}_ref"), null_if_empty(Some(route.r#ref.as_str())));
                feature.set_attr(format!("route_{n}_name"), route.name.as_deref());
                feature.set_attr(format!("route_{n}_colour"), route.colour.as_deref());
            }
        }

        if self.brunnel {
            // from OMT: "Drop brunnel if length of way < 2% of tile width (less than 3 pixels)"
            feature.set_attr_with_min_size_range(
                fields::BRUNNEL,
                brunnel_with_ford(element.is_bridge(), element.is_tunnel(), element.is_ford()),
                3.0,
                4,
                12,
            );
        }

        // to help group roads into longer segments, add temporary tags to limit which segments get grouped together.
        // Since a divided highway typically has a separate relation for each direction, this ends up keeping segments
        // going opposite directions group getting grouped together and confusing the line merging process
        if self.limit_merge {
            feature
                .set_attr(LINK_TEMP_KEY, if link { 1 } else { 0 })
                .set_attr(RELATION_ID_TEMP_KEY, first_relation_with_network.map(|r| r.id));
        }

        if is_footway_or_steps(Some(highway_value)) {
            feature
                .set_attr_with_minzoom(fields::LAYER, null_if_long(element.layer(), 0), 12)
                .set_attr_with_minzoom(
                    fields::LEVEL,
                    element.source().get_tag("level").and_then(parse::parse_long_or_null),
                    12,
                )
                .set_attr_with_minzoom(fields::INDOOR, if element.indoor() { Some(1) } else { None }, 12);
        }
    }
}

impl OsmAerialwayLinestringHandler for TransportationName {
    fn process_osm_aerialway_linestring(
        &self,
        element: &OsmAerialwayLinestring<'_>,
        features: &mut FeatureCollector<'_>,
    ) {
        if !null_or_empty(element.name().as_deref()) {
            features
                .line(LAYER_NAME)
                .set_buffer_pixels(BUFFER_SIZE)
                .set_buffer_pixel_overrides(Some(MIN_LENGTH_FN.clone()))
                .put_attrs(self.names(element.source()))
                .set_attr(fields::CLASS, "aerialway")
                .set_attr(fields::SUBCLASS, element.aerialway().as_deref())
                .set_min_pixel_size(0.0)
                .set_sort_key(element.z_order())
                .set_min_zoom(12);
        }
    }
}

impl OsmShipwayLinestringHandler for TransportationName {
    fn process_osm_shipway_linestring(&self, element: &OsmShipwayLinestring<'_>, features: &mut FeatureCollector<'_>) {
        if !null_or_empty(element.name().as_deref()) {
            features
                .line(LAYER_NAME)
                .set_buffer_pixels(BUFFER_SIZE)
                .set_buffer_pixel_overrides(Some(MIN_LENGTH_FN.clone()))
                .put_attrs(self.names(element.source()))
                .set_attr(fields::CLASS, element.shipway().as_deref())
                .set_min_pixel_size(0.0)
                .set_sort_key(element.z_order())
                .set_min_zoom(12);
        }
    }
}

impl LayerPostProcessor for TransportationName {
    fn post_process(&self, zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        let tolerance = self.config.tolerance(zoom);
        let min_length = MIN_LENGTH.apply(zoom).unwrap_or(0.0);
        // TODO tolerances:
        // z6: (tolerance: 500)
        // z7: (tolerance: 200)
        // z8: (tolerance: 120)
        // z9-11: (tolerance: 50)
        let length_limit_calculator = |attrs: &Tags| {
            if zoom >= 14 {
                0.0
            } else if min_length > 0.0 {
                min_length
            } else {
                self.get_min_length_for_name(attrs)
            }
        };
        let mut result = feature_merge::merge_line_strings_dynamic(
            std::mem::take(items),
            length_limit_calculator,
            tolerance,
            BUFFER_SIZE,
        );
        if self.limit_merge {
            // remove temp keys that were just used to improve line merging
            for feature in &mut result {
                feature.tags.shift_remove(LINK_TEMP_KEY);
                feature.tags.shift_remove(RELATION_ID_TEMP_KEY);
            }
        }
        *items = result;
        Ok(())
    }
}

impl Handler for TransportationName {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }

    fn as_osm_node_preprocessor(&self) -> Option<&dyn OsmNodePreprocessor> {
        Some(self)
    }

    fn as_osm_way_preprocessor(&self) -> Option<&dyn OsmWayPreprocessor> {
        Some(self)
    }
}

impl RowHandlers for TransportationName {
    fn as_osm_highway_point_handler(&self) -> Option<&dyn OsmHighwayPointHandler> {
        Some(self)
    }

    fn as_osm_highway_linestring_handler(&self) -> Option<&dyn OsmHighwayLinestringHandler> {
        Some(self)
    }

    fn as_osm_aerialway_linestring_handler(&self) -> Option<&dyn OsmAerialwayLinestringHandler> {
        Some(self)
    }

    fn as_osm_shipway_linestring_handler(&self) -> Option<&dyn OsmShipwayLinestringHandler> {
        Some(self)
    }
}

impl Layer for TransportationName {
    fn ignore_wikidata(&self) -> bool {
        true
    }
}
