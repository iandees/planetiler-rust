//! Port of the `custommap.expression.stdlib` package (`PlanetilerStdLib`, `GeometryVal`, `PlanetilerTypeRegistry`)
//! on top of the [`cel`] crate, plus the glue between [`ScriptContext`]s and CEL evaluation.
//!
//! # Differences from cel-java
//!
//! - cel-rust has no type checker: Java rejects scripts that reference undeclared variables or that don't type-check
//!   at parse time. Here [`check_references`] rejects references to undeclared variables (taking comprehension
//!   variables into account), but type errors (`1 + 'a'`) and calls to unknown functions are only detected when the
//!   script runs (they are logged and the result is `null`).
//! - `has(m.f)` on a map is true when the key is present with a non-null value (`feature.tags` returns `null` for
//!   missing keys, like Java's `mapWithDefault`), implemented by rewriting presence tests to an internal function.
//! - Equality and `has(map, key, values...)` use cel-rust semantics where `1 == 1.0` is true (cel-java: false).
//! - `replaceRegex` uses the `regex` crate syntax (no look-around or backreferences in patterns); Java-style
//!   `$1` group references in the replacement are supported.
//! - Geometry values (`feature`, `feature.centroid`, ...) can't be returned as an expression result: they convert to
//!   `null`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, OnceLock};

use cel::common::ast::{CallExpr, Expr, IdedExpr, LiteralValue};
use cel::common::traits::{self, Container, Indexer, Iterable, Sizer};
use cel::common::types::{
    CelBool, CelBytes, CelDouble, CelInt, CelList, CelMap, CelMapKey, CelNull, CelString, CelUInt, DOUBLE_TYPE,
    DYN_TYPE, INT_TYPE, Kind, LIST_TYPE, MAP_TYPE, STRING_TYPE, Type,
};
use cel::common::value::{CowVal, Val};
use cel::context::VariableResolver;
use cel::{Env, ExecutionError};
use geo::{BoundingRect, CoordsIter};
use geo_types::Geometry;
use planetiler_core::geo::geo_utils;
use planetiler_core::geo::unit;
use planetiler_core::geo::with_geometry::{FromWorld, WithGeometry, from_world_geometry};
use planetiler_core::value::{Tags, Value};
use regex::Regex;

use crate::contexts::{self, ContextKind, ScriptContext, osm_type_name};
use crate::expression::script_environment::ScriptEnvironment;
use crate::tag_value_producer::TagValueProducer;

const VARARG_LIMIT: usize = 32;
const HAS_FIELD: &str = "@planetiler_has_field";

/// The CEL type of geometry values.
pub static GEOMETRY_TYPE: Type = Type::new_unspecified_type("geometry");

/// The shared CEL environment: the standard library, the strings extension and the planetiler functions.
pub(crate) static CEL_ENV: LazyLock<Arc<Env>> = LazyLock::new(|| {
    let mut env = Env::stdlib();
    env.add_extension(cel::extensions::strings).expect("strings extension");
    register_planetiler_stdlib(&mut env).expect("planetiler stdlib");
    Arc::new(env)
});

// ---------------------------------------------------------------------------------------------------------------
// Value conversions
// ---------------------------------------------------------------------------------------------------------------

/// Converts a planetiler [`Value`] into a CEL value that borrows its strings.
pub(crate) fn value_to_val(value: &Value) -> Box<dyn Val + '_> {
    match value {
        Value::Null => Box::new(CelNull),
        Value::Bool(b) => Box::new(CelBool::from(*b)),
        Value::Int(i) => Box::new(CelInt::from(*i)),
        Value::Float(f) => Box::new(CelDouble::from(*f as f64)),
        Value::Double(d) => Box::new(CelDouble::from(*d)),
        Value::String(s) => Box::new(CelString::from(&**s)),
        Value::Bytes(b) => Box::new(CelBytes::from(&**b)),
        Value::List(l) => Box::new(CelList::from(l.iter().map(value_to_val).collect::<Vec<_>>())),
        Value::Map(m) => Box::new(CelMap::from(
            m.iter().map(|(k, v)| (CelMapKey::from(k.as_str()), value_to_val(v))).collect::<HashMap<_, _>>(),
        )),
    }
}

/// Converts a planetiler [`Value`] into an owned CEL value.
pub(crate) fn value_to_static_val(value: &Value) -> Box<dyn Val> {
    match value {
        Value::String(s) => Box::new(CelString::from(s.to_string())),
        Value::Bytes(b) => Box::new(CelBytes::from(b.to_vec())),
        Value::List(l) => Box::new(CelList::from(l.iter().map(value_to_static_val).collect::<Vec<_>>())),
        Value::Map(m) => Box::new(CelMap::from(
            m.iter().map(|(k, v)| (CelMapKey::from(k.clone()), value_to_static_val(v))).collect::<HashMap<_, _>>(),
        )),
        Value::Null => Box::new(CelNull),
        Value::Bool(b) => Box::new(CelBool::from(*b)),
        Value::Int(i) => Box::new(CelInt::from(*i)),
        Value::Float(f) => Box::new(CelDouble::from(*f as f64)),
        Value::Double(d) => Box::new(CelDouble::from(*d)),
    }
}

fn type_error(val: &dyn Val, want: &str) -> ExecutionError {
    ExecutionError::UnexpectedType { got: val.get_type().name().to_owned(), want: want.to_owned() }
}

/// Converts a CEL result into a planetiler [`Value`] (Java `TypeConversion` of the native object a CEL value wraps).
pub(crate) fn val_to_value(val: &dyn Val) -> Result<Value, ExecutionError> {
    Ok(match val.get_type().kind() {
        Kind::NullType => Value::Null,
        Kind::Boolean => Value::Bool(*val.downcast_ref::<CelBool>().ok_or_else(|| type_error(val, "bool"))?.inner()),
        Kind::Int => Value::Int(*val.downcast_ref::<CelInt>().ok_or_else(|| type_error(val, "int"))?.inner()),
        Kind::UInt => Value::Int(*val.downcast_ref::<CelUInt>().ok_or_else(|| type_error(val, "uint"))?.inner() as i64),
        Kind::Double => {
            Value::Double(*val.downcast_ref::<CelDouble>().ok_or_else(|| type_error(val, "double"))?.inner())
        }
        Kind::String => Value::str(val.downcast_ref::<CelString>().ok_or_else(|| type_error(val, "string"))?.inner()),
        Kind::Bytes => Value::Bytes(Arc::from(
            val.downcast_ref::<CelBytes>().ok_or_else(|| type_error(val, "bytes"))?.inner().to_vec(),
        )),
        Kind::List => {
            let iterable = val.as_iterable().ok_or_else(|| type_error(val, "list"))?;
            let mut iter = iterable.iter();
            let mut items = Vec::new();
            while let Some(item) = iter.next() {
                items.push(val_to_value(item)?);
            }
            Value::list(items)
        }
        Kind::Map => {
            let iterable = val.as_iterable().ok_or_else(|| type_error(val, "map"))?;
            let indexer = val.as_indexer().ok_or_else(|| type_error(val, "map"))?;
            let mut iter = iterable.iter();
            let mut tags = Tags::default();
            while let Some(key) = iter.next() {
                let key_string = match val_to_value(key)? {
                    Value::String(s) => s.to_string(),
                    other => other.to_string(),
                };
                let value = indexer.get(key)?;
                tags.insert(key_string, val_to_value(value.as_ref())?);
            }
            Value::map(tags)
        }
        Kind::Type => Value::str(val.get_type().name()),
        _ => {
            if let Some(optional) = val.downcast_ref::<cel::common::types::CelOptional>() {
                match optional.option() {
                    Some(v) => val_to_value(v)?,
                    None => Value::Null,
                }
            } else {
                // geometries, timestamps, durations, ... have no planetiler representation
                Value::Null
            }
        }
    })
}

fn is_null(val: &dyn Val) -> bool {
    val.get_type().kind() == Kind::NullType
}

fn as_str(val: &dyn Val) -> Result<&str, ExecutionError> {
    val.downcast_ref::<CelString>().map(|s| s.inner()).ok_or_else(|| type_error(val, "string"))
}

fn as_f64(val: &dyn Val) -> Result<f64, ExecutionError> {
    if let Some(d) = val.downcast_ref::<CelDouble>() {
        Ok(*d.inner())
    } else if let Some(i) = val.downcast_ref::<CelInt>() {
        Ok(*i.inner() as f64)
    } else if let Some(u) = val.downcast_ref::<CelUInt>() {
        Ok(*u.inner() as f64)
    } else {
        Err(type_error(val, "number"))
    }
}

// ---------------------------------------------------------------------------------------------------------------
// feature.tags
// ---------------------------------------------------------------------------------------------------------------

/// `feature.tags`: a lazy CEL map over the tags of a feature that applies the schema's `tag_mappings` and returns
/// `null` for missing keys (Java `mapWithDefault(tagValueProducer.mapTags(feature), NullValue)`).
#[derive(Debug, Clone)]
pub(crate) struct TagsVal<'a> {
    tags: &'a Tags,
    tvp: &'a TagValueProducer,
    keys: OnceLock<Vec<CelString<'a>>>,
}

impl<'a> TagsVal<'a> {
    pub(crate) fn new(tags: &'a Tags, tvp: &'a TagValueProducer) -> TagsVal<'a> {
        TagsVal { tags, tvp, keys: OnceLock::new() }
    }

    fn keys(&self) -> &Vec<CelString<'a>> {
        self.keys.get_or_init(|| {
            let mut keys: Vec<CelString<'a>> = self.tags.keys().map(|k| CelString::from(k.as_str())).collect();
            for key in self.tvp.mapped_keys() {
                if !self.tags.contains_key(key) {
                    keys.push(CelString::from(key.to_string()));
                }
            }
            keys
        })
    }

    fn contains_key(&self, key: &str) -> bool {
        self.tags.contains_key(key) || self.tvp.has_mapping(key)
    }
}

impl<'a> Val for TagsVal<'a> {
    fn get_type(&self) -> &Type {
        &MAP_TYPE
    }

    fn cel_type() -> &'static Type {
        &MAP_TYPE
    }

    fn as_indexer<'b, 'v>(&'b self) -> Option<&'b (dyn Indexer + 'v)>
    where
        Self: 'v,
    {
        Some(self)
    }

    fn as_container(&self) -> Option<&dyn Container> {
        Some(self)
    }

    fn as_iterable<'b, 'v>(&'b self) -> Option<&'b (dyn Iterable + 'v)>
    where
        Self: 'v,
    {
        Some(self)
    }

    fn as_sizer(&self) -> Option<&dyn Sizer> {
        Some(self)
    }

    fn equals(&self, other: &dyn Val) -> bool {
        val_to_value(self).ok() == val_to_value(other).ok() && other.get_type().kind() == Kind::Map
    }

    fn clone_as_boxed<'v>(&self) -> Box<dyn Val + 'v>
    where
        Self: 'v,
    {
        Box::new(self.clone())
    }
}

impl<'a> Indexer for TagsVal<'a> {
    fn get<'b, 'v>(&'b self, idx: &dyn Val) -> Result<CowVal<'b, 'v>, ExecutionError>
    where
        Self: 'v,
    {
        let key = idx.downcast_ref::<CelString>().ok_or_else(|| type_error(idx, "string"))?.inner();
        if self.tvp.has_mapping(key) {
            Ok(CowVal::Owned(value_to_static_val(&self.tvp.value_for_key_in_tags(self.tags, key))))
        } else {
            match self.tags.get(key) {
                Some(value) => Ok(CowVal::Owned(value_to_val(value))),
                None => Ok(CowVal::owned(CelNull)),
            }
        }
    }

    fn steal<'v>(self: Box<Self>, idx: &dyn Val) -> Result<Box<dyn Val + 'v>, ExecutionError>
    where
        Self: 'v,
    {
        self.get(idx).map(|v| v.into_owned())
    }
}

impl Container for TagsVal<'_> {
    fn contains(&self, value: &dyn Val) -> Result<bool, ExecutionError> {
        Ok(value.downcast_ref::<CelString>().is_some_and(|k| self.contains_key(k.inner())))
    }
}

impl Sizer for TagsVal<'_> {
    fn size(&self) -> CelInt {
        CelInt::from(self.keys().len() as i64)
    }
}

struct KeyIter<'b, 'a>(std::slice::Iter<'b, CelString<'a>>);

impl<'b, 'a: 'v, 'v> traits::Iterator<'b, 'v> for KeyIter<'b, 'a> {
    fn next(&mut self) -> Option<&'b (dyn Val + 'v)> {
        self.0.next().map(|s| s as &(dyn Val + 'v))
    }
}

impl<'a> Iterable for TagsVal<'a> {
    fn iter<'b, 'v>(&'b self) -> Box<dyn traits::Iterator<'b, 'v> + 'b>
    where
        Self: 'v,
    {
        Box::new(KeyIter(self.keys().iter()))
    }
}

// ---------------------------------------------------------------------------------------------------------------
// geometry
// ---------------------------------------------------------------------------------------------------------------

/// Wrapper for a geometry that exposes utility fields and functions to CEL expressions (Java `GeometryVal`).
///
/// Fields: `lat`, `lon` (of the first coordinate), `min_lat`, `max_lat`, `min_lon`, `max_lon`, `bbox`, `centroid`,
/// `centroid_if_convex`, `validated_polygon`, `point_on_surface`, `line_midpoint`, `innermost_point`. Functions:
/// `area(unit)`, `length(unit)`, `point_along_line(ratio)`, `innermost_point(tolerance)`, `partial_line(start, end)`.
///
/// Functions are implemented by indexing the geometry with an internal key (`"\0name\0args..."`) since CEL function
/// overloads can't downcast to a value that borrows the source feature.
#[derive(Clone)]
pub(crate) enum GeometryVal<'a> {
    Borrowed(&'a dyn WithGeometry),
    Owned(Arc<FromWorld>),
}

impl std::fmt::Debug for GeometryVal<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GeometryVal({:?})", self.geometry().world_geometry().ok())
    }
}

const FIELD_KEY_PREFIX: char = '\0';

impl<'a> GeometryVal<'a> {
    pub(crate) fn borrowed(geometry: &'a dyn WithGeometry) -> GeometryVal<'a> {
        GeometryVal::Borrowed(geometry)
    }

    pub(crate) fn from_world(geometry: Geometry<f64>) -> GeometryVal<'static> {
        GeometryVal::Owned(Arc::new(from_world_geometry(geometry)))
    }

    fn geometry(&self) -> &dyn WithGeometry {
        match self {
            GeometryVal::Borrowed(g) => *g,
            GeometryVal::Owned(g) => &**g,
        }
    }

    fn compute(&self, key: &str) -> Result<Box<dyn Val + 'static>, ExecutionError> {
        let geom = self.geometry();
        let err = |e: planetiler_core::geo::geometry_exception::GeometryException| {
            ExecutionError::function_error(key.trim_start_matches(FIELD_KEY_PREFIX), e.to_string())
        };
        let geometry = |g: Geometry<f64>| -> Box<dyn Val + 'static> { Box::new(GeometryVal::from_world(g)) };
        let double = |d: f64| -> Box<dyn Val + 'static> { Box::new(CelDouble::from(d)) };
        if let Some(call) = key.strip_prefix(FIELD_KEY_PREFIX) {
            let mut parts = call.split(FIELD_KEY_PREFIX);
            let name = parts.next().unwrap_or("");
            fn arg<'s>(name: &str, p: Option<&'s str>) -> Result<&'s str, ExecutionError> {
                p.ok_or_else(|| ExecutionError::function_error(name, "missing argument"))
            }
            let num = |p: Option<&str>| -> Result<f64, ExecutionError> {
                arg(name, p)?.parse::<f64>().map_err(|e| ExecutionError::function_error(name, e))
            };
            return match name {
                "area" => {
                    let unit = unit::Area::from(arg(name, parts.next())?)
                        .map_err(|e| ExecutionError::function_error(name, e))?;
                    Ok(double(geom.area_in(unit).map_err(err)?))
                }
                "length" => {
                    let unit = unit::Length::from(arg(name, parts.next())?)
                        .map_err(|e| ExecutionError::function_error(name, e))?;
                    Ok(double(geom.length_in(unit).map_err(err)?))
                }
                "point_along_line" => Ok(geometry(geom.point_along_line(num(parts.next())?).map_err(err)?)),
                "innermost_point" => Ok(geometry(geom.innermost_point(num(parts.next())?).map_err(err)?)),
                "partial_line" => {
                    let start = num(parts.next())?;
                    let end = num(parts.next())?;
                    Ok(geometry(geom.partial_line(start, end).map_err(err)?))
                }
                "debug" => Ok(Box::new(CelString::from(format!("{:?}", geom.world_geometry().map_err(err)?)))),
                _ => Err(ExecutionError::no_such_key(name)),
            };
        }
        let first_coord = || -> Result<geo_types::Coord<f64>, ExecutionError> {
            geom.world_geometry()
                .map_err(err)?
                .coords_iter()
                .next()
                .ok_or_else(|| ExecutionError::function_error(key, "empty geometry"))
        };
        let lat_lon_bounds = || -> Result<geo_types::Rect<f64>, ExecutionError> {
            geom.lat_lon_geometry()
                .map_err(err)?
                .bounding_rect()
                .ok_or_else(|| ExecutionError::function_error(key, "empty geometry"))
        };
        match key {
            "lat" => Ok(double(geo_utils::get_world_lat(first_coord()?.y))),
            "lon" => Ok(double(geo_utils::get_world_lon(first_coord()?.x))),
            "min_lat" => Ok(double(lat_lon_bounds()?.min().y)),
            "max_lat" => Ok(double(lat_lon_bounds()?.max().y)),
            "min_lon" => Ok(double(lat_lon_bounds()?.min().x)),
            "max_lon" => Ok(double(lat_lon_bounds()?.max().x)),
            "bbox" => {
                let world = geom.world_geometry().map_err(err)?;
                Ok(geometry(match world.bounding_rect() {
                    None => geo_utils::empty_geometry(),
                    Some(rect) if rect.min() == rect.max() => Geometry::Point(rect.min().into()),
                    Some(rect) => Geometry::Polygon(rect.to_polygon()),
                }))
            }
            "centroid" => Ok(geometry(geom.centroid().map_err(err)?.clone())),
            "centroid_if_convex" => Ok(geometry(geom.centroid_if_convex().map_err(err)?.clone())),
            "validated_polygon" => Ok(geometry(geom.validated_polygon().map_err(err)?.clone())),
            "point_on_surface" => Ok(geometry(geom.point_on_surface().map_err(err)?.clone())),
            "line_midpoint" => Ok(geometry(geom.line_midpoint().map_err(err)?.clone())),
            "innermost_point" => Ok(geometry(geom.innermost_point(0.1).map_err(err)?)),
            _ => Err(ExecutionError::no_such_key(key)),
        }
    }
}

/// Calls a geometry "method" on `target` by indexing it with an internal key.
fn geometry_call<'b, 'v>(target: &(dyn Val + 'v), name: &str, args: &[String]) -> CelResult<'b, 'v> {
    let mut key = String::new();
    key.push(FIELD_KEY_PREFIX);
    key.push_str(name);
    for arg in args {
        key.push(FIELD_KEY_PREFIX);
        key.push_str(arg);
    }
    let indexer = target.as_indexer().ok_or_else(|| type_error(target, "geometry"))?;
    Ok(CowVal::Owned(indexer.get(&CelString::from(key))?.into_owned()))
}

impl<'a> Val for GeometryVal<'a> {
    fn get_type(&self) -> &Type {
        &GEOMETRY_TYPE
    }

    fn cel_type() -> &'static Type {
        &GEOMETRY_TYPE
    }

    fn as_indexer<'b, 'v>(&'b self) -> Option<&'b (dyn Indexer + 'v)>
    where
        Self: 'v,
    {
        Some(self)
    }

    fn equals(&self, other: &dyn Val) -> bool {
        if other.get_type() != &GEOMETRY_TYPE {
            return false;
        }
        let debug = |v: &dyn Val| {
            v.as_indexer().and_then(|i| {
                let key = CelString::from(format!("{FIELD_KEY_PREFIX}debug"));
                i.get(&key).ok().and_then(|s| s.downcast_ref::<CelString>().map(|s| s.inner().to_string()))
            })
        };
        let mine = debug(self);
        mine.is_some() && mine == debug(other)
    }

    fn clone_as_boxed<'v>(&self) -> Box<dyn Val + 'v>
    where
        Self: 'v,
    {
        Box::new(self.clone())
    }
}

impl<'a> Indexer for GeometryVal<'a> {
    fn get<'b, 'v>(&'b self, idx: &dyn Val) -> Result<CowVal<'b, 'v>, ExecutionError>
    where
        Self: 'v,
    {
        let key = as_str(idx)?;
        Ok(CowVal::Owned(self.compute(key)?))
    }

    fn steal<'v>(self: Box<Self>, idx: &dyn Val) -> Result<Box<dyn Val + 'v>, ExecutionError>
    where
        Self: 'v,
    {
        self.get(idx).map(|v| v.into_owned())
    }
}

static EMPTY_LINE: LazyLock<FromWorld> =
    LazyLock::new(|| from_world_geometry(Geometry::LineString(geo_utils::empty_line())));

// ---------------------------------------------------------------------------------------------------------------
// variable resolution
// ---------------------------------------------------------------------------------------------------------------

/// Resolves CEL variables from a [`ScriptContext`] (Java `ScriptContext.apply(name)` on the CEL side).
pub(crate) struct Resolver<'a> {
    ctx: ScriptContext<'a>,
}

impl<'a> Resolver<'a> {
    pub(crate) fn new(ctx: &ScriptContext<'a>) -> Resolver<'a> {
        Resolver { ctx: *ctx }
    }
}

fn opt_str<'b>(s: Option<&'b str>) -> CowVal<'b, 'b> {
    match s {
        Some(s) => CowVal::owned(CelString::from(s)),
        None => CowVal::owned(CelNull),
    }
}

impl VariableResolver for Resolver<'_> {
    fn resolve<'b>(&'b self, variable: &str) -> Option<CowVal<'b, 'b>> {
        let ctx = &self.ctx;
        let root = ctx.root().inner();
        if variable.starts_with("args") {
            if variable == "args" {
                return Some(CowVal::Borrowed(root.args_binding.as_ref()));
            }
            return root.arg_bindings.get(variable).map(|v| CowVal::Borrowed(v.as_ref()));
        }
        let kind = ctx.kind();
        if kind >= ContextKind::FeaturePostMatch {
            match variable {
                contexts::MATCH_KEY => return Some(opt_str(ctx.match_key())),
                contexts::MATCH_VALUE => return Some(CowVal::Owned(value_to_static_val(&ctx.match_value()))),
                contexts::VALUE if kind >= ContextKind::FeatureAttribute => {
                    return Some(match ctx.value() {
                        Some(v) => CowVal::Owned(value_to_val(v)),
                        None => CowVal::owned(CelNull),
                    });
                }
                _ => {}
            }
        }
        if kind < ContextKind::ProcessFeature || !variable.starts_with("feature") {
            return None;
        }
        let feature = ctx.feature()?;
        Some(match variable {
            contexts::FEATURE_TAGS => CowVal::owned(TagsVal::new(feature.tags(), ctx.tag_value_producer())),
            contexts::FEATURE_ID => CowVal::owned(CelInt::from(feature.id())),
            contexts::FEATURE_SOURCE => opt_str(feature.source()),
            contexts::FEATURE_SOURCE_LAYER => opt_str(feature.source_layer()),
            contexts::FEATURE_GEOMETRY => match feature.with_geometry() {
                Some(g) => CowVal::owned(GeometryVal::borrowed(g)),
                None => CowVal::owned(GeometryVal::Borrowed(&*EMPTY_LINE)),
            },
            contexts::FEATURE_OSM_TYPE
            | contexts::FEATURE_OSM_FROM_NODE_ID
            | contexts::FEATURE_OSM_TO_NODE_ID
            | contexts::FEATURE_OSM_CHANGESET
            | contexts::FEATURE_OSM_VERSION
            | contexts::FEATURE_OSM_TIMESTAMP
            | contexts::FEATURE_OSM_USER_ID
            | contexts::FEATURE_OSM_USER_NAME => {
                let null = || CowVal::owned(CelNull);
                let Some(osm) = feature.osm() else {
                    return Some(null());
                };
                let int = |i: i64| CowVal::owned(CelInt::from(i));
                match variable {
                    contexts::FEATURE_OSM_TYPE => CowVal::owned(CelString::from(osm_type_name(osm.element_type()))),
                    contexts::FEATURE_OSM_FROM_NODE_ID => {
                        osm.way_nodes().and_then(|n| n.first()).map_or_else(null, |n| int(*n))
                    }
                    contexts::FEATURE_OSM_TO_NODE_ID => {
                        osm.way_nodes().and_then(|n| n.last()).map_or_else(null, |n| int(*n))
                    }
                    _ => match osm.info() {
                        None => null(),
                        Some(info) => match variable {
                            contexts::FEATURE_OSM_CHANGESET => int(info.changeset),
                            contexts::FEATURE_OSM_VERSION => int(info.version as i64),
                            contexts::FEATURE_OSM_TIMESTAMP => int(info.timestamp),
                            contexts::FEATURE_OSM_USER_ID => int(info.user_id as i64),
                            _ => CowVal::owned(CelString::from(&*info.user)),
                        },
                    },
                }
            }
            _ => return None,
        })
    }
}

/// Evaluates a compiled CEL expression against `ctx`.
pub(crate) fn evaluate(expr: &IdedExpr, ctx: &ScriptContext<'_>) -> Result<Value, ExecutionError> {
    let resolver = Resolver::new(ctx);
    let mut cel_context = cel::Context::with_env(CEL_ENV.clone());
    cel_context.set_variable_resolver(&resolver);
    let result = cel::Value::resolve_val(expr, &cel_context)?;
    val_to_value(result.as_ref())
}

// ---------------------------------------------------------------------------------------------------------------
// compilation
// ---------------------------------------------------------------------------------------------------------------

/// Parses `script` into a CEL expression, expanding macros and rewriting presence tests.
pub(crate) fn compile(script: &str) -> Result<IdedExpr, String> {
    let mut expr = CEL_ENV.parser().parse(script).map_err(|e| e.to_string())?;
    rewrite_presence_tests(&mut expr);
    Ok(expr)
}

/// Rewrites `has(x.f)` into `@planetiler_has_field(x, "f")` so that maps that return `null` for missing keys (like
/// `feature.tags`) report missing keys as absent.
fn rewrite_presence_tests(expr: &mut IdedExpr) {
    use cel::common::ast::EntryExpr;
    match &mut expr.expr {
        Expr::Select(select) => {
            rewrite_presence_tests(&mut select.operand);
            if select.test {
                let operand = std::mem::take(&mut *select.operand);
                let field = std::mem::take(&mut select.field);
                expr.expr = Expr::Call(CallExpr {
                    func_name: HAS_FIELD.to_string(),
                    target: None,
                    args: vec![
                        operand,
                        IdedExpr { id: expr.id, expr: Expr::Literal(LiteralValue::String(CelString::from(field))) },
                    ],
                });
            }
        }
        Expr::Call(call) => {
            if let Some(target) = &mut call.target {
                rewrite_presence_tests(target);
            }
            call.args.iter_mut().for_each(rewrite_presence_tests);
        }
        Expr::Comprehension(c) => {
            rewrite_presence_tests(&mut c.iter_range);
            rewrite_presence_tests(&mut c.accu_init);
            rewrite_presence_tests(&mut c.loop_cond);
            rewrite_presence_tests(&mut c.loop_step);
            rewrite_presence_tests(&mut c.result);
        }
        Expr::List(list) => list.elements.iter_mut().for_each(rewrite_presence_tests),
        Expr::Map(map) => {
            for entry in &mut map.entries {
                match &mut entry.expr {
                    EntryExpr::MapEntry(e) => {
                        rewrite_presence_tests(&mut e.key);
                        rewrite_presence_tests(&mut e.value);
                    }
                    EntryExpr::StructField(f) => rewrite_presence_tests(&mut f.value),
                }
            }
        }
        Expr::Struct(s) => {
            for entry in &mut s.entries {
                match &mut entry.expr {
                    EntryExpr::MapEntry(e) => {
                        rewrite_presence_tests(&mut e.key);
                        rewrite_presence_tests(&mut e.value);
                    }
                    EntryExpr::StructField(f) => rewrite_presence_tests(&mut f.value),
                }
            }
        }
        _ => {}
    }
}

fn qualified_name(expr: &Expr) -> Option<Vec<&str>> {
    let mut segments = Vec::new();
    let mut expr = expr;
    loop {
        match expr {
            Expr::Ident(name) => {
                segments.push(name.as_str());
                segments.reverse();
                return Some(segments);
            }
            Expr::Select(select) if !select.test => {
                segments.push(select.field.as_str());
                expr = &select.operand.expr;
            }
            _ => return None,
        }
    }
}

/// Checks that every variable `expr` references is declared by `env` or bound by an enclosing comprehension (an
/// approximation of the cel-java type checker). Returns the first undeclared reference.
pub(crate) fn check_references(expr: &IdedExpr, env: &ScriptEnvironment) -> Result<(), String> {
    fn check<'e>(expr: &'e IdedExpr, env: &ScriptEnvironment, scope: &mut Vec<&'e str>) -> Result<(), String> {
        use cel::common::ast::EntryExpr;
        match &expr.expr {
            Expr::Ident(_) | Expr::Select(_) if qualified_name(&expr.expr).is_some() => {
                let segments = qualified_name(&expr.expr).unwrap_or_default();
                if scope.contains(&segments[0]) {
                    return Ok(());
                }
                let mut name = String::new();
                for segment in &segments {
                    if !name.is_empty() {
                        name.push('.');
                    }
                    name.push_str(segment);
                    if env.contains_variable(&name) {
                        return Ok(());
                    }
                }
                Err(segments[0].to_string())
            }
            Expr::Select(select) => check(&select.operand, env, scope),
            Expr::Call(call) => {
                if let Some(target) = &call.target {
                    check(target, env, scope)?;
                }
                call.args.iter().try_for_each(|a| check(a, env, scope))
            }
            Expr::Comprehension(c) => {
                check(&c.iter_range, env, scope)?;
                check(&c.accu_init, env, scope)?;
                let len = scope.len();
                scope.push(&c.iter_var);
                if let Some(v) = &c.iter_var2 {
                    scope.push(v);
                }
                scope.push(&c.accu_var);
                let result = check(&c.loop_cond, env, scope)
                    .and_then(|_| check(&c.loop_step, env, scope))
                    .and_then(|_| check(&c.result, env, scope));
                scope.truncate(len);
                result
            }
            Expr::List(list) => list.elements.iter().try_for_each(|e| check(e, env, scope)),
            Expr::Map(map) => map.entries.iter().try_for_each(|entry| match &entry.expr {
                EntryExpr::MapEntry(e) => check(&e.key, env, scope).and_then(|_| check(&e.value, env, scope)),
                EntryExpr::StructField(f) => check(&f.value, env, scope),
            }),
            Expr::Struct(s) => s.entries.iter().try_for_each(|entry| match &entry.expr {
                EntryExpr::MapEntry(e) => check(&e.key, env, scope).and_then(|_| check(&e.value, env, scope)),
                EntryExpr::StructField(f) => check(&f.value, env, scope),
            }),
            _ => Ok(()),
        }
    }
    check(expr, env, &mut Vec::new())
}

// ---------------------------------------------------------------------------------------------------------------
// functions (Java PlanetilerStdLib)
// ---------------------------------------------------------------------------------------------------------------

type CelResult<'b, 'v> = Result<CowVal<'b, 'v>, ExecutionError>;

/// `coalesce(a, b, c...)`: the first non-null value.
fn coalesce<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    Ok(args.into_iter().find(|a| !is_null(a.as_ref())).unwrap_or_else(|| CowVal::owned(CelNull)))
}

/// `nullif(a, b)`: null if `a == b`, otherwise `a`.
fn nullif<'b, 'v>(mut args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    let b = args.pop().ok_or_else(ExecutionError::missing_argument_or_target)?;
    let a = args.pop().ok_or_else(ExecutionError::missing_argument_or_target)?;
    if a.get_type() == b.get_type() && a.equals(b.as_ref()) { Ok(CowVal::owned(CelNull)) } else { Ok(a) }
}

/// Converts a Java regex replacement string (`$1`, `\$`) into a `regex` crate one (`${1}`, `$$`).
fn java_replacement(replacement: &str) -> String {
    let mut result = String::with_capacity(replacement.len());
    let mut chars = replacement.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('$') => result.push_str("$$"),
                Some(other) => result.push(other),
                None => {}
            },
            '$' => {
                let mut group = String::new();
                while let Some(d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                    group.push(*d);
                    chars.next();
                }
                if group.is_empty() {
                    result.push_str("$$");
                } else {
                    result.push_str("${");
                    result.push_str(&group);
                    result.push('}');
                }
            }
            other => result.push(other),
        }
    }
    result
}

thread_local! {
    static REGEX_CACHE: RefCell<HashMap<String, Regex>> = RefCell::new(HashMap::new());
}

/// `string.replaceRegex(regex, replacement)`: replaces all matches of `regex` with `replacement`.
fn replace_regex<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    let string = as_str(args[0].as_ref())?;
    let pattern = as_str(args[1].as_ref())?;
    let replacement = java_replacement(as_str(args[2].as_ref())?);
    REGEX_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if !cache.contains_key(pattern) {
            if cache.len() > 1000 {
                cache.clear();
            }
            let regex = Regex::new(pattern).map_err(|e| ExecutionError::function_error("replaceRegex", e))?;
            cache.insert(pattern.to_string(), regex);
        }
        let regex = &cache[pattern];
        Ok(CowVal::owned(CelString::from(regex.replace_all(string, replacement.as_str()).into_owned())))
    })
}

/// Looks up `key` in a map value, returning `None` if missing or null.
fn map_get<'b, 'v>(map: &'b CowVal<'b, 'v>, key: &dyn Val) -> Option<CowVal<'b, 'v>> {
    let indexer = map.as_indexer()?;
    match indexer.get(key) {
        Ok(v) if !is_null(v.as_ref()) => Some(v),
        _ => None,
    }
}

/// `map.has(key)` / `map.has(key, values...)`: true if `key` is present (and its value is one of `values`).
fn map_has<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    let result = match map_get(&args[0], args[1].as_ref()) {
        None => false,
        Some(elem) => args.len() == 2 || args[2..].iter().any(|a| a.equals(elem.as_ref())),
    };
    Ok(CowVal::owned(CelBool::from(result)))
}

/// Internal implementation of `has(x.f)` after [`rewrite_presence_tests`].
fn has_field<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    let target = &args[0];
    let Some(indexer) = target.as_indexer() else {
        return Err(ExecutionError::no_such_key(as_str(args[1].as_ref())?));
    };
    let result = match indexer.get(args[1].as_ref()) {
        Ok(v) => target.get_type().kind() != Kind::Map || !is_null(v.as_ref()),
        Err(ExecutionError::NoSuchKey(_)) => false,
        Err(e) => return Err(e),
    };
    Ok(CowVal::owned(CelBool::from(result)))
}

/// `map.get(key)`: the value for key, or null if missing.
fn map_get_fn<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    Ok(map_get(&args[0], args[1].as_ref()).map(|v| CowVal::Owned(v.into_owned())).unwrap_or(CowVal::owned(CelNull)))
}

/// `map.getOrDefault(key, default)`: the value for key, or default if missing.
fn map_get_or_default<'b, 'v>(mut args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    if let Some(v) = map_get(&args[0], args[1].as_ref()) {
        return Ok(CowVal::Owned(v.into_owned()));
    }
    Ok(args.pop().expect("3 args"))
}

/// `map.getOrKeep(key)`: the value for key, or the key itself if missing.
fn map_get_or_keep<'b, 'v>(mut args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    if let Some(v) = map_get(&args[0], args[1].as_ref()) {
        return Ok(CowVal::Owned(v.into_owned()));
    }
    Ok(args.pop().expect("2 args"))
}

fn reduce_numeric<'b, 'v>(
    list: &dyn Val,
    name: &str,
    int_fn: fn(i64, i64) -> i64,
    double_fn: fn(f64, f64) -> f64,
) -> CelResult<'b, 'v> {
    let iterable = list.as_iterable().ok_or_else(|| type_error(list, "list"))?;
    let mut iter = iterable.iter();
    let Some(first) = iter.next() else {
        return Ok(CowVal::owned(CelNull));
    };
    if let Some(i) = first.downcast_ref::<CelInt>() {
        let mut acc = *i.inner();
        while let Some(next) = iter.next() {
            acc = int_fn(as_f64(next)? as i64, acc);
        }
        Ok(CowVal::owned(CelInt::from(acc)))
    } else if let Some(d) = first.downcast_ref::<CelDouble>() {
        let mut acc = *d.inner();
        while let Some(next) = iter.next() {
            acc = double_fn(as_f64(next)?, acc);
        }
        Ok(CowVal::owned(CelDouble::from(acc)))
    } else {
        Err(ExecutionError::function_error(name, format!("Bad element of list for {name}(): {first:?}")))
    }
}

/// `min(list)`: the minimum value from the list, or null if empty.
fn min<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    reduce_numeric(args[0].as_ref(), "min", i64::min, f64::min)
}

/// `max(list)`: the maximum value from the list, or null if empty.
fn max<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    reduce_numeric(args[0].as_ref(), "max", i64::max, f64::max)
}

/// `geometry.area(unit)`.
fn area<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    geometry_call(args[0].as_ref(), "area", &[as_str(args[1].as_ref())?.to_string()])
}

/// `geometry.length(unit)`.
fn length<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    geometry_call(args[0].as_ref(), "length", &[as_str(args[1].as_ref())?.to_string()])
}

/// `geometry.point_along_line(ratio)`.
fn point_along_line<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    geometry_call(args[0].as_ref(), "point_along_line", &[as_f64(args[1].as_ref())?.to_string()])
}

/// `geometry.innermost_point(tolerance)`.
fn innermost_point<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    geometry_call(args[0].as_ref(), "innermost_point", &[as_f64(args[1].as_ref())?.to_string()])
}

/// `geometry.partial_line(start, end)`.
fn partial_line<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> CelResult<'b, 'v> {
    let start = as_f64(args[1].as_ref())?.to_string();
    let end = as_f64(args[2].as_ref())?.to_string();
    geometry_call(args[0].as_ref(), "partial_line", &[start, end])
}

fn dyn_types(n: usize) -> Vec<Type> {
    (0..n).map(|_| DYN_TYPE).collect()
}

fn register_planetiler_stdlib(env: &mut Env) -> Result<(), cel::DeclarationError> {
    env.add_type(GEOMETRY_TYPE.to_owned())?;
    for i in 0..VARARG_LIMIT {
        env.add_overload("coalesce", &format!("coalesce_{i}"), dyn_types(i), coalesce)?;
    }
    env.add_overload("nullif", "nullif", vec![DYN_TYPE, DYN_TYPE], nullif)?;
    env.add_member_overload(
        "replaceRegex",
        "replaceRegex",
        STRING_TYPE,
        vec![STRING_TYPE, STRING_TYPE],
        replace_regex,
    )?;
    for i in 0..VARARG_LIMIT {
        env.add_member_overload("has", &format!("map_has_{i}"), MAP_TYPE, dyn_types(i + 1), map_has)?;
    }
    env.add_overload(HAS_FIELD, HAS_FIELD, vec![DYN_TYPE, STRING_TYPE], has_field)?;
    env.add_member_overload("get", "get", MAP_TYPE, vec![DYN_TYPE], map_get_fn)?;
    env.add_member_overload("getOrDefault", "getOrDefault", MAP_TYPE, vec![DYN_TYPE, DYN_TYPE], map_get_or_default)?;
    env.add_member_overload("getOrKeep", "getOrKeep", MAP_TYPE, vec![DYN_TYPE], map_get_or_keep)?;
    env.add_overload("min", "min_list", vec![LIST_TYPE], min)?;
    env.add_overload("max", "max_list", vec![LIST_TYPE], max)?;
    let geom = || GEOMETRY_TYPE.to_owned();
    env.add_member_overload("area", "area", geom(), vec![STRING_TYPE], area)?;
    env.add_member_overload("length", "length", geom(), vec![STRING_TYPE], length)?;
    let number = |n: &str| if n == "double" { DOUBLE_TYPE } else { INT_TYPE };
    for suffix in ["double", "int"] {
        env.add_member_overload(
            "point_along_line",
            &format!("point_along_line_{suffix}"),
            geom(),
            vec![number(suffix)],
            point_along_line,
        )?;
        env.add_member_overload(
            "innermost_point",
            &format!("innermost_point_{suffix}"),
            geom(),
            vec![number(suffix)],
            innermost_point,
        )?;
    }
    for a in ["double", "int"] {
        for b in ["double", "int"] {
            env.add_member_overload(
                "partial_line",
                &format!("partial_line_{a}_{b}"),
                geom(),
                vec![number(a), number(b)],
                partial_line,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CEL values and resolvers must be `Send + Sync`; that now follows from the types themselves (no `unsafe impl`).
    #[test]
    fn values_borrowing_features_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GeometryVal<'static>>();
        assert_send_sync::<Resolver<'static>>();
        assert_send_sync::<ScriptContext<'static>>();
    }

    #[test]
    fn java_replacements() {
        assert_eq!("${1}", java_replacement("$1"));
        assert_eq!("${1}a", java_replacement("$1a"));
        assert_eq!("$$", java_replacement("\\$"));
        assert_eq!("a", java_replacement("a"));
    }

    #[test]
    fn env_builds() {
        let _ = &*CEL_ENV;
    }
}
