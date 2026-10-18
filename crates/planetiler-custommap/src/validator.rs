//! Port of `validator/SchemaValidator.java` (custommap-specific parts).
//!
//! TODO(port): `SchemaValidator` extends core's `BaseSchemaValidator` (CLI output, `--watch` mode) and runs
//! `BaseSchemaValidator.validate(profile, SchemaSpecification, config)`; both live in `planetiler_core::validator`
//! which is not ported yet. Once it is, add `validate(schema, spec) -> Result` (build the root context with silenced
//! empty arguments, build a [`crate::configured_profile::ConfiguredProfile`] and call the core validator) and a
//! `main` that supports `verify schema.yml` as a shortcut. The test-only spec runner in
//! `tests/schema_tests.rs` mirrors the core logic until then.

use std::path::{Path, PathBuf};

use planetiler_core::value::Value;

use crate::configschema::SchemaConfig;

/// Where the examples (spec) for a schema come from.
#[derive(Clone, Debug, PartialEq)]
pub enum Examples {
    /// No examples.
    None,
    /// Examples in a separate file, resolved relative to the schema file.
    File(PathBuf),
    /// Examples embedded in the schema file under `examples:`.
    Embedded(Value),
}

/// Returns where the examples for `schema` (loaded from `schema_path`) are: embedded in the yaml file, or referenced
/// by a (relative) path.
pub fn examples_for(schema: &SchemaConfig, schema_path: &Path) -> Examples {
    match &schema.examples {
        None => Examples::None,
        Some(Value::String(s)) => {
            let path = Path::new(&**s);
            if path.is_absolute() {
                Examples::File(path.to_path_buf())
            } else {
                Examples::File(schema_path.parent().unwrap_or(Path::new("")).join(path))
            }
        }
        Some(other) => Examples::Embedded(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_examples() {
        let schema = SchemaConfig::load_str("examples: spec.yml\nlayers: []").unwrap();
        assert_eq!(Examples::File(PathBuf::from("dir/spec.yml")), examples_for(&schema, Path::new("dir/schema.yml")));
        let schema = SchemaConfig::load_str("layers: []").unwrap();
        assert_eq!(Examples::None, examples_for(&schema, Path::new("schema.yml")));
    }
}
