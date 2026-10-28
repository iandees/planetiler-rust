//! Port of `FeatureCollector.java`: utility that [`Profile`](crate::profile::Profile) implementations use to build
//! map features that should be emitted for an input source feature.
//!
//! For example to add a polygon feature for a lake and a center label point with its name:
//!
//! ```ignore
//! features.polygon("water").set_attr("class", "lake");
//! features.centroid("water_name").set_attr("class", "lake").set_attr("name", source.get_string("name"));
//! ```
//!
//! # Rust notes
//!
//! - A [`FeatureCollector`] borrows the [`Factory`] (config + stats) and the [`SourceFeature`] it was created for,
//!   so it (and its [`Feature`]s) only live as long as the source feature. Builder methods return `&mut Feature` for
//!   chaining.
//! - Geometries taken directly from the source (`point`, `line`, `polygon`, ...) are borrowed rather than cloned
//!   where possible ([`Feature::geometry`] returns a reference either way).
//! - Attribute values are [`AttrValue`]s: plain [`Value`]s (anything `Into<Value>`, plus [`Struct`]s which are
//!   unwrapped to their raw value) or zoom-dependent [`ZoomFunction`]s. Like Java, `null` values are ignored.
//! - Attributes are kept sorted by key (Java uses a `TreeMap`). [`Feature::get_attrs_at_zoom`] returns a shared
//!   `Arc<Tags>` so rendered features can share it.
//! - [`Feature::omit`] marks a feature as omitted; iteration ([`FeatureCollector::iter`]) skips omitted features
//!   and the "empty" features returned when a geometry can't be constructed.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, LazyLock, Mutex};

use geo_types::Geometry;

use crate::collection::feature_group::{SORT_KEY_MAX, SORT_KEY_MIN};
use crate::config::planetiler_config::PlanetilerConfig;
use crate::geo::geo_utils;
use crate::geo::geometry_exception::GeometryResult;
use crate::geo::geometry_pipeline::{GeometryPipelineRef, ZoomPipeline};
use crate::geo::geometry_type::GeometryType;
use crate::geo::simplify_method::SimplifyMethod;
use crate::reader::source_feature::SourceFeature;
use crate::reader::structs::Struct;
use crate::stats::stats::StatsRef;
use crate::util::cache_by_zoom::CacheByZoom;
use crate::util::merging_range_map::MergingRangeMap;
use crate::util::zoom_function::{self, ZoomFunction};
use crate::value::{Tags, Value};

/// Layers for which an out-of-range sort key has already been reported, so the error is logged once per layer.
static LAYERS_WITH_CLAMPED_SORT_KEY: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

const DEFAULT_LABEL_GRID_SIZE: f64 = 0.0;
const DEFAULT_LABEL_GRID_LIMIT: i32 = 0;

/// An attribute value: either a plain value, or a value that changes by zoom level (Java `Object` attribute values
/// that may be a `ZoomFunction` or `Struct`).
#[derive(Clone, Debug)]
pub enum AttrValue {
    Value(Value),
    Zoom(ZoomFunction<Value>),
}

impl AttrValue {
    /// Evaluates this value at `zoom` (Java `unwrap`).
    pub fn at_zoom(&self, zoom: u32) -> Option<Value> {
        match self {
            AttrValue::Value(v) => Some(v.clone()),
            AttrValue::Zoom(f) => f.apply(zoom),
        }
    }

    /// Returns a value that only applies at or above `minzoom` (Java `ZoomFunction.minZoom(minzoom, value)`).
    pub fn with_min_zoom(self, minzoom: u32) -> AttrValue {
        match self {
            AttrValue::Value(v) => AttrValue::Zoom(ZoomFunction::min_zoom(minzoom, v)),
            AttrValue::Zoom(f) => {
                AttrValue::Zoom(ZoomFunction::new(move |z| if z >= minzoom { f.apply(z) } else { None }))
            }
        }
    }
}

impl<T: Into<Value>> From<T> for AttrValue {
    fn from(value: T) -> Self {
        AttrValue::Value(value.into())
    }
}

impl From<&Value> for AttrValue {
    fn from(value: &Value) -> Self {
        AttrValue::Value(value.clone())
    }
}

impl From<Struct> for AttrValue {
    fn from(value: Struct) -> Self {
        AttrValue::Value(value.into_raw_value())
    }
}

impl From<&Struct> for AttrValue {
    fn from(value: &Struct) -> Self {
        AttrValue::Value(value.raw_value().clone())
    }
}

impl<T: Into<Value> + Clone + Send + Sync + 'static> From<ZoomFunction<T>> for AttrValue {
    fn from(value: ZoomFunction<T>) -> Self {
        AttrValue::Zoom(ZoomFunction::new(move |z| value.apply(z).map(Into::into)))
    }
}

impl From<ZoomFunction<Struct>> for AttrValue {
    fn from(value: ZoomFunction<Struct>) -> Self {
        AttrValue::Zoom(ZoomFunction::new(move |z| value.apply(z).map(Struct::into_raw_value)))
    }
}

/// Creates new feature collector instances for each source feature that we encounter.
#[derive(Clone)]
pub struct Factory {
    pub config: Arc<PlanetilerConfig>,
    pub stats: StatsRef,
}

impl fmt::Debug for Factory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FeatureCollector.Factory")
    }
}

impl Factory {
    pub fn new(config: impl Into<Arc<PlanetilerConfig>>, stats: StatsRef) -> Factory {
        Factory { config: config.into(), stats }
    }

    /// Returns a new collector for features generated from `source`.
    pub fn get<'a>(&'a self, source: &'a dyn SourceFeature) -> FeatureCollector<'a> {
        FeatureCollector { factory: self, source, output: Vec::new() }
    }
}

/// Collects the output map features for a single source feature.
pub struct FeatureCollector<'a> {
    factory: &'a Factory,
    source: &'a dyn SourceFeature,
    output: Vec<Feature<'a>>,
}

impl fmt::Debug for FeatureCollector<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a> FeatureCollector<'a> {
    /// The source feature this collector was created for.
    pub fn source(&self) -> &'a dyn SourceFeature {
        self.source
    }

    /// The config this collector uses.
    pub fn config(&self) -> &'a PlanetilerConfig {
        &self.factory.config
    }

    /// Iterates over the features to emit (excluding omitted and empty features).
    pub fn iter(&self) -> impl Iterator<Item = &Feature<'a>> + '_ {
        self.output.iter().filter(|f| !f.omitted)
    }

    /// Number of features to emit.
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    /// True if there are no features to emit.
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    /// Consumes this collector, returning the features to emit.
    pub fn into_features(self) -> Vec<Feature<'a>> {
        let mut output = self.output;
        output.retain(|f| !f.omitted);
        output
    }

    fn vector_tile_id(&self, renumber: bool) -> i64 {
        let multiplier = self.factory.config.feature_source_id_multiplier;
        if multiplier < 4 { self.source.id() } else { self.source.vector_tile_feature_id(multiplier, renumber) }
    }

    fn push(&mut self, layer: &str, geom: Cow<'a, Geometry<f64>>, id: i64, empty: bool) -> &mut Feature<'a> {
        let feature = Feature::new(self.factory, self.source, layer, geom, id, empty);
        self.output.push(feature);
        self.output.last_mut().expect("just pushed")
    }

    fn add(&mut self, layer: &str, geom: Cow<'a, Geometry<f64>>, renumber: bool) -> &mut Feature<'a> {
        let id = self.vector_tile_id(renumber);
        self.push(layer, geom, id, false)
    }

    fn empty(&mut self, layer: &str) -> &mut Feature<'a> {
        let id = self.source.id();
        self.push(layer, Cow::Owned(geo_utils::empty_geometry()), id, true)
    }

    fn add_or_log(
        &mut self,
        layer: &str,
        result: GeometryResult<Cow<'a, Geometry<f64>>>,
        renumber: bool,
        stat: &str,
        message: &str,
    ) -> &mut Feature<'a> {
        match result {
            Ok(geom) => self.add(layer, geom, renumber),
            Err(e) => {
                e.log_with_stats(&*self.factory.stats, stat, &format!("{message} {:?}", self.source));
                self.empty(layer)
            }
        }
    }

    /// Starts building a new map feature with an explicit `geometry` (world coordinates) that overrides the source
    /// geometry. `renumber=true` renumbers split ways.
    pub fn geometry_with_renumber(&mut self, layer: &str, geometry: Geometry<f64>, renumber: bool) -> &mut Feature<'a> {
        self.add(layer, Cow::Owned(geometry), renumber)
    }

    /// Alias for [`FeatureCollector::geometry_with_renumber`] with `renumber=false`.
    pub fn geometry(&mut self, layer: &str, geometry: Geometry<f64>) -> &mut Feature<'a> {
        self.geometry_with_renumber(layer, geometry, false)
    }

    /// Starts building a new point map feature that expects the source feature to be a point.
    ///
    /// If the source feature is not a point, logs an error and returns a feature that can be configured, but won't
    /// actually emit anything to the map.
    pub fn point(&mut self, layer: &str) -> &mut Feature<'a> {
        let source = self.source;
        let result = if source.is_point() {
            source.world_geometry().map(Cow::Borrowed)
        } else {
            Err(crate::geo::geometry_exception::GeometryException::non_fatal("feature_not_point", "not a point"))
        };
        self.add_or_log(layer, result, false, "feature_point", "Error getting point geometry for")
    }

    /// Starts building a new line map feature that expects the source feature to be a line.
    ///
    /// If the source feature cannot be a line, logs an error and returns a feature that can be configured, but won't
    /// actually emit anything to the map. Split ways (see [`FeatureCollector::split_line`]) are ignored.
    pub fn line(&mut self, layer: &str) -> &mut Feature<'a> {
        if self.source.is_split_way() {
            // when a way gets split there is a full way and split ways, use split_line(...) to opt into split ways
            return self.empty(layer);
        }
        let source = self.source;
        self.add_or_log(layer, source.line().map(Cow::Borrowed), false, "feature_line", "Error constructing line for")
    }

    /// Starts building a new line map feature for each segment of a way if
    /// [`Profile::split_osm_way_at_intersections`](crate::profile::Profile::split_osm_way_at_intersections) was
    /// enabled for that way. `renumber=true` assigns distinct vector tile feature IDs to each segment.
    pub fn split_line_with_renumber(&mut self, layer: &str, renumber: bool) -> &mut Feature<'a> {
        if self.source.is_full_way() {
            // exclude the full way when you opt into split ways
            return self.empty(layer);
        }
        let source = self.source;
        self.add_or_log(
            layer,
            source.line().map(Cow::Borrowed),
            renumber,
            "feature_line",
            "Error constructing line for",
        )
    }

    /// Alias for [`FeatureCollector::split_line_with_renumber`] with `renumber=true`.
    pub fn split_line(&mut self, layer: &str) -> &mut Feature<'a> {
        self.split_line_with_renumber(layer, true)
    }

    /// Starts building a new partial line feature from `start` to `end` where 0 is the beginning of the line and 1
    /// is the end of the line.
    pub fn partial_line(&mut self, layer: &str, start: f64, end: f64) -> &mut Feature<'a> {
        let result = self.source.partial_line(start, end).map(Cow::Owned);
        self.add_or_log(layer, result, false, "feature_partial_line", "Error constructing partial line for")
    }

    /// Starts building a new polygon map feature that expects the source feature to be a polygon.
    pub fn polygon(&mut self, layer: &str) -> &mut Feature<'a> {
        let source = self.source;
        self.add_or_log(
            layer,
            source.polygon().map(Cow::Borrowed),
            false,
            "feature_polygon",
            "Error constructing polygon for",
        )
    }

    /// Starts building a new polygon, line, or point map feature based on the geometry type of the input feature.
    pub fn any_geometry(&mut self, layer: &str) -> &mut Feature<'a> {
        if self.source.can_be_polygon() {
            self.polygon(layer)
        } else if self.source.can_be_line() {
            self.line(layer)
        } else if self.source.is_point() {
            self.point(layer)
        } else {
            self.empty(layer)
        }
    }

    /// Starts building a new point map feature at the centroid of the source feature.
    pub fn centroid(&mut self, layer: &str) -> &mut Feature<'a> {
        let source = self.source;
        self.add_or_log(
            layer,
            source.centroid().map(Cow::Borrowed),
            false,
            "feature_centroid",
            "Error getting centroid for",
        )
    }

    /// Starts building a new point map feature at the centroid if the source feature is a point, line, or simple
    /// convex polygon, or a point on the surface if it is a multipolygon, polygon with holes, or concave polygon.
    pub fn centroid_if_convex(&mut self, layer: &str) -> &mut Feature<'a> {
        let source = self.source;
        self.add_or_log(
            layer,
            source.centroid_if_convex().map(Cow::Borrowed),
            false,
            "feature_centroid_if_convex",
            "Error constructing centroid if convex for",
        )
    }

    /// Starts building a new point map feature at a point on the surface of the source feature (JTS
    /// `getInteriorPoint`).
    pub fn point_on_surface(&mut self, layer: &str) -> &mut Feature<'a> {
        let source = self.source;
        self.add_or_log(
            layer,
            source.point_on_surface().map(Cow::Borrowed),
            false,
            "feature_point_on_surface",
            "Error constructing point on surface for",
        )
    }

    /// Starts building a new point map feature at the furthest interior point of a polygon from its edge (aka "pole
    /// of inaccessibility").
    ///
    /// NOTE: this is substantially more expensive to compute than [`FeatureCollector::centroid`] or
    /// [`FeatureCollector::point_on_surface`], especially for small `tolerance` values. `tolerance` is the precision:
    /// 0.01 means 1% of the square root of the area. Values between 5% and 10% are a good compromise.
    pub fn innermost_point_with_tolerance(&mut self, layer: &str, tolerance: f64) -> &mut Feature<'a> {
        let result = self.source.innermost_point(tolerance).map(Cow::Owned);
        self.add_or_log(layer, result, false, "feature_innermost_point", "Error constructing innermost point for")
    }

    /// Alias for [`FeatureCollector::innermost_point_with_tolerance`] with a default tolerance of 10%.
    pub fn innermost_point(&mut self, layer: &str) -> &mut Feature<'a> {
        self.innermost_point_with_tolerance(layer, 0.1)
    }

    /// Starts building a new point map feature at the midpoint of this line, or the longest line segment if a
    /// multilinestring.
    pub fn line_midpoint(&mut self, layer: &str) -> &mut Feature<'a> {
        let source = self.source;
        self.add_or_log(
            layer,
            source.line_midpoint().map(Cow::Borrowed),
            false,
            "feature_line_midpoint",
            "Error getting midpoint for",
        )
    }

    /// Starts building a new point map feature at `ratio` along the linestring (or longest segment if it is a
    /// multilinestring): 0 for start, 1 for end, 0.5 for midpoint.
    pub fn point_along_line(&mut self, layer: &str, ratio: f64) -> &mut Feature<'a> {
        let result = self.source.point_along_line(ratio).map(Cow::Owned);
        self.add_or_log(layer, result, false, "feature_point_along_line", "Error getting point along line for")
    }

    /// Returns the minimum zoom level at which this feature is at least `pixel_size` pixels large.
    pub fn get_min_zoom_for_pixel_size(&self, pixel_size: f64) -> u32 {
        min_zoom_for_pixel_size(self.factory, self.source, pixel_size)
    }

    /// Returns the actual pixel size of the source feature at `zoom` (length if line, sqrt(area) if polygon).
    pub fn get_pixel_size_at_zoom(&self, zoom: u32) -> f64 {
        pixel_size_at_zoom(self.factory, self.source, zoom)
    }
}

impl<'c, 'a> IntoIterator for &'c FeatureCollector<'a> {
    type Item = &'c Feature<'a>;
    type IntoIter = Box<dyn Iterator<Item = &'c Feature<'a>> + 'c>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

fn min_zoom_for_pixel_size(factory: &Factory, source: &dyn SourceFeature, pixel_size: f64) -> u32 {
    match source.size() {
        Ok(size) => geo_utils::min_zoom_for_pixel_size(size, pixel_size),
        Err(e) => {
            e.log_with_stats(
                &*factory.stats,
                "min_zoom_for_size_failure",
                &format!("Error getting min zoom for size from geometry {source:?}"),
            );
            factory.config.maxzoom
        }
    }
}

fn pixel_size_at_zoom(factory: &Factory, source: &dyn SourceFeature, zoom: u32) -> f64 {
    match source.size() {
        Ok(size) => size * (256u64 << zoom) as f64,
        Err(e) => {
            e.log_with_stats(
                &*factory.stats,
                "source_feature_pixel_size_at_zoom_failure",
                &format!("Error getting source feature pixel size at zoom from geometry {source:?}"),
            );
            0.0
        }
    }
}

/// A linear-scoped override over `[start, end)` of a line.
#[derive(Clone, Debug)]
enum OverrideCommand {
    Minzoom(u32),
    Maxzoom(u32),
    Omit,
    Attr(String, AttrValue),
}

#[derive(Clone, Debug)]
struct Override {
    start: f64,
    end: f64,
    command: OverrideCommand,
}

/// A fully-configured subset of a line feature with linear-scoped attributes applied to a subset of the range.
#[derive(Clone, Debug, PartialEq)]
pub struct RangeWithTags {
    pub start: f64,
    pub end: f64,
    pub geom: Geometry<f64>,
    pub attrs: Arc<Tags>,
}

impl RangeWithTags {
    pub fn new(start: f64, end: f64, geom: impl Into<Geometry<f64>>, attrs: Tags) -> RangeWithTags {
        RangeWithTags { start, end, geom: geom.into(), attrs: Arc::new(attrs) }
    }
}

/// Inserts `key => value` into `tags` keeping keys sorted.
fn insert_sorted(tags: &mut Tags, key: String, value: Value) {
    match tags.binary_search_keys(&key) {
        Ok(idx) => {
            tags[idx] = value;
        }
        Err(idx) => {
            tags.shift_insert(idx, key, value);
        }
    }
}

/// Returns a new map with `key => value` (or without `key` if value is null/empty) (Java `MapUtil.with`).
fn with_attr(tags: &Tags, key: &str, value: Option<Value>) -> Tags {
    let mut copy = tags.clone();
    match value {
        None | Some(Value::Null) => {
            copy.shift_remove(key);
        }
        Some(Value::String(s)) if s.is_empty() => {
            copy.shift_remove(key);
        }
        Some(v) => insert_sorted(&mut copy, key.to_string(), v),
    }
    copy
}

/// A builder for an output map feature that contains all the information that will be needed to render vector tile
/// features from the input element.
///
/// Some feature attributes are set globally (like sort key), and some allow the value to change by zoom-level (like
/// tags).
pub struct Feature<'a> {
    factory: &'a Factory,
    source: &'a dyn SourceFeature,
    layer: String,
    geom: Cow<'a, Geometry<f64>>,
    geometry_type: GeometryType,
    empty: bool,
    omitted: bool,
    /// Plain (non-zoom-dependent) attributes, sorted by key.
    attrs: Arc<Tags>,
    /// Zoom-dependent attributes, sorted by key (disjoint from `attrs`).
    zoom_attrs: Vec<(String, AttrValue)>,
    must_unwrap_values: bool,
    attr_cache: OnceCell<Box<CacheByZoom<Arc<Tags>>>>,
    partial_range_cache: OnceCell<Box<CacheByZoom<Vec<RangeWithTags>>>>,
    id: i64,
    sort_key: i32,
    minzoom: u32,
    maxzoom: u32,
    label_grid_pixel_size: Option<ZoomFunction<f64>>,
    label_grid_limit: Option<ZoomFunction<i32>>,
    default_buffer_pixels: f64,
    buffer_pixel_overrides: Option<ZoomFunction<f64>>,
    default_min_pixel_size: f64,
    min_pixel_size_at_max_zoom: f64,
    min_pixel_size: Option<ZoomFunction<f64>>,
    default_pixel_tolerance: f64,
    pixel_tolerance_at_max_zoom: f64,
    pixel_tolerance: Option<ZoomFunction<f64>>,
    default_simplify_method: SimplifyMethod,
    simplify_method: Option<ZoomFunction<SimplifyMethod>>,
    default_geometry_pipeline: Option<GeometryPipelineRef>,
    geometry_pipeline_by_zoom: Option<ZoomPipeline>,
    num_points_attr: Option<String>,
    partial_overrides: Option<Vec<Override>>,
}

/// Generates the attribute helper methods shared by [`Feature`] and [`LinearRange`] (Java's `WithAttrs` and
/// `WithZoomRange` default methods).
macro_rules! attr_methods {
    () => {
        /// Sets an attribute on the output feature to a plain value (anything `Into<Value>`, `null` is ignored), a
        /// [`Struct`], or a [`ZoomFunction`] to change the value by zoom level.
        pub fn set_attr(&mut self, key: impl Into<String>, value: impl Into<AttrValue>) -> &mut Self {
            self.set_attr_value(key.into(), value.into());
            self
        }

        /// Copies the value for `key` attribute from the source feature to the output feature.
        pub fn inherit_attr_from_source(&mut self, key: &str) -> &mut Self {
            let value = self.source_feature().get_tag(key).cloned();
            self.set_attr(key, value)
        }

        /// Copies the values for `keys` attributes from the source feature to the output feature.
        pub fn inherit_attrs_from_source(&mut self, keys: &[&str]) -> &mut Self {
            for key in keys {
                self.inherit_attr_from_source(key);
            }
            self
        }

        /// Copies the values for `keys` attributes from the source feature to the output feature at or above
        /// `minzoom`.
        pub fn inherit_attrs_from_source_with_minzoom(&mut self, minzoom: u32, keys: &[&str]) -> &mut Self {
            for key in keys {
                let value = self.source_feature().get_tag(key).cloned();
                self.set_attr_with_minzoom(*key, value, minzoom);
            }
            self
        }

        /// Sets the value for `key` attribute at or above `minzoom`. Below `minzoom` it will be ignored.
        ///
        /// Replaces all previous values that have been set for `key` at any zoom level. To have a value that changes
        /// at multiple zoom level thresholds, call [`Self::set_attr`] with a manually-constructed [`ZoomFunction`].
        pub fn set_attr_with_minzoom(
            &mut self,
            key: impl Into<String>,
            value: impl Into<AttrValue>,
            minzoom: u32,
        ) -> &mut Self {
            self.set_attr_value(key.into(), value.into().with_min_zoom(minzoom));
            self
        }

        /// Sets the value for `key` only at zoom levels where the feature is at least `min_pixel_size` pixels in
        /// size.
        pub fn set_attr_with_min_size(
            &mut self,
            key: impl Into<String>,
            value: impl Into<AttrValue>,
            min_pixel_size: f64,
        ) -> &mut Self {
            let minzoom = self.get_min_zoom_for_pixel_size(min_pixel_size);
            self.set_attr_with_minzoom(key, value, minzoom)
        }

        /// Sets the value for `key` so that it always shows when `zoom_level >= min_zoom_to_show_always` but only
        /// shows when `min_zoom_if_big_enough <= zoom_level < min_zoom_to_show_always` when it is at least
        /// `min_pixel_size` pixels in size.
        pub fn set_attr_with_min_size_range(
            &mut self,
            key: impl Into<String>,
            value: impl Into<AttrValue>,
            min_pixel_size: f64,
            min_zoom_if_big_enough: u32,
            min_zoom_to_show_always: u32,
        ) -> &mut Self {
            let minzoom =
                self.get_min_zoom_for_pixel_size(min_pixel_size).clamp(min_zoom_if_big_enough, min_zoom_to_show_always);
            self.set_attr_with_minzoom(key, value, minzoom)
        }

        /// Inserts all key/value pairs in `attrs` into the set of attributes to emit on the output feature at or
        /// above `minzoom`, replacing values that have already been set.
        pub fn put_attrs_with_minzoom<K: Into<String>, V: Into<AttrValue>>(
            &mut self,
            attrs: impl IntoIterator<Item = (K, V)>,
            minzoom: u32,
        ) -> &mut Self {
            for (k, v) in attrs {
                self.set_attr_with_minzoom(k, v, minzoom);
            }
            self
        }

        /// Inserts all key/value pairs in `attrs` into the set of attributes to emit on the output feature.
        pub fn put_attrs<K: Into<String>, V: Into<AttrValue>>(
            &mut self,
            attrs: impl IntoIterator<Item = (K, V)>,
        ) -> &mut Self {
            for (k, v) in attrs {
                self.set_attr(k, v);
            }
            self
        }

        /// Sets the zoom range (inclusive) that this feature appears in.
        pub fn set_zoom_range(&mut self, min: u32, max: u32) -> &mut Self {
            debug_assert!(min <= max);
            self.set_min_zoom(min);
            self.set_max_zoom(max)
        }
    };
}

impl<'a> Feature<'a> {
    fn new(
        factory: &'a Factory,
        source: &'a dyn SourceFeature,
        layer: &str,
        geom: Cow<'a, Geometry<f64>>,
        id: i64,
        empty: bool,
    ) -> Feature<'a> {
        let config = &factory.config;
        let geometry_type = GeometryType::type_of(&geom);
        let is_point = geometry_type == GeometryType::Point;
        Feature {
            factory,
            source,
            layer: layer.to_string(),
            geom,
            geometry_type,
            empty,
            omitted: empty,
            attrs: Arc::new(Tags::default()),
            zoom_attrs: Vec::new(),
            must_unwrap_values: false,
            attr_cache: OnceCell::new(),
            partial_range_cache: OnceCell::new(),
            id,
            sort_key: 0,
            minzoom: config.minzoom,
            maxzoom: config.maxzoom,
            label_grid_pixel_size: None,
            label_grid_limit: None,
            default_buffer_pixels: 4.0,
            buffer_pixel_overrides: None,
            default_min_pixel_size: if is_point { 0.0 } else { config.min_feature_size_below_max_zoom },
            min_pixel_size_at_max_zoom: if is_point { 0.0 } else { config.min_feature_size_at_max_zoom },
            min_pixel_size: None,
            default_pixel_tolerance: config.simplify_tolerance_below_max_zoom,
            pixel_tolerance_at_max_zoom: config.simplify_tolerance_at_max_zoom,
            pixel_tolerance: None,
            default_simplify_method: SimplifyMethod::DOUGLAS_PEUCKER,
            simplify_method: None,
            default_geometry_pipeline: None,
            geometry_pipeline_by_zoom: None,
            num_points_attr: None,
            partial_overrides: None,
        }
    }

    fn config(&self) -> &PlanetilerConfig {
        &self.factory.config
    }

    fn source_feature(&self) -> &'a dyn SourceFeature {
        self.source
    }

    fn invalidate_attr_caches(&mut self) {
        self.attr_cache.take();
        self.partial_range_cache.take();
    }

    fn set_attr_value(&mut self, key: String, value: AttrValue) {
        self.invalidate_attr_caches();
        match value {
            AttrValue::Value(Value::Null) => {}
            AttrValue::Value(v) => {
                if !self.zoom_attrs.is_empty()
                    && let Ok(idx) = self.zoom_attrs.binary_search_by(|(k, _)| k.as_str().cmp(&key))
                {
                    self.zoom_attrs.remove(idx);
                }
                insert_sorted(Arc::make_mut(&mut self.attrs), key, v);
            }
            AttrValue::Zoom(f) => {
                self.must_unwrap_values = true;
                if !self.attrs.is_empty() {
                    Arc::make_mut(&mut self.attrs).shift_remove(&key);
                }
                match self.zoom_attrs.binary_search_by(|(k, _)| k.as_str().cmp(&key)) {
                    Ok(idx) => self.zoom_attrs[idx].1 = AttrValue::Zoom(f),
                    Err(idx) => self.zoom_attrs.insert(idx, (key, AttrValue::Zoom(f))),
                }
            }
        }
    }

    attr_methods!();

    /// Returns the minimum zoom level at which the source feature is at least `min_pixel_size` pixels large.
    pub fn get_min_zoom_for_pixel_size(&self, min_pixel_size: f64) -> u32 {
        min_zoom_for_pixel_size(self.factory, self.source, min_pixel_size)
    }

    /// Returns the original ID of the source feature that this feature came from (i.e. OSM node/way ID).
    pub fn get_id(&self) -> i64 {
        self.id
    }

    pub fn set_id(&mut self, id: i64) -> &mut Self {
        self.id = id;
        self
    }

    pub fn get_geometry_type(&self) -> GeometryType {
        self.geometry_type
    }

    pub fn is_polygon(&self) -> bool {
        self.geometry_type == GeometryType::Polygon
    }

    pub fn is_line(&self) -> bool {
        self.geometry_type == GeometryType::Line
    }

    /// Returns the value by which features are sorted within a layer in the output vector tile.
    pub fn get_sort_key(&self) -> i32 {
        self.sort_key
    }

    /// Sets the value by which features are sorted within a layer in the output vector tile. The sort key gets packed
    /// into [`SORT_KEY_BITS`](crate::collection::feature_group::SORT_KEY_BITS) bits so the range is limited to
    /// `SORT_KEY_MIN..=SORT_KEY_MAX`.
    ///
    /// A sort key outside that range is clamped to the nearest end of the range, and an error is logged the first
    /// time that happens for each layer.
    ///
    /// Circles, lines, and polygons are rendered in the order they appear in each layer, so features that appear
    /// later (higher sort key) show up on top of features with a lower sort key. For symbols (text/icons), features
    /// that appear earlier (lower sort key) will show up at lower zoom levels than features that appear later.
    pub fn set_sort_key(&mut self, sort_key: i32) -> &mut Self {
        let clamped = sort_key.clamp(SORT_KEY_MIN, SORT_KEY_MAX);
        if clamped != sort_key
            && LAYERS_WITH_CLAMPED_SORT_KEY.lock().unwrap_or_else(|e| e.into_inner()).insert(self.layer.clone())
        {
            tracing::error!(
                "Sort key {} in layer {} is outside of allowed range [{}, {}], clamping to {} (logged once per layer)",
                sort_key,
                self.layer,
                SORT_KEY_MIN,
                SORT_KEY_MAX,
                clamped
            );
        }
        // ensure unit tests fail fast in this case
        debug_assert!(
            clamped == sort_key,
            "Sort key {sort_key} outside of allowed range [{SORT_KEY_MIN}, {SORT_KEY_MAX}]"
        );
        self.sort_key = clamped;
        self
    }

    /// Sets the value by which features are sorted from high to low within a layer in the output vector tile.
    pub fn set_sort_key_descending(&mut self, sort_key: i32) -> &mut Self {
        self.set_sort_key(SORT_KEY_MAX + SORT_KEY_MIN - sort_key)
    }

    /// Returns the minimum zoom level (inclusive) that this feature appears in.
    pub fn get_min_zoom(&self) -> u32 {
        self.minzoom
    }

    /// Sets the minimum zoom level (inclusive) that this feature appears in. Defaults to the minimum zoom of the map.
    pub fn set_min_zoom(&mut self, min: u32) -> &mut Self {
        self.minzoom = min.max(self.config().minzoom);
        self
    }

    /// Returns the maximum zoom level (inclusive) that this feature appears in.
    pub fn get_max_zoom(&self) -> u32 {
        self.maxzoom
    }

    /// Sets the maximum zoom level (inclusive) that this feature appears in. Defaults to the maximum zoom of the map.
    pub fn set_max_zoom(&mut self, max: u32) -> &mut Self {
        self.maxzoom = max.min(self.config().maxzoom);
        self
    }

    /// Returns the output vector tile layer that this feature will appear in.
    pub fn get_layer(&self) -> &str {
        &self.layer
    }

    /// Returns the geometry (in world web mercator coordinates) of this feature.
    pub fn get_geometry(&self) -> &Geometry<f64> {
        &self.geom
    }

    /// Returns the number of pixels of detail to render outside the visible tile boundary at `zoom`.
    pub fn get_buffer_pixels_at_zoom(&self, zoom: u32) -> f64 {
        zoom_function::apply_as_double_or_else(self.buffer_pixel_overrides.as_ref(), zoom, self.default_buffer_pixels)
    }

    /// Sets the default number of pixels of detail to render outside the visible tile boundary when no zoom-specific
    /// override is set.
    pub fn set_buffer_pixels(&mut self, buffer: f64) -> &mut Self {
        self.default_buffer_pixels = buffer;
        self
    }

    /// Sets zoom-specific overrides to the number of pixels of detail to render outside the visible tile boundary.
    /// When it returns `None` for a zoom, the default from [`Feature::set_buffer_pixels`] is used.
    pub fn set_buffer_pixel_overrides(&mut self, buffer: Option<ZoomFunction<f64>>) -> &mut Self {
        self.buffer_pixel_overrides = buffer;
        self
    }

    /// Returns the minimum resolution in tile pixels of features to emit at `zoom`: length for lines, and square root
    /// of the minimum area for polygons.
    pub fn get_min_pixel_size_at_zoom(&self, zoom: u32) -> f64 {
        if zoom == self.config().maxzoom_for_rendering {
            self.min_pixel_size_at_max_zoom
        } else {
            zoom_function::apply_as_double_or_else(self.min_pixel_size.as_ref(), zoom, self.default_min_pixel_size)
        }
    }

    /// Sets the minimum length of line features or square root of the minimum area of polygon features to emit below
    /// the maximum zoom-level of the map.
    pub fn set_min_pixel_size(&mut self, min_pixel_size: f64) -> &mut Self {
        self.default_min_pixel_size = min_pixel_size;
        self
    }

    /// Sets zoom-specific overrides to the minimum pixel size below the maximum zoom-level of the map.
    pub fn set_min_pixel_size_overrides(&mut self, levels: Option<ZoomFunction<f64>>) -> &mut Self {
        self.min_pixel_size = levels;
        self
    }

    /// Overrides the default minimum pixel size at and below `zoom` with `min_pixel_size`, replacing all previous
    /// zoom overrides.
    pub fn set_min_pixel_size_below_zoom(&mut self, zoom: u32, min_pixel_size: f64) -> &mut Self {
        if zoom >= self.config().maxzoom_for_rendering {
            self.min_pixel_size_at_max_zoom = min_pixel_size;
        }
        self.min_pixel_size = Some(ZoomFunction::max_zoom(zoom, min_pixel_size));
        self
    }

    /// Sets the minimum pixel size at the maximum zoom-level of the map (clients can "overzoom" past it, so this is
    /// typically much smaller than at lower zoom levels).
    pub fn set_min_pixel_size_at_max_zoom(&mut self, min_pixel_size: f64) -> &mut Self {
        self.min_pixel_size_at_max_zoom = min_pixel_size;
        self
    }

    /// Sets the minimum pixel size at all zoom levels, including the maximum zoom-level of the map. Does not replace
    /// overrides set with [`Feature::set_min_pixel_size_overrides`].
    pub fn set_min_pixel_size_at_all_zooms(&mut self, min_pixel_size: f64) -> &mut Self {
        self.min_pixel_size_at_max_zoom = min_pixel_size;
        self.set_min_pixel_size(min_pixel_size)
    }

    /// Returns the simplification tolerance for lines and polygons in tile pixels at `zoom`.
    pub fn get_pixel_tolerance_at_zoom(&self, zoom: u32) -> f64 {
        if zoom == self.config().maxzoom_for_rendering {
            self.pixel_tolerance_at_max_zoom
        } else {
            zoom_function::apply_as_double_or_else(self.pixel_tolerance.as_ref(), zoom, self.default_pixel_tolerance)
        }
    }

    /// Sets the fallback line and polygon simplify method when not overridden by
    /// [`Feature::set_simplify_method_overrides`].
    pub fn set_simplify_method(&mut self, strategy: SimplifyMethod) -> &mut Self {
        self.default_simplify_method = strategy;
        self
    }

    /// Set simplification algorithm to use at different zoom levels.
    pub fn set_simplify_method_overrides(&mut self, overrides: Option<ZoomFunction<SimplifyMethod>>) -> &mut Self {
        self.simplify_method = overrides;
        self
    }

    /// Returns the simplification method for lines and polygons at `zoom`.
    pub fn get_simplify_method_at_zoom(&self, zoom: u32) -> SimplifyMethod {
        zoom_function::apply_or_else(self.simplify_method.as_ref(), zoom, self.default_simplify_method)
    }

    /// Sets the default pipeline to apply to geometries scaled to tile coordinates right before emitting vector tile
    /// features. This runs instead of simplification, so should include any simplification if you want that.
    ///
    /// Geometries will be in scaled tile coordinates, so `0,0` is the northwest corner and `2^z, 2^z` is the
    /// southeast corner of the world.
    pub fn transform_scaled_geometry(&mut self, pipeline: GeometryPipelineRef) -> &mut Self {
        self.default_geometry_pipeline = Some(pipeline);
        self
    }

    /// Dynamically change the geometry pipeline to apply to scaled geometries at each zoom level. These run instead
    /// of simplification.
    pub fn transform_scaled_geometry_by_zoom(&mut self, overrides: ZoomPipeline) -> &mut Self {
        self.geometry_pipeline_by_zoom = Some(overrides);
        self
    }

    /// Returns the geometry transform to apply to scaled geometries at `zoom`, or `None` to apply the default
    /// simplification.
    pub fn get_scaled_geometry_transform_at_zoom(&self, zoom: u32) -> Option<GeometryPipelineRef> {
        self.geometry_pipeline_by_zoom.as_ref().and_then(|f| f(zoom)).or_else(|| self.default_geometry_pipeline.clone())
    }

    /// Sets the simplification tolerance for lines and polygons in tile pixels below the maximum zoom-level of the
    /// map. Does not replace overrides set with [`Feature::set_pixel_tolerance_overrides`].
    pub fn set_pixel_tolerance(&mut self, tolerance: f64) -> &mut Self {
        self.default_pixel_tolerance = tolerance;
        self
    }

    /// Sets the simplification tolerance for lines and polygons in tile pixels at the maximum zoom-level of the map.
    pub fn set_pixel_tolerance_at_max_zoom(&mut self, tolerance: f64) -> &mut Self {
        self.pixel_tolerance_at_max_zoom = tolerance;
        self
    }

    /// Sets the simplification tolerance for lines and polygons in tile pixels including at the maximum zoom-level.
    pub fn set_pixel_tolerance_at_all_zooms(&mut self, tolerance: f64) -> &mut Self {
        self.set_pixel_tolerance_at_max_zoom(tolerance).set_pixel_tolerance(tolerance)
    }

    /// Sets zoom-specific overrides to the simplification tolerance below the maximum zoom-level of the map.
    pub fn set_pixel_tolerance_overrides(&mut self, overrides: Option<ZoomFunction<f64>>) -> &mut Self {
        self.pixel_tolerance = overrides;
        self
    }

    /// Overrides the default simplification tolerance at and below `zoom` with `tolerance`, replacing all previous
    /// zoom overrides.
    pub fn set_pixel_tolerance_below_zoom(&mut self, zoom: u32, tolerance: f64) -> &mut Self {
        if zoom == self.config().maxzoom_for_rendering {
            self.pixel_tolerance_at_max_zoom = tolerance;
        }
        self.set_pixel_tolerance_overrides(Some(ZoomFunction::max_zoom(zoom, tolerance)))
    }

    pub fn has_label_grid(&self) -> bool {
        self.label_grid_pixel_size.is_some() || self.label_grid_limit.is_some()
    }

    /// Returns the size in pixels of the grid used to group or limit output points.
    ///
    /// Panics in debug builds when the label grid size is larger than the buffer pixel size.
    pub fn get_point_label_grid_pixel_size_at_zoom(&self, zoom: u32) -> f64 {
        let result =
            zoom_function::apply_as_double_or_else(self.label_grid_pixel_size.as_ref(), zoom, DEFAULT_LABEL_GRID_SIZE);
        debug_assert!(
            result <= self.get_buffer_pixels_at_zoom(zoom),
            "to avoid inconsistent rendering of the same point between adjacent tiles, buffer pixel size should be >= \
             label grid size but in '{}' buffer pixel size={} was less than label grid size={}",
            self.layer,
            self.get_buffer_pixels_at_zoom(zoom),
            result
        );
        result
    }

    /// Returns the maximum number of lowest-sort-key points to include in the output vector tile in each square of a
    /// grid with size [`Feature::get_point_label_grid_pixel_size_at_zoom`].
    pub fn get_point_label_grid_limit_at_zoom(&self, zoom: u32) -> i32 {
        zoom_function::apply_as_int_or_else(self.label_grid_limit.as_ref(), zoom, DEFAULT_LABEL_GRID_LIMIT)
    }

    /// Sets the size of a grid in tile pixels that will be used to compute a "location hash" of points rendered in
    /// each zoom-level for limiting the density of features in the output tile, or computing a "rank" key that
    /// clients can use to control label density.
    ///
    /// NOTE: the label grid is computed within each tile independently of its neighbors, so to ensure consistent
    /// limits and ranking of a point rendered in adjacent tiles, be sure to set the buffer pixel size to at least be
    /// larger than the label grid pixel size at each zoom level.
    pub fn set_point_label_grid_pixel_size_fn(&mut self, label_grid_size: Option<ZoomFunction<f64>>) -> &mut Self {
        self.label_grid_pixel_size = label_grid_size;
        self
    }

    /// Sets the label grid size in tile pixels at and below `maxzoom`.
    pub fn set_point_label_grid_pixel_size(&mut self, maxzoom: u32, size: f64) -> &mut Self {
        self.set_point_label_grid_pixel_size_fn(Some(ZoomFunction::max_zoom(maxzoom, size)))
    }

    /// Sets the maximum number of points with the lowest sort-key to include with the same label grid hash in a tile.
    pub fn set_point_label_grid_limit(&mut self, label_grid_limit: Option<ZoomFunction<i32>>) -> &mut Self {
        self.label_grid_limit = label_grid_limit;
        self
    }

    /// Limits the density of points on an output tile at and below `maxzoom` by only emitting the `limit` features
    /// with lowest sort-key in each square of a `size x size` pixel grid.
    pub fn set_point_label_grid_size_and_limit(&mut self, maxzoom: u32, size: f64, limit: i32) -> &mut Self {
        self.set_point_label_grid_pixel_size_fn(Some(ZoomFunction::max_zoom(maxzoom, size)))
            .set_point_label_grid_limit(Some(ZoomFunction::max_zoom(maxzoom, limit)))
    }

    fn compute_attrs_at_zoom(&self, zoom: u32) -> Arc<Tags> {
        let mut result = crate::value::tags_with_capacity(self.attrs.len() + self.zoom_attrs.len());
        let keep = |v: &Value| match v {
            Value::Null => false,
            Value::String(s) => !s.is_empty(),
            _ => true,
        };
        // merge two sorted, disjoint lists
        let mut plain = self.attrs.iter().peekable();
        let mut zoom_attrs = self.zoom_attrs.iter().peekable();
        loop {
            let take_plain = match (plain.peek(), zoom_attrs.peek()) {
                (None, None) => break,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some((a, _)), Some((b, _))) => a.as_str() < b.as_str(),
            };
            if take_plain {
                let (k, v) = plain.next().expect("peeked");
                if keep(v) {
                    result.insert(k.clone(), v.clone());
                }
            } else {
                let (k, v) = zoom_attrs.next().expect("peeked");
                if let Some(v) = v.at_zoom(zoom)
                    && keep(&v)
                {
                    result.insert(k.clone(), v);
                }
            }
        }
        Arc::new(result)
    }

    /// Returns the attributes to put on all output vector tile features at a zoom level.
    pub fn get_attrs_at_zoom(&self, zoom: u32) -> &Arc<Tags> {
        if !self.must_unwrap_values {
            return &self.attrs;
        }
        self.attr_cache
            .get_or_init(|| Box::new(CacheByZoom::new()))
            .get_or_init(zoom, |z| self.compute_attrs_at_zoom(z))
    }

    /// Returns the attribute key that the renderer should use to store the number of points in the simplified
    /// geometry before slicing it into tiles.
    pub fn get_num_points_attr(&self) -> Option<&str> {
        self.num_points_attr.as_deref()
    }

    /// Sets a special attribute key that the renderer will use to store the number of points in the simplified
    /// geometry before slicing it into tiles.
    pub fn set_num_points_attr(&mut self, num_points_attr: impl Into<String>) -> &mut Self {
        self.num_points_attr = Some(num_points_attr.into());
        self
    }

    /// Omit this feature from the output.
    pub fn omit(&mut self) -> &mut Self {
        self.omitted = true;
        self
    }

    /// True if this feature was omitted, or could not be constructed (and won't be emitted).
    pub fn is_omitted(&self) -> bool {
        self.omitted
    }

    /// Returns the actual pixel size of the source feature at `zoom` (length if line, sqrt(area) if polygon).
    pub fn get_source_feature_pixel_size_at_zoom(&self, zoom: u32) -> f64 {
        pixel_size_at_zoom(self.factory, self.source, zoom)
    }

    /// Returns a [`LinearRange`] that can be used to configure attributes that apply to only a portion of this line
    /// from `start` to `end` where 0 is the beginning of the line and 1 is the end.
    ///
    /// Since mapbox vector tiles can't handle this natively, the line will be broken up into multiple lines in the
    /// output tiles at each zoom level with the unique sets of tags on each line. Adjacent segments with the same
    /// tags will get merged into a single segment.
    pub fn linear_range(&mut self, start: f64, end: f64) -> LinearRange<'_, 'a> {
        LinearRange { feature: self, start, end }
    }

    /// Returns true if any attributes have been configured over a subset of this line.
    pub fn has_linear_ranges(&self) -> bool {
        self.partial_overrides.is_some()
    }

    /// Computes and returns the linear-scoped attributes of this line, and the geometry they apply to.
    pub fn get_linear_ranges_at_zoom(&self, zoom: u32) -> &[RangeWithTags] {
        if self.partial_overrides.is_none() {
            return &[];
        }
        self.partial_range_cache
            .get_or_init(|| Box::new(CacheByZoom::new()))
            .get_or_init(zoom, |z| self.compute_linear_ranges_at_zoom(z))
    }

    fn compute_linear_ranges_at_zoom(&self, zoom: u32) -> Vec<RangeWithTags> {
        #[derive(Clone, PartialEq)]
        struct Partial {
            omit: bool,
            attrs: Tags,
        }
        let identity = Partial { omit: false, attrs: (**self.get_attrs_at_zoom(zoom)).clone() };
        let mut result = MergingRangeMap::unit(identity, |a: &Partial, b: &Partial| {
            let mut attrs = a.attrs.clone();
            for (k, v) in &b.attrs {
                insert_sorted(&mut attrs, k.clone(), v.clone());
            }
            Partial { omit: b.omit, attrs }
        });
        for o in self.partial_overrides.iter().flatten() {
            result.update(o.start, o.end, |m| match &o.command {
                OverrideCommand::Attr(key, value) => {
                    Partial { omit: m.omit, attrs: with_attr(&m.attrs, key, value.at_zoom(zoom)) }
                }
                OverrideCommand::Maxzoom(maxzoom) => {
                    Partial { omit: m.omit || *maxzoom < zoom, attrs: m.attrs.clone() }
                }
                OverrideCommand::Minzoom(minzoom) => {
                    Partial { omit: m.omit || *minzoom > zoom, attrs: m.attrs.clone() }
                }
                OverrideCommand::Omit => Partial { omit: true, attrs: m.attrs.clone() },
            });
        }
        let ranges = result.into_result();
        let mut out = Vec::with_capacity(ranges.len());
        for range in ranges {
            if !range.value.omit {
                let geom = self
                    .source
                    .partial_line(range.start, range.end)
                    .unwrap_or_else(|e| panic!("Error computing partial line: {e:?}"));
                out.push(RangeWithTags {
                    start: range.start,
                    end: range.end,
                    geom,
                    attrs: Arc::new(range.value.attrs),
                });
            }
        }
        out
    }

    /// The source feature this came from.
    pub fn source(&self) -> &'a dyn SourceFeature {
        self.source
    }

    /// True if this feature has an empty geometry because it could not be constructed from the source feature.
    pub fn is_empty(&self) -> bool {
        self.empty
    }
}

impl fmt::Debug for Feature<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Feature{{layer='{}', geom={:?}, attrs={:?}, zoom_attrs={:?}}}",
            self.layer,
            self.geometry_type,
            self.attrs,
            self.zoom_attrs.iter().map(|(k, _)| k).collect::<Vec<_>>()
        )
    }
}

/// A builder that can be used to configure linear-scoped attributes for a partial segment of a line feature.
pub struct LinearRange<'f, 'a> {
    feature: &'f mut Feature<'a>,
    start: f64,
    end: f64,
}

impl<'f, 'a> LinearRange<'f, 'a> {
    fn add(&mut self, command: OverrideCommand) -> &mut Self {
        self.feature.invalidate_attr_caches();
        self.feature.partial_overrides.get_or_insert_with(Vec::new).push(Override {
            start: self.start,
            end: self.end,
            command,
        });
        self
    }

    fn source_feature(&self) -> &'a dyn SourceFeature {
        self.feature.source
    }

    fn set_attr_value(&mut self, key: String, value: AttrValue) {
        if matches!(value, AttrValue::Zoom(_)) {
            self.feature.must_unwrap_values = true;
        }
        self.add(OverrideCommand::Attr(key, value));
    }

    attr_methods!();

    /// Only include this segment at or above `min`.
    pub fn set_min_zoom(&mut self, min: u32) -> &mut Self {
        self.add(OverrideCommand::Minzoom(min))
    }

    /// Only include this segment at or below `max`.
    pub fn set_max_zoom(&mut self, max: u32) -> &mut Self {
        self.add(OverrideCommand::Maxzoom(max))
    }

    /// Exclude this segment of the line feature at all zoom levels.
    pub fn omit(&mut self) -> &mut Self {
        self.add(OverrideCommand::Omit)
    }

    /// Returns the full line [`Feature`] that this segment came from.
    pub fn entire_line(&mut self) -> &mut Feature<'a> {
        self.feature
    }

    /// Returns a segment of the full parent line (not the current segment) that can be configured further.
    pub fn linear_range(&mut self, start: f64, end: f64) -> LinearRange<'_, 'a> {
        self.feature.linear_range(start, end)
    }

    /// Returns the minimum zoom at which this segment is at least `min_pixel_size` pixels long.
    pub fn get_min_zoom_for_pixel_size(&self, min_pixel_size: f64) -> u32 {
        self.feature.get_min_zoom_for_pixel_size(min_pixel_size / (self.end - self.start))
    }
}

/// Helpers shared by tests in this crate (port of `TestUtils.toMap(Feature, zoom)` and friends).
#[cfg(test)]
pub mod test_support {
    use super::*;

    /// Returns the attributes of `feature` at `zoom` along with special `_`-prefixed keys describing the feature
    /// (Java `TestUtils.toMap`), except `_geom` which can be compared with [`Feature::get_geometry`].
    pub fn to_map(feature: &Feature<'_>, zoom: u32) -> Tags {
        let mut result: Tags = (**feature.get_attrs_at_zoom(zoom)).clone();
        let geom = feature.get_geometry();
        result.insert("_id".into(), feature.get_id().into());
        result.insert("_minzoom".into(), feature.get_min_zoom().into());
        result.insert("_maxzoom".into(), feature.get_max_zoom().into());
        result.insert("_buffer".into(), feature.get_buffer_pixels_at_zoom(zoom).into());
        result.insert("_layer".into(), feature.get_layer().into());
        result.insert("_sortkey".into(), feature.get_sort_key().into());
        result.insert("_labelgrid_limit".into(), feature.get_point_label_grid_limit_at_zoom(zoom).into());
        result.insert("_labelgrid_size".into(), feature.get_point_label_grid_pixel_size_at_zoom(zoom).into());
        result.insert("_minpixelsize".into(), feature.get_min_pixel_size_at_zoom(zoom).into());
        result.insert(
            "_type".into(),
            match GeometryType::type_of(geom) {
                GeometryType::Point => "point",
                GeometryType::Line => "line",
                GeometryType::Polygon => "polygon",
                GeometryType::Unknown => "GeometryCollection",
            }
            .into(),
        );
        result.insert("_numpointsattr".into(), feature.get_num_points_attr().into());
        result.sort_keys();
        result
    }

    /// Asserts that every key in `expected` has the same value in `actual`, where an expected `"<null>"` value
    /// means the key must be absent (Java `TestUtils.assertSubmap`).
    pub fn assert_submap(expected: &Tags, actual: &Tags) {
        let mut actual_filtered = std::collections::BTreeMap::new();
        let mut others = std::collections::BTreeMap::new();
        for (k, v) in actual {
            if expected.contains_key(k) {
                actual_filtered.insert(k.clone(), v.clone());
            } else {
                others.insert(k.clone(), v.clone());
            }
        }
        for (k, v) in expected {
            if v.as_str() == Some("<null>") && !actual.contains_key(k) {
                actual_filtered.insert(k.clone(), v.clone());
            }
        }
        let expected: std::collections::BTreeMap<_, _> = expected.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(expected, actual_filtered, "others: {others:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::geo::test_utils::*;
    use crate::geo::with_geometry::WithGeometry;
    use crate::reader::simple_feature::SimpleFeature;
    use crate::stats::stats::in_memory;
    use crate::tags;

    fn factory() -> Factory {
        Factory::new(PlanetilerConfig::defaults(), in_memory())
    }

    fn new_reader_feature(geom: impl Into<Geometry<f64>>, tags: Tags) -> SimpleFeature {
        SimpleFeature::create_tags(geom, tags)
    }

    fn assert_features(zoom: u32, expected: &[Tags], actual: &FeatureCollector<'_>) {
        let actual: Vec<_> = actual.iter().collect();
        assert_eq!(expected.len(), actual.len(), "size");
        for (e, a) in expected.iter().zip(actual) {
            assert_submap(e, &to_map(a, zoom));
        }
    }

    fn assert_geom(expected: impl Into<Geometry<f64>>, actual: &Feature<'_>) {
        assert_eq!(round(&expected.into()), round(actual.get_geometry()));
    }

    fn world_to_lat_lon(coords: &[f64]) -> Vec<f64> {
        coords.chunks(2).flat_map(|c| [geo_utils::get_world_lon(c[0]), geo_utils::get_world_lat(c[1])]).collect()
    }

    fn round_trip(world: impl Into<Geometry<f64>>) -> Geometry<f64> {
        geo_utils::lat_lon_to_world_coords(&geo_utils::world_to_lat_lon_coords(&world.into()))
    }

    #[test]
    fn test_sort_key_in_range_unchanged() {
        let factory = factory();
        let source = new_reader_feature(new_point(0.0, 0.0), tags! {});
        let mut collector = factory.get(&source);
        let f = collector.point("sortkey_in_range");
        f.set_sort_key(SORT_KEY_MIN);
        assert_eq!(SORT_KEY_MIN, f.get_sort_key());
        f.set_sort_key(SORT_KEY_MAX);
        assert_eq!(SORT_KEY_MAX, f.get_sort_key());
        f.set_sort_key(0);
        assert_eq!(0, f.get_sort_key());
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "outside of allowed range")]
    fn test_sort_key_out_of_range_panics_in_debug() {
        let factory = factory();
        let source = new_reader_feature(new_point(0.0, 0.0), tags! {});
        let mut collector = factory.get(&source);
        collector.point("sortkey_debug_panic").set_sort_key(SORT_KEY_MAX + 1);
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn test_sort_key_out_of_range_clamps_in_release() {
        let factory = factory();
        let source = new_reader_feature(new_point(0.0, 0.0), tags! {});
        let mut collector = factory.get(&source);
        let f = collector.point("sortkey_clamp");
        f.set_sort_key(SORT_KEY_MAX + 1);
        assert_eq!(SORT_KEY_MAX, f.get_sort_key());
        f.set_sort_key(SORT_KEY_MIN - 1);
        assert_eq!(SORT_KEY_MIN, f.get_sort_key());
        f.set_sort_key(i32::MIN);
        assert_eq!(SORT_KEY_MIN, f.get_sort_key());
        f.set_sort_key_descending(SORT_KEY_MIN - 1);
        assert_eq!(SORT_KEY_MAX, f.get_sort_key());
    }

    #[test]
    fn test_empty() {
        let factory = factory();
        let source = new_reader_feature(new_point(0.0, 0.0), tags! {"key" => "val"});
        let collector = factory.get(&source);
        assert_features(14, &[], &collector);
    }

    #[test]
    fn test_point() {
        let factory = factory();
        let source = new_reader_feature(new_point(0.0, 0.0), tags! {"key" => "val"});
        let mut collector = factory.get(&source);
        collector
            .point("layername")
            .set_zoom_range(12, 14)
            .set_sort_key(3)
            .set_attr("attr1", 2)
            .set_buffer_pixels(10.0)
            .set_buffer_pixel_overrides(Some(ZoomFunction::max_zoom(12, 100.0)))
            .set_point_label_grid_size_and_limit(12, 100.0, 10)
            .set_id(123456789);
        assert_features(
            14,
            &[tags! {
                "_layer" => "layername",
                "_minzoom" => 12,
                "_maxzoom" => 14,
                "_sortkey" => 3,
                "_labelgrid_size" => 0.0,
                "_labelgrid_limit" => 0,
                "attr1" => 2,
                "_type" => "point",
                "_buffer" => 10.0,
                "_id" => 123456789i64,
            }],
            &collector,
        );
        assert_features(
            12,
            &[tags! {
                "_labelgrid_size" => 100.0,
                "_labelgrid_limit" => 10,
                "_buffer" => 100.0,
                "_id" => 123456789i64,
            }],
            &collector,
        );
    }

    #[test]
    fn test_omit() {
        let factory = factory();
        let source = new_reader_feature(new_point(0.0, 0.0), tags! {"key" => "val"});
        let mut collector = factory.get(&source);
        collector.point("layername");
        assert_features(14, &[tags! {"_layer" => "layername"}], &collector);
        collector.output[0].omit();
        assert_features(14, &[], &collector);
    }

    #[test]
    fn test_attr_with_minzoom() {
        let factory = factory();
        let source = new_reader_feature(new_point(0.0, 0.0), tags! {"key" => "val"});
        let mut collector = factory.get(&source);
        collector.point("layername").set_attr("attr1", "value1").set_attr_with_minzoom("attr2", "value2", 13);
        assert_features(13, &[tags! {"attr1" => "value1", "attr2" => "value2"}], &collector);
        assert_features(12, &[tags! {"attr1" => "value1", "attr2" => "<null>"}], &collector);
    }

    #[test]
    fn test_line() {
        let factory = factory();
        let source = new_reader_feature(new_line_string(&[0.0, 0.0, 1.0, 1.0]), tags! {"key" => "val"});
        let mut collector = factory.get(&source);
        collector
            .line("layername")
            .set_zoom_range(12, 14)
            .set_sort_key(3)
            .set_attr("attr1", 2)
            .set_min_pixel_size(1.0)
            .set_min_pixel_size_below_zoom(12, 10.0);
        assert_features(
            13,
            &[tags! {
                "_layer" => "layername",
                "_minzoom" => 12,
                "_maxzoom" => 14,
                "_sortkey" => 3,
                "_minpixelsize" => 1.0,
                "attr1" => 2,
                "_type" => "line",
            }],
            &collector,
        );
    }

    #[test]
    fn test_polygon() {
        let factory = factory();
        let source = new_reader_feature(
            new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0]),
            tags! {"key" => "val"},
        );
        let mut collector = factory.get(&source);
        collector
            .polygon("layername")
            .set_zoom_range(12, 14)
            .set_sort_key(3)
            .set_attr("attr1", 2)
            .inherit_attr_from_source("key")
            .set_min_pixel_size(1.0)
            .set_min_pixel_size_below_zoom(12, 10.0);
        assert_features(
            13,
            &[tags! {
                "_layer" => "layername",
                "_minzoom" => 12,
                "_maxzoom" => 14,
                "_sortkey" => 3,
                "_minpixelsize" => 1.0,
                "attr1" => 2,
                "key" => "val",
                "_type" => "polygon",
            }],
            &collector,
        );
        assert_features(12, &[tags! {"_minpixelsize" => 10.0}], &collector);
    }

    fn poly_feature_test(f: impl FnOnce(&mut Feature<'_>)) {
        let factory = factory();
        let source = new_reader_feature(rectangle(10.0, 20.0), tags! {});
        let mut collector = factory.get(&source);
        f(collector.polygon("layername"));
    }

    #[test]
    fn test_min_size_at_max_zoom_defaults_to_tile_resolution() {
        poly_feature_test(|poly| {
            poly.set_min_pixel_size(1.0).set_min_pixel_size_below_zoom(12, 10.0);
            assert_eq!(10.0, poly.get_min_pixel_size_at_zoom(12));
            assert_eq!(1.0, poly.get_min_pixel_size_at_zoom(13));
            assert_eq!(256.0 / 4096.0, poly.get_min_pixel_size_at_zoom(14));
        });
    }

    #[test]
    fn test_set_min_size_at_max_zoom() {
        poly_feature_test(|poly| {
            poly.set_min_pixel_size(1.0).set_min_pixel_size_at_max_zoom(0.5).set_min_pixel_size_below_zoom(12, 10.0);
            assert_eq!(10.0, poly.get_min_pixel_size_at_zoom(12));
            assert_eq!(1.0, poly.get_min_pixel_size_at_zoom(13));
            assert_eq!(0.5, poly.get_min_pixel_size_at_zoom(14));
        });
    }

    #[test]
    fn test_set_min_size_at_all_zooms() {
        poly_feature_test(|poly| {
            poly.set_min_pixel_size_at_all_zooms(2.0).set_min_pixel_size_below_zoom(12, 10.0);
            assert_eq!(10.0, poly.get_min_pixel_size_at_zoom(12));
            assert_eq!(2.0, poly.get_min_pixel_size_at_zoom(13));
            assert_eq!(2.0, poly.get_min_pixel_size_at_zoom(14));
        });
    }

    #[test]
    fn test_default_min_pixel_size() {
        poly_feature_test(|poly| {
            assert_eq!(1.0, poly.get_min_pixel_size_at_zoom(12));
            assert_eq!(1.0, poly.get_min_pixel_size_at_zoom(13));
            assert_eq!(256.0 / 4096.0, poly.get_min_pixel_size_at_zoom(14));
        });
    }

    #[test]
    fn test_tolerance_default() {
        poly_feature_test(|poly| {
            assert_eq!(0.1, poly.get_pixel_tolerance_at_zoom(12));
            assert_eq!(0.1, poly.get_pixel_tolerance_at_zoom(13));
            assert_eq!(256.0 / 4096.0, poly.get_pixel_tolerance_at_zoom(14));
        });
    }

    #[test]
    fn test_set_tolerance() {
        poly_feature_test(|poly| {
            poly.set_pixel_tolerance(1.0);
            assert_eq!(1.0, poly.get_pixel_tolerance_at_zoom(12));
            assert_eq!(1.0, poly.get_pixel_tolerance_at_zoom(13));
            assert_eq!(256.0 / 4096.0, poly.get_pixel_tolerance_at_zoom(14));
        });
    }

    #[test]
    fn test_set_tolerance_at_all_zooms() {
        poly_feature_test(|poly| {
            poly.set_pixel_tolerance_at_all_zooms(1.0);
            assert_eq!(1.0, poly.get_pixel_tolerance_at_zoom(12));
            assert_eq!(1.0, poly.get_pixel_tolerance_at_zoom(13));
            assert_eq!(1.0, poly.get_pixel_tolerance_at_zoom(14));
        });
    }

    #[test]
    fn test_set_max_zoom() {
        poly_feature_test(|poly| {
            poly.set_pixel_tolerance_at_max_zoom(2.0);
            assert_eq!(0.1, poly.get_pixel_tolerance_at_zoom(12));
            assert_eq!(0.1, poly.get_pixel_tolerance_at_zoom(13));
            assert_eq!(2.0, poly.get_pixel_tolerance_at_zoom(14));
        });
    }

    #[test]
    fn test_set_all_zoom_methods() {
        poly_feature_test(|poly| {
            poly.set_pixel_tolerance(1.0).set_pixel_tolerance_at_max_zoom(2.0).set_pixel_tolerance_below_zoom(12, 3.0);
            assert_eq!(3.0, poly.get_pixel_tolerance_at_zoom(12));
            assert_eq!(1.0, poly.get_pixel_tolerance_at_zoom(13));
            assert_eq!(2.0, poly.get_pixel_tolerance_at_zoom(14));
        });
    }

    #[test]
    fn test_point_reader_feature_coercion() {
        let factory = factory();
        let source = new_reader_feature(new_point(0.0, 0.0), tags! {});
        assert_eq!(0.0, source.area().unwrap());
        assert_eq!(0.0, source.length().unwrap());
        let mut fc = factory.get(&source);
        fc.line("layer").set_zoom_range(0, 10);
        fc.polygon("layer").set_zoom_range(0, 10);
        assert!(fc.is_empty(), "silently fail coercing to line/polygon");
        fc.point("layer").set_zoom_range(0, 10);
        fc.centroid("layer").set_zoom_range(0, 10);
        fc.point_on_surface("layer").set_zoom_range(0, 10);
        let items: Vec<_> = fc.iter().collect();
        assert_eq!(3, items.len());
        for item in items {
            assert_eq!(GeometryType::Point, item.get_geometry_type());
            assert_eq!(&Geometry::Point(new_point(0.5, 0.5)), item.get_geometry());
        }
    }

    #[test]
    fn test_line_with_same_points_reader_feature_coercion() {
        for n in [2, 3, 4] {
            let factory = factory();
            let coords = vec![0.0; n * 2];
            let world_coords = vec![0.5; n * 2];
            let source = new_reader_feature(new_line_string(&coords), tags! {});
            assert_eq!(0.0, source.length().unwrap());
            assert_eq!(0.0, source.area().unwrap());
            let mut fc = factory.get(&source);
            fc.point("layer").set_zoom_range(0, 10);
            fc.polygon("layer").set_zoom_range(0, 10);
            assert!(fc.is_empty(), "silently fail coercing to point/polygon");
            fc.line("layer").set_zoom_range(0, 10);
            fc.centroid("layer").set_zoom_range(0, 10);
            fc.point_on_surface("layer").set_zoom_range(0, 10);
            let items: Vec<_> = fc.iter().collect();
            assert_eq!(3, items.len());
            assert_eq!(GeometryType::Line, items[0].get_geometry_type());
            assert_eq!(&Geometry::LineString(new_line_string(&world_coords)), items[0].get_geometry());
            assert_eq!(GeometryType::Point, items[1].get_geometry_type());
            assert_eq!(&Geometry::Point(new_point(0.5, 0.5)), items[1].get_geometry());
            assert_eq!(GeometryType::Point, items[2].get_geometry_type());
            assert_eq!(&Geometry::Point(new_point(0.5, 0.5)), items[2].get_geometry());
        }
    }

    #[test]
    fn test_non_zero_line_string_reader_feature_coercion() {
        let factory = factory();
        let source = new_reader_feature(
            new_line_string(&world_to_lat_lon(&[0.2, 0.2, 0.75, 0.75, 0.25, 0.75, 0.2, 0.2])),
            tags! {},
        );
        assert_eq!(0.0, source.area().unwrap());
        assert!((1.83008 - source.length().unwrap()).abs() < 1e-5);
        let mut fc = factory.get(&source);
        fc.point("layer").set_zoom_range(0, 10);
        fc.polygon("layer").set_zoom_range(0, 10);
        assert!(fc.is_empty(), "silently fail coercing to point/polygon");
        fc.line("layer").set_zoom_range(0, 10);
        fc.centroid("layer").set_zoom_range(0, 10);
        fc.point_on_surface("layer").set_zoom_range(0, 10);
        let items: Vec<_> = fc.iter().collect();
        assert_eq!(3, items.len());
        assert_eq!(GeometryType::Line, items[0].get_geometry_type());
        assert_geom(new_line_string(&[0.2, 0.2, 0.75, 0.75, 0.25, 0.75, 0.2, 0.2]), items[0]);
        assert_eq!(GeometryType::Point, items[1].get_geometry_type());
        assert_geom(new_point(0.40639, 0.55013), items[1]);
        assert_eq!(GeometryType::Point, items[2].get_geometry_type());
        assert_geom(new_point(0.25, 0.75), items[2]);
    }

    #[test]
    fn test_polygon_reader_feature_coercion() {
        let factory = factory();
        let source = new_reader_feature(
            new_polygon(&world_to_lat_lon(&[0.25, 0.25, 0.75, 0.75, 0.25, 0.75, 0.25, 0.25])),
            tags! {},
        );
        assert_eq!(0.125, source.area().unwrap());
        assert!((1.7071067811865475 - source.length().unwrap()).abs() < 1e-5);
        let mut fc = factory.get(&source);
        fc.point("layer").set_zoom_range(0, 10);
        fc.line("layer").set_zoom_range(0, 10);
        assert!(fc.is_empty(), "silently fail coercing to point or line");
        fc.polygon("layer").set_zoom_range(0, 10);
        fc.centroid("layer").set_zoom_range(0, 10);
        fc.point_on_surface("layer").set_zoom_range(0, 10);
        let items: Vec<_> = fc.iter().collect();
        assert_eq!(3, items.len());
        assert_eq!(GeometryType::Polygon, items[0].get_geometry_type());
        assert_geom(new_polygon(&[0.25, 0.25, 0.75, 0.75, 0.25, 0.75, 0.25, 0.25]), items[0]);
        assert_geom(new_point(0.4166667, 0.5833333), items[1]);
        assert_geom(new_point(0.375, 0.5), items[2]);
    }

    #[test]
    fn test_polygon_with_hole_coercion() {
        let factory = factory();
        let source = new_reader_feature(
            new_polygon_with_holes(
                new_coordinate_list(&world_to_lat_lon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0])),
                vec![new_coordinate_list(&world_to_lat_lon(&[
                    0.25, 0.25, 0.75, 0.25, 0.75, 0.75, 0.25, 0.75, 0.25, 0.25,
                ]))],
            ),
            tags! {},
        );
        assert!((0.75 - source.area().unwrap()).abs() < 1e-5);
        assert!((6.0 - source.length().unwrap()).abs() < 1e-5);
        let mut fc = factory.get(&source);
        fc.point("layer").set_zoom_range(0, 10);
        fc.line("layer").set_zoom_range(0, 10);
        assert!(fc.is_empty(), "silently fail coercing to point or line");
        fc.polygon("layer").set_zoom_range(0, 10);
        fc.centroid("layer").set_zoom_range(0, 10);
        fc.point_on_surface("layer").set_zoom_range(0, 10);
        let items: Vec<_> = fc.iter().collect();
        assert_eq!(3, items.len());
        assert_eq!(GeometryType::Polygon, items[0].get_geometry_type());
        assert_geom(
            new_polygon_with_holes(rectangle_coord_list(0.0, 1.0), vec![rectangle_coord_list(0.25, 0.75)]),
            items[0],
        );
        assert_geom(new_point(0.5, 0.5), items[1]);
        assert_geom(new_point(0.125, 0.5), items[2]);
    }

    #[test]
    fn test_point_on_surface() {
        let factory = factory();
        let source = new_reader_feature(
            new_polygon(&world_to_lat_lon(&[
                0.0, 0.0, 1.0, 0.0, 1.0, 0.25, 0.25, 0.25, 0.25, 0.75, 1.0, 0.75, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0,
            ])),
            tags! {},
        );
        let mut fc = factory.get(&source);
        fc.centroid("layer").set_zoom_range(0, 10);
        fc.point_on_surface("layer").set_zoom_range(0, 10);
        let items: Vec<_> = fc.iter().collect();
        assert_eq!(2, items.len());
        assert_geom(new_point(0.425, 0.5), items[0]);
        assert_geom(new_point(0.125, 0.5), items[1]);
    }

    #[test]
    fn test_innermost_point() {
        let factory = factory();
        let source = new_reader_feature(
            new_polygon(&world_to_lat_lon(&[0.0, 0.0, 1.0, 0.0, 1.0, 0.5, 0.5, 0.5, 0.5, 1.0, 0.0, 1.0, 0.0, 0.0])),
            tags! {},
        );
        let mut fc = factory.get(&source);
        fc.innermost_point("layer").set_zoom_range(0, 10);
        let items: Vec<_> = fc.iter().collect();
        assert_eq!(1, items.len());
        assert_eq!(GeometryType::Point, items[0].get_geometry_type());
        assert_eq!(round_to(&new_point(0.28, 0.28).into(), 1e2), round_to(items[0].get_geometry(), 1e2));
    }

    #[test]
    fn test_multi_polygon_coercion() {
        let factory = factory();
        let source = new_reader_feature(
            new_multi_polygon(vec![
                new_polygon(&world_to_lat_lon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0])),
                new_polygon(&world_to_lat_lon(&[2.0, 0.0, 3.0, 0.0, 3.0, 1.0, 2.0, 1.0, 2.0, 0.0])),
            ]),
            tags! {},
        );
        assert!((2.0 - source.area().unwrap()).abs() < 1e-5);
        assert!((8.0 - source.length().unwrap()).abs() < 1e-5);
        let mut fc = factory.get(&source);
        fc.point("layer").set_zoom_range(0, 10);
        fc.line("layer").set_zoom_range(0, 10);
        assert!(fc.is_empty(), "silently fail coercing to point or line");
        fc.polygon("layer").set_zoom_range(0, 10);
        fc.line("layer").set_zoom_range(0, 10);
        fc.centroid("layer").set_zoom_range(0, 10);
        fc.point_on_surface("layer").set_zoom_range(0, 10);
        let items: Vec<_> = fc.iter().collect();
        assert_eq!(3, items.len());
        assert_eq!(GeometryType::Polygon, items[0].get_geometry_type());
        assert_geom(new_multi_polygon(vec![rectangle(0.0, 1.0), rectangle_xy(2.0, 0.0, 3.0, 1.0)]), items[0]);
        assert_geom(new_point(1.5, 0.5), items[1]);
        assert_geom(new_point(0.5, 0.5), items[2]);
    }

    #[test]
    fn test_multi_line_string_coercion() {
        let factory = factory();
        let source = new_reader_feature(
            new_multi_line_string(vec![
                new_line_string(&world_to_lat_lon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0])),
                new_line_string(&world_to_lat_lon(&[2.0, 0.0, 3.0, 0.0, 3.0, 1.0, 2.0, 1.0, 2.0, 0.0])),
            ]),
            tags! {},
        );
        assert!(source.area().unwrap().abs() < 1e-5);
        assert!((8.0 - source.length().unwrap()).abs() < 1e-5);
        let mut fc = factory.get(&source);
        fc.point("layer").set_zoom_range(0, 10);
        fc.polygon("layer").set_zoom_range(0, 10);
        assert!(fc.is_empty(), "silently fail coercing to point/polygon");
        fc.line("layer").set_zoom_range(0, 10);
        fc.centroid("layer").set_zoom_range(0, 10);
        fc.point_on_surface("layer").set_zoom_range(0, 10);
        let items: Vec<_> = fc.iter().collect();
        assert_eq!(3, items.len());
        assert_eq!(GeometryType::Line, items[0].get_geometry_type());
        assert_geom(
            new_multi_line_string(vec![
                new_line_string_from_coords(rectangle_coord_list(0.0, 1.0)),
                new_line_string_from_coords(rectangle_coord_list_xy(2.0, 0.0, 3.0, 1.0)),
            ]),
            items[0],
        );
        assert_geom(new_point(1.5, 0.5), items[1]);
        assert_geom(new_point(1.0, 0.0), items[2]);
    }

    #[test]
    fn test_many_attr() {
        let factory = factory();
        let mut tags = Tags::default();
        for i in 0..500 {
            tags.insert(format!("key{i}"), format!("val{i}").into());
        }
        let source = new_reader_feature(new_point(0.0, 0.0), tags.clone());
        let mut collector = factory.get(&source);
        let point = collector.point("layername");
        for i in 0..500 {
            point.set_attr(format!("key{i}"), &tags[&format!("key{i}")]);
        }
        assert_features(
            13,
            &[tags! {
                "key0" => "val0",
                "key10" => "val10",
                "key100" => "val100",
                "key256" => "val256",
                "key499" => "val499",
            }],
            &collector,
        );
    }

    #[test]
    fn test_partial_line_feature() {
        let factory = factory();
        let source = new_reader_feature(new_line_string(&world_to_lat_lon(&[0.0, 0.0, 1.0, 0.0])), tags! {});
        let mut collector = factory.get(&source);
        collector.partial_line("layername", 0.25, 0.5).set_attr("k1", "v1");
        collector.partial_line("layername", 0.75, 1.0).set_attr("k2", "v2");
        assert_features(
            14,
            &[tags! {"k1" => "v1", "k2" => "<null>"}, tags! {"k1" => "<null>", "k2" => "v2"}],
            &collector,
        );
        let items: Vec<_> = collector.iter().collect();
        assert_geom(new_line_string(&[0.25, 0.0, 0.5, 0.0]), items[0]);
        assert_geom(new_line_string(&[0.75, 0.0, 1.0, 0.0]), items[1]);
    }

    #[test]
    fn test_linear_reference_tags() {
        let factory = factory();
        let source = new_reader_feature(new_line_string(&world_to_lat_lon(&[0.0, 0.0, 1.0, 0.0])), tags! {});
        let mut collector = factory.get(&source);
        collector
            .line("layername")
            .linear_range(0.1, 0.5)
            .set_attr("k1", "v1")
            .linear_range(0.3, 0.7)
            .set_attr("k2", "v2")
            .entire_line()
            .set_attr("k3", "v3");
        let feature = collector.iter().next().unwrap();
        assert!(feature.has_linear_ranges());
        assert_eq!(
            vec![
                RangeWithTags::new(0.0, 0.1, round_trip(new_line_string(&[0.0, 0.0, 0.1, 0.0])), tags! {"k3" => "v3"}),
                RangeWithTags::new(
                    0.1,
                    0.3,
                    round_trip(new_line_string(&[0.1, 0.0, 0.3, 0.0])),
                    tags! {"k1" => "v1", "k3" => "v3"}
                ),
                RangeWithTags::new(
                    0.3,
                    0.5,
                    round_trip(new_line_string(&[0.3, 0.0, 0.5, 0.0])),
                    tags! {"k1" => "v1", "k2" => "v2", "k3" => "v3"}
                ),
                RangeWithTags::new(
                    0.5,
                    0.7,
                    round_trip(new_line_string(&[0.5, 0.0, 0.7, 0.0])),
                    tags! {"k2" => "v2", "k3" => "v3"}
                ),
                RangeWithTags::new(0.7, 1.0, round_trip(new_line_string(&[0.7, 0.0, 1.0, 0.0])), tags! {"k3" => "v3"}),
            ],
            feature.get_linear_ranges_at_zoom(14)
        );
    }

    #[test]
    fn test_partial_minzoom() {
        let factory = factory();
        let source = new_reader_feature(new_line_string(&world_to_lat_lon(&[0.0, 0.0, 1.0, 0.0])), tags! {});
        let mut collector = factory.get(&source);
        collector.line("layername").linear_range(0.25, 0.75).set_min_zoom(14);
        let feature = collector.iter().next().unwrap();
        assert_geom(new_line_string(&[0.0, 0.0, 1.0, 0.0]), feature);
        assert!(feature.has_linear_ranges());
        assert_eq!(
            vec![
                RangeWithTags::new(0.0, 0.25, round_trip(new_line_string(&[0.0, 0.0, 0.25, 0.0])), tags! {}),
                RangeWithTags::new(0.75, 1.0, round_trip(new_line_string(&[0.75, 0.0, 1.0, 0.0])), tags! {}),
            ],
            feature.get_linear_ranges_at_zoom(13)
        );
        assert_eq!(
            vec![RangeWithTags::new(0.0, 1.0, round_trip(new_line_string(&[0.0, 0.0, 1.0, 0.0])), tags! {})],
            feature.get_linear_ranges_at_zoom(14)
        );
    }

    #[test]
    fn test_partial_omit() {
        let factory = factory();
        let source = new_reader_feature(new_line_string(&world_to_lat_lon(&[0.0, 0.0, 1.0, 0.0])), tags! {});
        let mut collector = factory.get(&source);
        collector.line("layername").linear_range(0.25, 0.75).omit();
        let feature = collector.iter().next().unwrap();
        assert!(feature.has_linear_ranges());
        let expected = vec![
            RangeWithTags::new(0.0, 0.25, round_trip(new_line_string(&[0.0, 0.0, 0.25, 0.0])), tags! {}),
            RangeWithTags::new(0.75, 1.0, round_trip(new_line_string(&[0.75, 0.0, 1.0, 0.0])), tags! {}),
        ];
        assert_eq!(expected, feature.get_linear_ranges_at_zoom(13));
        assert_eq!(expected, feature.get_linear_ranges_at_zoom(14));
    }

    fn struct_attrs() -> Vec<(&'static str, AttrValue)> {
        vec![("c", Struct::of("3").into()), ("d", ZoomFunction::min_zoom(9, Struct::of(true)).into())]
    }

    #[test]
    fn test_set_attr_struct() {
        let factory = factory();
        let source = new_reader_feature(new_point(0.0, 0.0), tags! {});
        let mut collector = factory.get(&source);
        collector
            .point("layername")
            .set_attr("a", Struct::of(1))
            .set_attr_with_minzoom("b", Struct::of(2.0), 9)
            .put_attrs(struct_attrs());
        let feature = collector.iter().next().unwrap();
        assert_eq!(&tags! {"a" => 1, "b" => 2.0, "c" => "3", "d" => true}, &**feature.get_attrs_at_zoom(14));
    }

    #[test]
    fn test_set_attr_partial() {
        let factory = factory();
        let source = new_reader_feature(new_line_string(&[0.0, 0.0, 1.0, 1.0]), tags! {});
        let mut collector = factory.get(&source);
        collector
            .line("layername")
            .linear_range(0.0, 0.5)
            .set_attr("a", Struct::of(1))
            .set_attr_with_minzoom("b", Struct::of(2.0), 9)
            .put_attrs(struct_attrs());
        let feature = collector.iter().next().unwrap();
        let sub = &feature.get_linear_ranges_at_zoom(14)[0];
        assert_eq!(&tags! {"a" => 1, "b" => 2.0, "c" => "3", "d" => true}, &*sub.attrs);
    }

    #[test]
    fn test_set_attr_partial_with_minzoom() {
        let factory = factory();
        let source = new_reader_feature(new_line_string(&[0.0, 0.0, 1.0, 1.0]), tags! {});
        let mut collector = factory.get(&source);
        collector
            .line("layername")
            .set_attr_with_minzoom("full", 1, 2)
            .set_attr_with_minzoom("fullstruct", Struct::of(2), 2)
            .linear_range(0.0, 0.5)
            .set_attr("a", Struct::of(1))
            .set_attr_with_minzoom("b", Struct::of(2.0), 9)
            .put_attrs(struct_attrs());
        let feature = collector.iter().next().unwrap();
        let sub = &feature.get_linear_ranges_at_zoom(14)[0];
        assert_eq!(&tags! {"a" => 1, "b" => 2.0, "c" => "3", "d" => true, "full" => 1, "fullstruct" => 2}, &*sub.attrs);
    }

    #[test]
    fn test_unwrap_struct() {
        let factory = factory();
        let source = new_reader_feature(new_line_string(&[0.0, 0.0, 1.0, 1.0]), tags! {});
        let mut collector = factory.get(&source);
        collector
            .line("layername")
            .set_attr("full", Struct::of(1))
            .linear_range(0.0, 0.5)
            .set_attr("partial", Struct::of(2));
        let feature = collector.iter().next().unwrap();
        let sub = &feature.get_linear_ranges_at_zoom(14)[0];
        assert_eq!(&tags! {"full" => 1, "partial" => 2}, &*sub.attrs);
    }

    #[test]
    fn test_unwrap_struct_full() {
        let factory = factory();
        let source = new_reader_feature(new_line_string(&[0.0, 0.0, 1.0, 1.0]), tags! {});
        let mut collector = factory.get(&source);
        collector.line("layername").set_attr("full", Struct::of(1));
        let feature = collector.iter().next().unwrap();
        assert_eq!(&tags! {"full" => 1}, &**feature.get_attrs_at_zoom(14));
    }

    #[test]
    fn test_unwrap_struct_full_with_minzoom() {
        let factory = factory();
        let source = new_reader_feature(new_line_string(&[0.0, 0.0, 1.0, 1.0]), tags! {});
        let mut collector = factory.get(&source);
        collector.line("layername").set_attr_with_minzoom("full", Struct::of(1), 2);
        let feature = collector.iter().next().unwrap();
        assert_eq!(&tags! {"full" => 1}, &**feature.get_attrs_at_zoom(14));
    }

    #[test]
    fn test_set_attr_partial_with_min_size() {
        let factory = factory();
        let source = new_reader_feature(new_line_string(&[0.0, 0.0, 1.0, 1.0]), tags! {});
        let mut collector = factory.get(&source);
        let line = collector.line("layername");
        assert_eq!(7, line.get_min_zoom_for_pixel_size(100.0));
        assert_eq!(7, line.linear_range(0.0, 0.5).get_min_zoom_for_pixel_size(50.0));
        assert_eq!(7, line.linear_range(0.0, 0.25).get_min_zoom_for_pixel_size(25.0));
    }

    #[test]
    fn test_line_midpoint() {
        let factory = factory();
        let source = new_reader_feature(new_line_string(&world_to_lat_lon(&[0.0, 0.0, 1.0, 0.0])), tags! {});
        let mut fc = factory.get(&source);
        fc.line_midpoint("layer").set_zoom_range(0, 10);
        let items: Vec<_> = fc.iter().collect();
        assert_eq!(1, items.len());
        assert_eq!(GeometryType::Point, items[0].get_geometry_type());
        assert_geom(new_point(0.5, 0.0), items[0]);
    }

    #[test]
    fn test_point_along_line() {
        let factory = factory();
        let source = new_reader_feature(new_line_string(&world_to_lat_lon(&[0.0, 0.0, 1.0, 0.0])), tags! {});
        let mut fc = factory.get(&source);
        fc.point_along_line("layer", 0.25).set_zoom_range(0, 10);
        let items: Vec<_> = fc.iter().collect();
        assert_eq!(1, items.len());
        assert_geom(new_point(0.25, 0.0), items[0]);
    }

    #[test]
    fn test_size_in_meters_of_line() {
        let source = new_reader_feature(new_line_string(&[0.0, 0.0, 1.0, 0.0]), tags! {});
        assert!((111_195.0 - source.length_meters().unwrap()).abs() < 1.0);
        assert!((111_195.0 - source.size_meters().unwrap()).abs() < 1.0);
        assert_eq!(0.0, source.area_meters().unwrap());
    }

    #[test]
    fn test_size_in_meters_of_polygon() {
        let source = new_reader_feature(rectangle(0.0, 1.0), tags! {});
        assert_eq!(0.0, source.length_meters().unwrap());
        assert!((111192.0 - source.size_meters().unwrap()).abs() < 1.0);
        assert!((111192.25757749f64.powi(2) - source.area_meters().unwrap()).abs() < 1.0);
    }

    #[test]
    fn test_size_in_meters_of_point() {
        let source = new_reader_feature(new_point(0.0, 1.0), tags! {});
        assert_eq!(0.0, source.length_meters().unwrap());
        assert_eq!(0.0, source.size_meters().unwrap());
        assert_eq!(0.0, source.area_meters().unwrap());
    }

    #[test]
    fn test_empty_string_and_null_attrs() {
        let factory = factory();
        let source = new_reader_feature(new_point(0.0, 0.0), tags! {"a" => "b"});
        let mut collector = factory.get(&source);
        collector
            .point("layer")
            .set_attr("null", Value::Null)
            .inherit_attr_from_source("missing")
            .inherit_attr_from_source("a")
            .set_attr("z", 1)
            .set_attr("z", ZoomFunction::min_zoom(5, 2))
            .set_attr("y", ZoomFunction::max_zoom(5, 3))
            .set_attr("y", 4);
        let feature = collector.iter().next().unwrap();
        assert_eq!(&tags! {"a" => "b", "y" => 4}, &**feature.get_attrs_at_zoom(4));
        assert_eq!(&tags! {"a" => "b", "y" => 4, "z" => 2}, &**feature.get_attrs_at_zoom(5));
    }
}
