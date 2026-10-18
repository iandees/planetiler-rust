//! Port of `ConfigExpressionScript.java`: an expression that returns the result of evaluating a user-defined CEL
//! script on the input context.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

use cel::common::ast::IdedExpr;
use planetiler_core::value::Value;
use regex::Regex;
use tracing::{debug, warn};

use crate::contexts::{ContextKind, ScriptContext};
use crate::expression::config_expression::{ConfigExpression, Signature};
use crate::expression::script_environment::ScriptEnvironment;
use crate::expression::stdlib;
use crate::expression::{EvaluationException, ParseException};
use crate::type_conversion::{self, OutType};

static EXPRESSION_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\$\{(.*)}\s*$").unwrap());
static ESCAPED_EXPRESSION_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\\+\$\{(.*)}\s*$").unwrap());
static ESCAPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\\$").unwrap());

/// Returns true if this is a string expression like `"${ ... }"`.
pub fn is_script(obj: &Value) -> bool {
    obj.as_str().is_some_and(|s| EXPRESSION_PATTERN.is_match(s))
}

/// Returns true if this is an escaped string expression that should just be treated as a string like `"\${ ... }"`.
pub fn is_escaped_script(obj: &Value) -> bool {
    obj.as_str().is_some_and(|s| ESCAPED_EXPRESSION_PATTERN.is_match(s))
}

/// Removes the script escape character from a string: `"\${ ... }"` becomes `"${ ... }"`.
pub fn unescape(obj: Value) -> Value {
    match obj.as_str() {
        Some(s) if ESCAPED_EXPRESSION_PATTERN.is_match(s) => Value::str(ESCAPE.replace(s, "$$")),
        _ => obj,
    }
}

/// Returns the script text between the `"${ ... }"` characters.
pub fn extract_script(obj: &Value) -> Option<&str> {
    let s = obj.as_str()?;
    EXPRESSION_PATTERN.captures(s).and_then(|c| c.get(1)).map(|m| m.as_str())
}

/// A compiled CEL script with an output type, evaluated in an environment.
pub struct ConfigExpressionScript {
    script_text: String,
    expr: IdedExpr,
    environment: ScriptEnvironment,
    return_type: OutType,
    /// Result of evaluating this script with only root variables, cached between calls (`None` if not static).
    static_value: OnceLock<Option<Value>>,
    warned: AtomicBool,
}

impl fmt::Debug for ConfigExpressionScript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ConfigExpression[returnType={:?}, scriptText='{}']", self.return_type, self.script_text)
    }
}

impl PartialEq for ConfigExpressionScript {
    fn eq(&self, other: &Self) -> bool {
        // ignore the parsed script object
        self.return_type == other.return_type
            && self.script_text == other.script_text
            && self.environment == other.environment
    }
}

impl ConfigExpressionScript {
    /// Returns an expression parsed from a user-supplied script string that coerces the result to `expected`.
    ///
    /// Fails if the script fails to compile or references variables that `environment` does not declare.
    pub fn parse(
        string: &str,
        environment: &ScriptEnvironment,
        expected: OutType,
    ) -> Result<Arc<ConfigExpressionScript>, ParseException> {
        let expr = stdlib::compile(string).map_err(|e| ParseException::parsing(string, e))?;
        stdlib::check_references(&expr, environment)
            .map_err(|name| ParseException::parsing(string, format!("undeclared reference to '{name}'")))?;
        Ok(Arc::new(ConfigExpressionScript {
            script_text: string.to_string(),
            expr,
            environment: environment.clone(),
            return_type: expected,
            static_value: OnceLock::new(),
            warned: AtomicBool::new(false),
        }))
    }

    pub fn script_text(&self) -> &str {
        &self.script_text
    }

    pub fn environment(&self) -> &ScriptEnvironment {
        &self.environment
    }

    pub fn return_type(&self) -> OutType {
        self.return_type
    }

    /// Returns a copy of this script with a different return type.
    pub fn with_return_type(&self, return_type: OutType) -> Arc<ConfigExpressionScript> {
        Arc::new(ConfigExpressionScript {
            script_text: self.script_text.clone(),
            expr: self.expr.clone(),
            environment: self.environment.clone(),
            return_type,
            static_value: OnceLock::new(),
            warned: AtomicBool::new(false),
        })
    }

    /// Evaluates this script, returning an error if it fails or the result can't be converted to the return type.
    pub fn try_apply(&self, input: &ScriptContext<'_>) -> Result<Value, EvaluationException> {
        let err = |message: String| EvaluationException { script: self.script_text.clone(), message };
        let result = stdlib::evaluate(&self.expr, input).map_err(|e| err(e.to_string()))?;
        type_conversion::convert(result, self.return_type).map_err(|e| err(e.to_string()))
    }

    /// Evaluates this script, logging and returning [`Value::Null`] on errors (Java throws an
    /// `EvaluationException`).
    pub fn apply(&self, input: &ScriptContext<'_>) -> Value {
        match self.try_apply(input) {
            Ok(v) => v,
            Err(e) => {
                if !self.warned.swap(true, Ordering::Relaxed) {
                    warn!("{e} (further errors for this script are logged at debug level)");
                } else {
                    debug!("{e}");
                }
                Value::Null
            }
        }
    }

    /// Attempts to evaluate this script in an environment with only root variables (arguments).
    ///
    /// If this returns `Some` then it means this script will always return the same constant value and we can avoid
    /// evaluating it at runtime.
    pub fn try_static_evaluate(&self) -> Option<Value> {
        self.static_value
            .get_or_init(|| {
                let root = self.environment.root();
                let root_env = self.environment.for_input(ContextKind::Root);
                stdlib::check_references(&self.expr, &root_env).ok()?;
                let result = stdlib::evaluate(&self.expr, &root.as_context()).ok()?;
                type_conversion::convert(result, self.return_type).ok()
            })
            .clone()
    }

    /// Simplification rules: constant scripts become [`ConfigExpression::Const`], scripts that only reference a
    /// variable become [`ConfigExpression::Variable`].
    pub fn simplify_once(self: &Arc<Self>) -> ConfigExpression {
        if let Some(result) = self.try_static_evaluate() {
            ConfigExpression::Const(result)
        } else if self.environment.contains_variable(self.script_text.trim()) {
            ConfigExpression::Variable {
                sig: Signature::new(self.environment.clone(), self.return_type),
                name: Arc::from(self.script_text.trim()),
            }
        } else {
            ConfigExpression::Script(self.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        assert!(is_script(&Value::str("${a}")));
        assert!(is_script(&Value::str("  ${ a } ")));
        assert!(!is_script(&Value::str("\\${a}")));
        assert!(is_escaped_script(&Value::str("\\${a}")));
        assert_eq!(Some(" a "), extract_script(&Value::str("${ a }")));
        assert_eq!(Value::str("${a}"), unescape(Value::str("\\${a}")));
        assert_eq!(Value::str("\\${a}"), unescape(Value::str("\\\\${a}")));
        assert_eq!(Value::Int(1), unescape(Value::Int(1)));
    }
}
