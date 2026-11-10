//! Port of `collection/SparseMonotonicLongMap.java`: a map from non-negative `i64` keys to non-negative `i64` values
//! where keys and values are both added in ascending order.
//!
//! Keys are stored in 512-entry pages where a page only exists if it has values. Within each page, sparse values are
//! packed into a dense array using a presence bitmap to indicate which keys have a value.

use anyhow::{Result, bail};

// 512 entries per page
const PAGE_BITS: u32 = 9;
const PAGE_SIZE: usize = 1 << PAGE_BITS;
const PAGE_MASK: i64 = (PAGE_SIZE - 1) as i64;

/// Stores the value of each key in the page that was added, using a bitmap of which keys were added to find its index,
/// and storing values as `i32`s relative to the first one to use half the memory.
struct Page {
    // one bit per key in the page, set if the key was added
    bitmap: [u64; PAGE_SIZE / 64],
    // all values stored as a delta from base
    base: i64,
    // the value of each added key, in order; plus the next page's first value once it starts, so the last key here
    // knows where it ends
    values: Vec<i32>,
}

impl Page {
    fn new(base: i64) -> Page {
        Page { bitmap: [0; PAGE_SIZE / 64], base, values: Vec::new() }
    }

    fn add(&mut self, key: i64, value: i64) -> Result<()> {
        let bit = (key & PAGE_MASK) as usize;
        self.append(value)?;
        self.bitmap[bit >> 6] |= 1u64 << (bit & 63);
        Ok(())
    }

    fn append(&mut self, value: i64) -> Result<()> {
        match value.checked_sub(self.base).and_then(|d| i32::try_from(d).ok()) {
            Some(delta) => {
                self.values.push(delta);
                Ok(())
            }
            None => bail!("integer overflow: {value} is too far from page base {}", self.base),
        }
    }

    /// Returns the index of `key` in `values`, or `None` if it was not added.
    #[inline]
    fn index_of(&self, key: i64) -> Option<usize> {
        let bit = (key & PAGE_MASK) as usize;
        let word = bit >> 6;
        let bits = self.bitmap[word];
        if bits & (1u64 << (bit & 63)) == 0 {
            return None;
        }
        let mut index = (bits & ((1u64 << (bit & 63)) - 1)).count_ones() as usize;
        for w in &self.bitmap[..word] {
            index += w.count_ones() as usize;
        }
        Some(index)
    }

    /// Returns the value of `key`, or -1 if it was not added.
    #[inline]
    fn get(&self, key: i64) -> i64 {
        match self.index_of(key) {
            Some(index) => self.base + self.values[index] as i64,
            None => -1,
        }
    }

    /// Returns the value of the key added after `key`, -1 if there is none, or `None` if `key` was not added.
    #[inline]
    fn next(&self, key: i64) -> Option<i64> {
        let index = self.index_of(key)?;
        Some(self.values.get(index + 1).map_or(-1, |&v| self.base + v as i64))
    }
}

/// A map from non-negative `i64` keys to non-negative `i64` values where keys and values are both added in ascending
/// order. Only pages (of 512 keys) that have keys use memory.
pub struct SparseMonotonicLongMap {
    pages: Vec<Option<Box<Page>>>,
    num_pages: usize,
    // index of the last page, which is still being added to
    last_page: Option<usize>,
    last_key: i64,
}

impl Default for SparseMonotonicLongMap {
    fn default() -> Self {
        Self::new()
    }
}

impl SparseMonotonicLongMap {
    pub fn new() -> SparseMonotonicLongMap {
        let mut pages = Vec::new();
        pages.resize_with(16, || None);
        SparseMonotonicLongMap { pages, num_pages: 0, last_page: None, last_key: -1 }
    }

    /// Adds `value` for `key`, which must be greater than any key added before.
    pub fn add(&mut self, key: i64, value: i64) -> Result<()> {
        if key <= self.last_key {
            bail!("Keys must be added in ascending order, got {key} after {}", self.last_key);
        }
        let Ok(page_num) = i32::try_from(key >> PAGE_BITS) else {
            bail!("integer overflow: key {key}");
        };
        let page_num = page_num as usize;
        if page_num >= self.num_pages {
            if let Some(last) = self.last_page {
                // the previous page ends where this one starts so callers can get value or next value for a key
                let page = self.pages[last].as_mut().expect("last page exists");
                page.append(value)?;
                page.values.shrink_to_fit();
            }
            if page_num >= self.pages.len() {
                let new_len = (page_num + 1).max(self.pages.len() * 2);
                self.pages.resize_with(new_len, || None);
            }
            self.pages[page_num] = Some(Box::new(Page::new(value)));
            self.last_page = Some(page_num);
            self.num_pages = page_num + 1;
        }
        self.last_key = key;
        let last = self.last_page.expect("last page set");
        self.pages[last].as_mut().expect("last page exists").add(key, value)
    }

    /// Returns the value of `key`, or -1 if it was not added.
    #[inline]
    pub fn get(&self, key: i64) -> i64 {
        self.page(key).map_or(-1, |page| page.get(key))
    }

    /// Returns the value of the next key added after `key`, which must have been added, or -1 if it is the last one.
    /// Returns `None` if `key` was not added.
    #[inline]
    pub fn next(&self, key: i64) -> Option<i64> {
        self.page(key)?.next(key)
    }

    #[inline]
    fn page(&self, key: i64) -> Option<&Page> {
        if key < 0 {
            return None;
        }
        let page_num = (key >> PAGE_BITS) as usize;
        if page_num >= self.num_pages { None } else { self.pages[page_num].as_deref() }
    }

    pub fn estimate_memory_usage_bytes(&self) -> u64 {
        let mut result = (self.pages.capacity() * size_of::<Option<Box<Page>>>()) as u64;
        for page in self.pages[..self.num_pages].iter().flatten() {
            result += (size_of::<Page>() + page.values.capacity() * size_of::<i32>()) as u64;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty() {
        let map = SparseMonotonicLongMap::new();
        assert_eq!(-1, map.get(0));
        assert_eq!(-1, map.get(1));
        assert_eq!(-1, map.get(1_000_000));
        assert_eq!(-1, map.get(-1));
    }

    #[test]
    fn test_single_key() {
        let mut map = SparseMonotonicLongMap::new();
        map.add(5, 10).unwrap();
        assert_eq!(10, map.get(5));
        assert_eq!(Some(-1), map.next(5));
        assert_eq!(-1, map.get(4));
        assert_eq!(-1, map.get(6));
        assert_eq!(-1, map.get(512 + 5));
    }

    #[test]
    fn test_keys_around_bitmap_words_and_pages() {
        for key in [0i64, 1, 63, 64, 65, 127, 128, 510, 511, 512, 513, 1023, 1024, 100_000] {
            let mut map = SparseMonotonicLongMap::new();
            map.add(0, 0).unwrap();
            if key > 0 {
                map.add(key, 100).unwrap();
            }
            map.add(key + 1, 200).unwrap();
            map.add(key + 1_000, 300).unwrap();
            if key > 0 {
                assert_eq!(0, map.get(0), "key {key}");
                assert_eq!(Some(100), map.next(0), "key {key}");
            }
            if key > 1 {
                assert_eq!(-1, map.get(key - 1), "key {key}");
            }
            assert_eq!(if key > 0 { 100 } else { 0 }, map.get(key), "key {key}");
            assert_eq!(Some(200), map.next(key), "key {key}");
            assert_eq!(200, map.get(key + 1), "key {key}");
            assert_eq!(Some(300), map.next(key + 1), "key {key}");
            assert_eq!(-1, map.get(key + 2), "key {key}");
            assert_eq!(-1, map.get(key + 999), "key {key}");
            assert_eq!(300, map.get(key + 1_000), "key {key}");
            assert_eq!(Some(-1), map.next(key + 1_000), "key {key}");
            assert_eq!(-1, map.get(key + 1_001), "key {key}");
        }
    }

    #[test]
    fn test_full_pages() {
        let mut map = SparseMonotonicLongMap::new();
        for i in 0..2048 {
            map.add(i, i * 256).unwrap();
        }
        for i in 0..2048 {
            assert_eq!(i * 256, map.get(i));
            assert_eq!(Some(if i == 2047 { -1 } else { (i + 1) * 256 }), map.next(i));
        }
        assert_eq!(-1, map.get(2048));
    }

    #[test]
    fn test_every_other_key() {
        let mut map = SparseMonotonicLongMap::new();
        for i in (0..2048).step_by(2) {
            map.add(i, i * 10).unwrap();
        }
        for i in 0..2048 {
            assert_eq!(if i % 2 == 0 { i * 10 } else { -1 }, map.get(i));
        }
        for i in (0..2046).step_by(2) {
            assert_eq!(Some((i + 2) * 10), map.next(i));
        }
    }

    #[test]
    fn test_page_ends_where_next_page_starts_after_many_empty_pages() {
        let mut map = SparseMonotonicLongMap::new();
        map.add(511, 1_000_000).unwrap();
        map.add(50_000_000, 1_000_100).unwrap();
        assert_eq!(1_000_000, map.get(511));
        assert_eq!(Some(1_000_100), map.next(511));
        assert_eq!(1_000_100, map.get(50_000_000));
        assert_eq!(Some(-1), map.next(50_000_000));
        assert_eq!(-1, map.get(512));
        assert_eq!(-1, map.get(49_999_999));
    }

    #[test]
    fn test_large_values() {
        let mut map = SparseMonotonicLongMap::new();
        let base = 1i64 << 50;
        map.add(1, base).unwrap();
        map.add(2, base + i32::MAX as i64).unwrap();
        assert_eq!(base, map.get(1));
        assert_eq!(base + i32::MAX as i64, map.get(2));
    }

    #[test]
    fn test_rejects_out_of_order_keys() {
        let mut map = SparseMonotonicLongMap::new();
        map.add(5, 10).unwrap();
        assert!(map.add(5, 20).is_err());
        assert!(map.add(4, 20).is_err());
    }

    #[test]
    fn test_memory_scales_with_keys_added_not_largest_key() {
        // node maps add one key per 256 node IDs, up to the largest node ID today (~14 billion)
        let mut map = SparseMonotonicLongMap::new();
        map.add(1, 0).unwrap();
        map.add(14_000_000_000 / 256, 1).unwrap();
        let bytes = map.estimate_memory_usage_bytes();
        // a dense key of 4 bytes per key would need ~220MB, the page pointers alone ~850kB
        assert!(bytes < 1_000_000, "estimated {bytes} bytes");
    }

    #[test]
    fn test_next_rejects_keys_not_added() {
        let mut map = SparseMonotonicLongMap::new();
        map.add(5, 10).unwrap();
        assert_eq!(None, map.next(4));
        assert_eq!(None, map.next(6));
        assert_eq!(None, map.next(100_000));
    }
}
