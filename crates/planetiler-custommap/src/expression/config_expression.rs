//! Port of `ConfigExpression.java`: a function defined in part of a schema config that produces an output value (min
//! zoom, attribute value, etc.) for a feature at runtime.
//!
//! This can be parsed from a structured object that lists combinations of tag key/values, an embedded script, or a
//! combination of the two (see [`crate::config_expression_parser`]).

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use planetiler_core::expression::data_type::DataType;
use planetiler_core::expression::expression::{Expression, ExpressionInput, Getter, TypedGetter};
use planetiler_core::expression::multi_expression::{Index, MultiExpression, entry};
use planetiler_core::expression::simplifiable::Simplifiable;
use planetiler_core::value::Value;

use crate::contexts::ScriptContext;
use crate::expression::ParseException;
use crate::expression::config_expression_script::ConfigExpressionScript;
use crate::expression::script_environment::ScriptEnvironment;
use crate::type_conversion::{self, OutType};

/// The input environment and output type of an expression (Java `Signature<I, O>`). `env` is `None` for the
/// "any" signature used by constants and coalesce expressions.
#[derive(Clone, Debug, PartialEq)]
pub struct Signature {
    pub env: Option<ScriptEnvironment>,
    pub out: OutType,
}

impl Signature {
    pub fn new(env: ScriptEnvironment, out: OutType) -> Signature {
        Signature { env: Some(env), out }
    }

    /// Java `ConfigExpression.any()`.
    pub fn any() -> Signature {
        Signature { env: None, out: OutType::Object }
    }

    pub fn with_output(&self, out: OutType) -> Signature {
        Signature { env: self.env.clone(), out }
    }
}

/// Java `ConfigExpression.signature(in, out)`.
pub fn signature(env: &ScriptEnvironment, out: OutType) -> Signature {
    Signature::new(env.clone(), out)
}

/// An expression that returns the value associated with the first matching boolean expression.
pub struct MatchExpression {
    pub signature: Signature,
    pub multi_expression: MultiExpression<ConfigExpression>,
    pub fallback: ConfigExpression,
    indexed: Index<ConfigExpression>,
}

impl fmt::Debug for MatchExpression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Match")
            .field("signature", &self.signature)
            .field("multi_expression", &self.multi_expression)
            .field("fallback", &self.fallback)
            .finish()
    }
}

impl PartialEq for MatchExpression {
    fn eq(&self, other: &Self) -> bool {
        // ignore the indexed expression
        self.signature == other.signature
            && self.multi_expression == other.multi_expression
            && self.fallback == other.fallback
    }
}

/// A dynamic expression from a schema config that produces a value from a [`ScriptContext`].
#[derive(Clone, Debug)]
pub enum ConfigExpression {
    /// An expression that always returns a value.
    Const(Value),
    /// An expression that returns the value associated with the first matching boolean expression.
    Match(Arc<MatchExpression>),
    /// An expression that returns the first non-null result of evaluating each child expression.
    Coalesce(Vec<ConfigExpression>),
    /// An expression that returns the value associated with a given variable name at runtime.
    Variable { sig: Signature, name: Arc<str> },
    /// An expression that returns the value associated with a given tag of the input feature at runtime.
    GetTag { sig: Signature, tag: Box<ConfigExpression> },
    /// An expression that returns the value associated with a given argument at runtime.
    GetArg { sig: Signature, arg: Box<ConfigExpression> },
    /// An expression that converts the input to a desired output [`DataType`] at runtime.
    Cast { sig: Signature, input: Box<ConfigExpression>, output: DataType },
    /// A CEL script.
    Script(Arc<ConfigExpressionScript>),
}

impl PartialEq for ConfigExpression {
    fn eq(&self, other: &Self) -> bool {
        use ConfigExpression::*;
        match (self, other) {
            (Const(a), Const(b)) => a == b,
            (Match(a), Match(b)) => Arc::ptr_eq(a, b) || a == b,
            (Coalesce(a), Coalesce(b)) => a == b,
            (Variable { sig: s1, name: n1 }, Variable { sig: s2, name: n2 }) => s1 == s2 && n1 == n2,
            (GetTag { sig: s1, tag: t1 }, GetTag { sig: s2, tag: t2 }) => s1 == s2 && t1 == t2,
            (GetArg { sig: s1, arg: a1 }, GetArg { sig: s2, arg: a2 }) => s1 == s2 && a1 == a2,
            (Cast { sig: s1, input: i1, output: o1 }, Cast { sig: s2, input: i2, output: o2 }) => {
                s1 == s2 && i1 == i2 && o1 == o2
            }
            (Script(a), Script(b)) => Arc::ptr_eq(a, b) || a == b,
            _ => false,
        }
    }
}

/// An expression that always returns `value` (Java `constOf`).
pub fn const_of(value: impl Into<Value>) -> ConfigExpression {
    ConfigExpression::Const(value.into())
}

/// An expression that returns the value of variable `name` (Java `variable`). Fails if the variable is not declared.
pub fn variable(sig: Signature, name: &str) -> Result<ConfigExpression, ParseException> {
    let declared = sig.env.as_ref().is_some_and(|env| env.contains_variable(name));
    if !declared {
        return Err(ParseException::new(format!("Variable not available: {name}")));
    }
    Ok(ConfigExpression::Variable { sig, name: Arc::from(name) })
}

/// An expression that evaluates a script (Java `ConfigExpression.script`).
pub fn script(sig: Signature, script: &str) -> Result<ConfigExpression, ParseException> {
    let env = sig.env.ok_or_else(|| ParseException::new("script requires an environment"))?;
    Ok(ConfigExpression::Script(ConfigExpressionScript::parse(script, &env, sig.out)?))
}

/// An expression that returns the first non-null value of `values`.
pub fn coalesce(values: Vec<ConfigExpression>) -> ConfigExpression {
    ConfigExpression::Coalesce(values)
}

/// An expression that returns the value of tag `tag` (Java `getTag`).
pub fn get_tag(sig: Signature, tag: ConfigExpression) -> ConfigExpression {
    ConfigExpression::GetTag { sig, tag: Box::new(tag) }
}

/// An expression that returns the value of argument `arg` (Java `getArg`).
pub fn get_arg(sig: Signature, arg: ConfigExpression) -> ConfigExpression {
    ConfigExpression::GetArg { sig, arg: Box::new(arg) }
}

/// An expression that converts the result of `input` to `data_type` (Java `cast`).
pub fn cast(sig: Signature, input: ConfigExpression, data_type: DataType) -> ConfigExpression {
    ConfigExpression::Cast { sig, input: Box::new(input), output: data_type }
}

/// An expression that returns the value associated with the first matching expression, or `fallback`.
pub fn match_with_fallback(
    sig: Signature,
    multi_expression: MultiExpression<ConfigExpression>,
    fallback: ConfigExpression,
) -> ConfigExpression {
    let indexed = multi_expression.index();
    ConfigExpression::Match(Arc::new(MatchExpression { signature: sig, multi_expression, fallback, indexed }))
}

/// An expression that returns the value associated with the first matching expression, or null.
pub fn match_expr(sig: Signature, multi_expression: MultiExpression<ConfigExpression>) -> ConfigExpression {
    match_with_fallback(sig, multi_expression, ConfigExpression::Const(Value::Null))
}

fn convert(value: Value, out: OutType) -> Value {
    type_conversion::convert_or_null(value, out)
}

impl ConfigExpression {
    /// The environment this expression's input must be from, if any.
    pub fn environment(&self) -> Option<&ScriptEnvironment> {
        match self {
            ConfigExpression::Const(_) | ConfigExpression::Coalesce(_) => None,
            ConfigExpression::Match(m) => m.signature.env.as_ref(),
            ConfigExpression::Variable { sig, .. }
            | ConfigExpression::GetTag { sig, .. }
            | ConfigExpression::GetArg { sig, .. }
            | ConfigExpression::Cast { sig, .. } => sig.env.as_ref(),
            ConfigExpression::Script(s) => Some(s.environment()),
        }
    }

    /// Returns true if this always evaluates to null.
    pub fn is_null_const(&self) -> bool {
        matches!(self, ConfigExpression::Const(Value::Null))
    }

    /// Evaluates this expression on `input`.
    pub fn apply(&self, input: &ScriptContext<'_>) -> Value {
        match self {
            ConfigExpression::Const(v) => v.clone(),
            ConfigExpression::Match(m) => m.indexed.get_or_else(input, &m.fallback).apply(input),
            ConfigExpression::Coalesce(children) => {
                for child in children {
                    let result = child.apply(input);
                    if !result.is_null() {
                        return result;
                    }
                }
                Value::Null
            }
            ConfigExpression::Variable { sig, name } => convert(input.variable(name), sig.out),
            ConfigExpression::GetTag { sig, tag } => {
                let key = tag.apply(input);
                if key.is_null() {
                    return Value::Null;
                }
                let key = key.to_string_lossy();
                convert(input.tag_value_producer().value_for_key(input, &key), sig.out)
            }
            ConfigExpression::GetArg { sig, arg } => {
                let key = arg.apply(input);
                match input.argument(&key.to_string_lossy()) {
                    Some(v) => convert(v.clone(), sig.out),
                    None => Value::Null,
                }
            }
            ConfigExpression::Cast { sig, input: child, output } => {
                convert(output.convert_from(Some(&child.apply(input))), sig.out)
            }
            ConfigExpression::Script(script) => script.apply(input),
        }
    }

    /// Wraps this expression as a [`Getter`] for [`planetiler_core::expression::expression::match_any_typed`].
    pub fn into_getter(self) -> Getter {
        Getter::Custom(Arc::new(ConfigExpressionGetter(self)))
    }

    /// Returns a copy of a match expression with a new fallback (Java `Match.withDefaultValue`).
    pub fn with_default_value(&self, new_fallback: ConfigExpression) -> ConfigExpression {
        match self {
            ConfigExpression::Match(m) => {
                match_with_fallback(m.signature.clone(), m.multi_expression.clone(), new_fallback)
            }
            other => other.clone(),
        }
    }
}

impl Simplifiable for ConfigExpression {
    fn simplify_once(&self) -> ConfigExpression {
        match self {
            ConfigExpression::Match(m) => {
                let new_multi = m.multi_expression.map_results(|r| Some(r.simplify_once())).simplify();
                let new_fallback = m.fallback.simplify_once();
                if new_multi.expressions.is_empty() {
                    return new_fallback;
                }
                for (i, e) in new_multi.expressions.iter().enumerate() {
                    // if one of the cases is always true, then ignore the cases after it and make this value the
                    // fallback
                    if e.expression == Expression::TRUE {
                        return match_with_fallback(
                            m.signature.clone(),
                            MultiExpression::of_ordered(new_multi.expressions[..i].to_vec()),
                            e.result.clone(),
                        );
                    }
                }
                match_with_fallback(m.signature.clone(), new_multi, new_fallback)
            }
            ConfigExpression::Coalesce(children) => match children.len() {
                0 => ConfigExpression::Const(Value::Null),
                1 => children[0].clone(),
                _ => {
                    let mut result: Vec<ConfigExpression> = Vec::new();
                    for child in children {
                        let flattened: Vec<&ConfigExpression> = match child {
                            ConfigExpression::Coalesce(grandchildren) => grandchildren.iter().collect(),
                            other => vec![other],
                        };
                        for c in flattened {
                            if !c.is_null_const() && !result.contains(c) {
                                result.push(c.clone());
                            }
                        }
                    }
                    if let Some(first_const) = result.iter().position(|d| matches!(d, ConfigExpression::Const(_))) {
                        result.truncate(first_const + 1);
                    }
                    ConfigExpression::Coalesce(result)
                }
            },
            ConfigExpression::GetTag { sig, tag } => get_tag(sig.clone(), tag.simplify_once()),
            ConfigExpression::GetArg { sig, arg } => {
                let key = arg.simplify_once();
                if let (ConfigExpression::Const(k), Some(env)) = (&key, &sig.env) {
                    let raw = env.root().argument(&k.to_string_lossy()).cloned().unwrap_or_default();
                    if let Ok(converted) = type_conversion::convert(raw, sig.out) {
                        return ConfigExpression::Const(converted);
                    }
                }
                get_arg(sig.clone(), key)
            }
            ConfigExpression::Cast { sig, input, output } => {
                let simplified = input.simplify_once();
                match &simplified {
                    ConfigExpression::Const(v) => {
                        match type_conversion::convert(output.convert_from(Some(v)), sig.out) {
                            Ok(converted) => ConfigExpression::Const(converted),
                            Err(_) => cast(sig.clone(), simplified, *output),
                        }
                    }
                    ConfigExpression::Cast { input: inner, output: inner_output, .. } if inner_output == output => {
                        cast(sig.clone(), (**inner).clone(), *output)
                    }
                    _ => cast(sig.clone(), simplified, *output),
                }
            }
            ConfigExpression::Script(script) => script.simplify_once(),
            ConfigExpression::Const(_) | ConfigExpression::Variable { .. } => self.clone(),
        }
    }
}

/// A [`ConfigExpression`] used as a [`TypedGetter`] in a [`planetiler_core::expression::expression::MatchAny`]
/// expression: evaluates the expression when the input is a [`ScriptContext`] of the right kind (Java
/// `ConfigExpression.apply(WithTags, String)`).
#[derive(Debug, PartialEq)]
pub struct ConfigExpressionGetter(pub ConfigExpression);

impl TypedGetter for ConfigExpressionGetter {
    fn apply(&self, input: &dyn ExpressionInput, _key: Option<&str>) -> Value {
        match ScriptContext::from_input(input) {
            Some(ctx) => match self.0.environment() {
                Some(env) if env.kind() != ctx.kind() => Value::Null,
                _ => self.0.apply(ctx),
            },
            None => Value::Null,
        }
    }

    fn simplify_once(&self) -> Option<Arc<dyn TypedGetter>> {
        let simplified = self.0.simplify_once();
        if simplified == self.0 { None } else { Some(Arc::new(ConfigExpressionGetter(simplified))) }
    }

    fn dyn_eq(&self, other: &dyn TypedGetter) -> bool {
        other.as_any().downcast_ref::<ConfigExpressionGetter>().is_some_and(|o| o == self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Builds a multi-expression entry (re-exported for convenience).
pub fn match_entry(
    result: ConfigExpression,
    expression: Expression,
) -> planetiler_core::expression::multi_expression::Entry<ConfigExpression> {
    entry(result, expression)
}
