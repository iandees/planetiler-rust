//! Port of the `planetiler-custommap` module: build vector tile profiles from a YAML schema file.
//!
//! - [`configschema`] holds the serde models of the YAML schema ([`configschema::SchemaConfig`] and friends).
//! - [`contexts`] defines the runtime inputs available to the expressions in a schema (the [`contexts::Root`] context
//!   with arguments, and [`contexts::ScriptContext`] for per-feature evaluation).
//! - [`expression`] contains the dynamic expression layer: [`expression::ConfigExpression`] (values computed from
//!   tags, arguments and embedded CEL scripts), [`expression::BooleanExpressionScript`], and the CEL integration
//!   ([`expression::stdlib`]).
//! - [`boolean_expression_parser`] / [`config_expression_parser`] turn the YAML structures into
//!   [`planetiler_core::expression::expression::Expression`]s and [`expression::ConfigExpression`]s.
//! - [`configured_profile::ConfiguredProfile`] implements [`planetiler_core::profile::Profile`] for a schema, and
//!   [`configured_map_main`] is the `ConfiguredMapMain` entry point.

pub mod boolean_expression_parser;
pub mod config_expression_parser;
pub mod configschema;
pub mod configured_feature;
pub mod configured_map_main;
pub mod configured_profile;
pub mod contexts;
pub mod expression;
pub mod samples;
pub mod source;
pub mod tag_value_producer;
pub mod type_conversion;
pub mod validator;

pub use expression::{EvaluationException, ParseException};
