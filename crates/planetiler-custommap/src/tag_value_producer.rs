//! Port of `TagValueProducer.java`: parses attribute values from source features based on the `tag_mappings` YAML
//! config.

use std::any::Any;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use indexmap::IndexMap;
use planetiler_core::expression::data_type::DataType;
use planetiler_core::expression::expression::{ExpressionInput, Getter, TypedGetter};
use planetiler_core::reader::with_tags::WithTags;
use planetiler_core::value::{Tags, Value};

/// How to compute the value of a key.
#[derive(Clone, Debug, PartialEq)]
enum KeyGetter {
    /// Read the key itself with a data type.
    Data(DataType),
    /// Read `input` (which may itself be mapped) and convert it to a data type.
    Rename { input: String, data_type: DataType },
}

/// A getter for a key that is renamed from another (possibly mapped) input key: applies `conversions` in order to
/// the result of `base.apply(base_key)`.
#[derive(Debug, PartialEq)]
struct RenameGetter {
    base: DataType,
    base_key: String,
    conversions: Vec<DataType>,
}

impl RenameGetter {
    fn get(&self, input: &dyn WithTags) -> Value {
        let mut value = self.base.apply(input, &self.base_key);
        for conversion in &self.conversions {
            value = conversion.convert_from(Some(&value));
        }
        value
    }
}

impl TypedGetter for RenameGetter {
    fn apply(&self, input: &dyn ExpressionInput, _key: Option<&str>) -> Value {
        self.get(input)
    }

    fn dyn_eq(&self, other: &dyn TypedGetter) -> bool {
        other.as_any().downcast_ref::<RenameGetter>().is_some_and(|o| o == self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Utility that parses attribute values from source features, based on YAML config.
#[derive(Debug, Default)]
pub struct TagValueProducer {
    value_retriever: HashMap<String, Getter>,
    key_type: HashMap<String, String>,
}

static EMPTY: LazyLock<TagValueProducer> = LazyLock::new(TagValueProducer::default);

impl TagValueProducer {
    /// A producer with no mappings (Java `TagValueProducer.EMPTY`).
    pub fn empty() -> &'static TagValueProducer {
        &EMPTY
    }

    /// Creates a producer from the `tag_mappings` section of a schema: `key: type` or
    /// `key: {type: type, input: other_key}`.
    pub fn new(map: Option<&Tags>) -> TagValueProducer {
        let Some(map) = map else {
            return TagValueProducer::default();
        };
        let mut getters: IndexMap<String, KeyGetter> = IndexMap::new();
        let mut key_type = HashMap::new();
        for (key, value) in map {
            match value {
                Value::String(string_type) => {
                    getters.insert(key.clone(), KeyGetter::Data(DataType::from_id(string_type)));
                    key_type.insert(key.clone(), string_type.to_string());
                }
                Value::Map(rename_map) => {
                    let input_key = rename_map.get("input").map_or_else(|| key.clone(), |v| v.to_string());
                    let data_type =
                        rename_map.get("type").map_or(DataType::GetTag, |t| DataType::from_id(&t.to_string()));
                    // When requesting the output value, actually retrieve the input key with the desired getter
                    if &input_key == key {
                        getters.insert(key.clone(), KeyGetter::Data(data_type));
                    } else {
                        getters.insert(key.clone(), KeyGetter::Rename { input: input_key, data_type });
                    }
                    if let Some(t) = rename_map.get("type") {
                        key_type.insert(key.clone(), t.to_string());
                    }
                }
                _ => {}
            }
        }
        let value_retriever = getters
            .keys()
            .map(|key| {
                let getter = match &getters[key] {
                    KeyGetter::Data(d) => Getter::Data(*d),
                    KeyGetter::Rename { .. } => {
                        let mut conversions = Vec::new();
                        let mut current = key.as_str();
                        let mut depth = 0;
                        let (base, base_key) = loop {
                            match getters.get(current) {
                                Some(KeyGetter::Rename { input, data_type }) if depth < 100 => {
                                    conversions.push(*data_type);
                                    current = input;
                                    depth += 1;
                                }
                                Some(KeyGetter::Data(d)) => break (*d, current.to_string()),
                                _ => break (DataType::GetTag, current.to_string()),
                            }
                        };
                        conversions.reverse();
                        Getter::Custom(Arc::new(RenameGetter { base, base_key, conversions }))
                    }
                };
                (key.clone(), getter)
            })
            .collect();
        TagValueProducer { value_retriever, key_type }
    }

    pub fn is_empty(&self) -> bool {
        self.value_retriever.is_empty()
    }

    /// Returns true if there is a mapping for `key`.
    pub fn has_mapping(&self, key: &str) -> bool {
        self.value_retriever.contains_key(key)
    }

    /// Keys that have a mapping.
    pub fn mapped_keys(&self) -> impl Iterator<Item = &str> {
        self.value_retriever.keys().map(String::as_str)
    }

    /// Returns a getter that extracts the value for `key` from an input.
    pub fn value_getter_for_key(&self, key: &str) -> Getter {
        self.value_retriever.get(key).cloned().unwrap_or(Getter::Data(DataType::GetTag))
    }

    /// Returns the mapped value for a key where the key is not known ahead of time.
    pub fn value_for_key(&self, feature: &dyn ExpressionInput, key: &str) -> Value {
        match self.value_retriever.get(key) {
            None => feature.get_tag_nested(key).map(Cow::into_owned).unwrap_or_default(),
            Some(Getter::Data(d)) => d.apply(feature, key),
            Some(Getter::Custom(g)) => g.apply(feature, Some(key)),
        }
    }

    /// Same as [`TagValueProducer::value_for_key`] for a plain tag map.
    pub fn value_for_key_in_tags(&self, tags: &Tags, key: &str) -> Value {
        self.value_for_key(tags, key)
    }

    /// Returns a copy of `keyed_map` where the keys have been transformed by the parser associated with `key`.
    pub fn remap_keys_by_type<T: Clone>(&self, key: &str, keyed_map: &IndexMap<String, T>) -> IndexMap<Value, T> {
        match self.key_type.get(key) {
            None => keyed_map.iter().map(|(k, v)| (Value::str(k), v.clone())).collect(),
            Some(data_type) => {
                let parser = DataType::from_id(data_type);
                keyed_map.iter().map(|(k, v)| (parser.convert_from(Some(&Value::str(k))), v.clone())).collect()
            }
        }
    }

    /// Returns a map where every tag has been transformed (or inferred) by the registered conversions.
    pub fn map_tags<'a>(&self, feature: &'a dyn ExpressionInput) -> Cow<'a, Tags> {
        if self.value_retriever.is_empty() {
            Cow::Borrowed(feature.tags())
        } else {
            let mut result = feature.tags().clone();
            for key in self.value_retriever.keys() {
                result.insert(key.clone(), self.value_for_key(feature, key));
            }
            Cow::Owned(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contexts::Root;
    use planetiler_core::geo::geo_utils;
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;

    fn tvp(yaml: &str) -> TagValueProducer {
        match planetiler_core::util::yaml::load_value(yaml).unwrap() {
            Value::Map(m) => TagValueProducer::new(Some(&m)),
            _ => TagValueProducer::new(None),
        }
    }

    fn test_get(tvp: &TagValueProducer, tags: Tags, key: &str, expected: Value) {
        let wrapped = tags.clone();
        assert_eq!(expected, tvp.map_tags(&wrapped).get(key).cloned().unwrap_or_default());
        assert_eq!(expected, tvp.value_for_key(&wrapped, key));
        let value = match tvp.value_getter_for_key(key) {
            Getter::Data(d) => d.apply(&wrapped, key),
            Getter::Custom(g) => g.apply(&wrapped, Some(key)),
        };
        assert_eq!(expected, value);
        let root = Root::empty();
        let feature = SimpleFeature::create_tags(geo_utils::empty_geometry(), tags);
        let ctx = root.create_process_feature_context(&feature, tvp);
        let post_match = ctx.create_post_match_context(&[]);
        assert_eq!(expected, tvp.value_for_key(&post_match, key));
    }

    #[test]
    fn test_empty_tag_value_producer() {
        let tvp = tvp("{}");
        test_get(&tvp, tags! {}, "key", Value::Null);
        test_get(&tvp, tags! {"key" => 1}, "key", Value::Int(1));
        test_get(&tvp, tags! {"key" => 1}, "other", Value::Null);
    }

    #[test]
    fn test_null_tag_value_producer() {
        let tvp = TagValueProducer::new(None);
        test_get(&tvp, tags! {}, "key", Value::Null);
    }

    #[test]
    fn test_parse_types() {
        let tvp = tvp("int: integer\ndouble: {type: double}\ndirection: {type: direction}");
        test_get(&tvp, tags! {}, "int", Value::Null);
        test_get(&tvp, tags! {}, "double", Value::Null);
        test_get(&tvp, tags! {}, "direction", Value::Int(0));
        test_get(&tvp, tags! {"int" => 1}, "int", Value::Int(1));
        test_get(&tvp, tags! {"int" => "1"}, "int", Value::Int(1));
        test_get(&tvp, tags! {"direction" => "-1"}, "direction", Value::Int(-1));
    }

    #[test]
    fn test_remap_keys() {
        let tvp = tvp("int2: {type: integer, input: int}\nint3: {type: integer, input: int2}");
        test_get(&tvp, tags! {"int" => "1"}, "int", Value::str("1"));
        test_get(&tvp, tags! {"int" => "1"}, "int2", Value::Int(1));
        test_get(&tvp, tags! {"int" => "1"}, "int3", Value::Int(1));
        test_get(&tvp, tags! {}, "int", Value::Null);
        test_get(&tvp, tags! {}, "int2", Value::Null);
        test_get(&tvp, tags! {}, "int3", Value::Null);
    }
}
