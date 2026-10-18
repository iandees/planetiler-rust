//! Port of `collection/LongLongMultimap.java`: a map that stores multiple `i64` values for each `i64` key.
//!
//! Implementations implement [`Replaceable`] if they support replacing the previous set of values for a key and/or
//! [`Appendable`] if they support adding new values for a key.
//!
//! Writes take `&self` and are internally synchronized (Java callers wrap writes in `synchronized`). The first
//! [`LongLongMultimap::get`] freezes the map so that subsequent reads from many threads are lock-free; writes after
//! that fail.

use std::path::Path;

use hashbrown::HashMap;
use rayon::slice::ParallelSliceMut;

use super::append_store::{Longs, create_longs};
use super::long_long_map::SealOnRead;
use super::storage::{Storage, StorageParams};
use crate::stats::timer::Timer;

/// A map from `i64` to a list of `i64` values.
pub trait LongLongMultimap: Send + Sync {
    /// Returns the values for a key, or an empty list. Safe to call from multiple threads after all values have been
    /// written. After the first read, all writes fail.
    fn get(&self, key: i64) -> Vec<i64>;

    fn estimate_memory_usage_bytes(&self) -> u64 {
        0
    }

    fn disk_usage_bytes(&self) -> u64 {
        0
    }
}

/// A multimap where [`Replaceable::replace_values`] replaces the previous list of values for a key.
pub trait Replaceable: LongLongMultimap {
    /// Replaces the previous list of values for `key` with `values`.
    fn replace_values(&self, key: i64, values: &[i64]) -> anyhow::Result<()>;
}

/// A multimap where [`Appendable::put`] adds a value for a key.
pub trait Appendable: LongLongMultimap {
    /// Adds a value for a key.
    fn put(&self, key: i64, value: i64) -> anyhow::Result<()>;

    /// Adds all `values` for `key`.
    fn put_all(&self, key: i64, values: &[i64]) -> anyhow::Result<()> {
        for &v in values {
            self.put(key, v)?;
        }
        Ok(())
    }
}

/// Returns a [`Noop`] implementation that does nothing on writes and panics on [`LongLongMultimap::get`].
pub fn noop() -> Noop {
    Noop
}

/// Returns a new multimap where each write sets the list of values for a key, and that order is preserved on read.
pub fn new_replaceable_multimap(storage: Storage, params: &StorageParams) -> anyhow::Result<DenseOrderedMultimap> {
    DenseOrderedMultimap::new(storage, params)
}

/// Returns a new multimap from config strings (Java `newReplaceableMultimap(String, Path, boolean)`).
pub fn new_replaceable_multimap_from_config(
    storage: &str,
    path: &Path,
    madvise: bool,
) -> anyhow::Result<DenseOrderedMultimap> {
    DenseOrderedMultimap::new(Storage::from_id(storage)?, &StorageParams::new(path, madvise))
}

/// Returns a new replaceable multimap held in memory.
pub fn new_in_memory_replaceable_multimap() -> DenseOrderedMultimap {
    DenseOrderedMultimap::new(Storage::Ram, &StorageParams::default()).expect("in-memory multimap")
}

/// Returns a new multimap where each write adds a value for the given key.
pub fn new_appendable_multimap() -> SparseUnorderedBinarySearchMultimap {
    SparseUnorderedBinarySearchMultimap::new()
}

/// Dummy implementation that ignores writes and panics on [`LongLongMultimap::get`].
#[derive(Default, Clone, Copy, Debug)]
pub struct Noop;

impl LongLongMultimap for Noop {
    fn get(&self, _key: i64) -> Vec<i64> {
        panic!("get(key) not implemented");
    }
}

impl Replaceable for Noop {
    fn replace_values(&self, _key: i64, _values: &[i64]) -> anyhow::Result<()> {
        Ok(())
    }
}

impl Appendable for Noop {
    fn put(&self, _key: i64, _value: i64) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A map from `i64` to `i64` stored as a list of key/value pairs that uses binary search to find the values for a
/// key. Inserts do not need to be ordered: the first read sorts the list (stably, so values for a key are returned in
/// insertion order).
pub struct SparseUnorderedBinarySearchMultimap {
    state: SealOnRead<Vec<(i64, i64)>>,
}

impl SparseUnorderedBinarySearchMultimap {
    pub fn new() -> SparseUnorderedBinarySearchMultimap {
        SparseUnorderedBinarySearchMultimap { state: SealOnRead::new(Vec::new()) }
    }

    fn sorted(&self) -> &[(i64, i64)] {
        self.state.read_with(|pairs| {
            let timer = Timer::start();
            tracing::debug!("Sorting long long multimap...");
            // stable parallel sort keeps values for each key in insertion order
            pairs.par_sort_by_key(|p| p.0);
            pairs.shrink_to_fit();
            tracing::debug!("Sorted long long multimap {}", timer.stop());
        })
    }
}

impl Default for SparseUnorderedBinarySearchMultimap {
    fn default() -> Self {
        Self::new()
    }
}

impl LongLongMultimap for SparseUnorderedBinarySearchMultimap {
    fn get(&self, key: i64) -> Vec<i64> {
        let pairs = self.sorted();
        let start = pairs.partition_point(|p| p.0 < key);
        pairs[start..].iter().take_while(|p| p.0 == key).map(|p| p.1).collect()
    }

    fn estimate_memory_usage_bytes(&self) -> u64 {
        self.state.peek(|p| (p.capacity() * 16) as u64).unwrap_or(0)
    }
}

impl Appendable for SparseUnorderedBinarySearchMultimap {
    fn put(&self, key: i64, value: i64) -> anyhow::Result<()> {
        if value <= 0 {
            anyhow::bail!("Invalid value: {value} must be >0");
        }
        match self.state.lock_blocking().as_mut() {
            Some(pairs) => {
                pairs.push((key, value));
                Ok(())
            }
            None => anyhow::bail!("Cannot insert after preparing"),
        }
    }

    fn put_all(&self, key: i64, values: &[i64]) -> anyhow::Result<()> {
        if let Some(v) = values.iter().find(|&&v| v <= 0) {
            anyhow::bail!("Invalid value: {v} must be >0");
        }
        match self.state.lock_blocking().as_mut() {
            Some(pairs) => {
                pairs.extend(values.iter().map(|&v| (key, v)));
                Ok(())
            }
            None => anyhow::bail!("Cannot insert after preparing"),
        }
    }
}

struct DenseState {
    key_to_values_index: HashMap<i64, u64>,
    // each block starts with a "length" header then contains that number of entries
    values: Box<dyn Longs>,
}

/// A map from `i64` to `i64` where each [`Replaceable::replace_values`] replaces previous values and results are
/// returned in the same order they were inserted. Values are stored in an append store so they can live in a
/// memory-mapped file.
pub struct DenseOrderedMultimap {
    state: SealOnRead<DenseState>,
}

impl DenseOrderedMultimap {
    pub fn new(storage: Storage, params: &StorageParams) -> anyhow::Result<DenseOrderedMultimap> {
        Ok(DenseOrderedMultimap {
            state: SealOnRead::new(DenseState {
                key_to_values_index: HashMap::new(),
                values: create_longs(storage, params)?,
            }),
        })
    }
}

impl LongLongMultimap for DenseOrderedMultimap {
    fn get(&self, key: i64) -> Vec<i64> {
        let state = self.state.read();
        match state.key_to_values_index.get(&key) {
            Some(&index) => {
                let num = state.values.get_long(index) as u64;
                (0..num).map(|i| state.values.get_long(index + 1 + i)).collect()
            }
            None => Vec::new(),
        }
    }

    fn estimate_memory_usage_bytes(&self) -> u64 {
        self.state
            .peek(|s| (s.key_to_values_index.capacity() * 17) as u64 + s.values.estimate_memory_usage_bytes())
            .unwrap_or(0)
    }

    fn disk_usage_bytes(&self) -> u64 {
        self.state.peek(|s| s.values.disk_usage_bytes()).unwrap_or(0)
    }
}

impl Replaceable for DenseOrderedMultimap {
    fn replace_values(&self, key: i64, values: &[i64]) -> anyhow::Result<()> {
        if values.is_empty() {
            return Ok(());
        }
        let mut guard = self.state.lock_blocking();
        let Some(state) = guard.as_mut() else {
            anyhow::bail!("Cannot write to multimap after it has been read");
        };
        let index = state.values.size();
        state.values.append_long(values.len() as i64)?;
        for &v in values {
            state.values.append_long(v)?;
        }
        state.key_to_values_index.insert(key, index);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    enum Map {
        Appendable(Box<dyn Appendable>),
        Replaceable(Box<dyn Replaceable>),
    }

    struct Case {
        map: Map,
        retain_input_order: bool,
        _dir: Option<tempfile::TempDir>,
    }

    impl Case {
        fn put(&self, k: i64, v: i64) {
            self.put_all(k, &[v]);
        }

        fn put_all(&self, k: i64, vs: &[i64]) {
            match &self.map {
                Map::Appendable(a) => a.put_all(k, vs).unwrap(),
                Map::Replaceable(r) => r.replace_values(k, vs).unwrap(),
            }
        }

        fn get(&self, k: i64) -> Vec<i64> {
            match &self.map {
                Map::Appendable(a) => a.get(k),
                Map::Replaceable(r) => r.get(k),
            }
        }

        fn assert_result(&self, mut expected: Vec<i64>, k: i64) {
            let mut actual = self.get(k);
            if !self.retain_input_order {
                expected.sort();
                actual.sort();
            }
            assert_eq!(expected, actual, "key {k}");
        }
    }

    fn cases() -> Vec<Case> {
        let dir = tempfile::tempdir().unwrap();
        let mmap =
            new_replaceable_multimap(Storage::Mmap, &StorageParams::new(dir.path().join("multimap"), true)).unwrap();
        vec![
            Case { map: Map::Appendable(Box::new(new_appendable_multimap())), retain_input_order: false, _dir: None },
            Case {
                map: Map::Replaceable(Box::new(new_in_memory_replaceable_multimap())),
                retain_input_order: true,
                _dir: None,
            },
            Case { map: Map::Replaceable(Box::new(mmap)), retain_input_order: true, _dir: Some(dir) },
        ]
    }

    #[test]
    fn missing_value() {
        for c in cases() {
            assert!(c.get(0).is_empty());
        }
    }

    #[test]
    fn one_value() {
        for c in cases() {
            c.put(1, 1);
            c.assert_result(vec![], 0);
            c.assert_result(vec![1], 1);
            c.assert_result(vec![], 2);
        }
    }

    #[test]
    fn two_consecutive_values() {
        for c in cases() {
            c.put(1, 1);
            c.put(2, 2);
            c.assert_result(vec![], 0);
            c.assert_result(vec![1], 1);
            c.assert_result(vec![2], 2);
            c.assert_result(vec![], 3);
        }
    }

    #[test]
    fn two_nonconsecutive_values() {
        for c in cases() {
            c.put(1, 1);
            c.put(3, 3);
            c.assert_result(vec![], 0);
            c.assert_result(vec![1], 1);
            c.assert_result(vec![], 2);
            c.assert_result(vec![3], 3);
            c.assert_result(vec![], 4);
        }
    }

    #[test]
    fn return_to_first_key() {
        for c in cases().into_iter().filter(|c| !c.retain_input_order) {
            for (k, v) in [(3, 31), (2, 21), (1, 11), (1, 12), (2, 22), (3, 32), (3, 33), (2, 23), (1, 13)] {
                c.put(k, v);
            }
            c.assert_result(vec![11, 12, 13], 1);
            c.assert_result(vec![21, 22, 23], 2);
            c.assert_result(vec![31, 32, 33], 3);
            c.assert_result(vec![], 4);
            // Rust implementation also preserves insertion order for appendable maps
            assert_eq!(vec![31, 32, 33], c.get(3));
        }
    }

    #[test]
    fn many_inserts_ordered() {
        for c in cases() {
            for i in 0..100 {
                let to_insert: Vec<i64> = (0..10).map(|j| i * 10 + j + 1).collect();
                c.put_all(i, &to_insert);
            }
            for i in 0..100 {
                c.assert_result((1..=10).map(|j| i * 10 + j).collect(), i);
            }
        }
    }

    #[test]
    fn many_inserts_unordered() {
        for c in cases() {
            for i in (0..100).rev() {
                c.put_all(i, &(1..=10).rev().map(|j| i * 10 + j).collect::<Vec<_>>());
            }
            for i in 0..100 {
                c.assert_result((1..=10).rev().map(|j| i * 10 + j).collect(), i);
            }
        }
    }

    #[test]
    fn multi_insert() {
        for c in cases() {
            c.put_all(1, &[1, 2, 3]);
            c.put(0, 3);
            c.assert_result(vec![3], 0);
            c.assert_result(vec![1, 2, 3], 1);
            c.assert_result(vec![], 2);
        }
    }

    #[test]
    fn write_after_read_fails() {
        let a = new_appendable_multimap();
        a.put(1, 1).unwrap();
        assert_eq!(vec![1], a.get(1));
        assert!(a.put(2, 2).is_err());
        assert!(a.put(1, 0).is_err() && new_appendable_multimap().put(1, 0).is_err());
        let r = new_in_memory_replaceable_multimap();
        r.replace_values(1, &[1]).unwrap();
        assert_eq!(vec![1], r.get(1));
        assert!(r.replace_values(2, &[2]).is_err());
    }

    #[test]
    fn replace_overwrites() {
        let r = new_in_memory_replaceable_multimap();
        r.replace_values(1, &[1, 2]).unwrap();
        r.replace_values(1, &[3]).unwrap();
        assert_eq!(vec![3], r.get(1));
    }
}
