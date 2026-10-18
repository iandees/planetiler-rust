//! Port of `BooleanExpressionScript.java`: a boolean [`Expression`] based off of a dynamic expression script parsed
//! from a string.

use std::any::Any;
use std::sync::Arc;

use planetiler_core::expression::expression::{CustomExpression, Expression, ExpressionInput, MatchKeys};
use planetiler_core::value::Value;

use crate::contexts::ScriptContext;
use crate::expression::ParseException;
use crate::expression::config_expression_script::ConfigExpressionScript;
use crate::expression::script_environment::ScriptEnvironment;
use crate::type_conversion::OutType;

/// A boolean expression evaluating a CEL script against a [`ScriptContext`] of the environment's kind.
#[derive(Debug)]
pub struct BooleanExpressionScript {
    expression_text: String,
    expression: Arc<ConfigExpressionScript>,
}

impl BooleanExpressionScript {
    /// Creates a new boolean expression from `script` where `environment` defines the available variables.
    pub fn script(script: &str, environment: &ScriptEnvironment) -> Result<Expression, ParseException> {
        let parsed = ConfigExpressionScript::parse(script, environment, OutType::Boolean)?;
        Ok(Expression::Custom(Arc::new(BooleanExpressionScript {
            expression_text: script.to_string(),
            expression: parsed,
        })))
    }

    pub fn expression_text(&self) -> &str {
        &self.expression_text
    }
}

impl CustomExpression for BooleanExpressionScript {
    fn evaluate(&self, input: &dyn ExpressionInput, _match_keys: Option<&mut MatchKeys>) -> bool {
        match ScriptContext::from_input(input) {
            Some(ctx) if ctx.kind() == self.expression.environment().kind() => {
                matches!(self.expression.apply(ctx), Value::Bool(true))
            }
            _ => false,
        }
    }

    fn generate_java_code(&self) -> Option<String> {
        Some(format!("script({})", java_quote(&format!("${{ {} }}", self.expression_text))))
    }

    fn simplify_once(&self) -> Option<Expression> {
        self.expression
            .try_static_evaluate()
            .map(|v| if matches!(v, Value::Bool(true)) { Expression::TRUE } else { Expression::FALSE })
    }

    fn dyn_eq(&self, other: &dyn CustomExpression) -> bool {
        other.as_any().downcast_ref::<BooleanExpressionScript>().is_some_and(|o| {
            o.expression_text == self.expression_text
                && o.expression.environment().kind() == self.expression.environment().kind()
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn java_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
