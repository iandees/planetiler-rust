//! Ports of `ExpressionTests`, `BooleanExpressionScriptTest`, `ConfigExpressionTest`, `DataTypeTest` and
//! `ContextsTest`.

use planetiler_core::config::arguments::Arguments;
use planetiler_core::expression::data_type::DataType;
use planetiler_core::expression::expression::{
    Expression, and, match_any, match_any_typed, match_source, match_source_layer, or,
};
use planetiler_core::expression::multi_expression::MultiExpression;
use planetiler_core::expression::simplifiable::Simplifiable;
use planetiler_core::geo::test_utils::new_point;
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::tags;
use planetiler_core::value::{Tags, Value};
use planetiler_custommap::boolean_expression_parser;
use planetiler_custommap::contexts::{Root, build_root_context};
use planetiler_custommap::expression::config_expression::{
    cast, coalesce, const_of, get_arg, get_tag, match_entry, match_expr, match_with_fallback, script, signature,
    variable,
};
use planetiler_custommap::expression::{BooleanExpressionScript, ConfigExpressionScript, ScriptEnvironment, Signature};
use planetiler_custommap::tag_value_producer::TagValueProducer;
use planetiler_custommap::type_conversion::OutType;

fn root() -> Root {
    Root::empty()
}

fn root_env() -> ScriptEnvironment {
    root().description()
}

fn process_feature() -> ScriptEnvironment {
    root().process_feature_description()
}

fn feature_signature() -> Signature {
    signature(&process_feature(), OutType::Integer)
}

fn tvp(map: Tags) -> TagValueProducer {
    TagValueProducer::new(Some(&map))
}

// ---------------------------------------------------------------------------------------------------------------
// ExpressionTests
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn test_expression() {
    let cases: &[(&str, &str, &str)] = &[
        ("1", "1", "long"),
        ("1+1", "2", "long"),
        ("'1' + string(1)", "11", "string"),
        ("coalesce(null, null)", "", "null"),
        ("coalesce(null, 1)", "1", "long"),
        ("coalesce(1, null)", "1", "long"),
        ("coalesce(1, 2)", "1", "long"),
        ("coalesce(null, null, 1)", "1", "long"),
        ("coalesce(null, 1, 2)", "1", "long"),
        ("coalesce(1, 2, null)+2", "3", "long"),
        ("nullif('abc', '')", "abc", "string"),
        ("nullif('', '')", "null", "null"),
        ("nullif(1, 1)", "null", "null"),
        ("nullif(1, 12)", "1", "long"),
        ("'123'.replace('12', 'X')", "X3", "string"),
        ("'123'.replaceRegex('1(.)', '$1')", "23", "string"),
        ("string(123)", "123", "string"),
        ("string({1:2,3:'4'}[1])", "2", "string"),
        ("'abc'.matches('a.c')", "true", "boolean"),
        ("'abc'.matches('a.d')", "false", "boolean"),
        ("{'a': 1}.has('a')", "true", "boolean"),
        ("{'a': 1}.has('a', 1)", "true", "boolean"),
        ("{'a': 1}.has('a', 1, 2)", "true", "boolean"),
        ("{'a': 2}.has('a', 1, 2)", "true", "boolean"),
        ("{'a': 2}.has('a', 3)", "false", "boolean"),
        ("{'a': 1}.has('b')", "false", "boolean"),
        ("int({'tags': {'wikidata': 'Q1'}}.tags.wikidata.replace('Q', ''))", "1", "long"),
        ("coalesce({'a': 1}.get('a'), 2)", "1", "long"),
        ("coalesce({'a': 1}.get('b'), 2)", "2", "long"),
        ("{'a': 1}.getOrDefault('a', 2)", "1", "long"),
        ("{'a': 1}.getOrDefault('b', 2)", "2", "long"),
        ("{'a': 'b'}.getOrKeep('a')", "b", "string"),
        ("{'a': 'b'}.getOrKeep('c')", "c", "string"),
        ("max([1, 2, 3])", "3", "long"),
        ("max([1.1, 2.2, 3.3])", "3.3", "double"),
        ("min([1, 2, 3])", "1", "long"),
        ("min([1.1, 2.2, 3.3])", "1.1", "double"),
        ("max([1])", "1", "long"),
        ("min([1])", "1", "long"),
        ("args.arg_from_config", "1", "long"),
        ("args.arg_from_config + 1", "2", "long"),
        ("args[\"arg_from_config\"]", "1", "long"),
        ("args[\"arg_from_config\"] + 1", "2", "long"),
        ("args.overridden_arg_from_config", "4", "long"),
        ("args[\"overridden_arg_from_config\"] + 1", "5", "long"),
        ("args.arg_from_cli", "2", "string"),
        ("args[\"arg_from_cli\"]", "2", "string"),
    ];
    let config_from_schema = tags! {"arg_from_config" => 1, "overridden_arg_from_config" => 3};
    let config_from_cli = Arguments::of_pairs([("arg_from_cli", "2"), ("overridden_arg_from_config", "4")]);
    let context = build_root_context(&config_from_cli, &config_from_schema).unwrap();
    for (input, expected, kind) in cases {
        let expression = ConfigExpressionScript::parse(input, &context.description(), OutType::Object)
            .unwrap_or_else(|e| panic!("{input}: {e}"));
        let result = expression.try_apply(&context.as_context()).unwrap_or_else(|e| panic!("{input}: {e}"));
        let expected_value = match *kind {
            "long" => Value::Int(expected.parse().unwrap()),
            "double" => Value::Double(expected.parse().unwrap()),
            "string" => Value::str(expected),
            "boolean" => Value::Bool(expected.parse().unwrap()),
            "null" => Value::Null,
            _ => panic!("{kind}"),
        };
        assert_eq!(expected_value, result, "{input}");
    }
}

#[test]
fn test_parse_errors() {
    assert!(ConfigExpressionScript::parse("feature.tags.a", &root_env(), OutType::Object).is_err());
    assert!(ConfigExpressionScript::parse("1 +", &root_env(), OutType::Object).is_err());
    assert!(ConfigExpressionScript::parse("missing", &process_feature(), OutType::Object).is_err());
    assert!(ConfigExpressionScript::parse("[1, 2].map(x, x + 1)", &root_env(), OutType::Object).is_ok());
    assert!(ConfigExpressionScript::parse("[1, 2].map(x, x + y)", &root_env(), OutType::Object).is_err());
    assert!(ConfigExpressionScript::parse("feature.tags.a", &process_feature(), OutType::Object).is_ok());
}

#[test]
fn test_comprehensions_and_strings() {
    let feature = SimpleFeature::create(new_point(0.0, 0.0), tags! {"ref" => "A1;B22;C333"}, "osm", None::<&str>, 1);
    let tvp = TagValueProducer::empty();
    let r = root();
    let ctx = r.create_process_feature_context(&feature, tvp);
    let eval = |s: &str| ConfigExpressionScript::parse(s, &process_feature(), OutType::Object).unwrap().try_apply(&ctx);
    assert_eq!(Ok(Value::Int(4)), eval("max(feature.tags[\"ref\"].split(\";\").map(r, size(r)))"));
    assert_eq!(Ok(Value::Int(3)), eval("size(feature.tags[\"ref\"].split(\";\"))"));
    assert_eq!(Ok(Value::str("A1\nB22\nC333")), eval("feature.tags[\"ref\"].replace(\";\", \"\\n\")"));
    assert_eq!(Ok(Value::Bool(true)), eval("has(feature.tags.ref)"));
    assert_eq!(Ok(Value::Bool(false)), eval("has(feature.tags.missing)"));
    assert_eq!(Ok(Value::Bool(true)), eval("'ref' in feature.tags"));
    assert_eq!(Ok(Value::Bool(false)), eval("'missing' in feature.tags"));
    assert_eq!(Ok(Value::Int(1)), eval("size(feature.tags)"));
    assert_eq!(Ok(Value::Bool(true)), eval("feature.tags.exists(k, k == 'ref')"));
    assert_eq!(Ok(Value::Null), eval("feature.tags.missing"));
    assert_eq!(Ok(Value::Null), eval("feature.tags['missing']"));
    assert_eq!(Ok(Value::Bool(true)), eval("feature.tags.missing == null"));
    assert_eq!(Ok(Value::str("osm")), eval("feature.source"));
    assert_eq!(Ok(Value::Null), eval("feature.source_layer"));
    assert_eq!(Ok(Value::str("n")), eval("'node'.charAt(0)"));
}

// ---------------------------------------------------------------------------------------------------------------
// BooleanExpressionScriptTest
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn test_boolean_script_simplify() {
    assert_eq!(
        Expression::TRUE,
        and([or([BooleanExpressionScript::script("1+1<3", &root_env()).unwrap()])]).simplify()
    );
    assert_eq!(
        Expression::FALSE,
        and([or([BooleanExpressionScript::script("1+1>3", &root_env()).unwrap()])]).simplify()
    );
    let other = BooleanExpressionScript::script("feature.tags.natural", &process_feature()).unwrap();
    assert_eq!(other, and([or([other.clone()])]).simplify());
}

#[test]
fn test_boolean_script_simplify_inlines_arguments() {
    assert_eq!(
        Expression::TRUE,
        and([or([BooleanExpressionScript::script("args.threads > 0", &root_env()).unwrap()])]).simplify()
    );
    assert_eq!(
        Expression::FALSE,
        and([or([BooleanExpressionScript::script("args.threads < 0", &root_env()).unwrap()])]).simplify()
    );
}

// ---------------------------------------------------------------------------------------------------------------
// ConfigExpressionTest
// ---------------------------------------------------------------------------------------------------------------

fn bool_script(s: &str) -> Expression {
    BooleanExpressionScript::script(s, &process_feature()).unwrap()
}

#[test]
fn test_const() {
    assert_eq!(Value::Int(1), const_of(1).apply(&root().as_context()));
}

#[test]
fn test_variable() {
    let feature =
        SimpleFeature::create(new_point(0.0, 0.0), tags! {"a" => "b", "c" => 1}, "source", "source_layer", 99);
    let r = root();
    let tvp = tvp(Tags::new());
    let context = r.create_process_feature_context(&feature, &tvp);
    let sig = feature_signature();
    assert_eq!(
        Value::str("source"),
        variable(sig.with_output(OutType::String), "feature.source").unwrap().apply(&context)
    );
    assert_eq!(
        Value::str("source_layer"),
        variable(sig.with_output(OutType::String), "feature.source_layer").unwrap().apply(&context)
    );
    assert_eq!(Value::Int(99), variable(sig.with_output(OutType::Long), "feature.id").unwrap().apply(&context));
    assert_eq!(Value::Int(99), variable(sig.with_output(OutType::Integer), "feature.id").unwrap().apply(&context));
    assert_eq!(Value::Double(99.0), variable(sig.with_output(OutType::Double), "feature.id").unwrap().apply(&context));
    assert!(variable(sig, "missing").is_err());
}

#[test]
fn test_coalesce() {
    let r = root();
    let ctx = r.as_context();
    assert_eq!(Value::Null, coalesce(vec![]).apply(&ctx));
    assert_eq!(Value::Null, coalesce(vec![const_of(Value::Null)]).apply(&ctx));
    assert_eq!(Value::Int(2), coalesce(vec![const_of(Value::Null), const_of(2)]).apply(&ctx));
    assert_eq!(Value::Int(1), coalesce(vec![const_of(1), const_of(2)]).apply(&ctx));
}

#[test]
fn test_dynamic() {
    let sig = signature(&root_env(), OutType::Integer);
    assert_eq!(Value::Int(1), script(sig, "5 - 4").unwrap().apply(&root().as_context()));
}

#[test]
fn test_match() {
    let feature = SimpleFeature::create_tags(new_point(0.0, 0.0), tags! {"a" => "b", "c" => 1});
    let r = root();
    let tvp = tvp(Tags::new());
    let context = r.create_process_feature_context(&feature, &tvp);
    let sig = feature_signature();
    // simple match
    assert_eq!(
        Value::Int(2),
        match_expr(
            sig.clone(),
            MultiExpression::of(vec![
                match_entry(const_of(1), bool_script("feature.tags.has('a', 'c')")),
                match_entry(const_of(2), bool_script("feature.tags.has('a', 'b')")),
            ])
        )
        .apply(&context)
    );
    // dynamic fallback
    assert_eq!(
        Value::Int(5),
        match_with_fallback(
            sig.clone(),
            MultiExpression::of(vec![
                match_entry(const_of(1), bool_script("feature.tags.has('a', 'c')")),
                match_entry(const_of(2), bool_script("feature.tags.has('a', 'd')")),
            ]),
            script(sig.clone(), "feature.tags.c + 4").unwrap(),
        )
        .apply(&context)
    );
    // no fallback
    assert_eq!(
        Value::Null,
        match_expr(
            sig.clone(),
            MultiExpression::of(vec![
                match_entry(const_of(1), bool_script("feature.tags.has('a', 'd')")),
                match_entry(const_of(2), bool_script("feature.tags.has('a', 'e')")),
            ])
        )
        .apply(&context)
    );
    // dynamic value
    assert_eq!(
        Value::Int(2),
        match_expr(
            sig.clone(),
            MultiExpression::of(vec![
                match_entry(script(sig.clone(), "1 + size(feature.tags.a)").unwrap(), match_any("a", ["b"])),
                match_entry(const_of(1), match_any("a", ["c"])),
            ])
        )
        .apply(&context)
    );
}

#[test]
fn test_simplify_cel_function() {
    assert_eq!(const_of(3), script(feature_signature(), "1+2").unwrap().simplify());
}

#[test]
fn test_simplify_cel_function_that_just_accesses_var() {
    assert_eq!(
        variable(feature_signature(), "feature.id").unwrap(),
        script(feature_signature(), "feature.id").unwrap().simplify()
    );
    assert_eq!(
        script(feature_signature(), "feature.tags.a").unwrap(),
        script(feature_signature(), "feature.tags.a").unwrap().simplify()
    );
}

fn parse_bool(map: Tags) -> Expression {
    boolean_expression_parser::parse(Some(&Value::map(map)), TagValueProducer::empty(), &process_feature()).unwrap()
}

#[test]
fn test_cant_simplify_dynamic_left_expression() {
    let sig = feature_signature();
    assert_eq!(
        match_expr(
            sig.clone(),
            MultiExpression::of(vec![match_entry(
                const_of(1),
                match_any_typed(
                    None,
                    script(sig.with_output(OutType::Object), "feature.id + 1").unwrap().into_getter(),
                    [1, 2]
                )
            )])
        ),
        match_expr(
            sig.clone(),
            MultiExpression::of(vec![match_entry(
                const_of(1),
                parse_bool(tags! {"${feature.id + 1}" => Value::list([Value::Int(1), Value::Int(2)])})
            )])
        )
        .simplify()
    );
}

#[test]
fn test_simplify_simple_left_expression() {
    let sig = feature_signature();
    for varname in ["feature.id", "feature.osm_user_name", "feature.osm_type"] {
        assert_eq!(
            match_expr(
                sig.clone(),
                MultiExpression::of(vec![match_entry(
                    const_of(1),
                    match_any_typed(
                        None,
                        variable(sig.with_output(OutType::Object), varname).unwrap().into_getter(),
                        [1, 2]
                    )
                )])
            ),
            match_expr(
                sig.clone(),
                MultiExpression::of(vec![match_entry(
                    const_of(1),
                    parse_bool(tags! {format!("${{{varname}}}") => Value::list([Value::Int(1), Value::Int(2)])})
                )])
            )
            .simplify(),
            "{varname}"
        );
    }
}

#[test]
fn test_simplify_expression_to_match_source() {
    let sig = feature_signature();
    assert_eq!(
        match_expr(
            sig.clone(),
            MultiExpression::of(vec![match_entry(const_of(1), or([match_source("source1"), match_source("source2")]))])
        ),
        match_expr(
            sig.clone(),
            MultiExpression::of(vec![match_entry(
                const_of(1),
                parse_bool(tags! {"${feature.source}" => Value::list([Value::str("source1"), Value::str("source2")])})
            )])
        )
        .simplify()
    );
}

#[test]
fn test_simplify_expression_to_match_source_layer() {
    let sig = feature_signature();
    assert_eq!(
        match_expr(
            sig.clone(),
            MultiExpression::of(vec![match_entry(
                const_of(1),
                or([match_source_layer("layer1"), match_source_layer("layer2")])
            )])
        ),
        match_expr(
            sig.clone(),
            MultiExpression::of(vec![match_entry(
                const_of(1),
                parse_bool(
                    tags! {"${feature.source_layer}" => Value::list([Value::str("layer1"), Value::str("layer2")])}
                )
            )])
        )
        .simplify()
    );
}

#[test]
fn test_dont_simplify_other_source_layer_cases() {
    let sig = feature_signature();
    for (input, out) in [("__any__", vec![]), ("", vec![Value::str("")]), ("prefix%", vec![Value::str("prefix%")])] {
        assert_eq!(
            match_expr(
                sig.clone(),
                MultiExpression::of(vec![match_entry(
                    const_of(1),
                    match_any_typed(
                        None,
                        variable(sig.with_output(OutType::Object), "feature.source_layer").unwrap().into_getter(),
                        out
                    )
                )])
            ),
            match_expr(
                sig.clone(),
                MultiExpression::of(vec![match_entry(
                    const_of(1),
                    parse_bool(tags! {"${feature.source_layer}" => Value::list([Value::str(input)])})
                )])
            )
            .simplify(),
            "{input}"
        );
    }
}

#[test]
fn test_simplify_coalesce() {
    assert_eq!(const_of(Value::Null), coalesce(vec![]).simplify());
    assert_eq!(const_of(Value::Null), coalesce(vec![const_of(Value::Null)]).simplify());
    assert_eq!(const_of(1), coalesce(vec![const_of(1)]).simplify());
    assert_eq!(const_of(1), coalesce(vec![const_of(1), const_of(2)]).simplify());
    assert_eq!(const_of(1), coalesce(vec![const_of(1), const_of(1)]).simplify());
    assert_eq!(const_of(1), coalesce(vec![const_of(1), const_of(2), const_of(1)]).simplify());
}

#[test]
fn test_simplify_match_all_false() {
    assert_eq!(
        const_of(Value::Null),
        match_expr(
            feature_signature(),
            MultiExpression::of(vec![
                match_entry(const_of(1), bool_script("1 > 2")),
                match_entry(const_of(2), bool_script("1 > 3")),
            ])
        )
        .simplify()
    );
}

#[test]
fn test_simplify_match_all_false_with_fallback() {
    assert_eq!(
        const_of(3),
        match_with_fallback(
            feature_signature(),
            MultiExpression::of(vec![
                match_entry(const_of(1), bool_script("1 > 2")),
                match_entry(const_of(2), bool_script("1 > 3")),
            ]),
            script(feature_signature(), "1+2").unwrap()
        )
        .simplify()
    );
}

#[test]
fn test_simplify_remove_cases_after_true_and_replace_fallback() {
    assert_eq!(
        match_with_fallback(
            feature_signature(),
            MultiExpression::of(vec![match_entry(const_of(0), bool_script("feature.tags.has('a', 'b')"))]),
            const_of(1)
        ),
        match_with_fallback(
            feature_signature(),
            MultiExpression::of(vec![
                match_entry(const_of(0), bool_script("feature.tags.has('a', 'b')")),
                match_entry(const_of(1), bool_script("1 < 2")),
                match_entry(const_of(2), bool_script("feature.tags.has('c', 'd')")),
            ]),
            const_of(2)
        )
        .simplify()
    );
}

#[test]
fn test_simplify_remove_false_cases() {
    let fallback = || script(feature_signature(), "size(feature.tags.a)").unwrap();
    assert_eq!(
        match_with_fallback(
            feature_signature(),
            MultiExpression::of(vec![match_entry(const_of(2), bool_script("feature.tags.has('a', 'b')"))]),
            fallback()
        ),
        match_with_fallback(
            feature_signature(),
            MultiExpression::of(vec![
                match_entry(const_of(1), bool_script("1 > 2")),
                match_entry(const_of(2), bool_script("feature.tags.has('a', 'b')")),
            ]),
            fallback()
        )
        .simplify()
    );
}

#[test]
fn test_simplify_match_condition() {
    let fallback = || script(feature_signature(), "size(feature.tags.a)").unwrap();
    assert_eq!(
        match_with_fallback(
            feature_signature(),
            MultiExpression::of(vec![match_entry(const_of(2), match_any("a", ["b"]))]),
            fallback()
        ),
        match_with_fallback(
            feature_signature(),
            MultiExpression::of(vec![match_entry(const_of(2), or([or([match_any("a", ["b"])])]))]),
            fallback()
        )
        .simplify()
    );
}

#[test]
fn test_simplify_match_result_function() {
    let fallback = || script(feature_signature(), "size(feature.tags.a)").unwrap();
    assert_eq!(
        match_with_fallback(
            feature_signature(),
            MultiExpression::of(vec![match_entry(const_of(2), match_any("a", ["b"]))]),
            fallback()
        ),
        match_with_fallback(
            feature_signature(),
            MultiExpression::of(vec![match_entry(coalesce(vec![const_of(2)]), match_any("a", ["b"]))]),
            fallback()
        )
        .simplify()
    );
}

#[test]
fn test_simplify_fallback_function() {
    assert_eq!(
        match_with_fallback(
            feature_signature(),
            MultiExpression::of(vec![match_entry(const_of(2), match_any("a", ["b"]))]),
            const_of(3)
        ),
        match_with_fallback(
            feature_signature(),
            MultiExpression::of(vec![match_entry(const_of(2), match_any("a", ["b"]))]),
            script(feature_signature(), "1+2").unwrap()
        )
        .simplify()
    );
}

#[test]
fn test_simplify_first_true() {
    assert_eq!(
        const_of(1),
        match_expr(
            feature_signature(),
            MultiExpression::of(vec![
                match_entry(const_of(1), bool_script("1 < 2")),
                match_entry(const_of(2), bool_script("1 > 3")),
            ])
        )
        .simplify()
    );
}

#[test]
fn test_get_tag() {
    let feature = SimpleFeature::create(new_point(0.0, 0.0), tags! {"abc" => "123"}, "source", "source_layer", 99);
    let r = root();
    let sig = feature_signature().with_output(OutType::Object);
    let empty = tvp(Tags::new());
    let int_tvp = tvp(tags! {"abc" => "integer"});
    assert_eq!(
        Value::str("123"),
        get_tag(sig.clone(), const_of("abc")).apply(&r.create_process_feature_context(&feature, &empty))
    );
    assert_eq!(
        Value::Int(123),
        get_tag(sig.clone(), const_of("abc")).apply(&r.create_process_feature_context(&feature, &int_tvp))
    );
    let post_match_sig = signature(&r.feature_post_match_description(), OutType::Object);
    assert_eq!(
        Value::Int(123),
        get_tag(post_match_sig, const_of("abc"))
            .apply(&r.create_process_feature_context(&feature, &int_tvp).create_post_match_context(&[]))
    );
    assert_eq!(Value::Null, get_tag(signature(&root_env(), OutType::Object), const_of("abc")).apply(&r.as_context()));
}

#[test]
fn test_cast_get_tag() {
    let feature = SimpleFeature::create(new_point(0.0, 0.0), tags! {"abc" => "123"}, "source", "source_layer", 99);
    let r = root();
    let empty = tvp(Tags::new());
    let context = r.create_process_feature_context(&feature, &empty);
    let sig = feature_signature();
    let expression = cast(
        sig.with_output(OutType::Integer),
        get_tag(sig.with_output(OutType::Object), const_of("abc")),
        DataType::GetInt,
    );
    assert_eq!(Value::Int(123), expression.apply(&context));
    assert_eq!(
        Value::Double(123.0),
        cast(
            sig.with_output(OutType::Double),
            get_tag(sig.with_output(OutType::Object), const_of("abc")),
            DataType::GetInt
        )
        .apply(&context)
    );
}

#[test]
fn test_cast() {
    let expression = cast(signature(&root_env(), OutType::Integer), const_of("123"), DataType::GetInt);
    assert_eq!(Value::Int(123), expression.apply(&root().as_context()));
}

#[test]
fn test_simplify_cast() {
    assert_eq!(
        const_of(123),
        cast(signature(&root_env(), OutType::Integer), const_of("123"), DataType::GetInt).simplify()
    );
}

#[test]
fn test_simplify_get_arg() {
    let args = Arguments::of_pairs([("arg", "12")]);
    let root = build_root_context(&args, &Tags::new()).unwrap();
    let context = signature(&root.description(), OutType::Integer);
    assert_eq!(const_of(12), get_arg(context.clone(), const_of("arg")).simplify());
    assert_eq!(const_of(12), script(context.clone(), "args.arg").unwrap().simplify());
    assert_eq!(const_of(12), script(context, "args['arg']").unwrap().simplify());
}

// ---------------------------------------------------------------------------------------------------------------
// DataTypeTest
// ---------------------------------------------------------------------------------------------------------------

fn convert(id: &str, v: Value) -> Value {
    DataType::from_id(id).convert_from(Some(&v))
}

#[test]
fn test_data_types() {
    assert_eq!(Value::Int(1), convert("long", Value::str("1")));
    assert_eq!(Value::Int(1), convert("long", Value::Int(1)));
    assert_eq!(Value::Null, convert("long", Value::str("garbage")));
    assert_eq!(Value::Int(1), convert("integer", Value::str("1")));
    assert_eq!(Value::Null, convert("integer", Value::str("garbage")));
    assert_eq!(Value::Int(1), convert("integer", Value::str("1.5")));
    assert_eq!(Value::Double(1.5), convert("double", Value::str("1.5")));
    assert_eq!(Value::Double(1.0), convert("double", Value::Int(1)));
    assert_eq!(Value::Null, convert("double", Value::str("garbage")));
    assert_eq!(Value::str("1.5"), convert("string", Value::str("1.5")));
    assert_eq!(Value::str("1.5"), convert("string", Value::Double(1.5)));
    assert_eq!(Value::str("1.5"), convert("raw", Value::str("1.5")));
    assert_eq!(Value::Double(1.5), convert("raw", Value::Double(1.5)));
    for v in [Value::str("1"), Value::str("true"), Value::str("yes"), Value::Int(1)] {
        assert_eq!(Value::Bool(true), convert("boolean", v));
    }
    for v in [Value::Int(0), Value::str("false"), Value::str("no")] {
        assert_eq!(Value::Bool(false), convert("boolean", v));
    }
    for v in [Value::str("1"), Value::Int(1), Value::str("true"), Value::str("yes")] {
        assert_eq!(Value::Int(1), convert("direction", v));
    }
    assert_eq!(Value::Int(-1), convert("direction", Value::Int(-1)));
    assert_eq!(Value::Int(-1), convert("direction", Value::str("-1")));
    for v in [Value::Int(0), Value::str("no"), Value::str("false")] {
        assert_eq!(Value::Int(0), convert("direction", v));
    }
    assert_eq!(DataType::GetDouble, DataType::type_of(&Value::Double(1.5)));
    assert_eq!(DataType::GetLong, DataType::type_of(&Value::Int(1)));
    assert_eq!(DataType::GetBoolean, DataType::type_of(&Value::Bool(true)));
    assert_eq!(DataType::GetString, DataType::type_of(&Value::str("string")));
}

// ---------------------------------------------------------------------------------------------------------------
// ContextsTest
// ---------------------------------------------------------------------------------------------------------------

fn args(pairs: &[(&str, &str)]) -> Arguments {
    Arguments::of_pairs(pairs.iter().copied())
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::map(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

#[test]
fn test_parse_empty_args() {
    build_root_context(&args(&[]), &Tags::new()).unwrap();
}

#[test]
fn set_planetiler_config_through_cli() {
    let result = build_root_context(&args(&[("threads", "9999")]), &tags! {"threads" => "8888"}).unwrap();
    assert_eq!(9999, result.config().threads);
}

#[test]
fn set_planetiler_config_through_schema_args() {
    assert_eq!(8888, build_root_context(&args(&[]), &tags! {"threads" => "8888"}).unwrap().config().threads);
    assert_eq!(8888, build_root_context(&args(&[]), &tags! {"threads" => 8888}).unwrap().config().threads);
}

#[test]
fn config_defines_default_argument_value() {
    assert_eq!(
        Some(&Value::Int(8888)),
        build_root_context(&args(&[]), &tags! {"arg" => 8888}).unwrap().argument("arg")
    );
}

#[test]
fn override_default_arg_from_config() {
    assert_eq!(
        Some(&Value::Int(9999)),
        build_root_context(&args(&[("arg", "9999")]), &tags! {"arg" => 8888}).unwrap().argument("arg")
    );
}

#[test]
fn define_arg_type_in_config() {
    let schema = tags! {"arg" => map(vec![("default", Value::str("8888")), ("type", Value::str("integer"))])};
    assert_eq!(Some(&Value::Int(8888)), build_root_context(&args(&[]), &schema).unwrap().argument("arg"));
}

#[test]
fn imply_type_from_default_value() {
    assert_eq!(
        Some(&Value::str("8888")),
        build_root_context(&args(&[]), &tags! {"arg" => "8888"}).unwrap().argument("arg")
    );
    assert_eq!(
        Some(&Value::str("9999")),
        build_root_context(&args(&[("arg", "9999")]), &tags! {"arg" => "8888"}).unwrap().argument("arg")
    );
    assert_eq!(
        Some(&Value::Int(8888)),
        build_root_context(&args(&[]), &tags! {"arg" => 8888}).unwrap().argument("arg")
    );
    assert_eq!(
        Some(&Value::Int(9999)),
        build_root_context(&args(&[("arg", "9999")]), &tags! {"arg" => 8888}).unwrap().argument("arg")
    );
}

#[test]
fn compute_default_value_using_expression() {
    let schema = tags! {"arg" => map(vec![("default", Value::str("${ 1+1 }")), ("type", Value::str("integer"))])};
    assert_eq!(Some(&Value::Int(2)), build_root_context(&args(&[]), &schema).unwrap().argument("arg"));
}

#[test]
fn imply_type_from_expression_value() {
    assert_eq!(
        Some(&Value::Int(2)),
        build_root_context(&args(&[]), &tags! {"arg" => "${ 1+1 }"}).unwrap().argument("arg")
    );
}

#[test]
fn reference_other_arg_in_expression() {
    let config_args = tags! {"arg1" => 1, "arg2" => "${ args.arg1 + 1}"};
    assert_eq!(Some(&Value::Int(2)), build_root_context(&args(&[]), &config_args).unwrap().argument("arg2"));
    assert_eq!(
        Some(&Value::Int(3)),
        build_root_context(&args(&[("arg1", "2")]), &config_args).unwrap().argument("arg2")
    );
    assert_eq!(
        Some(&Value::Int(10)),
        build_root_context(&args(&[("arg2", "10")]), &config_args).unwrap().argument("arg2")
    );
}

#[test]
fn reference_other_arg_in_expression_twice() {
    let config_args = tags! {"arg1" => 1, "arg2" => "${ args.arg1 + 1}", "arg3" => "${ args.arg2 + 1}"};
    assert_eq!(Some(&Value::Int(3)), build_root_context(&args(&[]), &config_args).unwrap().argument("arg3"));
}

#[test]
fn fail_on_infinite_loop() {
    let config_args = tags! {
        "arg1" => map(vec![("default", Value::str("${ args.arg3 + 1 }")), ("type", Value::str("long"))]),
        "arg2" => "${ args.arg1 + 1}",
        "arg3" => "${ args.arg2 + 1}"
    };
    assert!(build_root_context(&args(&[]), &config_args).is_err());
    // but if you break the chain it's OK
    assert_eq!(
        Some(&Value::Int(3)),
        build_root_context(&args(&[("arg1", "1")]), &config_args).unwrap().argument("arg3")
    );
}

#[test]
fn set_planetiler_config_from_other_arg() {
    let schema = tags! {"other" => "8888", "threads" => "${ args.other }"};
    assert_eq!(8888, build_root_context(&args(&[]), &schema).unwrap().config().threads);
}

#[test]
fn test_cant_redefine_builtin() {
    let schema = tags! {"threads" => map(vec![("default", Value::Int(4)), ("type", Value::str("string"))])};
    assert!(build_root_context(&args(&[]), &schema).is_err());
}

#[test]
fn test_define_required_arg() {
    let schema = tags! {"arg" => map(vec![("type", Value::str("integer")), ("description", Value::str("desc"))])};
    assert!(build_root_context(&args(&[]), &schema).is_err());
    assert_eq!(Some(&Value::Int(3)), build_root_context(&args(&[("arg", "3")]), &schema).unwrap().argument("arg"));
}

#[test]
fn set_planetiler_config_from_other_planetiler_config() {
    let root = build_root_context(&args(&[]), &tags! {"mmap" => "${ args.threads < 1 }"}).unwrap();
    assert!(root.config().mmap_temp_storage);
}
