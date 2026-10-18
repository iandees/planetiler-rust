//! Ports of `SchemaYAMLLoadTest`, `SchemaTests` (the shortbread spec) and `SchemaValidatorTest`.
//!
//! TODO(port): `planetiler_core::validator` (`BaseSchemaValidator`, `SchemaSpecification`) isn't ported yet, so this
//! file contains a minimal test-only port of `BaseSchemaValidator.validate(profile, spec, config)`. Switch to the
//! core validator once it lands.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use planetiler_core::config::arguments::Arguments;
use planetiler_core::config::planetiler_config::{MAX_MAXZOOM, PlanetilerConfig};
use planetiler_core::feature_collector::{Factory, Feature};
use planetiler_core::geo::geometry_type::GeometryType;
use planetiler_core::geo::wkt::read_wkt;
use planetiler_core::profile::Profile;
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::stats::stats::in_memory;
use planetiler_core::util::yaml;
use planetiler_core::value::{Tags, Value};
use planetiler_custommap::configschema::SchemaConfig;
use planetiler_custommap::configured_profile::ConfiguredProfile;
use planetiler_custommap::contexts::build_root_context;
use serde::{Deserialize, Deserializer};

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

// ---------------------------------------------------------------------------------------------------------------
// minimal SchemaSpecification + BaseSchemaValidator
// ---------------------------------------------------------------------------------------------------------------

fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<OutputFeature>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(OutputFeature),
        Many(Vec<OutputFeature>),
    }
    Ok(match Option::<OneOrMany>::deserialize(d)? {
        None => vec![],
        Some(OneOrMany::One(o)) => vec![o],
        Some(OneOrMany::Many(m)) => m,
    })
}

fn yaml_tags<'de, D: Deserializer<'de>>(d: D) -> Result<Tags, D::Error> {
    match yaml::to_value(&serde_yaml::Value::deserialize(d)?) {
        Value::Map(m) => Ok((*m).clone()),
        _ => Ok(Tags::default()),
    }
}

#[derive(Deserialize)]
struct Spec {
    examples: Vec<Example>,
}

#[derive(Deserialize)]
struct Example {
    name: String,
    input: InputFeature,
    #[serde(default, deserialize_with = "one_or_many")]
    output: Vec<OutputFeature>,
}

#[derive(Deserialize)]
struct InputFeature {
    source: Option<String>,
    geometry: String,
    #[serde(default, deserialize_with = "yaml_tags")]
    tags: Tags,
}

#[derive(Deserialize)]
struct OutputFeature {
    layer: Option<String>,
    geometry: Option<String>,
    min_zoom: Option<u32>,
    max_zoom: Option<u32>,
    min_size: Option<f64>,
    at_zoom: Option<u32>,
    allow_extra_tags: Option<bool>,
    #[serde(default, deserialize_with = "yaml_tags")]
    tags: Tags,
}

impl OutputFeature {
    fn at_zoom(&self) -> u32 {
        self.at_zoom.unwrap_or(MAX_MAXZOOM)
    }

    fn geometry_type(&self) -> Option<GeometryType> {
        self.geometry.as_deref().map(|g| match g.to_lowercase().as_str() {
            "point" => GeometryType::Point,
            "line" => GeometryType::Line,
            "polygon" => GeometryType::Polygon,
            _ => GeometryType::Unknown,
        })
    }
}

fn parse_geometry(geometry: &str) -> geo_types::Geometry<f64> {
    let wkt = match geometry.to_lowercase().trim() {
        "point" => "POINT (0 0)",
        "line" => "LINESTRING (0 0, 1 1)",
        "polygon" => "POLYGON ((0 0, 1 0, 1 1, 0 1, 0 0))",
        _ => geometry,
    };
    read_wkt(wkt).unwrap_or_else(|e| panic!("Bad geometry: {geometry}: {e}"))
}

fn values_equal(expected: &Value, actual: &Value) -> bool {
    expected == actual || (expected.is_number() && actual.is_number() && expected.to_string() == actual.to_string())
}

fn check(field: String, issues: &mut Vec<String>, expected: Option<&Value>, actual: Option<&Value>, ignore_null: bool) {
    let expected = expected.filter(|e| !e.is_null());
    let actual = actual.filter(|a| !a.is_null());
    if ignore_null && expected.is_none() {
        return;
    }
    let equal = match (expected, actual) {
        (None, None) => true,
        (Some(e), Some(a)) => values_equal(e, a),
        _ => false,
    };
    if !equal {
        let fmt = |v: Option<&Value>| match v {
            None => "null".to_string(),
            Some(Value::String(s)) => format!("{s:?}"),
            Some(other) => other.to_string(),
        };
        issues.push(format!("{field}: expected <{}> actual <{}>", fmt(expected), fmt(actual)));
    }
}

struct Candidate<'f, 'a> {
    feature: &'f Feature<'a>,
    attrs: Tags,
}

fn proximity(expected: &OutputFeature, item: &Candidate<'_, '_>) -> i32 {
    let mut result = 0;
    if expected.layer.as_deref() == Some(item.feature.get_layer()) {
        result += 2;
    }
    if expected.geometry_type() == Some(GeometryType::type_of(item.feature.get_geometry())) {
        result += 1;
    }
    for (k, v) in &expected.tags {
        if let Some(actual) = item.attrs.get(k) {
            result += 1;
            if actual == v {
                result += 1;
            }
        }
    }
    result
}

/// Port of `BaseSchemaValidator.validate(profile, specification, config)`: returns `(example name, issues)` for
/// every example.
fn validate(profile: &dyn Profile, spec: &Spec, config: Arc<PlanetilerConfig>) -> Vec<(String, Vec<String>)> {
    let factory = Factory::new(config, in_memory());
    spec.examples
        .iter()
        .map(|example| {
            let mut issues = Vec::new();
            let input = &example.input;
            let geometry = parse_geometry(&input.geometry);
            let tags: Tags = input.tags.iter().map(|(k, v)| (k.clone(), Value::str(v.to_string()))).collect();
            let feature = SimpleFeature::create(geometry, tags, input.source.clone(), None::<&str>, 0);
            let mut collector = factory.get(&feature);
            profile.process_feature(&feature, &mut collector);
            let features: Vec<&Feature<'_>> = collector.iter().collect();
            let mut zooms: Vec<u32> = example.output.iter().map(OutputFeature::at_zoom).collect();
            zooms.sort();
            zooms.dedup();
            let mut result: Vec<Candidate<'_, '_>> = Vec::new();
            for f in &features {
                if f.has_linear_ranges() {
                    for zoom in &zooms {
                        for range in f.get_linear_ranges_at_zoom(*zoom) {
                            result.push(Candidate { feature: f, attrs: (*range.attrs).clone() });
                        }
                    }
                } else {
                    result.push(Candidate { feature: f, attrs: Tags::default() });
                }
            }
            if result.len() != example.output.len() {
                issues.push(format!(
                    "Different number of elements, expected={} actual={}",
                    example.output.len(),
                    result.len()
                ));
                return (example.name.clone(), issues);
            }
            for (i, expected) in example.output.iter().enumerate() {
                for candidate in result.iter_mut() {
                    if !candidate.feature.has_linear_ranges() {
                        candidate.attrs = (**candidate.feature.get_attrs_at_zoom(expected.at_zoom())).clone();
                    }
                }
                let best = result
                    .iter()
                    .enumerate()
                    .max_by_key(|(idx, c)| (proximity(expected, c), std::cmp::Reverse(*idx)))
                    .map(|(idx, _)| idx)
                    .unwrap();
                let actual = result.remove(best);
                let prefix = format!("feature[{i}]");
                let f = actual.feature;
                check(
                    format!("{prefix}.layer"),
                    &mut issues,
                    expected.layer.as_ref().map(Value::str).as_ref(),
                    Some(&Value::str(f.get_layer())),
                    true,
                );
                check(
                    format!("{prefix}.minzoom"),
                    &mut issues,
                    expected.min_zoom.map(Value::from).as_ref(),
                    Some(&Value::from(f.get_min_zoom())),
                    true,
                );
                check(
                    format!("{prefix}.maxzoom"),
                    &mut issues,
                    expected.max_zoom.map(Value::from).as_ref(),
                    Some(&Value::from(f.get_max_zoom())),
                    true,
                );
                check(
                    format!("{prefix}.minsize"),
                    &mut issues,
                    expected.min_size.map(Value::from).as_ref(),
                    Some(&Value::from(f.get_min_pixel_size_at_zoom(expected.at_zoom()))),
                    true,
                );
                let geom_name = |g: GeometryType| Value::str(format!("{g:?}"));
                check(
                    format!("{prefix}.geometry"),
                    &mut issues,
                    expected.geometry_type().map(geom_name).as_ref(),
                    Some(&geom_name(GeometryType::type_of(f.get_geometry()))),
                    true,
                );
                let mut remaining: Vec<&String> = actual.attrs.keys().collect();
                remaining.sort();
                for (tag, value) in &expected.tags {
                    check(format!("{prefix}.tags[\"{tag}\"]"), &mut issues, Some(value), actual.attrs.get(tag), false);
                    remaining.retain(|t| *t != tag);
                }
                if expected.allow_extra_tags == Some(false) {
                    for tag in remaining {
                        check(format!("{prefix}.tags[\"{tag}\"]"), &mut issues, None, actual.attrs.get(tag), false);
                    }
                }
            }
            (example.name.clone(), issues)
        })
        .collect()
}

fn validate_schema(schema: SchemaConfig, spec: &Spec) -> Vec<(String, Vec<String>)> {
    let root = build_root_context(&Arguments::of().silence(), schema.args()).unwrap();
    let config = root.config().clone();
    let profile = ConfiguredProfile::new(schema, root).unwrap();
    validate(&profile, spec, config)
}

fn assert_all_pass(results: &[(String, Vec<String>)]) {
    let failures: Vec<String> = results
        .iter()
        .filter(|(_, issues)| !issues.is_empty())
        .map(|(name, issues)| format!("FAIL {name}\n  {}", issues.join("\n  ")))
        .collect();
    assert!(failures.is_empty(), "{} of {} failed:\n{}", failures.len(), results.len(), failures.join("\n"));
}

// ---------------------------------------------------------------------------------------------------------------
// SchemaTests
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn shortbread() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let base = manifest_path("resources/samples");
    let schema = SchemaConfig::load(&base.join("shortbread.yml")).unwrap();
    let spec: Spec = yaml::load_file(&base.join("shortbread.spec.yml")).unwrap();
    let results = validate_schema(schema.clone(), &spec);
    assert!(results.len() > 50);
    assert_all_pass(&results);

    // make sure the validator isn't vacuous
    let mut bad_spec: Spec = yaml::load_file(&base.join("shortbread.spec.yml")).unwrap();
    bad_spec.examples[1].output[1].tags.insert("name".into(), Value::str("wrong"));
    let results = validate_schema(schema, &bad_spec);
    assert!(!results[1].1.is_empty());
    assert!(results[0].1.is_empty());
}

// ---------------------------------------------------------------------------------------------------------------
// SchemaValidatorTest
// ---------------------------------------------------------------------------------------------------------------

fn indent(s: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    s.lines().map(|l| format!("{pad}{l}")).collect::<Vec<_>>().join("\n")
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
            "examples:\n- name: test output\n  input:\n    source: osm\n    geometry: polygon\n    tags:\n      natural: water\n  output:\n    layer: {layer}\n    geometry: {geometry}\n    {}\n    tags:\n{}\n",
            extra.unwrap_or(""),
            indent(tags.unwrap_or(""), 6)
        );
        let spec: Spec = yaml::load(&spec).unwrap();
        let results = validate_schema(SchemaConfig::load_str(schema).unwrap(), &spec);
        assert_eq!(1, results.len());
        assert_eq!("test output", results[0].0);
        assert_eq!(*should_be_ok, results[0].1.is_empty(), "{layer} {geometry} {tags:?} {extra:?}: {:?}", results[0].1);
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
    let spec: Spec = yaml::load(&spec).unwrap();
    assert_all_pass(&validate_schema(SchemaConfig::load_str(schema).unwrap(), &spec));
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
    let inputs = [
        ("line", tags! {"highway" => "primary", "name" => "Main St", "ref" => "A1;B2", "surface" => "asphalt"}),
        ("point", tags! {"amenity" => "cafe", "name" => "Cafe"}),
        ("polygon", tags! {"building" => "yes", "addr:housenumber" => "12"}),
        ("point", tags! {"natural" => "tree"}),
        ("line", tags! {"footway" => "sidewalk"}),
        ("point", tags! {"place" => "city", "name" => "City", "population" => "100000"}),
    ];
    let features: Vec<SimpleFeature> =
        inputs.into_iter().map(|(g, t)| SimpleFeature::create(parse_geometry(g), t, "osm", None::<&str>, 1)).collect();
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
