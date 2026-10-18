//! Port of `ConfigExpressionParser.java`: parses user-defined YAML into a [`ConfigExpression`] that can be evaluated
//! against an input feature.
//!
//! Supported structures:
//! - constants (strings, numbers, booleans; `"\${...}"` escapes a literal `${...}`),
//! - `"${ script }"`: a CEL script,
//! - `{coalesce: [a, b, ...]}`: the first non-null value,
//! - `{match: [...]}` or a list: `{if: <boolean expression>, value: <expression>}` cases with an optional
//!   `{else: <expression>}`,
//! - `{match: {value: <boolean expression>, ..., fallback: default_value}}`,
//! - `{default_value: <expression>, overrides: <match cases>}`,
//! - `{tag_value: <expression>}`, `{arg_value: <expression>}`, `{value: <expression>}`,
//! - any of the above with `type: <data type>` to convert the result.

use std::sync::LazyLock;

use planetiler_core::expression::data_type::DataType;
use planetiler_core::expression::multi_expression::{Entry, MultiExpression, entry};
use planetiler_core::expression::simplifiable::Simplifiable;
use planetiler_core::value::{Tags, Value};
use regex::Regex;

use crate::boolean_expression_parser;
use crate::contexts::Root;
use crate::expression::config_expression::{
    self, ConfigExpression, Signature, cast, coalesce, get_arg, get_tag, match_with_fallback,
};
use crate::expression::config_expression_script::{self, ConfigExpressionScript};
use crate::expression::script_environment::ScriptEnvironment;
use crate::expression::{EvaluationException, ParseException};
use crate::tag_value_producer::TagValueProducer;
use crate::type_conversion::{self, OutType};

static DEFAULT_VALUE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^_*(default_value|otherwise|default)_*$").unwrap());

/// Returns an expression parsed from user-defined YAML that can be evaluated against an input from `context` and
/// returns output of type `output`. The result is simplified.
pub fn parse(
    object: Option<&Value>,
    tag_value_producer: &TagValueProducer,
    context: &ScriptEnvironment,
    output: OutType,
) -> Result<ConfigExpression, ParseException> {
    Ok(Parser { tag_value_producer, input: context }.parse(object, output)?.simplify())
}

/// Error from [`try_static_evaluate`].
#[derive(Debug, Clone, thiserror::Error)]
pub enum StaticEvaluationError {
    #[error(transparent)]
    Parse(#[from] ParseException),
    #[error(transparent)]
    Evaluation(#[from] EvaluationException),
    #[error("expression is not static: {0}")]
    NotStatic(String),
}

/// Attempts to evaluate `expression` from a yaml config, using only globally-available environmental variables from
/// the `root` context.
pub fn try_static_evaluate(
    root: &Root,
    expression: Option<&Value>,
    result_type: OutType,
) -> Result<Value, StaticEvaluationError> {
    let Some(expression) = expression.filter(|e| !e.is_null()) else {
        return Ok(Value::Null);
    };
    let parsed = parse(Some(expression), TagValueProducer::empty(), &root.description(), result_type)?;
    let context = root.as_context();
    match &parsed {
        ConfigExpression::Script(script) => Ok(script.try_apply(&context)?),
        _ => Ok(parsed.apply(&context)),
    }
}

struct Parser<'a> {
    tag_value_producer: &'a TagValueProducer,
    input: &'a ScriptEnvironment,
}

fn is_keys(map: &Tags, expected: &[&str]) -> bool {
    map.len() == expected.len() && expected.iter().all(|k| map.contains_key(*k))
}

impl Parser<'_> {
    fn signature(&self, output: OutType) -> Signature {
        Signature::new(self.input.clone(), output)
    }

    fn parse(&self, object: Option<&Value>, output: OutType) -> Result<ConfigExpression, ParseException> {
        let Some(object) = object.filter(|o| !o.is_null()) else {
            return Ok(ConfigExpression::Const(Value::Null));
        };
        if let Some(script) = config_expression_script::extract_script(object) {
            return Ok(ConfigExpression::Script(ConfigExpressionScript::parse(script, self.input, output)?));
        }
        match object {
            Value::List(_) => self.parse_match(object, true, output),
            Value::Map(map) => {
                if let Some(type_value) = map.get("type").filter(|t| !t.is_null()) {
                    let type_name = type_value.to_string();
                    let data_type = DataType::from_id(&type_name);
                    if data_type.id() != type_name {
                        let supported: Vec<&str> = DataType::ALL.iter().map(DataType::id).collect();
                        return Err(ParseException::new(format!(
                            "Unrecognized datatype '{type_name}' supported values: {}",
                            supported.join(", ")
                        )));
                    }
                    let mut map2 = (**map).clone();
                    map2.shift_remove("type");
                    let child = self.parse(Some(&Value::map(map2)), OutType::Object)?;
                    return Ok(cast(self.signature(output), child, data_type));
                }
                if is_keys(map, &["coalesce"])
                    && let Some(Value::List(cases)) = map.get("coalesce")
                {
                    return Ok(coalesce(
                        cases.iter().map(|item| self.parse(Some(item), output)).collect::<Result<Vec<_>, _>>()?,
                    ));
                }
                if is_keys(map, &["match"]) {
                    return self.parse_match(&map["match"], true, output);
                }
                if is_keys(map, &["default_value", "overrides"]) {
                    let matched = self.parse_match(&map["overrides"], false, output)?;
                    let default_value = self.parse(map.get("default_value"), output)?;
                    return Ok(matched.with_default_value(default_value));
                }
                if is_keys(map, &["tag_value"]) {
                    let tag = self.parse(map.get("tag_value"), OutType::String)?;
                    return Ok(get_tag(self.signature(output), tag));
                }
                if is_keys(map, &["arg_value"]) {
                    let arg = self.parse(map.get("arg_value"), OutType::String)?;
                    return Ok(get_arg(self.signature(output), arg));
                }
                if is_keys(map, &["value"]) {
                    return self.parse(map.get("value"), output);
                }
                self.parse_match(object, true, output)
                    .map_err(|_| ParseException::new(format!("Failed to parse: {object}")))
            }
            other => {
                let unescaped = config_expression_script::unescape(other.clone());
                type_conversion::convert(unescaped, output)
                    .map(ConfigExpression::Const)
                    .map_err(|e| ParseException::new(e.to_string()))
            }
        }
    }

    fn parse_match(
        &self,
        match_value: &Value,
        allow_else: bool,
        output: OutType,
    ) -> Result<ConfigExpression, ParseException> {
        let mut conditions: Vec<Entry<ConfigExpression>> = Vec::new();
        let mut fallback = ConfigExpression::Const(Value::Null);
        match match_value {
            Value::List(items) => {
                for item in items.iter() {
                    if let Value::Map(map) = item {
                        if is_keys(map, &["if", "value"]) {
                            conditions.push(entry(
                                self.parse(map.get("value"), output)?,
                                boolean_expression_parser::parse(map.get("if"), self.tag_value_producer, self.input)?,
                            ));
                        } else if allow_else && is_keys(map, &["else"]) {
                            fallback = self.parse(map.get("else"), output)?;
                            break;
                        } else {
                            return Err(ParseException::new(format!(
                                "Invalid match case. Expected if/then{}, got: {match_value}",
                                if allow_else { " or else" } else { "" }
                            )));
                        }
                    }
                }
            }
            Value::Map(map) => {
                for (key, value) in map.iter() {
                    let key_value = Value::str(key);
                    if DEFAULT_VALUE.is_match(&value.to_string()) {
                        fallback = self.parse(Some(&key_value), output)?;
                    } else {
                        conditions.push(entry(
                            self.parse(Some(&key_value), output)?,
                            boolean_expression_parser::parse(Some(value), self.tag_value_producer, self.input)?,
                        ));
                    }
                }
            }
            other => {
                return Err(ParseException::new(format!(
                    "Invalid match block. Expected a list or map, but got: {other}"
                )));
            }
        }
        Ok(match_with_fallback(self.signature(output), MultiExpression::of_ordered(conditions), fallback))
    }
}

/// Re-exports the expression constructors used by tests and callers building expressions by hand.
pub use config_expression::{const_of, match_expr, script, signature, variable};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expression::config_expression::match_entry;
    use planetiler_core::expression::expression::match_any;
    use planetiler_core::util::yaml;

    fn feature_signature() -> Signature {
        signature(&Root::empty().process_feature_description(), OutType::Object)
    }

    fn assert_parse(yaml_str: &str, parsed: ConfigExpression, out: OutType) {
        let expression = yaml::load_value(yaml_str).unwrap();
        let sig = feature_signature();
        let actual = parse(Some(&expression), TagValueProducer::empty(), sig.env.as_ref().unwrap(), out).unwrap();
        assert_eq!(parsed.simplify(), actual.simplify());
    }

    #[test]
    fn test_empty() {
        assert_parse("", const_of(Value::Null), OutType::String);
    }

    #[test]
    fn test_const() {
        for input in ["1", "'1'"] {
            assert_parse(input, const_of(1), OutType::Integer);
            assert_parse(input, const_of(1), OutType::Long);
            assert_parse(input, const_of(1.0), OutType::Double);
        }
    }

    #[test]
    fn test_var() {
        for var in ["feature.id", "feature.source", "feature.source_layer", "feature.osm_user_name"] {
            let script = format!("${{{var}}}");
            for out in [OutType::Integer, OutType::Long, OutType::Double, OutType::String] {
                assert_parse(&script, variable(feature_signature().with_output(out), var).unwrap(), out);
            }
        }
    }

    #[test]
    fn test_static_expression() {
        assert_parse("${1+2}", const_of(3), OutType::Integer);
        assert_parse("${1+2}", const_of(3), OutType::Long);
    }

    #[test]
    fn test_dynamic_expression() {
        assert_parse("${feature.tags.a}", script(feature_signature(), "feature.tags.a").unwrap(), OutType::Object);
    }

    #[test]
    fn test_coalesce_static() {
        assert_parse("coalesce:\n- 1\n- 2\n", const_of(1), OutType::Integer);
    }

    #[test]
    fn test_coalesce_dynamic() {
        assert_parse(
            "coalesce:\n- ${feature.tags.get('a')}\n- ${feature.tags.get('b')}\n",
            coalesce(vec![
                script(feature_signature(), "feature.tags.get('a')").unwrap(),
                script(feature_signature(), "feature.tags.get('b')").unwrap(),
            ]),
            OutType::Object,
        );
    }

    fn water_lake() -> ConfigExpression {
        match_with_fallback(
            feature_signature().with_output(OutType::Integer),
            MultiExpression::of(vec![
                match_entry(const_of(1), match_any("natural", ["water"])),
                match_entry(const_of(2), match_any("natural", ["lake"])),
            ]),
            const_of(3),
        )
    }

    #[test]
    fn test_match() {
        assert_parse(
            "match:\n- if:\n    natural: water\n  value: 1\n- if:\n    natural: lake\n  value: 2\n- else: 3\n",
            water_lake(),
            OutType::Integer,
        );
    }

    #[test]
    fn test_match_map() {
        assert_parse(
            "match:\n  1:\n    natural: water\n  2:\n    natural: lake\n  3: default_value\n",
            water_lake(),
            OutType::Integer,
        );
    }

    #[test]
    fn test_overrides() {
        assert_parse(
            "default_value: 3\noverrides:\n- if: {natural: water}\n  value: 1\n- if: {natural: lake}\n  value: 2\n",
            water_lake(),
            OutType::Integer,
        );
    }

    #[test]
    fn test_cast() {
        assert_parse(
            "tag_value: abc\ntype: integer\n",
            cast(
                feature_signature().with_output(OutType::Integer),
                get_tag(feature_signature().with_output(OutType::Object), const_of("abc")),
                DataType::GetInt,
            ),
            OutType::Integer,
        );
    }

    #[test]
    fn test_coalesce_with_type() {
        assert_parse("type: double\ncoalesce:\n- '1'\n- '2'\n", const_of(1.0), OutType::Double);
    }

    #[test]
    fn test_cast_a_value() {
        assert_parse(
            "type: double\nvalue: '${feature.tags.a}'\n",
            cast(
                feature_signature().with_output(OutType::Double),
                script(feature_signature().with_output(OutType::Object), "feature.tags.a").unwrap(),
                DataType::GetDouble,
            ),
            OutType::Double,
        );
    }
}
