//! Port of `util/YAML.java`: parse YAML files into Rust objects, handling aliases, anchors and the
//! [merge operator](https://yaml.org/type/merge.html) `<<`.
//!
//! Java uses snakeyaml to parse and jackson to map into model objects; here `serde_yaml` parses into a
//! [`serde_yaml::Value`], merge keys are expanded (preserving Java's key order: merged keys first, then the original
//! ones), and then the value is deserialized with serde.

use std::io::Read;
use std::path::Path;

use anyhow::Context;
use serde::de::DeserializeOwned;
use serde_yaml::{Mapping, Value as Yaml};

use crate::value::{Tags, Value};

/// Parses `config` into a [`serde_yaml::Value`] with merge keys expanded.
pub fn load_yaml(config: &str) -> anyhow::Result<Yaml> {
    let mut parsed: Yaml = serde_yaml::from_str(config)?;
    handle_merge_operator(&mut parsed, 0)?;
    Ok(parsed)
}

/// Parses `config` and deserializes it into `T`.
pub fn load<T: DeserializeOwned>(config: &str) -> anyhow::Result<T> {
    convert_value(load_yaml(config)?)
}

/// Reads and parses the YAML file at `path` and deserializes it into `T`.
pub fn load_file<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let contents = std::fs::read_to_string(path).with_context(|| format!("Unable to read {}", path.display()))?;
    load(&contents).with_context(|| format!("Unable to parse {}", path.display()))
}

/// Reads and parses YAML from `reader` and deserializes it into `T`.
pub fn load_reader<T: DeserializeOwned>(mut reader: impl Read) -> anyhow::Result<T> {
    let mut contents = String::new();
    reader.read_to_string(&mut contents)?;
    load(&contents)
}

/// Deserializes an already-parsed YAML value into `T`.
pub fn convert_value<T: DeserializeOwned>(parsed: Yaml) -> anyhow::Result<T> {
    Ok(serde_yaml::from_value(parsed)?)
}

/// Parses `config` into a dynamically-typed [`Value`] (Java's `YAML.load(string, Object.class)`).
pub fn load_value(config: &str) -> anyhow::Result<Value> {
    Ok(to_value(&load_yaml(config)?))
}

/// Converts a YAML value into a dynamically-typed [`Value`]. Mapping keys are converted to strings.
pub fn to_value(yaml: &Yaml) -> Value {
    match yaml {
        Yaml::Null => Value::Null,
        Yaml::Bool(b) => Value::Bool(*b),
        Yaml::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Double(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Yaml::String(s) => Value::str(s),
        Yaml::Sequence(seq) => Value::list(seq.iter().map(to_value)),
        Yaml::Mapping(map) => {
            let tags: Tags = map.iter().map(|(k, v)| (key_to_string(k), to_value(v))).collect();
            Value::map(tags)
        }
        Yaml::Tagged(tagged) => to_value(&tagged.value),
    }
}

fn key_to_string(key: &Yaml) -> String {
    match key {
        Yaml::String(s) => s.clone(),
        Yaml::Null => "null".into(),
        Yaml::Bool(b) => b.to_string(),
        Yaml::Number(n) => n.to_string(),
        other => to_value(other).to_string(),
    }
}

/// Maximum nesting depth before we assume the document contains a recursive reference.
const MAX_DEPTH: usize = 10_000;

fn is_merge_key(key: &Yaml) -> bool {
    matches!(key, Yaml::String(s) if s == "<<")
}

/// Manually post-process the parsed yaml object to merge referenced objects into the parent one.
fn handle_merge_operator(parsed: &mut Yaml, depth: usize) -> anyhow::Result<()> {
    if depth > MAX_DEPTH {
        anyhow::bail!("Illegal recursive reference in yaml file");
    }
    match parsed {
        Yaml::Mapping(map) => {
            if let Some(mut to_merge) = map.remove("<<") {
                handle_merge_operator(&mut to_merge, depth + 1)?;
                let orig = std::mem::take(map);
                // to preserve the map key order we insert the merged operator objects first, then the original ones
                merge_into(map, to_merge, false, depth)?;
                for (k, v) in orig {
                    if !is_merge_key(&k) {
                        map.insert(k, v);
                    }
                }
            }
            for (_, value) in map.iter_mut() {
                handle_merge_operator(value, depth + 1)?;
            }
        }
        Yaml::Sequence(seq) => {
            for item in seq.iter_mut() {
                handle_merge_operator(item, depth + 1)?;
            }
        }
        Yaml::Tagged(tagged) => handle_merge_operator(&mut tagged.value, depth + 1)?,
        _ => {}
    }
    Ok(())
}

fn merge_into(dest: &mut Mapping, source: Yaml, replace: bool, depth: usize) -> anyhow::Result<()> {
    if depth > MAX_DEPTH {
        anyhow::bail!("Illegal recursive reference in yaml file");
    }
    match source {
        Yaml::Mapping(map) => {
            for (k, v) in map {
                if replace || !dest.contains_key(&k) {
                    dest.insert(k, v);
                }
            }
        }
        Yaml::Sequence(nested) => {
            for item in nested {
                merge_into(dest, item, replace, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags;
    use std::collections::HashMap;

    #[test]
    fn test_load_yaml_raw() {
        assert_eq!(Value::map(tags! {"a" => 1}), load_value("a: 1\n").unwrap());
    }

    #[test]
    fn test_load_yaml_to_struct() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Result {
            a: i32,
        }
        assert_eq!(Result { a: 1 }, load::<Result>("a: 1\n").unwrap());
    }

    #[test]
    fn test_load_large_yaml_list() {
        let mut builder = String::new();
        let mut expected = Vec::new();
        for i in 0..500_000 {
            builder.push_str(&format!("\n- {i}"));
            expected.push(i);
        }
        assert_eq!(expected, load::<Vec<i32>>(&builder).unwrap());
    }

    #[test]
    fn test_load_large_yaml_map() {
        let mut builder = String::new();
        let mut expected = HashMap::new();
        for i in 0..500_000 {
            builder.push_str(&format!("\n{i}: {i}"));
            expected.insert(i.to_string(), i);
        }
        let Value::Map(result) = load_value(&builder).unwrap() else { panic!() };
        assert_eq!(expected.len(), result.len());
        for (k, v) in result.iter() {
            assert_eq!(Some(&v.as_i64().unwrap()), expected.get(k).map(|v| *v as i64).as_ref());
        }
    }

    fn assert_same_yaml(a: &str, b: &str) {
        assert_eq!(load_value(b).unwrap(), load_value(a).unwrap());
    }

    #[test]
    fn test_merge_operator() {
        assert_same_yaml(
            "source: &label\n  a: 1\ndest:\n  <<: *label\n  b: 2\n",
            "source:\n  a: 1\ndest:\n  a: 1\n  b: 2\n",
        );
    }

    #[test]
    fn test_merge_operator_key_order() {
        let Value::Map(result) = load_value("source: &label\n  a: 1\ndest:\n  c: 3\n  <<: *label\n  b: 2\n").unwrap()
        else {
            panic!()
        };
        let keys: Vec<&String> = result["dest"].as_map().unwrap().keys().collect();
        assert_eq!(vec!["a", "c", "b"], keys);
    }

    #[test]
    fn test_merge_operator_nested() {
        assert_same_yaml(
            "source: &label\n  a: 1\ndest:\n  l1:\n    l2:\n      l3:\n        <<: *label\n        b: 2\n",
            "source:\n  a: 1\ndest:\n  l1:\n    l2:\n      l3:\n        a: 1\n        b: 2\n",
        );
    }

    #[test]
    fn test_merge_operator_override() {
        assert_same_yaml("source: &label\n  a: 1\ndest:\n  <<: *label\n  a: 2\n", "source:\n  a: 1\ndest:\n  a: 2\n");
    }

    #[test]
    fn test_merge_operator_multiple() {
        assert_same_yaml(
            "source: &label1\n  a: 1\n  z: 1\nsource2: &label2\n  a: 2\n  b: 3\ndest:\n  <<: [*label1, *label2]\n  b: 4\n  c: 5\n",
            "source:\n  a: 1\n  z: 1\nsource2:\n  a: 2\n  b: 3\ndest:\n  a: 1\n  b: 4\n  c: 5\n  z: 1\n",
        );
    }

    #[test]
    fn test_merge_not_anchor() {
        assert_same_yaml("<<:\n  a: 1\n  b: 2\nb: 3\nc: 4\n", "a: 1\nb: 3\nc: 4\n");
    }

    #[test]
    fn test_merge_operator_second() {
        assert_same_yaml(
            "source: &label\n  a: 1\ndest:\n  c: 3\n  <<: *label\n  b: 2\n",
            "source:\n  a: 1\ndest:\n  a: 1\n  b: 2\n  c: 3\n",
        );
    }

    const DRAFT_EXPECTED: &str = "- { x: 1, y: 2 }\n- { x: 0, y: 2 }\n- { r: 10 }\n- { r: 1 }\n- # Explicit keys\n  x: 1\n  y: 2\n  r: 10\n  label: center/big\n";
    const DRAFT_PREFIX: &str =
        "- &CENTER { x: 1, y: 2 }\n- &LEFT { x: 0, y: 2 }\n- &BIG { r: 10 }\n- &SMALL { r: 1 }\n";

    #[test]
    fn test_merge_operator_from_draft1() {
        assert_same_yaml(
            DRAFT_EXPECTED,
            &format!("{DRAFT_PREFIX}- # Merge one map\n  << : *CENTER\n  r: 10\n  label: center/big\n"),
        );
    }

    #[test]
    fn test_merge_operator_from_draft2() {
        assert_same_yaml(
            DRAFT_EXPECTED,
            &format!("{DRAFT_PREFIX}- # Merge multiple maps\n  << : [ *CENTER, *BIG ]\n  label: center/big\n"),
        );
    }

    #[test]
    fn test_merge_operator_from_draft3() {
        assert_same_yaml(
            DRAFT_EXPECTED,
            &format!("{DRAFT_PREFIX}- # Override\n  << : [ *BIG, *LEFT, *SMALL ]\n  x: 1\n  label: center/big\n"),
        );
    }

    #[test]
    fn test_anchor_and_alias_map() {
        assert_same_yaml("source: &label\n  a: 1\ndest: *label\n", "source:\n  a: 1\ndest:\n  a: 1\n");
    }

    #[test]
    fn test_anchor_and_alias_list() {
        assert_same_yaml("source: &label\n  - 1\ndest: *label\n", "source: [1]\ndest: [1]\n");
    }

    #[test]
    fn test_allow_ref_in_merge_doc() {
        assert_same_yaml(
            "source: &label\n  a: &label1\n    c: 1\n  b: *label1\n  d:\n    <<: *label1\ndest: *label\n",
            "source: {a: {c: 1}, b: {c: 1}, d: {c: 1}}\ndest: {a: {c: 1}, b: {c: 1}, d: {c: 1}}\n",
        );
    }

    #[test]
    fn test_fails_on_recursive_refs() {
        assert!(load_value("source: &label\n  - *label\n").is_err());
        assert!(load_value("source: &label\n  <<: *label\n").is_err());
        assert!(load_value("source: &label\n  a: *label\n").is_err());
    }
}
