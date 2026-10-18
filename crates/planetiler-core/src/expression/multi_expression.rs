//! Port of `expression/MultiExpression.java`: a list of [`Expression`]s to evaluate on input elements.
//!
//! [`MultiExpression::index`] returns an optimized [`Index`] that evaluates the minimal set of expressions on the keys
//! present on the element, and [`Index::get_matches`] returns the data values associated with the expressions that
//! match an input element.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::expression::expression::{
    Expression, ExpressionInput, LINESTRING_TYPE, MatchKeys, POINT_TYPE, POLYGON_TYPE, UNKNOWN_GEOMETRY_TYPE,
    match_source, match_source_layer,
};
use crate::expression::simplifiable::Simplifiable;
use crate::value::Tags;

/// An `expression` to evaluate on input elements and `result` value to return when the element matches.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry<T> {
    pub result: T,
    pub expression: Expression,
}

pub fn entry<T>(result: T, expression: Expression) -> Entry<T> {
    Entry { result, expression }
}

/// The result when an expression matches, along with the input element tag `keys` that triggered the match.
#[derive(Clone, Debug, PartialEq)]
pub struct Match<'a, T> {
    pub result: &'a T,
    pub keys: MatchKeys,
    pub id: usize,
}

/// A list of expressions and associated values.
#[derive(Clone, Debug, PartialEq)]
pub struct MultiExpression<T> {
    pub expressions: Vec<Entry<T>>,
}

/// Returns true if `expression` only contains "not filter" so we can't limit evaluating this expression to only when
/// a particular key is present on the input.
fn must_always_evaluate(expression: &Expression) -> bool {
    match expression {
        Expression::Or(children) => children.iter().any(must_always_evaluate),
        Expression::And(children) => children.iter().all(must_always_evaluate),
        Expression::Not(child) => !must_always_evaluate(child),
        Expression::MatchAny(any) => any.must_always_evaluate(),
        Expression::MatchField(_) => false,
        other => !other.is_false(),
    }
}

/// Calls `accept_key` for every tag that could possibly cause `exp` to match an input element.
fn get_relevant_keys(exp: &Expression, accept_key: &mut dyn FnMut(&str)) {
    // if a sub-expression must always be evaluated, then either the whole expression must always be evaluated
    // or there is another part of the expression that limits the elements on which it must be evaluated, so we can
    // ignore keys from this sub-expression.
    if !must_always_evaluate(exp) {
        match exp {
            Expression::And(children) | Expression::Or(children) => {
                children.iter().for_each(|c| get_relevant_keys(c, accept_key))
            }
            Expression::MatchField(field) => accept_key(field),
            Expression::MatchAny(any) if !any.must_always_evaluate() => {
                if let Some(field) = &any.field {
                    accept_key(field)
                }
            }
            // ignore not case since not(matchAny("field", "")) should track "field" as a relevant key, but that gets
            // simplified to matchField("field") so don't need to handle that here
            _ => {}
        }
    }
}

/// Strips the last segment of a dotted key: `a.b.c` -> `a.b`, `a[].b` -> `a`, `a` -> ``.
fn parent_key(key: &str) -> &str {
    match key.rfind('.') {
        None => "",
        Some(idx) => {
            let prefix = &key[..idx];
            prefix.strip_suffix("[]").unwrap_or(prefix)
        }
    }
}

impl<T: Clone + PartialEq> MultiExpression<T> {
    /// Returns a new multi-expression from `expressions` where multiple expressions for the same result get OR'd
    /// together. If the order in which expressions match matters, use [`MultiExpression::of_ordered`] instead.
    pub fn of(expressions: Vec<Entry<T>>) -> MultiExpression<T> {
        let mut merged: Vec<Entry<T>> = Vec::with_capacity(expressions.len());
        for e in expressions {
            match merged.iter_mut().find(|m| m.result == e.result) {
                Some(existing) => {
                    let prev = std::mem::replace(&mut existing.expression, Expression::FALSE);
                    existing.expression = Expression::Or(vec![prev, e.expression]);
                }
                None => merged.push(e),
            }
        }
        MultiExpression { expressions: merged }
    }

    /// Returns a new multi-expression from `expressions` where multiple expressions for the same result stay
    /// separate.
    pub fn of_ordered(expressions: Vec<Entry<T>>) -> MultiExpression<T> {
        MultiExpression { expressions }
    }

    /// Returns an optimized index for matching expressions against each input element.
    pub fn index(&self) -> Index<T> {
        self.index_impl(false)
    }

    /// Same as [`MultiExpression::index`] but logs a warning when there are degenerate expressions that must be
    /// evaluated on every input.
    pub fn index_and_warn(&self) -> Index<T> {
        self.index_impl(true)
    }

    fn index_impl(&self, warn: bool) -> Index<T> {
        if self.expressions.is_empty() {
            Index::Empty
        } else if self.contains(&|e| matches!(e, Expression::MatchSource(_))) {
            Index::Source(Box::new(StringFieldIndex::new(self, warn, StringField::Source)))
        } else if self.contains(&|e| matches!(e, Expression::MatchSourceLayer(_))) {
            Index::SourceLayer(Box::new(StringFieldIndex::new(self, warn, StringField::SourceLayer)))
        } else if self.contains(&|e| matches!(e, Expression::MatchType(_))) {
            Index::GeometryType(Box::new(GeometryTypeIndex::new(self, warn)))
        } else {
            Index::Key(KeyIndex::new(self.simplify(), warn))
        }
    }

    fn contains(&self, test: &dyn Fn(&Expression) -> bool) -> bool {
        self.expressions.iter().any(|e| e.expression.contains(test))
    }

    /// Returns a copy of this multi-expression that replaces every expression using `mapper` (and simplifies them,
    /// dropping entries that become [`Expression::FALSE`]).
    pub fn map(&self, mapper: impl Fn(&Expression) -> Expression) -> MultiExpression<T> {
        MultiExpression {
            expressions: self
                .expressions
                .iter()
                .map(|e| entry(e.result.clone(), mapper(&e.expression).simplify()))
                .filter(|e| !e.expression.is_false())
                .collect(),
        }
    }

    /// Returns a copy of this multi-expression that replaces every sub-expression that matches `test` with `b`.
    pub fn replace_matching(&self, test: &dyn Fn(&Expression) -> bool, b: &Expression) -> MultiExpression<T> {
        self.map(|e| e.replace_matching(test, b))
    }

    /// Returns a copy of this multi-expression that replaces every sub-expression equal to `a` with `b`.
    pub fn replace(&self, a: &Expression, b: &Expression) -> MultiExpression<T> {
        self.map(|e| e.replace(a, b))
    }

    /// Returns a copy of this multi-expression, keeping only entries whose data value matches `accept`.
    pub fn filter_results(&self, accept: impl Fn(&T) -> bool) -> MultiExpression<T> {
        MultiExpression { expressions: self.expressions.iter().filter(|e| accept(&e.result)).cloned().collect() }
    }

    /// Returns a copy of this multi-expression, replacing the data value with `f` (dropping entries where it returns
    /// `None`).
    pub fn map_results<U>(&self, f: impl Fn(&T) -> Option<U>) -> MultiExpression<U> {
        MultiExpression {
            expressions: self
                .expressions
                .iter()
                .filter_map(|e| f(&e.result).map(|r| entry(r, e.expression.clone())))
                .collect(),
        }
    }
}

impl<T: Clone + PartialEq> Simplifiable for MultiExpression<T> {
    fn simplify_once(&self) -> Self {
        self.map(Simplifiable::simplify)
    }
}

/// An optimized index for finding which expressions match an input element.
#[derive(Debug)]
pub enum Index<T> {
    Empty,
    Key(KeyIndex<T>),
    GeometryType(Box<GeometryTypeIndex<T>>),
    Source(Box<StringFieldIndex<T>>),
    SourceLayer(Box<StringFieldIndex<T>>),
}

impl<T> Index<T> {
    /// Returns all data values associated with expressions that match an input element, along with the tag keys that
    /// caused the match, ordered by expression id.
    pub fn get_matches_with_triggers(&self, input: &dyn ExpressionInput) -> Vec<Match<'_, T>> {
        match self {
            Index::Empty => Vec::new(),
            Index::Key(k) => k.get_matches_with_triggers(input),
            Index::GeometryType(g) => g.get_matches_with_triggers(input),
            Index::Source(s) => s.get_matches_with_triggers(input, input.source()),
            Index::SourceLayer(s) => s.get_matches_with_triggers(input, input.source_layer()),
        }
    }

    /// Returns all data values associated with expressions that match an input element.
    pub fn get_matches(&self, input: &dyn ExpressionInput) -> Vec<&T> {
        self.get_matches_with_triggers(input).into_iter().map(|m| m.result).collect()
    }

    /// Returns the data value associated with the first expression that matches an input element.
    pub fn get_first(&self, input: &dyn ExpressionInput) -> Option<&T> {
        self.get_matches_with_triggers(input).into_iter().next().map(|m| m.result)
    }

    /// Returns the data value associated with the first expression that matches an input element, or `default` if
    /// none match.
    pub fn get_or_else<'a>(&'a self, input: &dyn ExpressionInput, default: &'a T) -> &'a T {
        self.get_first(input).unwrap_or(default)
    }

    /// Returns the data value associated with the first expression matching a feature with `tags`.
    pub fn get_or_else_tags<'a>(&'a self, tags: &Tags, default: &'a T) -> &'a T {
        self.get_first(tags).unwrap_or(default)
    }

    /// Returns true if any expression matches the input element.
    pub fn matches(&self, input: &dyn ExpressionInput) -> bool {
        !self.get_matches_with_triggers(input).is_empty()
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Index::Empty)
    }
}

#[derive(Debug)]
struct EntryWithId<T> {
    result: T,
    expression: Expression,
    id: usize,
}

/// Index that limits the search space of expressions based on keys present on an input element.
#[derive(Debug)]
pub struct KeyIndex<T> {
    num_expressions: usize,
    entries: Vec<EntryWithId<T>>,
    // index from source feature tag key to the expressions that include it so that
    // we can limit the number of expressions we need to evaluate for each input,
    // improves matching performance by ~5x
    key_to_expressions_map: HashMap<Arc<str>, Vec<usize>>,
    // same as key_to_expressions_map but as a list (optimized for iteration when # source feature keys > # tags we
    // care about)
    key_to_expressions_list: Vec<(Arc<str>, Vec<usize>)>,
    // expressions that must always be evaluated on each input element
    always_evaluate: Vec<usize>,
}

impl<T> KeyIndex<T> {
    fn new(expressions: MultiExpression<T>, warn: bool) -> KeyIndex<T> {
        let mut key_to_expressions: HashMap<Arc<str>, Vec<usize>> = HashMap::new();
        let mut always = Vec::new();
        let mut entries = Vec::with_capacity(expressions.expressions.len());
        for (idx, e) in expressions.expressions.into_iter().enumerate() {
            let id = idx + 1;
            if must_always_evaluate(&e.expression) {
                always.push(idx);
            } else {
                get_relevant_keys(&e.expression, &mut |key| {
                    let mut key = key;
                    while !key.trim().is_empty() {
                        let list = key_to_expressions.entry(Arc::from(key)).or_default();
                        if !list.contains(&idx) {
                            list.push(idx);
                        }
                        key = parent_key(key);
                    }
                });
            }
            entries.push(EntryWithId { result: e.result, expression: e.expression, id });
        }
        if warn && !always.is_empty() {
            tracing::warn!("{} expressions will be evaluated for every element:", always.len());
            for idx in &always {
                tracing::warn!("    {}: {}", entries[*idx].id, entries[*idx].expression);
            }
        }
        let key_to_expressions_list = key_to_expressions.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        KeyIndex {
            num_expressions: entries.len() + 1,
            entries,
            key_to_expressions_map: key_to_expressions,
            key_to_expressions_list,
            always_evaluate: always,
        }
    }

    fn visit<'a>(
        &'a self,
        input: &dyn ExpressionInput,
        result: &mut Vec<Match<'a, T>>,
        visited: &mut [bool],
        expressions: &[usize],
    ) {
        for &idx in expressions {
            let e = &self.entries[idx];
            if !visited[e.id] {
                visited[e.id] = true;
                let mut keys = MatchKeys::new();
                if e.expression.evaluate_with_keys(input, Some(&mut keys)) {
                    result.push(Match { result: &e.result, keys, id: e.id });
                }
            }
        }
    }

    fn get_matches_with_triggers(&self, input: &dyn ExpressionInput) -> Vec<Match<'_, T>> {
        let mut result = Vec::new();
        let mut visited = vec![false; self.num_expressions];
        self.visit(input, &mut result, &mut visited, &self.always_evaluate);
        let tags = input.tags();
        if tags.len() < self.key_to_expressions_map.len() {
            for key in tags.keys() {
                if let Some(list) = self.key_to_expressions_map.get(key.as_str()) {
                    self.visit(input, &mut result, &mut visited, list);
                }
            }
        } else {
            for (key, list) in &self.key_to_expressions_list {
                if tags.contains_key(&**key) {
                    self.visit(input, &mut result, &mut visited, list);
                }
            }
        }
        result.sort_by_key(|m| m.id);
        result
    }
}

/// Index that limits the search space of expressions based on geometry type of an input element.
#[derive(Debug)]
pub struct GeometryTypeIndex<T> {
    point_index: Index<T>,
    line_index: Index<T>,
    polygon_index: Index<T>,
    other_index: Index<T>,
}

impl<T: Clone + PartialEq> GeometryTypeIndex<T> {
    fn new(expressions: &MultiExpression<T>, warn: bool) -> GeometryTypeIndex<T> {
        // build an index per type then search in each of those indexes based on the geometry type of each input
        // element this narrows the search space substantially, improving matching performance
        let index_for_type = |type_name: &'static str| {
            expressions
                .replace(&Expression::MatchType(type_name), &Expression::TRUE)
                .replace_matching(&|e| matches!(e, Expression::MatchType(_)), &Expression::FALSE)
                .simplify()
                .index_impl(warn)
        };
        GeometryTypeIndex {
            point_index: index_for_type(POINT_TYPE),
            line_index: index_for_type(LINESTRING_TYPE),
            polygon_index: index_for_type(POLYGON_TYPE),
            other_index: index_for_type(UNKNOWN_GEOMETRY_TYPE),
        }
    }
}

impl<T> GeometryTypeIndex<T> {
    fn get_matches_with_triggers(&self, input: &dyn ExpressionInput) -> Vec<Match<'_, T>> {
        let mut result = match input.as_with_geometry_type() {
            Some(g) if g.is_point() => self.point_index.get_matches_with_triggers(input),
            Some(g) if g.can_be_line() => {
                let mut result = self.line_index.get_matches_with_triggers(input);
                // closed ways can be lines or polygons, unless area=yes or no
                if g.can_be_polygon() {
                    result.extend(self.polygon_index.get_matches_with_triggers(input));
                }
                result
            }
            Some(g) if g.can_be_polygon() => self.polygon_index.get_matches_with_triggers(input),
            _ => self.other_index.get_matches_with_triggers(input),
        };
        result.sort_by_key(|m| m.id);
        result
    }
}

#[derive(Clone, Copy, Debug)]
enum StringField {
    Source,
    SourceLayer,
}

impl StringField {
    fn extract(self, e: &Expression) -> Option<&str> {
        match (self, e) {
            (StringField::Source, Expression::MatchSource(s)) => Some(s),
            (StringField::SourceLayer, Expression::MatchSourceLayer(l)) => Some(l),
            _ => None,
        }
    }

    fn make(self, value: &str) -> Expression {
        match self {
            StringField::Source => match_source(value),
            StringField::SourceLayer => match_source_layer(value),
        }
    }
}

/// Index that limits the search space of expressions based on the source or source layer of an input element.
#[derive(Debug)]
pub struct StringFieldIndex<T> {
    by_value: HashMap<String, Index<T>>,
    all_values_index: Index<T>,
}

impl<T: Clone + PartialEq> StringFieldIndex<T> {
    fn new(expressions: &MultiExpression<T>, warn: bool, field: StringField) -> StringFieldIndex<T> {
        let mut values = HashSet::new();
        for e in &expressions.expressions {
            e.expression.visit(&mut |e| {
                if let Some(v) = field.extract(e) {
                    values.insert(v.to_string());
                }
            });
        }
        let is_field = move |e: &Expression| field.extract(e).is_some();
        let mut by_value = HashMap::with_capacity(values.len());
        for value in values {
            let for_this = expressions
                .replace(&field.make(&value), &Expression::TRUE)
                .replace_matching(&is_field, &Expression::FALSE)
                .simplify()
                .index_impl(warn);
            if !for_this.is_empty() {
                by_value.insert(value, for_this);
            }
        }
        let all_values_index = expressions.replace_matching(&is_field, &Expression::FALSE).simplify().index_impl(warn);
        StringFieldIndex { by_value, all_values_index }
    }
}

impl<T> StringFieldIndex<T> {
    fn get_matches_with_triggers(&self, input: &dyn ExpressionInput, key: Option<&str>) -> Vec<Match<'_, T>> {
        let index = key.and_then(|k| self.by_value.get(k)).unwrap_or(&self.all_values_index);
        let mut result = index.get_matches_with_triggers(input);
        result.sort_by_key(|m| m.id);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expression::data_type::DataType;
    use crate::expression::expression::tests::{TestFeature, feature_with_tags, fwt};
    use crate::expression::expression::*;
    use crate::geo::geometry_type::GeometryType;
    use crate::reader::with_tags::OfMap;
    use crate::tags;

    fn same<T: ToString>(expected: &[&str], actual: Vec<&T>) {
        let mut a: Vec<String> = actual.iter().map(|s| s.to_string()).collect();
        let mut e: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        a.sort();
        e.sort();
        assert_eq!(e, a);
    }

    fn of(entries: Vec<(&'static str, Expression)>) -> MultiExpression<&'static str> {
        MultiExpression::of(entries.into_iter().map(|(r, e)| entry(r, e)).collect())
    }

    fn m(k: &str, v: &str) -> Expression {
        match_any(k, [v])
    }

    #[test]
    fn test_empty() {
        let index = of(vec![]).index();
        same(&[], index.get_matches(&fwt(&[])));
        same(&[], index.get_matches(&fwt(&[("key", "value")])));
    }

    #[test]
    fn test_single_element() {
        let index = of(vec![("a", m("key", "value"))]).index();
        same(&["a"], index.get_matches(&fwt(&[("key", "value")])));
        same(&["a"], index.get_matches(&fwt(&[("key", "value"), ("otherkey", "othervalue")])));
        same(&[], index.get_matches(&fwt(&[("key2", "value"), ("key3", "value")])));
        same(&[], index.get_matches(&fwt(&[("key2", "value")])));
        same(&[], index.get_matches(&fwt(&[("key", "no")])));
        same(&[], index.get_matches(&fwt(&[])));
    }

    #[test]
    fn test_double_element() {
        let index = of(vec![("a", m("key", "value")), ("a", m("key", "value"))]).index();
        same(&["a"], index.get_matches(&fwt(&[("key", "value")])));
        same(&[], index.get_matches(&fwt(&[])));
    }

    #[test]
    fn test_typed() {
        let index = of(vec![("a", match_any_typed(Some("key"), DataType::GetBoolean, [true]))]).index();
        for v in ["true", "yes", "1"] {
            same(&["a"], index.get_matches(&fwt(&[("key", v)])));
        }
        same(&[], index.get_matches(&fwt(&[("key2", "true"), ("key3", "value")])));
        same(&[], index.get_matches(&fwt(&[("key", "false")])));
        same(&[], index.get_matches(&fwt(&[])));

        let index = of(vec![("a", match_any_typed(Some("key"), DataType::GetBoolean, [false]))]).index();
        for v in ["false", "no", "0"] {
            same(&["a"], index.get_matches(&fwt(&[("key", v)])));
        }
        same(&[], index.get_matches(&fwt(&[("key2", "false")])));
        same(&[], index.get_matches(&fwt(&[("key", "true")])));
        same(&[], index.get_matches(&fwt(&[])));

        let index = of(vec![("a", match_any_typed(Some("key"), DataType::GetLong, [42]))]).index();
        same(&["a"], index.get_matches(&fwt(&[("key", "42")])));
        same(&[], index.get_matches(&fwt(&[("key2", "42")])));
        same(&[], index.get_matches(&fwt(&[("key", "99")])));
        same(&[], index.get_matches(&fwt(&[])));

        let index = of(vec![("a", match_any_typed(Some("key"), DataType::GetDirection, [1]))]).index();
        same(&["a"], index.get_matches(&fwt(&[("key", "yes")])));
        same(&["a"], index.get_matches(&fwt(&[("key", "1"), ("otherkey", "othervalue")])));
        same(&[], index.get_matches(&fwt(&[("key2", "1")])));
        same(&[], index.get_matches(&fwt(&[("key", "99")])));
        same(&[], index.get_matches(&fwt(&[])));
    }

    #[test]
    fn test_blank_string_treated_as_not_match() {
        let index = of(vec![("a", match_any("key", ["value", ""]))]).index();
        same(&["a"], index.get_matches(&fwt(&[("key", "value")])));
        same(&["a"], index.get_matches(&fwt(&[("key", "")])));
        same(&["a"], index.get_matches(&fwt(&[])));
        same(&["a"], index.get_matches(&fwt(&[("otherkey", "othervalue")])));
        same(&["a"], index.get_matches(&fwt(&[("key2", "value"), ("key3", "value")])));
        same(&[], index.get_matches(&fwt(&[("key", "no")])));
    }

    fn match_field_check(index: &Index<&'static str>) {
        same(&["a"], index.get_matches(&fwt(&[("key", "value")])));
        same(&["a"], index.get_matches(&fwt(&[("key", "value2"), ("otherkey", "othervalue")])));
        same(&[], index.get_matches(&fwt(&[("key", "")])));
        same(&[], index.get_matches(&feature_with_tags(&[("key", None)])));
        same(&[], index.get_matches(&fwt(&[("key2", "value"), ("key3", "value")])));
        same(&[], index.get_matches(&fwt(&[("key2", "no")])));
        same(&[], index.get_matches(&fwt(&[])));
    }

    #[test]
    fn test_match_field() {
        match_field_check(&of(vec![("a", match_field("key"))]).index());
        match_field_check(&of(vec![("a", not(not(match_field("key"))))]).index());
    }

    #[test]
    fn test_inverse_match_field() {
        let index = of(vec![("a", not(match_field("key")))]).index();
        same(&[], index.get_matches(&fwt(&[("key", "value")])));
        same(&["a"], index.get_matches(&fwt(&[("key", "")])));
        same(&["a"], index.get_matches(&feature_with_tags(&[("key", None)])));
        same(&[], index.get_matches(&fwt(&[("key", "value2"), ("otherkey", "othervalue")])));
        same(&["a"], index.get_matches(&fwt(&[("key2", "value"), ("key3", "value")])));
        same(&["a"], index.get_matches(&fwt(&[("key2", "no")])));
        same(&["a"], index.get_matches(&fwt(&[])));
    }

    #[test]
    fn test_static_boolean_match() {
        let index = of(vec![("t", Expression::TRUE)]).index();
        assert!(index.matches(&fwt(&[("key", "value")])));
        let index = of(vec![("f", Expression::FALSE)]).index();
        assert!(!index.matches(&fwt(&[("key", "value")])));

        let index = of(vec![("a", match_field("key")), ("t", Expression::TRUE), ("f", Expression::FALSE)]).index();
        same(&["a", "t"], index.get_matches(&fwt(&[("key", "value")])));

        let index = of(vec![
            ("a", match_field("key")),
            ("t1", Expression::TRUE),
            ("t2", Expression::TRUE),
            ("t3", Expression::TRUE),
            ("f1", Expression::FALSE),
            ("f2", Expression::FALSE),
            ("f3", Expression::FALSE),
        ])
        .index();
        same(&["a", "t1", "t2", "t3"], index.get_matches(&fwt(&[("key", "value")])));

        let index = of(vec![
            ("t3", Expression::TRUE),
            ("t2", Expression::TRUE),
            ("t1", Expression::TRUE),
            ("a", match_field("key")),
        ])
        .index();
        assert_eq!(vec![&"t3", &"t2", &"t1", &"a"], index.get_matches(&fwt(&[("key", "value")])));
    }

    #[test]
    fn test_wildcards() {
        let index = of(vec![("a", m("key", "%value%"))]).index();
        for v in ["value", "value1", "1value", "1value1"] {
            same(&["a"], index.get_matches(&fwt(&[("key", v)])));
        }
        same(&[], index.get_matches(&fwt(&[("key2", "value")])));
        same(&[], index.get_matches(&fwt(&[("key", "no")])));
        same(&[], index.get_matches(&fwt(&[])));

        let index = of(vec![("a", m("key", "value%"))]).index();
        same(&["a"], index.get_matches(&fwt(&[("key", "value")])));
        same(&["a"], index.get_matches(&fwt(&[("key", "value1")])));
        same(&[], index.get_matches(&fwt(&[("key", "1value")])));

        let index = of(vec![("a", m("key", "%value"))]).index();
        same(&["a"], index.get_matches(&fwt(&[("key", "value")])));
        same(&[], index.get_matches(&fwt(&[("key", "value1")])));
        same(&["a"], index.get_matches(&fwt(&[("key", "1value")])));

        let index = of(vec![("a", match_any("key", ["%value%", "other"]))]).index();
        same(&["a"], index.get_matches(&fwt(&[("key", "1value1")])));
        same(&["a"], index.get_matches(&fwt(&[("key", "other")])));
        same(&[], index.get_matches(&fwt(&[("key", "no")])));
    }

    #[test]
    fn test_multiple_matches() {
        let feature = fwt(&[("a", "b"), ("c", "d")]);
        let index = of(vec![("a", m("a", "b")), ("b", m("c", "d"))]).index();
        let index2 = of(vec![("b", m("c", "d")), ("a", m("a", "b"))]).index();
        same(&["a", "b"], index.get_matches(&feature));
        same(&["b", "a"], index2.get_matches(&feature));
        assert_eq!(&"a", index.get_or_else(&feature, &"miss"));
        assert_eq!(&"b", index2.get_or_else(&feature, &"miss"));
    }

    #[test]
    fn test_and_or_not() {
        let index = of(vec![("a", and([m("key1", "val1"), m("key2", "val2")]))]).index();
        same(&["a"], index.get_matches(&fwt(&[("key1", "val1"), ("key2", "val2")])));
        same(&["a"], index.get_matches(&fwt(&[("key1", "val1"), ("key2", "val2"), ("key3", "val3")])));
        same(&[], index.get_matches(&fwt(&[("key1", "no"), ("key2", "val2")])));
        same(&[], index.get_matches(&fwt(&[("key1", "val1")])));
        same(&[], index.get_matches(&fwt(&[])));

        let index = of(vec![("a", and([Expression::TRUE, m("key1", "val1")]))]).index();
        same(&["a"], index.get_matches(&fwt(&[("key1", "val1")])));
        same(&[], index.get_matches(&fwt(&[("key2", "val2")])));

        let index = of(vec![("a", and([Expression::FALSE, m("key1", "val1")]))]).index();
        same(&[], index.get_matches(&fwt(&[("key1", "val1")])));

        let index = of(vec![("a", or([m("key1", "val1"), m("key2", "val2")]))]).index();
        same(&["a"], index.get_matches(&fwt(&[("key1", "no"), ("key2", "val2")])));
        same(&["a"], index.get_matches(&fwt(&[("key1", "val1")])));
        same(&[], index.get_matches(&fwt(&[("key1", "no"), ("key2", "no")])));
        same(&[], index.get_matches(&fwt(&[])));

        let index = of(vec![("a", or([Expression::TRUE, m("key1", "val1")]))]).index_and_warn();
        same(&["a"], index.get_matches(&fwt(&[("key2", "val2")])));
        same(&["a"], index.get_matches(&fwt(&[])));

        let index = of(vec![("a", or([Expression::FALSE, m("key1", "val1")]))]).index();
        same(&["a"], index.get_matches(&fwt(&[("key1", "val1")])));
        same(&[], index.get_matches(&fwt(&[("key2", "val2")])));

        let index = of(vec![("a", and([m("key1", "val1"), not(m("key2", "val2"))]))]).index();
        same(&["a"], index.get_matches(&fwt(&[("key1", "val1")])));
        same(&[], index.get_matches(&fwt(&[("key1", "val1"), ("key2", "val2")])));
        same(&["a"], index.get_matches(&fwt(&[("key1", "val1"), ("key2", "val3")])));
        same(&[], index.get_matches(&fwt(&[])));
    }

    #[test]
    fn test_nor_nand_xor_xnor() {
        let nor = of(vec![("a", not(or([m("key1", "val1"), m("key2", "val2")])))]).index();
        same(&[], nor.get_matches(&fwt(&[("key1", "val1"), ("key2", "val2")])));
        same(&[], nor.get_matches(&fwt(&[("key1", "val1")])));
        same(&["a"], nor.get_matches(&fwt(&[])));
        same(&["a"], nor.get_matches(&fwt(&[("key1", "no"), ("key2", "no")])));

        let nand = of(vec![("a", not(and([m("key1", "val1"), m("key2", "val2")])))]).index();
        same(&[], nand.get_matches(&fwt(&[("key1", "val1"), ("key2", "val2")])));
        same(&["a"], nand.get_matches(&fwt(&[])));
        same(&["a"], nand.get_matches(&fwt(&[("key1", "val1")])));
        same(&["a"], nand.get_matches(&fwt(&[("key1", "no"), ("key2", "val2")])));

        let xor = of(vec![(
            "a",
            or([and([not(m("key1", "val1")), m("key2", "val2")]), and([m("key1", "val1"), not(m("key2", "val2"))])]),
        )])
        .index();
        same(&[], xor.get_matches(&fwt(&[("key1", "val1"), ("key2", "val2")])));
        same(&["a"], xor.get_matches(&fwt(&[("key1", "no"), ("key2", "val2")])));
        same(&["a"], xor.get_matches(&fwt(&[("key1", "val1")])));
        same(&[], xor.get_matches(&fwt(&[("key1", "no"), ("key2", "no")])));
        same(&[], xor.get_matches(&fwt(&[])));

        let xnor = of(vec![(
            "a",
            or([and([not(m("key1", "val1")), not(m("key2", "val2"))]), and([m("key1", "val1"), m("key2", "val2")])]),
        )])
        .index();
        same(&["a"], xnor.get_matches(&fwt(&[("key1", "val1"), ("key2", "val2")])));
        same(&[], xnor.get_matches(&fwt(&[("key1", "no"), ("key2", "val2")])));
        same(&[], xnor.get_matches(&fwt(&[("key1", "val1")])));
        same(&["a"], xnor.get_matches(&fwt(&[("key1", "no"), ("key2", "no")])));
        same(&["a"], xnor.get_matches(&fwt(&[])));
    }

    #[test]
    fn test_map_and_filter_results() {
        let index = of(vec![("a", match_field("key"))]).map_results(|r| Some(r.to_uppercase())).index();
        assert_eq!(vec![&"A".to_string()], index.get_matches(&fwt(&[("key", "value")])));
        let index =
            of(vec![("a", match_field("key")), ("b", match_field("key"))]).filter_results(|r| *r != "b").index();
        same(&["a"], index.get_matches(&fwt(&[("key", "value")])));
    }

    #[test]
    fn test_matches_multiple() {
        let index = of(vec![
            ("a", or([m("key1", "val1"), m("key2", "val2")])),
            ("b", or([m("key2", "val2"), m("key3", "val3")])),
        ])
        .index();
        same(&["a"], index.get_matches(&fwt(&[("key1", "val1")])));
        same(&["a", "b"], index.get_matches(&fwt(&[("key2", "val2")])));
        same(&["b"], index.get_matches(&fwt(&[("key3", "val3")])));
        same(&["a", "b"], index.get_matches(&fwt(&[("key2", "val2"), ("key3", "val3")])));
        same(&["a", "b"], index.get_matches(&fwt(&[("key1", "val1"), ("key3", "val3")])));
        same(&[], index.get_matches(&fwt(&[])));
    }

    fn keys(k: &[&str]) -> MatchKeys {
        k.iter().map(|s| Arc::from(*s)).collect()
    }

    #[test]
    fn test_tracks_matching_key() {
        let index = of(vec![
            ("a", or([m("key1", "val1"), m("key2", "val2")])),
            ("b", or([m("key2", "val2"), m("key3", "val3")])),
        ])
        .index();
        assert_eq!(
            vec![Match { result: &"a", keys: keys(&["key1"]), id: 1 }],
            index.get_matches_with_triggers(&fwt(&[("key1", "val1")]))
        );
        assert_eq!(
            vec![
                Match { result: &"a", keys: keys(&["key2"]), id: 1 },
                Match { result: &"b", keys: keys(&["key2"]), id: 2 }
            ],
            index.get_matches_with_triggers(&fwt(&[("key2", "val2")]))
        );
        assert_eq!(
            vec![Match { result: &"b", keys: keys(&["key3"]), id: 2 }],
            index.get_matches_with_triggers(&fwt(&[("key3", "val3")]))
        );
    }

    #[test]
    fn test_tracks_matching_key_from_correct_path() {
        let index = of(vec![(
            "a",
            or([and([m("key3", "val3"), m("key2", "val2")]), and([m("key1", "val1"), m("key3", "val3")])]),
        )])
        .index();
        assert_eq!(
            vec![Match { result: &"a", keys: keys(&["key1", "key3"]), id: 1 }],
            index.get_matches_with_triggers(&fwt(&[("key1", "val1"), ("key3", "val3")]))
        );
    }

    #[test]
    fn test_match_different_types() {
        let polygon_expression = and([match_type("polygon"), match_field("field")]);
        let linestring_expression = and([match_type("linestring"), match_field("field")]);
        let point_expression = and([match_type("point"), match_field("field")]);
        let other_expression = match_field("field");
        let feature =
            |t: GeometryType| TestFeature { tags: tags! {"field" => "value"}, geometry_type: t, ..Default::default() };
        let point = feature(GeometryType::Point);
        let linestring = feature(GeometryType::Line);
        let polygon = feature(GeometryType::Polygon);
        let other = OfMap(tags! {"field" => "value"});
        let index = of(vec![
            ("polygon", polygon_expression.clone()),
            ("linestring", linestring_expression.clone()),
            ("point", point_expression.clone()),
            ("other", other_expression.clone()),
        ])
        .index();
        assert!(point_expression.evaluate(&point));
        assert!(linestring_expression.evaluate(&linestring));
        assert!(polygon_expression.evaluate(&polygon));
        assert!(other_expression.evaluate(&other));
        assert_eq!(Some(&"point"), index.get_first(&point));
        assert_eq!(Some(&"linestring"), index.get_first(&linestring));
        assert_eq!(Some(&"polygon"), index.get_first(&polygon));
        assert_eq!(Some(&"other"), index.get_first(&other));
    }

    #[test]
    fn test_match_missing() {
        let index1 = of(vec![("a", or([m("key1", ""), m("key2", "")]))]).index();
        let index2 = of(vec![("a", not(and([match_field("key1"), match_field("key2")])))]).index();
        for index in [index1, index2] {
            same(&["a"], index.get_matches(&fwt(&[])));
            same(&["a"], index.get_matches(&fwt(&[("key1", "value1")])));
            same(&["a"], index.get_matches(&fwt(&[("key2", "value2")])));
            same(&[], index.get_matches(&fwt(&[("key1", "value1"), ("key2", "value2")])));
        }
    }

    #[test]
    fn test_match_not_missing() {
        let index1 = of(vec![("a", match_field("key1"))]).index();
        let index2 = of(vec![("a", not(m("key1", "")))]).index();
        for index in [index1, index2] {
            same(&[], index.get_matches(&fwt(&[])));
            same(&["a"], index.get_matches(&fwt(&[("key1", "value1")])));
            same(&[], index.get_matches(&fwt(&[("key2", "value2")])));
            same(&["a"], index.get_matches(&fwt(&[("key1", "value1"), ("key2", "value2")])));
        }
    }

    #[test]
    fn test_custom_expression() {
        let dont_evaluate = custom(|_, _| panic!("should not evaluate"));
        let match_abc = custom(|input, _| input.has_tag("abc"));
        let index = of(vec![
            ("a", match_abc.clone()),
            ("b", and([match_field("def"), dont_evaluate])),
            ("c", or([match_field("abc"), match_abc])),
        ])
        .index();
        same(&[], index.get_matches(&fwt(&[])));
        same(&[], index.get_matches(&fwt(&[("a", "1")])));
        same(&["a", "c"], index.get_matches(&fwt(&[("abc", "123")])));
        let bad = fwt(&[("def", "123")]);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| index.get_matches(&bad).len())).is_err());
    }

    #[test]
    fn test_and_or_match() {
        let expr = and([or([m("key", "val")]), Expression::TRUE]);
        let index = of(vec![("a", expr.clone())]).index();
        let inputs = [
            (fwt(&[("key", "val")]), true),
            (fwt(&[("key", "val"), ("otherkey", "otherval")]), true),
            (fwt(&[("otherkey", "otherval")]), false),
            (fwt(&[("key", "otherval")]), false),
            (fwt(&[]), false),
        ];
        for (input, expected) in &inputs {
            assert_eq!(*expected, index.matches(input));
            assert_eq!(*expected, expr.evaluate(input));
            assert_eq!(*expected, expr.simplify().evaluate(input));
        }
    }

    fn point(source: &str, layer: &str, tags: Tags) -> TestFeature {
        TestFeature { tags, geometry_type: GeometryType::Point, source: Some(source.into()), layer: Some(layer.into()) }
    }

    #[test]
    fn test_source_filter() {
        let index = of(vec![
            ("a", match_source("a")),
            ("b", and([match_source("b"), match_field("field")])),
            ("c", or([match_field("abc"), match_source_layer("c")])),
        ])
        .index();
        same(&[], index.get_matches(&point("source", "layer", tags! {})));
        same(&["a"], index.get_matches(&point("a", "layer", tags! {})));
        same(&[], index.get_matches(&point("b", "layer", tags! {})));
        same(&["b"], index.get_matches(&point("b", "layer", tags! {"field" => "value"})));
        same(&["c"], index.get_matches(&point("source", "layer", tags! {"abc" => "value"})));
        same(&["c"], index.get_matches(&point("source", "c", tags! {"abc" => "value"})));
        same(&["c"], index.get_matches(&point("source", "c", tags! {})));
    }

    #[test]
    fn test_source_and_geom_filter() {
        let index = of(vec![
            ("a", and([match_geometry_type(GeometryType::Point), match_source("a")])),
            ("b", and([match_geometry_type(GeometryType::Polygon), and([match_source("b"), match_field("field")])])),
            ("c", and([match_geometry_type(GeometryType::Polygon), or([match_field("abc"), match_source_layer("c")])])),
            ("d", and([match_field("field2")])),
        ])
        .index();
        same(&[], index.get_matches(&point("source", "layer", tags! {})));
        same(&["a"], index.get_matches(&point("a", "layer", tags! {})));
        same(&[], index.get_matches(&point("b", "layer", tags! {})));
        same(&[], index.get_matches(&point("b", "layer", tags! {"field" => "value"})));
        same(&[], index.get_matches(&point("source", "layer", tags! {"abc" => "value"})));
        same(&[], index.get_matches(&point("source", "c", tags! {"abc" => "value"})));
        same(&[], index.get_matches(&point("source", "c", tags! {})));
        same(&["d"], index.get_matches(&point("source", "layer", tags! {"field2" => "value"})));
    }

    #[test]
    fn test_match_order() {
        let index = MultiExpression::of_ordered(vec![
            entry("green", m("natural", "tree")),
            entry("black", m("historic", "memorial")),
            entry("green", m("tourism", "viewpoint")),
        ])
        .index();
        assert_eq!(
            vec![&"black", &"green"],
            index.get_matches(&tags! {"historic" => "memorial", "tourism" => "viewpoint"})
        );
        assert_eq!(vec![&"green", &"green"], index.get_matches(&tags! {"natural" => "tree", "tourism" => "viewpoint"}));
    }

    #[test]
    fn test_nested_keys() {
        let index = of(vec![("a", m("a.b", "c")), ("b", match_field("x[].y"))]).index();
        same(&["a"], index.get_matches(&tags! {"a" => Value::map(tags!{"b" => "c"})}));
        same(&["b"], index.get_matches(&tags! {"x" => Value::list([Value::map(tags!{"y" => 1})])}));
        same(&[], index.get_matches(&tags! {"x" => Value::list([Value::map(tags!{"z" => 1})])}));
        assert_eq!("a", parent_key("a.b"));
        assert_eq!("a", parent_key("a[].b"));
        assert_eq!("a.b", parent_key("a.b.c"));
        assert_eq!("", parent_key("a"));
    }

    use crate::value::Value;
}
