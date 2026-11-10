//! Port of `collection/SortedTableLongLongMap.java`: a long-long map that stores keys and values sorted by key and
//! does a binary search to look up values.
//!
//! It's not a full binary search: it keeps track of the first index of each block of 256 keys, so it can do an O(1)
//! lookup to narrow down the search space to at most 256 values.

use super::append_store::Longs;
use super::long_long_map::{LongLongMap, LongLongMapWriter, MISSING_VALUE, SealOnRead, SequentialWriter};
use super::sparse_monotonic_long_map::SparseMonotonicLongMap;

struct State {
    // only chunks with keys use memory, so small extracts don't pay for every chunk up to the largest node ID
    offsets: SparseMonotonicLongMap,
    keys: Box<dyn Longs>,
    values: Box<dyn Longs>,
    last_chunk: i64,
    last_key: i64,
}

impl State {
    #[inline]
    fn put(&mut self, key: i64, value: i64) -> anyhow::Result<()> {
        if key <= self.last_key {
            anyhow::bail!("Nodes must be sorted ascending by ID, {key} came after {}", self.last_key);
        }
        self.last_key = key;
        let idx = self.keys.size() as i64;
        let chunk = key >> 8;
        if chunk != self.last_chunk {
            self.offsets.add(chunk, idx)?;
            self.last_chunk = chunk;
        }
        self.keys.append_long(key)?;
        self.values.append_long(value)
    }

    #[inline]
    fn get(&self, key: i64) -> i64 {
        if key < 0 {
            return MISSING_VALUE;
        }
        let chunk = key >> 8;
        // use the "offsets" index to narrow search space to <256 values: [lo, hi)
        let lo = self.offsets.get(chunk);
        if lo < 0 {
            return MISSING_VALUE;
        }
        let mut lo = lo as u64;
        let keys_size = self.keys.size();
        let next = self.offsets.next(chunk).unwrap_or(-1);
        let mut hi = if next < 0 { keys_size } else { (next as u64).min(keys_size) };
        while lo < hi {
            let idx = (lo + hi) >> 1;
            let value = self.keys.get_long(idx);
            if value < key {
                lo = idx + 1;
            } else if value > key {
                hi = idx;
            } else {
                return self.values.get_long(idx);
            }
        }
        MISSING_VALUE
    }
}

/// Sequential-write long-long map, see [`LongLongMapType::SortedTable`](super::long_long_map::LongLongMapType).
pub struct SortedTableLongLongMap {
    state: SealOnRead<State>,
}

impl SortedTableLongLongMap {
    pub fn new(keys: Box<dyn Longs>, values: Box<dyn Longs>) -> SortedTableLongLongMap {
        SortedTableLongLongMap {
            state: SealOnRead::new(State {
                offsets: SparseMonotonicLongMap::new(),
                keys,
                values,
                last_chunk: -1,
                last_key: -1,
            }),
        }
    }

    /// Writes a value directly without a writer handle (Java `SequentialWrites.put`).
    pub fn put(&mut self, key: i64, value: i64) -> anyhow::Result<()> {
        match self.state.get_mut() {
            Some(s) => s.put(key, value),
            None => anyhow::bail!("Cannot write to map after it has been read"),
        }
    }
}

impl LongLongMap for SortedTableLongLongMap {
    fn new_writer(&self) -> Box<dyn LongLongMapWriter + '_> {
        Box::new(SequentialWriter { guard: self.state.lock_for_write(), put: State::put })
    }

    #[inline]
    fn get(&self, key: i64) -> i64 {
        self.state.read().get(key)
    }

    fn supports_parallel_writes(&self) -> bool {
        false
    }

    fn disk_usage_bytes(&self) -> u64 {
        self.state.peek(|s| s.keys.disk_usage_bytes() + s.values.disk_usage_bytes()).unwrap_or(0)
    }

    fn estimate_memory_usage_bytes(&self) -> u64 {
        self.state
            .peek(|s| {
                s.keys.estimate_memory_usage_bytes()
                    + s.values.estimate_memory_usage_bytes()
                    + s.offsets.estimate_memory_usage_bytes()
            })
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::append_store::SmallLongs;
    use crate::collection::append_store_mmap::AppendStoreMmap;

    #[test]
    fn mmap_backed() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_path_buf();
        let mut map = SortedTableLongLongMap::new(
            Box::new(
                SmallLongs::new(|i| Ok(Box::new(AppendStoreMmap::<i32>::new(&d.join(format!("k{i}")), true)?) as _))
                    .unwrap(),
            ),
            Box::new(AppendStoreMmap::<i64>::new(&d.join("v"), true).unwrap()),
        );
        for i in 0..10_000 {
            map.put(i * 3 + 1, i).unwrap();
        }
        for i in 0..10_000 {
            assert_eq!(i, map.get(i * 3 + 1));
            assert_eq!(MISSING_VALUE, map.get(i * 3 + 2));
        }
        assert!(map.disk_usage_bytes() > 0);
    }
}
