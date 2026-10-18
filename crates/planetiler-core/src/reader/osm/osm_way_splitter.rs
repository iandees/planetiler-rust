//! Port of `reader/osm/OsmWaySplitter.java`: finds nodes where ways intersect.
//!
//! We process the list of node IDs in each way once. If a node ID appears more than once (even in the same way) then
//! it is an intersection point.

use std::collections::HashMap;

use parking_lot::{Mutex, RwLock};
use roaring::RoaringBitmap;

use crate::util::memory_estimator::{self, HasEstimate};

/// Utility that finds nodes where ways intersect.
pub trait OsmWaySplitter: HasEstimate + Send + Sync {
    /// Returns a new thread-local writer that should be used to add ways to this splitter. Pending writes are flushed
    /// when the writer is dropped (Java's `close()`).
    fn writer_for_thread(&self) -> Box<dyn PerThreadWriter + '_>;

    /// Returns `true` if this node is used more than once in a way.
    fn is_shared(&self, node_id: i64) -> bool;

    /// Returns indices of shared nodes where this way should get split, excluding the first and last node.
    fn get_split_indices(&self, node_ids: &[i64]) -> Vec<usize> {
        let len = node_ids.len();
        node_ids
            .iter()
            .enumerate()
            .filter(|&(i, &node)| i > 0 && i + 1 < len && self.is_shared(node))
            .map(|(i, _)| i)
            .collect()
    }

    /// Call this after adding all ways to clean up unused memory.
    fn finish(&self) {}
}

/// A thread-local writer that adds ways to an [`OsmWaySplitter`].
pub trait PerThreadWriter {
    fn add_way(&mut self, node_ids: &[i64]);
}

/// Returns a naive splitter backed by a synchronized in-memory [`HashMap`].
pub fn map_splitter() -> MapSplitter {
    MapSplitter::default()
}

/// Returns a new optimized splitter backed by [`RoaringBitmap`]s where each thread updates local bitmaps and
/// periodically merges them into shared bitmaps to avoid contention.
///
/// With 1.1B ways and 10B nodes, this uses at most 3GB (1.5GB after [`OsmWaySplitter::finish`] is called).
pub fn roaring_bitmap_splitter() -> RoaringBitmapSplitter {
    RoaringBitmapSplitter::new(100_000_000_000, 23, 10_000)
}

/// Naive splitter backed by a synchronized [`HashMap`] of node ID to count.
#[derive(Default)]
pub struct MapSplitter {
    node_counts: Mutex<HashMap<i64, u32>>,
}

impl HasEstimate for MapSplitter {
    fn estimate_memory_usage_bytes(&self) -> u64 {
        0
    }
}

impl OsmWaySplitter for MapSplitter {
    fn writer_for_thread(&self) -> Box<dyn PerThreadWriter + '_> {
        struct Writer<'a>(&'a MapSplitter);
        impl PerThreadWriter for Writer<'_> {
            fn add_way(&mut self, node_ids: &[i64]) {
                let mut counts = self.0.node_counts.lock();
                for &node in node_ids {
                    *counts.entry(node).or_default() += 1;
                }
            }
        }
        Box::new(Writer(self))
    }

    fn is_shared(&self, node_id: i64) -> bool {
        self.node_counts.lock().get(&node_id).is_some_and(|&c| c > 1)
    }
}

#[derive(Default)]
struct Partition {
    visited: Option<RoaringBitmap>,
    shared: Option<RoaringBitmap>,
}

/// Splitter backed by partitioned [`RoaringBitmap`]s.
///
/// The basic algorithm works like:
/// ```text
/// if visited.contains(nodeId): shared.add(node)
/// else visited.add(nodeId)
/// ```
/// Each thread does that with local bitmaps, then we periodically do a batch operation to merge those bitmaps into
/// global ones:
/// ```text
/// crossPartitionCollisions = allVisited & threadVisited
/// allShared |= crossPartitionCollisions | shared
/// allVisited |= threadVisited
/// ```
pub struct RoaringBitmapSplitter {
    partitions: Vec<RwLock<Partition>>,
    lower_bits: u32,
    lower_bit_mask: i64,
    flush_limit: usize,
}

impl RoaringBitmapSplitter {
    /// - `max_id`: maximum node ID this supports
    /// - `lower_bits`: number of bits to store in individual roaring bitmaps
    /// - `flush_limit`: number of writes to a partition before we flush it to shared bitmaps
    pub fn new(max_id: i64, lower_bits: u32, flush_limit: usize) -> RoaringBitmapSplitter {
        assert!(lower_bits <= 30, "Must be <=30 lower bits");
        let num_per_bitmap = 1i64 << lower_bits;
        let num_bitmaps = usize::try_from(max_id / num_per_bitmap + 1).expect("too many bitmaps");
        RoaringBitmapSplitter {
            partitions: (0..num_bitmaps).map(|_| RwLock::new(Partition::default())).collect(),
            lower_bits,
            lower_bit_mask: (1i64 << lower_bits) - 1,
            flush_limit,
        }
    }

    fn offset(&self, node_id: i64) -> u32 {
        (node_id & self.lower_bit_mask) as u32
    }

    fn index(&self, node_id: i64) -> usize {
        (node_id >> self.lower_bits) as usize
    }
}

struct RoaringWriter<'a> {
    splitter: &'a RoaringBitmapSplitter,
    visited: Vec<Option<RoaringBitmap>>,
    shared: Vec<Option<RoaringBitmap>>,
    counts: Vec<usize>,
}

impl RoaringWriter<'_> {
    fn flush(&mut self, index: usize) {
        if self.counts[index] > 0 {
            let thread_visited = self.visited[index].take().unwrap_or_default();
            let thread_shared = self.shared[index].take().unwrap_or_default();
            let mut guard = self.splitter.partitions[index].write();
            let partition = &mut *guard;
            match (&mut partition.visited, &mut partition.shared) {
                (Some(all_visited), Some(all_shared)) => {
                    // Merge intermediate bitmaps into result
                    *all_shared |= &*all_visited & &thread_visited;
                    *all_visited |= thread_visited;
                    *all_shared |= thread_shared;
                }
                _ => {
                    partition.visited = Some(thread_visited);
                    partition.shared = Some(thread_shared);
                }
            }
            self.counts[index] = 0;
        }
    }
}

impl PerThreadWriter for RoaringWriter<'_> {
    fn add_way(&mut self, node_ids: &[i64]) {
        for &node in node_ids {
            let index = self.splitter.index(node);
            let offset = self.splitter.offset(node);
            let visited = self.visited[index].get_or_insert_with(RoaringBitmap::new);
            if !visited.insert(offset) {
                self.shared[index].get_or_insert_with(RoaringBitmap::new).insert(offset);
            }
            let count = self.counts[index];
            self.counts[index] += 1;
            if count > self.splitter.flush_limit {
                self.flush(index);
            }
        }
    }
}

impl Drop for RoaringWriter<'_> {
    fn drop(&mut self) {
        for i in 0..self.counts.len() {
            self.flush(i);
        }
    }
}

impl HasEstimate for RoaringBitmapSplitter {
    fn estimate_memory_usage_bytes(&self) -> u64 {
        let mut total =
            3 * memory_estimator::estimate_array_size(self.partitions.len(), memory_estimator::POINTER_BYTES);
        for partition in &self.partitions {
            let p = partition.read();
            total += p.visited.as_ref().map_or(0, |b| b.serialized_size() as u64);
            total += p.shared.as_ref().map_or(0, |b| b.serialized_size() as u64);
        }
        total
    }
}

impl OsmWaySplitter for RoaringBitmapSplitter {
    fn writer_for_thread(&self) -> Box<dyn PerThreadWriter + '_> {
        let n = self.partitions.len();
        Box::new(RoaringWriter {
            splitter: self,
            visited: (0..n).map(|_| None).collect(),
            shared: (0..n).map(|_| None).collect(),
            counts: vec![0; n],
        })
    }

    fn is_shared(&self, node_id: i64) -> bool {
        self.partitions
            .get(self.index(node_id))
            .is_some_and(|p| p.read().shared.as_ref().is_some_and(|s| s.contains(self.offset(node_id))))
    }

    fn finish(&self) {
        for partition in &self.partitions {
            partition.write().visited = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_way(splitter: &dyn OsmWaySplitter, node_ids: &[i64]) {
        splitter.writer_for_thread().add_way(node_ids);
    }

    fn run_all(get: fn() -> Box<dyn OsmWaySplitter>) {
        // testSplitWhenEmpty
        let splitter = get();
        let empty: Vec<usize> = vec![];
        assert_eq!(empty, splitter.get_split_indices(&[]));
        assert_eq!(empty, splitter.get_split_indices(&[1]));
        assert_eq!(empty, splitter.get_split_indices(&[1, 2]));

        // testSplitNoIntersection
        let splitter = get();
        add_way(&*splitter, &[1, 2]);
        assert_eq!(empty, splitter.get_split_indices(&[]));
        assert_eq!(empty, splitter.get_split_indices(&[1]));
        assert_eq!(empty, splitter.get_split_indices(&[1, 2]));

        // testSplitWithIntersection
        let splitter = get();
        add_way(&*splitter, &[1, 2, 3]);
        add_way(&*splitter, &[2, 4]);
        assert_eq!(vec![1], splitter.get_split_indices(&[1, 2, 3]));
        assert_eq!(empty, splitter.get_split_indices(&[2, 4]));

        // testSplitIgnoresEndpoints
        let splitter = get();
        add_way(&*splitter, &[1, 2, 3]);
        add_way(&*splitter, &[1, 4]);
        add_way(&*splitter, &[3, 4]);
        assert_eq!(empty, splitter.get_split_indices(&[1, 2, 3]));
        assert_eq!(empty, splitter.get_split_indices(&[1, 4]));
        assert_eq!(empty, splitter.get_split_indices(&[3, 4]));

        // testSplitWithMultipleIntersections
        let splitter = get();
        add_way(&*splitter, &[1, 2, 3, 4, 5, 3]);
        add_way(&*splitter, &[1, 11]);
        add_way(&*splitter, &[2, 12]);
        add_way(&*splitter, &[4, 14]);
        add_way(&*splitter, &[5, 15]);
        assert_eq!(vec![1, 2, 3, 4], splitter.get_split_indices(&[1, 2, 3, 4, 5, 3]));
        assert_eq!(empty, splitter.get_split_indices(&[1, 11]));
        assert_eq!(empty, splitter.get_split_indices(&[2, 12]));
        assert_eq!(empty, splitter.get_split_indices(&[4, 14]));
        assert_eq!(empty, splitter.get_split_indices(&[5, 15]));

        // testSplitWithinPartition
        let splitter = get();
        {
            let mut writer = splitter.writer_for_thread();
            writer.add_way(&[1, 2, 3, 4]);
            writer.add_way(&[2, 5]);
        }
        assert_eq!(vec![1], splitter.get_split_indices(&[1, 2, 3, 4]));

        // testBigNumbers
        let splitter = get();
        let base = 5_000_000_000i64;
        {
            let mut writer = splitter.writer_for_thread();
            writer.add_way(&[base + 1, base + 2, base + 3]);
            writer.add_way(&[base + 2, base + 4, base + 5]);
        }
        add_way(&*splitter, &[base + 4, base + 6]);
        assert_eq!(vec![1], splitter.get_split_indices(&[base + 1, base + 2, base + 3]));
        assert_eq!(vec![1], splitter.get_split_indices(&[base + 2, base + 4, base + 5]));

        // testSelfLoop
        let splitter = get();
        add_way(&*splitter, &[1, 2, 3, 4, 2, 5]);
        assert_eq!(vec![1, 4], splitter.get_split_indices(&[1, 2, 3, 4, 2, 5]));
    }

    #[test]
    fn test_map_splitter() {
        run_all(|| Box::new(map_splitter()));
    }

    #[test]
    fn test_roaring_bitmap_splitter() {
        run_all(|| Box::new(roaring_bitmap_splitter()));
    }

    #[test]
    fn test_roaring_bitmap_splitter_flushes_and_finish() {
        let splitter = RoaringBitmapSplitter::new(1_000, 4, 2);
        std::thread::scope(|s| {
            s.spawn(|| splitter.writer_for_thread().add_way(&[1, 2, 3, 4, 5, 6, 7, 8]));
            s.spawn(|| splitter.writer_for_thread().add_way(&[100, 5, 200]));
        });
        splitter.finish();
        assert!(splitter.is_shared(5));
        assert!(!splitter.is_shared(4));
        assert!(!splitter.is_shared(999_999));
        assert!(splitter.estimate_memory_usage_bytes() > 0);
    }
}
