//! Port of `BooleanExpressionParser.java`: parses user-defined YAML into boolean [`Expression`]s that can be
//! evaluated against an input feature.
//!
//! - `key: value` / `key: [values]` match tag values (with `%` wildcards, `""` or `null` to match missing tags, and
//!   `__any__` to match any value),
//! - `__any__: {...}` ors, `__all__: {...}` ands, and `__not__: {...}` negates its children,
//! - `"${ script }"` evaluates a CEL boolean expression, and `"${ script }": [values]` matches the result of a
//!   script against values.

use std::sync::LazyLock;

use planetiler_core::expression::expression::{
    Expression, and, match_any_typed, match_field, match_source, match_source_layer, not, or,
};
use planetiler_core::value::Value;
use regex::Regex;

use crate::contexts::{FEATURE_SOURCE, FEATURE_SOURCE_LAYER};
use crate::expression::config_expression::ConfigExpression;
use crate::expression::config_expression_script::{self, ConfigExpressionScript};
use crate::expression::script_environment::ScriptEnvironment;
use crate::expression::{BooleanExpressionScript, ParseException};
use crate::tag_value_producer::TagValueProducer;
use crate::type_conversion::OutType;
use planetiler_core::expression::simplifiable::Simplifiable;

static ESCAPED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^([\s\\]*)\\(__any__|__all__)$").unwrap());
static IS_ANY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*__any__\s*$").unwrap());
static IS_ALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*__all__\s*$").unwrap());
static IS_NOT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*__not__\s*$").unwrap());

/// Returns a boolean expression that determines whether a source feature matches a criteria defined in yaml config.
///
/// `object` is a map or list of tag criteria (or `None`, which never matches), `tag_value_producer` determines how tag
/// values are parsed, and `context` determines which variables are available to scripts.
pub fn parse(
    object: Option<&Value>,
    tag_value_producer: &TagValueProducer,
    context: &ScriptEnvironment,
) -> Result<Expression, ParseException> {
    Parser { tag_value_producer, context }.parse(object, Collector::Or)
}

#[derive(Clone, Copy)]
enum Collector {
    Or,
    And,
}

impl Collector {
    fn apply(self, children: Vec<Expression>) -> Expression {
        match self {
            Collector::Or => or(children),
            Collector::And => and(children),
        }
    }
}

fn is_list_or_map(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Map(_) | Value::List(_)))
}

fn unescape_str(s: &str) -> String {
    match ESCAPED.captures(s) {
        Some(c) => format!("{}{}", &c[1], &c[2]),
        None => s.to_string(),
    }
}

fn unescape(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::str(unescape_str(s)),
        other => other.clone(),
    }
}

struct Parser<'a> {
    tag_value_producer: &'a TagValueProducer,
    context: &'a ScriptEnvironment,
}

impl Parser<'_> {
    fn parse(&self, object: Option<&Value>, collector: Collector) -> Result<Expression, ParseException> {
        let Some(object) = object.filter(|o| !o.is_null()) else {
            return Ok(Expression::FALSE);
        };
        if let Value::String(s) = object
            && s.trim().eq_ignore_ascii_case("__any__")
        {
            return Ok(Expression::TRUE);
        }
        if let Some(script) = config_expression_script::extract_script(object) {
            return BooleanExpressionScript::script(script, self.context);
        }
        match object {
            Value::Map(map) => Ok(collector.apply(
                map.iter().map(|(k, v)| self.tag_criterion_to_expression(k, v)).collect::<Result<Vec<_>, _>>()?,
            )),
            Value::List(list) => Ok(collector
                .apply(list.iter().map(|item| self.parse(Some(item), Collector::Or)).collect::<Result<Vec<_>, _>>()?)),
            other => Err(ParseException::new(format!("Unsupported object for matcher input: {other}"))),
        }
    }

    fn tag_criterion_to_expression(&self, key: &str, value: &Value) -> Result<Expression, ParseException> {
        let value_opt = Some(value).filter(|v| !v.is_null());
        if IS_ANY.is_match(key) && is_list_or_map(value_opt) {
            // __any__ ors together its children
            return self.parse(value_opt, Collector::Or);
        }
        if IS_ALL.is_match(key) && is_list_or_map(value_opt) {
            // __all__ ands together its children
            return self.parse(value_opt, Collector::And);
        }
        if IS_NOT.is_match(key) {
            // __not__ negates its children
            return Ok(not(self.parse(value_opt, Collector::Or)?));
        }
        // If only a key is provided, with no value, match any object tagged with that key.
        let is_any = match value {
            Value::Null => true,
            Value::List(values) => values.iter().any(|d| !d.is_null() && IS_ANY.is_match(d.to_string().trim())),
            other => IS_ANY.is_match(&other.to_string()),
        };
        // If a collection or single item are provided, match any of these values.
        let mut values: Vec<Value> = match value {
            Value::List(items) => items.iter().map(unescape).collect(),
            Value::Null => Vec::new(),
            other => vec![unescape(other)],
        };
        let key_value = Value::str(key);
        if let Some(script) = config_expression_script::extract_script(&key_value) {
            let expression =
                ConfigExpression::Script(ConfigExpressionScript::parse(script, self.context, OutType::Object)?)
                    .simplify();
            if is_any {
                values = Vec::new();
            }
            let variable_name = match &expression {
                ConfigExpression::Variable { name, .. } => Some(name.clone()),
                _ => None,
            };
            let result = match_any_typed(None, expression.into_getter(), values.clone());
            if let (Expression::MatchAny(any), Some(name)) = (&result, variable_name)
                && !values.is_empty()
                && any.pattern.is_none()
                && !any.is_match_anything()
                && !any.match_when_missing
            {
                let strings = values.iter().filter_map(Value::as_str);
                if &*name == FEATURE_SOURCE {
                    return Ok(or(strings.map(match_source)));
                } else if &*name == FEATURE_SOURCE_LAYER {
                    return Ok(or(strings.map(match_source_layer)));
                }
            }
            return Ok(result);
        }
        let field = unescape_str(key);
        if is_any {
            Ok(match_field(&field))
        } else {
            Ok(match_any_typed(Some(&field), self.tag_value_producer.value_getter_for_key(key), values))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contexts::Root;
    use planetiler_core::expression::expression::match_any;
    use planetiler_core::util::yaml;

    fn assert_parse(yaml: &str, parsed: Expression) {
        let expression = yaml::load_value(yaml).unwrap();
        let root = Root::empty();
        let actual = parse(Some(&expression), TagValueProducer::empty(), &root.description()).unwrap();
        assert_eq!(parsed.simplify().generate_java_code(), actual.simplify().generate_java_code());
        assert_eq!(parsed.simplify(), actual.simplify());
    }

    #[test]
    fn test_empty() {
        assert_parse("", Expression::FALSE);
    }

    #[test]
    fn test_single_value() {
        assert_parse("a: b", match_any("a", ["b"]));
    }

    #[test]
    fn test_multivalue() {
        assert_parse("a:\n- b\n- c\n", or([match_any("a", ["b", "c"])]));
    }

    #[test]
    fn test_multi_key() {
        assert_parse("a: b\nc: [d, e]\n", or([match_any("a", ["b"]), match_any("c", ["d", "e"])]));
    }

    #[test]
    fn test_any_value() {
        assert_parse("a: __any__", match_field("a"));
        assert_parse("a: __ANY__", match_field("a"));
        assert_parse("a: [b, __any__]", match_field("a"));
    }

    #[test]
    fn test_escape_any() {
        assert_parse("a: \\__any__\nb: [\\__any__]\n", or([match_any("a", ["__any__"]), match_any("b", ["__any__"])]));
        assert_parse("a: \\__ANY__", match_any("a", ["__ANY__"]));
        assert_parse("a: \\\\__any__", match_any("a", ["\\__any__"]));
        assert_parse("a: \\\\__ANY__", match_any("a", ["\\__ANY__"]));
    }

    #[test]
    fn test_match_anything() {
        assert_parse("__any__", Expression::TRUE);
    }

    #[test]
    fn test_any_wrapper() {
        assert_parse("__any__:\n  a: b\n  c: d\n", or([match_any("a", ["b"]), match_any("c", ["d"])]));
    }

    #[test]
    fn test_all_wrapper() {
        assert_parse("__all__:\n  a: b\n  c: d\n", and([match_any("a", ["b"]), match_any("c", ["d"])]));
    }

    #[test]
    fn test_nested_not() {
        assert_parse(
            "__all__:\n  a: b\n  __not__:\n    c: d\n",
            and([match_any("a", ["b"]), not(match_any("c", ["d"]))]),
        );
    }

    #[test]
    fn test_nested_and() {
        assert_parse(
            "a: b\n__ALL__:\n  c: d\n  e: f\n",
            or([match_any("a", ["b"]), and([match_any("c", ["d"]), match_any("e", ["f"])])]),
        );
    }

    #[test]
    fn test_nested_and_or_not() {
        assert_parse(
            "a: b\n__ALL__:\n  c: d\n  __NOT__:\n    e: f\n    g: h\n",
            or([
                match_any("a", ["b"]),
                and([match_any("c", ["d"]), not(or([match_any("e", ["f"]), match_any("g", ["h"])]))]),
            ]),
        );
    }

    #[test]
    fn test_actual_any_all_unescaped() {
        assert_parse(
            "a: b\n__any__: d\n__all__: d\n",
            or([match_any("a", ["b"]), match_any("__any__", ["d"]), match_any("__all__", ["d"])]),
        );
    }

    #[test]
    fn test_actual_any_all() {
        assert_parse(
            "a: b\n\\__any__: d\n\\__all__: d\n",
            or([match_any("a", ["b"]), match_any("__any__", ["d"]), match_any("__all__", ["d"])]),
        );
    }

    #[test]
    fn test_actual_any_all_list() {
        assert_parse(
            "a: b\n\\__any__: [d1, d2]\n\\__all__: [e1, e2]\n",
            or([match_any("a", ["b"]), match_any("__any__", ["d1", "d2"]), match_any("__all__", ["e1", "e2"])]),
        );
    }

    #[test]
    fn test_list() {
        assert_parse(
            "__all__:\n  - a: b\n  - __any__:\n    - c: d\n    - e: f\n  - __any__:\n    - g: h\n    - j: i\n",
            and([
                match_any("a", ["b"]),
                or([match_any("c", ["d"]), match_any("e", ["f"])]),
                or([match_any("g", ["h"]), match_any("j", ["i"])]),
            ]),
        );
    }
}
