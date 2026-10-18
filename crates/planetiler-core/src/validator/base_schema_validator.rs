//! Port of `validator/BaseSchemaValidator.java`: verifies that a profile maps input elements to expected output
//! vector tile features as defined by a [`SchemaSpecification`].
//!
//! - [`validate`] runs every example of a specification through a [`Profile`] and returns a [`ValidationResult`].
//! - [`assert_valid`] / [`assert_valid_str`] are test helpers (Java `TestUtils.validateProfile`) that panic with the
//!   issues of every failing example.
//! - [`BaseSchemaValidator`] is the CLI driver (pretty-printed pass/fail report, optional watch mode); implementations
//!   only provide [`BaseSchemaValidator::validate_paths`], see
//!   [`ProfileValidator`](crate::validator::java_profile_validator::ProfileValidator).

use std::collections::BTreeSet;
use std::io::Write;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use geo_types::Geometry;

use crate::config::arguments::Arguments;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::feature_collector::{self, RangeWithTags};
use crate::geo::geometry_type::GeometryType;
use crate::geo::wkt;
use crate::profile::Profile;
use crate::reader::simple_feature::SimpleFeature;
use crate::stats::stats;
use crate::util::ansi_colors;
use crate::util::file_watcher::FileWatcher;
use crate::util::format;
use crate::validator::schema_specification::{Example, OutputFeature, SchemaSpecification};
use crate::value::{Tags, Value};

/// Result of comparing the output vector tile features of one example to what was expected.
#[derive(Debug)]
pub struct ExampleResult {
    pub example: Example,
    /// The list of mismatches, or the error (or panic) raised while processing the example.
    // TODO: include a symmetric diff so we can pretty-print the expected/actual output diff
    pub issues: anyhow::Result<Vec<String>>,
}

impl ExampleResult {
    /// True if the example was processed without error and had no issues.
    pub fn ok(&self) -> bool {
        matches!(&self.issues, Ok(issues) if issues.is_empty())
    }
}

/// Result of validating all examples of a [`SchemaSpecification`] (Java `BaseSchemaValidator.Result`).
#[derive(Debug, Default)]
pub struct ValidationResult {
    pub results: Vec<ExampleResult>,
}

impl ValidationResult {
    /// True if every example passed.
    pub fn ok(&self) -> bool {
        self.results.iter().all(ExampleResult::ok)
    }
}

/// What [`BaseSchemaValidator::validate_from_cli`] validated: the result (`None` if the specification could not be
/// loaded) and the files to watch for changes.
#[derive(Debug, Default)]
pub struct TestSummary {
    pub result: Option<ValidationResult>,
    pub paths: BTreeSet<PathBuf>,
}

/// Returns the `--watch` argument: whether to watch files for changes and re-run validation.
pub fn watch_arg(args: &Arguments) -> bool {
    args.get_boolean("watch", "Watch files for changes and re-run validation when schema or spec changes", false)
}

/// Shared CLI driver for schema validators (Java abstract class `BaseSchemaValidator`).
pub trait BaseSchemaValidator {
    /// True to keep watching the files from [`TestSummary::paths`] and re-run validation when they change.
    fn watch(&self) -> bool;

    /// Where to print the report.
    fn output(&mut self) -> &mut dyn Write;

    /// Loads the specification (adding every file it reads to `paths_to_watch`) and validates it, returning `None`
    /// (after printing the reason) if it could not be loaded.
    fn validate_paths(&mut self, paths_to_watch: &mut BTreeSet<PathBuf>) -> Option<ValidationResult>;

    /// Validates once and prints a report of passing and failing examples.
    fn validate_from_cli(&mut self) -> TestSummary {
        let mut paths = BTreeSet::new();
        let out = self.output();
        let _ = write!(out, "\nValidating...\n\n");
        let result = self.validate_paths(&mut paths);
        if let Some(result) = &result {
            print_report(result, self.output());
        }
        TestSummary { result, paths }
    }

    /// Validates, and if [`BaseSchemaValidator::watch`] is set keeps re-validating whenever an input file changes
    /// (blocking forever). Returns whether the first validation passed.
    fn run_or_watch(&mut self) -> anyhow::Result<bool> {
        let _ = writeln!(self.output(), "OK");
        let response = self.validate_from_cli();
        if self.watch() {
            let _ = write!(self.output(), "\nWatching filesystem for changes...\n");
            let paths: Vec<PathBuf> = response.paths.iter().cloned().collect();
            let mut watcher = FileWatcher::new_watcher(&paths);
            watcher.poll_for_changes(Duration::from_millis(300), |_changed| Ok(self.validate_from_cli().paths))?;
        }
        Ok(response.result.is_some_and(|r| r.ok()))
    }
}

fn pass_badge() -> String {
    ansi_colors::green_background(" PASS ")
}

fn fail_badge() -> String {
    ansi_colors::red_background(" FAIL ")
}

/// Prints the pass/fail line of each example, a summary of failures and the pass/fail counts to `out`.
pub fn print_report(result: &ValidationResult, out: &mut dyn Write) {
    let mut passed = 0;
    let mut failures = Vec::new();
    for example in &result.results {
        if example.ok() {
            passed += 1;
            let _ = writeln!(out, "{} {}", pass_badge(), example.example.name);
        } else {
            print_failure(example, out);
            failures.push(example);
        }
    }
    let failed = failures.len();
    if !failures.is_empty() {
        let _ = write!(out, "\nSummary of failures:\n");
        for failure in &failures {
            print_failure(failure, out);
        }
    }
    let mut summary = Vec::new();
    let none = passed + failed == 0;
    if none || failed > 0 {
        summary.push(ansi_colors::red_bold(&format!("{failed} failed")));
    }
    if none || passed > 0 {
        summary.push(ansi_colors::green_bold(&format!("{passed} passed")));
    }
    if none || (passed > 0 && failed > 0) {
        summary.push(format!("{} total", failed + passed));
    }
    let _ = write!(out, "\n{}\n", summary.join(", "));
}

fn print_failure(example: &ExampleResult, out: &mut dyn Write) {
    let _ = writeln!(out, "{} {}", fail_badge(), example.example.name);
    match &example.issues {
        Err(err) => {
            let _ = writeln!(out, "{}", indent(&format!("{err:?}"), 4).trim_end());
        }
        Ok(issues) => {
            for issue in issues {
                let _ = writeln!(out, "  ● {}", indent(issue, 4).trim());
            }
        }
    }
}

/// Java `String.indent(n)` for positive `n`: prefixes every line with `n` spaces and terminates it with `\n`.
pub(crate) fn indent(string: &str, n: usize) -> String {
    let prefix = " ".repeat(n);
    string.lines().map(|line| format!("{prefix}{line}\n")).collect()
}

/// Parses an example input geometry: `point`, `line`, `polygon` or a WKT string.
fn parse_geometry(geometry: &str) -> anyhow::Result<Geometry<f64>> {
    let wkt = match geometry.trim().to_lowercase().as_str() {
        "point" => "POINT (0 0)",
        "line" => "LINESTRING (0 0, 1 1)",
        "polygon" => "POLYGON ((0 0, 1 0, 1 1, 0 1, 0 0))",
        _ => geometry,
    };
    wkt::read_wkt(wkt).map_err(|_| {
        anyhow::anyhow!(r#"Bad geometry: "{geometry}", must be "point" "line" "polygon" or a valid WKT string."#)
    })
}

/// An output feature, or one linear range of it.
struct FeatureWithOverrides<'f, 'a> {
    feature: &'f feature_collector::Feature<'a>,
    overrides: Option<&'f RangeWithTags>,
}

impl FeatureWithOverrides<'_, '_> {
    fn attrs(&self, zoom: u32) -> &Tags {
        match self.overrides {
            Some(range) => &range.attrs,
            None => self.feature.get_attrs_at_zoom(zoom),
        }
    }
}

/// Returns the result of validating `profile` against the examples in `specification`.
pub fn validate(
    profile: &dyn Profile,
    specification: &SchemaSpecification,
    config: &PlanetilerConfig,
) -> ValidationResult {
    let factory = feature_collector::Factory::new(Arc::new(config.clone()), stats::in_memory());
    let results = specification
        .examples
        .iter()
        .map(|example| {
            let issues = panic::catch_unwind(AssertUnwindSafe(|| validate_example(profile, example, &factory)))
                .unwrap_or_else(|payload| Err(anyhow::anyhow!("panicked: {}", panic_message(&payload))));
            ExampleResult { example: example.clone(), issues }
        })
        .collect();
    ValidationResult { results }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn validate_example(
    profile: &dyn Profile,
    example: &Example,
    factory: &feature_collector::Factory,
) -> anyhow::Result<Vec<String>> {
    let mut issues = Vec::new();
    let input = &example.input;
    let expected_features = &example.output;
    let geometry = parse_geometry(&input.geometry)?;
    let tags: Tags = input.tags.iter().map(|(k, v)| (k.clone(), Value::str(v))).collect();
    let feature = SimpleFeature::create(geometry, tags, input.source.clone(), None::<String>, 0);
    let mut collector = factory.get(&feature);
    profile.process_feature(&feature, &mut collector);

    let zooms: BTreeSet<u32> = expected_features.iter().map(OutputFeature::at_zoom).collect();
    let mut result = Vec::new();
    for output_feature in collector.iter() {
        if output_feature.has_linear_ranges() {
            for &zoom in &zooms {
                for range in output_feature.get_linear_ranges_at_zoom(zoom) {
                    result.push(FeatureWithOverrides { feature: output_feature, overrides: Some(range) });
                }
            }
        } else {
            result.push(FeatureWithOverrides { feature: output_feature, overrides: None });
        }
    }

    if result.len() != expected_features.len() {
        issues.push(format!(
            "Different number of elements, expected={} actual={}",
            expected_features.len(),
            result.len()
        ));
        return Ok(issues);
    }

    // TODO: print a diff of the input and output feature YAML representations
    for (i, expected) in expected_features.iter().enumerate() {
        // first feature with the highest proximity (Java Stream.max keeps the first of equal elements)
        let mut best = 0;
        let mut best_score = i32::MIN;
        for (idx, candidate) in result.iter().enumerate() {
            let score = proximity(expected, candidate);
            if score > best_score {
                best = idx;
                best_score = score;
            }
        }
        let actual = result.remove(best);
        let zoom = expected.at_zoom();
        let actual_tags = actual.attrs(zoom);
        let feature = actual.feature;
        let prefix = format!("feature[{i}]");
        check(
            &format!("{prefix}.layer"),
            &mut issues,
            expected.layer.as_deref().map(Value::str),
            Some(Value::str(feature.get_layer())),
            true,
        );
        check(
            &format!("{prefix}.minzoom"),
            &mut issues,
            expected.min_zoom.map(Value::from),
            Some(Value::from(feature.get_min_zoom())),
            true,
        );
        check(
            &format!("{prefix}.maxzoom"),
            &mut issues,
            expected.max_zoom.map(Value::from),
            Some(Value::from(feature.get_max_zoom())),
            true,
        );
        check(
            &format!("{prefix}.minsize"),
            &mut issues,
            expected.min_size.map(Value::Double),
            Some(Value::Double(feature.get_min_pixel_size_at_zoom(zoom))),
            true,
        );
        let actual_geometry = GeometryType::type_of(feature.get_geometry());
        if let Some(expected_geometry) = expected.geometry
            && expected_geometry != actual_geometry
        {
            issues.push(format!(
                "{prefix}.geometry: expected <{}> actual <{}>",
                geometry_type_name(expected_geometry),
                geometry_type_name(actual_geometry)
            ));
        }
        let mut remaining: BTreeSet<&str> = actual_tags.keys().map(String::as_str).collect();
        for (tag, value) in &expected.tags {
            check(
                &format!("{prefix}.tags[\"{tag}\"]"),
                &mut issues,
                non_null(Some(value)).cloned(),
                non_null(actual_tags.get(tag)).cloned(),
                false,
            );
            remaining.remove(tag.as_str());
        }
        if !expected.allow_extra_tags() {
            for tag in remaining {
                check(&format!("{prefix}.tags[\"{tag}\"]"), &mut issues, None, actual_tags.get(tag).cloned(), false);
            }
        }
    }
    Ok(issues)
}

fn non_null(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| !v.is_null())
}

/// Java enum name of a geometry type (used in issue messages).
fn geometry_type_name(geometry_type: GeometryType) -> &'static str {
    match geometry_type {
        GeometryType::Unknown => "UNKNOWN",
        GeometryType::Point => "POINT",
        GeometryType::Line => "LINE",
        GeometryType::Polygon => "POLYGON",
    }
}

fn proximity(expected: &OutputFeature, item: &FeatureWithOverrides<'_, '_>) -> i32 {
    let layer = if expected.layer.as_deref() == Some(item.feature.get_layer()) { 2 } else { 0 };
    let geometry = if expected.geometry == Some(GeometryType::type_of(item.feature.get_geometry())) { 1 } else { 0 };
    layer + geometry + map_proximity(expected, item)
}

fn map_proximity(expected: &OutputFeature, actual: &FeatureWithOverrides<'_, '_>) -> i32 {
    let actual_map = actual.attrs(expected.at_zoom());
    let mut result = 0;
    for (key, value) in &expected.tags {
        if let Some(actual_value) = actual_map.get(key) {
            result += 1;
            if actual_value == value {
                result += 1;
            }
        }
    }
    result
}

/// Records an issue if `expected` and `actual` differ (skipped when `expected` is `None` and `ignore_when_null`).
fn check(
    field: &str,
    issues: &mut Vec<String>,
    expected: Option<Value>,
    actual: Option<Value>,
    ignore_when_null: bool,
) {
    if (!ignore_when_null || expected.is_some()) && expected != actual {
        // numbers of different widths with the same string representation are equal
        if let (Some(e), Some(a)) = (&expected, &actual)
            && e.is_number()
            && a.is_number()
            && e.to_string() == a.to_string()
        {
            return;
        }
        issues.push(format!("{field}: expected <{}> actual <{}>", format_value(&expected), format_value(&actual)));
    }
}

fn format_value(value: &Option<Value>) -> String {
    match value {
        None | Some(Value::Null) => "null".to_string(),
        Some(Value::String(s)) => format::quote(Some(s)),
        Some(other) => other.to_string(),
    }
}

/// Validates `profile` against `spec` with default config, panicking with the issues of every failing example
/// (Java `TestUtils.validateProfile`, for use in tests).
pub fn assert_valid(profile: &dyn Profile, spec: &SchemaSpecification) {
    let result = validate(profile, spec, &PlanetilerConfig::defaults());
    let failures: Vec<String> = result
        .results
        .iter()
        .filter(|r| !r.ok())
        .map(|r| match &r.issues {
            Ok(issues) => {
                format!("{}: Failed with {} issues:\n{}", r.example.name, issues.len(), issues.join("\n"))
            }
            Err(err) => format!("{}: {err:?}", r.example.name),
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// [`assert_valid`] for a YAML specification string.
pub fn assert_valid_str(profile: &dyn Profile, spec: &str) {
    let spec = SchemaSpecification::load_str(spec).expect("invalid schema specification");
    assert_valid(profile, &spec);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::feature_collector::FeatureCollector;
    use crate::reader::source_feature::SourceFeature;

    /// A profile backed by a closure.
    pub(crate) struct FnProfile<F>(pub F);

    impl<F> Profile for FnProfile<F>
    where
        F: Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync,
    {
        fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
            (self.0)(source_feature, features)
        }

        fn name(&self) -> String {
            "test profile".into()
        }
    }

    pub(crate) fn water_schema() -> impl Profile {
        FnProfile(|source: &dyn SourceFeature, features: &mut FeatureCollector<'_>| {
            if source.can_be_polygon() && source.has_tag_str("natural", "water") {
                features.polygon("water").set_min_pixel_size(10.0).inherit_attr_from_source("natural");
            }
        })
    }

    const GOOD_SPEC: &str = r#"
examples:
- name: test output
  input:
    source: osm
    geometry: polygon
    tags:
      natural: water
  output:
  - layer: water
    geometry: polygon
    tags:
      natural: water
"#;

    struct TestValidator<'p> {
        profile: &'p dyn Profile,
        spec: SchemaSpecification,
        output: Vec<u8>,
    }

    impl BaseSchemaValidator for TestValidator<'_> {
        fn watch(&self) -> bool {
            watch_arg(&Arguments::of())
        }

        fn output(&mut self) -> &mut dyn Write {
            &mut self.output
        }

        fn validate_paths(&mut self, _paths_to_watch: &mut BTreeSet<PathBuf>) -> Option<ValidationResult> {
            Some(validate(self.profile, &self.spec, &PlanetilerConfig::defaults()))
        }
    }

    /// Validates `spec` and also returns what the CLI would have printed.
    fn validate_both(profile: &dyn Profile, spec: &str) -> (ValidationResult, String) {
        let spec = SchemaSpecification::load_str(spec).unwrap();
        let result = validate(profile, &spec, &PlanetilerConfig::defaults());
        let mut cli = TestValidator { profile, spec, output: Vec::new() };
        let summary = cli.validate_from_cli();
        assert!(summary.result.is_some());
        (result, String::from_utf8(cli.output).unwrap())
    }

    /// Java `"...".indent(6).strip()`.
    fn indent_strip(s: &str, n: usize) -> String {
        indent(s, n).trim().to_string()
    }

    pub(crate) fn water_spec(layer: &str, geometry: &str, tags: Option<&str>, extra: Option<&str>) -> String {
        format!(
            r#"
examples:
- name: test output
  input:
    source: osm
    geometry: polygon
    tags:
      natural: water
  output:
    layer: {layer}
    geometry: {geometry}
    {}
    tags:
      {}
"#,
            extra.unwrap_or(""),
            tags.map(|t| indent_strip(t, 6)).unwrap_or_default()
        )
    }

    pub(crate) type WaterCase = (bool, &'static str, &'static str, Option<&'static str>, Option<&'static str>);

    /// Cases of Java's `testValidateWaterPolygon`: (should be ok, layer, geometry, tags, extra property).
    pub(crate) const WATER_CASES: &[WaterCase] = &[
        (true, "water", "polygon", Some("natural: water"), None),
        (true, "water", "polygon", None, None),
        (true, "water", "polygon", Some("natural: water\nother: null"), None),
        (false, "water", "polygon", Some("natural: null"), None),
        (false, "water2", "polygon", Some("natural: water"), None),
        (false, "water", "line", Some("natural: water"), None),
        (false, "water", "polygon", Some("natural: water2"), None),
        (false, "water", "polygon", Some("natural: water\nother: value"), None),
        (true, "water", "polygon", Some("natural: water"), Some("allow_extra_tags: true")),
        (true, "water", "polygon", Some("natural: water"), Some("allow_extra_tags: false")),
        (true, "water", "polygon", None, Some("allow_extra_tags: true")),
        (false, "water", "polygon", None, Some("allow_extra_tags: false")),
        (true, "water", "polygon", None, Some("min_size: 10")),
        (false, "water", "polygon", None, Some("min_size: 9")),
    ];

    #[test]
    fn test_validate_water_polygon() {
        let profile = water_schema();
        for &(should_be_ok, layer, geometry, tags, extra) in WATER_CASES {
            let spec = water_spec(layer, geometry, tags, extra);
            let (result, cli) = validate_both(&profile, &spec);
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
    fn test_partial_length_attribute() {
        let profile = FnProfile(|_: &dyn SourceFeature, features: &mut FeatureCollector<'_>| {
            let feature = features.line("layer");
            feature.linear_range(0.0, 0.6).set_attr("a", 1);
            feature.linear_range(0.4, 1.0).set_attr("b", 2);
        });
        for tags in [["{a:1}", "{a:1,b:2}", "{b:2}"], ["{a: 1}", "{a: 1, b: 2}", "{b: 2}"]] {
            let (result, _) = validate_both(
                &profile,
                &format!(
                    r#"
examples:
- name: test output
  input:
    source: osm
    geometry: line
    tags:
      natural: water
  output:
  - layer: layer
    geometry: line
    tags: {}
  - layer: layer
    geometry: line
    tags: {}
  - layer: layer
    geometry: line
    tags: {}
"#,
                    tags[0], tags[1], tags[2]
                ),
            );
            assert_eq!(1, result.results.len());
            assert_eq!("test output", result.results[0].example.name);
            assert!(result.results[0].ok(), "{result:?}");
        }
    }

    #[test]
    fn test_validation_fails_wrong_number_of_features() {
        let profile = water_schema();
        let (result, _) = validate_both(
            &profile,
            r#"
examples:
- name: test output
  input:
    source: osm
    geometry: polygon
    tags:
      natural: water
  output:
"#,
        );
        assert!(!result.ok(), "{result:?}");

        let (result, _) = validate_both(
            &profile,
            r#"
examples:
- name: test output
  input:
    source: osm
    geometry: polygon
    tags:
      natural: water
  output:
  - layer: water
    geometry: polygon
    tags:
      natural: water
  - layer: water2
    geometry: polygon
    tags:
      natural: water2
"#,
        );
        assert!(!result.ok(), "{result:?}");
    }

    #[test]
    fn test_junit_adapter_spec() {
        assert_valid(&water_schema(), &SchemaSpecification::load_str(GOOD_SPEC).unwrap());
    }

    #[test]
    fn test_junit_adapter_string() {
        assert_valid_str(&water_schema(), GOOD_SPEC);
    }

    #[test]
    #[should_panic(expected = "Failed with 1 issues")]
    fn test_junit_adapter_fails() {
        assert_valid_str(&water_schema(), &water_spec("water2", "polygon", None, None));
    }

    #[test]
    fn test_bad_geometry_and_panics_are_failures() {
        let (result, cli) =
            validate_both(&water_schema(), &GOOD_SPEC.replacen("geometry: polygon", "geometry: blob", 1));
        assert!(!result.ok());
        assert!(result.results[0].issues.is_err());
        assert!(cli.contains("Bad geometry"), "{cli}");

        let profile = FnProfile(|_: &dyn SourceFeature, _: &mut FeatureCollector<'_>| panic!("boom"));
        let (result, cli) = validate_both(&profile, GOOD_SPEC);
        assert!(!result.ok());
        assert!(cli.contains("FAIL") && cli.contains("boom"), "{cli}");
    }

    #[test]
    fn test_wkt_input_and_zoom_checks() {
        let profile = FnProfile(|_: &dyn SourceFeature, features: &mut FeatureCollector<'_>| {
            features.point("poi").set_zoom_range(5, 10).set_attr("n", 1).set_attr("d", 1.5);
        });
        let spec = |minzoom: u32, n: &str| {
            format!(
                r#"
examples:
- name: wkt
  input:
    geometry: POINT (1 2)
  output:
    layer: poi
    geometry: point
    min_zoom: {minzoom}
    max_zoom: 10
    tags:
      n: {n}
      d: 1.5
"#
            )
        };
        let (result, _) = validate_both(&profile, &spec(5, "1"));
        assert!(result.ok(), "{result:?}");
        let (result, _) = validate_both(&profile, &spec(4, "1.0"));
        let issues = result.results[0].issues.as_ref().unwrap();
        assert_eq!(
            &vec![
                "feature[0].minzoom: expected <4> actual <5>".to_string(),
                "feature[0].tags[\"n\"]: expected <1.0> actual <1>".to_string()
            ],
            issues
        );
    }

    #[test]
    fn test_cli_summary() {
        let profile = water_schema();
        let (_, cli) = validate_both(&profile, GOOD_SPEC);
        assert!(cli.contains("PASS") && cli.contains("test output") && cli.contains("1 passed"), "{cli}");
        assert!(!cli.contains("total"), "{cli}");
        let (_, cli) = validate_both(&profile, "examples: []");
        assert!(cli.contains("0 failed") && cli.contains("0 passed") && cli.contains("0 total"), "{cli}");
    }
}
