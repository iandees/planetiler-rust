//! Port of `validator/SchemaValidator.java`: validates a custommap schema against its examples (embedded in the
//! schema under `examples:`, or in a separate file that `examples:` points to), on top of the shared
//! [`BaseSchemaValidator`] CLI driver from `planetiler_core::validator`.
//!
//! ```text
//! planetiler verify schema.yml [--watch]
//! planetiler verify-schema --schema=schema.yml [--watch]
//! ```

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use planetiler_core::config::arguments::Arguments;
use planetiler_core::util::ansi_colors;
use planetiler_core::validator::base_schema_validator::{self, BaseSchemaValidator, ValidationResult, indent};
use planetiler_core::validator::schema_specification::SchemaSpecification;
use planetiler_core::value::Value;

use crate::configschema::SchemaConfig;
use crate::configured_profile::ConfiguredProfile;
use crate::contexts::build_root_context;
use crate::expression::ParseException;

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

/// Returns the result of validating the profile defined by `schema` against the examples in `specification`.
pub fn validate(schema: SchemaConfig, specification: &SchemaSpecification) -> anyhow::Result<ValidationResult> {
    let root = build_root_context(&Arguments::of().silence(), schema.args())?;
    let config = root.config().clone();
    let profile = ConfiguredProfile::new(schema, root)?;
    Ok(base_schema_validator::validate(&profile, specification, &config))
}

/// Validates a schema file against its examples, printing a report to `output` (Java `SchemaValidator`).
pub struct SchemaValidator<W: Write> {
    schema_path: PathBuf,
    output: W,
    watch: bool,
}

impl<W: Write> SchemaValidator<W> {
    /// Creates a validator for `schema_file`, or the `--schema` argument if set (which must exist). `--watch` is read
    /// from `args`.
    pub fn new(args: &Arguments, schema_file: Option<&str>, output: W) -> anyhow::Result<Self> {
        let schema_path = match schema_file {
            Some(file) => args.input_file("schema", "Schema file", file)?,
            None => args.required_input_file("schema", "Schema file")?,
        };
        Ok(SchemaValidator { schema_path, output, watch: base_schema_validator::watch_arg(args) })
    }

    /// Returns the output writer (e.g. to inspect a `Vec<u8>` buffer).
    pub fn into_output(self) -> W {
        self.output
    }

    fn load_and_validate(&self, paths_to_watch: &mut BTreeSet<PathBuf>) -> anyhow::Result<ValidationResult> {
        paths_to_watch.insert(self.schema_path.clone());
        let schema = SchemaConfig::load(&self.schema_path)?;
        // examples can either be embedded in the yaml file, or referenced
        let spec = match examples_for(&schema, &self.schema_path) {
            Examples::File(path) => {
                // if referenced, make sure we watch that file for changes
                paths_to_watch.insert(path.clone());
                SchemaSpecification::load(&path)?
            }
            // like Java's `YAML.convertValue(schema, SchemaSpecification.class)`: read `examples` from the schema
            Examples::Embedded(_) => SchemaSpecification::load(&self.schema_path)?,
            Examples::None => SchemaSpecification::default(),
        };
        validate(schema, &spec)
    }
}

impl<W: Write> BaseSchemaValidator for SchemaValidator<W> {
    fn watch(&self) -> bool {
        self.watch
    }

    fn output(&mut self) -> &mut dyn Write {
        &mut self.output
    }

    fn validate_paths(&mut self, paths_to_watch: &mut BTreeSet<PathBuf>) -> Option<ValidationResult> {
        match self.load_and_validate(paths_to_watch) {
            Ok(result) => Some(result),
            Err(err) => {
                let root_cause = err.root_cause().to_string();
                let message = if err.chain().any(|e| e.is::<ParseException>()) {
                    ansi_colors::red(&format!("Malformed expression:\n\n{}", indent(&root_cause, 4)))
                } else if err.chain().any(|e| e.is::<serde_yaml::Error>()) {
                    ansi_colors::red(&format!("Malformed yaml input:\n\n{}", indent(&root_cause, 4)))
                } else {
                    indent(
                        &ansi_colors::red(&format!(
                            "Unexpected exception thrown:\n{}\n{err:?}",
                            indent(&root_cause, 4)
                        )),
                        4,
                    )
                };
                let _ = writeln!(self.output, "{message}");
                None
            }
        }
    }
}

/// `SchemaValidator.main`: `verify [schema.yml] [--schema=schema.yml] [--watch]`. Prints a report to stdout (and keeps
/// re-validating on changes with `--watch`). Returns true if every example passed.
pub fn main<S: AsRef<str>>(args: &[S]) -> anyhow::Result<bool> {
    // let users run `verify schema.yml` as a shortcut
    let (schema_file, rest) = match args.first().map(AsRef::as_ref) {
        Some(first) if (first.ends_with(".yml") || first.ends_with(".yaml")) && !first.starts_with('-') => {
            (Some(first), &args[1..])
        }
        _ => (None, args),
    };
    let arguments = Arguments::from_env_or_args(rest);
    SchemaValidator::new(&arguments, schema_file, std::io::stdout())?.run_or_watch()
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
