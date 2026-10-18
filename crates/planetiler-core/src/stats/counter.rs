//! Port of `stats/Counter.java`: a `long` value that can go up and down, optimized for updates from many threads.
//!
//! Java has `SingleThreadCounter` (one `AtomicLong`) and `MultiThreadCounter` (one counter per thread, summed on
//! read). Here [`Counter`] is a cheaply-cloneable handle to a set of cache-line-padded atomic stripes: each thread
//! updates the stripe assigned to it, and reads add up all stripes. [`Counter::counter_for_thread`] returns a handle
//! that skips the thread-local stripe lookup on every update.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

const STRIPES: usize = 32;

#[repr(align(128))]
#[derive(Default, Debug)]
struct Padded(AtomicI64);

static NEXT_STRIPE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static STRIPE: usize = NEXT_STRIPE.fetch_add(1, Ordering::Relaxed) % STRIPES;
}

/// A counter that can be incremented from many threads and read from any thread. Clones share the same value.
#[derive(Clone, Debug)]
pub struct Counter(Arc<[Padded; STRIPES]>);

impl Default for Counter {
    fn default() -> Self {
        Counter(Arc::new(std::array::from_fn(|_| Padded::default())))
    }
}

impl Counter {
    /// Returns a new counter (Java's `Counter.newMultiThreadCounter()`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a new counter (Java's `Counter.newSingleThreadCounter()`); identical to [`Counter::new`].
    pub fn new_single_thread_counter() -> Self {
        Self::default()
    }

    /// Returns a new counter (Java's `Counter.newMultiThreadCounter()`); identical to [`Counter::new`].
    pub fn new_multi_thread_counter() -> Self {
        Self::default()
    }

    #[inline]
    pub fn inc(&self) {
        self.inc_by(1);
    }

    #[inline]
    pub fn inc_by(&self, value: i64) {
        let stripe = STRIPE.with(|s| *s);
        self.0[stripe].0.fetch_add(value, Ordering::Relaxed);
    }

    /// Returns the sum of all updates from all threads.
    pub fn get(&self) -> i64 {
        self.0.iter().map(|p| p.0.load(Ordering::Relaxed)).sum()
    }

    /// Returns a handle to update this counter that should only be used from the current thread.
    pub fn counter_for_thread(&self) -> ThreadCounter {
        ThreadCounter { counter: self.clone(), stripe: STRIPE.with(|s| *s) }
    }
}

/// A handle for updating a [`Counter`] from one thread without a thread-local lookup on each update.
#[derive(Clone, Debug)]
pub struct ThreadCounter {
    counter: Counter,
    stripe: usize,
}

impl ThreadCounter {
    #[inline]
    pub fn inc(&self) {
        self.inc_by(1);
    }

    #[inline]
    pub fn inc_by(&self, value: i64) {
        self.counter.0[self.stripe].0.fetch_add(value, Ordering::Relaxed);
    }

    /// Returns the total value of the underlying counter across all threads.
    pub fn get(&self) -> i64 {
        self.counter.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_threaded_counter() {
        let counter = Counter::new_single_thread_counter();
        assert_eq!(0, counter.get());
        counter.inc();
        assert_eq!(1, counter.get());
        counter.inc_by(2);
        assert_eq!(3, counter.get());
        counter.inc_by(-1);
        assert_eq!(2, counter.get());
    }

    #[test]
    fn multi_threaded_counter() {
        let counter = Counter::new_multi_thread_counter();
        let c1 = counter.clone();
        let t1 = std::thread::spawn(move || {
            c1.inc_by(1);
            c1.inc_by(1);
        });
        let c2 = counter.clone();
        let t2 = std::thread::spawn(move || c2.inc_by(1));
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(3, counter.get());
    }

    #[test]
    fn multi_threaded_sub_counter() {
        let counter = Counter::new_multi_thread_counter();
        let c1 = counter.clone();
        let t1 = std::thread::spawn(move || {
            let sub = c1.counter_for_thread();
            sub.inc_by(1);
            sub.inc_by(1);
        });
        let c2 = counter.clone();
        let t2 = std::thread::spawn(move || c2.counter_for_thread().inc_by(1));
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(3, counter.get());
    }
}
