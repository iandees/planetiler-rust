//! Port of `ConfiguredFeatureTest.java`.

use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use geo_types::Geometry;
use planetiler_core::config::arguments::Arguments;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::{Factory, Feature};
use planetiler_core::geo::geo_utils;
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::geo::geometry_type::GeometryType;
use planetiler_core::geo::test_utils::{new_line_string, new_point, new_polygon, rectangle};
use planetiler_core::geo::with_geometry::{GeometryCache, WithGeometry};
use planetiler_core::profile::Profile;
use planetiler_core::reader::osm::osm_element::{Info, OsmElement, Way};
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::reader::source_feature::{SourceFeature, SourceFeatureBase};
use planetiler_core::reader::with_geometry_type::WithGeometryType;
use planetiler_core::stats::stats::in_memory;
use planetiler_core::value::{Tags, Value};
use planetiler_core::{impl_source_feature_traits, tags, vector_tile};
use planetiler_custommap::configschema::{DataSourceType, MergeLineStrings, MergePolygons, PostProcess, SchemaConfig};
use planetiler_custommap::configured_profile::ConfiguredProfile;
use planetiler_custommap::contexts::build_root_context;
use planetiler_custommap::source::Source;

fn resource(dir: &str, file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(dir).join(file)
}

fn test_resource(file: &str) -> PathBuf {
    resource("tests/resources/validSchema", file)
}

fn sample_resource(file: &str) -> PathBuf {
    resource("resources/samples", file)
}

fn test_invalid_resource(file: &str) -> PathBuf {
    resource("tests/resources/invalidSchema", file)
}

fn osm_info() -> Info {
    Info::new(2, 3, 4, 5, "user")
}

fn water_tags() -> Tags {
    tags! {"natural" => "water", "water" => "pond", "name" => "Little Pond", "test_zoom_tag" => "test_zoom_value"}
}

fn motorway_tags() -> Tags {
    tags! {"highway" => "motorway", "layer" => "1", "bridge" => "yes", "tunnel" => "yes"}
}

fn trunk_tags() -> Tags {
    tags! {"highway" => "trunk", "toll" => "yes"}
}

fn primary_tags() -> Tags {
    tags! {"highway" => "primary", "lanes" => "2"}
}

fn highway_area_tags() -> Tags {
    tags! {"area:highway" => "motorway", "layer" => "1", "bridge" => "yes", "surface" => "asphalt"}
}

fn input_mapping_tags() -> Tags {
    tags! {
        "s_type" => "string_val",
        "l_type" => "1",
        "i_type" => "1",
        "double_type" => "1.5",
        "b_type" => "yes",
        "d_type" => "yes",
        "intermittent" => "yes",
        "bridge" => "yes"
    }
}

const OSM_SOURCE: &str = r#"sources:
  osm:
    type: osm
    url: geofabrik:rhode-island
    local_path: data/rhode-island.osm.pbf
"#;

/// Test harness holding the planetiler config that arguments are read from (Java's `planetilerConfig` field).
struct Harness {
    arguments: Arguments,
    config: Arc<PlanetilerConfig>,
}

impl Harness {
    fn new() -> Harness {
        Harness::with_args(Arguments::of())
    }

    fn with_args(arguments: Arguments) -> Harness {
        let config = Arc::new(PlanetilerConfig::from(&arguments).unwrap());
        Harness { arguments, config }
    }

    fn load_schema(&mut self, schema: SchemaConfig) -> ConfiguredProfile {
        let root = build_root_context(&self.arguments, schema.args()).unwrap();
        self.config = root.config().clone();
        ConfiguredProfile::new(schema, root).unwrap()
    }

    fn load_config(&mut self, config: &str) -> ConfiguredProfile {
        self.load_schema(SchemaConfig::load_str(config).unwrap())
    }

    fn load_file(&mut self, path: PathBuf) -> ConfiguredProfile {
        self.load_schema(SchemaConfig::load(&path).unwrap())
    }

    fn run(&self, profile: &ConfiguredProfile, sf: &dyn SourceFeature, test: impl Fn(&Feature<'_>), expected: usize) {
        let factory = Factory::new(self.config.clone(), in_memory());
        let mut fc = factory.get(sf);
        profile.process_feature(sf, &mut fc);
        let mut length = 0;
        for f in fc.iter() {
            test(f);
            length += 1;
        }
        assert_eq!(expected, length, "Wrong number of features generated");
    }

    fn test_feature(&mut self, config: &str, sf: &dyn SourceFeature, test: impl Fn(&Feature<'_>), expected: usize) {
        let profile = self.load_config(config);
        self.run(&profile, sf, test, expected);
    }

    fn test_file(&mut self, path: PathBuf, sf: &dyn SourceFeature, test: impl Fn(&Feature<'_>), expected: usize) {
        let profile = self.load_file(path);
        self.run(&profile, sf, test, expected);
    }
}

fn polygon_feature(tags: Tags) -> SimpleFeature {
    SimpleFeature::create_fake_osm_feature(
        new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0]),
        tags,
        "osm",
        None::<&str>,
        1,
        vec![],
        Some(osm_info()),
    )
}

fn point_feature(tags: Tags) -> SimpleFeature {
    SimpleFeature::create_fake_osm_feature(new_point(0.0, 0.0), tags, "osm", None::<&str>, 1, vec![], Some(osm_info()))
}

fn line_feature(tags: Tags) -> SimpleFeature {
    SimpleFeature::create_fake_osm_feature(
        new_line_string(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0]),
        tags,
        "osm",
        None::<&str>,
        1,
        vec![],
        Some(osm_info()),
    )
}

fn test_polygon(config: &str, tags: Tags, test: impl Fn(&Feature<'_>), expected: usize) {
    Harness::new().test_feature(config, &polygon_feature(tags), test, expected);
}

fn test_point(config: &str, tags: Tags, test: impl Fn(&Feature<'_>), expected: usize) {
    Harness::new().test_feature(config, &point_feature(tags), test, expected);
}

fn test_linestring(config: &str, tags: Tags, test: impl Fn(&Feature<'_>), expected: usize) {
    Harness::new().test_feature(config, &line_feature(tags), test, expected);
}

fn test_polygon_file(path: PathBuf, tags: Tags, test: impl Fn(&Feature<'_>), expected: usize) {
    Harness::new().test_file(path, &polygon_feature(tags), test, expected);
}

fn test_linestring_file(path: PathBuf, tags: Tags, test: impl Fn(&Feature<'_>), expected: usize) {
    Harness::new().test_file(path, &line_feature(tags), test, expected);
}

fn attr(f: &Feature<'_>, zoom: u32, key: &str) -> Option<Value> {
    f.get_attrs_at_zoom(zoom).get(key).cloned()
}

fn layer_config(features: &str) -> String {
    format!("{OSM_SOURCE}layers:\n- id: testLayer\n  features:\n{features}")
}

fn indent(s: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    s.lines().map(|l| format!("{pad}{l}")).collect::<Vec<_>>().join("\n")
}

fn assert_attrs(expected: Tags, actual: &Tags) {
    let mut a: Vec<_> = actual.iter().collect();
    let mut e: Vec<_> = expected.iter().collect();
    a.sort_by(|x, y| x.0.cmp(y.0));
    e.sort_by(|x, y| x.0.cmp(y.0));
    assert_eq!(e, a);
}

fn point_tile_feature() -> vector_tile::Feature {
    vector_tile::Feature::new(
        "testLayer",
        1,
        vector_tile::encode_geometry(&Geometry::Point(geo_utils::point(0.0, 0.0))),
        Tags::default(),
    )
}

#[test]
fn test_feature_post_processor_noop() {
    let profile = Harness::new().load_config(&layer_config("  - source: osm\n    geometry: point\n"));
    let mut items = vec![point_tile_feature()];
    profile.post_process_layer_features("testLayer", 0, &mut items).unwrap();
    assert_eq!(vec![point_tile_feature()], items);
}

#[test]
fn test_feature_post_processor_merge_line_strings() {
    let profile = Harness::new().load_config(&format!(
        "{}\n  tile_post_process:\n    merge_line_strings:\n      min_length: 1\n      tolerance: 5\n      buffer: 10\n",
        layer_config("  - source: osm\n    geometry: point")
    ));
    let mut items = vec![point_tile_feature()];
    profile.post_process_layer_features("testLayer", 0, &mut items).unwrap();
    assert_eq!(vec![point_tile_feature()], items);
}

#[test]
fn test_merge_line_string_min_length_sets_buffer_pixels() {
    test_linestring(
        &format!(
            "{}\n  tile_post_process:\n    merge_line_strings:\n      min_length: 10\n      min_length_at_max_zoom: 2\n      tolerance: 5\n      buffer: 6\n",
            layer_config("  - source: osm\n    geometry: line")
        ),
        Tags::default(),
        |f| {
            assert_eq!(10.0, f.get_buffer_pixels_at_zoom(12));
            assert_eq!(6.0, f.get_buffer_pixels_at_zoom(14));
        },
        1,
    );
}

#[test]
fn test_feature_post_processor_merge_polygons() {
    let profile = Harness::new().load_config(&format!(
        "{}\n  tile_post_process:\n    merge_polygons:\n      min_area: 3\n",
        layer_config("  - source: osm\n    geometry: point")
    ));
    let mut items = vec![point_tile_feature()];
    profile.post_process_layer_features("testLayer", 0, &mut items).unwrap();
    assert_eq!(vec![point_tile_feature()], items);
}

#[test]
fn test_static_attribute_test() {
    test_polygon_file(
        test_resource("static_attribute.yml"),
        water_tags(),
        |f| assert_eq!(Some(Value::str("aTestConstantValue")), attr(f, 14, "natural")),
        1,
    );
}

#[test]
fn test_tag_value_attribute_test() {
    test_polygon_file(
        test_resource("tag_attribute.yml"),
        water_tags(),
        |f| assert_eq!(Some(Value::str("water")), attr(f, 14, "natural")),
        1,
    );
}

#[test]
fn test_tag_null_value_attribute_test() {
    test_polygon_file(
        test_resource("tag_attribute_null.yml"),
        water_tags(),
        |f| {
            assert_eq!(None, attr(f, 14, "non_existent"));
            assert_eq!(None, attr(f, 14, "non_existent_typed"));
        },
        1,
    );
}

#[test]
fn test_tag_include_attribute_test() {
    test_polygon_file(
        test_resource("tag_include.yml"),
        water_tags(),
        |f| {
            assert_eq!(Some(Value::str("ok")), attr(f, 14, "test_include"));
            assert!(!f.get_attrs_at_zoom(14).contains_key("test_exclude"));
        },
        1,
    );
}

#[test]
fn test_zoom_attribute_test() {
    test_polygon_file(
        test_resource("tag_include.yml"),
        water_tags(),
        |f| {
            assert_eq!(Some(Value::str("test_zoom_value")), attr(f, 14, "test_zoom_tag"));
            assert_ne!(Some(Value::str("test_zoom_value")), attr(f, 11, "test_zoom_tag"));
            assert_ne!(Some(Value::str("test_zoom_value")), attr(f, 9, "test_zoom_tag"));
        },
        1,
    );
}

#[test]
fn test_tag_highway_linestring_test() {
    test_linestring_file(
        test_resource("road_motorway.yml"),
        motorway_tags(),
        |f| assert_eq!(Some(Value::str("motorway")), attr(f, 14, "highway")),
        1,
    );
}

#[test]
fn test_tag_type_conversion_test() {
    test_linestring_file(
        test_resource("road_motorway.yml"),
        motorway_tags(),
        |f| {
            let attrs = f.get_attrs_at_zoom(14);
            assert!(attrs.contains_key("layer"), "Produce attribute layer");
            assert!(attrs.contains_key("bridge"), "Produce attribute bridge");
            assert!(attrs.contains_key("tunnel"), "Produce attribute tunnel");
            assert_eq!(Some(&Value::Int(1)), attrs.get("layer"), "Extract layer as LONG");
            assert_eq!(Some(&Value::Bool(true)), attrs.get("bridge"), "Extract bridge as tagValue BOOLEAN");
            assert_eq!(Some(&Value::Bool(true)), attrs.get("tunnel"), "Extract tunnel as constantValue BOOLEAN");
        },
        1,
    );
}

#[test]
fn test_zoom_filter_attribute_test() {
    test_linestring_file(
        test_resource("road_motorway.yml"),
        motorway_tags(),
        |f| {
            assert!(f.get_attrs_at_zoom(14).contains_key("bridge"), "Produce attribute bridge at z14");
            assert!(!f.get_attrs_at_zoom(10).contains_key("bridge"), "Don't produce attribute bridge at z10");
        },
        1,
    );
}

#[test]
fn test_zoom_filter_conditional_test() {
    test_linestring_file(
        test_resource("zoom_filter.yml"),
        motorway_tags(),
        |f| assert_eq!(Some(Value::str("motorway")), attr(f, 4, "highway"), "Produce attribute highway at z4"),
        1,
    );
    test_linestring_file(
        test_resource("zoom_filter.yml"),
        trunk_tags(),
        |f| {
            assert_eq!(5, f.get_min_zoom());
            assert_eq!(Some(Value::str("trunk")), attr(f, 5, "highway"), "Produce highway=trunk at z5");
            assert_eq!(None, attr(f, 5, "toll"), "Skip toll at z5");
            assert_eq!(Some(Value::str("trunk")), attr(f, 6, "highway"), "Produce highway=trunk at z6");
            assert_eq!(Some(Value::str("yes")), attr(f, 8, "toll"), "render toll at z8");
        },
        1,
    );
    test_linestring_file(
        test_resource("zoom_filter.yml"),
        primary_tags(),
        |f| {
            assert_eq!(None, attr(f, 6, "highway"), "Skip highway=primary at z6");
            assert_eq!(None, attr(f, 6, "lanes"));
            assert_eq!(Some(Value::str("primary")), attr(f, 7, "highway"), "Produce highway=primary at z7");
            assert_eq!(None, attr(f, 7, "lanes"));
            assert_eq!(Some(Value::str("primary")), attr(f, 12, "highway"), "Produce highway=primary at z12");
            assert_eq!(Some(Value::Int(2)), attr(f, 12, "lanes"));
        },
        1,
    );
}

#[test]
fn test_all_values_in_key() {
    // Show that a key in includeWhen with no values matches all values
    test_polygon_file(
        sample_resource("highway_areas.yml"),
        highway_area_tags(),
        |f| {
            assert_eq!(Some(Value::Bool(true)), attr(f, 14, "bridge"), "Produce bridge attribute");
            assert_eq!(Some(Value::str("motorway")), attr(f, 14, "highway"), "Produce highway area attribute");
            assert_eq!(Some(Value::str("asphalt")), attr(f, 14, "surface"), "Produce surface attribute");
            assert_eq!(Some(Value::Int(1)), attr(f, 14, "layer"), "Produce layer attribute");
        },
        1,
    );
}

#[test]
fn test_input_mapping() {
    test_linestring_file(
        test_resource("data_type_attributes.yml"),
        input_mapping_tags(),
        |f| {
            assert_eq!(Some(Value::Bool(true)), attr(f, 14, "b_type"), "Produce boolean");
            assert_eq!(Some(Value::str("string_val")), attr(f, 14, "s_type"), "Produce string");
            assert_eq!(Some(Value::Int(1)), attr(f, 14, "d_type"), "Produce direction");
            assert_eq!(Some(Value::Int(1)), attr(f, 14, "l_type"), "Produce long");
            assert_eq!(Some(Value::Int(1)), attr(f, 14, "i_type"), "Produce integer");
            assert_eq!(Some(Value::Double(1.5)), attr(f, 14, "double_type"), "Produce double");
            assert_eq!(Some(Value::str("yes")), attr(f, 14, "intermittent"), "Produce raw attribute");
            assert_eq!(Some(Value::Bool(true)), attr(f, 14, "is_intermittent"), "Produce and rename boolean");
            assert_eq!(Some(Value::Bool(true)), attr(f, 14, "bridge"), "Produce boolean from full structure");
        },
        1,
    );
}

#[test]
fn test_match_any() {
    for filter in ["natural:", "natural: [__any__]", "natural: __any__"] {
        test_polygon(
            &layer_config(&format!("  - source: osm\n    geometry: polygon\n    include_when:\n      {filter}\n")),
            tags! {"natural" => "water"},
            |_| {},
            1,
        );
    }
}

#[test]
fn test_exclude_value() {
    let config = layer_config(
        "  - source: osm\n    geometry: polygon\n    include_when:\n      natural: water\n    exclude_when:\n      name: excluded\n",
    );
    test_polygon(&config, tags! {"natural" => "water", "name" => "name"}, |_| {}, 1);
    test_polygon(&config, tags! {"natural" => "water", "name" => "excluded"}, |_| {}, 0);
}

#[test]
fn test_require_value() {
    for match_string in ["''", "['']", "[null]"] {
        let config = layer_config(&format!(
            "  - source: osm\n    geometry: polygon\n    include_when:\n      natural: water\n    exclude_when:\n      name: {match_string}\n"
        ));
        test_polygon(&config, tags! {"natural" => "water", "name" => "name"}, |_| {}, 1);
        test_polygon(&config, tags! {"natural" => "water"}, |_| {}, 0);
        test_polygon(&config, tags! {"natural" => "water", "name" => ""}, |_| {}, 0);
    }
}

#[test]
fn test_mapping_key_value() {
    test_polygon(
        &layer_config(
            "  - source: osm\n    geometry: polygon\n    include_when:\n      natural: water\n    attributes:\n    - key: key\n      type: match_key\n    - key: value\n      type: match_value\n",
        ),
        tags! {"natural" => "water"},
        |f| assert_attrs(tags! {"key" => "natural", "value" => "water"}, f.get_attrs_at_zoom(14)),
        1,
    );
}

#[test]
fn test_coerce_attribute_value() {
    test_polygon(
        &layer_config(
            "  - source: osm\n    geometry: polygon\n    attributes:\n    - key: int\n      type: integer\n    - key: long\n      type: long\n    - key: double\n      type: double\n",
        ),
        tags! {"int" => "1", "long" => "-1", "double" => "1.5"},
        |f| assert_attrs(tags! {"int" => 1, "long" => -1, "double" => 1.5}, f.get_attrs_at_zoom(14)),
        1,
    );
}

#[test]
fn test_expression_value() {
    let cases: &[(&str, &str)] = &[
        ("1", "1"),
        ("1+1", "1+1"),
        ("${1+1}", "2"),
        ("${match_key + '=' + match_value}", "natural=water"),
        ("${match_value.replace('ter', 'wa')}", "wawa"),
        ("${feature.tags.natural}", "water"),
        ("${feature.id}", "1"),
        ("\\${feature.id}", "${feature.id}"),
        ("\\\\${feature.id}", "\\${feature.id}"),
        ("${feature.source}", "osm"),
        ("${feature.source_layer}", "null"),
        ("${feature.osm_changeset}", "2"),
        ("${feature.osm_timestamp}", "3"),
        ("${feature.osm_user_id}", "4"),
        ("${feature.osm_version}", "5"),
        ("${feature.osm_user_name}", "user"),
        ("${feature.osm_type}", "node"),
        ("${feature.osm_type.charAt(0)}", "n"),
        ("${coalesce(feature.source_layer, 'missing')}", "missing"),
        ("{match: {test: {natural: water}}}", "test"),
        ("{match: {test: {natural: not_water}}}", "null"),
    ];
    for (expression, value) in cases {
        test_point(
            &layer_config(&format!(
                "  - source: osm\n    geometry: point\n    include_when:\n      natural: water\n    attributes:\n    - key: key\n      value: {expression}\n"
            )),
            tags! {"natural" => "water"},
            |f| {
                let result = attr(f, 14, "key");
                let result_string = result.map_or("null".to_string(), |r| r.to_string());
                assert_eq!(*value, result_string, "{expression}");
            },
            1,
        );
    }
}

#[test]
fn test_get_tag() {
    test_point(
        &layer_config(
            "  - source: osm\n    geometry: point\n    include_when:\n      natural: water\n    attributes:\n    - key: key\n      value:\n        tag_value: natural\n    - key: key2\n      value:\n        tag_value: intval\n        type: integer\n",
        ),
        tags! {"natural" => "water", "intval" => "1"},
        |f| {
            assert_eq!(Some(Value::str("water")), attr(f, 14, "key"));
            assert_eq!(Some(Value::Int(1)), attr(f, 14, "key2"));
        },
        1,
    );
}

#[test]
fn test_get_in_expression_uses_tag_mapping() {
    for getter in
        ["", "tag_value: depth", "value: '${feature.tags[\"depth\"]}'", "value: '${feature.tags.get(\"depth\")}'"]
    {
        test_point(
            &format!(
                "{OSM_SOURCE}tag_mappings:\n  depth: long\nlayers:\n- id: testLayer\n  features:\n  - source: osm\n    geometry: point\n    attributes:\n    - key: depth\n      {getter}\n"
            ),
            tags! {"depth" => "35"},
            |f| assert_eq!(Some(Value::Int(35)), attr(f, 14, "depth"), "{getter}"),
            1,
        );
    }
}

#[test]
fn test_attribute_min_zoom_expression() {
    let cases: &[(&str, u32)] = &[
        ("12", 12),
        ("${5+5}", 10),
        ("${match_key.size()}", 7),
        ("${value.size()}", 5),
        ("{default_value: 4, overrides: {3: {natural: water}}}", 3),
        ("{default_value: 4, overrides: {3: {natural: not_water}}}", 4),
    ];
    for (expression, min_zoom) in cases {
        test_point(
            &layer_config(&format!(
                "  - source: osm\n    geometry: point\n    include_when:\n      natural: water\n    attributes:\n    - key: key\n      value: value\n      min_zoom: {expression}\n"
            )),
            tags! {"natural" => "water"},
            |f| {
                assert_eq!(None, attr(f, min_zoom - 1, "key"), "{expression}");
                assert_eq!(Some(Value::str("value")), attr(f, *min_zoom, "key"), "{expression}");
            },
            1,
        );
    }
}

#[test]
fn test_min_zoom_expression() {
    let config = layer_config(
        "  - source: osm\n    geometry: point\n    min_zoom:\n      default_value: 4\n      overrides:\n      - if: '${feature.tags.has(\"a\", \"b\")}'\n        value: 5\n    include_when:\n      natural: water\n",
    );
    test_point(&config, tags! {"natural" => "water"}, |f| assert_eq!(4, f.get_min_zoom()), 1);
    test_point(&config, tags! {"natural" => "water", "a" => "b"}, |f| assert_eq!(5, f.get_min_zoom()), 1);
}

#[test]
fn test_fallback_value() {
    let config = layer_config(
        "  - source: osm\n    geometry: polygon\n    include_when:\n      natural: water\n    attributes:\n    - key: key\n      value: 1\n      include_when:\n        otherkey: value\n      else: 0\n",
    );
    test_polygon(
        &config,
        tags! {"natural" => "water"},
        |f| assert_attrs(tags! {"key" => 0}, f.get_attrs_at_zoom(14)),
        1,
    );
    test_polygon(
        &config,
        tags! {"natural" => "water", "otherkey" => "othervalue"},
        |f| assert_attrs(tags! {"key" => 0}, f.get_attrs_at_zoom(14)),
        1,
    );
    test_polygon(
        &config,
        tags! {"natural" => "water", "otherkey" => "value"},
        |f| assert_attrs(tags! {"key" => 1}, f.get_attrs_at_zoom(14)),
        1,
    );
}

#[test]
fn test_expression_in_match() {
    for filter in
        ["\"${feature.tags.has('natural', 'water')}\"", "{__all__: [\"${feature.tags.has('natural', 'water')}\"]}"]
    {
        let config = layer_config(&format!("  - source: osm\n    geometry: polygon\n    include_when: {filter}\n"));
        test_polygon(&config, tags! {"natural" => "water"}, |_| {}, 1);
        test_polygon(&config, tags! {"natural" => "other"}, |_| {}, 0);
        test_polygon(&config, tags! {}, |_| {}, 0);
    }
}

#[test]
fn test_expression_attr_filter() {
    let config = layer_config(
        "  - source: osm\n    geometry: polygon\n    include_when:\n      natural: water\n      highway: motorway\n    attributes:\n    - key: key\n      value: true\n      include_when: ${ match_value.startsWith(\"wa\") }\n      else: false\n",
    );
    test_polygon(&config, tags! {"natural" => "water"}, |f| assert_eq!(Some(Value::Bool(true)), attr(f, 14, "key")), 1);
    test_polygon(
        &config,
        tags! {"highway" => "motorway"},
        |f| assert_eq!(Some(Value::Bool(false)), attr(f, 14, "key")),
        1,
    );
}

#[test]
fn test_expression_attr_filter_no_matching_key() {
    let config = layer_config(
        "  - source: osm\n    geometry: polygon\n    include_when: ${ feature.tags.has(\"natural\", \"water\") }\n    attributes:\n    - key: key\n      value: true\n      include_when: ${ coalesce(match_value, '').startsWith(\"wa\") }\n      else: false\n",
    );
    test_polygon(
        &config,
        tags! {"natural" => "water"},
        |f| assert_eq!(Some(Value::Bool(false)), attr(f, 14, "key")),
        1,
    );
}

#[test]
fn test_geometry_type_mismatch() {
    // Validate that a schema that filters on lines does not match on a polygon feature
    let sf = SimpleFeature::create_fake_osm_feature(
        new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0]),
        motorway_tags(),
        "osm",
        None::<&str>,
        1,
        vec![],
        None,
    );
    Harness::new().test_file(test_resource("road_motorway.yml"), &sf, |_| {}, 0);
}

#[test]
fn test_source_type_mismatch() {
    // Validate that a schema only matches on the specified data source
    let sf = SimpleFeature::create_fake_osm_feature(
        new_line_string(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0]),
        highway_area_tags(),
        "not_osm",
        None::<&str>,
        1,
        vec![],
        None,
    );
    Harness::new().test_file(sample_resource("highway_areas.yml"), &sf, |_| {}, 0);
}

#[test]
fn test_invalid_schemas() {
    for (file, message) in [
        ("bad_geometry_type.yml", "Profile defined with invalid geometry type"),
        ("no_layers.yml", "Profile defined with no layers"),
        ("invalid_post_process.yml", "Profile defined with invalid post process element"),
    ] {
        let result = SchemaConfig::load(&test_invalid_resource(file)).map_err(|e| e.to_string()).and_then(|schema| {
            let root = build_root_context(&Arguments::of(), schema.args()).map_err(|e| e.to_string())?;
            ConfiguredProfile::new(schema, root).map_err(|e| e.to_string())
        });
        assert!(result.is_err(), "{message}");
    }
}

#[test]
fn test_use_argument_not_defined() {
    for string in ["arg_value: argument", "value: '${ args.argument }'", "value: '${ args[\"argument\"] }'"] {
        let mut harness = Harness::with_args(Arguments::of_pairs([("argument", "value")]));
        let config = layer_config(&format!(
            "  - source: osm\n    geometry: point\n    include_when:\n      natural: water\n    attributes:\n    - key: key\n      {string}\n"
        ));
        harness.test_feature(
            &config,
            &point_feature(tags! {"natural" => "water"}),
            |f| assert_eq!(Some(Value::str("value")), attr(f, 14, "key"), "{string}"),
            1,
        );
    }
}

#[test]
fn test_override_argument() {
    for string in ["arg_value: threads", "value: '${ args.threads }'", "value: '${ args[\"threads\"] }'"] {
        let config = format!(
            "{OSM_SOURCE}args:\n  threads: 2\nlayers:\n- id: testLayer\n  features:\n  - source: osm\n    geometry: point\n    include_when:\n      natural: water\n    attributes:\n    - key: key\n      type: integer\n      {string}\n"
        );
        test_point(&config, tags! {"natural" => "water"}, |f| assert_eq!(Some(Value::Int(2)), attr(f, 14, "key")), 1);
    }
}

#[test]
fn test_define_argument() {
    let mut harness = Harness::with_args(Arguments::of_pairs([
        ("custom_overridden_arg", "test2"),
        ("custom_simple_overridden_int_arg", "3"),
    ]));
    let config = format!(
        r#"{OSM_SOURCE}args:
  custom_int_arg:
    type: integer
    description: test arg out
    default: 12
  custom_boolean_arg:
    type: boolean
    description: test boolean arg out
    default: true
  custom_overridden_arg:
    default: test
  custom_simple_string_arg: value
  custom_simple_int_arg: 1
  custom_simple_double_arg: 1.5
  custom_simple_bool_arg: true
  custom_simple_overridden_int_arg: 2
layers:
- id: testLayer
  features:
  - source: osm
    geometry: point
    include_when:
      natural: water
    attributes:
    - key: int
      value: '${{ args.custom_int_arg }}'
    - key: bool
      value: '${{ args["custom_boolean_arg"] }}'
    - key: overridden
      arg_value: custom_overridden_arg
    - key: custom_simple_string_arg
      arg_value: custom_simple_string_arg
    - key: custom_simple_int_arg
      arg_value: custom_simple_int_arg
    - key: custom_simple_bool_arg
      arg_value: custom_simple_bool_arg
    - key: custom_simple_overridden_int_arg
      arg_value: custom_simple_overridden_int_arg
    - key: custom_simple_double_arg
      arg_value: custom_simple_double_arg
    - key: custom_simple_int_arg_as_string
      arg_value: custom_simple_int_arg
      type: string
"#
    );
    harness.test_feature(
        &config,
        &point_feature(tags! {"natural" => "water"}),
        |f| {
            assert_eq!(Some(Value::Int(12)), attr(f, 14, "int"));
            assert_eq!(Some(Value::Bool(true)), attr(f, 14, "bool"));
            assert_eq!(Some(Value::str("test2")), attr(f, 14, "overridden"));
            assert_eq!(Some(Value::str("value")), attr(f, 14, "custom_simple_string_arg"));
            assert_eq!(Some(Value::Int(1)), attr(f, 14, "custom_simple_int_arg"));
            assert_eq!(Some(Value::str("1")), attr(f, 14, "custom_simple_int_arg_as_string"));
            assert_eq!(Some(Value::Double(1.5)), attr(f, 14, "custom_simple_double_arg"));
            assert_eq!(Some(Value::Bool(true)), attr(f, 14, "custom_simple_bool_arg"));
            assert_eq!(Some(Value::Int(3)), attr(f, 14, "custom_simple_overridden_int_arg"));
        },
        1,
    );
}

#[test]
fn test_define_arguments_using_expressions() {
    let mut harness = Harness::with_args(Arguments::of_pairs([
        ("custom_overridden_arg", "test2"),
        ("custom_simple_overridden_int_arg", "3"),
    ]));
    let config = format!(
        r#"{OSM_SOURCE}args:
  arg1:
    type: long
    description: test arg out
    default: '${{ 2 - 1 }}'
  arg2: '${{ 2 - 1 }}'
  arg3:
    default: '${{ 2 - 1 }}'
  arg4: ${{ args.arg3 + 1 }}
layers:
- id: testLayer
  features:
  - source: osm
    geometry: point
    attributes:
    - key: arg1
      arg_value: arg1
    - key: arg2
      arg_value: arg2
    - key: arg3
      arg_value: arg3
    - key: arg4
      arg_value: arg4
"#
    );
    harness.test_feature(
        &config,
        &point_feature(Tags::default()),
        |f| {
            assert_eq!(Some(Value::Int(1)), attr(f, 14, "arg1"));
            assert_eq!(Some(Value::Int(1)), attr(f, 14, "arg2"));
            assert_eq!(Some(Value::Int(1)), attr(f, 14, "arg3"));
            assert_eq!(Some(Value::Int(2)), attr(f, 14, "arg4"));
        },
        1,
    );
}

#[test]
fn test_use_argument_in_source_url_path() {
    let config = r#"args:
  area: rhode-island
  url: '${ "geofabrik:" + args.area }'
sources:
  osm:
    type: osm
    url: '${ args.url }'
layers:
- id: testLayer
  features:
  - source: osm
    geometry: point
"#;
    let source = |url: &str| Source::new("osm", DataSourceType::Osm, Some(url.to_string()), None, None);
    let mut harness = Harness::with_args(Arguments::of_pairs([("area", "boston")]));
    assert_eq!(vec![source("geofabrik:boston")], harness.load_config(config).sources().unwrap());

    let mut harness = Harness::new();
    assert_eq!(vec![source("geofabrik:rhode-island")], harness.load_config(config).sources().unwrap());
    assert_eq!("geofabrik_rhode_island.osm.pbf", harness.load_config(config).sources().unwrap()[0].default_file_url());

    let mut harness = Harness::with_args(Arguments::of_pairs([("url", "https://example.com/file.osm.pbf")]));
    assert_eq!("example.com_file.osm.pbf", harness.load_config(config).sources().unwrap()[0].default_file_url());
}

#[test]
fn test_set_projection() {
    for (input, out) in [("EPSG:3875", "EPSG:3875"), ("${'EPSG:' + '3875'}", "EPSG:3875")] {
        let config = format!(
            "sources:\n  osm:\n    type: osm\n    url: geofabrik:rhode-island\n    projection: {input}\nlayers:\n- id: testLayer\n  features:\n  - source: osm\n    geometry: point\n"
        );
        assert_eq!(
            vec![Source::new(
                "osm",
                DataSourceType::Osm,
                Some("geofabrik:rhode-island".into()),
                None,
                Some(out.to_string())
            )],
            Harness::new().load_config(&config).sources().unwrap()
        );
    }
}

#[test]
fn set_min_size() {
    for (input, output) in [("10", 10.0), ("${10+1}", 11.0), ("${feature.tags.key}", 9.0)] {
        test_linestring(
            &layer_config(&format!("  - source: osm\n    min_size: {input}\n    geometry: line\n")),
            tags! {"key" => 9},
            |f| assert_eq!(output, f.get_min_pixel_size_at_zoom(11), "{input}"),
            1,
        );
    }
}

#[test]
fn test_schema_empty_post_process() {
    let profile = Harness::new().load_config(&layer_config("  - source: osm\n    geometry: point\n"));
    assert_eq!(None, profile.find_feature_layer("testLayer").unwrap().post_process);
}

fn post_process_of(post_process: &str) -> Option<PostProcess> {
    let config = format!(
        "{}\n  tile_post_process:\n{}\n",
        layer_config("  - source: osm\n    geometry: point"),
        indent(post_process, 4)
    );
    Harness::new().load_config(&config).find_feature_layer("testLayer").unwrap().post_process
}

#[test]
fn test_schema_post_process_with_merge_line_strings() {
    assert_eq!(
        Some(PostProcess {
            merge_line_strings: Some(MergeLineStrings {
                min_length: Some(1.0),
                min_length_at_max_zoom: Some(0.125),
                tolerance: Some(5.0),
                tolerance_at_max_zoom: Some(0.0625),
                buffer: Some(10.0),
            }),
            merge_polygons: None,
        }),
        post_process_of(
            "merge_line_strings:\n  min_length: 1\n  min_length_at_max_zoom: 0.125\n  tolerance: 5\n  tolerance_at_max_zoom: 0.0625\n  buffer: 10"
        )
    );
}

#[test]
fn test_schema_post_process_with_merge_line_strings_defaults() {
    assert_eq!(
        Some(PostProcess {
            merge_line_strings: Some(MergeLineStrings { buffer: Some(10.0), ..Default::default() }),
            merge_polygons: None
        }),
        post_process_of("merge_line_strings:\n  buffer: 10")
    );
}

#[test]
fn test_schema_post_process_merge_polygons_tolerance() {
    assert_eq!(
        Some(PostProcess {
            merge_line_strings: None,
            merge_polygons: Some(MergePolygons {
                tolerance: Some(1.23),
                tolerance_at_max_zoom: Some(0.123),
                ..Default::default()
            }),
        }),
        post_process_of("merge_polygons:\n  tolerance: 1.23\n  tolerance_at_max_zoom: 0.123")
    );
}

#[test]
fn test_schema_post_process_with_merge_polygons() {
    assert_eq!(
        Some(PostProcess {
            merge_line_strings: None,
            merge_polygons: Some(MergePolygons {
                min_area: Some(3.0),
                min_area_at_max_zoom: Some(1.0),
                ..Default::default()
            }),
        }),
        post_process_of("merge_polygons:\n  min_area: 3\n  min_area_at_max_zoom: 1")
    );
}

fn geometry_type(f: &Feature<'_>) -> GeometryType {
    GeometryType::type_of(f.get_geometry())
}

#[test]
fn test_centroid() {
    let config = layer_config("  - source: osm\n    geometry: centroid\n");
    test_polygon(&config, tags! {"natural" => "water"}, |f| assert_eq!(GeometryType::Point, geometry_type(f)), 1);
    test_linestring(&config, tags! {"natural" => "water"}, |f| assert_eq!(GeometryType::Point, geometry_type(f)), 1);
    test_point(&config, tags! {"natural" => "water"}, |f| assert_eq!(GeometryType::Point, geometry_type(f)), 1);
}

#[test]
fn test_line_centroid() {
    for t in ["line_centroid", "point_on_line"] {
        test_linestring(
            &layer_config(&format!("  - source: osm\n    geometry: {t}\n")),
            tags! {"natural" => "water"},
            |f| assert_eq!(GeometryType::Point, geometry_type(f)),
            1,
        );
    }
}

#[test]
fn test_any_geometry() {
    for expression in ["geometry: any", ""] {
        let config = layer_config(&format!("  - source: osm\n    {expression}\n"));
        test_linestring(&config, Tags::default(), |f| assert_eq!(GeometryType::Line, geometry_type(f)), 1);
        test_point(&config, Tags::default(), |f| assert_eq!(GeometryType::Point, geometry_type(f)), 1);
        test_polygon(&config, Tags::default(), |f| assert_eq!(GeometryType::Polygon, geometry_type(f)), 1);
    }
}

#[test]
fn test_any_source() {
    for expression in ["source: []", ""] {
        let config = layer_config(&format!("  - geometry: point\n    {expression}\n"));
        for source in ["osm", "other"] {
            let sf = SimpleFeature::create_fake_osm_feature(
                new_point(0.0, 0.0),
                Tags::default(),
                source,
                None::<&str>,
                1,
                vec![],
                Some(osm_info()),
            );
            Harness::new().test_feature(&config, &sf, |f| assert_eq!(GeometryType::Point, geometry_type(f)), 1);
        }
    }
}

#[test]
fn test_wikidata_parse() {
    let config = layer_config(
        "  - source: osm\n    geometry: point\n    attributes:\n    - key: wikidata\n      value: \"${feature.tags.wikidata != null ? int(feature.tags.wikidata.replace('Q', '')) : 0}\"\n",
    );
    test_point(
        &config,
        tags! {"wikidata" => "Q235"},
        |f| assert_attrs(tags! {"wikidata" => 235}, f.get_attrs_at_zoom(14)),
        1,
    );
    test_point(
        &config,
        tags! {"wikidata" => "235"},
        |f| assert_attrs(tags! {"wikidata" => 235}, f.get_attrs_at_zoom(14)),
        1,
    );
    test_point(&config, tags! {}, |f| assert_attrs(tags! {"wikidata" => 0}, f.get_attrs_at_zoom(14)), 1);
}

fn left_hand_side_features() -> (SimpleFeature, SimpleFeature) {
    let sf_match = SimpleFeature::create_fake_osm_feature(
        rectangle(0.0, 1.0),
        Tags::default(),
        "osm",
        "layer",
        1,
        vec![],
        Some(Info::new(2, 3, 4, 5, "user")),
    );
    let sf_no_match = SimpleFeature::create_fake_osm_feature(
        new_point(0.0, 0.0),
        Tags::default(),
        "osm",
        "other layer",
        2,
        vec![],
        Some(Info::new(6, 7, 8, 9, "other user")),
    );
    (sf_match, sf_no_match)
}

#[test]
fn test_left_hand_side_expression() {
    for match_string in [
        "${feature.id}: 1",
        "${feature.id + 1}: 2",
        "${feature.id}: [1, 3]",
        "${feature.source_layer}: layer",
        "${  feature .  source_layer  }: [layer, layer2]",
        "${feature.osm_changeset}: 2",
        "${feature.osm_version}: 5",
        "${feature.osm_timestamp}: 3",
        "${feature.osm_user_id}: 4",
        "${feature.osm_user_name}: user",
        "${feature.osm_type}: way",
    ] {
        let config = layer_config(&format!("  - source: osm\n    include_when:\n      {match_string}\n"));
        let (sf_match, sf_no_match) = left_hand_side_features();
        let mut harness = Harness::new();
        let profile = harness.load_config(&config);
        harness.run(&profile, &sf_match, |_| {}, 1);
        harness.run(&profile, &sf_no_match, |_| {}, 0);
    }
}

fn match_none_features() -> (SimpleFeature, SimpleFeature) {
    let sf_match = SimpleFeature::create_fake_osm_feature(
        rectangle(0.0, 1.0),
        Tags::default(),
        "osm",
        "layer",
        1,
        vec![],
        Some(Info::new(2, 3, 4, 5, "user")),
    );
    let sf_no_match = SimpleFeature::create_fake_osm_feature(
        new_point(0.0, 0.0),
        Tags::default(),
        "osm",
        None::<&str>,
        2,
        vec![],
        Some(Info::new(6, 7, 8, 9, "")),
    );
    (sf_match, sf_no_match)
}

#[test]
fn test_left_hand_side_expression_match_any() {
    for match_string in [
        "${feature.osm_user_name}: __any__",
        "${feature.osm_user_name}: null",
        "${feature.source_layer}: __any__",
        "${feature.source_layer}: null",
    ] {
        let config = layer_config(&format!("  - source: osm\n    include_when:\n      {match_string}\n"));
        let (sf_match, sf_no_match) = match_none_features();
        let mut harness = Harness::new();
        let profile = harness.load_config(&config);
        harness.run(&profile, &sf_match, |_| {}, 1);
        harness.run(&profile, &sf_no_match, |_| {}, 0);
    }
}

#[test]
fn test_left_hand_side_expression_match_none() {
    for match_string in [
        "${feature.osm_user_name}: ''",
        "${feature.osm_user_name}: ['']",
        "${feature.source_layer}: ''",
        "${feature.source_layer}: ['']",
    ] {
        let config = layer_config(&format!("  - source: osm\n    include_when:\n      {match_string}\n"));
        let (sf_match, sf_no_match) = match_none_features();
        let mut harness = Harness::new();
        let profile = harness.load_config(&config);
        harness.run(&profile, &sf_match, |_| {}, 0);
        harness.run(&profile, &sf_no_match, |_| {}, 1);
    }
}

fn geometry_config(expression: &str) -> String {
    layer_config(&format!("  - source: osm\n    attributes:\n    - key: attr\n      value: ${{{expression}}}\n"))
}

fn assert_close(expected: f64, actual: Option<Value>, expression: &str) {
    let actual = actual.and_then(|v| v.as_f64()).unwrap_or_else(|| panic!("{expression}: no value"));
    assert!((expected - actual).abs() <= (expected / 1e3).abs(), "{expression}: expected {expected} got {actual}");
}

#[test]
fn test_geometry_attributes_line() {
    let cases: &[(&str, f64)] = &[
        ("feature.length('z0 px')", 3.0712E-5),
        ("feature.length('z0 tiles')", 0.007862),
        ("feature.length('m')", 314283.0),
        ("feature.length('km')", 314.28),
        ("feature.length('nm')", 169.7),
        ("feature.length('ft')", 1031114.0),
        ("feature.length('yd')", 343704.0),
        ("feature.length('mi')", 195.287),
        ("feature.bbox.area('mi2')", 19068.0),
        ("feature.centroid.lat", 3.0),
        ("feature.centroid.lon", 2.0),
        ("feature.innermost_point.lat", 3.0),
        ("feature.innermost_point(0.01).lat", 3.0),
        ("feature.line_midpoint.lat", 3.0),
        ("feature.point_along_line(0).lat", 2.0),
        ("feature.point_along_line(1.0).lat", 4.0),
        ("feature.partial_line(0.0, 0.1).centroid.lat", 2.1),
    ];
    for (expression, expected) in cases {
        let sf = SimpleFeature::create_fake_osm_feature(
            new_line_string(&[1.0, 2.0, 3.0, 4.0]),
            Tags::default(),
            "osm",
            "layer",
            1,
            vec![],
            Some(Info::new(2, 3, 4, 5, "user")),
        );
        Harness::new().test_feature(
            &geometry_config(expression),
            &sf,
            |f| assert_close(*expected, attr(f, 14, "attr"), expression),
            1,
        );
    }
}

#[test]
fn test_geometry_attributes_line_boolean() {
    let cases: &[(&str, bool)] = &[
        ("feature.point_along_line(0) == feature.point_along_line(0)", true),
        ("feature.point_along_line(0) == feature.point_along_line(1)", true),
        ("feature.point_along_line(0) == feature.point_along_line(0.5)", false),
    ];
    for (expression, expected) in cases {
        let sf = SimpleFeature::create_fake_osm_feature(
            new_line_string(&[1.0, 2.0, 3.0, 4.0, 1.0, 2.0]),
            Tags::default(),
            "osm",
            "layer",
            1,
            vec![],
            Some(Info::new(2, 3, 4, 5, "user")),
        );
        Harness::new().test_feature(
            &geometry_config(expression),
            &sf,
            |f| assert_eq!(Some(Value::Bool(*expected)), attr(f, 14, "attr"), "{expression}"),
            1,
        );
    }
}

#[test]
fn test_geometry_attributes_area() {
    let cases: &[(&str, f64)] = &[
        ("feature.area('z0 px2')", 1.17743E-10),
        ("feature.area('z0 tiles')", 7.7164E-6),
        ("feature.area('sm')", 1.2364E10),
        ("feature.area('km2')", 12363.0),
        ("feature.area('m2')", 12363000000.0),
        ("feature.area('ft2')", 1.3308E11),
        ("feature.area('a')", 1.23637E8),
        ("feature.area('ac')", 3055141.0),
        ("feature.area('acres')", 3055141.0),
        ("feature.area('ha')", 1236371.0),
        ("feature.area('mi2')", 4773.7),
        ("feature.bbox.area('mi2')", 4773.7),
        ("feature.centroid.lat", 0.5),
        ("feature.centroid.lon", 0.5),
        ("feature.centroid_if_convex.lon", 0.5),
        ("feature.point_on_surface.lat", 0.5),
        ("feature.innermost_point.lat", 0.5),
        ("feature.validated_polygon.area('mi2')", 4773.7),
    ];
    for (expression, expected) in cases {
        let sf = SimpleFeature::create_fake_osm_feature(
            rectangle(0.0, 1.0),
            Tags::default(),
            "osm",
            "layer",
            1,
            vec![],
            Some(Info::new(2, 3, 4, 5, "user")),
        );
        Harness::new().test_feature(
            &geometry_config(expression),
            &sf,
            |f| assert_close(*expected, attr(f, 14, "attr"), expression),
            1,
        );
    }
}

#[test]
fn test_match_ordering() {
    for with_fallback in [false, true] {
        let config = layer_config(&format!(
            r#"  - source: osm
    attributes:
      - key: attr
        value:
        - if: {{natural: tree}}
          value: green
        - if: {{historic: memorial}}
          value: black
        - if: {{tourism: viewpoint}}
          value: green
        - if: ${{{with_fallback}}}
          value: fallback
"#
        ));
        let sf = SimpleFeature::create_fake_osm_feature(
            new_point(0.0, 0.0),
            tags! {"historic" => "memorial", "tourism" => "viewpoint"},
            "osm",
            None::<&str>,
            1,
            vec![],
            Some(osm_info()),
        );
        Harness::new().test_feature(&config, &sf, |f| assert_eq!(Some(Value::str("black")), attr(f, 14, "attr")), 1);
    }
}

fn way(tags: Tags, info: Option<Info>) -> Way {
    let mut way = Way::with_tags(1, tags, vec![1, 2]);
    way.info = info;
    way
}

#[test]
fn test_split_ways() {
    for split in [false, true] {
        let config = layer_config(&format!(
            r#"  - source: osm
    geometry: {}
    include_when:
      highway:
      - motorway
      - footway
    exclude_when:
      footway:
      - sidewalk
    attributes:
      - key: from
        value: ${{feature.osm_from_node_id}}
      - key: to
        value: ${{feature.osm_to_node_id}}
"#,
            if split { "split_line" } else { "line" }
        ));
        let profile = Harness::new().load_config(&config);
        assert!(!profile.split_osm_way_at_intersections(&way(Tags::default(), None)));
        assert!(!profile.split_osm_way_at_intersections(&way(tags! {"waterway" => "river"}, None)));
        assert_eq!(split, profile.split_osm_way_at_intersections(&way(tags! {"highway" => "motorway"}, None)));
        assert_eq!(split, profile.split_osm_way_at_intersections(&way(tags! {"highway" => "footway"}, None)));
        assert!(
            !profile
                .split_osm_way_at_intersections(&way(tags! {"highway" => "footway", "footway" => "sidewalk"}, None))
        );
    }
}

/// A source feature for an OSM way with an empty geometry (Java `FakeWay`).
#[derive(Debug)]
struct FakeWay {
    base: SourceFeatureBase,
    element: OsmElement,
    geometry: Geometry<f64>,
}

impl FakeWay {
    fn new(way: Way) -> FakeWay {
        FakeWay {
            base: SourceFeatureBase::new(way.tags.clone(), Some(Arc::from("osm")), None, vec![], way.id),
            element: OsmElement::Way(way),
            geometry: Geometry::LineString(geo_utils::empty_line()),
        }
    }
}

impl WithGeometryType for FakeWay {
    fn is_point(&self) -> bool {
        false
    }

    fn can_be_polygon(&self) -> bool {
        false
    }

    fn can_be_line(&self) -> bool {
        true
    }
}

impl WithGeometry for FakeWay {
    fn world_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        Ok(&self.geometry)
    }

    fn lat_lon_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        Ok(&self.geometry)
    }

    fn geometry_cache(&self) -> &GeometryCache {
        &self.base.geometry_cache
    }
}

impl SourceFeature for FakeWay {
    fn base(&self) -> &SourceFeatureBase {
        &self.base
    }

    fn original_element(&self) -> Option<&OsmElement> {
        Some(&self.element)
    }

    fn as_any_feature(&self) -> &dyn Any {
        self
    }
}

impl_source_feature_traits!(FakeWay);

#[test]
fn test_from_to_node_id() {
    let config = layer_config(
        r#"  - source: osm
    geometry: line
    include_when:
      highway:
      - motorway
    attributes:
      - key: from
        value: ${feature.osm_from_node_id}
      - key: to
        value: ${feature.osm_to_node_id}
"#,
    );
    let fake = FakeWay::new(way(tags! {"highway" => "motorway"}, None));
    // the fake way has an empty geometry, so check the attributes directly from the profile's feature
    let mut harness = Harness::new();
    let profile = harness.load_config(&config);
    let factory = Factory::new(harness.config.clone(), in_memory());
    let mut fc = factory.get(&fake);
    profile.process_feature(&fake, &mut fc);
    let features = fc.into_features();
    assert_eq!(1, features.len());
    let f = &features[0];
    assert_eq!(Some(Value::Int(1)), attr(f, 14, "from"));
    assert_eq!(Some(Value::Int(2)), attr(f, 14, "to"));
}

#[test]
fn test_split_with_expression() {
    for filter in [
        "${feature.osm_user_name == 'user'}",
        "${feature.tags.key == 'value'}",
        "{ '${feature.osm_user_name}': 'user' }",
        "{ '${feature.tags.key}': 'value' }",
    ] {
        let config = layer_config(&format!("  - source: osm\n    geometry: split_line\n    include_when: {filter}\n"));
        let profile = Harness::new().load_config(&config);
        assert!(
            !profile.split_osm_way_at_intersections(&way(Tags::default(), Some(Info::new(1, 2, 3, 4, "other_user"))))
        );
        assert!(
            profile.split_osm_way_at_intersections(&way(tags! {"key" => "value"}, Some(Info::new(1, 2, 3, 4, "user"))))
        );
    }
}

#[test]
fn test_buffer() {
    test_polygon(
        &format!(
            "{OSM_SOURCE}layers:\n- id: testLayer\n  buffer: 5\n  features:\n  - source: osm\n    geometry: polygon\n"
        ),
        tags! {"int" => "1", "long" => "-1", "double" => "1.5"},
        |f| assert_eq!(5.0, f.get_buffer_pixels_at_zoom(14)),
        1,
    );
}

#[test]
fn test_id() {
    for (input, output) in [("100", 100), ("${ feature.id }", 1)] {
        test_polygon(
            &layer_config(&format!("  - source: osm\n    geometry: polygon\n    id: {input}\n")),
            Tags::default(),
            |f| assert_eq!(output, f.get_id()),
            1,
        );
    }
}

#[test]
fn test_point_label_grid() {
    test_point(
        &format!(
            "{OSM_SOURCE}layers:\n- id: testLayer\n  buffer: 64\n  features:\n  - source: osm\n    geometry: point\n    point_label_grid:\n      pixel_size:\n        maxzoom: 8\n        value: 32\n      limit:\n        maxzoom: 8\n        value: 64\n"
        ),
        Tags::default(),
        |f| {
            assert_eq!(32.0, f.get_point_label_grid_pixel_size_at_zoom(8));
            assert_eq!(0.0, f.get_point_label_grid_pixel_size_at_zoom(9));
            assert_eq!(64, f.get_point_label_grid_limit_at_zoom(8));
            assert_eq!(0, f.get_point_label_grid_limit_at_zoom(9));
        },
        1,
    );
}

#[test]
fn test_sort_key() {
    for (input, output) in [
        ("sort_key: 100", 100),
        ("sort_key: ${ feature.id }", 1),
        ("sort_key_descending: 100", -101),
        ("sort_key_descending: ${ feature.id }", -2),
    ] {
        test_polygon(
            &layer_config(&format!("  - source: osm\n    geometry: polygon\n    {input}\n")),
            Tags::default(),
            |f| assert_eq!(output, f.get_sort_key(), "{input}"),
            1,
        );
    }
}
