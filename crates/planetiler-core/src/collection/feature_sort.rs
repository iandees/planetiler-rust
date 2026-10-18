//! Port of `collection/FeatureSort.java`: accepts [`SortableFeature`]s in any order and lets you iterate through them
//! ordered by key (unsigned) then value bytes.
//!
//! Writes go through per-thread [`FeatureSortWriter`]s (which may be used from different threads concurrently), then
//! [`FeatureSort::sort`] is called once with exclusive access, then features can be read back in order, optionally
//! from several shards in parallel.

use std::path::Path;
use std::sync::Arc;

use crossbeam_channel::{Receiver, bounded};
use parking_lot::Mutex;

use super::long_merger;
use super::sortable_feature::SortableFeature;
use crate::stats::stats::StatsRef;
use crate::worker::worker::Worker;

/// A boxed iterator over sorted features.
///
/// Iterators returned by disk-backed sorts panic if reading a temporary file fails (Java throws
/// `UncheckedIOException`).
pub type FeatureIter = Box<dyn Iterator<Item = SortableFeature> + Send>;

/// A handle a single thread can use to write features, independent of writers used from other threads.
pub trait FeatureSortWriter: Send {
    /// Adds a feature to be sorted.
    fn accept(&mut self, feature: SortableFeature) -> anyhow::Result<()> {
        self.accept_parts(feature.key, &feature.value)
    }

    /// Adds a feature from its parts, avoiding an allocation for disk-backed sorts.
    fn accept_parts(&mut self, key: i64, value: &[u8]) -> anyhow::Result<()>;

    /// Flushes and closes this writer. Called automatically on drop (where errors are logged).
    fn close(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Sorts features by key.
pub trait FeatureSort: Send + Sync {
    /// Sorts all features written so far. All writers must have been dropped.
    fn sort(&mut self) -> anyhow::Result<()>;

    fn num_features_written(&self) -> u64;

    /// Returns a new writer that can be used to write features from a single thread.
    fn writer_for_thread(&self) -> anyhow::Result<Box<dyn FeatureSortWriter + '_>>;

    /// Returns an iterator over a subset of the features: shard `shard` of `shards`. Combining the results of all
    /// shards (for example with [`long_merger::merge_iterators`]) gives the full sorted set.
    fn iterator(&self, shard: usize, shards: usize) -> anyhow::Result<FeatureIter>;

    /// Returns an iterator over all features in sorted order.
    fn iter(&self) -> FeatureIter {
        self.iterator(0, 1).expect("valid shard")
    }

    /// Returns all elements in a list. WARNING: this materializes all elements in memory.
    fn to_list(&self) -> Vec<SortableFeature> {
        self.iter().collect()
    }

    /// Number of chunks that readers will need to read.
    fn chunks_to_read(&self) -> usize;

    fn disk_usage_bytes(&self) -> u64 {
        0
    }

    fn estimate_memory_usage_bytes(&self) -> u64 {
        0
    }

    /// Saves the list of sorted chunks to `path` so they can be restored on a later run (only supported by
    /// [`ExternalMergeSort`](super::external_merge_sort::ExternalMergeSort)).
    fn save_manifest(&self, _path: &Path) -> anyhow::Result<()> {
        anyhow::bail!("Can't save chunk manifest: sorter is not ExternalMergeSort")
    }

    /// Restores sorted chunks from a manifest written by [`FeatureSort::save_manifest`].
    fn init_from_manifest(&mut self, _path: &Path) -> anyhow::Result<()> {
        anyhow::bail!("Can't initialize chunk manifest: sorter is not ExternalMergeSort")
    }

    /// Reads features using `threads` parallel threads (each reading one shard, in a [`Worker`] named `read`) and
    /// merges them into a sorted iterator.
    fn parallel_iterator(&self, stats: StatsRef, threads: usize) -> anyhow::Result<ParallelIterator> {
        let threads = threads.max(1);
        let mut shards = Vec::with_capacity(threads);
        let mut receivers = Vec::with_capacity(threads);
        for shard in 0..threads {
            let iter = self.iterator(shard, threads)?;
            let (tx, rx) = bounded::<Batch>(16);
            receivers.push(rx);
            shards.push(Mutex::new(Some((iter, tx))));
        }
        let reader = Worker::new("read", stats, threads, move |shard| {
            let Some((iter, tx)) = shards[shard].lock().take() else { return Ok(()) };
            let mut batch = Vec::with_capacity(BATCH_SIZE);
            for item in iter {
                batch.push(item);
                if batch.len() >= BATCH_SIZE {
                    let full = std::mem::replace(&mut batch, Vec::with_capacity(BATCH_SIZE));
                    if tx.send(Batch::Items(full)).is_err() {
                        return Ok(()); // consumer went away
                    }
                }
            }
            if !batch.is_empty() {
                let _ = tx.send(Batch::Items(batch));
            }
            let _ = tx.send(Batch::Done);
            Ok(())
        });
        let iters: Vec<ChannelIter> = receivers.into_iter().map(ChannelIter::new).collect();
        let merged = long_merger::merge_iterators(iters, SortableFeature::compare_bytes);
        Ok(ParallelIterator { reader, iterator: Box::new(merged) })
    }
}

const BATCH_SIZE: usize = 512;

enum Batch {
    Items(Vec<SortableFeature>),
    Done,
}

/// Iterates over batches received from a reader thread, panicking if the reader thread failed.
struct ChannelIter {
    rx: Receiver<Batch>,
    current: std::vec::IntoIter<SortableFeature>,
    done: bool,
}

impl ChannelIter {
    fn new(rx: Receiver<Batch>) -> ChannelIter {
        ChannelIter { rx, current: Vec::new().into_iter(), done: false }
    }
}

impl Iterator for ChannelIter {
    type Item = SortableFeature;

    #[inline]
    fn next(&mut self) -> Option<SortableFeature> {
        loop {
            if let Some(item) = self.current.next() {
                return Some(item);
            }
            if self.done {
                return None;
            }
            match self.rx.recv() {
                Ok(Batch::Items(items)) => self.current = items.into_iter(),
                Ok(Batch::Done) => self.done = true,
                Err(_) => panic!("feature reader thread failed before finishing"),
            }
        }
    }
}

/// The reader threads spawned by [`FeatureSort::parallel_iterator`] (log them with
/// [`ProgressLoggers::add_thread_pool_stats_for_worker`](crate::stats::progress_loggers::ProgressLoggers::add_thread_pool_stats_for_worker))
/// plus the merged iterator.
pub struct ParallelIterator {
    pub reader: Worker,
    pub iterator: FeatureIter,
}

impl ParallelIterator {
    /// Stops iterating and waits for all reader threads to finish, returning an error if any failed.
    pub fn join(self) -> anyhow::Result<()> {
        drop(self.iterator);
        self.reader.await_done()
    }
}

impl Iterator for ParallelIterator {
    type Item = SortableFeature;

    #[inline]
    fn next(&mut self) -> Option<SortableFeature> {
        self.iterator.next()
    }
}

/// Returns a feature sorter that sorts all features in memory. Suitable for toy examples (unit tests).
pub fn new_in_memory() -> InMemoryFeatureSort {
    InMemoryFeatureSort::default()
}

/// A [`FeatureSort`] that keeps all features in memory.
#[derive(Default)]
pub struct InMemoryFeatureSort {
    list: Mutex<Vec<SortableFeature>>,
    sorted: Option<Arc<Vec<SortableFeature>>>,
}

impl InMemoryFeatureSort {
    fn snapshot(&self) -> Arc<Vec<SortableFeature>> {
        match &self.sorted {
            Some(s) => s.clone(),
            None => Arc::new(self.list.lock().clone()),
        }
    }
}

struct InMemoryWriter<'a> {
    list: &'a Mutex<Vec<SortableFeature>>,
}

impl FeatureSortWriter for InMemoryWriter<'_> {
    fn accept(&mut self, feature: SortableFeature) -> anyhow::Result<()> {
        self.list.lock().push(feature);
        Ok(())
    }

    fn accept_parts(&mut self, key: i64, value: &[u8]) -> anyhow::Result<()> {
        self.accept(SortableFeature::new(key, value.to_vec()))
    }
}

impl FeatureSort for InMemoryFeatureSort {
    fn sort(&mut self) -> anyhow::Result<()> {
        let mut list = std::mem::take(self.list.get_mut());
        if let Some(prev) = self.sorted.take() {
            list.extend(Arc::unwrap_or_clone(prev));
        }
        list.sort();
        self.sorted = Some(Arc::new(list));
        Ok(())
    }

    fn num_features_written(&self) -> u64 {
        (self.list.lock().len() + self.sorted.as_ref().map_or(0, |s| s.len())) as u64
    }

    fn writer_for_thread(&self) -> anyhow::Result<Box<dyn FeatureSortWriter + '_>> {
        Ok(Box::new(InMemoryWriter { list: &self.list }))
    }

    fn iterator(&self, shard: usize, shards: usize) -> anyhow::Result<FeatureIter> {
        if shard >= shards {
            anyhow::bail!("Bad shard params: shard={shard} shards={shards}");
        }
        let list = self.snapshot();
        let len = list.len();
        Ok(Box::new((shard..len).step_by(shards).map(move |i| list[i].clone())))
    }

    fn chunks_to_read(&self) -> usize {
        self.num_features_written() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(i: i64) -> SortableFeature {
        SortableFeature::new(i64::MIN + i, vec![i as u8, (1 + i) as u8])
    }

    #[test]
    fn in_memory() {
        let mut sorter = new_in_memory();
        {
            let mut w1 = sorter.writer_for_thread().unwrap();
            let mut w2 = sorter.writer_for_thread().unwrap();
            w1.accept(entry(3)).unwrap();
            w2.accept(entry(1)).unwrap();
            w1.accept(entry(2)).unwrap();
        }
        sorter.sort().unwrap();
        assert_eq!(vec![entry(1), entry(2), entry(3)], sorter.to_list());
        assert_eq!(3, sorter.num_features_written());
        assert_eq!(vec![entry(1), entry(3)], sorter.iterator(0, 2).unwrap().collect::<Vec<_>>());
        assert!(sorter.iterator(2, 2).is_err());
        let par: Vec<_> = sorter.parallel_iterator(crate::stats::stats::InMemoryStats::arc(), 2).unwrap().collect();
        assert_eq!(vec![entry(1), entry(2), entry(3)], par);
    }
}
