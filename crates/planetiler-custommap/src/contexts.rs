//! Port of `Contexts.java`: wrapper objects that provide all available inputs to different parts of planetiler schema
//! configs at runtime.
//!
//! Contexts provide inputs to Rust code, and also global variable definitions to CEL expressions. Contexts are nested
//! so that all global variables from a parent context are also available to its child context:
//!
//! - [`Root`]: argument values (`args.*`) and the [`PlanetilerConfig`], long-lived and shared.
//! - [`ContextKind::ProcessFeature`]: adds `feature.*` variables for the input feature being processed.
//! - [`ContextKind::FeaturePostMatch`]: adds `match_key` / `match_value` after a feature matched.
//! - [`ContextKind::FeatureAttribute`]: adds `value` once an attribute value has been computed.
//!
//! Java has a separate record class per nested context; in Rust they are all a [`ScriptContext`] (a cheap `Copy`
//! struct of borrowed references) tagged with a [`ContextKind`].

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, LazyLock};

use cel::common::value::Val;
use indexmap::IndexMap;
use planetiler_core::config::arguments::Arguments;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::expression::data_type::DataType;
use planetiler_core::expression::expression::ExpressionInput;
use planetiler_core::geo::with_geometry::WithGeometry;
use planetiler_core::reader::osm::osm_element::{Info, OsmElement, OsmElementType, Way};
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::reader::with_geometry_type::WithGeometryType;
use planetiler_core::reader::with_tags::WithTags;
use planetiler_core::value::{Tags, Value};
use tracing::error;

use crate::config_expression_parser;
use crate::expression::ParseException;
use crate::expression::script_environment::ScriptEnvironment;
use crate::expression::stdlib::value_to_static_val;
use crate::tag_value_producer::TagValueProducer;
use crate::type_conversion::OutType;

pub const FEATURE_TAGS: &str = "feature.tags";
pub const FEATURE_ID: &str = "feature.id";
pub const FEATURE_SOURCE: &str = "feature.source";
pub const FEATURE_SOURCE_LAYER: &str = "feature.source_layer";
pub const FEATURE_OSM_CHANGESET: &str = "feature.osm_changeset";
pub const FEATURE_OSM_VERSION: &str = "feature.osm_version";
pub const FEATURE_OSM_TIMESTAMP: &str = "feature.osm_timestamp";
pub const FEATURE_OSM_USER_ID: &str = "feature.osm_user_id";
pub const FEATURE_OSM_USER_NAME: &str = "feature.osm_user_name";
pub const FEATURE_OSM_FROM_NODE_ID: &str = "feature.osm_from_node_id";
pub const FEATURE_OSM_TO_NODE_ID: &str = "feature.osm_to_node_id";
pub const FEATURE_OSM_TYPE: &str = "feature.osm_type";
/// The feature geometry, exposed as a `geometry` object.
pub const FEATURE_GEOMETRY: &str = "feature";
pub const MATCH_KEY: &str = "match_key";
pub const MATCH_VALUE: &str = "match_value";
pub const VALUE: &str = "value";

/// Variables added by [`ContextKind::ProcessFeature`].
pub const PROCESS_FEATURE_VARIABLES: &[&str] = &[
    FEATURE_TAGS,
    FEATURE_ID,
    FEATURE_SOURCE,
    FEATURE_SOURCE_LAYER,
    FEATURE_OSM_CHANGESET,
    FEATURE_OSM_VERSION,
    FEATURE_OSM_TIMESTAMP,
    FEATURE_OSM_USER_ID,
    FEATURE_OSM_USER_NAME,
    FEATURE_OSM_FROM_NODE_ID,
    FEATURE_OSM_TO_NODE_ID,
    FEATURE_OSM_TYPE,
    FEATURE_GEOMETRY,
];

/// Variables added by [`ContextKind::FeaturePostMatch`].
pub const POST_MATCH_VARIABLES: &[&str] = &[MATCH_KEY, MATCH_VALUE];

/// Variables added by [`ContextKind::FeatureAttribute`].
pub const ATTRIBUTE_VARIABLES: &[&str] = &[VALUE];

/// Which nested context a [`ScriptContext`] is (Java's `Root`, `ProcessFeature`, `FeaturePostMatch` and
/// `FeatureAttribute` classes). Each kind has all the variables of the kinds before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ContextKind {
    Root,
    ProcessFeature,
    FeaturePostMatch,
    FeatureAttribute,
}

impl ContextKind {
    /// Returns true if `name` is a variable added by this kind or one of its parents (excluding root `args`).
    pub fn has_feature_variable(&self, name: &str) -> bool {
        (*self >= ContextKind::ProcessFeature && PROCESS_FEATURE_VARIABLES.contains(&name))
            || (*self >= ContextKind::FeaturePostMatch && POST_MATCH_VARIABLES.contains(&name))
            || (*self >= ContextKind::FeatureAttribute && ATTRIBUTE_VARIABLES.contains(&name))
    }
}

/// Root context available everywhere in a planetiler schema config: holds argument values parsed from the schema
/// config and command-line args. Cheap to clone.
#[derive(Clone)]
pub struct Root(Arc<RootInner>);

pub(crate) struct RootInner {
    arguments: Arguments,
    config: Arc<PlanetilerConfig>,
    argument_values: HashMap<String, Value>,
    built_in_args: HashSet<String>,
    /// CEL values for `args.<name>`.
    pub(crate) arg_bindings: HashMap<String, Box<dyn Val>>,
    /// CEL value for `args`.
    pub(crate) args_binding: Box<dyn Val>,
    /// Root variable declarations: `args` and `args.<name>`.
    declarations: HashSet<String>,
}

impl fmt::Debug for Root {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Root").field("args", &self.0.argument_values).finish()
    }
}

impl PartialEq for Root {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.0.argument_values == other.0.argument_values
    }
}

fn usize_value(v: usize) -> Value {
    Value::Int(v as i64)
}

impl Root {
    /// A root context with no arguments (Java `Contexts.emptyRoot()`).
    pub fn empty() -> Root {
        static EMPTY: LazyLock<Root> = LazyLock::new(|| {
            Root::new(Arguments::of().silence(), &HashMap::new()).expect("empty root context is valid")
        });
        EMPTY.clone()
    }

    fn new(arguments: Arguments, schema_args: &HashMap<String, Value>) -> Result<Root, ParseException> {
        let config = PlanetilerConfig::from(&arguments).map_err(|e| ParseException::new(e.to_string()))?;
        let mut values: HashMap<String, Value> = HashMap::new();
        values.insert("threads".into(), usize_value(config.threads));
        values.insert("write_threads".into(), usize_value(config.feature_write_threads));
        values.insert("process_threads".into(), usize_value(config.feature_process_threads));
        values.insert("feature_read_threads".into(), usize_value(config.feature_read_threads));
        values.insert("minzoom".into(), Value::Int(config.minzoom as i64));
        values.insert("maxzoom".into(), Value::Int(config.maxzoom as i64));
        values.insert("render_maxzoom".into(), Value::Int(config.maxzoom_for_rendering as i64));
        values.insert("force".into(), Value::Bool(config.force));
        values.insert("compress_temp".into(), Value::Bool(config.compress_temp_storage));
        values.insert("mmap_temp".into(), Value::Bool(config.mmap_temp_storage));
        values.insert("sort_max_readers".into(), usize_value(config.sort_max_readers));
        values.insert("sort_max_writers".into(), usize_value(config.sort_max_writers));
        values.insert("nodemap_type".into(), Value::str(&config.node_map_type));
        values.insert("nodemap_storage".into(), Value::str(&config.node_map_storage));
        values.insert("nodemap_madvise".into(), Value::Bool(config.node_map_madvise));
        values.insert("multipolygon_geometry_storage".into(), Value::str(&config.multipolygon_geometry_storage));
        values.insert("multipolygon_geometry_madvise".into(), Value::Bool(config.multipolygon_geometry_madvise));
        values.insert("http_user_agent".into(), Value::str(&config.http_user_agent));
        values.insert("http_retries".into(), Value::Int(config.http_retries as i64));
        values.insert("download_chunk_size_mb".into(), Value::Int(config.download_chunk_size_mb as i64));
        values.insert("download_threads".into(), usize_value(config.download_threads));
        values.insert("download_max_bandwidth".into(), Value::Double(config.download_max_bandwidth));
        values.insert("min_feature_size_at_max_zoom".into(), Value::Double(config.min_feature_size_at_max_zoom));
        values.insert("min_feature_size".into(), Value::Double(config.min_feature_size_below_max_zoom));
        values.insert("simplify_tolerance_at_max_zoom".into(), Value::Double(config.simplify_tolerance_at_max_zoom));
        values.insert("simplify_tolerance".into(), Value::Double(config.simplify_tolerance_below_max_zoom));
        values.insert("skip_filled_tiles".into(), Value::Bool(config.skip_filled_tiles));
        values.insert("tile_warning_size_mb".into(), usize_value(config.tile_warning_size_bytes));
        let built_in_args: HashSet<String> = values.keys().cloned().collect();

        for (k, v) in schema_args {
            values.entry(k.clone()).or_insert_with(|| v.clone());
        }
        for (k, v) in config.arguments.to_map() {
            values.entry(k).or_insert_with(|| Value::str(v));
        }

        let arg_bindings = values.iter().map(|(k, v)| (format!("args.{k}"), value_to_static_val(v))).collect();
        let args_map: Tags = values.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let args_binding = value_to_static_val(&Value::map(args_map));
        let mut declarations: HashSet<String> = values.keys().map(|k| format!("args.{k}")).collect();
        declarations.insert("args".into());

        Ok(Root(Arc::new(RootInner {
            arguments,
            config: Arc::new(config),
            argument_values: values,
            built_in_args,
            arg_bindings,
            args_binding,
            declarations,
        })))
    }

    pub(crate) fn inner(&self) -> &RootInner {
        &self.0
    }

    pub fn arguments(&self) -> &Arguments {
        &self.0.arguments
    }

    pub fn config(&self) -> &Arc<PlanetilerConfig> {
        &self.0.config
    }

    /// The value of argument `key` (Java `argument(key)`), or `None` if not set.
    pub fn argument(&self, key: &str) -> Option<&Value> {
        self.0.argument_values.get(key)
    }

    /// All argument values.
    pub fn argument_values(&self) -> &HashMap<String, Value> {
        &self.0.argument_values
    }

    /// Names of the built-in arguments that come from [`PlanetilerConfig`] and can't be redefined.
    pub fn built_in_args(&self) -> &HashSet<String> {
        &self.0.built_in_args
    }

    /// Returns true if `name` is a root-level variable (`args` or `args.<name>`).
    pub fn declares(&self, name: &str) -> bool {
        self.0.declarations.contains(name)
    }

    /// The script environment of this root context (Java `description()`).
    pub fn description(&self) -> ScriptEnvironment {
        ScriptEnvironment::new(ContextKind::Root, self.clone())
    }

    /// The script environment for expressions evaluated when processing a feature.
    pub fn process_feature_description(&self) -> ScriptEnvironment {
        ScriptEnvironment::new(ContextKind::ProcessFeature, self.clone())
    }

    /// The script environment for expressions evaluated after a feature matched.
    pub fn feature_post_match_description(&self) -> ScriptEnvironment {
        ScriptEnvironment::new(ContextKind::FeaturePostMatch, self.clone())
    }

    /// The script environment for expressions evaluated once an attribute value is known.
    pub fn feature_attribute_description(&self) -> ScriptEnvironment {
        ScriptEnvironment::new(ContextKind::FeatureAttribute, self.clone())
    }

    /// This root as a script input.
    pub fn as_context(&self) -> ScriptContext<'_> {
        ScriptContext {
            kind: ContextKind::Root,
            root: self,
            feature: None,
            tvp: TagValueProducer::empty(),
            match_keys: &[],
            value: None,
        }
    }

    /// Returns the context available when processing `source_feature`.
    pub fn create_process_feature_context<'a>(
        &'a self,
        source_feature: &'a dyn SourceFeature,
        tag_value_producer: &'a TagValueProducer,
    ) -> ScriptContext<'a> {
        self.create_context(FeatureRef::Source(source_feature), tag_value_producer)
    }

    /// Returns the context available when processing a feature.
    pub fn create_context<'a>(&'a self, feature: FeatureRef<'a>, tvp: &'a TagValueProducer) -> ScriptContext<'a> {
        ScriptContext {
            kind: ContextKind::ProcessFeature,
            root: self,
            feature: Some(feature),
            tvp,
            match_keys: &[],
            value: None,
        }
    }
}

/// Returns a [`Root`] context built from `schema_args` argument definitions and `orig_arguments` arguments provided
/// from the command-line/environment.
///
/// Arguments may depend on the value of other arguments so this iteratively evaluates the arguments until their values
/// settle.
///
/// Returns an error if the argument definitions are malformed, or if there's an infinite loop.
pub fn build_root_context(orig_arguments: &Arguments, schema_args: &Tags) -> Result<Root, ParseException> {
    let logging_enabled = !orig_arguments.silenced();
    orig_arguments.silence();
    let mut arg_descriptions: IndexMap<String, String> = IndexMap::new();
    let mut unparsed: IndexMap<String, Value> = schema_args.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut parsed: HashMap<String, Value> =
        orig_arguments.to_map().into_iter().map(|(k, v)| (k, Value::str(v))).collect();
    let mut result = Root::new(orig_arguments.clone(), &parsed)?;
    let mut arguments = orig_arguments.clone();
    let mut iters = 0;
    // arguments may reference the value of other arguments, so continue parsing until they all succeed...
    while !unparsed.is_empty() {
        let root = result.clone();
        let args = arguments.clone();
        let mut failures: Vec<(String, String)> = Vec::new();
        for (key, value) in unparsed.clone() {
            let builtin = root.built_in_args().contains(&key);
            let description: String;
            let default_value: Option<Value>;
            let mut data_type: Option<DataType> = None;
            if let Value::Map(map) = &value {
                if builtin {
                    return Err(ParseException::new(format!("Cannot override built-in argument: {key}")));
                }
                data_type = map.get("type").filter(|v| !v.is_null()).map(|t| DataType::from_id(&t.to_string()));
                description = map
                    .get("description")
                    .filter(|v| !v.is_null())
                    .map_or_else(|| "no description provided".to_string(), |d| d.to_string());
                default_value = map.get("default").filter(|v| !v.is_null()).cloned();
                if let Some(t) = data_type
                    && let Some(from_args) = args.get_string_opt(&key, &description)
                {
                    parsed.insert(key.clone(), t.convert_from(Some(&Value::str(from_args))));
                }
            } else {
                default_value = Some(value.clone());
                description = "no description provided".to_string();
            }
            arg_descriptions.insert(key.clone(), description.clone());
            match config_expression_parser::try_static_evaluate(&root, default_value.as_ref(), OutType::Object) {
                Ok(raw) => {
                    let as_string = raw.to_string();
                    let data_type = data_type.unwrap_or_else(|| DataType::type_of(&raw));
                    let string_result = args.get_string(&key, &description, &as_string);
                    let casted = data_type.convert_from(Some(&Value::str(&string_result)));
                    if casted.is_null() {
                        return Err(ParseException::new(format!(
                            "Cannot convert value for {key} to {}: {string_result}",
                            data_type.id()
                        )));
                    }
                    parsed.insert(key.clone(), casted);
                    unparsed.shift_remove(&key);
                }
                Err(e) => failures.push((key.clone(), e.to_string())),
            }
        }
        arguments = orig_arguments
            .or_else(Arguments::of_map(parsed.iter().map(|(k, v)| (k.clone(), v.to_string())).collect::<Vec<_>>()));
        result = Root::new(arguments.clone(), &parsed)?;
        if iters > 100 {
            for (key, failure) in &failures {
                error!("Error computing {key}:\n    {failure}");
            }
            let keys: Vec<&String> = unparsed.keys().collect();
            return Err(ParseException::new(format!("Infinite loop while processing arguments: {keys:?}")));
        }
        iters += 1;
    }
    let final_arguments = if logging_enabled { arguments.with_exactly_once_logging() } else { arguments.silence() };
    if logging_enabled {
        for (key, description) in &arg_descriptions {
            final_arguments.get_string_opt(key, description);
        }
    }
    Root::new(final_arguments, &parsed)
}

/// Geometry type of features used to test whether an OSM way should be split (always a line).
struct LineOnly;

impl WithGeometryType for LineOnly {
    fn is_point(&self) -> bool {
        false
    }

    fn can_be_polygon(&self) -> bool {
        false
    }

    fn can_be_line(&self) -> bool {
        true
    }
}

/// The input feature of a [`ScriptContext`].
#[derive(Clone, Copy)]
pub enum FeatureRef<'a> {
    /// A source feature from a reader.
    Source(&'a dyn SourceFeature),
    /// An OSM way with no geometry, to test whether it should be split at intersections (Java
    /// `TestForSplitFeature`): it can only be a line and its geometry is empty.
    SplitTest(&'a Way),
}

impl fmt::Debug for FeatureRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeatureRef::Source(s) => write!(f, "{s:?}"),
            FeatureRef::SplitTest(w) => write!(f, "SplitTest(way {})", w.id),
        }
    }
}

/// OSM element information for a feature.
#[derive(Clone, Copy)]
pub enum OsmRef<'a> {
    Element(&'a OsmElement),
    Way(&'a Way),
}

impl<'a> OsmRef<'a> {
    pub fn element_type(&self) -> OsmElementType {
        match self {
            OsmRef::Element(e) => e.element_type(),
            OsmRef::Way(_) => OsmElementType::Way,
        }
    }

    pub fn info(&self) -> Option<&'a Info> {
        match self {
            OsmRef::Element(e) => e.info(),
            OsmRef::Way(w) => w.info.as_ref(),
        }
    }

    /// Node IDs if this is a way.
    pub fn way_nodes(&self) -> Option<&'a [i64]> {
        match *self {
            OsmRef::Element(OsmElement::Way(w)) => Some(&w.nodes),
            OsmRef::Way(w) => Some(&w.nodes),
            _ => None,
        }
    }
}

/// Lowercase name of an OSM element type (`node`, `way`, `relation`, `other`).
pub fn osm_type_name(t: OsmElementType) -> &'static str {
    match t {
        OsmElementType::Node => "node",
        OsmElementType::Way => "way",
        OsmElementType::Relation => "relation",
        OsmElementType::Other => "other",
    }
}

static EMPTY_TAGS: LazyLock<Tags> = LazyLock::new(Tags::default);

impl<'a> FeatureRef<'a> {
    pub fn tags(&self) -> &'a Tags {
        match self {
            FeatureRef::Source(s) => planetiler_core::reader::with_tags::WithTags::tags(*s),
            FeatureRef::SplitTest(w) => &w.tags,
        }
    }

    pub fn id(&self) -> i64 {
        match self {
            FeatureRef::Source(s) => s.id(),
            FeatureRef::SplitTest(w) => w.id,
        }
    }

    pub fn source(&self) -> Option<&'a str> {
        match self {
            FeatureRef::Source(s) => s.base().source.as_deref(),
            FeatureRef::SplitTest(_) => None,
        }
    }

    pub fn source_layer(&self) -> Option<&'a str> {
        match self {
            FeatureRef::Source(s) => s.base().source_layer.as_deref(),
            FeatureRef::SplitTest(_) => None,
        }
    }

    pub fn osm(&self) -> Option<OsmRef<'a>> {
        match self {
            FeatureRef::Source(s) => s.original_element().map(OsmRef::Element),
            FeatureRef::SplitTest(w) => Some(OsmRef::Way(w)),
        }
    }

    /// The source feature geometry, if any.
    pub fn with_geometry(&self) -> Option<&'a dyn WithGeometry> {
        match self {
            FeatureRef::Source(s) => Some(*s as &dyn WithGeometry),
            FeatureRef::SplitTest(_) => None,
        }
    }

    pub fn source_feature(&self) -> Option<&'a dyn SourceFeature> {
        match self {
            FeatureRef::Source(s) => Some(*s),
            FeatureRef::SplitTest(_) => None,
        }
    }

    pub fn geometry_type(&self) -> &'a dyn WithGeometryType {
        match self {
            FeatureRef::Source(s) => *s as &dyn WithGeometryType,
            FeatureRef::SplitTest(_) => &LineOnly,
        }
    }
}

/// The runtime environment of an executing expression: the root context, plus (depending on [`ContextKind`]) the
/// input feature, the keys that caused it to match, and an attribute value.
#[derive(Clone, Copy)]
pub struct ScriptContext<'a> {
    kind: ContextKind,
    root: &'a Root,
    feature: Option<FeatureRef<'a>>,
    tvp: &'a TagValueProducer,
    match_keys: &'a [Arc<str>],
    value: Option<&'a Value>,
}

impl fmt::Debug for ScriptContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScriptContext")
            .field("kind", &self.kind)
            .field("feature", &self.feature)
            .field("match_keys", &self.match_keys)
            .field("value", &self.value)
            .finish()
    }
}

impl<'a> ScriptContext<'a> {
    pub fn kind(&self) -> ContextKind {
        self.kind
    }

    pub fn root(&self) -> &'a Root {
        self.root
    }

    pub fn feature(&self) -> Option<FeatureRef<'a>> {
        self.feature
    }

    pub fn tag_value_producer(&self) -> &'a TagValueProducer {
        self.tvp
    }

    pub fn match_keys(&self) -> &'a [Arc<str>] {
        self.match_keys
    }

    /// The attribute value for a [`ContextKind::FeatureAttribute`] context.
    pub fn value(&self) -> Option<&'a Value> {
        self.value
    }

    pub fn argument(&self, key: &str) -> Option<&'a Value> {
        self.root.argument(key)
    }

    /// Returns the context available after a feature has been matched by tags `match_keys`.
    pub fn create_post_match_context(&self, match_keys: &'a [Arc<str>]) -> ScriptContext<'a> {
        ScriptContext { kind: ContextKind::FeaturePostMatch, match_keys, ..*self }
    }

    /// Returns the context available when configuring an attribute with value `value`.
    pub fn create_attr_zoom_context(&self, value: &'a Value) -> ScriptContext<'a> {
        ScriptContext { kind: ContextKind::FeatureAttribute, value: Some(value), ..*self }
    }

    /// The first tag key that caused the feature to match.
    pub fn match_key(&self) -> Option<&'a str> {
        self.match_keys.first().map(|k| &**k)
    }

    /// The value of the first tag key that caused the feature to match.
    pub fn match_value(&self) -> Value {
        match self.match_key() {
            None => Value::Null,
            Some(key) => self.tvp.value_for_key(self, key),
        }
    }

    /// Returns the value of a variable by name (Java `ScriptContext.apply(name)`), or [`Value::Null`] if missing.
    pub fn variable(&self, name: &str) -> Value {
        if let Some(arg) = name.strip_prefix("args.") {
            return self.argument(arg).cloned().unwrap_or_default();
        }
        if name == "args" {
            return Value::map(self.root.argument_values().iter().map(|(k, v)| (k.clone(), v.clone())).collect());
        }
        if !self.kind.has_feature_variable(name) {
            return Value::Null;
        }
        match name {
            MATCH_KEY => return self.match_key().map_or(Value::Null, Value::str),
            MATCH_VALUE => return self.match_value(),
            VALUE => return self.value.cloned().unwrap_or_default(),
            _ => {}
        }
        let Some(feature) = self.feature else {
            return Value::Null;
        };
        match name {
            FEATURE_TAGS => Value::map(self.tvp.map_tags(self).into_owned()),
            FEATURE_ID => Value::Int(feature.id()),
            FEATURE_SOURCE => feature.source().map_or(Value::Null, Value::str),
            FEATURE_SOURCE_LAYER => feature.source_layer().map_or(Value::Null, Value::str),
            FEATURE_GEOMETRY => Value::Null,
            _ => {
                let Some(osm) = feature.osm() else {
                    return Value::Null;
                };
                match name {
                    FEATURE_OSM_TYPE => Value::str(osm_type_name(osm.element_type())),
                    FEATURE_OSM_FROM_NODE_ID => {
                        osm.way_nodes().and_then(|n| n.first()).map_or(Value::Null, |n| Value::Int(*n))
                    }
                    FEATURE_OSM_TO_NODE_ID => {
                        osm.way_nodes().and_then(|n| n.last()).map_or(Value::Null, |n| Value::Int(*n))
                    }
                    _ => match osm.info() {
                        None => Value::Null,
                        Some(info) => match name {
                            FEATURE_OSM_CHANGESET => Value::Int(info.changeset),
                            FEATURE_OSM_VERSION => Value::Int(info.version as i64),
                            FEATURE_OSM_TIMESTAMP => Value::Int(info.timestamp),
                            FEATURE_OSM_USER_ID => Value::Int(info.user_id as i64),
                            FEATURE_OSM_USER_NAME => Value::str(&info.user),
                            _ => Value::Null,
                        },
                    },
                }
            }
        }
    }

    /// Recovers a [`ScriptContext`] from an expression input (Java's `instanceof` check on the context class).
    pub fn from_input<'b>(input: &'b dyn ExpressionInput) -> Option<&'b ScriptContext<'b>> {
        let erased = input.as_any()?.downcast_ref::<ErasedScriptContext>()?;
        // shorten the erased 'static lifetime back down to the borrow of the input (ScriptContext is covariant)
        Some(shorten(&erased.0))
    }
}

/// A [`ScriptContext`] whose lifetime has been erased to `'static` so it can be identified through `TypeId`.
///
/// Deliberately private to this module: [`ExpressionInput::as_any`] hands out a `&dyn Any` pointing at one of these,
/// and the only code that can name the type (and so downcast to it) is [`ScriptContext::from_input`], which shortens
/// the lifetime back to the borrow of the input. Downcasting to the public `ScriptContext<'static>` fails, so no
/// reference with an extended lifetime can escape.
#[repr(transparent)]
struct ErasedScriptContext(ScriptContext<'static>);

fn shorten<'b>(context: &'b ScriptContext<'static>) -> &'b ScriptContext<'b> {
    context
}

impl WithTags for ScriptContext<'_> {
    fn tags(&self) -> &Tags {
        match &self.feature {
            Some(f) => f.tags(),
            None => &EMPTY_TAGS,
        }
    }
}

impl ExpressionInput for ScriptContext<'_> {
    fn as_with_geometry_type(&self) -> Option<&dyn WithGeometryType> {
        self.feature.map(|f| f.geometry_type())
    }

    fn source(&self) -> Option<&str> {
        self.feature.and_then(|f| f.source())
    }

    fn source_layer(&self) -> Option<&str> {
        self.feature.and_then(|f| f.source_layer())
    }

    fn as_any(&self) -> Option<&dyn Any> {
        // SAFETY: lifetime-only cast: the layout of `ScriptContext<'a>` does not depend on `'a`, and
        // `ErasedScriptContext` is `repr(transparent)` over it. `ErasedScriptContext` is private to this module, so
        // the only code that can downcast to it is `ScriptContext::from_input`, which immediately shortens the
        // lifetime back to the borrow of `self`; nothing else can observe the erased `'static`.
        let erased: &ErasedScriptContext =
            unsafe { &*(self as *const ScriptContext<'_> as *const ErasedScriptContext) };
        Some(erased)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_input_recovers_context() {
        let root = Root::empty();
        let ctx = root.as_context();
        let input: &dyn ExpressionInput = &ctx;
        let recovered = ScriptContext::from_input(input).expect("script context");
        assert_eq!(ctx.kind(), recovered.kind());
        assert!(std::ptr::eq(recovered, &ctx));
    }

    #[test]
    fn cannot_downcast_to_public_static_context() {
        // Code outside this module can only name the public type; downcasting to it must fail so that a `'static`
        // reference into the feature/value can't be smuggled out through the accessors.
        let root = Root::empty();
        let ctx = root.as_context();
        let input: &dyn ExpressionInput = &ctx;
        let any = input.as_any().expect("as_any");
        assert!(any.downcast_ref::<ScriptContext<'static>>().is_none());
    }

    #[test]
    fn other_inputs_are_not_contexts() {
        let tags = Tags::default();
        assert!(ScriptContext::from_input(&tags).is_none());
    }
}
