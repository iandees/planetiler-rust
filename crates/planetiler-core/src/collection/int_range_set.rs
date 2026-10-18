//! Port of `collection/IntRangeSet.java`: a set of ints that efficiently represents large continuous ranges.
//!
//! Java wraps a `RoaringBitmap`; in Rust the set is a sorted list of disjoint, non-adjacent half-open ranges, which is
//! compact for the use case (iterating through tile coordinates inside ocean polygons) and makes set operations
//! linear in the number of ranges.
//!
//! NOTE: `RoaringBitmap` orders ints as unsigned; this implementation orders them as signed. They only differ for
//! negative values, which are not used (tile coordinates are non-negative).

use std::fmt;

/// A set of `i32` values stored as ranges.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct IntRangeSet {
    /// Sorted, disjoint, non-adjacent `[start, end)` ranges.
    ranges: Vec<(i64, i64)>,
}

impl IntRangeSet {
    pub fn new() -> IntRangeSet {
        IntRangeSet { ranges: Vec::new() }
    }

    /// Mutates and returns this range set, adding all elements in `other` to it.
    pub fn add_all(&mut self, other: &IntRangeSet) -> &mut Self {
        if self.ranges.is_empty() {
            self.ranges.clone_from(&other.ranges);
        } else if !other.ranges.is_empty() {
            self.ranges = combine(&self.ranges, &other.ranges, |a, b| a || b);
        }
        self
    }

    /// Mutates and returns this range set, removing all elements in `other` from it.
    pub fn remove_all(&mut self, other: &IntRangeSet) -> &mut Self {
        if !self.ranges.is_empty() && !other.ranges.is_empty() {
            self.ranges = combine(&self.ranges, &other.ranges, |a, b| a && !b);
        }
        self
    }

    /// Mutates and returns this range set to remove all elements not in `other`.
    pub fn intersect(&mut self, other: &IntRangeSet) -> &mut Self {
        if !self.ranges.is_empty() {
            self.ranges = combine(&self.ranges, &other.ranges, |a, b| a && b);
        }
        self
    }

    /// Mutates and returns this range set, with range `a` to `b` (inclusive) added.
    pub fn add(&mut self, a: i32, b: i32) -> &mut Self {
        let (start, end) = (a as i64, b as i64 + 1);
        if start >= end {
            return self;
        }
        // first range that ends at or after start (adjacent ranges merge)
        let lo = self.ranges.partition_point(|r| r.1 < start);
        // first range that starts after end (adjacent ranges merge)
        let hi = self.ranges.partition_point(|r| r.0 <= end);
        if lo == hi {
            self.ranges.insert(lo, (start, end));
        } else {
            let merged = (start.min(self.ranges[lo].0), end.max(self.ranges[hi - 1].1));
            self.ranges.splice(lo..hi, std::iter::once(merged));
        }
        self
    }

    /// Mutates and returns this range set, with range `a` to `b` (inclusive) removed.
    pub fn remove(&mut self, a: i32, b: i32) -> &mut Self {
        let (start, end) = (a as i64, b as i64 + 1);
        if start >= end {
            return self;
        }
        // ranges that overlap [start, end)
        let lo = self.ranges.partition_point(|r| r.1 <= start);
        let hi = self.ranges.partition_point(|r| r.0 < end);
        if lo >= hi {
            return self;
        }
        let first = self.ranges[lo];
        let last = self.ranges[hi - 1];
        let mut replacement = Vec::with_capacity(2);
        if first.0 < start {
            replacement.push((first.0, start));
        }
        if last.1 > end {
            replacement.push((end, last.1));
        }
        self.ranges.splice(lo..hi, replacement);
        self
    }

    /// Mutates and returns this range set, with `a` removed.
    pub fn remove_one(&mut self, a: i32) -> &mut Self {
        self.remove(a, a)
    }

    pub fn contains(&self, y: i32) -> bool {
        let y = y as i64;
        let idx = self.ranges.partition_point(|r| r.1 <= y);
        idx < self.ranges.len() && self.ranges[idx].0 <= y
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Number of ints in this set.
    pub fn cardinality(&self) -> u64 {
        self.ranges.iter().map(|r| (r.1 - r.0) as u64).sum()
    }

    /// Mutates and returns this range set, with each element from `start` to `end` (inclusive) added if missing, and
    /// removed if present.
    pub fn xor(&mut self, start: i32, end: i32) -> &mut Self {
        if start <= end {
            self.ranges = combine(&self.ranges, &[(start as i64, end as i64 + 1)], |a, b| a != b);
        }
        self
    }

    /// Iterates over the inclusive `(start, end)` ranges in this set, in ascending order (replaces Java's access to
    /// the underlying `RoaringBitmap`).
    pub fn ranges(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
        self.ranges.iter().map(|&(s, e)| (s as i32, (e - 1) as i32))
    }

    /// Iterates through all ints in this set in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = i32> + '_ {
        self.ranges.iter().flat_map(|&(s, e)| (s..e).map(|i| i as i32))
    }
}

/// Sweeps the boundaries of two range lists and returns the ranges where `op(in_a, in_b)` holds.
fn combine(a: &[(i64, i64)], b: &[(i64, i64)], op: impl Fn(bool, bool) -> bool) -> Vec<(i64, i64)> {
    let mut result: Vec<(i64, i64)> = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    // boundary positions: for each list, whether we are inside a range is determined by the next boundary
    let boundary = |list: &[(i64, i64)], idx: usize| -> i64 {
        let r = idx / 2;
        if r >= list.len() {
            i64::MAX
        } else if idx.is_multiple_of(2) {
            list[r].0
        } else {
            list[r].1
        }
    };
    let mut start: Option<i64> = None;
    loop {
        let pa = boundary(a, i);
        let pb = boundary(b, j);
        let pos = pa.min(pb);
        if pos == i64::MAX {
            break;
        }
        if pa == pos {
            i += 1;
        }
        if pb == pos {
            j += 1;
        }
        // odd index means we're inside a range
        let inside = op(i % 2 == 1, j % 2 == 1);
        match (inside, start) {
            (true, None) => start = Some(pos),
            (false, Some(s)) => {
                if s < pos {
                    match result.last_mut() {
                        Some(last) if last.1 == s => last.1 = pos,
                        _ => result.push((s, pos)),
                    }
                }
                start = None;
            }
            _ => {}
        }
    }
    result
}

impl<'a> IntoIterator for &'a IntRangeSet {
    type Item = i32;
    type IntoIter = Box<dyn Iterator<Item = i32> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl fmt::Debug for IntRangeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{")?;
        for (i, (s, e)) in self.ranges().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }
            if s == e {
                write!(f, "{s}")?;
            } else {
                write!(f, "{s}-{e}")?;
            }
        }
        write!(f, "}}")
    }
}

impl fmt::Display for IntRangeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ints(range: &IntRangeSet) -> Vec<i32> {
        range.iter().collect()
    }

    #[test]
    fn empty() {
        assert_eq!(Vec::<i32>::new(), ints(&IntRangeSet::new()));
    }

    #[test]
    fn single() {
        let mut range = IntRangeSet::new();
        range.add(1, 1);
        assert_eq!(vec![1], ints(&range));
    }

    #[test]
    fn longer() {
        let mut range = IntRangeSet::new();
        range.add(1, 3);
        assert_eq!(vec![1, 2, 3], ints(&range));
    }

    #[test]
    fn two_ranges() {
        let mut range = IntRangeSet::new();
        range.add(1, 3);
        range.add(5, 7);
        assert_eq!(vec![1, 2, 3, 5, 6, 7], ints(&range));
    }

    #[test]
    fn two_overlapping() {
        let mut range = IntRangeSet::new();
        range.add(1, 5);
        range.add(4, 7);
        assert_eq!(vec![1, 2, 3, 4, 5, 6, 7], ints(&range));
    }

    #[test]
    fn remove_single() {
        let mut range = IntRangeSet::new();
        range.add(1, 5);
        range.remove_one(3);
        assert_eq!(vec![1, 2, 4, 5], ints(&range));
    }

    #[test]
    fn remove_other_range() {
        let mut range = IntRangeSet::new();
        range.add(1, 5);
        let mut range2 = IntRangeSet::new();
        range2.add(4, 6);
        range.remove_all(&range2);
        assert_eq!(vec![1, 2, 3], ints(&range));
    }

    #[test]
    fn add_other_range() {
        let mut range = IntRangeSet::new();
        range.add(1, 5);
        let mut range2 = IntRangeSet::new();
        range2.add(8, 10);
        range.add_all(&range2);
        assert_eq!(vec![1, 2, 3, 4, 5, 8, 9, 10], ints(&range));
    }

    #[test]
    fn contains() {
        let mut range = IntRangeSet::new();
        range.add(1, 5);
        let mut range2 = IntRangeSet::new();
        range2.add(8, 10);
        range.add_all(&range2);
        range.remove_one(3);
        for (num, val) in [
            (1, true),
            (2, true),
            (3, false),
            (4, true),
            (5, true),
            (6, false),
            (7, false),
            (8, true),
            (9, true),
            (10, true),
            (11, false),
        ] {
            assert_eq!(val, range.contains(num), "{num}");
        }
    }

    #[test]
    fn no_intersection() {
        let mut range = IntRangeSet::new();
        range.add(1, 5);
        let mut range2 = IntRangeSet::new();
        range2.add(8, 10);
        range.intersect(&range2);
        assert_eq!(Vec::<i32>::new(), ints(&range));
    }

    #[test]
    fn single_intersection() {
        let mut range = IntRangeSet::new();
        range.add(1, 5);
        let mut range2 = IntRangeSet::new();
        range2.add(5, 10);
        range.intersect(&range2);
        assert_eq!(vec![5], ints(&range));
    }

    #[test]
    fn multiple_intersection() {
        let mut range = IntRangeSet::new();
        range.add(1, 5);
        let mut range2 = IntRangeSet::new();
        range2.add(3, 10);
        range2.remove_one(4);
        range.intersect(&range2);
        assert_eq!(vec![3, 5], ints(&range));
    }

    #[test]
    fn is_empty() {
        let mut range = IntRangeSet::new();
        assert!(range.is_empty());
        range.add(1, 5);
        assert!(!range.is_empty());
        let mut other = IntRangeSet::new();
        other.add(6, 10);
        range.intersect(&other);
        assert!(range.is_empty());
    }

    #[test]
    fn xor() {
        let mut range = IntRangeSet::new();
        assert_eq!(Vec::<i32>::new(), ints(&range));
        range.xor(1, 5);
        assert_eq!(vec![1, 2, 3, 4, 5], ints(&range));
        range.xor(1, 5);
        assert_eq!(Vec::<i32>::new(), ints(&range));
        range.xor(1, 5);
        assert_eq!(vec![1, 2, 3, 4, 5], ints(&range));
        range.xor(3, 6);
        assert_eq!(vec![1, 2, 6], ints(&range));
    }

    #[test]
    fn adjacent_ranges_merge() {
        let mut range = IntRangeSet::new();
        range.add(1, 2);
        range.add(3, 4);
        range.add(10, 12);
        range.add(5, 9);
        assert_eq!(vec![(1, 12)], range.ranges().collect::<Vec<_>>());
        range.remove(4, 5);
        assert_eq!(vec![(1, 3), (6, 12)], range.ranges().collect::<Vec<_>>());
        assert_eq!(10, range.cardinality());
        let mut big = IntRangeSet::new();
        big.add(0, i32::MAX);
        assert!(big.contains(i32::MAX));
        assert_eq!(i32::MAX as u64 + 1, big.cardinality());
    }

    #[test]
    fn matches_naive_set() {
        use crate::collection::array_long_min_heap::tests::Rng;
        let mut rng = Rng::new(42);
        let mut set = IntRangeSet::new();
        let mut naive = std::collections::BTreeSet::new();
        for _ in 0..2000 {
            let a = rng.next_int(200) as i32;
            let b = a + rng.next_int(20) as i32;
            match rng.next_int(4) {
                0 => {
                    set.add(a, b);
                    naive.extend(a..=b);
                }
                1 => {
                    set.remove(a, b);
                    for i in a..=b {
                        naive.remove(&i);
                    }
                }
                2 => {
                    set.xor(a, b);
                    for i in a..=b {
                        if !naive.remove(&i) {
                            naive.insert(i);
                        }
                    }
                }
                _ => {
                    let mut other = IntRangeSet::new();
                    other.add(a, b);
                    set.intersect(&other.add(b + 5, b + 50).clone());
                    naive.retain(|&i| (a..=b).contains(&i) || (b + 5..=b + 50).contains(&i));
                }
            }
            assert_eq!(naive.iter().copied().collect::<Vec<_>>(), ints(&set));
        }
    }
}
