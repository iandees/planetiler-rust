//! Port of `ForwardingProfile.java`: a [`Profile`] that forwards each event to a set of registered [`Handler`]s
//! (i.e. one per layer) that care about it.
//!
//! # Rust design
//!
//! Java's abstract `ForwardingProfile` class becomes the concrete [`ForwardingProfile`] struct. It implements
//! [`Profile`] itself, so it can be used directly. A custom profile embeds one and forwards to it, overriding the
//! methods it wants (`name`, `attribution`, ...):
//!
//! ```ignore
//! struct MyProfile { forwarding: ForwardingProfile }
//! impl Profile for MyProfile {
//!     fn process_feature(&self, f: &dyn SourceFeature, c: &mut FeatureCollector<'_>) {
//!         self.forwarding.process_feature(f, c)
//!     }
//!     fn post_process_layer_features(&self, layer: &str, zoom: u32, items: &mut Vec<Feature>) -> GeometryResult<()> {
//!         self.forwarding.post_process_layer_features(layer, zoom, items)
//!     }
//!     // ... forward the other hooks you use, override name() etc.
//! }
//! ```
//!
//! Handlers implement [`Handler`] and expose the capabilities they support through the `as_*` accessors (Java uses
//! `instanceof` checks on sub-interfaces), for example:
//!
//! ```ignore
//! impl Handler for Water {
//!     fn name(&self) -> Option<&str> { Some("water") }
//!     fn as_feature_processor(&self) -> Option<&dyn FeatureProcessor> { Some(self) }
//!     fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> { Some(self) }
//! }
//! ```
//!
//! The `*_fn` helper functions ([`feature_processor_fn`], [`node_preprocessor_fn`], ...) wrap closures as handlers.
//!
//! Registration takes `&mut self` (profiles register handlers while being constructed); the index used to dispatch
//! [`Profile::process_feature`] is built lazily on first use and reset by every registration. Registering the same
//! `Arc` handler more than once (as a feature handler and/or source handler) only invokes it once per matching
//! feature, like Java where the same processor instance gets merged in the `MultiExpression`.
//!
//! Java's overridable `dependsOnLayer()` becomes [`ForwardingProfile::set_layer_dependencies`], which must be called
//! before registering handlers.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::config::planetiler_config::PlanetilerConfig;
use crate::expression::expression::{self, Expression, PartialInput};
use crate::expression::multi_expression::{self, Index, MultiExpression};
use crate::expression::simplifiable::Simplifiable;
use crate::feature_collector::{self, FeatureCollector};
use crate::geo::geometry_exception::GeometryResult;
use crate::geo::tile_coord::TileCoord;
use crate::profile::{FeatureConsumer, LayerFeatures, Profile};
use crate::reader::osm::osm_element::{Node, Relation, Way};
use crate::reader::osm::osm_relation_info::OsmRelationInfo;
use crate::reader::source_feature::SourceFeature;
use crate::vector_tile;

/// A handler that processes events (features, OSM preprocessing, post-processing, ...) for a [`ForwardingProfile`].
///
/// Override the `as_*` methods to return `Some(self)` for each capability this handler implements.
pub trait Handler: Send + Sync {
    /// Free any resources associated with this handler.
    fn release(&self) {}

    /// The output layer this handler is for (Java `HandlerForLayer`), used for `--only-layers`/`--exclude-layers`
    /// filtering and layer post-processing.
    fn name(&self) -> Option<&str> {
        None
    }

    fn as_feature_processor(&self) -> Option<&dyn FeatureProcessor> {
        None
    }

    fn as_osm_node_preprocessor(&self) -> Option<&dyn OsmNodePreprocessor> {
        None
    }

    fn as_osm_way_preprocessor(&self) -> Option<&dyn OsmWayPreprocessor> {
        None
    }

    fn as_osm_relation_preprocessor(&self) -> Option<&dyn OsmRelationPreprocessor> {
        None
    }

    fn as_finish_handler(&self) -> Option<&dyn FinishHandler> {
        None
    }

    /// Layer post processors must also return a [`Handler::name`] for the layer they post-process.
    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        None
    }

    fn as_tile_post_processor(&self) -> Option<&dyn TilePostProcessor> {
        None
    }
}

/// Handler that processes source features (Java `ForwardingProfile.FeatureProcessor`).
pub trait FeatureProcessor: Send + Sync {
    /// Generates output features for a source feature (see [`Profile::process_feature`]).
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>);

    /// Only invoke this processor for source features matching this expression. Defaults to all features.
    fn filter(&self) -> Expression {
        Expression::TRUE
    }
}

/// Handler that is invoked once after all elements from a source have been processed.
pub trait FinishHandler: Send + Sync {
    /// See [`Profile::finish`].
    fn finish(
        &self,
        source_name: &str,
        feature_collectors: &feature_collector::Factory,
        emit: &mut FeatureConsumer<'_>,
    );
}

/// Handler that preprocesses OSM nodes during pass 1.
pub trait OsmNodePreprocessor: Send + Sync {
    fn preprocess_osm_node(&self, node: &Node);
}

/// Handler that preprocesses OSM ways during pass 1.
pub trait OsmWayPreprocessor: Send + Sync {
    fn preprocess_osm_way(&self, way: &Way);
}

/// Handler that extracts information from OSM relations (see [`Profile::preprocess_osm_relation`]).
pub trait OsmRelationPreprocessor: Send + Sync {
    fn preprocess_osm_relation(&self, relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>>;
}

/// Handler that post-processes the features of the layer named [`Handler::name`] in each tile (see
/// [`Profile::post_process_layer_features`]).
pub trait LayerPostProcessor: Send + Sync {
    fn post_process(&self, zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()>;
}

/// Handler that post-processes all layers in a tile (see [`Profile::post_process_tile_features`]).
pub trait TilePostProcessor: Send + Sync {
    fn post_process_tile(&self, tile_coord: TileCoord, layers: &mut LayerFeatures) -> GeometryResult<()>;
}

/// A [`Profile`] that forwards each event to registered [`Handler`]s. See the [module docs](self).
#[derive(Default)]
pub struct ForwardingProfile {
    handlers: Vec<Arc<dyn Handler>>,
    osm_node_preprocessors: Vec<Arc<dyn Handler>>,
    osm_way_preprocessors: Vec<Arc<dyn Handler>>,
    osm_relation_preprocessors: Vec<Arc<dyn Handler>>,
    finish_handlers: Vec<Arc<dyn Handler>>,
    layer_post_processors: HashMap<String, Vec<Arc<dyn Handler>>>,
    tile_post_processors: Vec<Arc<dyn Handler>>,
    /// Distinct feature processors, indexed by the `usize` results of `source_element_processors`.
    processors: Vec<Arc<dyn Handler>>,
    source_element_processors: Vec<multi_expression::Entry<usize>>,
    only_layers: Vec<String>,
    exclude_layers: Vec<String>,
    layer_dependencies: HashMap<String, Vec<String>>,
    indexed_source_element_processors: OnceLock<Index<usize>>,
}

impl std::fmt::Debug for ForwardingProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwardingProfile")
            .field("handlers", &self.handlers.len())
            .field("only_layers", &self.only_layers)
            .field("exclude_layers", &self.exclude_layers)
            .finish()
    }
}

fn same_handler(a: &Arc<dyn Handler>, b: &Arc<dyn Handler>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}

impl ForwardingProfile {
    /// A forwarding profile with no layer filters.
    pub fn new() -> ForwardingProfile {
        ForwardingProfile::default()
    }

    /// A forwarding profile that only includes layers from the `only_layers` argument and excludes layers from the
    /// `exclude_layers` argument.
    pub fn with_config(config: &PlanetilerConfig) -> ForwardingProfile {
        ForwardingProfile {
            only_layers: config.arguments.get_list::<&str>("only_layers", "Include only certain layers", &[]),
            exclude_layers: config.arguments.get_list::<&str>("exclude_layers", "Exclude certain layers", &[]),
            ..Default::default()
        }
    }

    /// Same as [`ForwardingProfile::with_config`] and registers `handlers`.
    pub fn with_config_and_handlers(
        config: &PlanetilerConfig,
        handlers: impl IntoIterator<Item = Arc<dyn Handler>>,
    ) -> ForwardingProfile {
        let mut result = Self::with_config(config);
        for handler in handlers {
            result.register_handler(handler);
        }
        result
    }

    /// A forwarding profile with no layer filters that registers `handlers`.
    pub fn with_handlers(handlers: impl IntoIterator<Item = Arc<dyn Handler>>) -> ForwardingProfile {
        let mut result = Self::new();
        for handler in handlers {
            result.register_handler(handler);
        }
        result
    }

    /// Indicates that a layer (key) depends on other layers (values): when a layer is included via
    /// `--only-layers`/not excluded with `--exclude-layers`, the layers it depends on are processed too (Java's
    /// overridable `dependsOnLayer()`). Must be called before registering handlers.
    pub fn set_layer_dependencies(&mut self, dependencies: HashMap<String, Vec<String>>) -> &mut Self {
        self.layer_dependencies = dependencies;
        self
    }

    /// The layer dependencies set with [`ForwardingProfile::set_layer_dependencies`].
    pub fn depends_on_layer(&self) -> &HashMap<String, Vec<String>> {
        &self.layer_dependencies
    }

    /// Returns true if `layer` should be included in the output based on the layer filters and dependencies of
    /// already-registered layer handlers.
    pub fn cares_about_layer(&self, layer: &str) -> bool {
        let included = (self.only_layers.is_empty() || self.only_layers.iter().any(|l| l == layer))
            && !self.exclude_layers.iter().any(|l| l == layer);
        included
            || self.handlers.iter().filter_map(|h| h.name()).filter(|name| *name != layer).any(|existing| {
                self.layer_dependencies.get(existing).is_some_and(|deps| deps.iter().any(|d| d == layer))
            })
    }

    /// Returns true if `handler` is not for a layer, or is for a layer that [`ForwardingProfile::cares_about_layer`].
    pub fn cares_about_handler(&self, handler: &dyn Handler) -> bool {
        handler.name().is_none_or(|name| self.cares_about_layer(name))
    }

    fn add_processor(&mut self, processor: Arc<dyn Handler>, expression: Expression) {
        let fp = processor.as_feature_processor().expect("handler must implement FeatureProcessor");
        let expression = expression::and([expression, fp.filter()]);
        let idx = match self.processors.iter().position(|p| same_handler(p, &processor)) {
            Some(idx) => idx,
            None => {
                self.processors.push(processor);
                self.processors.len() - 1
            }
        };
        self.source_element_processors.push(multi_expression::entry(idx, expression));
        self.indexed_source_element_processors = OnceLock::new();
    }

    /// Registers a feature processor handler (one that returns `Some` from [`Handler::as_feature_processor`]) for
    /// features from `source` that match its [`FeatureProcessor::filter`].
    ///
    /// Panics if `processor` is not a feature processor.
    pub fn register_source_handler(&mut self, source: &str, processor: Arc<dyn Handler>) {
        if !self.cares_about_handler(&*processor) {
            return;
        }
        self.add_processor(processor, expression::match_source(source));
    }

    /// Registers a closure to process features from `source`.
    pub fn register_source_fn(
        &mut self,
        source: &str,
        f: impl Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync + 'static,
    ) {
        self.register_source_handler(source, feature_processor_fn(f));
    }

    /// Registers a feature processor handler for all features that match its [`FeatureProcessor::filter`] from any
    /// source.
    ///
    /// Panics if `processor` is not a feature processor.
    pub fn register_feature_handler(&mut self, processor: Arc<dyn Handler>) {
        if !self.cares_about_handler(&*processor) {
            return;
        }
        self.add_processor(processor, Expression::TRUE);
    }

    /// Registers a closure to process features from any source.
    pub fn register_feature_fn(
        &mut self,
        f: impl Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync + 'static,
    ) {
        self.register_feature_handler(feature_processor_fn(f));
    }

    /// Registers a handler for every capability it implements (feature processing from all sources, OSM
    /// preprocessing, finish, layer and tile post-processing).
    ///
    /// Handlers for layers that [`ForwardingProfile::cares_about_layer`] returns false for are ignored.
    pub fn register_handler(&mut self, handler: Arc<dyn Handler>) {
        if !self.cares_about_handler(&*handler) {
            return;
        }
        self.handlers.push(handler.clone());
        if handler.as_osm_node_preprocessor().is_some() {
            self.osm_node_preprocessors.push(handler.clone());
        }
        if handler.as_osm_way_preprocessor().is_some() {
            self.osm_way_preprocessors.push(handler.clone());
        }
        if handler.as_osm_relation_preprocessor().is_some() {
            self.osm_relation_preprocessors.push(handler.clone());
        }
        if handler.as_finish_handler().is_some() {
            self.finish_handlers.push(handler.clone());
        }
        if handler.as_layer_post_processor().is_some() {
            let name = handler.name().expect("layer post processors must have a name").to_string();
            self.layer_post_processors.entry(name).or_default().push(handler.clone());
        }
        if handler.as_tile_post_processor().is_some() {
            self.tile_post_processors.push(handler.clone());
        }
        if handler.as_feature_processor().is_some() {
            self.register_feature_handler(handler);
        }
    }

    /// All registered handlers.
    pub fn handlers(&self) -> &[Arc<dyn Handler>] {
        &self.handlers
    }

    fn handler_index(&self) -> &Index<usize> {
        self.indexed_source_element_processors
            .get_or_init(|| MultiExpression::of(self.source_element_processors.clone()).index())
    }
}

impl Profile for ForwardingProfile {
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        // delegate source feature processing to each handler for that source
        for idx in self.handler_index().get_matches(source_feature) {
            if let Some(fp) = self.processors[*idx].as_feature_processor() {
                fp.process_feature(source_feature, features);
            }
        }
    }

    fn preprocess_osm_node(&self, node: &Node) {
        for h in &self.osm_node_preprocessors {
            if let Some(p) = h.as_osm_node_preprocessor() {
                p.preprocess_osm_node(node);
            }
        }
    }

    fn preprocess_osm_way(&self, way: &Way) {
        for h in &self.osm_way_preprocessors {
            if let Some(p) = h.as_osm_way_preprocessor() {
                p.preprocess_osm_way(way);
            }
        }
    }

    fn preprocess_osm_relation(&self, relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
        // delegate OSM relation pre-processing to each layer that implements it
        let mut result: Option<Vec<Arc<dyn OsmRelationInfo>>> = None;
        for h in &self.osm_relation_preprocessors {
            if let Some(this_result) =
                h.as_osm_relation_preprocessor().and_then(|p| p.preprocess_osm_relation(relation))
            {
                result.get_or_insert_with(Vec::new).extend(this_result);
            }
        }
        result
    }

    fn release(&self) {
        for h in &self.handlers {
            h.release();
        }
    }

    fn post_process_layer_features(
        &self,
        layer: &str,
        zoom: u32,
        items: &mut Vec<vector_tile::Feature>,
    ) -> GeometryResult<()> {
        // delegate feature post-processing to each handler for that layer
        if let Some(handlers) = self.layer_post_processors.get(layer) {
            for h in handlers {
                if let Some(p) = h.as_layer_post_processor() {
                    p.post_process(zoom, items)?;
                }
            }
        }
        Ok(())
    }

    fn post_process_tile_features(&self, tile_coord: TileCoord, layers: &mut LayerFeatures) -> GeometryResult<()> {
        for h in &self.tile_post_processors {
            // TODO catch failures to isolate from other tile postprocessors?
            if let Some(p) = h.as_tile_post_processor() {
                p.post_process_tile(tile_coord, layers)?;
            }
        }
        Ok(())
    }

    fn finish(
        &self,
        source_name: &str,
        feature_collectors: &feature_collector::Factory,
        next: &mut FeatureConsumer<'_>,
    ) {
        // delegate finish handling to every handler that implements it
        for h in &self.finish_handlers {
            if let Some(f) = h.as_finish_handler() {
                f.finish(source_name, feature_collectors, next);
            }
        }
    }

    fn cares_about_source(&self, name: &str) -> bool {
        self.cares_about(&PartialInput::of_source(name))
    }

    fn cares_about(&self, input: &PartialInput) -> bool {
        self.source_element_processors
            .iter()
            .any(|e| e.expression.partial_evaluate(input).simplify() != Expression::FALSE)
    }
}

// Closure-backed handlers.

type ProcessFn = dyn Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync;

struct FnFeatureProcessor {
    name: Option<String>,
    filter: Expression,
    f: Box<ProcessFn>,
}

impl FeatureProcessor for FnFeatureProcessor {
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        (self.f)(source_feature, features)
    }

    fn filter(&self) -> Expression {
        self.filter.clone()
    }
}

impl Handler for FnFeatureProcessor {
    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn as_feature_processor(&self) -> Option<&dyn FeatureProcessor> {
        Some(self)
    }
}

/// Wraps a closure as a feature processor handler that processes every feature.
pub fn feature_processor_fn(
    f: impl Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync + 'static,
) -> Arc<dyn Handler> {
    Arc::new(FnFeatureProcessor { name: None, filter: Expression::TRUE, f: Box::new(f) })
}

/// Wraps a closure as a feature processor handler that only processes features matching `filter`.
pub fn feature_processor_fn_with_filter(
    filter: Expression,
    f: impl Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync + 'static,
) -> Arc<dyn Handler> {
    Arc::new(FnFeatureProcessor { name: None, filter, f: Box::new(f) })
}

/// Wraps a closure as a feature processor handler for the output layer `name` (Java `HandlerForLayer` +
/// `FeatureProcessor`).
pub fn layer_feature_processor_fn(
    name: &str,
    f: impl Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync + 'static,
) -> Arc<dyn Handler> {
    Arc::new(FnFeatureProcessor { name: Some(name.to_string()), filter: Expression::TRUE, f: Box::new(f) })
}

struct FnNodePreprocessor<F>(F);

impl<F: Fn(&Node) + Send + Sync> OsmNodePreprocessor for FnNodePreprocessor<F> {
    fn preprocess_osm_node(&self, node: &Node) {
        (self.0)(node)
    }
}

impl<F: Fn(&Node) + Send + Sync> Handler for FnNodePreprocessor<F> {
    fn as_osm_node_preprocessor(&self) -> Option<&dyn OsmNodePreprocessor> {
        Some(self)
    }
}

/// Wraps a closure as an OSM node preprocessor handler.
pub fn node_preprocessor_fn(f: impl Fn(&Node) + Send + Sync + 'static) -> Arc<dyn Handler> {
    Arc::new(FnNodePreprocessor(f))
}

struct FnWayPreprocessor<F>(F);

impl<F: Fn(&Way) + Send + Sync> OsmWayPreprocessor for FnWayPreprocessor<F> {
    fn preprocess_osm_way(&self, way: &Way) {
        (self.0)(way)
    }
}

impl<F: Fn(&Way) + Send + Sync> Handler for FnWayPreprocessor<F> {
    fn as_osm_way_preprocessor(&self) -> Option<&dyn OsmWayPreprocessor> {
        Some(self)
    }
}

/// Wraps a closure as an OSM way preprocessor handler.
pub fn way_preprocessor_fn(f: impl Fn(&Way) + Send + Sync + 'static) -> Arc<dyn Handler> {
    Arc::new(FnWayPreprocessor(f))
}

struct FnRelationPreprocessor<F>(F);

impl<F: Fn(&Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> + Send + Sync> OsmRelationPreprocessor
    for FnRelationPreprocessor<F>
{
    fn preprocess_osm_relation(&self, relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
        (self.0)(relation)
    }
}

impl<F: Fn(&Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> + Send + Sync> Handler for FnRelationPreprocessor<F> {
    fn as_osm_relation_preprocessor(&self) -> Option<&dyn OsmRelationPreprocessor> {
        Some(self)
    }
}

/// Wraps a closure as an OSM relation preprocessor handler.
pub fn relation_preprocessor_fn(
    f: impl Fn(&Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> + Send + Sync + 'static,
) -> Arc<dyn Handler> {
    Arc::new(FnRelationPreprocessor(f))
}

struct FnFinishHandler<F>(F);

impl<F: Fn(&str, &feature_collector::Factory, &mut FeatureConsumer<'_>) + Send + Sync> FinishHandler
    for FnFinishHandler<F>
{
    fn finish(
        &self,
        source_name: &str,
        feature_collectors: &feature_collector::Factory,
        emit: &mut FeatureConsumer<'_>,
    ) {
        (self.0)(source_name, feature_collectors, emit)
    }
}

impl<F: Fn(&str, &feature_collector::Factory, &mut FeatureConsumer<'_>) + Send + Sync> Handler for FnFinishHandler<F> {
    fn as_finish_handler(&self) -> Option<&dyn FinishHandler> {
        Some(self)
    }
}

/// Wraps a closure as a finish handler.
pub fn finish_handler_fn(
    f: impl Fn(&str, &feature_collector::Factory, &mut FeatureConsumer<'_>) + Send + Sync + 'static,
) -> Arc<dyn Handler> {
    Arc::new(FnFinishHandler(f))
}

type LayerPostFn = dyn Fn(u32, &mut Vec<vector_tile::Feature>) -> GeometryResult<()> + Send + Sync;

struct FnLayerPostProcessor {
    name: String,
    f: Box<LayerPostFn>,
}

impl LayerPostProcessor for FnLayerPostProcessor {
    fn post_process(&self, zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        (self.f)(zoom, items)
    }
}

impl Handler for FnLayerPostProcessor {
    fn name(&self) -> Option<&str> {
        Some(&self.name)
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }
}

/// Wraps a closure as a post-processor for features in layer `name`.
pub fn layer_post_processor_fn(
    name: &str,
    f: impl Fn(u32, &mut Vec<vector_tile::Feature>) -> GeometryResult<()> + Send + Sync + 'static,
) -> Arc<dyn Handler> {
    Arc::new(FnLayerPostProcessor { name: name.to_string(), f: Box::new(f) })
}

struct FnTilePostProcessor<F>(F);

impl<F: Fn(TileCoord, &mut LayerFeatures) -> GeometryResult<()> + Send + Sync> TilePostProcessor
    for FnTilePostProcessor<F>
{
    fn post_process_tile(&self, tile_coord: TileCoord, layers: &mut LayerFeatures) -> GeometryResult<()> {
        (self.0)(tile_coord, layers)
    }
}

impl<F: Fn(TileCoord, &mut LayerFeatures) -> GeometryResult<()> + Send + Sync> Handler for FnTilePostProcessor<F> {
    fn as_tile_post_processor(&self) -> Option<&dyn TilePostProcessor> {
        Some(self)
    }
}

/// Wraps a closure as a tile post-processor.
pub fn tile_post_processor_fn(
    f: impl Fn(TileCoord, &mut LayerFeatures) -> GeometryResult<()> + Send + Sync + 'static,
) -> Arc<dyn Handler> {
    Arc::new(FnTilePostProcessor(f))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use parking_lot::Mutex;

    use super::*;
    use crate::config::arguments::Arguments;
    use crate::feature_collector::test_support::{assert_submap, to_map};
    use crate::geo::geo_utils;
    use crate::reader::simple_feature::SimpleFeature;
    use crate::stats::stats::in_memory;
    use crate::tags;
    use crate::value::Tags;

    fn factory() -> feature_collector::Factory {
        feature_collector::Factory::new(PlanetilerConfig::defaults(), in_memory())
    }

    /// Port of `TestUtils.processSourceFeature` + `toMap` at z14.
    fn process_source_feature(feature: &dyn SourceFeature, profile: &dyn Profile) -> Vec<Tags> {
        let factory = factory();
        let mut collector = factory.get(feature);
        profile.process_feature(feature, &mut collector);
        collector.iter().map(|f| to_map(f, 14)).collect()
    }

    fn test_features(profile: &ForwardingProfile, expected: &[Tags], feature: &dyn SourceFeature) {
        let actual = process_source_feature(feature, profile);
        assert_eq!(expected.len(), actual.len(), "size: {actual:?}");
        for (e, a) in expected.iter().zip(&actual) {
            assert_submap(e, a);
        }
    }

    fn feature(tags: Tags, source: &str, layer: Option<&str>) -> SimpleFeature {
        SimpleFeature::create(geo_utils::empty_point(), tags, source, layer, 1)
    }

    fn layer(name: &str) -> Tags {
        tags! {"_layer" => name}
    }

    #[test]
    fn test_preprocess_osm_node() {
        let mut profile = ForwardingProfile::new();
        let node1 = Node::new(1, 2.0, 3.0);
        let node2 = Node::new(2, 3.0, 4.0);
        let called_with = Arc::new(Mutex::new(Vec::new()));
        let c = called_with.clone();
        profile.register_handler(node_preprocessor_fn(move |n| c.lock().push(n.clone())));
        profile.preprocess_osm_node(&node1);
        assert_eq!(vec![node1.clone()], *called_with.lock());

        let called_with2 = Arc::new(Mutex::new(Vec::new()));
        let c = called_with2.clone();
        profile.register_handler(node_preprocessor_fn(move |n| c.lock().push(n.clone())));
        profile.preprocess_osm_node(&node2);
        assert_eq!(vec![node1, node2.clone()], *called_with.lock());
        assert_eq!(vec![node2], *called_with2.lock());
    }

    #[test]
    fn test_preprocess_osm_way() {
        let mut profile = ForwardingProfile::new();
        let way1 = Way::new(1);
        let way2 = Way::new(2);
        let called_with = Arc::new(Mutex::new(Vec::new()));
        let c = called_with.clone();
        profile.register_handler(way_preprocessor_fn(move |w| c.lock().push(w.clone())));
        profile.preprocess_osm_way(&way1);
        assert_eq!(vec![way1.clone()], *called_with.lock());

        let called_with2 = Arc::new(Mutex::new(Vec::new()));
        let c = called_with2.clone();
        profile.register_handler(way_preprocessor_fn(move |w| c.lock().push(w.clone())));
        profile.preprocess_osm_way(&way2);
        assert_eq!(vec![way1, way2.clone()], *called_with.lock());
        assert_eq!(vec![way2], *called_with2.lock());
    }

    #[derive(Debug, PartialEq)]
    struct RelA(i64);
    impl OsmRelationInfo for RelA {
        fn id(&self) -> i64 {
            self.0
        }
    }

    #[derive(Debug, PartialEq)]
    struct RelB(i64);
    impl OsmRelationInfo for RelB {
        fn id(&self) -> i64 {
            self.0
        }
    }

    fn describe(rels: Option<Vec<Arc<dyn OsmRelationInfo>>>) -> Option<Vec<String>> {
        rels.map(|r| r.iter().map(|r| format!("{r:?}")).collect())
    }

    #[test]
    fn test_preprocess_osm_relation() {
        let mut profile = ForwardingProfile::new();
        assert!(profile.preprocess_osm_relation(&Relation::new(1)).is_none());
        profile.register_handler(relation_preprocessor_fn(|r| {
            Some(vec![Arc::new(RelA(r.id)) as Arc<dyn OsmRelationInfo>])
        }));
        assert_eq!(Some(vec!["RelA(1)".to_string()]), describe(profile.preprocess_osm_relation(&Relation::new(1))));
        profile.register_handler(relation_preprocessor_fn(|_| None));
        profile.register_handler(relation_preprocessor_fn(|r| {
            Some(vec![Arc::new(RelB(r.id)) as Arc<dyn OsmRelationInfo>])
        }));
        let result = profile.preprocess_osm_relation(&Relation::new(1)).unwrap();
        assert_eq!(Some(&RelA(1)), result[0].downcast_ref::<RelA>());
        assert_eq!(Some(&RelB(1)), result[1].downcast_ref::<RelB>());
        assert_eq!(2, result.len());
    }

    #[test]
    fn test_process_feature() {
        let mut profile = ForwardingProfile::new();
        let a = feature(tags! {}, "srca", None);
        let b = feature(tags! {}, "srcb", None);
        test_features(&profile, &[], &a);
        test_features(&profile, &[], &b);

        profile.register_source_fn("srca", |_, f| {
            f.point("a");
        });
        test_features(&profile, &[layer("a")], &a);
        test_features(&profile, &[], &b);

        profile.register_source_fn("srcb", |_, f| {
            f.point("b");
        });
        test_features(&profile, &[layer("a")], &a);
        test_features(&profile, &[layer("b")], &b);

        profile.register_source_fn("srca", |_, f| {
            f.point("a2");
        });
        test_features(&profile, &[layer("a"), layer("a2")], &a);
        test_features(&profile, &[layer("b")], &b);
    }

    #[test]
    fn test_process_feature_with_filter() {
        let mut profile = ForwardingProfile::new();
        let a = feature(tags! {"key" => "value"}, "srca", None);
        let b = feature(tags! {}, "srcb", None);
        profile.register_source_handler(
            "srca",
            feature_processor_fn_with_filter(expression::match_any("key", ["value"]), |_, f| {
                f.point("a");
            }),
        );
        test_features(&profile, &[layer("a")], &a);
        test_features(&profile, &[], &b);
    }

    #[test]
    fn test_finish_handler() {
        let mut profile = ForwardingProfile::new();
        let factory = factory();
        let finished = Arc::new(Mutex::new(BTreeSet::new()));
        profile.finish("source", &factory, &mut |_| {});
        assert!(finished.lock().is_empty());

        let f = finished.clone();
        profile.register_handler(finish_handler_fn(move |name, _, _| {
            f.lock().insert(format!("1-{name}"));
        }));
        profile.finish("source", &factory, &mut |_| {});
        assert_eq!(BTreeSet::from(["1-source".to_string()]), *finished.lock());

        finished.lock().clear();
        let f = finished.clone();
        profile.register_handler(finish_handler_fn(move |name, _, _| {
            f.lock().insert(format!("2-{name}"));
        }));
        profile.finish("source2", &factory, &mut |_| {});
        assert_eq!(BTreeSet::from(["1-source2".to_string(), "2-source2".to_string()]), *finished.lock());
    }

    fn vt_feature() -> vector_tile::Feature {
        vector_tile::Feature::new(
            "layer",
            1,
            vector_tile::encode_geometry(&geo_types::Geometry::Point(geo_utils::point(0.0, 0.0))),
            Tags::new(),
        )
    }

    fn post_layer(
        profile: &ForwardingProfile,
        layer: &str,
        items: Vec<vector_tile::Feature>,
    ) -> Vec<vector_tile::Feature> {
        let mut items = items;
        profile.post_process_layer_features(layer, 0, &mut items).unwrap();
        items
    }

    #[test]
    fn test_layer_post_processor() {
        let mut profile = ForwardingProfile::new();
        let feature = vt_feature();
        assert_eq!(vec![feature.clone()], post_layer(&profile, "layer", vec![feature.clone()]));

        // no-op
        profile.register_handler(layer_post_processor_fn("a", |_, _| Ok(())));
        assert_eq!(vec![feature.clone()], post_layer(&profile, "a", vec![feature.clone()]));

        // allow mutations on initial input
        profile.register_handler(layer_post_processor_fn("a", |_, items| {
            let first = items[0].clone();
            items[0] = first;
            Ok(())
        }));
        assert_eq!(vec![feature.clone()], post_layer(&profile, "a", vec![feature.clone()]));

        // empty list removes
        profile.register_handler(layer_post_processor_fn("a", |_, items| {
            items.clear();
            Ok(())
        }));
        assert_eq!(Vec::<vector_tile::Feature>::new(), post_layer(&profile, "a", vec![feature.clone()]));
        // doesn't touch elements in another layer
        assert_eq!(vec![feature.clone()], post_layer(&profile, "b", vec![feature.clone()]));

        // allow mutations on subsequent input
        let f2 = feature.clone();
        profile.register_handler(layer_post_processor_fn("a", move |_, items| {
            items.push(f2.clone());
            items.pop();
            Ok(())
        }));
        assert_eq!(Vec::<vector_tile::Feature>::new(), post_layer(&profile, "a", vec![feature.clone()]));

        // 2 handlers for same layer run one after another
        let skip1 = layer_post_processor_fn("b", |_, items| {
            if !items.is_empty() {
                items.remove(0);
            }
            Ok(())
        });
        profile.register_handler(skip1.clone());
        profile.register_handler(skip1);
        profile.register_handler(layer_post_processor_fn("b", |_, _| Ok(())));
        let four = vec![feature.clone(); 4];
        assert_eq!(vec![feature.clone(); 2], post_layer(&profile, "b", four.clone()));
        assert_eq!(four.clone(), post_layer(&profile, "c", four));
    }

    fn post_tile(profile: &ForwardingProfile, layers: LayerFeatures) -> LayerFeatures {
        let mut layers = layers;
        profile.post_process_tile_features(TileCoord::of_xyz(0, 0, 0), &mut layers).unwrap();
        layers
    }

    fn layers(items: &[(&str, Vec<vector_tile::Feature>)]) -> LayerFeatures {
        items.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn test_tile_post_processor() {
        let mut profile = ForwardingProfile::new();
        let feature = vt_feature();
        assert_eq!(
            layers(&[("layer", vec![feature.clone()])]),
            post_tile(&profile, layers(&[("layer", vec![feature.clone()])]))
        );

        // no-op
        profile.register_handler(tile_post_processor_fn(|_, _| Ok(())));
        assert_eq!(
            layers(&[("a", vec![feature.clone()])]),
            post_tile(&profile, layers(&[("a", vec![feature.clone()])]))
        );

        // allow mutation on input
        profile.register_handler(tile_post_processor_fn(|_, layers| {
            if let Some(list) = layers.get_mut("a") {
                let item = list[0].clone();
                list[0] = item;
            }
            Ok(())
        }));
        assert_eq!(
            layers(&[("a", vec![feature.clone()])]),
            post_tile(&profile, layers(&[("a", vec![feature.clone()])]))
        );

        // empty map removes
        profile.register_handler(tile_post_processor_fn(|_, layers| {
            layers.clear();
            Ok(())
        }));
        assert_eq!(LayerFeatures::new(), post_tile(&profile, layers(&[("a", vec![feature.clone()])])));

        // allow mutation on subsequent inputs
        profile.register_handler(tile_post_processor_fn(|_, layers| {
            layers.insert("a".into(), vec![]);
            layers.remove("a");
            Ok(())
        }));
        assert_eq!(LayerFeatures::new(), post_tile(&profile, layers(&[("a", vec![feature.clone()])])));

        // also touches elements in another layer
        assert_eq!(LayerFeatures::new(), post_tile(&profile, layers(&[("b", vec![feature.clone()])])));
    }

    #[test]
    fn test_stacked_tile_post_processors() {
        let mut profile = ForwardingProfile::new();
        let feature = vt_feature();
        let skip1 = tile_post_processor_fn(|_, layers| {
            for items in layers.values_mut() {
                if !items.is_empty() {
                    items.remove(0);
                }
            }
            Ok(())
        });
        profile.register_handler(skip1.clone());
        profile.register_handler(skip1);
        profile.register_handler(tile_post_processor_fn(|_, _| Ok(())));
        assert_eq!(
            layers(&[("b", vec![feature.clone(); 2])]),
            post_tile(&profile, layers(&[("b", vec![feature.clone(); 4])]))
        );
        assert_eq!(
            layers(&[("c", vec![feature.clone(); 2])]),
            post_tile(&profile, layers(&[("c", vec![feature.clone(); 4])]))
        );
    }

    #[test]
    fn test_cares_about_source() {
        let mut profile = ForwardingProfile::new();
        profile.register_source_fn("a", |_, _| {});
        assert!(profile.cares_about_source("a"));
        assert!(!profile.cares_about_source("b"));

        profile.register_source_fn("b", |_, _| {});
        assert!(profile.cares_about_source("a"));
        assert!(profile.cares_about_source("b"));
        assert!(!profile.cares_about_source("c"));

        profile.register_handler(feature_processor_fn_with_filter(expression::match_source("c"), |_, _| {}));
        assert!(profile.cares_about_source("a"));
        assert!(profile.cares_about_source("b"));
        assert!(profile.cares_about_source("c"));
        assert!(!profile.cares_about_source("d"));

        profile.register_feature_fn(|_, _| {});
        assert!(profile.cares_about_source("d"));
        assert!(profile.cares_about_source("e"));
    }

    #[test]
    fn register_any_source_feature_handler() {
        let mut profile = ForwardingProfile::new();
        let a = feature(tags! {}, "srca", None);
        let b = feature(tags! {}, "srcb", None);
        test_features(&profile, &[], &a);
        test_features(&profile, &[], &b);
        profile.register_feature_fn(|_, f| {
            f.point("a");
        });
        test_features(&profile, &[layer("a")], &a);
        test_features(&profile, &[layer("a")], &b);
    }

    #[test]
    fn register_handler_with_filter() {
        let mut profile = ForwardingProfile::new();
        let a = feature(tags! {"key" => "value"}, "srca", None);
        let b = feature(tags! {}, "srcb", None);
        test_features(&profile, &[], &a);
        test_features(&profile, &[], &b);
        profile.register_feature_handler(feature_processor_fn_with_filter(
            expression::match_any("key", ["value"]),
            |_, f| {
                f.point("a");
            },
        ));
        test_features(&profile, &[layer("a")], &a);
        test_features(&profile, &[], &b);
    }

    #[test]
    fn register_handler_twice() {
        let mut profile = ForwardingProfile::new();
        let a = feature(tags! {}, "source", None);
        test_features(&profile, &[], &a);
        let processor = feature_processor_fn(|_, f| {
            f.point("a");
        });
        profile.register_handler(processor.clone());
        profile.register_source_handler("source", processor);
        test_features(&profile, &[layer("a")], &a);
    }

    fn config(args: &str) -> PlanetilerConfig {
        let args: Vec<&str> = args.split(' ').collect();
        PlanetilerConfig::from(&Arguments::from_args(&args)).unwrap()
    }

    fn processor(name: &str) -> Arc<dyn Handler> {
        let n = name.to_string();
        layer_feature_processor_fn(name, move |_, f| {
            f.point(&n);
        })
    }

    #[test]
    fn test_layer_cli_arg_filter() {
        for args in ["--only-layers=water", "--exclude-layers=land", "--exclude-layers=land --only-layers=water,land"] {
            let mut profile = ForwardingProfile::with_config(&config(args));
            let a = feature(tags! {"key" => "value"}, "source", Some("source layer"));
            profile.register_handler(processor("water"));
            profile.register_handler(processor("land"));
            test_features(&profile, &[layer("water")], &a);
        }
    }

    #[test]
    fn test_layer_with_deps_cli_arg_filter() {
        let cases = [
            ("--only-layers=water", "water"),
            ("--exclude-layers=land,transportation,transportation_name", "water"),
            // transportation excluded but transportation_name NOT => transportation will be processed
            ("--exclude-layers=land,transportation", "water transportation_name transportation"),
            ("--exclude-layers=land,transportation_name", "water transportation"),
            ("--exclude-layers=land --only-layers=water,land", "water"),
            (
                "--exclude-layers=transportation --only-layers=water,transportation,transportation_name",
                "water transportation_name transportation",
            ),
            (
                "--exclude-layers=transportation_name --only-layers=water,transportation,transportation_name",
                "water transportation",
            ),
            (
                "--exclude-layers=transportation,transportation_name --only-layers=water,transportation,transportation_name",
                "water",
            ),
            (
                "--exclude-layers=transportation --only-layers=water,transportation_name",
                "water transportation_name transportation",
            ),
            ("--exclude-layers=transportation_name --only-layers=water,transportation", "water transportation"),
        ];
        for (args, expected_layers) in cases {
            let mut profile = ForwardingProfile::with_config(&config(args));
            profile.set_layer_dependencies(HashMap::from([(
                "transportation_name".to_string(),
                vec!["transportation".to_string()],
            )]));
            let a = feature(tags! {"key" => "value"}, "source", Some("source layer"));
            // Java uses records for these processors so equal instances get merged in the MultiExpression; Rust
            // dedupes by handler identity so reuse the same Arc
            let transportation = processor("transportation");
            profile.register_handler(processor("water"));
            profile.register_handler(transportation.clone());
            profile.register_handler(processor("transportation_name"));
            profile.register_handler(processor("land"));
            // profiles like OpenMapTiles will try to add "transportation" once again to cover for dependency
            profile.register_handler(transportation);
            let expected: Vec<Tags> = expected_layers.split(' ').map(layer).collect();
            test_features(&profile, &expected, &a);
        }
    }
}
