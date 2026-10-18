//! Port of `collection/LongLongMap.java`: a map that stores a single `i64` value for each OSM node.
//!
//! See [`LongLongMapType`] for the available implementations and [`Storage`] for the storage options. Use
//! [`from_config`] / [`from`] to construct one.
//!
//! Threading model (same as Java): one or more writer threads each obtain a [`LongLongMapWriter`] via
//! [`LongLongMap::new_writer`], write all values, and drop the writer. Then any number of threads may call
//! [`LongLongMap::get`] concurrently. The first read "freezes" the map; writes after that fail.
//! [`LongLongMap::supports_parallel_writes`] replaces Java's `instanceof ParallelWrites` check: maps that return
//! `false` only allow one writer at a time and require ascending keys.

use std::fmt;
use std::path::Path;
use std::sync::OnceLock;

use parking_lot::{Mutex, MutexGuard};

use super::append_store::{SmallLongs, create_ints, create_longs};
use super::array_long_long_map_mmap::ArrayLongLongMapMmap;
use super::array_long_long_map_ram::ArrayLongLongMapRam;
use super::sorted_table_long_long_map::SortedTableLongLongMap;
use super::sparse_array_long_long_map::SparseArrayLongLongMap;
use super::storage::{Storage, StorageParams};

/// Value returned by [`LongLongMap::get`] for keys that have no value.
pub const MISSING_VALUE: i64 = i64::MIN;

/// A map from `i64` keys (OSM node IDs) to `i64` values.
pub trait LongLongMap: Send + Sync {
    /// Returns a handle a single thread can use to write into this map. Drop it when finished writing.
    ///
    /// For sequential-write maps only one writer may be open at a time; opening a second one panics.
    fn new_writer(&self) -> Box<dyn LongLongMapWriter + '_>;

    /// Returns the value for `key` or [`MISSING_VALUE`]. Safe to call from many threads after all writers have been
    /// dropped. After the first read, all writes fail.
    fn get(&self, key: i64) -> i64;

    /// Returns the values for each of `keys`.
    fn multi_get(&self, keys: &[i64]) -> Vec<i64> {
        keys.iter().map(|&k| self.get(k)).collect()
    }

    /// Whether multiple writers may write concurrently from different threads (Java `ParallelWrites`).
    fn supports_parallel_writes(&self) -> bool;

    fn disk_usage_bytes(&self) -> u64 {
        0
    }

    fn estimate_memory_usage_bytes(&self) -> u64 {
        0
    }
}

/// A handle for a single thread to use to insert into a [`LongLongMap`]. Writes are flushed when dropped.
pub trait LongLongMapWriter {
    /// Writes the value for a key. Calls must come from a single thread, in ascending order by key for
    /// sequential-write maps. Fails on out-of-order keys or if the map has already been read from.
    fn put(&mut self, key: i64, value: i64) -> anyhow::Result<()>;
}

/// Which long map implementation to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LongLongMapType {
    /// Ignore writes and panic on reads.
    Noop,
    /// Store an ordered list of keys, and an ordered list of values, and on read do a binary search on keys to find
    /// the index of the value to read. Uses exactly 12 bytes per value stored so is ideal for small extracts.
    /// Requires ordered writes from a single thread.
    SortedTable,
    /// Stores values in many small arrays indexed by key, compressing large ranges from the key space. Uses ~9 bytes
    /// per value as the input approaches full planet size. Requires ordered writes from a single thread.
    SparseArray,
    /// Stores values indexed by key without compressing unused ranges so writes can come from many threads in
    /// parallel. Uses exactly `maxNodeId * 8` bytes.
    Array,
}

impl LongLongMapType {
    pub const VALUES: [LongLongMapType; 4] =
        [LongLongMapType::Noop, LongLongMapType::SortedTable, LongLongMapType::SparseArray, LongLongMapType::Array];

    pub fn id(&self) -> &'static str {
        match self {
            LongLongMapType::Noop => "noop",
            LongLongMapType::SortedTable => "sortedtable",
            LongLongMapType::SparseArray => "sparsearray",
            LongLongMapType::Array => "array",
        }
    }

    /// Returns the type associated with `id` (case-insensitive) or an error if no match is found.
    pub fn from_id(id: &str) -> anyhow::Result<LongLongMapType> {
        let id = id.trim();
        LongLongMapType::VALUES
            .into_iter()
            .find(|t| t.id().eq_ignore_ascii_case(id))
            .ok_or_else(|| anyhow::anyhow!("Unexpected long long map type: {id}"))
    }
}

impl fmt::Display for LongLongMapType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// Returns a new long-long map from config strings (Java `LongLongMap.from(String, String, Path, boolean)`).
pub fn from_config(name: &str, storage: &str, path: &Path, madvise: bool) -> anyhow::Result<Box<dyn LongLongMap>> {
    from(LongLongMapType::from_id(name)?, Storage::from_id(storage)?, &StorageParams::new(path, madvise))
}

/// Returns a new long-long map of `map_type` using `storage`.
pub fn from(
    map_type: LongLongMapType,
    storage: Storage,
    params: &StorageParams,
) -> anyhow::Result<Box<dyn LongLongMap>> {
    Ok(match map_type {
        LongLongMapType::Noop => Box::new(NoopLongLongMap),
        LongLongMapType::SparseArray => Box::new(SparseArrayLongLongMap::new(create_longs(storage, params)?)),
        LongLongMapType::SortedTable => Box::new(SortedTableLongLongMap::new(
            Box::new(SmallLongs::new(|i| create_ints(storage, &params.resolve(&format!("keys-{i}"))))?),
            create_longs(storage, &params.resolve("values"))?,
        )),
        LongLongMapType::Array => match storage {
            Storage::Mmap => Box::new(ArrayLongLongMapMmap::new(&params.path, params.madvise)?),
            Storage::Ram | Storage::Direct => Box::new(ArrayLongLongMapRam::new()),
        },
    })
}

/// Returns a new map using [`LongLongMapType::SortedTable`] and [`Storage::Ram`].
pub fn new_in_memory_sorted_table() -> Box<dyn LongLongMap> {
    from(LongLongMapType::SortedTable, Storage::Ram, &StorageParams::default()).expect("in-memory map")
}

/// Returns a map that stores no data and panics on read.
pub fn noop() -> Box<dyn LongLongMap> {
    Box::new(NoopLongLongMap)
}

/// A map that ignores writes and panics on reads.
pub struct NoopLongLongMap;

impl LongLongMap for NoopLongLongMap {
    fn new_writer(&self) -> Box<dyn LongLongMapWriter + '_> {
        struct NoopWriter;
        impl LongLongMapWriter for NoopWriter {
            fn put(&mut self, _key: i64, _value: i64) -> anyhow::Result<()> {
                Ok(())
            }
        }
        Box::new(NoopWriter)
    }

    fn get(&self, _key: i64) -> i64 {
        panic!("get not supported on noop LongLongMap");
    }

    fn supports_parallel_writes(&self) -> bool {
        true
    }
}

/// Holds mutable state `T` that is built by (single-threaded) writes, then frozen on the first read so that reads
/// from many threads are lock-free.
pub(crate) struct SealOnRead<T> {
    building: Mutex<Option<T>>,
    frozen: OnceLock<T>,
}

impl<T> SealOnRead<T> {
    pub(crate) fn new(value: T) -> SealOnRead<T> {
        SealOnRead { building: Mutex::new(Some(value)), frozen: OnceLock::new() }
    }

    /// Mutable access to the building state without locking, or `None` if already frozen.
    pub(crate) fn get_mut(&mut self) -> Option<&mut T> {
        self.building.get_mut().as_mut()
    }

    /// Locks the building state for writing. Panics if another writer holds the lock.
    pub(crate) fn lock_for_write(&self) -> MutexGuard<'_, Option<T>> {
        self.building.try_lock().expect("only one writer may be open at a time on a sequential-write map")
    }

    /// Locks the building state, blocking until any other writer is done.
    pub(crate) fn lock_blocking(&self) -> MutexGuard<'_, Option<T>> {
        self.building.lock()
    }

    /// Returns the frozen state, freezing it on the first call. Panics if a writer is still open.
    #[inline]
    pub(crate) fn read(&self) -> &T {
        self.read_with(|_| {})
    }

    /// Like [`SealOnRead::read`] but calls `prepare` on the state once when freezing it.
    #[inline]
    pub(crate) fn read_with(&self, prepare: impl FnOnce(&mut T)) -> &T {
        if let Some(v) = self.frozen.get() {
            return v;
        }
        self.frozen.get_or_init(|| {
            let mut value = self
                .building
                .try_lock()
                .expect("cannot read from a map while a writer is still open")
                .take()
                .expect("map state missing");
            prepare(&mut value);
            value
        })
    }

    /// Returns whichever state is current without freezing.
    pub(crate) fn peek<R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        if let Some(v) = self.frozen.get() {
            return Some(f(v));
        }
        self.building.try_lock().and_then(|g| g.as_ref().map(f))
    }
}

/// A [`LongLongMapWriter`] for sequential-write maps that holds the map's write lock for its lifetime.
pub(crate) struct SequentialWriter<'a, T, F: FnMut(&mut T, i64, i64) -> anyhow::Result<()>> {
    pub(crate) guard: MutexGuard<'a, Option<T>>,
    pub(crate) put: F,
}

impl<T, F: FnMut(&mut T, i64, i64) -> anyhow::Result<()>> LongLongMapWriter for SequentialWriter<'_, T, F> {
    #[inline]
    fn put(&mut self, key: i64, value: i64) -> anyhow::Result<()> {
        match self.guard.as_mut() {
            Some(state) => (self.put)(state, key, value),
            None => anyhow::bail!("Cannot write to map after it has been read"),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::collection::append_store_ram::AppendStoreRam;

    /// Shared test cases from Java `LongLongMapTest`, run against a fresh map from `create` for each case.
    /// A map plus the temp dir it stores data in (map dropped first).
    pub(crate) struct TestMap(Box<dyn LongLongMap>, #[allow(dead_code)] tempfile::TempDir);

    impl std::ops::Deref for TestMap {
        type Target = dyn LongLongMap;
        fn deref(&self) -> &Self::Target {
            &*self.0
        }
    }

    pub(crate) fn run_sequential_tests(create: &dyn Fn(&Path) -> Box<dyn LongLongMap>) {
        fn with_map(create: &dyn Fn(&Path) -> Box<dyn LongLongMap>, puts: &[(i64, i64)]) -> TestMap {
            let dir = tempfile::tempdir().unwrap();
            let map = create(dir.path());
            {
                let mut writer = map.new_writer();
                for &(k, v) in puts {
                    writer.put(k, v).unwrap();
                }
            }
            TestMap(map, dir)
        }
        // missingValue
        assert_eq!(MISSING_VALUE, with_map(create, &[]).get(0));
        // insertLookup
        let map = with_map(create, &[(1, 1)]);
        assert_eq!(MISSING_VALUE, map.get(0));
        assert_eq!(1, map.get(1));
        assert_eq!(MISSING_VALUE, map.get(2));
        // insertLookupHighValue
        let map = with_map(create, &[(1_000_000, 1)]);
        assert_eq!(MISSING_VALUE, map.get(999_999));
        assert_eq!(1, map.get(1_000_000));
        assert_eq!(MISSING_VALUE, map.get(1_000_001));
        // insertWithGaps
        let map = with_map(create, &[(1, 2), (50, 3), (500, 4), (505, 5)]);
        for (k, v) in [
            (0, MISSING_VALUE),
            (1, 2),
            (2, MISSING_VALUE),
            (49, MISSING_VALUE),
            (50, 3),
            (51, MISSING_VALUE),
            (300, MISSING_VALUE),
            (499, MISSING_VALUE),
            (500, 4),
            (501, MISSING_VALUE),
            (505, 5),
            (506, MISSING_VALUE),
            (1_000, MISSING_VALUE),
        ] {
            assert_eq!(v, map.get(k), "key {k}");
        }
        // insertMultiLookup
        let map = with_map(create, &[(1, 3), (2, 4), (1_000_000, i64::MAX)]);
        assert_eq!(MISSING_VALUE, map.get(0));
        assert_eq!(MISSING_VALUE, map.get(3));
        assert_eq!(vec![3, 4, i64::MAX, MISSING_VALUE], map.multi_get(&[1, 2, 1_000_000, 3]));
        // bigMultiInsert
        let puts: Vec<(i64, i64)> = (0..50_000).map(|i| (i * 4, i + 1)).collect();
        let map = with_map(create, &puts);
        let keys: Vec<i64> = puts.iter().map(|p| p.0).collect();
        let expected: Vec<i64> = puts.iter().map(|p| p.1).collect();
        assert_eq!(expected, map.multi_get(&keys));
    }

    #[test]
    fn sorted_table() {
        run_sequential_tests(&|_| {
            Box::new(SortedTableLongLongMap::new(
                Box::new(SmallLongs::new(|_| Ok(Box::new(AppendStoreRam::<i32>::new()) as _)).unwrap()),
                Box::new(AppendStoreRam::<i64>::new()),
            ))
        });
    }

    #[test]
    fn sparse_array() {
        run_sequential_tests(&|_| Box::new(SparseArrayLongLongMap::new(Box::new(AppendStoreRam::<i64>::new()))));
    }

    #[test]
    fn type_from_id() {
        assert_eq!(LongLongMapType::SparseArray, LongLongMapType::from_id("SparseArray").unwrap());
        assert!(LongLongMapType::from_id("bad").is_err());
    }

    #[test]
    fn all_implementations() {
        let dir = tempfile::tempdir().unwrap();
        for storage in Storage::VALUES {
            for map_type in LongLongMapType::VALUES {
                let variant = format!("{storage}-{map_type}");
                let params = StorageParams::new(dir.path().join(&variant), true);
                let estimate = crate::reader::osm::osm_reader::OsmReader::estimate_node_location_usage(
                    map_type,
                    storage,
                    70_000_000_000,
                    &params.path,
                );
                let usage = if storage == Storage::Mmap {
                    estimate.disk_usage()
                } else {
                    estimate.get(&[&crate::util::resource_usage::MEMORY])
                };
                if map_type != LongLongMapType::Noop {
                    assert!(usage > 60e9 as u64 && usage < 100e9 as u64, "{variant}: {usage}");
                }
                let map = from(map_type, storage, &params).unwrap();
                {
                    let mut writer = map.new_writer();
                    writer.put(2, 3).unwrap();
                    writer.put(4, 5).unwrap();
                }
                if map_type != LongLongMapType::Noop {
                    assert_eq!(3, map.get(2), "{variant}");
                    assert_eq!(5, map.get(4), "{variant}");
                    assert!(map.new_writer().put(6, 7).is_err(), "{variant} write after read");
                }
            }
        }
        let map = from_config("sparsearray", "ram", &dir.path().join("x"), false).unwrap();
        assert!(!map.supports_parallel_writes());
        let map = from_config("array", "mmap", &dir.path().join("y"), false).unwrap();
        assert!(map.supports_parallel_writes());
    }

    #[test]
    #[should_panic]
    fn noop_get_panics() {
        noop().get(1);
    }
}
