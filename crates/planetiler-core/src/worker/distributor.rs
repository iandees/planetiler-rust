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
//!
//! Difference from Java: a draining thread waits on the queue in 100ms polls like Java, but the worker whose
//! [`ForThread::finish`] brings the number of working threads to zero also wakes every draining thread, so they
//! exit as soon as the work runs out instead of up to 100ms later.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};

/// How long a draining thread waits for an item before re-checking whether all workers are done (Java's
/// `pending.poll(100, TimeUnit.MILLISECONDS)`).
const POLL_INTERVAL: Duration = Duration::from_millis(100);

struct Inner<T> {
    done: AtomicI32,
    working: AtomicI32,
    sender: Sender<T>,
    receiver: Receiver<T>,
    /// One token per finished thread is sent when the last working thread finishes, to wake up draining threads.
    wake_sender: Sender<()>,
    wake_receiver: Receiver<()>,
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
        let (wake_sender, wake_receiver) = crossbeam_channel::unbounded();
        Distributor {
            inner: Arc::new(Inner {
                done: AtomicI32::new(0),
                working: AtomicI32::new(0),
                sender,
                receiver,
                wake_sender,
                wake_receiver,
            }),
        }
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
            let done = self.inner.done.fetch_add(1, Ordering::SeqCst) + 1;
            let working = self.inner.working.fetch_sub(1, Ordering::SeqCst) - 1;
            self.finished = true;
            if working <= 0 {
                // wake up threads waiting in drain(); a stale token left over only causes one extra re-check
                for _ in 0..done {
                    let _ = self.inner.wake_sender.send(());
                }
            }
        }
    }

    /// Processes items handed off from other threads until they are all finished.
    pub fn drain(&mut self) {
        let inner = &*self.inner;
        loop {
            if let Ok(item) = inner.receiver.try_recv() {
                (self.consumer)(item);
                continue;
            }
            // nothing is enqueued after the last worker finishes, so once the queue is empty we are done
            if inner.working.load(Ordering::SeqCst) <= 0 {
                break;
            }
            crossbeam_channel::select! {
                recv(inner.receiver) -> item => match item {
                    Ok(item) => (self.consumer)(item),
                    Err(_) => break,
                },
                recv(inner.wake_receiver) -> _ => {}
                default(POLL_INTERVAL) => {}
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

    /// A draining thread returns as soon as the last worker finishes, rather than after the next 100ms poll.
    #[test]
    fn drain_wakes_up_when_last_worker_finishes() {
        let distributor = Distributor::<i32>::create_with_capacity(10);
        let mut worker = distributor.for_thread(|_| {});
        let started = Arc::new(Barrier::new(2));
        let drainer = {
            let (distributor, started) = (distributor.clone(), started.clone());
            std::thread::spawn(move || {
                let mut this = distributor.for_thread(|_| {});
                this.finish();
                started.wait();
                let start = std::time::Instant::now();
                this.drain();
                start.elapsed()
            })
        };
        started.wait();
        std::thread::sleep(Duration::from_millis(30));
        worker.close();
        let waited = drainer.join().unwrap();
        assert!(waited >= Duration::from_millis(25) && waited < Duration::from_millis(90), "{waited:?}");
    }

    /// Every item is processed exactly once when workers with very different amounts of input hand work off to each
    /// other, and threads that finish early process some of the work of the busy one.
    #[test]
    fn uneven_workers_process_every_item_once() {
        let distributor = Distributor::<u32>::create_with_capacity(16);
        let threads = 6;
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let distributor = distributor.clone();
                std::thread::spawn(move || {
                    let mut seen = vec![];
                    let mut this = distributor.for_thread(|i: u32| {
                        seen.push(i);
                        std::hint::black_box((0..200).sum::<u32>());
                    });
                    let n = if t == 0 { 200_000 } else { 10 };
                    for i in 0..n {
                        this.accept(t << 24 | i);
                    }
                    this.close();
                    drop(this);
                    seen
                })
            })
            .collect();
        let results: Vec<Vec<u32>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let mut all: Vec<u32> = results.iter().flatten().copied().collect();
        all.sort_unstable();
        let mut expected: Vec<u32> = (0..200_000).collect();
        for t in 1..threads {
            expected.extend((0..10).map(|i| t << 24 | i));
        }
        expected.sort_unstable();
        assert_eq!(expected, all);
        let helped = results[1..].iter().map(|r| r.iter().filter(|i| *i >> 24 == 0).count()).sum::<usize>();
        assert!(helped > 0, "idle threads did not help");
    }
}
