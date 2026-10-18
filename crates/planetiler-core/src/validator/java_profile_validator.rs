//! Port of `validator/JavaProfileValidator.java`: validates a Rust [`Profile`] against a YAML set of example source
//! features and the vector tile features they should map to (see
//! [`SchemaSpecification`](crate::validator::schema_specification::SchemaSpecification)).

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::planetiler_config::PlanetilerConfig;
use crate::profile::Profile;
use crate::util::ansi_colors;
use crate::validator::base_schema_validator::{self, BaseSchemaValidator, ValidationResult, indent};
use crate::validator::schema_specification::SchemaSpecification;

/// Validates a [`Profile`] against the specification at a path, printing a report to `output` (Java
/// `JavaProfileValidator`).
pub struct ProfileValidator<'p, W: Write> {
    profile: &'p dyn Profile,
    spec_path: PathBuf,
    config: PlanetilerConfig,
    output: W,
    watch: bool,
}

impl<'p, W: Write> ProfileValidator<'p, W> {
    /// Creates a validator; `--watch` is read from `config.arguments`.
    pub fn new(config: PlanetilerConfig, spec_path: impl Into<PathBuf>, profile: &'p dyn Profile, output: W) -> Self {
        let watch = base_schema_validator::watch_arg(&config.arguments);
        ProfileValidator { profile, spec_path: spec_path.into(), config, output, watch }
    }

    /// Returns the output writer (e.g. to inspect a `Vec<u8>` buffer).
    pub fn into_output(self) -> W {
        self.output
    }
}

/// Validates that `profile` maps input features to expected output features as defined in `spec_path`, printing a
/// report to stdout (and watching for changes when `--watch` is set). Returns true if successful, false if failed.
pub fn validate(profile: &dyn Profile, spec_path: &Path, config: &PlanetilerConfig) -> anyhow::Result<bool> {
    ProfileValidator::new(config.clone(), spec_path, profile, std::io::stdout()).run_or_watch()
}

impl<W: Write> BaseSchemaValidator for ProfileValidator<'_, W> {
    fn watch(&self) -> bool {
        self.watch
    }

    fn output(&mut self) -> &mut dyn Write {
        &mut self.output
    }

    fn validate_paths(&mut self, paths_to_watch: &mut BTreeSet<PathBuf>) -> Option<ValidationResult> {
        paths_to_watch.insert(self.spec_path.clone());
        match SchemaSpecification::load(&self.spec_path) {
            Ok(spec) => Some(base_schema_validator::validate(self.profile, &spec, &self.config)),
            Err(err) => {
                print_load_error(&err, &mut self.output);
                None
            }
        }
    }
}

/// Prints an error raised while loading a specification, distinguishing malformed YAML from other errors.
pub(crate) fn print_load_error(err: &anyhow::Error, output: &mut dyn Write) {
    let root_cause = err.root_cause().to_string();
    let message = if err.chain().any(|e| e.is::<serde_yaml::Error>()) {
        ansi_colors::red(&format!("Malformed yaml input:\n\n{}", indent(&root_cause, 4)))
    } else {
        indent(&ansi_colors::red(&format!("Unexpected exception thrown:\n{}\n{err:?}", indent(&root_cause, 4))), 4)
    };
    let _ = writeln!(output, "{message}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::arguments::Arguments;
    use crate::feature_collector::FeatureCollector;
    use crate::reader::source_feature::SourceFeature;
    use crate::validator::base_schema_validator::TestSummary;
    use crate::validator::base_schema_validator::tests::{FnProfile, WATER_CASES, water_spec};

    fn validate_file(profile: &dyn Profile, spec: Option<&str>) -> (TestSummary, String) {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("spec.yaml");
        if let Some(spec) = spec {
            std::fs::write(&spec_path, spec).unwrap();
        }
        let config = PlanetilerConfig::from(&Arguments::of()).unwrap();
        let mut validator = ProfileValidator::new(config, &spec_path, profile, Vec::new());
        let summary = validator.validate_from_cli();
        assert_eq!(BTreeSet::from([spec_path]), summary.paths);
        (summary, String::from_utf8(validator.into_output()).unwrap())
    }

    #[test]
    fn test_validate_water_polygon() {
        let profile = FnProfile(|source: &dyn SourceFeature, features: &mut FeatureCollector<'_>| {
            if source.can_be_polygon() && source.has_tag_str("natural", "water") {
                features.polygon("water").inherit_attr_from_source("natural").set_min_pixel_size(10.0);
            }
        });
        for &(should_be_ok, layer, geometry, tags, extra) in WATER_CASES {
            let spec = water_spec(layer, geometry, tags, extra);
            let (summary, cli) = validate_file(&profile, Some(&spec));
            let result = summary.result.unwrap();
            assert_eq!(1, result.results.len());
            assert_eq!("test output", result.results[0].example.name);
            if should_be_ok {
                assert!(result.ok(), "{spec}: {result:?}");
                assert!(!cli.contains("FAIL"), "{spec}: contained FAIL but should not have: {cli}");
            } else {
                assert!(!result.ok(), "{spec}: Expected an issue, but there were none");
                assert!(cli.contains("FAIL"), "{spec}: did not contain FAIL but should have: {cli}");
            }
        }
    }

    #[test]
    fn test_malformed_and_missing_spec() {
        let profile = FnProfile(|_: &dyn SourceFeature, _: &mut FeatureCollector<'_>| {});
        let (summary, cli) = validate_file(&profile, Some("examples: [\n"));
        assert!(summary.result.is_none());
        assert!(cli.contains("Malformed yaml input"), "{cli}");

        let (summary, cli) = validate_file(&profile, None);
        assert!(summary.result.is_none());
        assert!(cli.contains("Unexpected exception thrown"), "{cli}");
    }
}
