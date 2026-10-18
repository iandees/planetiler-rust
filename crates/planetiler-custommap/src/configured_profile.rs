//! Port of `ConfiguredProfile.java`: a [`Profile`] configured from a YAML schema.

use std::collections::HashMap;
use std::path::PathBuf;

use planetiler_core::expression::expression::{Expression, match_type, or};
use planetiler_core::expression::multi_expression::{Index, MultiExpression, entry};
use planetiler_core::expression::simplifiable::Simplifiable;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::profile::Profile;
use planetiler_core::reader::osm::osm_element::Way;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::value::Value;
use planetiler_core::vector_tile;

use crate::config_expression_parser;
use crate::configschema::{FeatureLayer, SchemaConfig};
use crate::configured_feature::ConfiguredFeature;
use crate::contexts::{FeatureRef, Root};
use crate::expression::ParseException;
use crate::source::Source;
use crate::tag_value_producer::TagValueProducer;
use crate::type_conversion::OutType;

/// Port of `OsmReader.canBeLine`: true if an OSM way with these attributes can be a line.
pub fn can_be_line(closed: bool, area: Option<&str>, points: usize) -> bool {
    (!closed || area != Some("yes")) && points >= 2
}

/// A profile configured from a YAML schema.
pub struct ConfiguredProfile {
    schema: SchemaConfig,
    layers_by_id: HashMap<String, FeatureLayer>,
    features: Vec<ConfiguredFeature>,
    feature_layer_matcher: Index<usize>,
    tag_value_producer: TagValueProducer,
    root_context: Root,
    split_features_at_ways: Option<Expression>,
}

impl std::fmt::Debug for ConfiguredProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfiguredProfile").field("name", &self.schema.schema_name).finish()
    }
}

impl ConfiguredProfile {
    /// Builds a profile from `schema`, with argument values from `root_context`.
    pub fn new(schema: SchemaConfig, root_context: Root) -> Result<ConfiguredProfile, ParseException> {
        let layers = match &schema.layers {
            Some(layers) if !layers.is_empty() => layers.clone(),
            _ => return Err(ParseException::new("No layers defined")),
        };
        let tag_value_producer = TagValueProducer::new(schema.input_mappings.as_ref());
        let mut layers_by_id = HashMap::new();
        let mut features = Vec::new();
        let mut entries = Vec::new();
        let mut split_at_intersection_tests = Vec::new();
        for layer in &layers {
            layers_by_id.insert(layer.id.clone(), layer.clone());
            for feature in &layer.features {
                let configured = ConfiguredFeature::new(layer, &tag_value_producer, feature, &root_context)?;
                entries.push(entry(features.len(), configured.match_expression()));
                if configured.split_at_intersections() {
                    split_at_intersection_tests.push(configured.match_expression());
                }
                features.push(configured);
            }
        }
        let feature_layer_matcher = MultiExpression::of(entries).index();
        let split_features_at_ways = if split_at_intersection_tests.is_empty() {
            None
        } else {
            Some(
                or(split_at_intersection_tests)
                    .replace_matching(
                        &|e| matches!(e, Expression::MatchSource(_) | Expression::MatchSourceLayer(_)),
                        &Expression::TRUE,
                    )
                    .replace(&match_type("linestring"), &Expression::TRUE)
                    .replace_matching(&|e| matches!(e, Expression::MatchType(_)), &Expression::FALSE)
                    .simplify(),
            )
        };
        Ok(ConfiguredProfile {
            schema,
            layers_by_id,
            features,
            feature_layer_matcher,
            tag_value_producer,
            root_context,
            split_features_at_ways,
        })
    }

    pub fn schema(&self) -> &SchemaConfig {
        &self.schema
    }

    pub fn root_context(&self) -> &Root {
        &self.root_context
    }

    /// The configured layer with id `layer_id`.
    pub fn find_feature_layer(&self, layer_id: &str) -> Option<&FeatureLayer> {
        self.layers_by_id.get(layer_id)
    }

    fn evaluate(&self, expression: Option<&Value>) -> Result<Option<String>, ParseException> {
        let result = config_expression_parser::try_static_evaluate(&self.root_context, expression, OutType::String)
            .map_err(|e| ParseException::new(e.to_string()))?;
        Ok(match result {
            Value::Null => None,
            other => Some(other.to_string()),
        })
    }

    /// The sources defined in the schema with `url`, `local_path` and `projection` expressions evaluated.
    pub fn sources(&self) -> Result<Vec<Source>, ParseException> {
        let mut sources = Vec::new();
        for (key, value) in &self.schema.sources {
            let url = self.evaluate(value.url.as_ref())?;
            let path = self.evaluate(value.local_path.as_ref())?;
            let projection = self.evaluate(value.projection.as_ref())?;
            sources.push(Source::new(key.clone(), value.source_type, url, path.map(PathBuf::from), projection));
        }
        Ok(sources)
    }
}

impl Profile for ConfiguredProfile {
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        let context = self.root_context.create_process_feature_context(source_feature, &self.tag_value_producer);
        for matched in self.feature_layer_matcher.get_matches_with_triggers(&context) {
            let post_match = context.create_post_match_context(&matched.keys);
            self.features[*matched.result].process_feature(&post_match, features);
        }
    }

    fn post_process_layer_features(
        &self,
        layer: &str,
        _zoom: u32,
        _items: &mut Vec<vector_tile::Feature>,
    ) -> GeometryResult<()> {
        let Some(feature_layer) = self.find_feature_layer(layer) else {
            return Ok(());
        };
        let Some(_post_process) = &feature_layer.post_process else {
            return Ok(());
        };
        // TODO(port): FeatureMerge is not ported yet. When it is:
        // - merge_line_strings: FeatureMerge::merge_line_strings(items, min_length (min_length_at_max_zoom at
        //   config.maxzoom_for_rendering, falling back to config.min_feature_size(zoom)), tolerance
        //   (tolerance_at_max_zoom / config.tolerance(zoom)), buffer (merge.buffer, layer.buffer, or 4))
        // - merge_polygons: FeatureMerge::merge_overlapping_polygons(items, min_area (min_area_at_max_zoom at max
        //   zoom, falling back to config.min_feature_size(zoom)^2))
        Ok(())
    }

    fn name(&self) -> String {
        self.schema.schema_name.clone().unwrap_or_default()
    }

    fn description(&self) -> Option<String> {
        self.schema.schema_description.clone()
    }

    fn attribution(&self) -> Option<String> {
        Some(self.schema.attribution().to_string())
    }

    fn version(&self) -> Option<String> {
        self.schema.version.clone()
    }

    fn is_overlay(&self) -> bool {
        self.schema.is_overlay()
    }

    fn split_osm_way_at_intersections(&self, way: &Way) -> bool {
        let Some(split) = &self.split_features_at_ways else {
            return false;
        };
        let area = way.tags.get("area").and_then(Value::as_str);
        can_be_line(way.is_closed(), area, way.nodes.len())
            && split.evaluate(&self.root_context.create_context(FeatureRef::SplitTest(way), &self.tag_value_producer))
    }
}
