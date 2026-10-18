//! Port of `collection/AppendStore.java`: a large append-only array of primitives.
//!
//! A single thread appends all elements (through `&mut self`) then many threads may read with random access (through
//! `&self`). [`AppendStoreRam`](super::append_store_ram::AppendStoreRam) keeps the data in fixed-size slabs in memory
//! and [`AppendStoreMmap`](super::append_store_mmap::AppendStoreMmap) writes it to a file that gets memory-mapped on
//! the first read. Once a memory-mapped store has been read from, further appends fail.

use super::append_store_mmap::AppendStoreMmap;
use super::append_store_ram::AppendStoreRam;
use super::storage::{Storage, StorageParams};

/// Common functionality of all append stores.
pub trait AppendStore: Send + Sync {
    /// Number of elements in the array.
    fn size(&self) -> u64;

    /// Estimated number of bytes of RAM used by this store.
    fn estimate_memory_usage_bytes(&self) -> u64 {
        0
    }

    /// Number of bytes on disk used by this store.
    fn disk_usage_bytes(&self) -> u64 {
        0
    }
}

/// Panics with an index out of bounds message if `index >= size`.
#[inline]
#[track_caller]
pub(crate) fn check_index_in_bounds(index: u64, size: u64) {
    if index >= size {
        panic!("index out of bounds: index: {index} size: {size}");
    }
}

/// An append-only array of `i32`.
pub trait Ints: AppendStore {
    /// Appends a value. Fails if writing to disk fails or if the store has already been read from.
    fn append_int(&mut self, value: i32) -> anyhow::Result<()>;

    /// Returns the value at `index`. Panics if `index >= size()`.
    fn get_int(&self, index: u64) -> i32;
}

/// An append-only array of `i64`.
pub trait Longs: AppendStore {
    /// Appends a value. Fails if writing to disk fails or if the store has already been read from.
    fn append_long(&mut self, value: i64) -> anyhow::Result<()>;

    /// Returns the value at `index`. Panics if `index >= size()`.
    fn get_long(&self, index: u64) -> i64;
}

/// Returns a new [`Ints`] store using `storage`.
pub fn create_ints(storage: Storage, params: &StorageParams) -> anyhow::Result<Box<dyn Ints>> {
    Ok(match storage {
        Storage::Ram | Storage::Direct => Box::new(AppendStoreRam::<i32>::new()),
        Storage::Mmap => Box::new(AppendStoreMmap::<i32>::new(&params.path, params.madvise)?),
    })
}

/// Returns a new [`Longs`] store using `storage`.
pub fn create_longs(storage: Storage, params: &StorageParams) -> anyhow::Result<Box<dyn Longs>> {
    Ok(match storage {
        Storage::Ram | Storage::Direct => Box::new(AppendStoreRam::<i64>::new()),
        Storage::Mmap => Box::new(AppendStoreMmap::<i64>::new(&params.path, params.madvise)?),
    })
}

/// An array of longs that uses 4 bytes to represent each long by using a list of [`Ints`] arrays. Only suitable for
/// values less than ~20 billion (i.e. OSM node IDs) that are appended in ascending order.
pub struct SmallLongs {
    ints: Vec<Box<dyn Ints>>,
    num_written: u64,
}

impl SmallLongs {
    const BITS: u32 = 31;
    const MASK: i64 = (1 << Self::BITS) - 1;
    const SLABS: usize = 10;

    /// Creates a new store, calling `supplier(i)` to create each of the underlying int arrays.
    pub fn new(mut supplier: impl FnMut(usize) -> anyhow::Result<Box<dyn Ints>>) -> anyhow::Result<SmallLongs> {
        let ints = (0..Self::SLABS).map(&mut supplier).collect::<anyhow::Result<Vec<_>>>()?;
        Ok(SmallLongs { ints, num_written: 0 })
    }
}

impl AppendStore for SmallLongs {
    fn size(&self) -> u64 {
        self.num_written
    }

    fn estimate_memory_usage_bytes(&self) -> u64 {
        self.ints.iter().map(|i| i.estimate_memory_usage_bytes()).sum()
    }

    fn disk_usage_bytes(&self) -> u64 {
        self.ints.iter().map(|i| i.disk_usage_bytes()).sum()
    }
}

impl Longs for SmallLongs {
    fn append_long(&mut self, value: i64) -> anyhow::Result<()> {
        let block = (value as u64 >> Self::BITS) as usize;
        let offset = (value & Self::MASK) as i32;
        let Some(slab) = self.ints.get_mut(block) else {
            anyhow::bail!("value {value} too large for SmallLongs");
        };
        slab.append_int(offset)?;
        self.num_written += 1;
        Ok(())
    }

    fn get_long(&self, index: u64) -> i64 {
        check_index_in_bounds(index, self.size());
        let mut index = index;
        for (i, slab) in self.ints.iter().enumerate() {
            let size = slab.size();
            if index < size {
                return slab.get_int(index) as i64 + ((i as i64) << Self::BITS);
            }
            index -= size;
        }
        panic!("index out of bounds: index: {index} size: {}", self.size());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    const SEGMENT: usize = 4 << 2;

    fn int_stores(dir: &std::path::Path) -> Vec<(&'static str, Box<dyn Ints>)> {
        vec![
            ("ram", Box::new(AppendStoreRam::<i32>::with_segment_size(SEGMENT).unwrap()) as Box<dyn Ints>),
            ("mmap", Box::new(AppendStoreMmap::<i32>::new(&dir.join("ints"), true).unwrap())),
        ]
    }

    fn long_stores(dir: &std::path::Path) -> Vec<(&'static str, Box<dyn Longs>)> {
        vec![
            ("ram", Box::new(AppendStoreRam::<i64>::with_segment_size(SEGMENT).unwrap()) as Box<dyn Longs>),
            ("mmap", Box::new(AppendStoreMmap::<i64>::new(&dir.join("longs"), true).unwrap())),
            (
                "mmap-small",
                Box::new(
                    SmallLongs::new(|i| {
                        Ok(Box::new(AppendStoreMmap::<i32>::new(&dir.join(format!("smalllongs{i}")), true)?) as _)
                    })
                    .unwrap(),
                ),
            ),
            (
                "ram-small",
                Box::new(
                    SmallLongs::new(|_| Ok(Box::new(AppendStoreRam::<i32>::with_segment_size(SEGMENT)?) as _)).unwrap(),
                ),
            ),
        ]
    }

    fn panics(f: impl FnOnce()) -> bool {
        catch_unwind(AssertUnwindSafe(f)).is_err()
    }

    #[test]
    fn ints_write_then_read() {
        for num in 0..10 {
            let dir = tempfile::tempdir().unwrap();
            for (name, mut store) in int_stores(dir.path()) {
                for i in 0..num {
                    store.append_int(i + 1).unwrap();
                }
                for i in 0..num {
                    assert_eq!(i + 1, store.get_int(i as u64), "{name}");
                }
                assert!(panics(|| {
                    store.get_int(num as u64);
                }));
                assert!(panics(|| {
                    store.get_int(num as u64 + 1);
                }));
            }
        }
    }

    #[test]
    fn ints_read_big() {
        let dir = tempfile::tempdir().unwrap();
        for (name, mut store) in int_stores(dir.path()) {
            store.append_int(i32::MAX).unwrap();
            store.append_int(i32::MAX - 1).unwrap();
            store.append_int(i32::MAX - 2).unwrap();
            assert_eq!(i32::MAX, store.get_int(0), "{name}");
            assert_eq!(i32::MAX - 1, store.get_int(1), "{name}");
            assert_eq!(i32::MAX - 2, store.get_int(2), "{name}");
        }
    }

    #[test]
    fn longs_write_then_read() {
        for num in 0..10 {
            let dir = tempfile::tempdir().unwrap();
            for (name, mut store) in long_stores(dir.path()) {
                for i in 0..num {
                    store.append_long(i + 1).unwrap();
                }
                for i in 0..num {
                    assert_eq!(i + 1, store.get_long(i as u64), "{name}");
                }
                assert!(panics(|| {
                    store.get_long(num as u64);
                }));
                assert!(panics(|| {
                    store.get_long(num as u64 + 1);
                }));
            }
        }
    }

    #[test]
    fn longs_read_big() {
        let max_int = i32::MAX as i64;
        for value in [max_int - 1, max_int, max_int + 1, 2 * max_int - 1, 2 * max_int, 5 * max_int - 1, 5 * max_int + 1]
        {
            let dir = tempfile::tempdir().unwrap();
            for (name, mut store) in long_stores(dir.path()) {
                store.append_long(value).unwrap();
                assert_eq!(value, store.get_long(0), "{name}");
            }
        }
    }

    #[test]
    fn mmap_append_after_read_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AppendStoreMmap::<i64>::new(&dir.path().join("x"), false).unwrap();
        store.append_long(1).unwrap();
        assert_eq!(1, store.get_long(0));
        assert!(store.append_long(2).is_err());
    }

    #[test]
    fn factory() {
        let dir = tempfile::tempdir().unwrap();
        for storage in Storage::VALUES {
            let params = StorageParams::new(dir.path().join(storage.id()), true);
            let mut longs = create_longs(storage, &params.resolve("longs")).unwrap();
            let mut ints = create_ints(storage, &params.resolve("ints")).unwrap();
            longs.append_long(5).unwrap();
            ints.append_int(6).unwrap();
            assert_eq!(5, longs.get_long(0));
            assert_eq!(6, ints.get_int(0));
        }
    }
}
