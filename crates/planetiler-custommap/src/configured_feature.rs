//! Port of `ConfiguredFeature.java`: a map feature, configured from a YAML configuration file.
//!
//! [`ConfiguredFeature::match_expression`] returns a filtering expression to limit input elements to ones this feature
//! cares about, and [`ConfiguredFeature::process_feature`] processes matching elements.

use std::collections::HashSet;
use std::fmt;

use indexmap::IndexMap;
use planetiler_core::expression::expression::{Expression, and, match_source, not, or};
use planetiler_core::expression::simplifiable::Simplifiable;
use planetiler_core::feature_collector::{Feature, FeatureCollector};
use planetiler_core::util::zoom_function::ZoomFunction;
use planetiler_core::value::Value;

use crate::boolean_expression_parser;
use crate::config_expression_parser;
use crate::configschema::{AttributeDefinition, FeatureGeometry, FeatureItem, FeatureLayer};
use crate::contexts::{FeatureRef, Root, ScriptContext};
use crate::expression::ParseException;
use crate::expression::config_expression::ConfigExpression;
use crate::expression::script_environment::ScriptEnvironment;
use crate::tag_value_producer::TagValueProducer;
use crate::type_conversion::OutType;

const LOG4: f64 = std::f64::consts::LN_2 * 2.0;

type Processor = Box<dyn Fn(&ScriptContext<'_>, &mut Feature<'_>) + Send + Sync>;

/// A map feature configured from YAML.
pub struct ConfiguredFeature {
    layer: String,
    geometry: FeatureGeometry,
    geometry_test: Expression,
    tag_test: Expression,
    feature_processors: Vec<Processor>,
    sources: HashSet<String>,
    split_at_intersections: bool,
}

impl fmt::Debug for ConfiguredFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfiguredFeature")
            .field("layer", &self.layer)
            .field("geometry", &self.geometry)
            .field("tag_test", &self.tag_test)
            .finish()
    }
}

fn clamp_zoom(zoom: i64) -> u32 {
    zoom.clamp(0, u32::MAX as i64) as u32
}

fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        other => other.as_f64().map(|d| d as i64),
    }
}

struct Builder<'a> {
    tag_value_producer: &'a TagValueProducer,
    feature_post_match_context: ScriptEnvironment,
    feature_attribute_context: ScriptEnvironment,
}

impl Builder<'_> {
    /// Returns a processor that evaluates `input` and passes non-null results to `consumer`.
    fn make_feature_processor(
        &self,
        input: Option<&Value>,
        clazz: OutType,
        consumer: impl Fn(&mut Feature<'_>, &Value) + Send + Sync + 'static,
    ) -> Result<Option<Processor>, ParseException> {
        let Some(input) = input else {
            return Ok(None);
        };
        let expression = config_expression_parser::parse(
            Some(input),
            self.tag_value_producer,
            &self.feature_post_match_context,
            clazz,
        )?;
        if expression.is_null_const() {
            return Ok(None);
        }
        Ok(Some(match expression {
            ConfigExpression::Const(value) => Box::new(move |_, feature| consumer(feature, &value)),
            expression => Box::new(move |context, feature| {
                let result = expression.apply(context);
                if !result.is_null() {
                    consumer(feature, &result);
                }
            }),
        }))
    }

    /// Produces logic that generates attribute values based on configuration and input data.
    fn attribute_value_producer(&self, attribute: &AttributeDefinition) -> Result<ConfigExpression, ParseException> {
        let attr_type = attribute.attr_type.as_ref();
        // some expression features are hoisted to the top-level for attribute values for brevity, so just map them to
        // what the equivalent expression syntax would be and parse as an expression.
        let mut value = planetiler_core::value::Tags::default();
        if attr_type.and_then(Value::as_str) == Some("match_key") {
            value.insert("value".into(), Value::str("${match_key}"));
        } else if attr_type.and_then(Value::as_str) == Some("match_value") {
            value.insert("value".into(), Value::str("${match_value}"));
        } else {
            if let Some(t) = attr_type {
                value.insert("type".into(), t.clone());
            }
            if let Some(c) = &attribute.coalesce {
                value.insert("coalesce".into(), c.clone());
            } else if let Some(v) = &attribute.value {
                value.insert("value".into(), v.clone());
            } else if let Some(t) = &attribute.tag_value {
                value.insert("tag_value".into(), Value::str(t));
            } else if let Some(a) = &attribute.arg_value {
                value.insert("arg_value".into(), Value::str(a));
            } else {
                value.insert("tag_value".into(), Value::str(&attribute.key));
            }
        }
        config_expression_parser::parse(
            Some(&Value::map(value)),
            self.tag_value_producer,
            &self.feature_post_match_context,
            OutType::Object,
        )
    }

    /// Generates logic which determines the minimum zoom level for an attribute based on a configured pixel size
    /// limit, a min zoom expression, and zoom levels by value.
    fn attribute_zoom_threshold(
        &self,
        min_tile_percent: Option<f64>,
        raw_min_zoom: &Value,
        min_zoom_by_value: IndexMap<Value, i32>,
    ) -> Result<Option<AttrZoom>, ParseException> {
        let result = config_expression_parser::parse(
            Some(raw_min_zoom),
            self.tag_value_producer,
            &self.feature_attribute_context,
            OutType::Integer,
        )?;
        if (result == ConfigExpression::Const(Value::Int(0)) || result.is_null_const()) && min_zoom_by_value.is_empty()
        {
            return Ok(None);
        }
        Ok(Some(AttrZoom { min_zoom: result, min_tile_percent, by_value: min_zoom_by_value }))
    }

    /// Generates a function which produces a fully-configured attribute for a feature.
    fn attribute_processor(&self, attribute: &AttributeDefinition) -> Result<Processor, ParseException> {
        let tag_key = attribute.key.clone();
        let attribute_min_zoom = attribute.min_zoom.clone().unwrap_or_else(|| Value::str("0"));
        // workaround because numeric keys are mapped as strings
        let min_zoom_by_value = self
            .tag_value_producer
            .remap_keys_by_type(&tag_key, attribute.min_zoom_by_value.as_ref().unwrap_or(&IndexMap::new()));
        let attribute_value_producer = self.attribute_value_producer(attribute)?;
        let fallback = attribute.fallback.clone();
        let include = match &attribute.include_when {
            None => Expression::TRUE,
            Some(v) => {
                boolean_expression_parser::parse(Some(v), self.tag_value_producer, &self.feature_post_match_context)?
            }
        };
        let exclude = match &attribute.exclude_when {
            None => Expression::TRUE,
            Some(v) => not(boolean_expression_parser::parse(
                Some(v),
                self.tag_value_producer,
                &self.feature_post_match_context,
            )?),
        };
        let attribute_test = and([include, exclude]).simplify();
        let min_tile_coverage = attribute.include_when.as_ref().and(attribute.min_tile_cover_size);
        let attribute_zoom_producer =
            self.attribute_zoom_threshold(min_tile_coverage, &attribute_min_zoom, min_zoom_by_value)?;

        Ok(Box::new(move |context, f| {
            let mut value = Value::Null;
            if attribute_test.evaluate(context) {
                value = attribute_value_producer.apply(context);
                if value.as_str() == Some("") {
                    value = Value::Null;
                }
            }
            if value.is_null()
                && let Some(fallback) = &fallback
            {
                value = fallback.clone();
            }
            if !value.is_null() {
                match &attribute_zoom_producer {
                    Some(zoom_producer) => {
                        let minzoom = zoom_producer.apply(&context.create_attr_zoom_context(&value));
                        f.set_attr_with_minzoom(tag_key.as_str(), value, clamp_zoom(minzoom as i64));
                    }
                    None => {
                        f.set_attr(tag_key.as_str(), value);
                    }
                }
            }
        }))
    }
}

/// Computes the minimum zoom of an attribute.
struct AttrZoom {
    min_zoom: ConfigExpression,
    min_tile_percent: Option<f64>,
    by_value: IndexMap<Value, i32>,
}

impl AttrZoom {
    fn apply(&self, context: &ScriptContext<'_>) -> i32 {
        if !self.by_value.is_empty()
            && let Some(value) = context.value()
            && let Some(zoom) = self.by_value.get(value)
        {
            // attribute value-specific zooms override static zooms
            return *zoom;
        }
        let result = as_i64(&self.min_zoom.apply(context)).unwrap_or(0) as i32;
        result.max(min_zoom_from_tile_percent(context, self.min_tile_percent))
    }
}

fn min_zoom_from_tile_percent(context: &ScriptContext<'_>, min_tile_percent: Option<f64>) -> i32 {
    let Some(min_tile_percent) = min_tile_percent else {
        return 0;
    };
    match context.feature().and_then(|f| f.with_geometry()).map(|g| g.area()) {
        Some(Ok(area)) => ((min_tile_percent / area).ln() / LOG4) as i32,
        _ => 14,
    }
}

fn max_ignoring_nulls(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (None, b) => b,
        (a, None) => a,
        (Some(a), Some(b)) => Some(a.max(b)),
    }
}

impl ConfiguredFeature {
    /// Builds a feature from its config in `layer`.
    pub fn new(
        layer: &FeatureLayer,
        tag_value_producer: &TagValueProducer,
        feature: &FeatureItem,
        root_context: &Root,
    ) -> Result<ConfiguredFeature, ParseException> {
        let sources: HashSet<String> = feature.source.iter().cloned().collect();
        let geometry_type = feature.geometry;
        // Test to determine whether this type of geometry is included
        let geometry_test = geometry_type.feature_test();
        let process_feature_context = root_context.process_feature_description();
        let builder = Builder {
            tag_value_producer,
            feature_post_match_context: root_context.feature_post_match_description(),
            feature_attribute_context: root_context.feature_attribute_description(),
        };

        // Test to determine whether this feature is included based on tagging
        let mut filter = match &feature.include_when {
            None => Expression::TRUE,
            Some(v) => boolean_expression_parser::parse(Some(v), tag_value_producer, &process_feature_context)?,
        };
        if !feature.source.is_empty() {
            filter = and([filter, or(feature.source.iter().map(|s| match_source(s)))]);
        }
        if let Some(exclude) = &feature.exclude_when {
            filter = and([
                filter,
                not(boolean_expression_parser::parse(Some(exclude), tag_value_producer, &process_feature_context)?),
            ]);
        }
        let tag_test = filter;

        // Configure logic for each attribute in the output tile
        let mut processors: Vec<Option<Processor>> = Vec::new();
        for attribute in &feature.attributes {
            processors.push(Some(builder.attribute_processor(attribute)?));
        }
        processors.push(builder.make_feature_processor(feature.id.as_ref(), OutType::Long, |f, v| {
            if let Some(id) = as_i64(v) {
                f.set_id(id);
            }
        })?);
        processors.push(builder.make_feature_processor(feature.min_zoom.as_ref(), OutType::Integer, |f, v| {
            if let Some(z) = as_i64(v) {
                f.set_min_zoom(clamp_zoom(z));
            }
        })?);
        processors.push(builder.make_feature_processor(feature.max_zoom.as_ref(), OutType::Integer, |f, v| {
            if let Some(z) = as_i64(v) {
                f.set_max_zoom(clamp_zoom(z));
            }
        })?);
        if let Some(buffer) = layer.buffer {
            processors.push(Some(Box::new(move |_, f| {
                f.set_buffer_pixels(buffer);
            })));
        }

        Self::add_post_processing_implications(&builder, layer, feature, &mut processors, root_context)?;

        // per-feature tolerance settings should take precedence over defaults from post-processing config
        processors.push(builder.make_feature_processor(feature.tolerance.as_ref(), OutType::Double, |f, v| {
            if let Some(d) = as_f64(v) {
                f.set_pixel_tolerance(d);
            }
        })?);
        processors.push(builder.make_feature_processor(
            feature.tolerance_at_max_zoom.as_ref(),
            OutType::Double,
            |f, v| {
                if let Some(d) = as_f64(v) {
                    f.set_pixel_tolerance_at_max_zoom(d);
                }
            },
        )?);
        if let Some(grid) = feature.point_label_grid {
            let pixel_size = grid.pixel_size.unwrap_or_default();
            let limit = grid.limit.unwrap_or_default();
            processors.push(Some(Box::new(move |_, f| {
                f.set_point_label_grid_pixel_size_fn(Some(ZoomFunction::max_zoom(
                    clamp_zoom(pixel_size.max_zoom as i64),
                    pixel_size.value,
                )));
                f.set_point_label_grid_limit(Some(ZoomFunction::max_zoom(
                    clamp_zoom(limit.max_zoom as i64),
                    limit.value,
                )));
            })));
        }
        processors.push(builder.make_feature_processor(feature.sort_key.as_ref(), OutType::Integer, |f, v| {
            if let Some(k) = as_i64(v) {
                f.set_sort_key(k as i32);
            }
        })?);
        processors.push(builder.make_feature_processor(
            feature.sort_key_descending.as_ref(),
            OutType::Integer,
            |f, v| {
                if let Some(k) = as_i64(v) {
                    f.set_sort_key_descending(k as i32);
                }
            },
        )?);

        Ok(ConfiguredFeature {
            layer: layer.id.clone(),
            geometry: geometry_type,
            geometry_test,
            tag_test,
            feature_processors: processors.into_iter().flatten().collect(),
            sources,
            split_at_intersections: geometry_type == FeatureGeometry::SplitLine,
        })
    }

    /// Considers implications of post-processing on the feature's processors.
    fn add_post_processing_implications(
        builder: &Builder<'_>,
        layer: &FeatureLayer,
        feature: &FeatureItem,
        processors: &mut Vec<Option<Processor>>,
        root_context: &Root,
    ) -> Result<(), ParseException> {
        let min_size = |f: &mut Feature<'_>, v: &Value| {
            if let Some(d) = as_f64(v) {
                f.set_min_pixel_size(d);
            }
        };
        let min_size_at_max_zoom = |f: &mut Feature<'_>, v: &Value| {
            if let Some(d) = as_f64(v) {
                f.set_min_pixel_size_at_max_zoom(d);
            }
        };
        let Some(post_process) = &layer.post_process else {
            processors.push(builder.make_feature_processor(feature.min_size.as_ref(), OutType::Double, min_size)?);
            processors.push(builder.make_feature_processor(
                feature.min_size_at_max_zoom.as_ref(),
                OutType::Double,
                min_size_at_max_zoom,
            )?);
            return Ok(());
        };
        // In order for post-processing to receive all features, the default min pixel sizes are zero when features
        // are collected
        let zero = Value::Int(0);
        processors.push(builder.make_feature_processor(
            Some(feature.min_size.as_ref().unwrap_or(&zero)),
            OutType::Double,
            min_size,
        )?);
        processors.push(builder.make_feature_processor(
            Some(feature.min_size_at_max_zoom.as_ref().unwrap_or(&zero)),
            OutType::Double,
            min_size_at_max_zoom,
        )?);
        // Implications of tile_post_process.merge_line_strings
        if let Some(merge) = &post_process.merge_line_strings {
            if let Some(tolerance) = merge.tolerance {
                processors.push(Some(Box::new(move |_, f| {
                    if f.is_line() {
                        f.set_pixel_tolerance(tolerance);
                    }
                })));
            }
            if let Some(tolerance) = merge.tolerance_at_max_zoom {
                processors.push(Some(Box::new(move |_, f| {
                    if f.is_line() {
                        f.set_pixel_tolerance_at_max_zoom(tolerance);
                    }
                })));
            }
            // merge_line_strings.min_length* and merge_line_strings.buffer
            let buffer = merge.buffer.or(layer.buffer);
            let buffer_pixels = max_ignoring_nulls(merge.min_length, buffer);
            let buffer_pixels_at_max_zoom = max_ignoring_nulls(merge.min_length_at_max_zoom, buffer);
            let max_zoom = root_context.config().maxzoom_for_rendering;
            if buffer_pixels.is_some() || buffer_pixels_at_max_zoom.is_some() {
                processors.push(Some(Box::new(move |_, f| {
                    if f.is_line() {
                        f.set_buffer_pixel_overrides(Some(ZoomFunction::new(move |z| {
                            if z == max_zoom { buffer_pixels_at_max_zoom } else { buffer_pixels }
                        })));
                    }
                })));
            }
        }
        // Implications of tile_post_process.merge_polygons
        if let Some(merge) = &post_process.merge_polygons {
            if let Some(tolerance) = merge.tolerance {
                processors.push(Some(Box::new(move |_, f| {
                    if f.is_polygon() {
                        f.set_pixel_tolerance(tolerance);
                    }
                })));
            }
            if let Some(tolerance) = merge.tolerance_at_max_zoom {
                processors.push(Some(Box::new(move |_, f| {
                    if f.is_polygon() {
                        f.set_pixel_tolerance_at_max_zoom(tolerance);
                    }
                })));
            }
            // TODO: merge_polygons.min_area* (same TODO as Java)
        }
        Ok(())
    }

    /// Returns an expression that evaluates to true if a source feature should be included in the output.
    pub fn match_expression(&self) -> Expression {
        and([self.geometry_test.clone(), self.tag_test.clone()])
    }

    /// The output layer of this feature.
    pub fn layer(&self) -> &str {
        &self.layer
    }

    /// Generates a tile feature based on a source feature.
    pub fn process_feature(&self, context: &ScriptContext<'_>, features: &mut FeatureCollector<'_>) {
        debug_assert!(
            self.sources.is_empty()
                || context.feature().and_then(|f: FeatureRef<'_>| f.source()).is_some_and(|s| self.sources.contains(s))
        );
        let f = self.geometry.new_feature(features, &self.layer);
        if !f.is_empty() {
            for processor in &self.feature_processors {
                processor(context, f);
            }
        }
    }

    /// True if this feature splits OSM ways at intersections.
    pub fn split_at_intersections(&self) -> bool {
        self.split_at_intersections
    }
}
