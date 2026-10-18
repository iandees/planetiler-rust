//! Port of `collection/AppendStoreRam.java`: an append-only array of primitives stored in fixed-size in-memory slabs.
//!
//! Java distinguishes heap (`ram`) and off-heap (`direct`) buffers; in Rust both use the same implementation.

use super::append_store::{AppendStore, Ints, Longs, check_index_in_bounds};

/// Primitive element types that can be stored in an append store.
pub trait Primitive: Copy + Default + Send + Sync + 'static {
    const BYTES: usize;
    fn write_ne(self, out: &mut [u8]);
}

impl Primitive for i32 {
    const BYTES: usize = 4;
    #[inline]
    fn write_ne(self, out: &mut [u8]) {
        out.copy_from_slice(&self.to_ne_bytes());
    }
}

impl Primitive for i64 {
    const BYTES: usize = 8;
    #[inline]
    fn write_ne(self, out: &mut [u8]) {
        out.copy_from_slice(&self.to_ne_bytes());
    }
}

/// An append-only array of `T` backed by in-memory slabs of a fixed size, so growing never copies existing data.
pub struct AppendStoreRam<T: Primitive> {
    slabs: Vec<Vec<T>>,
    slab_bits: u32,
    slab_mask: u64,
    slab_elems: usize,
    size: u64,
}

impl<T: Primitive> AppendStoreRam<T> {
    /// Default slab size: 1MB.
    pub const DEFAULT_SEGMENT_BYTES: usize = 1 << 20;

    pub fn new() -> AppendStoreRam<T> {
        Self::with_segment_size(Self::DEFAULT_SEGMENT_BYTES).expect("valid default segment size")
    }

    /// Creates a store with `segment_size_bytes` per slab, which must be a power of 2 and a multiple of 8.
    pub fn with_segment_size(segment_size_bytes: usize) -> anyhow::Result<AppendStoreRam<T>> {
        if !segment_size_bytes.is_power_of_two() {
            anyhow::bail!("Segment size must be a power of 2: {segment_size_bytes}");
        }
        if !segment_size_bytes.is_multiple_of(8) {
            anyhow::bail!("Segment size must be a multiple of 8: {segment_size_bytes}");
        }
        let slab_elems = segment_size_bytes / T::BYTES;
        let slab_bits = slab_elems.trailing_zeros();
        Ok(AppendStoreRam { slabs: Vec::new(), slab_bits, slab_mask: (slab_elems as u64) - 1, slab_elems, size: 0 })
    }

    #[inline]
    fn append(&mut self, value: T) {
        let slab_idx = (self.size >> self.slab_bits) as usize;
        if slab_idx >= self.slabs.len() {
            self.slabs.push(Vec::with_capacity(self.slab_elems));
        }
        // push never reallocates: capacity == slab_elems and a new slab is started once one is full.
        self.slabs[slab_idx].push(value);
        self.size += 1;
    }

    #[inline]
    fn get(&self, index: u64) -> T {
        check_index_in_bounds(index, self.size);
        self.slabs[(index >> self.slab_bits) as usize][(index & self.slab_mask) as usize]
    }
}

impl<T: Primitive> Default for AppendStoreRam<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Primitive> AppendStore for AppendStoreRam<T> {
    fn size(&self) -> u64 {
        self.size
    }

    fn estimate_memory_usage_bytes(&self) -> u64 {
        (self.slabs.len() * (self.slab_elems * T::BYTES + 24)) as u64
    }
}

impl Ints for AppendStoreRam<i32> {
    #[inline]
    fn append_int(&mut self, value: i32) -> anyhow::Result<()> {
        self.append(value);
        Ok(())
    }

    #[inline]
    fn get_int(&self, index: u64) -> i32 {
        self.get(index)
    }
}

impl Longs for AppendStoreRam<i64> {
    #[inline]
    fn append_long(&mut self, value: i64) -> anyhow::Result<()> {
        self.append(value);
        Ok(())
    }

    #[inline]
    fn get_long(&self, index: u64) -> i64 {
        self.get(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_segment_size() {
        assert!(AppendStoreRam::<i64>::with_segment_size(12).is_err());
        assert!(AppendStoreRam::<i64>::with_segment_size(4).is_err());
    }

    #[test]
    fn many_slabs() {
        let mut store = AppendStoreRam::<i64>::with_segment_size(16).unwrap();
        for i in 0..1000 {
            store.append_long(i * 3).unwrap();
        }
        for i in 0..1000 {
            assert_eq!(i as i64 * 3, store.get_long(i));
        }
        assert_eq!(1000, store.size());
    }
}
