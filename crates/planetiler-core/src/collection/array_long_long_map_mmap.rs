//! Port of `collection/ArrayLongLongMapMmap.java`: a map from sequential `i64` keys to `i64` values backed by a file
//! on disk where the key defines the offset in the file.
//!
//! Java buffers a sliding window of segments in heap `ByteBuffer`s and flushes them with positioned writes when all
//! workers have moved past them. In Rust, writers instead store values directly into shared writable memory-mapped
//! segments of a sparse file (segments are mapped lazily as writers reach them and the file grows as needed). The
//! kernel's page cache handles write-back, untouched regions stay as holes in the file, and multiple writers can
//! write to the same segment concurrently. On the first read all writable mappings are dropped and the whole file is
//! mapped read-only, optionally with `madvise(MADV_RANDOM)`.
//!
//! Missing values are stored as 0, so a value of 0 reads back as [`MISSING_VALUE`] (same as Java).

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

use anyhow::Context;
use memmap2::{Mmap, MmapOptions, MmapRaw};
use parking_lot::Mutex;

use super::append_store_mmap::map_read_only;
use super::long_long_map::{LongLongMap, LongLongMapWriter, MISSING_VALUE};

/// 1GB writable segments by default.
const DEFAULT_SEGMENT_BITS: u32 = 30;
/// Smallest segment we map: 64KB is a multiple of the page size on all supported platforms.
const MIN_SEGMENT_BITS: u32 = 16;
/// Largest segment we accept (1TB) so that `1 << segment_bits` and `index << segment_bits` can't overflow.
const MAX_SEGMENT_BITS: u32 = 40;

struct WriteState {
    file: Option<File>,
    file_len: u64,
    /// Writable segments. `MmapRaw` hands out raw pointers without ever creating a `&mut [u8]`, so concurrent
    /// writers storing through those pointers never alias a unique reference.
    segments: Vec<Option<MmapRaw>>,
}

/// Parallel-write long-long map stored in a memory-mapped file, see
/// [`LongLongMapType::Array`](super::long_long_map::LongLongMapType).
pub struct ArrayLongLongMapMmap {
    path: PathBuf,
    madvise: bool,
    segment_bits: u32,
    write: Mutex<WriteState>,
    sealed: AtomicBool,
    active_writers: AtomicUsize,
    read: OnceLock<Option<Mmap>>,
}

impl ArrayLongLongMapMmap {
    /// Creates a new map stored at `path` with 1GB segments.
    pub fn new(path: &Path, madvise: bool) -> anyhow::Result<ArrayLongLongMapMmap> {
        Self::with_segment_bits(path, DEFAULT_SEGMENT_BITS, madvise)
    }

    /// Creates a new map stored at `path` where each writable segment is `2^segment_bits` bytes (rounded up to at
    /// least 64KB).
    pub fn with_segment_bits(path: &Path, segment_bits: u32, madvise: bool) -> anyhow::Result<ArrayLongLongMapMmap> {
        if segment_bits < 3 {
            anyhow::bail!("Segment size must be a multiple of 8, got 2^{segment_bits}");
        }
        if segment_bits > MAX_SEGMENT_BITS {
            anyhow::bail!("Segment size must be at most 2^{MAX_SEGMENT_BITS}, got 2^{segment_bits}");
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("Unable to open {path:?}"))?;
        Ok(ArrayLongLongMapMmap {
            path: path.to_path_buf(),
            madvise,
            segment_bits: segment_bits.max(MIN_SEGMENT_BITS),
            write: Mutex::new(WriteState { file: Some(file), file_len: 0, segments: Vec::new() }),
            sealed: AtomicBool::new(false),
            active_writers: AtomicUsize::new(0),
            read: OnceLock::new(),
        })
    }

    /// Returns a pointer to the start of writable segment `index`, mapping it (and growing the file) if necessary.
    fn segment_ptr(&self, index: u64) -> anyhow::Result<*mut i64> {
        let mut state = self.write.lock();
        if self.sealed.load(Ordering::Acquire) {
            anyhow::bail!("Cannot write to map after it has been read");
        }
        let state = &mut *state;
        let idx = index as usize;
        if state.segments.len() <= idx {
            state.segments.resize_with(idx + 1, || None);
        }
        if state.segments[idx].is_none() {
            let seg_bytes = 1u64 << self.segment_bits;
            let offset = index << self.segment_bits;
            let file = state.file.as_ref().expect("file open while writing");
            let needed = offset + seg_bytes;
            if state.file_len < needed {
                // grows the file sparsely: untouched regions do not use disk space
                file.set_len(needed).with_context(|| format!("Unable to grow {:?}", self.path))?;
                state.file_len = needed;
            }
            // The file is private to this map, is at least `offset + seg_bytes` long and is never truncated while
            // mapped. Writers access the mapping only through atomics on raw pointers (see `Writer::put`).
            let map = MmapOptions::new()
                .offset(offset)
                .len(seg_bytes as usize)
                .map_raw(file)
                .with_context(|| format!("Unable to map segment {index} of {:?}", self.path))?;
            state.segments[idx] = Some(map);
        }
        Ok(state.segments[idx].as_ref().unwrap().as_mut_ptr() as *mut i64)
    }

    fn read_map(&self) -> Option<&Mmap> {
        if let Some(m) = self.read.get() {
            return m.as_ref();
        }
        self.read
            .get_or_init(|| {
                // Check the writer count and seal under the same lock `new_writer` registers writers under, so no
                // writer can be created (and map a segment) between the check and the unmapping below.
                let mut state = self.write.lock();
                assert_eq!(0, self.active_writers.load(Ordering::Acquire), "cannot read while writers are still open");
                self.sealed.store(true, Ordering::Release);
                // dropping writable mappings is safe: no writers exist, and data stays in the shared page cache
                state.segments.clear();
                let file = state.file.take().expect("file open");
                map_read_only(&file, self.madvise).unwrap_or_else(|e| panic!("Unable to map {:?}: {e}", self.path))
            })
            .as_ref()
    }
}

struct Writer<'a> {
    map: &'a ArrayLongLongMapMmap,
    last_segment: u64,
    base: *mut i64,
}

// SAFETY: `base` points into a shared memory mapping owned by `map` that stays alive (is not unmapped) until all
// writers are dropped, and all accesses through it are atomic, so moving the writer to another thread is sound.
unsafe impl Send for Writer<'_> {}

impl LongLongMapWriter for Writer<'_> {
    #[inline]
    fn put(&mut self, key: i64, value: i64) -> anyhow::Result<()> {
        if key < 0 {
            anyhow::bail!("Negative keys not supported: {key}");
        }
        let key = key as u64;
        let elem_bits = self.map.segment_bits - 3;
        let segment = key >> elem_bits;
        if segment != self.last_segment || self.base.is_null() {
            if segment >= i32::MAX as u64 {
                anyhow::bail!("Segment {segment} > Integer.MAX_VALUE");
            }
            self.base = self.map.segment_ptr(segment)?;
            self.last_segment = segment;
        }
        let offset = (key & ((1u64 << elem_bits) - 1)) as usize;
        // SAFETY: `base` is the start of a live, page-aligned writable mapping of 2^segment_bits bytes, so `offset`
        // (< 2^(segment_bits-3)) is in bounds and 8-byte aligned. The mapping is not unmapped while this writer exists
        // (`read_map` asserts there are no active writers before dropping mappings, under the same lock that
        // `new_writer` registers writers under). Other writers may write the same
        // memory concurrently, so the store is atomic to avoid a data race.
        unsafe { (*(self.base.add(offset) as *const AtomicI64)).store(value, Ordering::Relaxed) };
        Ok(())
    }
}

impl Drop for Writer<'_> {
    fn drop(&mut self) {
        self.map.active_writers.fetch_sub(1, Ordering::AcqRel);
    }
}

impl LongLongMap for ArrayLongLongMapMmap {
    fn new_writer(&self) -> Box<dyn LongLongMapWriter + '_> {
        // Register under the write lock so `read_map` can't seal between its writer-count check and unmapping. A
        // writer created after sealing never gets a segment pointer: its first `put` fails in `segment_ptr`.
        let _state = self.write.lock();
        self.active_writers.fetch_add(1, Ordering::AcqRel);
        Box::new(Writer { map: self, last_segment: u64::MAX, base: std::ptr::null_mut() })
    }

    #[inline]
    fn get(&self, key: i64) -> i64 {
        if key < 0 {
            return MISSING_VALUE;
        }
        let Some(map) = self.read_map() else {
            return MISSING_VALUE;
        };
        let idx = key as u64 as usize;
        if idx >= map.len() / 8 {
            return MISSING_VALUE;
        }
        // SAFETY: idx * 8 + 8 <= map.len(), the mapping is page-aligned so the i64 read is aligned, and the mapping is
        // read-only and immutable for the rest of this map's lifetime.
        let result = unsafe { (map.as_ptr() as *const i64).add(idx).read() };
        if result == 0 { MISSING_VALUE } else { result }
    }

    fn supports_parallel_writes(&self) -> bool {
        true
    }

    fn disk_usage_bytes(&self) -> u64 {
        crate::util::file_utils::size(&self.path).unwrap_or(0)
    }
}

impl Drop for ArrayLongLongMapMmap {
    fn drop(&mut self) {
        // unmap before deleting
        self.write.get_mut().segments.clear();
        self.read.take();
        self.write.get_mut().file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::collection::long_long_map::tests::run_sequential_tests;
    use std::sync::Barrier;

    /// Java `ParallelLongLongMapTest`.
    pub(crate) fn run_parallel_tests(create: &dyn Fn(&Path) -> Box<dyn LongLongMap>) {
        // testWaitForBothWritersToClose
        {
            let dir = tempfile::tempdir().unwrap();
            let map = create(dir.path());
            let mut writer1 = map.new_writer();
            let mut writer2 = map.new_writer();
            writer1.put(0, 1).unwrap();
            drop(writer1);
            writer2.put(1, 2).unwrap();
            drop(writer2);
            assert_eq!(1, map.get(0));
            assert_eq!(2, map.get(1));
        }
        let limit = 1000i64;
        // testInterleavedWritesFromParallelThreads
        {
            let dir = tempfile::tempdir().unwrap();
            let map = create(dir.path());
            let ready = Barrier::new(2);
            std::thread::scope(|s| {
                s.spawn(|| {
                    let mut writer = map.new_writer();
                    ready.wait();
                    for i in 1..limit {
                        writer.put(i * 2, i * 2).unwrap();
                    }
                });
                s.spawn(|| {
                    let mut writer = map.new_writer();
                    ready.wait();
                    for i in 1..limit {
                        writer.put(i * 10 + 1, i * 3).unwrap();
                    }
                });
            });
            assert_eq!(MISSING_VALUE, map.get(0));
            for i in 1..limit {
                assert_eq!(i * 2, map.get(i * 2), "item:{}", i * 2);
                assert_eq!(i * 3, map.get(i * 10 + 1), "item:{}", i * 10 + 1);
            }
        }
        // testAdjacentBlocksFromParallelThreads
        {
            let dir = tempfile::tempdir().unwrap();
            let map = create(dir.path());
            let ready = Barrier::new(2);
            std::thread::scope(|s| {
                s.spawn(|| {
                    let mut writer = map.new_writer();
                    ready.wait();
                    for i in 1..limit {
                        writer.put(i, i).unwrap();
                    }
                });
                s.spawn(|| {
                    let mut writer = map.new_writer();
                    ready.wait();
                    for i in 1..limit * 2 {
                        writer.put(i + limit, i * 2).unwrap();
                    }
                });
            });
            assert_eq!(MISSING_VALUE, map.get(0));
            for i in 1..limit {
                assert_eq!(i, map.get(i), "item:{i}");
                assert_eq!(i * 2, map.get(i + limit), "item:{}", i * 2);
            }
        }
    }

    #[test]
    fn sequential() {
        for bits in [4, 20, 30] {
            run_sequential_tests(&|p| {
                Box::new(ArrayLongLongMapMmap::with_segment_bits(&p.join("node.db"), bits, true).unwrap())
            });
        }
    }

    #[test]
    fn parallel() {
        for bits in [4, 20] {
            run_parallel_tests(&|p| {
                Box::new(ArrayLongLongMapMmap::with_segment_bits(&p.join("node.db"), bits, true).unwrap())
            });
        }
    }

    #[test]
    fn deletes_file_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.db");
        {
            let map = ArrayLongLongMapMmap::new(&path, false).unwrap();
            map.new_writer().put(5, 6).unwrap();
            assert_eq!(6, map.get(5));
            assert!(path.exists());
        }
        assert!(!path.exists());
    }

    #[test]
    fn rejects_huge_segments() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ArrayLongLongMapMmap::with_segment_bits(&dir.path().join("a.db"), 64, false).is_err());
        assert!(ArrayLongLongMapMmap::with_segment_bits(&dir.path().join("b.db"), MAX_SEGMENT_BITS, false).is_ok());
    }

    #[test]
    fn writer_created_after_read_fails() {
        let dir = tempfile::tempdir().unwrap();
        let map = ArrayLongLongMapMmap::with_segment_bits(&dir.path().join("node.db"), 16, false).unwrap();
        map.new_writer().put(1, 2).unwrap();
        assert_eq!(2, map.get(1));
        let mut writer = map.new_writer();
        assert!(writer.put(3, 4).is_err());
        drop(writer);
        assert_eq!(MISSING_VALUE, map.get(3));
    }

    #[test]
    #[should_panic(expected = "cannot read while writers are still open")]
    fn read_with_open_writer_panics() {
        let dir = tempfile::tempdir().unwrap();
        let map = ArrayLongLongMapMmap::with_segment_bits(&dir.path().join("node.db"), 16, false).unwrap();
        let mut writer = map.new_writer();
        writer.put(1, 2).unwrap();
        map.get(1);
    }

    /// Readers racing against writers that are being created must never let a writer store into a segment that has
    /// been unmapped: every outcome is either a failed `put`, or a read that panics because writers are open.
    #[test]
    fn racing_read_and_new_writer_is_safe() {
        for _ in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let map = ArrayLongLongMapMmap::with_segment_bits(&dir.path().join("node.db"), 16, false).unwrap();
            let start = Barrier::new(2);
            std::thread::scope(|s| {
                s.spawn(|| {
                    start.wait();
                    for i in 0..2000 {
                        let mut writer = map.new_writer();
                        for j in 0..10 {
                            if writer.put(i * 10 + j, 1).is_err() {
                                return;
                            }
                        }
                    }
                });
                s.spawn(|| {
                    start.wait();
                    loop {
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| map.get(0)));
                        if r.is_ok() {
                            break;
                        }
                    }
                });
            });
            assert!(map.new_writer().put(0, 1).is_err());
        }
    }
}
