//! Port of `util/LayerAttrStats.java`.
//!
//! Tracks the feature attributes and zoom range of each layer to populate the archive output metadata.
//!
//! Matches the MBTiles spec for `vector_layers`, but can be reused by other archive formats. To minimize overhead of
//! stat collection, each updating thread should call [`LayerAttrStats::handler_for_thread`] first to get a
//! thread-local handler that can update stats without contention.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use indexmap::IndexMap;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use serde::{Deserialize, Serialize};

use crate::value::Value;

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static HANDLERS: RefCell<HashMap<usize, Updater>> = RefCell::new(HashMap::new());
}

/// Thread-safe collector of layer attribute statistics.
pub struct LayerAttrStats {
    id: usize,
    // This utility is called for billions of features by multiple threads when processing the planet which can make
    // access to shared data structures a bottleneck. So give each thread an individual handler to update and
    // aggregate the results on read.
    thread_locals: Mutex<Vec<Arc<ThreadLocalHandler>>>,
}

impl Default for LayerAttrStats {
    fn default() -> Self {
        Self::new()
    }
}

impl LayerAttrStats {
    pub fn new() -> LayerAttrStats {
        LayerAttrStats { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), thread_locals: Mutex::new(Vec::new()) }
    }

    /// Returns stats on all features that have been emitted as a list of [`VectorLayer`] objects, sorted by layer.
    pub fn get_tile_stats(&self) -> Vec<VectorLayer> {
        let mut layers: BTreeMap<String, StatsForLayer> = BTreeMap::new();
        for thread_local in self.thread_locals.lock().iter() {
            for stats in thread_local.layers.lock().values() {
                match layers.get_mut(&stats.layer) {
                    None => {
                        layers.insert(stats.layer.clone(), stats.clone());
                    }
                    Some(prev) => {
                        prev.expand_zoom_range_to_include(stats.maxzoom);
                        prev.expand_zoom_range_to_include(stats.minzoom);
                        for (key, value) in &stats.fields {
                            // keep track of field type as a number/boolean/string but widen to string if multiple
                            // different types are encountered
                            merge_field(&mut prev.fields, key, *value);
                        }
                    }
                }
            }
        }
        layers
            .into_values()
            .map(|stats| {
                VectorLayer::with_zooms(stats.layer, stats.fields.into_iter().collect(), stats.minzoom, stats.maxzoom)
            })
            .collect()
    }

    /// Shortcut for tests.
    #[cfg(test)]
    pub(crate) fn accept(&self, layer: &str, zoom: u32, key: &str, value: &Value) {
        self.handler_for_thread().for_zoom(zoom).for_layer(layer).accept(key, value);
    }

    /// Returns a handler optimized for accepting features from a single thread (the same handler is returned for
    /// repeated calls from the same thread).
    pub fn handler_for_thread(&self) -> Updater {
        HANDLERS.with(|handlers| handlers.borrow_mut().entry(self.id).or_insert_with(|| self.new_handler()).clone())
    }

    /// Returns a new handler that should only be updated from one thread at a time.
    pub fn new_handler(&self) -> Updater {
        let handler = Arc::new(ThreadLocalHandler::default());
        self.thread_locals.lock().push(handler.clone());
        Updater(handler)
    }
}

fn merge_field(fields: &mut BTreeMap<String, FieldType>, key: &str, field_type: FieldType) {
    match fields.get_mut(key) {
        Some(existing) => *existing = FieldType::merge(*existing, field_type),
        None => {
            fields.insert(key.to_string(), field_type);
        }
    }
}

pub use crate::archive::tile_archive_metadata::FieldType;

/// An entry in the MBTiles `vector_layers` metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VectorLayer {
    pub id: String,
    pub fields: IndexMap<String, FieldType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minzoom: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maxzoom: Option<u32>,
}

impl VectorLayer {
    pub fn new(id: impl Into<String>, fields: IndexMap<String, FieldType>) -> VectorLayer {
        VectorLayer { id: id.into(), fields, description: None, minzoom: None, maxzoom: None }
    }

    pub fn with_zooms(
        id: impl Into<String>,
        fields: IndexMap<String, FieldType>,
        minzoom: u32,
        maxzoom: u32,
    ) -> VectorLayer {
        VectorLayer { id: id.into(), fields, description: None, minzoom: Some(minzoom), maxzoom: Some(maxzoom) }
    }

    pub fn with_description(&self, new_description: impl Into<String>) -> VectorLayer {
        VectorLayer { description: Some(new_description.into()), ..self.clone() }
    }

    pub fn with_minzoom(&self, new_minzoom: u32) -> VectorLayer {
        VectorLayer { minzoom: Some(new_minzoom), ..self.clone() }
    }

    pub fn with_maxzoom(&self, new_maxzoom: u32) -> VectorLayer {
        VectorLayer { maxzoom: Some(new_maxzoom), ..self.clone() }
    }
}

/// Converts to the archive metadata's `vector_layers` entry type.
///
/// The two `VectorLayer` types differ on purpose: this one keeps fields in insertion order (`IndexMap`) with `u32`
/// zooms while stats are collected, and the archive one keeps them sorted (`BTreeMap`, as written to metadata) with
/// `i32` zooms.
impl From<VectorLayer> for crate::archive::tile_archive_metadata::VectorLayer {
    fn from(layer: VectorLayer) -> Self {
        crate::archive::tile_archive_metadata::VectorLayer {
            id: layer.id,
            fields: layer.fields.into_iter().collect(),
            description: layer.description,
            minzoom: layer.minzoom.map(|z| z as i32),
            maxzoom: layer.maxzoom.map(|z| z as i32),
        }
    }
}

#[derive(Default)]
struct ThreadLocalHandler {
    layers: Mutex<BTreeMap<String, StatsForLayer>>,
}

/// Accepts features from a single thread that will be combined across all threads in
/// [`LayerAttrStats::get_tile_stats`].
#[derive(Clone)]
pub struct Updater(Arc<ThreadLocalHandler>);

impl Updater {
    pub fn for_zoom(&self, zoom: u32) -> ForZoom {
        ForZoom { handler: Some(self.0.clone()), zoom }
    }
}

/// Stats updater for a single zoom level. [`ForZoom::default`] (or [`ForZoom::noop`]) discards everything.
#[derive(Clone, Default)]
pub struct ForZoom {
    handler: Option<Arc<ThreadLocalHandler>>,
    zoom: u32,
}

impl ForZoom {
    /// An updater that ignores all stats.
    pub fn noop() -> ForZoom {
        ForZoom::default()
    }

    /// Returns an updater for attributes of features in `layer`. Holds a lock on this thread's handler while alive.
    pub fn for_layer(&self, layer: &str) -> ForLayer<'_> {
        let Some(handler) = &self.handler else {
            return ForLayer { stats: None };
        };
        let mut guard = handler.layers.lock();
        if !guard.contains_key(layer) {
            guard.insert(layer.to_string(), StatsForLayer::new(layer));
        }
        let mut stats = MutexGuard::map(guard, |m| m.get_mut(layer).expect("inserted above"));
        stats.expand_zoom_range_to_include(self.zoom);
        ForLayer { stats: Some(stats) }
    }
}

/// Stats updater for a single layer at a single zoom level.
pub struct ForLayer<'a> {
    stats: Option<MappedMutexGuard<'a, StatsForLayer>>,
}

impl ForLayer<'_> {
    pub fn accept(&mut self, key: &str, value: &Value) {
        if let Some(stats) = self.stats.as_mut()
            && let Some(field_type) = FieldType::of(value)
        {
            // widen different types to string
            merge_field(&mut stats.fields, key, field_type);
        }
    }
}

#[derive(Clone)]
struct StatsForLayer {
    layer: String,
    // use sorted map to ensure the same output always appears the same in an archive
    fields: BTreeMap<String, FieldType>,
    minzoom: u32,
    maxzoom: u32,
}

impl StatsForLayer {
    fn new(layer: &str) -> Self {
        StatsForLayer { layer: layer.to_string(), fields: BTreeMap::new(), minzoom: u32::MAX, maxzoom: u32::MIN }
    }

    fn expand_zoom_range_to_include(&mut self, zoom: u32) {
        self.minzoom = self.minzoom.min(zoom);
        self.maxzoom = self.maxzoom.max(zoom);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(items: &[(&str, FieldType)]) -> IndexMap<String, FieldType> {
        items.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn test_empty_layer_stats() {
        assert_eq!(Vec::<VectorLayer>::new(), LayerAttrStats::new().get_tile_stats());
    }

    #[test]
    fn test_empty_layer_stats_one_layer() {
        let layer_stats = LayerAttrStats::new();
        layer_stats.accept("layer1", 3, "a", &Value::Int(1));
        layer_stats.accept("layer1", 3, "b", &Value::str("string"));
        layer_stats.accept("layer1", 3, "c", &Value::Bool(true));
        assert_eq!(
            vec![VectorLayer::with_zooms(
                "layer1",
                fields(&[("a", FieldType::Number), ("b", FieldType::String), ("c", FieldType::Boolean)]),
                3,
                3
            )],
            layer_stats.get_tile_stats()
        );
    }

    #[test]
    fn test_empty_layer_stats_two_layers() {
        let layer_stats = LayerAttrStats::new();
        let _ = layer_stats.handler_for_thread().for_zoom(3).for_layer("layer1");
        layer_stats.accept("layer2", 4, "a", &Value::Int(1));
        layer_stats.accept("layer2", 4, "b", &Value::Bool(true));
        layer_stats.accept("layer2", 4, "c", &Value::Bool(true));
        layer_stats.accept("layer2", 1, "a", &Value::Int(1));
        layer_stats.accept("layer2", 1, "b", &Value::Bool(true));
        layer_stats.accept("layer2", 1, "c", &Value::Int(1));
        assert_eq!(
            vec![
                VectorLayer::with_zooms("layer1", fields(&[]), 3, 3),
                VectorLayer::with_zooms(
                    "layer2",
                    fields(&[("a", FieldType::Number), ("b", FieldType::Boolean), ("c", FieldType::String)]),
                    1,
                    4
                )
            ],
            layer_stats.get_tile_stats()
        );
    }

    #[test]
    fn test_merge_from_multiple_threads() {
        let layer_stats = LayerAttrStats::new();
        layer_stats.accept("layer1", 3, "a", &Value::Bool(true));
        std::thread::scope(|s| {
            s.spawn(|| layer_stats.accept("layer1", 3, "a", &Value::Int(1)));
            s.spawn(|| layer_stats.accept("layer1", 4, "a", &Value::Bool(true)));
        });
        assert_eq!(
            vec![VectorLayer::with_zooms("layer1", fields(&[("a", FieldType::String)]), 3, 4)],
            layer_stats.get_tile_stats()
        );
        // reading stats does not modify them
        assert_eq!(
            vec![VectorLayer::with_zooms("layer1", fields(&[("a", FieldType::String)]), 3, 4)],
            layer_stats.get_tile_stats()
        );
    }

    #[test]
    fn test_serialize() {
        let layer = VectorLayer::with_zooms("a", fields(&[("x", FieldType::Number)]), 0, 2);
        let json = serde_json::to_string(&layer).unwrap();
        assert_eq!(r#"{"id":"a","fields":{"x":"Number"},"minzoom":0,"maxzoom":2}"#, json);
        assert_eq!(layer, serde_json::from_str(&json).unwrap());
        assert_eq!(
            r#"{"id":"a","fields":{}}"#,
            serde_json::to_string(&VectorLayer::new("a", IndexMap::new())).unwrap()
        );
    }
}
