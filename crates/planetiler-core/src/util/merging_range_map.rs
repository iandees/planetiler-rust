//! Port of `util/MergingRangeMap.java`: a mapping from disjoint ranges to values that merges values assigned to
//! overlapping ranges.
//!
//! Similar to Guava's `RangeMap`, but merges overlapping values and coalesces adjacent ranges with identical values.
//!
//! ```
//! use planetiler_core::tags;
//! use planetiler_core::util::merging_range_map::MergingRangeMap;
//! let mut map = MergingRangeMap::unit_map();
//! map.put(0.0, 0.5, tags! {"key" => "value"});
//! // overrides value for key from [0.4, 0.5), and sets from [0.5, 1)
//! map.put(0.4, 1.0, tags! {"key" => "value2"});
//! // adds key2 from [0.9, 1)
//! map.put(0.9, 1.0, tags! {"key2" => "value3"});
//! assert_eq!(3, map.result().len());
//! ```

use crate::value::Tags;

/// Subset of the range and value that applies to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Partial<T> {
    pub start: f64,
    pub end: f64,
    pub value: T,
}

type Merger<T> = Box<dyn Fn(&T, &T) -> T + Send + Sync>;

/// A mapping from disjoint `[start, end)` ranges to values.
pub struct MergingRangeMap<T> {
    /// Sorted, disjoint, contiguous ranges covering `[lo, hi)`.
    items: Vec<Partial<T>>,
    merger: Merger<T>,
}

/// Merge two tag maps, with values from `b` overriding values from `a` (Java's `MapUtil.merge`).
pub fn merge_tags(a: &Tags, b: &Tags) -> Tags {
    let mut result = a.clone();
    for (k, v) in b {
        result.insert(k.clone(), v.clone());
    }
    result
}

impl MergingRangeMap<Tags> {
    /// Returns a new range map where values are [`Tags`] maps that get merged together when they overlap from `[0, 1)`.
    pub fn unit_map() -> MergingRangeMap<Tags> {
        Self::unit(Tags::new(), merge_tags)
    }
}

impl<T: Clone + PartialEq> MergingRangeMap<T> {
    /// Returns a new range map with `identity` from `[0, 1)` and `merger` as the default merging function for
    /// [`MergingRangeMap::put`].
    pub fn unit(identity: T, merger: impl Fn(&T, &T) -> T + Send + Sync + 'static) -> MergingRangeMap<T> {
        Self::create(0.0, 1.0, identity, merger)
    }

    /// Returns a new range map with `identity` from `[lo, hi)` and `merger` as the default merging function for
    /// [`MergingRangeMap::put`].
    pub fn create(
        lo: f64,
        hi: f64,
        identity: T,
        merger: impl Fn(&T, &T) -> T + Send + Sync + 'static,
    ) -> MergingRangeMap<T> {
        MergingRangeMap { items: vec![Partial { start: lo, end: hi, value: identity }], merger: Box::new(merger) }
    }

    /// Returns the distinct set of ranges and their values where adjacent ranges with identical values are merged.
    pub fn result(&self) -> Vec<Partial<T>> {
        self.items.clone()
    }

    /// Consumes the map and returns the distinct set of ranges and their values.
    pub fn into_result(self) -> Vec<Partial<T>> {
        self.items
    }

    /// Splits the range containing `at` so that a range boundary exists at `at`.
    fn split_at(items: &mut Vec<Partial<T>>, at: f64) {
        if let Some(idx) = items.iter().position(|p| p.start < at && at < p.end) {
            let mut right = items[idx].clone();
            right.start = at;
            items[idx].end = at;
            items.insert(idx + 1, right);
        }
    }

    fn update_items(items: &mut Vec<Partial<T>>, start: f64, end: f64, operator: impl Fn(&T) -> T) {
        if start.is_nan() || end.is_nan() || start >= end {
            return;
        }
        Self::split_at(items, start);
        Self::split_at(items, end);
        for item in items.iter_mut() {
            if item.start >= start && item.end <= end {
                item.value = operator(&item.value);
            }
        }
        // coalesce adjacent ranges with equal values
        let mut coalesced: Vec<Partial<T>> = Vec::with_capacity(items.len());
        for item in items.drain(..) {
            match coalesced.last_mut() {
                Some(last) if last.value == item.value && last.end == item.start => last.end = item.end,
                _ => coalesced.push(item),
            }
        }
        *items = coalesced;
    }

    /// Change each of the distinct values over `[start, end)` to the result of applying `operator` to the existing
    /// value.
    pub fn update(&mut self, start: f64, end: f64, operator: impl Fn(&T) -> T) {
        Self::update_items(&mut self.items, start, end, operator);
    }

    /// Merge `next` into the value associated with all ranges that overlap `[start, end)` using the default merging
    /// function.
    pub fn put(&mut self, start: f64, end: f64, next: T) {
        let merger = &self.merger;
        Self::update_items(&mut self.items, start, end, |prev| merger(prev, &next));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags;

    fn p(start: f64, end: f64, value: Tags) -> Partial<Tags> {
        Partial { start, end, value }
    }

    #[test]
    fn empty() {
        let map = MergingRangeMap::unit_map();
        assert_eq!(vec![p(0.0, 1.0, tags! {})], map.result());
    }

    #[test]
    fn test_partial_overlap() {
        let mut map = MergingRangeMap::unit_map();
        map.put(0.25, 0.75, tags! {"b" => 3, "c" => 4});
        map.put(0.5, 1.0, tags! {"a" => 1, "b" => 2});
        assert_eq!(
            vec![
                p(0.0, 0.25, tags! {}),
                p(0.25, 0.5, tags! {"b" => 3, "c" => 4}),
                p(0.5, 0.75, tags! {"a" => 1, "b" => 2, "c" => 4}),
                p(0.75, 1.0, tags! {"a" => 1, "b" => 2}),
            ],
            map.result()
        );
    }

    #[test]
    fn test_put_single() {
        let mut map = MergingRangeMap::unit_map();
        map.put(0.25, 0.75, tags! {"b" => 3});
        map.put(0.25, 0.75, tags! {"c" => 4});
        map.put(0.5, 1.0, tags! {"a" => 1});
        map.put(0.5, 1.0, tags! {"b" => 2});
        assert_eq!(
            vec![
                p(0.0, 0.25, tags! {}),
                p(0.25, 0.5, tags! {"b" => 3, "c" => 4}),
                p(0.5, 0.75, tags! {"a" => 1, "b" => 2, "c" => 4}),
                p(0.75, 1.0, tags! {"a" => 1, "b" => 2}),
            ],
            map.result()
        );
    }

    #[test]
    fn test_duplicate_keys() {
        let mut map = MergingRangeMap::unit_map();
        map.put(0.25, 0.75, tags! {"a" => 1});
        map.put(0.5, 1.0, tags! {"a" => 1});
        assert_eq!(vec![p(0.0, 0.25, tags! {}), p(0.25, 1.0, tags! {"a" => 1})], map.result());
    }
}
