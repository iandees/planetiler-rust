//! Port of `stats/Counter.java`.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// A counter that can be incremented from many threads and read from any thread.
///
/// Java uses per-thread counters to avoid contention; here a relaxed atomic is used, and hot loops should accumulate
/// locally and call [`Counter::inc_by`] in batches.
#[derive(Clone, Debug, Default)]
pub struct Counter(Arc<AtomicI64>);

impl Counter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn inc(&self) {
        self.inc_by(1);
    }

    pub fn inc_by(&self, value: i64) {
        self.0.fetch_add(value, Ordering::Relaxed);
    }

    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}
