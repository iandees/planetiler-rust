//! Port of `collection/ArrayLongMinHeap.java`: a min-heap stored in an array where each element has 4 children.
//!
//! This is about 5-10% faster than the standard binary min-heap for the case of merging sorted lists.
//!
//! Ported from [GraphHopper](https://github.com/graphhopper/graphhopper/blob/master/core/src/main/java/com/graphhopper/coll/MinHeapWithUpdate.java)
//! (Apache 2.0) and modified to use `i64` values, 4 children per element, and optional unsigned comparison.
//!
//! See <https://en.wikipedia.org/wiki/D-ary_heap>.

use std::cmp::Ordering;

use super::long_min_heap::LongMinHeap;

/// Position value meaning "not in the heap" (positions are 1-based).
const NOT_PRESENT: u32 = 0;

/// The heap data without a stored tie-breaker: every operation takes the tie-breaker as an argument so that callers
/// (like the k-way merge in [`LongMerger`](super::long_merger)) can compare items they own without borrow conflicts.
///
/// For unsigned heaps, values are stored with the sign bit flipped so signed comparison gives unsigned order.
#[derive(Clone, Debug)]
pub struct RawLongMinHeap {
    // we use an offset of one to make the arithmetic a bit simpler/more efficient, the 0th elements are not used
    pos_to_id: Vec<u32>,
    id_to_pos: Vec<u32>,
    pos_to_value: Vec<i64>,
    max: usize,
    size: usize,
    flip: i64,
}

#[inline(always)]
fn first_child(index: usize) -> usize {
    (index << 2) - 2
}

#[inline(always)]
fn parent(index: usize) -> usize {
    (index + 2) >> 2
}

impl RawLongMinHeap {
    /// Creates a heap that can hold ids in `[0, elements)`.
    pub fn new(elements: usize, unsigned: bool) -> RawLongMinHeap {
        assert!(elements < u32::MAX as usize, "too many elements: {elements}");
        RawLongMinHeap {
            pos_to_id: vec![0; elements + 1],
            id_to_pos: vec![NOT_PRESENT; elements + 1],
            pos_to_value: vec![i64::MIN; elements + 1],
            max: elements,
            size: 0,
            flip: if unsigned { i64::MIN } else { 0 },
        }
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    #[inline]
    #[track_caller]
    fn check_id_in_range(&self, id: usize) {
        if id >= self.max {
            panic!("Illegal id: {id}, legal range: [0, {}[", self.max);
        }
    }

    #[track_caller]
    pub fn push<F: FnMut(usize, usize) -> Ordering>(&mut self, id: usize, value: i64, tie: &mut F) {
        self.check_id_in_range(id);
        if self.size == self.max {
            panic!("Cannot push anymore, the heap is already full. size: {}", self.size);
        }
        if self.contains(id) {
            panic!(
                "Element with id: {id} was pushed already, you need to use the update method if you want to change its value"
            );
        }
        self.size += 1;
        let size = self.size;
        self.pos_to_id[size] = id as u32;
        self.id_to_pos[id] = size as u32;
        self.pos_to_value[size] = value ^ self.flip;
        self.percolate_up(size, tie);
    }

    #[inline]
    #[track_caller]
    pub fn contains(&self, id: usize) -> bool {
        self.check_id_in_range(id);
        self.id_to_pos[id] != NOT_PRESENT
    }

    #[track_caller]
    pub fn update<F: FnMut(usize, usize) -> Ordering>(&mut self, id: usize, value: i64, tie: &mut F) {
        self.check_id_in_range(id);
        let pos = self.id_to_pos[id] as usize;
        if pos == NOT_PRESENT as usize {
            panic!("The heap does not contain: {id}. Use the contains method to check this before calling update");
        }
        let value = value ^ self.flip;
        let prev = self.pos_to_value[pos];
        self.pos_to_value[pos] = value;
        match value.cmp(&prev).then_with(|| tie(id, self.pos_to_id[pos] as usize)) {
            Ordering::Greater => self.percolate_down(pos, tie),
            Ordering::Less => self.percolate_up(pos, tie),
            Ordering::Equal => {}
        }
    }

    #[inline]
    pub fn update_head<F: FnMut(usize, usize) -> Ordering>(&mut self, value: i64, tie: &mut F) {
        self.pos_to_value[1] = value ^ self.flip;
        self.percolate_down(1, tie);
    }

    #[inline]
    pub fn peek_id(&self) -> usize {
        self.pos_to_id[1] as usize
    }

    #[inline]
    pub fn peek_value(&self) -> i64 {
        self.pos_to_value[1] ^ self.flip
    }

    pub fn poll<F: FnMut(usize, usize) -> Ordering>(&mut self, tie: &mut F) -> usize {
        let id = self.peek_id();
        let size = self.size;
        self.pos_to_id[1] = self.pos_to_id[size];
        self.pos_to_value[1] = self.pos_to_value[size];
        self.id_to_pos[self.pos_to_id[1] as usize] = 1;
        self.id_to_pos[id] = NOT_PRESENT;
        self.size -= 1;
        self.percolate_down(1, tie);
        id
    }

    pub fn clear(&mut self) {
        for i in 1..=self.size {
            self.id_to_pos[self.pos_to_id[i] as usize] = NOT_PRESENT;
        }
        self.size = 0;
    }

    fn percolate_up<F: FnMut(usize, usize) -> Ordering>(&mut self, mut pos: usize, tie: &mut F) {
        debug_assert!(pos != 0);
        if pos == 1 {
            return;
        }
        let id = self.pos_to_id[pos];
        let val = self.pos_to_value[pos];
        while pos > 1 {
            let parent = parent(pos);
            let parent_value = self.pos_to_value[parent];
            let parent_id = self.pos_to_id[parent];
            if val.cmp(&parent_value).then_with(|| tie(id as usize, parent_id as usize)) != Ordering::Less {
                break;
            }
            self.pos_to_value[pos] = parent_value;
            self.pos_to_id[pos] = parent_id;
            self.id_to_pos[parent_id as usize] = pos as u32;
            pos = parent;
        }
        self.pos_to_id[pos] = id;
        self.pos_to_value[pos] = val;
        self.id_to_pos[id as usize] = pos as u32;
    }

    #[inline]
    fn percolate_down<F: FnMut(usize, usize) -> Ordering>(&mut self, mut pos: usize, tie: &mut F) {
        let size = self.size;
        if size == 0 {
            return;
        }
        debug_assert!(pos > 0 && pos <= size);
        let id = self.pos_to_id[pos];
        let value = self.pos_to_value[pos];
        loop {
            let first = first_child(pos);
            if first > size {
                break;
            }
            // this is a very hot code path for k-way merging: find the minimum of up to 4 children
            let last = (first + 3).min(size);
            let mut min_child = first;
            let mut min_value = self.pos_to_value[first];
            for child in first + 1..=last {
                let child_value = self.pos_to_value[child];
                let cmp = child_value
                    .cmp(&min_value)
                    .then_with(|| tie(self.pos_to_id[child] as usize, self.pos_to_id[min_child] as usize));
                if cmp == Ordering::Less {
                    min_child = child;
                    min_value = child_value;
                }
            }
            if value.cmp(&min_value).then_with(|| tie(id as usize, self.pos_to_id[min_child] as usize))
                != Ordering::Greater
            {
                break;
            }
            let min_id = self.pos_to_id[min_child];
            self.pos_to_value[pos] = min_value;
            self.pos_to_id[pos] = min_id;
            self.id_to_pos[min_id as usize] = pos as u32;
            pos = min_child;
        }
        self.pos_to_id[pos] = id;
        self.pos_to_value[pos] = value;
        self.id_to_pos[id as usize] = pos as u32;
    }
}

/// A 4-ary min-heap with a stored tie-breaker function `tie_breaker(id1, id2)`.
pub struct ArrayLongMinHeap<F: FnMut(usize, usize) -> Ordering> {
    raw: RawLongMinHeap,
    tie_breaker: F,
}

impl<F: FnMut(usize, usize) -> Ordering> ArrayLongMinHeap<F> {
    /// `elements` is the number of elements that can be stored in this heap; `elements - 1` is the maximum id.
    pub fn new(elements: usize, tie_breaker: F, unsigned: bool) -> ArrayLongMinHeap<F> {
        ArrayLongMinHeap { raw: RawLongMinHeap::new(elements, unsigned), tie_breaker }
    }
}

impl<F: FnMut(usize, usize) -> Ordering> LongMinHeap for ArrayLongMinHeap<F> {
    fn size(&self) -> usize {
        self.raw.size()
    }

    fn push(&mut self, id: usize, value: i64) {
        self.raw.push(id, value, &mut self.tie_breaker)
    }

    fn contains(&self, id: usize) -> bool {
        self.raw.contains(id)
    }

    fn update(&mut self, id: usize, value: i64) {
        self.raw.update(id, value, &mut self.tie_breaker)
    }

    fn update_head(&mut self, value: i64) {
        self.raw.update_head(value, &mut self.tie_breaker)
    }

    fn peek_id(&self) -> usize {
        self.raw.peek_id()
    }

    fn peek_value(&self) -> i64 {
        self.raw.peek_value()
    }

    fn poll(&mut self) -> usize {
        self.raw.poll(&mut self.tie_breaker)
    }

    fn clear(&mut self) {
        self.raw.clear()
    }
}

/// Java `MinHeapTest`, shared with the double heap tests.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::collection::long_min_heap::{new_array_heap, new_unsigned_array_heap};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    pub(crate) type Create = fn(usize, fn(usize, usize) -> Ordering) -> Box<dyn LongMinHeap>;

    fn natural(a: usize, b: usize) -> Ordering {
        a.cmp(&b)
    }

    fn panics(f: impl FnOnce()) -> bool {
        catch_unwind(AssertUnwindSafe(f)).is_err()
    }

    /// Simple deterministic PRNG for tests.
    pub(crate) struct Rng(u64);
    impl Rng {
        pub(crate) fn new(seed: u64) -> Rng {
            Rng(seed ^ 0x9E3779B97F4A7C15)
        }
        pub(crate) fn next_u64(&mut self) -> u64 {
            // xorshift64*
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545F4914F6CDD1D)
        }
        pub(crate) fn next_int(&mut self, bound: usize) -> usize {
            (self.next_u64() % bound as u64) as usize
        }
        pub(crate) fn next_bool(&mut self) -> bool {
            self.next_u64() & 1 == 1
        }
    }

    fn polled(heap: &mut dyn LongMinHeap) -> Vec<usize> {
        let mut result = vec![];
        while !heap.is_empty() {
            result.push(heap.poll());
        }
        result
    }

    /// Tests shared by all heap implementations. `signed` disables tests that rely on signed ordering.
    pub(crate) fn run_common_tests(create: Create, signed: bool) {
        // outOfRange
        let mut heap = create(4, natural);
        assert!(panics(|| heap.push(4, 12)));
        // tooManyElements
        let mut heap = create(3, natural);
        heap.push(1, 1);
        heap.push(2, 1);
        heap.push(0, 1);
        assert!(panics(|| heap.push(1, 1)));
        // duplicateElements
        let mut heap = create(5, natural);
        heap.push(1, 2);
        heap.push(0, 4);
        heap.push(2, 1);
        assert_eq!(2, heap.poll());
        heap.push(2, 6);
        assert!(panics(|| heap.push(2, 4)));
        // testContains
        let mut heap = create(4, natural);
        heap.push(1, 1);
        heap.push(2, 7);
        heap.push(0, 5);
        assert!(!heap.contains(3));
        assert!(heap.contains(1));
        assert_eq!(1, heap.poll());
        assert!(!heap.contains(1));
        // containsAfterClear
        let mut heap = create(4, natural);
        heap.push(1, 1);
        heap.push(2, 1);
        assert_eq!(2, heap.size());
        heap.clear();
        assert!(!heap.contains(0) && !heap.contains(1) && !heap.contains(2));
        // testSize
        let mut heap = create(10, natural);
        assert_eq!(0, heap.size());
        assert!(heap.is_empty());
        heap.push(9, 36);
        heap.push(5, 23);
        heap.push(3, 23);
        assert_eq!(3, heap.size());
        assert!(!heap.is_empty());
        // testClear
        let mut heap = create(5, natural);
        heap.push(3, 12);
        heap.push(4, 3);
        assert_eq!(2, heap.size());
        heap.clear();
        assert!(heap.is_empty());
        heap.push(4, 63);
        heap.push(1, 21);
        assert_eq!(2, heap.size());
        assert_eq!(1, heap.peek_id());
        assert_eq!(21, heap.peek_value());
        assert_eq!(1, heap.poll());
        assert_eq!(4, heap.poll());
        assert!(heap.is_empty());
        // pushAndPoll
        let mut heap = create(10, natural);
        heap.push(9, 36);
        heap.push(5, 23);
        heap.push(3, 23);
        assert_eq!(3, heap.size());
        heap.poll();
        assert_eq!(2, heap.size());
        heap.poll();
        heap.poll();
        assert!(heap.is_empty());
        // pollSorted
        let mut heap = create(10, natural);
        for (id, v) in [(9, 36), (5, 21), (3, 23), (8, 57), (7, 22)] {
            heap.push(id, v);
        }
        assert_eq!(vec![5, 7, 3, 9, 8], polled(&mut *heap));
        // poll
        let mut heap = create(10, natural);
        for (i, (id, v)) in [(9, 36), (5, 21), (3, 23), (8, 57)].into_iter().enumerate() {
            heap.push(id, v);
            assert_eq!(i + 1, heap.size());
        }
        for (i, id) in [5, 3, 9, 8].into_iter().enumerate() {
            assert_eq!(id, heap.poll());
            assert_eq!(3 - i, heap.size());
        }
        assert!(heap.is_empty());
        // poll100Ascending / Descending
        let mut heap = create(100, natural);
        for i in 1..100 {
            heap.push(i, i as i64);
        }
        for i in 1..100 {
            assert_eq!(i, heap.poll());
        }
        let mut heap = create(100, natural);
        for i in (1..100).rev() {
            heap.push(i, i as i64);
        }
        for i in 1..100 {
            assert_eq!(i, heap.poll());
        }
        // updateHead
        let mut heap = create(10, natural);
        for i in 1..=5 {
            heap.push(i, i as i64);
        }
        heap.update_head(6);
        heap.update_head(7);
        heap.update_head(8);
        assert_eq!(vec![4, 5, 1, 2, 3], polled(&mut *heap));
        // tieBreaker
        for order in [
            [0, 1, 2, 3, 4, 5, 6, 7],
            [7, 6, 5, 4, 3, 2, 1, 0],
            [0, 1, 2, 6, 7, 5, 4, 3],
            [0, 1, 5, 2, 4, 3, 6, 7],
            [0, 5, 6, 7, 1, 2, 4, 3],
            [5, 0, 1, 2, 7, 6, 4, 3],
        ] {
            let mut heap = create(9, |a, b| b.cmp(&a));
            for id in order {
                heap.push(id, 0);
            }
            assert_eq!(vec![7, 6, 5, 4, 3, 2, 1, 0], polled(&mut *heap));
        }
        // randomPushsThenPolls / randomPushsAndPolls against std BinaryHeap
        for mixed in [false, true] {
            let mut rnd = Rng::new(0);
            let size = 1 + rnd.next_int(100);
            let mut pq = std::collections::BinaryHeap::new();
            let mut heap = create(size, natural);
            let mut set = std::collections::HashSet::new();
            let mut push_count = 0;
            let mut iterations = 0;
            loop {
                iterations += 1;
                if mixed && iterations > 1000 || !mixed && pq.len() >= size {
                    break;
                }
                let push = !mixed || pq.is_empty() || rnd.next_bool();
                if push {
                    let id = rnd.next_int(size);
                    if !set.insert(id) {
                        continue;
                    }
                    let val = (rnd.next_u64() >> 11) as i64; // exactly representable as f64
                    pq.push(std::cmp::Reverse((val, id)));
                    heap.push(id, val);
                    push_count += 1;
                } else {
                    let std::cmp::Reverse((val, id)) = pq.pop().unwrap();
                    assert_eq!(val, heap.peek_value());
                    assert_eq!(id, heap.poll());
                    assert_eq!(pq.len(), heap.size());
                    set.remove(&id);
                }
            }
            while let Some(std::cmp::Reverse((val, id))) = pq.pop() {
                assert_eq!(val, heap.peek_value());
                assert_eq!(id, heap.poll());
                assert_eq!(pq.len(), heap.size());
            }
            assert!(push_count > 0);
        }
        if signed {
            // testPeek
            let mut heap = create(5, natural);
            heap.push(4, -16);
            heap.push(2, 13);
            heap.push(1, -51);
            heap.push(3, 4);
            assert_eq!(1, heap.peek_id());
            assert_eq!(-51, heap.peek_value());
            // update
            let mut heap = create(10, natural);
            heap.push(9, 36);
            heap.push(5, 21);
            heap.push(3, 23);
            heap.update(3, 1);
            assert_eq!(3, heap.peek_id());
            heap.update(3, 100);
            assert_eq!(5, heap.peek_id());
            heap.update(9, -13);
            assert_eq!(9, heap.peek_id());
            assert_eq!(-13, heap.peek_value());
            assert_eq!(vec![9, 5, 3], polled(&mut *heap));
        }
    }

    #[test]
    fn long_min_heap() {
        run_common_tests(|n, t| Box::new(new_array_heap(n, t)), true);
    }

    #[test]
    fn unsigned_long_min_heap() {
        run_common_tests(|n, t| Box::new(new_unsigned_array_heap(n, t)), false);
    }

    #[test]
    fn unsigned_peek() {
        let mut heap = new_unsigned_array_heap(5, natural);
        heap.push(4, -16);
        heap.push(2, 13);
        heap.push(1, -51);
        heap.push(3, 4);
        assert_eq!(3, heap.peek_id());
        assert_eq!(4, heap.peek_value());
    }

    #[test]
    fn unsigned_update() {
        let mut heap = new_unsigned_array_heap(10, natural);
        heap.push(9, 36);
        heap.push(5, 21);
        heap.push(3, 23);
        heap.update(3, 1);
        assert_eq!(3, heap.peek_id());
        heap.update(3, 100);
        assert_eq!(5, heap.peek_id());
        heap.update(9, -13);
        assert_eq!(5, heap.peek_id());
        assert_eq!(21, heap.peek_value());
        assert_eq!(vec![5, 3, 9], polled(&mut heap));
    }

    #[test]
    fn unsigned_ordering_treats_negative_longs_as_largest() {
        let mut heap = new_unsigned_array_heap(5, natural);
        heap.push(0, -1);
        heap.push(1, 1);
        heap.push(2, i64::MAX);
        heap.push(3, i64::MIN);
        assert_eq!(1, heap.poll());
        assert_eq!(2, heap.poll());
        assert_eq!(3, heap.poll());
        assert_eq!(0, heap.poll());
    }

    #[test]
    fn unsigned_update_head() {
        let mut heap = new_unsigned_array_heap(4, natural);
        heap.push(0, 1);
        heap.push(1, 2);
        heap.push(2, 3);
        assert_eq!(0, heap.peek_id());
        heap.update_head(i64::MIN);
        assert_eq!(1, heap.peek_id());
        assert_eq!(2, heap.peek_value());
    }

    #[test]
    fn unsigned_poll_sorted() {
        let mut heap = new_unsigned_array_heap(10, natural);
        heap.push(0, -100);
        heap.push(1, 100);
        heap.push(2, -1);
        heap.push(3, 0);
        heap.push(4, i64::MIN);
        assert_eq!(vec![3, 1, 4, 0, 2], polled(&mut heap));
    }
}
