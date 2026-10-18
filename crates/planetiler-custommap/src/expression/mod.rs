//! Port of the `custommap.expression` package: dynamic expressions in schema configs.

pub mod boolean_expression_script;
pub mod config_expression;
pub mod config_expression_script;
pub mod script_environment;
pub mod stdlib;

pub use boolean_expression_script::BooleanExpressionScript;
pub use config_expression::{ConfigExpression, Signature};
pub use config_expression_script::ConfigExpressionScript;
pub use script_environment::ScriptEnvironment;

/// Exception that occurs at compile-time when preparing an embedded expression, or when a schema is malformed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseException(pub String);

impl ParseException {
    pub fn new(message: impl Into<String>) -> ParseException {
        ParseException(message.into())
    }

    /// Java `new ParseException(script, cause)`.
    pub fn parsing(script: &str, cause: impl std::fmt::Display) -> ParseException {
        ParseException(format!("Error parsing: {script}: {cause}"))
    }
}

/// Exception that occurs at runtime when evaluating a [`ConfigExpressionScript`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Error evaluating script: {script}: {message}")]
pub struct EvaluationException {
    pub script: String,
    pub message: String,
}
