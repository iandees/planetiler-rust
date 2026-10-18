//! Port of `collection/LongMerger.java`: a utility for merging sorted lists of items with an `i64` key to sort by.
//!
//! Keys are compared as **unsigned** 64-bit integers; ties are broken by a caller-provided comparator. Has a
//! general-purpose k-way merge implementation using a min-heap and specialized (faster) two-way/three-way merges when
//! a small number of lists are being merged.

use std::cmp::Ordering;
use std::iter::FromFn;

use super::array_long_min_heap::RawLongMinHeap;
use super::sortable_feature::HasLongSortKey;

/// Unsigned max: sentinel for exhausted iterators so they sort last in unsigned comparison.
const MAX_UNSIGNED: u64 = u64::MAX;

/// Merges sorted items from suppliers that return `None` when there are no items left.
pub fn merge_suppliers<T, S, F>(suppliers: Vec<S>, tie_breaker: F) -> LongMerger<T, FromFn<S>, F>
where
    T: HasLongSortKey,
    S: FnMut() -> Option<T>,
    F: FnMut(&T, &T) -> Ordering,
{
    merge_iterators(suppliers.into_iter().map(std::iter::from_fn).collect(), tie_breaker)
}

/// Merges sorted iterators into a combined iterator over all the items.
pub fn merge_iterators<T, I, F>(iterators: Vec<I>, mut tie_breaker: F) -> LongMerger<T, I, F>
where
    T: HasLongSortKey,
    I: Iterator<Item = T>,
    F: FnMut(&T, &T) -> Ordering,
{
    let inner = match iterators.len() {
        0 => Inner::Empty,
        1 => Inner::One(iterators.into_iter().next().unwrap()),
        2 => {
            let mut it = iterators.into_iter();
            let a = Source::new(it.next().unwrap());
            let b = Source::new(it.next().unwrap());
            Inner::Two(a, b)
        }
        3 => {
            let mut it = iterators.into_iter();
            let a = Source::new(it.next().unwrap());
            let b = Source::new(it.next().unwrap());
            let c = Source::new(it.next().unwrap());
            Inner::Three(a, b, c)
        }
        _ => Inner::K(KWayMerge::new(iterators, &mut tie_breaker)),
    };
    LongMerger { inner, tie_breaker }
}

/// Iterator over the merged items, see [`merge_iterators`].
pub struct LongMerger<T, I, F> {
    inner: Inner<T, I>,
    tie_breaker: F,
}

enum Inner<T, I> {
    Empty,
    One(I),
    Two(Source<T, I>, Source<T, I>),
    Three(Source<T, I>, Source<T, I>, Source<T, I>),
    K(KWayMerge<T, I>),
}

/// An input iterator plus its current head item and key.
struct Source<T, I> {
    item: Option<T>,
    key: u64,
    input: I,
}

impl<T: HasLongSortKey, I: Iterator<Item = T>> Source<T, I> {
    fn new(mut input: I) -> Source<T, I> {
        let item = input.next();
        let key = item.as_ref().map_or(MAX_UNSIGNED, |i| i.key() as u64);
        Source { item, key, input }
    }

    /// Returns the current item and advances to the next one.
    #[inline]
    fn advance(&mut self) -> Option<T> {
        let next = self.input.next();
        self.key = next.as_ref().map_or(MAX_UNSIGNED, |i| i.key() as u64);
        std::mem::replace(&mut self.item, next)
    }
}

#[inline]
fn less_than<T, I, F: FnMut(&T, &T) -> Ordering>(a: &Source<T, I>, b: &Source<T, I>, tie: &mut F) -> bool {
    a.key < b.key
        || (a.key == b.key
            && match (&a.item, &b.item) {
                // nulls go at the end
                (None, _) => false,
                (Some(_), None) => true,
                (Some(x), Some(y)) => tie(x, y) == Ordering::Less,
            })
}

struct KWayMerge<T, I> {
    items: Vec<Option<T>>,
    iterators: Vec<I>,
    heap: RawLongMinHeap,
}

impl<T: HasLongSortKey, I: Iterator<Item = T>> KWayMerge<T, I> {
    fn new<F: FnMut(&T, &T) -> Ordering>(inputs: Vec<I>, tie: &mut F) -> KWayMerge<T, I> {
        let n = inputs.len();
        let mut items = Vec::with_capacity(n);
        let mut iterators = Vec::with_capacity(n);
        for mut iter in inputs {
            if let Some(item) = iter.next() {
                items.push(Some(item));
                iterators.push(iter);
            }
        }
        let mut heap = RawLongMinHeap::new(items.len(), true);
        {
            let items = &items;
            let mut cmp = |a: usize, b: usize| tie(items[a].as_ref().unwrap(), items[b].as_ref().unwrap());
            for (i, item) in items.iter().enumerate() {
                heap.push(i, item.as_ref().unwrap().key(), &mut cmp);
            }
        }
        KWayMerge { heap, items, iterators }
    }
}

impl<T, I, F> LongMerger<T, I, F>
where
    T: HasLongSortKey,
    I: Iterator<Item = T>,
    F: FnMut(&T, &T) -> Ordering,
{
    #[inline]
    fn next_k(k: &mut KWayMerge<T, I>, tie: &mut F) -> Option<T> {
        let KWayMerge { items, iterators, heap } = k;
        if heap.is_empty() {
            return None;
        }
        let id = heap.peek_id();
        match iterators[id].next() {
            Some(next) => {
                let key = next.key();
                let result = items[id].replace(next);
                let items = &*items;
                heap.update_head(key, &mut |a, b| tie(items[a].as_ref().unwrap(), items[b].as_ref().unwrap()));
                result
            }
            None => {
                {
                    let items = &*items;
                    heap.poll(&mut |a, b| tie(items[a].as_ref().unwrap(), items[b].as_ref().unwrap()));
                }
                items[id].take()
            }
        }
    }
}

impl<T, I, F> Iterator for LongMerger<T, I, F>
where
    T: HasLongSortKey,
    I: Iterator<Item = T>,
    F: FnMut(&T, &T) -> Ordering,
{
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<T> {
        let tie = &mut self.tie_breaker;
        match &mut self.inner {
            Inner::Empty => None,
            Inner::One(i) => i.next(),
            Inner::Two(a, b) => {
                if less_than(a, b, tie) {
                    a.advance()
                } else {
                    b.advance()
                }
            }
            Inner::Three(a, b, c) => {
                // use at most 2 comparisons to get the next item
                if less_than(a, b, tie) {
                    if less_than(a, c, tie) { a.advance() } else { c.advance() }
                } else if less_than(c, b, tie) {
                    c.advance()
                } else {
                    b.advance()
                }
            }
            Inner::K(k) => Self::next_k(k, tie),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::array_long_min_heap::tests::Rng;
    use crate::collection::sortable_feature::SortableFeature;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Item {
        key: i64,
        secondary: i32,
    }

    impl HasLongSortKey for Item {
        fn key(&self) -> i64 {
            self.key
        }
    }

    impl Item {
        fn value(&self) -> i64 {
            self.key + self.secondary as i64
        }
    }

    fn list(primary_key: bool, items: &[i64]) -> Vec<Item> {
        items
            .iter()
            .map(|&i| if primary_key { Item { key: i, secondary: 0 } } else { Item { key: 0, secondary: i as i32 } })
            .collect()
    }

    fn merge(lists: &[&Vec<Item>]) -> Vec<i64> {
        let mut iter =
            merge_iterators(lists.iter().map(|l| l.iter().copied()).collect(), |a: &Item, b: &Item| a.cmp(b));
        let result: Vec<i64> = iter.by_ref().map(|i| i.value()).collect();
        assert!(iter.next().is_none());
        result
    }

    fn parse(s: &str) -> Vec<i64> {
        s.split_whitespace().map(|p| p.parse().unwrap()).collect()
    }

    #[test]
    fn merge_empty() {
        assert_eq!(Vec::<i64>::new(), merge(&[]));
    }

    #[test]
    fn merge_supplier() {
        for primary in [true, false] {
            let items = list(primary, &[1, 2]);
            let mut it = items.into_iter();
            let mut iter = merge_suppliers(vec![move || it.next()], |a: &Item, b: &Item| a.cmp(b));
            let result: Vec<i64> = iter.by_ref().map(|i| i.value()).collect();
            assert!(iter.next().is_none());
            assert_eq!(vec![1, 2], result);
        }
    }

    #[test]
    fn merge1() {
        for primary in [true, false] {
            assert_eq!(Vec::<i64>::new(), merge(&[&list(primary, &[])]));
            assert_eq!(vec![1], merge(&[&list(primary, &[1])]));
            assert_eq!(vec![1, 2], merge(&[&list(primary, &[1, 2])]));
        }
    }

    #[test]
    fn merge2() {
        for (a, b, out) in [
            ("", "", ""),
            ("1", "", "1"),
            ("1", "1", "1 1"),
            ("1 2", "", "1 2"),
            ("1 2", "2 3", "1 2 2 3"),
            ("1", "2", "1 2"),
            ("1 2", "3", "1 2 3"),
            ("1 3", "2", "1 2 3"),
        ] {
            for primary in [false, true] {
                let la = list(primary, &parse(a));
                let lb = list(primary, &parse(b));
                assert_eq!(parse(out), merge(&[&la, &lb]), "primary={primary}");
                assert_eq!(parse(out), merge(&[&lb, &la]), "primary={primary}");
            }
        }
    }

    #[test]
    fn merge3() {
        for (a, b, c, out) in [
            ("", "", "", ""),
            ("1", "", "", "1"),
            ("1", "1", "1", "1 1 1"),
            ("1 2", "", "", "1 2"),
            ("1 2", "2 3", "", "1 2 2 3"),
            ("1", "2", "", "1 2"),
            ("1", "2", "3", "1 2 3"),
            ("1 2", "3", "4", "1 2 3 4"),
            ("1 3", "2", "4", "1 2 3 4"),
        ] {
            for primary in [false, true] {
                let la = list(primary, &parse(a));
                let lb = list(primary, &parse(b));
                let lc = list(primary, &parse(c));
                for order in [
                    [&la, &lb, &lc],
                    [&la, &lc, &lb],
                    [&lb, &la, &lc],
                    [&lb, &lc, &la],
                    [&lc, &la, &lb],
                    [&lc, &lb, &la],
                ] {
                    assert_eq!(parse(out), merge(&order), "primary={primary}");
                }
            }
        }
    }

    #[test]
    fn merge4() {
        for (a, b, c, d, out) in [
            ("", "", "", "", ""),
            ("1", "", "", "", "1"),
            ("1", "1", "1", "1", "1 1 1 1"),
            ("1 2", "", "", "", "1 2"),
            ("1 2", "3", "", "", "1 2 3"),
            ("1 3", "2", "", "", "1 2 3"),
            ("1 3", "2 4", "", "", "1 2 3 4"),
            ("1 5", "2 4", "", "", "1 2 4 5"),
            ("1 2", "2 3", "", "", "1 2 2 3"),
        ] {
            for primary in [false, true] {
                let la = list(primary, &parse(a));
                let lb = list(primary, &parse(b));
                let lc = list(primary, &parse(c));
                let ld = list(primary, &parse(d));
                for order in [[&la, &lb, &lc, &ld], [&lb, &la, &lc, &ld], [&lb, &lc, &la, &ld], [&lb, &lc, &ld, &la]] {
                    assert_eq!(parse(out), merge(&order), "primary={primary}");
                }
            }
        }
    }

    #[test]
    fn stress_test() {
        for n in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 20, 100] {
            let mut random = Rng::new(0);
            let lists: Vec<Vec<SortableFeature>> = (0..n)
                .map(|_| {
                    let mut list: Vec<SortableFeature> = (0..10)
                        .map(|_| {
                            let key = random.next_int(20) as i64;
                            SortableFeature::new(key, vec![random.next_int(256) as u8])
                        })
                        .collect();
                    list.sort();
                    list
                })
                .collect();
            let mut iter =
                merge_iterators(lists.iter().map(|l| l.iter().cloned()).collect(), SortableFeature::compare_bytes);
            let mut last = iter.next().unwrap();
            let mut count = 1;
            for item in iter {
                count += 1;
                assert!(last <= item, "items out of order i={count} lists={n} last={last:?} item={item:?}");
                last = item;
            }
            assert_eq!(n * 10, count);
        }
    }

    #[test]
    fn merge_unsigned_key_order() {
        let low = vec![Item { key: 1, secondary: 0 }, Item { key: 100, secondary: 0 }];
        let high =
            vec![Item { key: i64::MAX, secondary: 0 }, Item { key: -2, secondary: 0 }, Item { key: -1, secondary: 0 }];
        let result: Vec<i64> = merge_iterators(vec![low.into_iter(), high.into_iter()], |a: &Item, b: &Item| a.cmp(b))
            .map(|i| i.key)
            .collect();
        assert_eq!(vec![1, 100, i64::MAX, -2, -1], result);
    }

    #[test]
    fn merge_unsigned_key_order_n_way() {
        for n in 2..=5 {
            let mut lists = vec![
                vec![Item { key: 1, secondary: 0 }, Item { key: -1, secondary: 0 }],
                vec![Item { key: 2, secondary: 0 }, Item { key: -2, secondary: 0 }],
            ];
            for i in 2..n {
                lists.push(vec![Item { key: i + 1, secondary: 0 }]);
            }
            let result: Vec<i64> =
                merge_iterators(lists.into_iter().map(|l| l.into_iter()).collect(), |a: &Item, b: &Item| a.cmp(b))
                    .map(|i| i.key)
                    .collect();
            let mut prev = 0u64;
            for key in &result {
                assert!(prev <= *key as u64, "keys out of unsigned order in {result:?}");
                prev = *key as u64;
            }
            assert_eq!(n as usize + 2, result.len());
        }
    }
}
