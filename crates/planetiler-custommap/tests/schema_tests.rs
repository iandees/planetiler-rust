//! Ports of `SchemaYAMLLoadTest`, `SchemaTests` (the shortbread spec) and `SchemaValidatorTest`, using the shared
//! `planetiler_core::validator` and [`planetiler_custommap::validator`].

use std::path::{Path, PathBuf};

use planetiler_core::config::arguments::Arguments;
use planetiler_core::feature_collector::Factory;
use planetiler_core::geo::wkt::read_wkt;
use planetiler_core::profile::Profile;
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::stats::stats::in_memory;
use planetiler_core::validator::base_schema_validator::{BaseSchemaValidator, ValidationResult};
use planetiler_core::validator::schema_specification::SchemaSpecification;
use planetiler_core::value::Value;
use planetiler_custommap::configschema::SchemaConfig;
use planetiler_custommap::configured_profile::ConfiguredProfile;
use planetiler_custommap::contexts::build_root_context;
use planetiler_custommap::validator::{self, SchemaValidator};

fn manifest_path(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
}

// ---------------------------------------------------------------------------------------------------------------
// SchemaYAMLLoadTest
// ---------------------------------------------------------------------------------------------------------------

fn test_schemas_in_folder(path: &Path) {
    let mut schema_files: Vec<PathBuf> = std::fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            name.ends_with(".yml") && !name.ends_with("spec.yml")
        })
        .collect();
    schema_files.sort();
    assert!(!schema_files.is_empty(), "No files found");
    for schema_file in schema_files {
        let schema =
            SchemaConfig::load(&schema_file).unwrap_or_else(|e| panic!("Failed to unmarshall {schema_file:?}: {e:?}"));
        let root = build_root_context(&Arguments::of(), schema.args()).unwrap();
        ConfiguredProfile::new(schema, root).unwrap_or_else(|e| panic!("{schema_file:?}: {e}"));
    }
}

#[test]
fn test_schema_load() {
    test_schemas_in_folder(&manifest_path("resources/samples"));
    test_schemas_in_folder(&manifest_path("tests/resources/validSchema"));
}

fn assert_all_pass(result: &ValidationResult) {
    let failures: Vec<String> = result
        .results
        .iter()
        .filter(|r| !r.ok())
        .map(|r| format!("FAIL {}\n  {:?}", r.example.name, r.issues))
        .collect();
    assert!(failures.is_empty(), "{} of {} failed:\n{}", failures.len(), result.results.len(), failures.join("\n"));
}

// ---------------------------------------------------------------------------------------------------------------
// SchemaTests
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn shortbread() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let base = manifest_path("resources/samples");
    let schema = SchemaConfig::load(&base.join("shortbread.yml")).unwrap();
    let spec = SchemaSpecification::load(&base.join("shortbread.spec.yml")).unwrap();
    let result = validator::validate(schema.clone(), &spec).unwrap();
    assert!(result.results.len() > 50);
    assert_all_pass(&result);

    // make sure the validator isn't vacuous
    let mut bad_spec = spec;
    bad_spec.examples[1].output[1].tags.insert("name".into(), Value::str("wrong"));
    let result = validator::validate(schema, &bad_spec).unwrap();
    assert!(!result.results[1].ok());
    assert!(result.results[0].ok());
}

// ---------------------------------------------------------------------------------------------------------------
// SchemaValidatorTest
// ---------------------------------------------------------------------------------------------------------------

/// Runs the CLI validator on `schema_path` and returns what it printed.
fn validate_cli(schema_path: &Path) -> String {
    let mut validator =
        SchemaValidator::new(&Arguments::of(), Some(&schema_path.to_string_lossy()), Vec::new()).unwrap();
    validator.validate_from_cli();
    String::from_utf8(validator.into_output()).unwrap()
}

/// Validates `schema` against `spec` directly, and through the CLI with the examples in a referenced file (absolute
/// and relative path) and embedded in the schema; the CLI output must be the same for all three.
fn validate(schema: &str, spec: &str) -> (ValidationResult, String) {
    let result =
        validator::validate(SchemaConfig::load_str(schema).unwrap(), &SchemaSpecification::load_str(spec).unwrap())
            .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let schema_path = tmp.path().join("schema");
    let spec_path = tmp.path().join("spec.yml");
    std::fs::write(&spec_path, spec).unwrap();
    std::fs::write(&schema_path, format!("{schema}\nexamples: {}", spec_path.display())).unwrap();
    let cli_output = validate_cli(&schema_path);

    // also test the case where the examples are embedded in the schema itself
    std::fs::write(&schema_path, format!("{schema}\n{spec}")).unwrap();
    assert_eq!(cli_output, validate_cli(&schema_path));

    // also test where examples points to a relative path
    std::fs::write(&schema_path, format!("{schema}\nexamples: spec.yml")).unwrap();
    assert_eq!(cli_output, validate_cli(&schema_path));
    (result, cli_output)
}

/// Java `"...".indent(n).strip()`.
fn indent_strip(s: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    s.lines().map(|l| format!("{pad}{l}")).collect::<Vec<_>>().join("\n").trim().to_string()
}

#[test]
fn test_validate_water_polygon() {
    type Case<'a> = (bool, &'a str, &'a str, Option<&'a str>, Option<&'a str>);
    let cases: &[Case<'_>] = &[
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
    let schema = r#"
sources:
  osm:
    type: osm
    url: geofabrik:rhode-island
layers:
- id: water
  features:
  - source: osm
    geometry: polygon
    min_size: 10
    include_when:
      natural: water
    attributes:
    - key: natural
"#;
    for (should_be_ok, layer, geometry, tags, extra) in cases {
        let spec = format!(
            "examples:\n- name: test output\n  input:\n    source: osm\n    geometry: polygon\n    tags:\n      natural: water\n  output:\n    layer: {layer}\n    geometry: {geometry}\n    {}\n    tags:\n      {}\n",
            extra.unwrap_or(""),
            indent_strip(tags.unwrap_or(""), 6)
        );
        let (result, cli) = validate(schema, &spec);
        let case = format!("{layer} {geometry} {tags:?} {extra:?}");
        assert_eq!(1, result.results.len());
        assert_eq!("test output", result.results[0].example.name);
        if *should_be_ok {
            assert!(result.ok(), "{case}: {result:?}");
            assert!(!cli.contains("FAIL"), "{case}: contained FAIL but should not have: {cli}");
        } else {
            assert!(!result.ok(), "{case}: Expected an issue, but there were none");
            assert!(cli.contains("FAIL"), "{case}: did not contain FAIL but should have: {cli}");
        }
    }
}

#[test]
fn test_validation_wires_in_arguments() {
    let schema = r#"
sources:
  osm:
    type: osm
    url: geofabrik:rhode-island
args:
  key: default_value
layers:
- id: water
  features:
  - source: osm
    geometry: polygon
    include_when:
      natural: water
    attributes:
    - key: from_arg
      arg_value: key
    - key: threads
      value: '${ args.threads + 1 }'
"#;
    let threads = Arguments::of().threads().unwrap();
    let spec = format!(
        "examples:\n- name: test output\n  input:\n    source: osm\n    geometry: polygon\n    tags:\n      natural: water\n  output:\n    layer: water\n    tags:\n      from_arg: default_value\n      threads: {}\n",
        threads + 1
    );
    let (result, cli) = validate(schema, &spec);
    assert_all_pass(&result);
    assert!(!cli.contains("FAIL"), "{cli}");
}

#[test]
fn test_cli_reports_malformed_input() {
    let tmp = tempfile::tempdir().unwrap();
    let schema_path = tmp.path().join("schema.yml");
    std::fs::write(&schema_path, "layers: [\n").unwrap();
    assert!(validate_cli(&schema_path).contains("Malformed yaml input"));

    std::fs::write(
        &schema_path,
        "sources:\n  osm:\n    type: osm\n    url: geofabrik:monaco\nlayers:\n- id: water\n  features:\n  - source: osm\n    geometry: polygon\n    include_when: '${ 1 + }'\n",
    )
    .unwrap();
    let output = validate_cli(&schema_path);
    assert!(output.contains("Malformed expression"), "{output}");

    // missing schema file
    assert!(SchemaValidator::new(&Arguments::of(), Some("missing.yml"), Vec::new()).is_err());
}

/// Rough throughput check of the shortbread profile:
/// `cargo test --release -p planetiler-custommap --test schema_tests bench_shortbread -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench_shortbread() {
    use planetiler_core::tags;
    let base = manifest_path("resources/samples");
    let schema = SchemaConfig::load(&base.join("shortbread.yml")).unwrap();
    let root = build_root_context(&Arguments::of().silence(), schema.args()).unwrap();
    let config = root.config().clone();
    let profile = ConfiguredProfile::new(schema, root).unwrap();
    let factory = Factory::new(config, in_memory());
    let (point, line, polygon) = ("POINT (0 0)", "LINESTRING (0 0, 1 1)", "POLYGON ((0 0, 1 0, 1 1, 0 1, 0 0))");
    let inputs = [
        (line, tags! {"highway" => "primary", "name" => "Main St", "ref" => "A1;B2", "surface" => "asphalt"}),
        (point, tags! {"amenity" => "cafe", "name" => "Cafe"}),
        (polygon, tags! {"building" => "yes", "addr:housenumber" => "12"}),
        (point, tags! {"natural" => "tree"}),
        (line, tags! {"footway" => "sidewalk"}),
        (point, tags! {"place" => "city", "name" => "City", "population" => "100000"}),
    ];
    let features: Vec<SimpleFeature> = inputs
        .into_iter()
        .map(|(g, t)| SimpleFeature::create(read_wkt(g).unwrap(), t, "osm", None::<&str>, 1))
        .collect();
    let n = 300_000;
    let start = std::time::Instant::now();
    let mut count = 0;
    for i in 0..n {
        let feature = &features[i % features.len()];
        let mut collector = factory.get(feature);
        profile.process_feature(feature, &mut collector);
        count += collector.len();
    }
    let elapsed = start.elapsed();
    println!("{n} features -> {count} outputs in {elapsed:?} ({:.0} features/s)", n as f64 / elapsed.as_secs_f64());
}
