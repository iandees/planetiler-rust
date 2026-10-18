//! Port of `util/CacheByZoom.java`: caches a value that changes by integer zoom level to avoid recomputing it.
//!
//! Like the Java version this is not thread-safe (it is meant to be used by a single feature on a single thread) and
//! covers zoom levels `0..=MAX_MAXZOOM`.
//!
//! Because a Rust struct can't easily store a closure that borrows its owner, [`CacheByZoom`] does not own its
//! supplier: call [`CacheByZoom::get_or_init`] with the function to compute the value. [`CacheByZoomFn`] bundles a
//! supplier for the common case where the function is self-contained (Java's `CacheByZoom.create(fn)`).

use std::cell::OnceCell;

use crate::config::planetiler_config::MAX_MAXZOOM;

/// Per-zoom cache of values.
#[derive(Debug)]
pub struct CacheByZoom<T> {
    values: Vec<OnceCell<T>>,
}

impl<T> Default for CacheByZoom<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> CacheByZoom<T> {
    /// Returns an empty cache for zoom levels `0..=MAX_MAXZOOM`.
    pub fn new() -> CacheByZoom<T> {
        CacheByZoom { values: (0..=MAX_MAXZOOM).map(|_| OnceCell::new()).collect() }
    }

    /// Returns a cache that will call `supplier` with each zoom level the first time it is requested.
    pub fn create<F: Fn(u32) -> T>(supplier: F) -> CacheByZoomFn<T, F> {
        CacheByZoomFn { cache: CacheByZoom::new(), supplier }
    }

    /// Returns the cached value for `zoom`, computing it with `supplier` if it hasn't been computed yet.
    pub fn get_or_init(&self, zoom: u32, supplier: impl FnOnce(u32) -> T) -> &T {
        self.values[zoom as usize].get_or_init(|| supplier(zoom))
    }

    /// Returns the cached value for `zoom` if present.
    pub fn get(&self, zoom: u32) -> Option<&T> {
        self.values.get(zoom as usize).and_then(OnceCell::get)
    }
}

/// A [`CacheByZoom`] bundled with the function that computes each value.
pub struct CacheByZoomFn<T, F: Fn(u32) -> T> {
    cache: CacheByZoom<T>,
    supplier: F,
}

impl<T, F: Fn(u32) -> T> CacheByZoomFn<T, F> {
    /// Returns the value at `zoom`, computing it on first access.
    pub fn get(&self, zoom: u32) -> &T {
        self.cache.get_or_init(zoom, &self.supplier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn test_cache_zoom() {
        let calls = RefCell::new(Vec::new());
        let cached = CacheByZoom::create(|i| {
            calls.borrow_mut().push(i);
            i + 1
        });
        assert_eq!(3, *cached.get(2));
        assert_eq!(3, *cached.get(2));
        assert_eq!(6, *cached.get(5));
        assert_eq!(vec![2, 5], *calls.borrow());
    }

    #[test]
    fn test_get_or_init() {
        let cache = CacheByZoom::new();
        assert_eq!(None, cache.get(1));
        assert_eq!(2, *cache.get_or_init(1, |z| z * 2));
        assert_eq!(2, *cache.get_or_init(1, |_| 100));
        assert_eq!(Some(&2), cache.get(1));
    }
}
