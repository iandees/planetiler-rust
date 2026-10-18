//! Port of `worker/Distributor.java`: redistributes work among worker threads when some finish early.
//!
//! When a group of worker threads are processing large blocks, some may finish early, resulting in idle time at the
//! end waiting for the "long pole in the tent" to finish:
//!
//! ```text
//!          busy         idle | done
//! worker1: ===========>xxxxxx|
//! worker2: ===============>xx|
//! worker3: =================>|
//! worker4: =============>xxxx|
//! ```
//!
//! This utility wraps the operation to perform on each element and then works through items in 3 phases:
//!
//! 1. If all threads are still busy, process it in the same thread
//! 2. If some threads are done, enqueue the item onto a work queue (but if it is full, just process it in the same
//!    thread)
//! 3. When the thread is done processing input elements, then process items off of the work queue until it is empty
//!    and all other workers are finished

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

struct Inner<T> {
    done: AtomicI32,
    working: AtomicI32,
    sender: Sender<T>,
    receiver: Receiver<T>,
}

/// Shares work between threads when some finish early. Clones share the same state.
pub struct Distributor<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Clone for Distributor<T> {
    fn clone(&self) -> Self {
        Distributor { inner: self.inner.clone() }
    }
}

impl<T> Distributor<T> {
    /// Returns a new `Distributor` that can hold up to `capacity` pending elements.
    pub fn create_with_capacity(capacity: usize) -> Distributor<T> {
        let (sender, receiver) = crossbeam_channel::bounded(capacity.max(1));
        Distributor { inner: Arc::new(Inner { done: AtomicI32::new(0), working: AtomicI32::new(0), sender, receiver }) }
    }

    /// Returns a handle for a worker thread to offer new items to, which calls `consumer` on each item.
    pub fn for_thread<F: FnMut(T)>(&self, consumer: F) -> ForThread<T, F> {
        self.inner.working.fetch_add(1, Ordering::SeqCst);
        ForThread { inner: self.inner.clone(), consumer, finished: false }
    }
}

/// A handle for each worker thread to offer new items, and drain the remaining ones when done.
///
/// Dropping the handle calls [`ForThread::close`].
pub struct ForThread<T, F: FnMut(T)> {
    inner: Arc<Inner<T>>,
    consumer: F,
    finished: bool,
}

impl<T, F: FnMut(T)> ForThread<T, F> {
    /// Processes `item` in this thread, or hands it off to an idle thread if any have finished.
    ///
    /// # Panics
    /// If called after [`ForThread::finish`].
    pub fn accept(&mut self, item: T) {
        assert!(!self.finished, "Finished");
        if self.inner.done.load(Ordering::SeqCst) == 0 {
            (self.consumer)(item);
        } else if let Err(e) = self.inner.sender.try_send(item) {
            (self.consumer)(e.into_inner());
        }
    }

    /// Marks this thread as done processing its own input.
    pub fn finish(&mut self) {
        if !self.finished {
            self.inner.done.fetch_add(1, Ordering::SeqCst);
            self.inner.working.fetch_sub(1, Ordering::SeqCst);
            self.finished = true;
        }
    }

    /// Processes items handed off from other threads until they are all finished.
    pub fn drain(&mut self) {
        loop {
            let item = match self.inner.receiver.try_recv() {
                Ok(item) => Some(item),
                Err(_) if self.inner.working.load(Ordering::SeqCst) > 0 => {
                    match self.inner.receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(item) => Some(item),
                        Err(RecvTimeoutError::Timeout) => {
                            if self.inner.working.load(Ordering::SeqCst) <= 0 {
                                break;
                            }
                            None
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                Err(_) => break,
            };
            if let Some(item) = item {
                (self.consumer)(item);
            }
        }
    }

    /// Calls [`ForThread::finish`] then [`ForThread::drain`].
    pub fn close(&mut self) {
        self.finish();
        self.drain();
    }

    /// Returns true if [`ForThread::finish`] has been called.
    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

impl<T, F: FnMut(T)> Drop for ForThread<T, F> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.close();
        } else {
            self.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use parking_lot::Mutex;

    use super::*;

    #[test]
    fn empty() {
        let processed = Arc::new(Mutex::new(Vec::<i32>::new()));
        let distributor = Distributor::create_with_capacity(1);
        let p = processed.clone();
        let mut this = distributor.for_thread(move |i| p.lock().push(i));
        this.close();
        this.close();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| this.accept(3)));
        assert!(result.is_err());
        std::mem::forget(this);
        assert!(processed.lock().is_empty());
    }

    #[test]
    fn distributor_1_thread() {
        let processed = Arc::new(Mutex::new(Vec::<i32>::new()));
        let distributor = Distributor::create_with_capacity(1);
        let p = processed.clone();
        let mut this = distributor.for_thread(move |i| p.lock().push(i));
        assert!(processed.lock().is_empty());
        this.accept(1);
        assert_eq!(vec![1], *processed.lock());
        this.accept(2);
        assert_eq!(vec![1, 2], *processed.lock());
        this.close();
        assert!(this.is_finished());
        assert_eq!(vec![1, 2], *processed.lock());
    }

    #[test]
    fn distributor_2_threads() {
        let processed = Arc::new(Mutex::new(Vec::<i32>::new()));
        let distributor = Distributor::create_with_capacity(1);
        // latches a-f from the Java test, emulated with barriers between the main thread and worker threads
        let ab = Arc::new(Barrier::new(2)); // thread1 -> thread2: "a"
        let b = Arc::new(Barrier::new(2)); // thread2 -> main: "b"
        let c = Arc::new(Barrier::new(2)); // main -> thread1: "c"
        let d = Arc::new(Barrier::new(2)); // thread2 -> thread1: "d"
        let e = Arc::new(Barrier::new(2)); // thread1 -> main: "e"
        let f = Arc::new(Barrier::new(3)); // main -> both: "f"

        let thread1 = {
            let (distributor, processed, ab, c, d, e, f) =
                (distributor.clone(), processed.clone(), ab.clone(), c.clone(), d.clone(), e.clone(), f.clone());
            std::thread::spawn(move || {
                let p = processed.clone();
                let mut this = distributor.for_thread(move |i| p.lock().push(i));
                this.accept(1);
                this.accept(2);
                ab.wait();
                c.wait();
                d.wait();
                this.accept(5);
                this.accept(6); // queue full
                e.wait();
                f.wait();
                this.close();
            })
        };
        let thread2 = {
            let (distributor, processed, ab, b, d, f) =
                (distributor.clone(), processed.clone(), ab.clone(), b.clone(), d.clone(), f.clone());
            std::thread::spawn(move || {
                let p = processed.clone();
                let mut this = distributor.for_thread(move |i| p.lock().push(i));
                ab.wait();
                this.accept(3);
                this.accept(4);
                b.wait();
                this.finish();
                d.wait();
                f.wait();
                this.drain();
            })
        };
        b.wait();
        assert_eq!(vec![1, 2, 3, 4], *processed.lock());
        c.wait();
        e.wait();
        assert_eq!(vec![1, 2, 3, 4, 6], *processed.lock());
        f.wait();
        thread1.join().unwrap();
        thread2.join().unwrap();
        assert_eq!(vec![1, 2, 3, 4, 6, 5], *processed.lock());
    }
}
