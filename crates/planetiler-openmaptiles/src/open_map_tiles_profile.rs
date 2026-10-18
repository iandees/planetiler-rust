//! Port of `OpenMapTilesProfile.java`: delegates the logic for generating a map to individual implementations in the
//! [`layers`](crate::layers) module.
//!
//! Layer implementations (see [`Layer`]) subscribe to elements from different sources:
//! - [`LakeCenterlineProcessor`]
//! - [`NaturalEarthProcessor`]
//! - [`OsmWaterPolygonProcessor`]
//! - [`OsmAllProcessor`] to process every OSM feature
//! - `OsmRelationPreprocessor` (core) to process every OSM relation during the first pass through the OSM file
//! - a `*Handler` trait from [`tables`] to process input features filtered and parsed according to the imposm3
//!   mappings defined in the OpenMapTiles schema. Each element corresponds to a row in the table that imposm3 would
//!   have generated, with generated methods for accessing the data that would have been in each column.
//!
//! Layers can also subscribe to notifications when we finished processing an input source by implementing
//! `FinishHandler` or post-process features in that layer before rendering the output tile by implementing
//! `LayerPostProcessor`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::expression::expression::{ExpressionInput, PartialInput};
use planetiler_core::expression::multi_expression::{Index, Match};
use planetiler_core::expression::simplifiable::Simplifiable;
use planetiler_core::feature_collector::{self, FeatureCollector};
use planetiler_core::forwarding_profile::{ForwardingProfile, Handler};
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::geo::tile_coord::TileCoord;
use planetiler_core::profile::{FeatureConsumer, LayerFeatures, Profile};
use planetiler_core::reader::osm::osm_element::{Node, OsmElement, Relation, Way};
use planetiler_core::reader::osm::osm_relation_info::OsmRelationInfo;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::reader::with_geometry_type::WithGeometryType;
use planetiler_core::reader::with_tags::WithTags;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;
use planetiler_core::value::Tags;
use planetiler_core::vector_tile;

use crate::addons;
use crate::generated::open_map_tiles_schema;
use crate::generated::tables::{self, Table};
use crate::layer::Layer;
use crate::layers::transportation::Transportation;
use crate::layers::transportation_name::TransportationName;

// IDs used in stats and logs for each input source, as well as argument/config file overrides to source locations
pub const LAKE_CENTERLINE_SOURCE: &str = "lake_centerlines";
pub const WATER_POLYGON_SOURCE: &str = "water_polygons";
pub const NATURAL_EARTH_SOURCE: &str = "natural_earth";
pub const OSM_SOURCE: &str = "osm";

/// Layers implement this to subscribe to elements from [natural earth](https://www.naturalearthdata.com/).
pub trait NaturalEarthProcessor: Send + Sync {
    /// Process an element from `table` in the natural earth source (see [`Profile::process_feature`]).
    fn process_natural_earth(&self, table: &str, feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>);
}

/// Layers implement this to subscribe to elements from the
/// [OSM lake centerlines source](https://github.com/openmaptiles/osm-lakelines).
pub trait LakeCenterlineProcessor: Send + Sync {
    /// Process an element from the OSM lake centerlines source (see [`Profile::process_feature`]).
    fn process_lake_centerline(&self, feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>);
}

/// Layers implement this to subscribe to elements from the
/// [OSM water polygons source](https://osmdata.openstreetmap.de/data/water-polygons.html).
pub trait OsmWaterPolygonProcessor: Send + Sync {
    /// Process an element from the OSM water polygons source (see [`Profile::process_feature`]).
    fn process_osm_water(&self, feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>);
}

/// Layers implement this to subscribe to every OSM element.
pub trait OsmAllProcessor: Send + Sync {
    /// Process an OSM element during the second pass through the OSM data file (see [`Profile::process_feature`]).
    fn process_all_osm(&self, feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>);
}

/// The imposm3 table an element matched and the layers that handle rows from it.
#[derive(Clone)]
pub struct RowDispatch {
    pub table: Table,
    pub handlers: Vec<Arc<dyn Layer>>,
}

impl PartialEq for RowDispatch {
    fn eq(&self, other: &Self) -> bool {
        self.table == other.table
            && self.handlers.len() == other.handlers.len()
            && self.handlers.iter().zip(&other.handlers).all(|(a, b)| std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b)))
    }
}

impl std::fmt::Debug for RowDispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowDispatch")
            .field("table", &self.table)
            .field("handlers", &self.handlers.iter().map(|h| h.name().unwrap_or("?").to_string()).collect::<Vec<_>>())
            .finish()
    }
}

/// The OpenMapTiles profile. See the [module docs](self).
pub struct OpenMapTilesProfile {
    forwarding: ForwardingProfile,
    /// Index to efficiently find the imposm3 "table row" constructor from an OSM element based on its tags.
    osm_mappings: Arc<Index<RowDispatch>>,
    /// Index variant that filters out any table only used by layers that ignore wikidata.
    wikidata_mappings: Index<bool>,
    layers: Vec<Arc<dyn Layer>>,
}

impl std::fmt::Debug for OpenMapTilesProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenMapTilesProfile")
            .field("layers", &self.layers.iter().map(|h| h.name().unwrap_or("?").to_string()).collect::<Vec<_>>())
            .finish()
    }
}

fn downcast_layer<T: Layer>(layer: &Arc<dyn Layer>) -> Option<Arc<T>> {
    let any: Arc<dyn std::any::Any + Send + Sync> = layer.clone();
    any.downcast::<T>().ok()
}

fn same_layer(a: &Arc<dyn Layer>, b: &Arc<dyn Layer>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}

impl OpenMapTilesProfile {
    /// Creates the profile with all OpenMapTiles layers (filtered by `--only-layers`/`--exclude-layers`) plus any
    /// [`addons`].
    pub fn new(translations: Arc<Translations>, config: Arc<PlanetilerConfig>, stats: StatsRef) -> OpenMapTilesProfile {
        let mut forwarding = ForwardingProfile::with_config(&config);
        forwarding.set_layer_dependencies(HashMap::from([(
            open_map_tiles_schema::transportation_name::LAYER_NAME.to_string(),
            vec![open_map_tiles_schema::transportation::LAYER_NAME.to_string()],
        )]));

        // register release/finish/feature postprocessor/osm relationship handler methods...
        let mut layers: Vec<Arc<dyn Layer>> = Vec::new();
        let mut transportation_layer: Option<Arc<dyn Layer>> = None;
        let mut transportation_name_layer: Option<Arc<TransportationName>> = None;
        let omt_layers = open_map_tiles_schema::create_instances(&translations, &config, &stats);
        let extra_layers = addons::create(&translations, &config, &stats);
        for layer in omt_layers.into_iter().chain(extra_layers) {
            let handler: Arc<dyn Handler> = layer.clone();
            if forwarding.cares_about_handler(&*handler) {
                layers.push(layer.clone());
                forwarding.register_handler(handler);
                if let Some(transportation_name) = downcast_layer::<TransportationName>(&layer) {
                    transportation_name_layer = Some(transportation_name);
                }
            }
            if downcast_layer::<Transportation>(&layer).is_some() {
                transportation_layer = Some(layer);
            }
        }

        // special-case: transportation_name layer depends on transportation layer
        if let (Some(transportation_name), Some(transportation)) = (&transportation_name_layer, &transportation_layer) {
            transportation_name.needs_transportation_layer(
                downcast_layer::<Transportation>(transportation).expect("transportation layer"),
            );
            if !layers.iter().any(|l| same_layer(l, transportation)) {
                layers.push(transportation.clone());
                forwarding.register_handler(transportation.clone());
            }
        }

        // register per-source input element handlers
        for layer in &layers {
            if layer.as_natural_earth_processor().is_some() {
                let layer = layer.clone();
                forwarding.register_source_fn(NATURAL_EARTH_SOURCE, move |source, features| {
                    if let Some(p) = layer.as_natural_earth_processor() {
                        p.process_natural_earth(ExpressionInput::source_layer(source).unwrap_or(""), source, features);
                    }
                });
            }
            if layer.as_osm_water_polygon_processor().is_some() {
                let layer = layer.clone();
                forwarding.register_source_fn(WATER_POLYGON_SOURCE, move |source, features| {
                    if let Some(p) = layer.as_osm_water_polygon_processor() {
                        p.process_osm_water(source, features);
                    }
                });
            }
            if layer.as_lake_centerline_processor().is_some() {
                let layer = layer.clone();
                forwarding.register_source_fn(LAKE_CENTERLINE_SOURCE, move |source, features| {
                    if let Some(p) = layer.as_lake_centerline_processor() {
                        p.process_lake_centerline(source, features);
                    }
                });
            }
            if layer.as_osm_all_processor().is_some() {
                let layer = layer.clone();
                forwarding.register_source_fn(OSM_SOURCE, move |source, features| {
                    if let Some(p) = layer.as_osm_all_processor() {
                        p.process_all_osm(source, features);
                    }
                });
            }
        }

        // pre-process layers to build efficient indexes for matching OSM elements based on matching expressions
        // Map from imposm3 table to the layers that implement its handler.
        let osm_mappings = Arc::new(
            tables::MAPPINGS
                .map_results(|table| {
                    let handlers: Vec<Arc<dyn Layer>> =
                        layers.iter().filter(|l| table.is_handled_by(&***l)).cloned().collect();
                    // tables without handlers can be skipped entirely
                    (!handlers.is_empty()).then_some(RowDispatch { table: *table, handlers })
                })
                .simplify()
                .index_and_warn(),
        );
        let wikidata_mappings = tables::MAPPINGS
            .map_results(|table| {
                layers.iter().any(|l| table.is_handled_by(&**l) && !l.ignore_wikidata()).then_some(true)
            })
            .simplify()
            .index();

        // register a handler for all OSM elements that forwards to imposm3 "table row" handler methods
        // based on efficient pre-processed index
        if !osm_mappings.is_empty() {
            let mappings = osm_mappings.clone();
            forwarding.register_source_fn(OSM_SOURCE, move |source, features| {
                for m in mappings.get_matches_with_triggers(source) {
                    let dispatch = m.result;
                    let key = m.keys.first().map(|k| &**k).unwrap_or("");
                    for handler in &dispatch.handlers {
                        dispatch.table.dispatch(&**handler, source, key, features);
                    }
                }
            });
        }

        OpenMapTilesProfile { forwarding, osm_mappings, wikidata_mappings, layers }
    }

    /// Returns the imposm3 table rows that match an input element's tags.
    pub fn get_table_matches(&self, input: &dyn SourceFeature) -> Vec<Match<'_, RowDispatch>> {
        self.osm_mappings.get_matches_with_triggers(input)
    }

    /// The layers this profile delegates to.
    pub fn layers(&self) -> &[Arc<dyn Layer>] {
        &self.layers
    }

    /// The underlying forwarding profile.
    pub fn forwarding_profile(&self) -> &ForwardingProfile {
        &self.forwarding
    }

    fn wikidata_matches(&self, tags: &Tags, geometry_type: GeometryKind) -> bool {
        *self.wikidata_mappings.get_or_else(&TagsWithGeometryType { tags, geometry_type }, &false)
    }
}

/// Geometry type used to check whether an OSM element could appear in a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GeometryKind {
    Point,
    Line,
    Polygon,
}

/// A lightweight expression input over borrowed tags with a fixed geometry type (Java creates a `SimpleFeature` with
/// an empty geometry and copied tags).
struct TagsWithGeometryType<'a> {
    tags: &'a Tags,
    geometry_type: GeometryKind,
}

impl WithTags for TagsWithGeometryType<'_> {
    fn tags(&self) -> &Tags {
        self.tags
    }
}

impl WithGeometryType for TagsWithGeometryType<'_> {
    fn is_point(&self) -> bool {
        self.geometry_type == GeometryKind::Point
    }

    fn can_be_polygon(&self) -> bool {
        self.geometry_type == GeometryKind::Polygon
    }

    fn can_be_line(&self) -> bool {
        self.geometry_type == GeometryKind::Line
    }
}

impl ExpressionInput for TagsWithGeometryType<'_> {
    fn as_with_geometry_type(&self) -> Option<&dyn WithGeometryType> {
        Some(self)
    }
}

impl Profile for OpenMapTilesProfile {
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        self.forwarding.process_feature(source_feature, features)
    }

    fn preprocess_osm_node(&self, node: &Node) {
        self.forwarding.preprocess_osm_node(node)
    }

    fn preprocess_osm_way(&self, way: &Way) {
        self.forwarding.preprocess_osm_way(way)
    }

    fn split_osm_way_at_intersections(&self, way: &Way) -> bool {
        self.forwarding.split_osm_way_at_intersections(way)
    }

    fn preprocess_osm_relation(&self, relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
        self.forwarding.preprocess_osm_relation(relation)
    }

    fn release(&self) {
        self.forwarding.release()
    }

    fn post_process_layer_features(
        &self,
        layer: &str,
        zoom: u32,
        items: &mut Vec<vector_tile::Feature>,
    ) -> GeometryResult<()> {
        self.forwarding.post_process_layer_features(layer, zoom, items)
    }

    fn post_process_tile_features(&self, tile_coord: TileCoord, layers: &mut LayerFeatures) -> GeometryResult<()> {
        self.forwarding.post_process_tile_features(tile_coord, layers)
    }

    /*
     * Pass-through constants generated from the OpenMapTiles vector schema
     */

    fn name(&self) -> String {
        open_map_tiles_schema::NAME.to_string()
    }

    fn description(&self) -> Option<String> {
        Some(open_map_tiles_schema::DESCRIPTION.to_string())
    }

    fn attribution(&self) -> Option<String> {
        Some(open_map_tiles_schema::ATTRIBUTION.to_string())
    }

    fn version(&self) -> Option<String> {
        Some(open_map_tiles_schema::VERSION.to_string())
    }

    fn extra_archive_metadata(&self) -> BTreeMap<String, String> {
        self.forwarding.extra_archive_metadata()
    }

    fn cares_about_wikidata_translation(&self, elem: &OsmElement) -> bool {
        match elem {
            OsmElement::Node(node) => self.wikidata_matches(&node.tags, GeometryKind::Point),
            OsmElement::Way(way) => {
                self.wikidata_matches(&way.tags, GeometryKind::Polygon)
                    || self.wikidata_matches(&way.tags, GeometryKind::Line)
            }
            OsmElement::Relation(relation) => self.wikidata_matches(&relation.tags, GeometryKind::Polygon),
            OsmElement::Other(_) => false,
        }
    }

    fn finish(
        &self,
        source_name: &str,
        feature_collectors: &feature_collector::Factory,
        next: &mut FeatureConsumer<'_>,
    ) {
        self.forwarding.finish(source_name, feature_collectors, next)
    }

    fn cares_about_source(&self, name: &str) -> bool {
        self.forwarding.cares_about_source(name)
    }

    fn estimate_intermediate_disk_bytes(&self, osm_file_size: u64) -> u64 {
        // in late 2021, a 60gb OSM file used 200GB for intermediate storage
        osm_file_size * 200 / 60
    }

    fn estimate_output_bytes(&self, osm_file_size: u64) -> u64 {
        // in late 2021, a 60gb OSM file generated a 100GB output file
        osm_file_size * 100 / 60
    }

    fn estimate_ram_required(&self, osm_file_size: u64) -> u64 {
        // 20gb for a 67gb OSM file is safe, although less might be OK too
        osm_file_size * 20 / 67
    }

    fn cares_about(&self, input: &PartialInput) -> bool {
        self.forwarding.cares_about(input)
    }
}

#[cfg(test)]
mod tests {
    use planetiler_core::reader::osm::osm_element::{Node, OsmElement, Way};

    use super::*;
    use crate::test_support::new_profile;

    #[test]
    fn test_cares_about_wikidata() {
        let profile = new_profile();
        let mut node = Node::new(1, 1.0, 1.0);
        node.tags.insert("aeroway".into(), "gate".into());
        assert!(profile.cares_about_wikidata_translation(&OsmElement::Node(node.clone())));

        node.tags.insert("aeroway".into(), "other".into());
        assert!(!profile.cares_about_wikidata_translation(&OsmElement::Node(node)));
    }

    #[test]
    fn test_doesnt_care_about_wikidata_for_roads() {
        let profile = new_profile();
        let mut way = Way::new(1);
        way.tags.insert("highway".into(), "footway".into());
        assert!(!profile.cares_about_wikidata_translation(&OsmElement::Way(way)));
    }
}
