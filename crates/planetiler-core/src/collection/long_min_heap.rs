//! Port of `collection/LongMinHeap.java`: API for min-heaps that keep track of integer ids in a range `[0, size)`
//! ordered by `i64` values.
//!
//! Ported from [GraphHopper](https://github.com/graphhopper/graphhopper/blob/master/core/src/main/java/com/graphhopper/coll/MinHeapWithUpdate.java)
//! (Apache 2.0) and modified to extract a common interface for implementations.
//!
//! Programming errors (out-of-range ids, pushing an id twice, overflowing the heap) panic, where Java throws.

use std::cmp::Ordering;

pub use super::array_long_min_heap::ArrayLongMinHeap;

/// A min-heap of ids ordered by `i64` values.
pub trait LongMinHeap {
    fn size(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.size() == 0
    }

    /// Adds an element to the heap. The id must be less than the capacity specified in the constructor. It is illegal
    /// to push the same id twice (unless it was polled/removed before); use [`LongMinHeap::update`] to change values.
    fn push(&mut self, id: usize, value: i64);

    /// Returns true if the heap contains an element with the given id.
    fn contains(&self, id: usize) -> bool;

    /// Updates the element with the given id in O(log(N)). It is illegal to update elements not in the heap.
    fn update(&mut self, id: usize, value: i64);

    /// Updates the value of the head element, pushing it down and bubbling up the new min element if necessary.
    fn update_head(&mut self, value: i64);

    /// The id of the next element to be polled.
    fn peek_id(&self) -> usize;

    fn peek_value(&self) -> i64;

    /// Removes and returns the id of the element with the minimum value.
    fn poll(&mut self) -> usize;

    fn clear(&mut self);
}

/// Returns a new min-heap where each element has 4 children backed by elements in an array. Ties are broken by
/// `tie_breaker(id1, id2)`.
pub fn new_array_heap<F: FnMut(usize, usize) -> Ordering>(elements: usize, tie_breaker: F) -> ArrayLongMinHeap<F> {
    ArrayLongMinHeap::new(elements, tie_breaker, false)
}

/// Returns a new min-heap that compares values as unsigned 64-bit integers (e.g. tile IDs that may set bit 63).
pub fn new_unsigned_array_heap<F: FnMut(usize, usize) -> Ordering>(
    elements: usize,
    tie_breaker: F,
) -> ArrayLongMinHeap<F> {
    ArrayLongMinHeap::new(elements, tie_breaker, true)
}
