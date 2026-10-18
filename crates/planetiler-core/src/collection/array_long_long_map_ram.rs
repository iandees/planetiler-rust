//! Port of `collection/ArrayLongLongMapRam.java`: a map from sequential `i64` keys to `i64` values backed by
//! fixed-size in-memory segments where the key defines the segment and offset into that segment.
//!
//! Supports parallel writes from many threads. Segments are allocated lazily the first time a writer touches them.
//! Missing values are stored as 0, so a value of 0 reads back as [`MISSING_VALUE`] (same as Java).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

use parking_lot::Mutex;

use super::long_long_map::{LongLongMap, LongLongMapWriter, MISSING_VALUE};

type Segment = Arc<[AtomicI64]>;

/// Parallel-write long-long map stored in RAM, see
/// [`LongLongMapType::Array`](super::long_long_map::LongLongMapType).
pub struct ArrayLongLongMapRam {
    segment_bits: u32,
    segment_mask: u64,
    segments: Mutex<Vec<Option<Segment>>>,
    frozen: std::sync::OnceLock<Vec<Option<Segment>>>,
    sealed: AtomicBool,
    num_segments: AtomicUsize,
    active_writers: AtomicUsize,
}

impl ArrayLongLongMapRam {
    /// Creates a map with 1MB segments.
    pub fn new() -> ArrayLongLongMapRam {
        Self::with_segment_bits(20)
    }

    /// Creates a map where each segment is `2^segment_bits` bytes.
    pub fn with_segment_bits(segment_bits: u32) -> ArrayLongLongMapRam {
        assert!(segment_bits >= 3, "Segment size must be a multiple of 8, got 2^{segment_bits}");
        let elem_bits = segment_bits - 3;
        ArrayLongLongMapRam {
            segment_bits: elem_bits,
            segment_mask: (1u64 << elem_bits) - 1,
            segments: Mutex::new(Vec::new()),
            frozen: std::sync::OnceLock::new(),
            sealed: AtomicBool::new(false),
            num_segments: AtomicUsize::new(0),
            active_writers: AtomicUsize::new(0),
        }
    }

    fn get_or_create_segment(&self, index: usize) -> anyhow::Result<Segment> {
        let mut segments = self.segments.lock();
        if self.sealed.load(Ordering::Acquire) {
            anyhow::bail!("Cannot write to map after it has been read");
        }
        if segments.len() <= index {
            segments.resize(index + 1, None);
        }
        let slot = &mut segments[index];
        if slot.is_none() {
            self.num_segments.fetch_add(1, Ordering::Relaxed);
            let seg: Segment = (0..(1usize << self.segment_bits)).map(|_| AtomicI64::new(0)).collect();
            *slot = Some(seg);
        }
        Ok(slot.as_ref().unwrap().clone())
    }

    fn frozen(&self) -> &Vec<Option<Segment>> {
        if let Some(f) = self.frozen.get() {
            return f;
        }
        self.frozen.get_or_init(|| {
            assert_eq!(0, self.active_writers.load(Ordering::Acquire), "cannot read while writers are still open");
            let mut segments = self.segments.lock();
            self.sealed.store(true, Ordering::Release);
            std::mem::take(&mut *segments)
        })
    }
}

impl Default for ArrayLongLongMapRam {
    fn default() -> Self {
        Self::new()
    }
}

struct Writer<'a> {
    map: &'a ArrayLongLongMapRam,
    last_segment: u64,
    segment: Option<Segment>,
}

impl LongLongMapWriter for Writer<'_> {
    #[inline]
    fn put(&mut self, key: i64, value: i64) -> anyhow::Result<()> {
        if key < 0 {
            anyhow::bail!("Negative keys not supported: {key}");
        }
        let key = key as u64;
        let segment = key >> self.map.segment_bits;
        if segment != self.last_segment || self.segment.is_none() {
            if segment >= i32::MAX as u64 {
                anyhow::bail!("Segment {segment} >= Integer.MAX_VALUE");
            }
            self.segment = Some(self.map.get_or_create_segment(segment as usize)?);
            self.last_segment = segment;
        }
        // Relaxed is enough: readers only read after all writers are dropped, and the happens-before edge comes from
        // whatever mechanism the caller used to wait for writer threads (join, channel, ...).
        self.segment.as_ref().unwrap()[(key & self.map.segment_mask) as usize].store(value, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for Writer<'_> {
    fn drop(&mut self) {
        self.map.active_writers.fetch_sub(1, Ordering::AcqRel);
    }
}

impl LongLongMap for ArrayLongLongMapRam {
    fn new_writer(&self) -> Box<dyn LongLongMapWriter + '_> {
        self.active_writers.fetch_add(1, Ordering::AcqRel);
        Box::new(Writer { map: self, last_segment: u64::MAX, segment: None })
    }

    #[inline]
    fn get(&self, key: i64) -> i64 {
        if key < 0 {
            return MISSING_VALUE;
        }
        let key = key as u64;
        let segments = self.frozen();
        let idx = (key >> self.segment_bits) as usize;
        match segments.get(idx) {
            Some(Some(seg)) => {
                let result = seg[(key & self.segment_mask) as usize].load(Ordering::Relaxed);
                if result == 0 { MISSING_VALUE } else { result }
            }
            _ => MISSING_VALUE,
        }
    }

    fn supports_parallel_writes(&self) -> bool {
        true
    }

    fn estimate_memory_usage_bytes(&self) -> u64 {
        let segs = self.num_segments.load(Ordering::Relaxed) as u64;
        segs * ((8u64 << self.segment_bits) + 16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::long_long_map::tests::run_sequential_tests;

    #[test]
    fn sequential() {
        for bits in [4, 10, 20] {
            run_sequential_tests(&|_| Box::new(ArrayLongLongMapRam::with_segment_bits(bits)));
        }
    }

    #[test]
    fn parallel() {
        for bits in [4, 10] {
            crate::collection::array_long_long_map_mmap::tests::run_parallel_tests(&|_| {
                Box::new(ArrayLongLongMapRam::with_segment_bits(bits))
            });
        }
    }
}
