//! Port of `util/CompletableFutures.java`: utilities for waiting on groups of asynchronous tasks.
//!
//! Java's `CompletableFuture<Void>` is [`Done`](crate::worker::Done) here, tasks run on plain threads, and `await` is
//! [`Done::wait`](crate::worker::Done::wait). This module has the remaining helpers: [`in_completion_order`],
//! [`join_all_or_close`] and a counting [`Semaphore`].

use std::sync::Arc;

use anyhow::Result;
use parking_lot::{Condvar, Mutex};

use crate::worker::Done;

/// A counting semaphore: [`Semaphore::acquire`] blocks until a permit is available.
#[derive(Debug)]
pub struct Semaphore {
    permits: Mutex<usize>,
    available: Condvar,
}

impl Semaphore {
    pub fn new(permits: usize) -> Semaphore {
        Semaphore { permits: Mutex::new(permits), available: Condvar::new() }
    }

    /// Waits for a permit.
    pub fn acquire(&self) {
        let mut permits = self.permits.lock();
        while *permits == 0 {
            self.available.wait(&mut permits);
        }
        *permits -= 1;
    }

    /// Returns a permit.
    pub fn release(&self) {
        *self.permits.lock() += 1;
        self.available.notify_one();
    }

    /// Waits for a permit, which gets returned when the result is dropped.
    pub fn acquire_owned(self: &Arc<Self>) -> Permit {
        self.acquire();
        Permit(self.clone())
    }
}

/// A permit from a [`Semaphore`] that returns itself when dropped.
#[derive(Debug)]
pub struct Permit(Arc<Semaphore>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Passes `items` to `next` in the order they become ready instead of the order they come in.
///
/// `start` gets called on each item in order and returns a [`Done`] that completes, normally or exceptionally, once
/// the item is ready. At most `window` items are started but not passed to `next` yet at once. Items that are ready
/// right away get passed on in order.
///
/// # Panics
///
/// If `window` is 0.
pub fn in_completion_order<T: Send + 'static>(
    items: impl IntoIterator<Item = T>,
    window: usize,
    mut start: impl FnMut(&T) -> Done,
    mut next: impl FnMut(T),
) {
    assert!(window >= 1, "window must be at least 1, was {window}");
    let (ready_tx, ready_rx) = crossbeam_channel::unbounded::<T>();
    let mut start_item = |item: T| {
        let done = start(&item);
        let tx = ready_tx.clone();
        done.on_complete(move |_| {
            let _ = tx.send(item);
        });
    };
    let mut items = items.into_iter();
    let mut in_progress = 0;
    // initially, start up to `window` futures
    while in_progress < window {
        let Some(item) = items.next() else { break };
        start_item(item);
        in_progress += 1;
    }
    // then each time any one finishes, pass it on and start the next one
    for item in items {
        next(ready_rx.recv().expect("sender kept alive"));
        start_item(item);
    }
    // then after all started, drain the queue
    while in_progress > 0 {
        next(ready_rx.recv().expect("sender kept alive"));
        in_progress -= 1;
    }
}

/// Returns the values of `results`, which each open a resource, in the same order.
///
/// If any failed, this closes the ones that opened (with `close`) and returns the first failure in list order. A
/// failure to close one is logged.
pub fn join_all_or_close<T>(results: Vec<Result<T>>, close: impl Fn(T) -> Result<()>) -> Result<Vec<T>> {
    let mut opened = Vec::with_capacity(results.len());
    let mut error = None;
    for result in results {
        match result {
            Ok(value) => opened.push(value),
            Err(e) => {
                error.get_or_insert(e);
            }
        }
    }
    match error {
        None => Ok(opened),
        Some(error) => {
            for resource in opened {
                if let Err(e) = close(resource) {
                    tracing::warn!("Error closing resource: {e:#}");
                }
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use anyhow::anyhow;

    use super::*;

    #[test]
    fn join_all_or_close_returns_results_in_order() {
        let closed = AtomicBool::new(false);
        let result = join_all_or_close(vec![Ok("a"), Ok("b")], |_| {
            closed.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        assert_eq!(vec!["a", "b"], result);
        assert!(!closed.load(Ordering::SeqCst));
    }

    #[test]
    fn join_all_or_close_closes_opened_on_failure() {
        let closed = Mutex::new(vec![]);
        let error =
            join_all_or_close(vec![Ok("a"), Err(anyhow!("first")), Ok("c"), Err(anyhow!("second"))], |resource| {
                closed.lock().push(resource);
                if resource == "c" { Err(anyhow!("close c")) } else { Ok(()) }
            })
            .unwrap_err();
        assert_eq!("first", error.to_string(), "the first failure in list order");
        assert_eq!(vec!["a", "c"], *closed.lock());
    }

    #[test]
    fn in_completion_order_passes_items_on_as_they_become_ready() {
        let futures = [Done::new(), Done::new(), Done::new()];
        let mut passed = vec![];
        in_completion_order(
            0..3,
            3,
            |&i| {
                if i == 2 {
                    futures[1].fail(anyhow!("failed items get passed on too"));
                    futures[0].complete(Ok(()));
                    futures[2].complete(Ok(()));
                }
                futures[i].clone()
            },
            |i| passed.push(i),
        );
        assert_eq!(vec![1, 0, 2], passed);
    }

    #[test]
    fn in_completion_order_starts_at_most_window_items_ahead() {
        let events = Mutex::new(vec![]);
        in_completion_order(
            0..4,
            2,
            |i| {
                events.lock().push(format!("start {i}"));
                Done::completed()
            },
            |i| events.lock().push(format!("next {i}")),
        );
        assert_eq!(
            vec!["start 0", "start 1", "next 0", "start 2", "next 1", "start 3", "next 2", "next 3"],
            *events.lock()
        );
    }

    #[test]
    #[should_panic(expected = "window must be at least 1")]
    fn in_completion_order_rejects_empty_window() {
        in_completion_order([1], 0, |_| Done::completed(), |_| {});
    }

    #[test]
    fn semaphore_blocks_until_released() {
        let semaphore = Arc::new(Semaphore::new(1));
        let first = semaphore.acquire_owned();
        let other = semaphore.clone();
        let handle = std::thread::spawn(move || {
            let _permit = other.acquire_owned();
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!handle.is_finished());
        drop(first);
        handle.join().unwrap();
    }
}
