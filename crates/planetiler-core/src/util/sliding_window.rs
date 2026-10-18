//! Port of `util/SlidingWindow.java`: lets multiple threads work on a sliding window of values.
//!
//! Calls to [`SlidingWindow::wait_until_inside_window`] block until [`SlidingWindow::advance_tail`] is within a given
//! range from the new value being requested.

use parking_lot::{Condvar, Mutex};

/// A window of `window_size` values starting at a tail that only moves forward.
pub struct SlidingWindow {
    tail: Mutex<i64>,
    cond: Condvar,
    window_size: i64,
}

impl SlidingWindow {
    pub fn new(window_size: i64) -> SlidingWindow {
        SlidingWindow { tail: Mutex::new(0), cond: Condvar::new(), window_size }
    }

    /// Moves the current value for the tail to `to`, unblocking any thread waiting on moving the head to
    /// `to + window_size`.
    ///
    /// # Panics
    ///
    /// If `to` is less than the current tail.
    pub fn advance_tail(&self, to: i64) {
        let mut tail = self.tail.lock();
        if to < *tail {
            panic!("Tried to move sliding window tail backwards from {} to {to}", *tail);
        }
        *tail = to;
        self.cond.notify_all();
    }

    /// Blocks until another thread moves the tail to at least `to - window_size`.
    pub fn wait_until_inside_window(&self, to: i64) {
        let mut tail = self.tail.lock();
        while to - *tail >= self.window_size {
            self.cond.wait(&mut tail);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn test_sliding_window() {
        let sliding_window = Arc::new(SlidingWindow::new(5));
        let (tx1, rx1) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        let sw = sliding_window.clone();
        let t1 = std::thread::spawn(move || {
            tx1.send(()).unwrap();
            sw.wait_until_inside_window(9);
            tx2.send(()).unwrap();
        });
        rx1.recv().unwrap();
        assert!(rx2.recv_timeout(Duration::from_millis(100)).is_err());
        sliding_window.advance_tail(4);
        assert!(rx2.recv_timeout(Duration::from_millis(100)).is_err());
        sliding_window.advance_tail(5);
        rx2.recv_timeout(Duration::from_secs(10)).unwrap();
        t1.join().unwrap();
    }
}
