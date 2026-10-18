//! Port of `collection/ArrayDoubleMinHeap.java`: a min-heap stored in an array where each element has 4 children,
//! ordered by `f64` values.
//!
//! Ported from [GraphHopper](https://github.com/graphhopper/graphhopper/blob/master/core/src/main/java/com/graphhopper/coll/MinHeapWithUpdate.java)
//! (Apache 2.0) and modified to use `f64` values and 4 children per element.

use std::cmp::Ordering;

use super::double_min_heap::DoubleMinHeap;

const NOT_PRESENT: u32 = 0;

#[inline(always)]
fn first_child(index: usize) -> usize {
    (index << 2) - 2
}

#[inline(always)]
fn parent(index: usize) -> usize {
    (index + 2) >> 2
}

/// A 4-ary min-heap of ids ordered by `f64` values with tie-breaker `tie_breaker(id1, id2)`.
pub struct ArrayDoubleMinHeap<F: FnMut(usize, usize) -> Ordering> {
    pos_to_id: Vec<u32>,
    id_to_pos: Vec<u32>,
    pos_to_value: Vec<f64>,
    max: usize,
    size: usize,
    tie_breaker: F,
}

impl<F: FnMut(usize, usize) -> Ordering> ArrayDoubleMinHeap<F> {
    /// `elements` is the number of elements that can be stored in this heap; `elements - 1` is the maximum id.
    pub fn new(elements: usize, tie_breaker: F) -> ArrayDoubleMinHeap<F> {
        assert!(elements < u32::MAX as usize, "too many elements: {elements}");
        let mut pos_to_value = vec![0.0; elements + 1];
        pos_to_value[0] = f64::NEG_INFINITY;
        ArrayDoubleMinHeap {
            pos_to_id: vec![0; elements + 1],
            id_to_pos: vec![NOT_PRESENT; elements + 1],
            pos_to_value,
            max: elements,
            size: 0,
            tie_breaker,
        }
    }

    #[inline]
    #[track_caller]
    fn check_id_in_range(&self, id: usize) {
        if id >= self.max {
            panic!("Illegal id: {id}, legal range: [0, {}[", self.max);
        }
    }

    /// Same semantics as Java: less-than wins, equal values (other than -inf) use the tie-breaker, else greater.
    #[inline(always)]
    fn compare(tie: &mut F, val1: f64, val2: f64, id1: u32, id2: u32) -> Ordering {
        if val1 < val2 {
            Ordering::Less
        } else if val1 == val2 && val1 != f64::NEG_INFINITY {
            tie(id1 as usize, id2 as usize)
        } else {
            Ordering::Greater
        }
    }

    fn percolate_up(&mut self, mut pos: usize) {
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
            if Self::compare(&mut self.tie_breaker, val, parent_value, id, parent_id) != Ordering::Less {
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

    fn percolate_down(&mut self, mut pos: usize) {
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
            let last = (first + 3).min(size);
            let mut min_child = first;
            let mut min_value = self.pos_to_value[first];
            for child in first + 1..=last {
                let child_value = self.pos_to_value[child];
                if Self::compare(
                    &mut self.tie_breaker,
                    child_value,
                    min_value,
                    self.pos_to_id[child],
                    self.pos_to_id[min_child],
                ) == Ordering::Less
                {
                    min_child = child;
                    min_value = child_value;
                }
            }
            if Self::compare(&mut self.tie_breaker, value, min_value, id, self.pos_to_id[min_child])
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

impl<F: FnMut(usize, usize) -> Ordering> DoubleMinHeap for ArrayDoubleMinHeap<F> {
    fn size(&self) -> usize {
        self.size
    }

    #[track_caller]
    fn push(&mut self, id: usize, value: f64) {
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
        self.pos_to_value[size] = value;
        self.percolate_up(size);
    }

    #[track_caller]
    fn contains(&self, id: usize) -> bool {
        self.check_id_in_range(id);
        self.id_to_pos[id] != NOT_PRESENT
    }

    #[track_caller]
    fn update(&mut self, id: usize, value: f64) {
        self.check_id_in_range(id);
        let pos = self.id_to_pos[id] as usize;
        if pos == NOT_PRESENT as usize {
            panic!("The heap does not contain: {id}. Use the contains method to check this before calling update");
        }
        let prev = self.pos_to_value[pos];
        self.pos_to_value[pos] = value;
        let pos_id = self.pos_to_id[pos];
        match Self::compare(&mut self.tie_breaker, value, prev, id as u32, pos_id) {
            Ordering::Greater => self.percolate_down(pos),
            Ordering::Less => self.percolate_up(pos),
            Ordering::Equal => {}
        }
    }

    fn update_head(&mut self, value: f64) {
        self.pos_to_value[1] = value;
        self.percolate_down(1);
    }

    fn peek_id(&self) -> usize {
        self.pos_to_id[1] as usize
    }

    fn peek_value(&self) -> f64 {
        self.pos_to_value[1]
    }

    fn poll(&mut self) -> usize {
        let id = self.peek_id();
        let size = self.size;
        self.pos_to_id[1] = self.pos_to_id[size];
        self.pos_to_value[1] = self.pos_to_value[size];
        self.id_to_pos[self.pos_to_id[1] as usize] = 1;
        self.id_to_pos[id] = NOT_PRESENT;
        self.size -= 1;
        self.percolate_down(1);
        id
    }

    fn clear(&mut self) {
        for i in 1..=self.size {
            self.id_to_pos[self.pos_to_id[i] as usize] = NOT_PRESENT;
        }
        self.size = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::array_long_min_heap::tests::run_common_tests;
    use crate::collection::double_min_heap::new_array_heap;
    use crate::collection::long_min_heap::LongMinHeap;

    /// Adapts a double heap to the long heap API (Java `DoubleMinHeapTest.create`).
    struct Adapter<F: FnMut(usize, usize) -> Ordering>(ArrayDoubleMinHeap<F>);

    impl<F: FnMut(usize, usize) -> Ordering> LongMinHeap for Adapter<F> {
        fn size(&self) -> usize {
            self.0.size()
        }
        fn push(&mut self, id: usize, value: i64) {
            self.0.push(id, value as f64)
        }
        fn contains(&self, id: usize) -> bool {
            self.0.contains(id)
        }
        fn update(&mut self, id: usize, value: i64) {
            self.0.update(id, value as f64)
        }
        fn update_head(&mut self, value: i64) {
            self.0.update_head(value as f64)
        }
        fn peek_id(&self) -> usize {
            self.0.peek_id()
        }
        fn peek_value(&self) -> i64 {
            self.0.peek_value() as i64
        }
        fn poll(&mut self) -> usize {
            self.0.poll()
        }
        fn clear(&mut self) {
            self.0.clear()
        }
    }

    #[test]
    fn common() {
        run_common_tests(|n, t| Box::new(Adapter(new_array_heap(n, t))), true);
    }

    #[test]
    fn doubles() {
        let mut heap = new_array_heap(5, |a: usize, b: usize| a.cmp(&b));
        heap.push(4, 1.5);
        heap.push(1, 1.4);
        assert_eq!(2, heap.size());
        assert_eq!(1, heap.peek_id());
        assert_eq!(1.4, heap.peek_value());
        assert_eq!(1, heap.poll());
        assert_eq!(4, heap.poll());
        assert!(heap.is_empty());
    }

    #[test]
    fn doubles_reverse() {
        let mut heap = new_array_heap(5, |a: usize, b: usize| a.cmp(&b));
        heap.push(4, 1.4);
        heap.push(1, 1.5);
        assert_eq!(2, heap.size());
        assert_eq!(4, heap.peek_id());
        assert_eq!(1.4, heap.peek_value());
        assert_eq!(4, heap.poll());
        assert_eq!(1, heap.poll());
        assert!(heap.is_empty());
    }
}
