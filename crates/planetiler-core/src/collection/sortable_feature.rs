//! Port of `collection/SortableFeature.java` (and `HasLongSortKey.java`): an encoded feature with an `i64` sort key.

use std::cmp::Ordering;
use std::fmt;

/// An item with an `i64` key that can be used for sorting/grouping (Java `HasLongSortKey`).
///
/// Keys are compared as **unsigned** 64-bit integers by [`FeatureSort`](super::feature_sort::FeatureSort) and
/// [`LongMerger`](super::long_merger).
pub trait HasLongSortKey {
    /// Value to sort/group items by.
    fn key(&self) -> i64;
}

/// A feature with a sort key and opaque encoded value bytes.
///
/// Ordering compares `key` as unsigned, then `value` lexicographically as unsigned bytes.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct SortableFeature {
    pub key: i64,
    pub value: Vec<u8>,
}

impl SortableFeature {
    pub fn new(key: i64, value: Vec<u8>) -> SortableFeature {
        SortableFeature { key, value }
    }

    /// Compares only the value bytes, as unsigned (Java `SortableFeature.COMPARE_BYTES`).
    #[inline]
    pub fn compare_bytes(a: &SortableFeature, b: &SortableFeature) -> Ordering {
        a.value.cmp(&b.value)
    }
}

impl HasLongSortKey for SortableFeature {
    #[inline]
    fn key(&self) -> i64 {
        self.key
    }
}

impl Ord for SortableFeature {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        (self.key as u64).cmp(&(other.key as u64)).then_with(|| self.value.cmp(&other.value))
    }
}

impl PartialOrd for SortableFeature {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for SortableFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SortableFeature{{key={}, value={:?}}}", self.key, self.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_ordering() {
        let a = SortableFeature::new(1, vec![]);
        let b = SortableFeature::new(-1, vec![]);
        assert!(a < b);
        let c = SortableFeature::new(1, vec![0xff]);
        let d = SortableFeature::new(1, vec![0x01, 0x00]);
        assert!(d < c);
        assert_eq!(Ordering::Greater, SortableFeature::compare_bytes(&c, &d));
    }
}
