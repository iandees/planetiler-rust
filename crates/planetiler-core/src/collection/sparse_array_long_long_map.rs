//! Port of `collection/SparseArrayLongLongMap.java`: a long-long map that only stores values and uses the key as an
//! index into the array, with some tweaks to avoid storing many sequential missing values.

use super::append_store::Longs;
use super::long_long_map::{LongLongMap, LongLongMapWriter, MISSING_VALUE, SealOnRead, SequentialWriter};
use super::sparse_monotonic_long_map::SparseMonotonicLongMap;

struct State {
    // The key space is broken into chunks of 256 and for each chunk with keys, store the index in the values array of
    // its first key shifted left 8 bits, plus the number of missing keys at the start of the chunk in the low 8 bits.
    // Only chunks with keys use memory, so small extracts don't pay for every chunk up to the largest node ID.
    offsets: SparseMonotonicLongMap,
    values: Box<dyn Longs>,
    last_chunk: i64,
    last_offset: i32,
    last_key: i64,
}

impl State {
    #[inline]
    fn put(&mut self, key: i64, value: i64) -> anyhow::Result<()> {
        if key <= self.last_key {
            anyhow::bail!("Nodes must be sorted ascending by ID, {key} came after {}", self.last_key);
        }
        self.last_key = key;
        let idx = self.values.size() as i64;
        let chunk = key >> 8;
        let offset = (key & 255) as i32;

        if chunk != self.last_chunk {
            // new chunk, store offset and leading zeros
            self.last_offset = offset;
            self.offsets.add(chunk, (idx << 8) | offset as i64)?;
            self.last_chunk = chunk;
        } else {
            // same chunk, write missing values until we get to right idx
            self.last_offset += 1;
            while self.last_offset < offset {
                self.values.append_long(MISSING_VALUE)?;
                self.last_offset += 1;
            }
        }
        self.values.append_long(value)
    }

    #[inline]
    fn get(&self, key: i64) -> i64 {
        if key < 0 {
            return MISSING_VALUE;
        }
        let chunk = key >> 8;
        let offset = (key & 255) as u64;
        let entry = self.offsets.get(chunk);
        if entry < 0 {
            return MISSING_VALUE;
        }
        let lo = (entry >> 8) as u64;
        let values_size = self.values.size();
        let next = self.offsets.next(chunk).unwrap_or(-1);
        let hi = if next < 0 { values_size } else { ((next >> 8) as u64).min(values_size) };
        let start_pad = (entry & 255) as u64;
        // index = lo + offset - start_pad, must be in [lo, hi)
        if offset < start_pad {
            return MISSING_VALUE;
        }
        let index = lo + offset - start_pad;
        if index >= hi {
            return MISSING_VALUE;
        }
        self.values.get_long(index)
    }
}

/// Sequential-write long-long map, see [`LongLongMapType::SparseArray`](super::long_long_map::LongLongMapType).
pub struct SparseArrayLongLongMap {
    state: SealOnRead<State>,
}

impl SparseArrayLongLongMap {
    pub fn new(values: Box<dyn Longs>) -> SparseArrayLongLongMap {
        SparseArrayLongLongMap {
            state: SealOnRead::new(State {
                offsets: SparseMonotonicLongMap::new(),
                values,
                last_chunk: -1,
                last_offset: 0,
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

impl LongLongMap for SparseArrayLongLongMap {
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
        self.state.peek(|s| s.values.disk_usage_bytes()).unwrap_or(0)
    }

    fn estimate_memory_usage_bytes(&self) -> u64 {
        self.state
            .peek(|s| s.values.estimate_memory_usage_bytes() + s.offsets.estimate_memory_usage_bytes())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::append_store_ram::AppendStoreRam;

    #[test]
    fn direct_put_and_get() {
        let mut map = SparseArrayLongLongMap::new(Box::new(AppendStoreRam::<i64>::new()));
        map.put(1, 2).unwrap();
        map.put(300, 4).unwrap();
        assert!(map.put(300, 5).is_err());
        map.put(301, 5).unwrap();
        map.put(310, 6).unwrap();
        assert_eq!(2, map.get(1));
        assert_eq!(4, map.get(300));
        assert_eq!(5, map.get(301));
        assert_eq!(MISSING_VALUE, map.get(302));
        assert_eq!(6, map.get(310));
        assert_eq!(MISSING_VALUE, map.get(299));
        assert_eq!(MISSING_VALUE, map.get(-1));
        assert_eq!(MISSING_VALUE, map.get(1 << 40));
        assert!(map.put(400, 1).is_err());
    }

    #[test]
    fn random_sparse() {
        // deterministic pseudo-random gaps
        let mut map = SparseArrayLongLongMap::new(Box::new(AppendStoreRam::<i64>::new()));
        let mut expected = Vec::new();
        let mut key = 0i64;
        let mut seed = 12345u64;
        for _ in 0..100_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            key += 1 + ((seed >> 33) % 600) as i64;
            map.put(key, key * 7).unwrap();
            expected.push(key);
        }
        let mut idx = 0;
        for k in 0..=key {
            let v = map.get(k);
            if idx < expected.len() && expected[idx] == k {
                assert_eq!(k * 7, v);
                idx += 1;
            } else {
                assert_eq!(MISSING_VALUE, v, "key {k}");
            }
        }
    }
}
