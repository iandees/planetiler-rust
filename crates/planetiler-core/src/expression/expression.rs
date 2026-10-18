//! Port of `expression/Expression.java`: a framework for defining and manipulating boolean expressions that match on
//! input elements.
//!
//! Calling [`Expression::generate_java_code`] (or `to_string()`) generates code that can be used to recreate an
//! identical copy of the original expression (`and(matchAny("a", "b"), ...)`), matching the Java output.

use std::any::Any;

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use regex::Regex;

use crate::expression::data_type::DataType;
use crate::expression::simplifiable::Simplifiable;
use crate::geo::geometry_type::GeometryType;
use crate::reader::with_geometry_type::WithGeometryType;
use crate::reader::with_tags::{OfMap, WithTags};
use crate::value::{Tags, Value};

pub const LINESTRING_TYPE: &str = "linestring";
pub const POINT_TYPE: &str = "point";
pub const POLYGON_TYPE: &str = "polygon";
pub const UNKNOWN_GEOMETRY_TYPE: &str = "unknown_type";

/// List of tag keys that caused an expression to match.
pub type MatchKeys = Vec<Arc<str>>;

/// The input that expressions evaluate against: something with tags, and optionally a geometry type, source, and
/// source layer (Java uses `instanceof WithGeometryType/WithSource/WithSourceLayer/SourceFeature` checks).
///
/// `SourceFeature` implements this in the reader; plain tag maps ([`Tags`], [`OfMap`]) implement it with no geometry
/// or source.
pub trait ExpressionInput: WithTags {
    /// The geometry type info of this input, if any (used by `matchType`).
    fn as_with_geometry_type(&self) -> Option<&dyn WithGeometryType> {
        None
    }

    /// Name of the source this input came from, if any (used by `matchSource`).
    fn source(&self) -> Option<&str> {
        None
    }

    /// Name of the layer within the source this input came from, if any (used by `matchSourceLayer`).
    fn source_layer(&self) -> Option<&str> {
        None
    }

    /// Allows custom expressions/getters to downcast to a concrete input type (Java `instanceof` checks).
    fn as_any(&self) -> Option<&dyn Any> {
        None
    }
}

impl ExpressionInput for Tags {}
impl ExpressionInput for OfMap {}

/// A user-defined boolean expression (Java lambdas implementing `Expression`, or custommap's `BooleanExpressionScript`).
pub trait CustomExpression: Send + Sync + fmt::Debug {
    fn evaluate(&self, input: &dyn ExpressionInput, match_keys: Option<&mut MatchKeys>) -> bool;

    /// Java code that can reconstruct this expression, or `None` if unsupported.
    fn generate_java_code(&self) -> Option<String> {
        None
    }

    /// Simplification rules for this expression, or `None` to leave it unchanged.
    fn simplify_once(&self) -> Option<Expression> {
        None
    }

    /// Structural equality with another custom expression. Defaults to identity.
    fn dyn_eq(&self, other: &dyn CustomExpression) -> bool {
        std::ptr::addr_eq(self as *const Self, other as *const dyn CustomExpression)
    }

    fn as_any(&self) -> &dyn Any;
}

/// A user-defined value getter used by [`match_any_typed`] (Java `TypedGetter`), for example a custommap
/// `ConfigExpression`.
pub trait TypedGetter: Send + Sync + fmt::Debug {
    /// Returns the value for `key` (`None` for dynamic getters that don't read a specific tag).
    fn apply(&self, input: &dyn ExpressionInput, key: Option<&str>) -> Value;

    /// Simplification rules for this getter, or `None` to leave it unchanged.
    fn simplify_once(&self) -> Option<Arc<dyn TypedGetter>> {
        None
    }

    /// Structural equality with another getter. Defaults to identity.
    fn dyn_eq(&self, other: &dyn TypedGetter) -> bool {
        std::ptr::addr_eq(self as *const Self, other as *const dyn TypedGetter)
    }

    fn as_any(&self) -> &dyn Any;
}

/// How [`MatchAny`] gets the value to compare from an input.
#[derive(Clone, Debug)]
pub enum Getter {
    Data(DataType),
    Custom(Arc<dyn TypedGetter>),
}

impl PartialEq for Getter {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Getter::Data(a), Getter::Data(b)) => a == b,
            (Getter::Custom(a), Getter::Custom(b)) => Arc::ptr_eq(a, b) || a.dyn_eq(&**b),
            _ => false,
        }
    }
}

impl From<DataType> for Getter {
    fn from(value: DataType) -> Self {
        Getter::Data(value)
    }
}

impl<T: TypedGetter + 'static> From<Arc<T>> for Getter {
    fn from(value: Arc<T>) -> Self {
        Getter::Custom(value)
    }
}

/// Wraps a closure as a [`TypedGetter`].
pub struct FnGetter<F>(pub F);

impl<F> fmt::Debug for FnGetter<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FnGetter")
    }
}

impl<F: Fn(&dyn ExpressionInput, Option<&str>) -> Value + Send + Sync + 'static> TypedGetter for FnGetter<F> {
    fn apply(&self, input: &dyn ExpressionInput, key: Option<&str>) -> Value {
        (self.0)(input, key)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Creates a [`Getter`] from a closure.
pub fn getter_fn(f: impl Fn(&dyn ExpressionInput, Option<&str>) -> Value + Send + Sync + 'static) -> Getter {
    Getter::Custom(Arc::new(FnGetter(f)))
}

/// Wraps a closure as a [`CustomExpression`].
pub struct FnExpression<F>(pub F);

impl<F> fmt::Debug for FnExpression<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FnExpression")
    }
}

impl<F: Fn(&dyn ExpressionInput, Option<&mut MatchKeys>) -> bool + Send + Sync + 'static> CustomExpression
    for FnExpression<F>
{
    fn evaluate(&self, input: &dyn ExpressionInput, match_keys: Option<&mut MatchKeys>) -> bool {
        (self.0)(input, match_keys)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Evaluates to true if the value for `field` tag is any of `exact_matches` or matches `pattern`.
#[derive(Clone, Debug)]
pub struct MatchAny {
    /// The field to match, or `None` when relying on a dynamic getter.
    pub field: Option<Arc<str>>,
    /// All raw values that were initially provided.
    pub values: Vec<Value>,
    /// The input `values` that should be treated as exact matches.
    pub exact_matches: HashSet<String>,
    /// Regular expression that the value must match, or `None`.
    pub pattern: Option<Regex>,
    /// If `values` contained "" (or null).
    pub match_when_missing: bool,
    pub getter: Getter,
}

impl PartialEq for MatchAny {
    fn eq(&self, other: &Self) -> bool {
        self.match_when_missing == other.match_when_missing
            && self.field == other.field
            && self.values == other.values
            && self.exact_matches == other.exact_matches
            && self.pattern.as_ref().map(Regex::as_str) == other.pattern.as_ref().map(Regex::as_str)
            && self.getter == other.getter
    }
}

/// Returns true if `s` contains a `%` that is not preceded by a backslash.
fn has_unescaped_wildcard(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.iter().enumerate().any(|(i, b)| *b == b'%' && (i == 0 || bytes[i - 1] != b'\\'))
}

fn wildcard_to_regex(string: &str) -> String {
    let mut regex = String::from("^");
    let mut token = String::new();
    let mut rest = string;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("\\%") {
            if !token.is_empty() {
                regex.push_str(&regex::escape(&token));
            }
            token.clear();
            regex.push('%');
            rest = r;
        } else if rest.starts_with('%') {
            if !token.is_empty() {
                regex.push_str(&regex::escape(&token));
            }
            token.clear();
            regex.push_str(".*");
            rest = rest.trim_start_matches('%');
        } else {
            let c = rest.chars().next().unwrap();
            token.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    if !token.is_empty() {
        regex.push_str(&regex::escape(&token));
    }
    regex.push('$');
    regex
}

impl MatchAny {
    pub fn from(field: Option<&str>, getter: Getter, values: Vec<Value>) -> MatchAny {
        let mut exact_matches = HashSet::new();
        let mut patterns = Vec::new();
        for value in &values {
            if !value.is_null() {
                let string = value.to_string_lossy();
                if has_unescaped_wildcard(&string) {
                    patterns.push(wildcard_to_regex(&string));
                } else {
                    exact_matches.insert(string.replace("\\%", "%"));
                }
            }
        }
        let match_when_missing = values.iter().any(|v| v.is_null() || v.as_str() == Some(""));
        let pattern = if patterns.is_empty() {
            None
        } else {
            Some(Regex::new(&format!("({})", patterns.join("|"))).expect("invalid wildcard regex"))
        };
        MatchAny { field: field.map(Arc::from), values, exact_matches, pattern, match_when_missing, getter }
    }

    fn evaluate_input(&self, input: &dyn ExpressionInput, match_keys: Option<&mut MatchKeys>) -> bool {
        match (&self.getter, &self.field) {
            (Getter::Data(DataType::GetTag), Some(field)) => {
                let value = input.get_tag_nested(field);
                self.evaluate_value(match_keys, value.as_deref())
            }
            (Getter::Data(d), Some(field)) => {
                let value = d.apply(input, field);
                self.evaluate_value(match_keys, Some(&value))
            }
            (Getter::Data(_), None) => self.evaluate_value(match_keys, None),
            (Getter::Custom(g), field) => {
                let value = g.apply(input, field.as_deref());
                self.evaluate_value(match_keys, Some(&value))
            }
        }
    }

    fn evaluate_value(&self, match_keys: Option<&mut MatchKeys>, value: Option<&Value>) -> bool {
        match value {
            None | Some(Value::Null) => self.match_when_missing,
            Some(Value::String(s)) if s.is_empty() => self.match_when_missing,
            Some(Value::List(list)) => {
                if list.is_empty() {
                    self.match_when_missing
                } else {
                    let mut match_keys = match_keys;
                    list.iter().any(|item| self.evaluate_value(match_keys.as_deref_mut(), Some(item)))
                }
            }
            Some(Value::Map(_)) => false,
            Some(value) => {
                let str = value.to_string_lossy();
                if self.exact_matches.contains(str.as_ref())
                    || (self.field.is_none() && self.exact_matches.is_empty())
                    || self.pattern.as_ref().is_some_and(|p| p.is_match(&str))
                {
                    if let (Some(keys), Some(field)) = (match_keys, &self.field) {
                        keys.push(field.clone());
                    }
                    true
                } else {
                    false
                }
            }
        }
    }

    /// True if this matches any non-empty value (`matchAny("key", "%")`).
    pub fn is_match_anything(&self) -> bool {
        !self.match_when_missing
            && self.exact_matches.is_empty()
            && self.pattern.as_ref().is_some_and(|p| p.as_str() == "(^.*$)")
    }

    /// True when this must be evaluated on every input, not only ones that have `field`.
    pub fn must_always_evaluate(&self) -> bool {
        self.field.is_none() || self.match_when_missing
    }
}

/// A boolean expression that matches input elements.
#[derive(Clone, Debug)]
pub enum Expression {
    /// A constant boolean value ([`Expression::TRUE`]/[`Expression::FALSE`]).
    Constant(bool),
    And(Vec<Expression>),
    Or(Vec<Expression>),
    Not(Box<Expression>),
    MatchAny(Arc<MatchAny>),
    /// Evaluates to true if an input element contains any value for the field.
    MatchField(Arc<str>),
    /// Evaluates to true if an input element has a geometry type matching the type (one of [`POINT_TYPE`],
    /// [`LINESTRING_TYPE`], [`POLYGON_TYPE`], [`UNKNOWN_GEOMETRY_TYPE`]).
    MatchType(&'static str),
    /// Evaluates to true if an input element has source matching this value.
    MatchSource(Arc<str>),
    /// Evaluates to true if an input element has source layer matching this value.
    MatchSourceLayer(Arc<str>),
    Custom(Arc<dyn CustomExpression>),
}

impl PartialEq for Expression {
    fn eq(&self, other: &Self) -> bool {
        use Expression::*;
        match (self, other) {
            (Constant(a), Constant(b)) => a == b,
            (And(a), And(b)) => a == b,
            (Or(a), Or(b)) => a == b,
            (Not(a), Not(b)) => a == b,
            (MatchAny(a), MatchAny(b)) => Arc::ptr_eq(a, b) || a == b,
            (MatchField(a), MatchField(b)) => a == b,
            (MatchType(a), MatchType(b)) => a == b,
            (MatchSource(a), MatchSource(b)) => a == b,
            (MatchSourceLayer(a), MatchSourceLayer(b)) => a == b,
            (Custom(a), Custom(b)) => Arc::ptr_eq(a, b) || a.dyn_eq(&**b),
            _ => false,
        }
    }
}

/// `and(children)`: true if all children match (true when empty).
pub fn and(children: impl IntoIterator<Item = Expression>) -> Expression {
    Expression::And(children.into_iter().collect())
}

/// `or(children)`: true if any child matches (false when empty).
pub fn or(children: impl IntoIterator<Item = Expression>) -> Expression {
    Expression::Or(children.into_iter().collect())
}

pub fn not(child: Expression) -> Expression {
    Expression::Not(Box::new(child))
}

/// Returns an expression that evaluates to true if the value for `field` tag is any of `values`.
///
/// `values` can contain exact matches, "%text%" to match any value containing "text", or "" to match any value.
pub fn match_any<V: Into<Value>>(field: &str, values: impl IntoIterator<Item = V>) -> Expression {
    match_any_typed(Some(field), DataType::GetTag, values)
}

/// Same as [`match_any`] but considers the tag as a specified data type (or dynamic getter) and then converts it to a
/// string. `field` may be `None` with a dynamic getter.
pub fn match_any_typed<V: Into<Value>>(
    field: Option<&str>,
    getter: impl Into<Getter>,
    values: impl IntoIterator<Item = V>,
) -> Expression {
    Expression::MatchAny(Arc::new(MatchAny::from(field, getter.into(), values.into_iter().map(Into::into).collect())))
}

/// Returns an expression that evaluates to true if the element has any value for tag `field`.
pub fn match_field(field: &str) -> Expression {
    Expression::MatchField(Arc::from(field))
}

/// Returns an expression that evaluates to true if the geometry of an element matches `type_name` ("linestring",
/// "point", "polygon", or "unknown_type"). Panics for other values; see [`try_match_type`].
pub fn match_type(type_name: &str) -> Expression {
    try_match_type(type_name).unwrap_or_else(|e| panic!("{e}"))
}

/// Same as [`match_type`] but returns an error for unsupported types.
pub fn try_match_type(type_name: &str) -> anyhow::Result<Expression> {
    Ok(Expression::MatchType(match type_name {
        LINESTRING_TYPE => LINESTRING_TYPE,
        POINT_TYPE => POINT_TYPE,
        POLYGON_TYPE => POLYGON_TYPE,
        UNKNOWN_GEOMETRY_TYPE => UNKNOWN_GEOMETRY_TYPE,
        other => anyhow::bail!("Unsupported type: {other}"),
    }))
}

/// Returns an expression that evaluates to true if the geometry of an element matches `geometry_type`. Panics for
/// [`GeometryType::Unknown`].
pub fn match_geometry_type(geometry_type: GeometryType) -> Expression {
    Expression::MatchType(match geometry_type {
        GeometryType::Point => POINT_TYPE,
        GeometryType::Line => LINESTRING_TYPE,
        GeometryType::Polygon => POLYGON_TYPE,
        GeometryType::Unknown => panic!("Unsupported type: {geometry_type:?}"),
    })
}

/// Returns an expression that evaluates to true if the source of an element matches `source`.
pub fn match_source(source: &str) -> Expression {
    Expression::MatchSource(Arc::from(source))
}

/// Returns an expression that evaluates to true if the source layer of an element matches `layer`.
pub fn match_source_layer(layer: &str) -> Expression {
    Expression::MatchSourceLayer(Arc::from(layer))
}

/// Wraps a closure as a custom expression.
pub fn custom(f: impl Fn(&dyn ExpressionInput, Option<&mut MatchKeys>) -> bool + Send + Sync + 'static) -> Expression {
    Expression::Custom(Arc::new(FnExpression(f)))
}

fn const_bool(value: bool) -> Expression {
    Expression::Constant(value)
}

/// Java `Format.quote`: a quoted, java-escaped string.
pub(crate) fn java_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7f => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04X}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Error returned when java code generation is not supported for an expression.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("unsupported operation: {0}")]
pub struct UnsupportedOperation(pub String);

impl Expression {
    pub const TRUE: Expression = Expression::Constant(true);
    pub const FALSE: Expression = Expression::Constant(false);

    /// Returns true if this expression matches an input element.
    pub fn evaluate(&self, input: &dyn ExpressionInput) -> bool {
        self.evaluate_with_keys(input, None)
    }

    /// Returns true if this expression matches an input element, adding any keys responsible for triggering the match
    /// to `match_keys`.
    pub fn evaluate_with_keys(&self, input: &dyn ExpressionInput, match_keys: Option<&mut MatchKeys>) -> bool {
        match self {
            Expression::Constant(v) => *v,
            Expression::And(children) => {
                let mut match_keys = match_keys;
                for child in children {
                    if !child.evaluate_with_keys(input, match_keys.as_deref_mut()) {
                        if let Some(keys) = match_keys {
                            keys.clear();
                        }
                        return false;
                    }
                }
                true
            }
            Expression::Or(children) => {
                let mut match_keys = match_keys;
                children.iter().any(|child| child.evaluate_with_keys(input, match_keys.as_deref_mut()))
            }
            Expression::Not(child) => !child.evaluate_with_keys(input, None),
            Expression::MatchAny(any) => any.evaluate_input(input, match_keys),
            Expression::MatchField(field) => {
                let present = match input.get_tag_nested(field).as_deref() {
                    None | Some(Value::Null) => false,
                    Some(Value::String(s)) => !s.is_empty(),
                    Some(Value::List(l)) => !l.is_empty(),
                    Some(_) => true,
                };
                if present && let Some(keys) = match_keys {
                    keys.push(field.clone());
                }
                present
            }
            Expression::MatchType(t) => input.as_with_geometry_type().is_some_and(|g| match *t {
                LINESTRING_TYPE => g.can_be_line(),
                POLYGON_TYPE => g.can_be_polygon(),
                POINT_TYPE => g.is_point(),
                _ => false,
            }),
            Expression::MatchSource(source) => input.source() == Some(&**source),
            Expression::MatchSourceLayer(layer) => input.source_layer() == Some(&**layer),
            Expression::Custom(c) => c.evaluate(input, match_keys),
        }
    }

    /// Returns a copy of this expression where every nested instance of `a` is replaced with `b`.
    pub fn replace(&self, a: &Expression, b: &Expression) -> Expression {
        self.replace_matching(&|e| e == a, b)
    }

    /// Returns a copy of this expression where every nested instance matching `test` is replaced with `b`.
    pub fn replace_matching(&self, test: &dyn Fn(&Expression) -> bool, b: &Expression) -> Expression {
        if test(self) {
            return b.clone();
        }
        match self {
            Expression::Not(child) => not(child.replace_matching(test, b)),
            Expression::Or(children) => or(children.iter().map(|c| c.replace_matching(test, b))),
            Expression::And(children) => and(children.iter().map(|c| c.replace_matching(test, b))),
            other => other.clone(),
        }
    }

    /// Calls `f` for every expression within the current one (including itself).
    pub fn visit(&self, f: &mut dyn FnMut(&Expression)) {
        f(self);
        match self {
            Expression::Not(child) => child.visit(f),
            Expression::Or(children) | Expression::And(children) => children.iter().for_each(|c| c.visit(f)),
            _ => {}
        }
    }

    /// Returns true if this expression or any subexpression matches `filter`.
    pub fn contains(&self, filter: &dyn Fn(&Expression) -> bool) -> bool {
        filter(self)
            || match self {
                Expression::Not(child) => child.contains(filter),
                Expression::Or(children) | Expression::And(children) => children.iter().any(|c| c.contains(filter)),
                _ => false,
            }
    }

    /// Returns a copy of this expression where any parts whose value is known are replaced with [`Expression::TRUE`]
    /// or [`Expression::FALSE`], for a set of features where [`PartialInput`] is known ahead of time.
    pub fn partial_evaluate(&self, input: &PartialInput) -> Expression {
        match self {
            Expression::Not(e) => not(e.partial_evaluate(input)),
            Expression::And(es) => and(es.iter().map(|e| e.partial_evaluate(input))),
            Expression::Or(es) => or(es.iter().map(|e| e.partial_evaluate(input))),
            Expression::MatchAny(any) => match &any.field {
                // dynamic getters always need to be evaluated
                None => self.clone(),
                Some(field) => match input.get_tag_nested(field) {
                    None => self.clone(),
                    Some(value) => const_bool(any.evaluate_value(None, Some(&value))),
                },
            },
            Expression::MatchField(field) => {
                if input.has_tag(field) {
                    Expression::TRUE
                } else {
                    self.clone()
                }
            }
            Expression::MatchType(t) => {
                if input.types.is_empty() || input.types.contains(&GeometryType::Unknown) {
                    self.clone()
                } else {
                    let geometry_type = match *t {
                        LINESTRING_TYPE => GeometryType::Line,
                        POLYGON_TYPE => GeometryType::Polygon,
                        POINT_TYPE => GeometryType::Point,
                        _ => GeometryType::Unknown,
                    };
                    partial_evaluate_contains(&input.types, &geometry_type, self)
                }
            }
            Expression::MatchSource(s) => partial_evaluate_contains(&input.source, &s.to_string(), self),
            Expression::MatchSourceLayer(l) => partial_evaluate_contains(&input.layer, &l.to_string(), self),
            other => other.clone(),
        }
    }

    /// Returns Java code that can be used to reconstruct this expression.
    pub fn generate_java_code(&self) -> Result<String, UnsupportedOperation> {
        fn list(items: &[Expression]) -> Result<String, UnsupportedOperation> {
            Ok(items.iter().map(Expression::generate_java_code).collect::<Result<Vec<_>, _>>()?.join(", "))
        }
        Ok(match self {
            Expression::Constant(true) => "TRUE".to_string(),
            Expression::Constant(false) => "FALSE".to_string(),
            Expression::And(children) => format!("and({})", list(children)?),
            Expression::Or(children) => format!("or({})", list(children)?),
            Expression::Not(child) => format!("not({})", child.generate_java_code()?),
            Expression::MatchAny(any) => {
                if any.getter != Getter::Data(DataType::GetTag) {
                    return Err(UnsupportedOperation("Code generation only supported for default getTag".into()));
                }
                let mut value_strings = Vec::with_capacity(any.values.len());
                for value in &any.values {
                    match value {
                        Value::String(s) => value_strings.push(java_quote(s)),
                        other => {
                            return Err(UnsupportedOperation(format!(
                                "Code generation only supported for string values, found: {other:?}"
                            )));
                        }
                    }
                }
                let field = any.field.as_deref().map_or_else(|| "null".to_string(), java_quote);
                format!("matchAny({}, {})", field, value_strings.join(", "))
            }
            Expression::MatchField(field) => format!("matchField({})", java_quote(field)),
            Expression::MatchType(t) => format!("matchType({})", java_quote(t)),
            Expression::MatchSource(s) => format!("matchSource({})", java_quote(s)),
            Expression::MatchSourceLayer(l) => format!("matchSourceLayer({})", java_quote(l)),
            Expression::Custom(c) => c.generate_java_code().ok_or_else(|| UnsupportedOperation(format!("{c:?}")))?,
        })
    }

    pub fn is_true(&self) -> bool {
        matches!(self, Expression::Constant(true))
    }

    pub fn is_false(&self) -> bool {
        matches!(self, Expression::Constant(false))
    }
}

fn partial_evaluate_contains<T: Eq + std::hash::Hash>(set: &HashSet<T>, item: &T, this: &Expression) -> Expression {
    if set.is_empty() {
        this.clone()
    } else if set.len() == 1 {
        const_bool(set.contains(item))
    } else if set.contains(item) {
        this.clone()
    } else {
        Expression::FALSE
    }
}

fn distinct(items: impl IntoIterator<Item = Expression>) -> Vec<Expression> {
    let mut result: Vec<Expression> = Vec::new();
    for item in items {
        if !result.contains(&item) {
            result.push(item);
        }
    }
    result
}

impl Simplifiable for Expression {
    fn simplify_once(&self) -> Expression {
        match self {
            Expression::And(children) => {
                if children.is_empty() {
                    return Expression::TRUE;
                }
                if children.len() == 1 {
                    return children[0].simplify_once();
                }
                if children.contains(&Expression::FALSE) {
                    return Expression::FALSE;
                }
                let hoisted = children
                    .iter()
                    .flat_map(|child| match child {
                        Expression::And(grandchildren) => grandchildren.clone(),
                        other => vec![other.clone()],
                    })
                    // and() == and(TRUE) == and(TRUE, TRUE) == TRUE, so safe to remove all here
                    .filter(|c| !c.is_true());
                and(distinct(hoisted).iter().map(Simplifiable::simplify_once))
            }
            Expression::Or(children) => {
                if children.is_empty() {
                    return Expression::FALSE;
                }
                if children.len() == 1 {
                    return children[0].simplify_once();
                }
                if children.contains(&Expression::TRUE) {
                    return Expression::TRUE;
                }
                let hoisted = children
                    .iter()
                    .flat_map(|child| match child {
                        Expression::Or(grandchildren) => grandchildren.clone(),
                        other => vec![other.clone()],
                    })
                    .filter(|c| !c.is_false());
                or(distinct(hoisted).iter().map(Simplifiable::simplify_once))
            }
            Expression::Not(child) => match &**child {
                Expression::Or(children) => and(children.iter().cloned().map(not)),
                Expression::And(children) => or(children.iter().cloned().map(not)),
                Expression::Not(grandchild) => (**grandchild).clone(),
                Expression::Constant(v) => const_bool(!v),
                Expression::MatchAny(any)
                    if any.values.len() == 1 && any.values[0].as_str() == Some("") && any.field.is_some() =>
                {
                    match_field(any.field.as_deref().unwrap())
                }
                _ => self.clone(),
            },
            Expression::MatchAny(any) => {
                if any.is_match_anything()
                    && let Some(field) = &any.field
                {
                    return Expression::MatchField(field.clone());
                }
                if let Getter::Custom(g) = &any.getter
                    && let Some(simplified) = g.simplify_once()
                {
                    let mut copy = (**any).clone();
                    copy.getter = Getter::Custom(simplified);
                    return Expression::MatchAny(Arc::new(copy));
                }
                self.clone()
            }
            Expression::Custom(c) => c.simplify_once().unwrap_or_else(|| self.clone()),
            other => other.clone(),
        }
    }
}

impl fmt::Display for Expression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.generate_java_code() {
            Ok(code) => f.write_str(&code),
            Err(_) => write!(f, "{self:?}"),
        }
    }
}

/// Partial attributes of a set of features that are known ahead of time, for example a hive-partitioned parquet input
/// file.
///
/// Features within this set will only add tags but not change them, and the source/layer/geometry type will be one of
/// the values specified in those sets. If a set is empty, the values are not known ahead of time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PartialInput {
    pub source: HashSet<String>,
    pub layer: HashSet<String>,
    pub tags: Tags,
    pub types: HashSet<GeometryType>,
}

impl PartialInput {
    pub fn new(
        source: impl IntoIterator<Item = String>,
        layer: impl IntoIterator<Item = String>,
        tags: Tags,
        types: impl IntoIterator<Item = GeometryType>,
    ) -> PartialInput {
        PartialInput {
            source: source.into_iter().collect(),
            layer: layer.into_iter().collect(),
            tags,
            types: types.into_iter().collect(),
        }
    }

    pub fn of_source(source: &str) -> PartialInput {
        PartialInput { source: HashSet::from([source.to_string()]), ..Default::default() }
    }
}

impl WithTags for PartialInput {
    fn tags(&self) -> &Tags {
        &self.tags
    }
}

impl ExpressionInput for PartialInput {}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tags;

    /// Test input standing in for Java's `SimpleFeature`.
    #[derive(Debug, Default)]
    pub(crate) struct TestFeature {
        pub tags: Tags,
        pub geometry_type: GeometryType,
        pub source: Option<String>,
        pub layer: Option<String>,
    }

    impl WithTags for TestFeature {
        fn tags(&self) -> &Tags {
            &self.tags
        }
    }

    impl WithGeometryType for TestFeature {
        fn is_point(&self) -> bool {
            self.geometry_type == GeometryType::Point
        }

        fn can_be_polygon(&self) -> bool {
            self.geometry_type == GeometryType::Polygon
        }

        fn can_be_line(&self) -> bool {
            self.geometry_type == GeometryType::Line
        }
    }

    impl ExpressionInput for TestFeature {
        fn as_with_geometry_type(&self) -> Option<&dyn WithGeometryType> {
            Some(self)
        }

        fn source(&self) -> Option<&str> {
            self.source.as_deref()
        }

        fn source_layer(&self) -> Option<&str> {
            self.layer.as_deref()
        }
    }

    /// Java `ExpressionTestUtil.featureWithTags`: a point feature with string tags.
    pub(crate) fn feature_with_tags(kvs: &[(&str, Option<&str>)]) -> TestFeature {
        let mut tags = Tags::new();
        for (k, v) in kvs {
            tags.insert(k.to_string(), v.map_or(Value::Null, Value::str));
        }
        TestFeature { tags, geometry_type: GeometryType::Point, source: None, layer: None }
    }

    pub(crate) fn fwt(kvs: &[(&str, &str)]) -> TestFeature {
        feature_with_tags(&kvs.iter().map(|(k, v)| (*k, Some(*v))).collect::<Vec<_>>())
    }

    fn match_ab() -> Expression {
        match_any("a", ["b"])
    }
    fn match_cd() -> Expression {
        match_any("c", ["d"])
    }
    fn match_bc() -> Expression {
        match_any("b", ["c"])
    }

    fn pattern_of(e: &Expression) -> Option<String> {
        match e {
            Expression::MatchAny(any) => any.pattern.as_ref().map(|p| p.as_str().to_string()),
            _ => panic!(),
        }
    }

    fn exact_of(e: &Expression) -> HashSet<String> {
        match e {
            Expression::MatchAny(any) => any.exact_matches.clone(),
            _ => panic!(),
        }
    }

    #[test]
    fn test_simplify() {
        assert_eq!(match_ab(), match_ab().simplify());
    }

    #[test]
    fn test_simplify_adjacent_ors() {
        assert_eq!(or([match_ab(), match_cd()]), or([or([match_ab()]), or([match_cd()])]).simplify());
    }

    #[test]
    fn test_simplify_duplicates() {
        assert_eq!(match_ab(), or([or([match_ab()]), or([match_ab()])]).simplify());
        assert_eq!(match_ab(), and([match_ab(), match_ab()]).simplify());
    }

    #[test]
    fn test_match_any_equals() {
        assert_eq!(match_any("a", ["b%"]), match_any("a", ["b%"]));
    }

    #[test]
    fn test_simplify_single_children() {
        assert_eq!(match_ab(), or([match_ab()]).simplify());
        assert_eq!(match_ab(), and([match_ab()]).simplify());
        assert_eq!(match_ab(), or([or([and([and([match_ab()])])])]).simplify());
        assert_eq!(or([match_ab(), match_bc()]), or([or([and([and([match_ab()])]), match_bc()])]).simplify());
        assert_eq!(
            or([and([match_ab(), match_cd()]), match_bc()]),
            or([or([and([and([match_ab()]), match_cd()]), match_bc()])]).simplify()
        );
    }

    #[test]
    fn test_not_not_and_demorgans() {
        assert_eq!(match_ab(), not(not(match_ab())).simplify());
        assert_eq!(or([not(match_ab()), not(match_bc())]), not(and([match_ab(), match_bc()])).simplify());
        assert_eq!(and([not(match_ab()), not(match_bc())]), not(or([match_ab(), match_bc()])).simplify());
    }

    #[test]
    fn test_simplify_constants() {
        use Expression as E;
        assert_eq!(E::FALSE, and([E::FALSE]).simplify());
        assert_eq!(E::FALSE, and([E::FALSE, match_ab()]).simplify());
        assert_eq!(E::FALSE, or([E::FALSE]).simplify());
        assert_eq!(E::TRUE, and([E::TRUE]).simplify());
        assert_eq!(match_ab(), and([E::TRUE, match_ab()]).simplify());
        assert_eq!(E::TRUE, or([E::TRUE, match_ab()]).simplify());
        assert_eq!(E::TRUE, and([E::TRUE, E::TRUE]).simplify());
        assert_eq!(E::TRUE, and([E::TRUE, and([])]).simplify());
        assert_eq!(E::TRUE, and([and([E::TRUE])]).simplify());
        assert_eq!(E::TRUE, and([and([E::TRUE]), E::TRUE]).simplify());
        assert_eq!(E::TRUE, and([E::TRUE, and([E::TRUE]), E::TRUE]).simplify());
        assert_eq!(match_ab(), and([E::TRUE, and([E::TRUE]), match_ab()]).simplify());
        assert_eq!(E::FALSE, or([or([E::FALSE])]).simplify());
        assert_eq!(E::FALSE, or([or([E::FALSE]), E::FALSE]).simplify());
        assert_eq!(E::FALSE, or([E::FALSE, or([E::FALSE]), E::FALSE]).simplify());
        assert_eq!(match_ab(), or([E::FALSE, or([E::FALSE]), match_ab()]).simplify());
        assert_eq!(E::FALSE, not(E::TRUE).simplify());
        assert_eq!(E::TRUE, not(E::FALSE).simplify());
    }

    #[test]
    fn test_evaluate_empty_and() {
        assert_eq!(and([]).evaluate(&fwt(&[])), and([]).simplify().evaluate(&fwt(&[])));
    }

    #[test]
    fn test_replace() {
        assert_eq!(
            or([not(match_cd()), match_cd(), and([match_cd(), match_bc()])]),
            or([not(match_ab()), match_ab(), and([match_ab(), match_bc()])]).replace(&match_ab(), &match_cd())
        );
        assert_eq!(
            or([not(match_cd()), match_cd(), and([match_cd(), match_cd()])]),
            or([not(match_cd()), match_cd(), and([match_cd(), match_cd()])])
                .replace_matching(&|e| *e == match_ab() || *e == match_bc(), &match_cd())
        );
    }

    #[test]
    fn test_contains() {
        assert_eq!(None, pattern_of(&match_cd()));
        assert!(match_cd().contains(&|e| *e == match_cd()));
        assert!(or([not(match_cd())]).contains(&|e| *e == match_cd()));
        assert!(!match_cd().contains(&|e| *e == match_ab()));
        assert!(!or([not(match_cd())]).contains(&|e| *e == match_ab()));
    }

    #[test]
    fn test_wildcards() {
        let m = match_any("key", ["a%"]);
        assert!(exact_of(&m).is_empty());
        assert!(pattern_of(&m).is_some());
        assert!(m.evaluate(&fwt(&[("key", "abc")])));
        assert!(m.evaluate(&fwt(&[("key", "a")])));
        assert!(!m.evaluate(&fwt(&[("key", "cba")])));

        let m = match_any("key", ["%a"]);
        assert!(m.evaluate(&fwt(&[("key", "cba")])));
        assert!(m.evaluate(&fwt(&[("key", "a")])));
        assert!(!m.evaluate(&fwt(&[("key", "abc")])));

        let m = match_any("key", ["%a%"]);
        assert!(m.evaluate(&fwt(&[("key", "bab")])));
        assert!(m.evaluate(&fwt(&[("key", "a")])));
        assert!(!m.evaluate(&fwt(&[("key", "c")])));

        let m = match_any("key", ["%"]);
        assert!(exact_of(&m).is_empty());
        assert_eq!(match_field("key"), m.simplify());
        assert!(m.evaluate(&fwt(&[("key", "abc")])));
        assert!(!m.evaluate(&fwt(&[("key", "")])));

        let m = match_any("key", ["a%c"]);
        assert!(m.evaluate(&fwt(&[("key", "abc")])));
        assert!(m.evaluate(&fwt(&[("key", "ac")])));
        assert!(!m.evaluate(&fwt(&[("key", "ab")])));
    }

    #[test]
    fn test_match_nested() {
        let l = |v: Vec<&str>| OfMap(tags! {"key" => v});
        assert!(match_any("key", ["a%"]).evaluate(&l(vec!["abc"])));
        assert!(match_any("key", ["abc"]).evaluate(&l(vec!["abc"])));
        assert!(match_field("key").evaluate(&l(vec!["abc"])));
        assert!(!match_field("key").evaluate(&l(vec![])));
        assert!(!match_any("key", ["a%"]).evaluate(&OfMap(tags! {"key" => Value::map(tags!{"key2" => "abc"})})));
        assert!(match_any("key", ["a%"]).evaluate(&l(vec!["a"])));
        assert!(!match_any("key", ["a%"]).evaluate(&l(vec!["cba"])));
    }

    #[test]
    fn test_nested_query() {
        let e = match_any("key.key2", ["a"]);
        assert!(!e.evaluate(&OfMap(tags! {"other" => "value"})));
        assert!(!e.evaluate(&OfMap(tags! {"key" => "value"})));
        assert!(e.evaluate(&OfMap(tags! {"key" => Value::map(tags!{"key2" => "a"})})));
        assert!(!e.evaluate(&OfMap(tags! {"key" => Value::map(tags!{"key2" => "b"})})));
    }

    #[test]
    fn test_wildcard_escape() {
        let t = |pat: &str, v: &str| match_any("key", [pat]).evaluate(&fwt(&[("key", v)]));
        assert!(t("a\\%", "a%"));
        assert!(!t("a\\%", "ab"));
        assert!(t("a\\%b", "a%b"));
        assert!(t("%a\\%b%", "dda%b"));
        assert!(t("\\%%", "%abc"));
        assert!(t("%\\%", "abc%"));
        assert!(t("%\\%%", "a%c"));
        assert!(t("%\\%%", "%"));
        assert!(!t("\\%%", "abc%"));
    }

    #[test]
    fn test_stringify_expression() {
        assert_eq!(Ok("matchAny(\"key\", \"true\")".to_string()), match_any("key", ["true"]).generate_java_code());
        assert_eq!(Ok("matchAny(\"key\", \"foo\")".to_string()), match_any("key", ["foo"]).generate_java_code());
        assert_eq!("matchAny(\"key\", \"foo\")", match_any("key", ["foo"]).to_string());
        assert_eq!(
            "and(matchField(\"a\"), or(TRUE, not(matchType(\"point\"))))",
            and([match_field("a"), or([Expression::TRUE, not(match_type("point"))])]).to_string()
        );
        let e = match_any_typed(Some("key"), DataType::GetDirection, [1]);
        assert!(e.generate_java_code().is_err());
    }

    #[test]
    fn test_evaluate() {
        let f = fwt(&[("key1", "value1"), ("key2", "value2"), ("key3", "")]);
        let m = |k: &str, v: &str| match_any(k, [v]);
        assert!(and([m("key1", "value1"), m("key2", "value2")]).evaluate(&f));
        assert!(!and([m("key1", "value1"), m("key2", "wrong")]).evaluate(&f));
        assert!(!and([m("key1", "wrong"), m("key2", "value2")]).evaluate(&f));
        assert!(!and([m("key1", "wrong"), m("key2", "wrong")]).evaluate(&f));

        assert!(or([m("key1", "value1"), m("key2", "value2")]).evaluate(&f));
        assert!(or([m("key1", "value1"), m("key2", "wrong")]).evaluate(&f));
        assert!(or([m("key1", "wrong"), m("key2", "value2")]).evaluate(&f));
        assert!(!or([m("key1", "wrong"), m("key2", "wrong")]).evaluate(&f));

        assert!(!not(m("key1", "value1")).evaluate(&f));
        assert!(not(m("key1", "wrong")).evaluate(&f));

        assert!(match_field("key1").evaluate(&f));
        assert!(!match_field("wrong").evaluate(&f));
        assert!(not(m("key1", "")).evaluate(&f));
        assert!(m("wrong", "").evaluate(&f));
        assert!(m("key3", "").evaluate(&f));

        assert!(Expression::TRUE.evaluate(&f));
        assert!(!Expression::FALSE.evaluate(&f));
    }

    #[test]
    fn test_custom_expression() {
        let c = custom(|input, _| input.has_tag("abc"));
        let matching = fwt(&[("abc", "123")]);
        let not_matching = fwt(&[("abcd", "123")]);
        for (input, expected) in [(&matching, true), (&not_matching, false)] {
            assert_eq!(expected, c.evaluate(input));
            assert_eq!(expected, and([c.clone()]).evaluate(input));
            assert_eq!(expected, and([c.clone(), c.clone()]).evaluate(input));
            assert_eq!(expected, or([c.clone(), c.clone()]).evaluate(input));
            assert_eq!(expected, and([Expression::TRUE, c.clone()]).evaluate(input));
            assert_eq!(expected, or([Expression::FALSE, c.clone()]).evaluate(input));
        }
    }

    #[test]
    fn test_source_filter() {
        let feature = |source: &str, layer: &str| TestFeature {
            source: Some(source.into()),
            layer: Some(layer.into()),
            ..Default::default()
        };
        assert!(match_source("source").evaluate(&feature("source", "layer")));
        assert!(!match_source("source").evaluate(&feature("other source", "layer")));
        assert!(match_source_layer("layer").evaluate(&feature("source", "layer")));
        assert!(!match_source_layer("layer").evaluate(&feature("other source", "other layer")));
    }

    fn pi(source: &[&str], layer: &[&str], tags: Tags, types: &[GeometryType]) -> PartialInput {
        PartialInput::new(
            source.iter().map(|s| s.to_string()),
            layer.iter().map(|s| s.to_string()),
            tags,
            types.iter().copied(),
        )
    }

    #[test]
    fn test_partial_evaluate_match_field() {
        assert_eq!(
            match_field("field"),
            match_field("field").partial_evaluate(&pi(&[], &[], tags! {"other" => "value"}, &[]))
        );
        assert_eq!(
            Expression::TRUE,
            match_field("field").partial_evaluate(&pi(&[], &[], tags! {"field" => "value"}, &[]))
        );
    }

    #[test]
    fn test_partial_evaluate_match_any() {
        let e = match_any("field", ["value1", "other%"]);
        assert_eq!(e, e.partial_evaluate(&pi(&[], &[], tags! {"other" => "value"}, &[])));
        assert_eq!(e, e.partial_evaluate(&pi(&[], &[], tags! {}, &[])));
        assert_eq!(Expression::TRUE, e.partial_evaluate(&pi(&[], &[], tags! {"field" => "value1"}, &[])));
        assert_eq!(Expression::TRUE, e.partial_evaluate(&pi(&[], &[], tags! {"field" => "other"}, &[])));
        assert_eq!(Expression::TRUE, e.partial_evaluate(&pi(&[], &[], tags! {"field" => "other..."}, &[])));
        assert_eq!(Expression::FALSE, e.partial_evaluate(&pi(&[], &[], tags! {"field" => "not a value"}, &[])));
    }

    #[test]
    fn test_partial_evaluate_match_any_dynamic_getter() {
        let e = match_any_typed(None, getter_fn(|_, _| Value::str("")), [1, 2, 3]);
        assert_eq!(e, e.partial_evaluate(&pi(&[], &[], tags! {"other" => "value"}, &[])));
        assert_eq!(e, e.partial_evaluate(&pi(&[], &[], tags! {}, &[])));
        assert_eq!(e, e.partial_evaluate(&pi(&[], &[], tags! {"field" => "value1"}, &[])));
        assert_eq!(e, e.partial_evaluate(&pi(&[], &[], tags! {"field" => "not a value"}, &[])));
    }

    #[test]
    fn test_partial_evaluate_match_geometry_type() {
        use GeometryType::*;
        let e = match_geometry_type(Point);
        assert_eq!(e, e.partial_evaluate(&pi(&[], &[], tags! {}, &[])));
        assert_eq!(e, e.partial_evaluate(&pi(&[], &[], tags! {}, &[Point, Unknown])));
        assert_eq!(e, e.partial_evaluate(&pi(&[], &[], tags! {}, &[Unknown])));
        assert_eq!(e, e.partial_evaluate(&pi(&[], &[], tags! {}, &[Point, Polygon])));
        assert_eq!(Expression::TRUE, e.partial_evaluate(&pi(&[], &[], tags! {}, &[Point])));
        assert_eq!(Expression::FALSE, e.partial_evaluate(&pi(&[], &[], tags! {}, &[Polygon])));
    }

    #[test]
    fn test_partial_evaluate_match_source() {
        let e = match_source("source");
        assert_eq!(e, e.partial_evaluate(&pi(&["source", "others"], &[], tags! {}, &[])));
        assert_eq!(e, e.partial_evaluate(&pi(&[], &[], tags! {}, &[])));
        assert_eq!(Expression::TRUE, e.partial_evaluate(&pi(&["source"], &[], tags! {}, &[])));
        assert_eq!(Expression::FALSE, e.partial_evaluate(&pi(&["other source"], &[], tags! {}, &[])));
        assert_eq!(Expression::TRUE, e.partial_evaluate(&PartialInput::of_source("source")));
    }

    #[test]
    fn test_partial_evaluate_match_source_layer() {
        let e = match_source_layer("layer");
        assert_eq!(e, e.partial_evaluate(&pi(&[], &["layer", "others"], tags! {}, &[])));
        assert_eq!(e, e.partial_evaluate(&pi(&[], &[], tags! {}, &[])));
        assert_eq!(Expression::TRUE, e.partial_evaluate(&pi(&[], &["layer"], tags! {}, &[])));
        assert_eq!(Expression::FALSE, e.partial_evaluate(&pi(&[], &["other layer"], tags! {}, &[])));
    }

    #[test]
    fn test_match_keys() {
        let e = or([and([match_any("a", ["b"]), match_field("c")]), match_any("d", ["e"])]);
        let mut keys = MatchKeys::new();
        assert!(e.evaluate_with_keys(&fwt(&[("a", "b"), ("d", "e")]), Some(&mut keys)));
        assert_eq!(vec![Arc::<str>::from("d")], keys);
        keys.clear();
        assert!(e.evaluate_with_keys(&fwt(&[("a", "b"), ("c", "x")]), Some(&mut keys)));
        assert_eq!(vec![Arc::<str>::from("a"), Arc::from("c")], keys);
    }
}
