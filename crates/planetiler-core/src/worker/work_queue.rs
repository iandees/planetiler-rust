//! Port of `worker/WorkQueue.java`: a high-performance blocking queue to hand off work from producing threads to
//! consuming threads.
//!
//! Wraps a bounded `crossbeam_channel` with a few customizations:
//! - items are buffered into configurable-sized batches before putting them on the channel to reduce contention
//! - writers mark the queue "finished" with [`WorkQueue::close`] and readers get `None` when there are no more items
//!
//! Each thread should get its own [`WorkQueueWriter`] / [`WorkQueueReader`] through
//! [`WorkQueue::thread_local_writer`] / [`WorkQueue::thread_local_reader`] (or use [`WorkQueue::accept`] /
//! [`WorkQueue::get`] which look one up from a thread-local). Once a thread starts reading from this queue, it needs to
//! finish, otherwise not all items might be read.
//!
//! Since Rust threads can't be interrupted, [`WorkQueue::abort`] is provided for error handling: it makes readers
//! return `None` and writers discard items without blocking so that threads in a failed pipeline can exit.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender, TrySendError};
use parking_lot::{Mutex, RwLock};

use crate::stats::counter::{Counter, ThreadCounter};
use crate::stats::stats::StatsRef;

const POLL_INTERVAL: Duration = Duration::from_millis(100);

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct Inner<T> {
    id: u64,
    sender: RwLock<Option<Sender<Vec<T>>>>,
    receiver: Receiver<Vec<T>>,
    batch_size: usize,
    pending_batches_capacity: usize,
    writers: Mutex<Vec<Arc<Mutex<Vec<T>>>>>,
    num_readers: AtomicUsize,
    pending_count: Counter,
    enqueue_count: Counter,
    enqueue_block_time_nanos: Counter,
    dequeue_count: Counter,
    dequeue_block_time_nanos: Counter,
    aborted: AtomicBool,
}

impl<T> Inner<T> {
    fn get_pending(&self) -> usize {
        self.pending_count.get().max(0) as usize
    }

    fn get_capacity(&self) -> usize {
        (self.pending_batches_capacity + self.writers.lock().len() + self.num_readers.load(Ordering::Relaxed))
            * self.batch_size
    }

    fn flush(&self, batch: Vec<T>, block_time: &ThreadCounter) {
        if batch.is_empty() || self.aborted.load(Ordering::Relaxed) {
            return;
        }
        let sender = self.sender.read();
        let Some(sender) = sender.as_ref() else {
            tracing::debug!("Discarding {} items written to closed queue", batch.len());
            return;
        };
        match sender.try_send(batch) {
            Ok(()) => {}
            Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(mut batch)) => {
                let start = Instant::now();
                loop {
                    match sender.send_timeout(batch, POLL_INTERVAL) {
                        Ok(()) | Err(SendTimeoutError::Disconnected(_)) => break,
                        Err(SendTimeoutError::Timeout(b)) => {
                            if self.aborted.load(Ordering::Relaxed) {
                                break;
                            }
                            batch = b;
                        }
                    }
                }
                block_time.inc_by(start.elapsed().as_nanos() as i64);
            }
        }
    }
}

/// A queue that hands off items of type `T` between threads in batches. Clones share the same queue.
pub struct WorkQueue<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Clone for WorkQueue<T> {
    fn clone(&self) -> Self {
        WorkQueue { inner: self.inner.clone() }
    }
}

impl<T> std::fmt::Debug for WorkQueue<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkQueue")
            .field("pending", &self.inner.get_pending())
            .field("capacity", &self.inner.get_capacity())
            .finish()
    }
}

thread_local! {
    static THREAD_LOCAL_WRITERS: RefCell<HashMap<u64, Box<dyn Any>>> = RefCell::new(HashMap::new());
    static THREAD_LOCAL_READERS: RefCell<HashMap<u64, Box<dyn Any>>> = RefCell::new(HashMap::new());
}

impl<T: Send + 'static> WorkQueue<T> {
    /// Creates a new queue.
    ///
    /// - `name`: ID to prepend to stats generated about this queue
    /// - `capacity`: maximum number of pending items that can be held in the queue
    /// - `max_batch`: batch size to buffer elements into before handing off to the blocking queue
    pub fn new(name: &str, capacity: usize, max_batch: usize, stats: &StatsRef) -> WorkQueue<T> {
        let max_batch = max_batch.max(1);
        let pending_batches_capacity = (capacity / max_batch).max(1);
        let (sender, receiver) = crossbeam_channel::bounded(pending_batches_capacity);
        let inner = Arc::new(Inner {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            sender: RwLock::new(Some(sender)),
            receiver,
            batch_size: max_batch,
            pending_batches_capacity,
            writers: Mutex::new(Vec::new()),
            num_readers: AtomicUsize::new(0),
            pending_count: Counter::new(),
            enqueue_count: stats.long_counter(&format!("{name}_enqueue_count")),
            enqueue_block_time_nanos: stats.nano_counter(&format!("{name}_enqueue_block_time_seconds")),
            dequeue_count: stats.long_counter(&format!("{name}_dequeue_count")),
            dequeue_block_time_nanos: stats.nano_counter(&format!("{name}_dequeue_block_time_seconds")),
            aborted: AtomicBool::new(false),
        });
        let gauge = |suffix: &str, f: fn(&Inner<T>) -> f64| {
            let weak: Weak<Inner<T>> = Arc::downgrade(&inner);
            stats.gauge(&format!("{name}_{suffix}"), Arc::new(move || weak.upgrade().map_or(0.0, |i| f(&i))));
        };
        gauge("blocking_queue_capacity", |i| i.pending_batches_capacity as f64);
        gauge("blocking_queue_size", |i| i.receiver.len() as f64);
        gauge("capacity", |i| i.get_capacity() as f64);
        gauge("size", |i| i.get_pending() as f64);
        WorkQueue { inner }
    }

    /// Returns a writer optimized to accept items from a single thread.
    pub fn thread_local_writer(&self) -> WorkQueueWriter<T> {
        let batch = Arc::new(Mutex::new(Vec::with_capacity(self.inner.batch_size)));
        self.inner.writers.lock().push(batch.clone());
        WorkQueueWriter {
            pending_count: self.inner.pending_count.counter_for_thread(),
            enqueue_count: self.inner.enqueue_count.counter_for_thread(),
            enqueue_block_time_nanos: self.inner.enqueue_block_time_nanos.counter_for_thread(),
            queue: self.inner.clone(),
            batch,
        }
    }

    /// Returns a reader optimized to produce items for a single thread.
    pub fn thread_local_reader(&self) -> WorkQueueReader<T> {
        self.inner.num_readers.fetch_add(1, Ordering::Relaxed);
        WorkQueueReader {
            pending_count: self.inner.pending_count.counter_for_thread(),
            dequeue_count: self.inner.dequeue_count.counter_for_thread(),
            dequeue_block_time_nanos: self.inner.dequeue_block_time_nanos.counter_for_thread(),
            queue: self.inner.clone(),
            batch: Vec::new().into_iter(),
        }
    }

    /// Adds an item to the queue using a writer cached for the current thread.
    pub fn accept(&self, item: T) {
        let id = self.inner.id;
        THREAD_LOCAL_WRITERS.with(|w| {
            let mut writers = w.borrow_mut();
            let writer = writers.entry(id).or_insert_with(|| Box::new(self.thread_local_writer()));
            writer.downcast_mut::<WorkQueueWriter<T>>().expect("writer type").accept(item);
        });
    }

    /// Gets the next item from the queue using a reader cached for the current thread, or `None` when the queue is
    /// closed and empty.
    pub fn get(&self) -> Option<T> {
        let id = self.inner.id;
        let mut reader = THREAD_LOCAL_READERS.with(|r| r.borrow_mut().remove(&id));
        let reader_ref = reader.get_or_insert_with(|| Box::new(self.thread_local_reader()));
        let result = reader_ref.downcast_mut::<WorkQueueReader<T>>().expect("reader type").get();
        THREAD_LOCAL_READERS.with(|r| r.borrow_mut().insert(id, reader.unwrap()));
        result
    }
}

impl<T> WorkQueue<T> {
    /// Flushes items buffered by every writer and marks the queue as finished so readers return `None` once it is
    /// empty.
    pub fn close(&self) {
        let writers = self.inner.writers.lock().clone();
        let block = self.inner.enqueue_block_time_nanos.counter_for_thread();
        for writer in writers {
            let batch = std::mem::take(&mut *writer.lock());
            self.inner.flush(batch, &block);
        }
        self.inner.sender.write().take();
    }

    /// Makes readers return `None` immediately and writers discard items without blocking, so that threads using this
    /// queue in a failed pipeline can exit.
    pub fn abort(&self) {
        self.inner.aborted.store(true, Ordering::Relaxed);
        self.inner.sender.write().take();
    }

    /// Returns true if [`WorkQueue::abort`] was called.
    pub fn is_aborted(&self) -> bool {
        self.inner.aborted.load(Ordering::Relaxed)
    }

    /// Returns the number of enqueued items that have not been dequeued yet.
    ///
    /// This may be larger than the initial capacity because each writer thread can buffer items into a batch and each
    /// reader thread might be reading items from a batch.
    pub fn get_pending(&self) -> usize {
        self.inner.get_pending()
    }

    /// Returns the total number of items that can be pending, including batches held by each reader and writer.
    pub fn get_capacity(&self) -> usize {
        self.inner.get_capacity()
    }
}

/// Type-erased view of a [`WorkQueue`] for logging and lifecycle management.
pub trait QueueHandle: Send + Sync {
    fn get_pending(&self) -> usize;
    fn get_capacity(&self) -> usize;
    fn close(&self);
    fn abort(&self);
}

impl<T: Send> QueueHandle for WorkQueue<T> {
    fn get_pending(&self) -> usize {
        WorkQueue::get_pending(self)
    }

    fn get_capacity(&self) -> usize {
        WorkQueue::get_capacity(self)
    }

    fn close(&self) {
        WorkQueue::close(self)
    }

    fn abort(&self) {
        WorkQueue::abort(self)
    }
}

/// Accepts items into a [`WorkQueue`] from a single thread. Remaining buffered items are flushed when dropped.
pub struct WorkQueueWriter<T> {
    queue: Arc<Inner<T>>,
    batch: Arc<Mutex<Vec<T>>>,
    pending_count: ThreadCounter,
    enqueue_count: ThreadCounter,
    enqueue_block_time_nanos: ThreadCounter,
}

impl<T> WorkQueueWriter<T> {
    /// Adds `item` to the queue, blocking if the queue is full.
    pub fn accept(&mut self, item: T) {
        let full = {
            let mut batch = self.batch.lock();
            batch.push(item);
            if batch.len() >= self.queue.batch_size {
                Some(std::mem::replace(&mut *batch, Vec::with_capacity(self.queue.batch_size)))
            } else {
                None
            }
        };
        self.pending_count.inc();
        if let Some(full) = full {
            self.queue.flush(full, &self.enqueue_block_time_nanos);
        }
        self.enqueue_count.inc();
    }

    /// Hands off any buffered items to readers.
    pub fn flush(&mut self) {
        let batch = std::mem::take(&mut *self.batch.lock());
        self.queue.flush(batch, &self.enqueue_block_time_nanos);
    }
}

impl<T> Drop for WorkQueueWriter<T> {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Reads items from a [`WorkQueue`] in a single thread. Also an [`Iterator`] over the remaining items.
pub struct WorkQueueReader<T> {
    queue: Arc<Inner<T>>,
    batch: std::vec::IntoIter<T>,
    pending_count: ThreadCounter,
    dequeue_count: ThreadCounter,
    dequeue_block_time_nanos: ThreadCounter,
}

impl<T> WorkQueueReader<T> {
    /// Returns the next item, blocking until one is available, or `None` when the queue is closed and empty (or
    /// aborted).
    pub fn get(&mut self) -> Option<T> {
        let mut result = self.batch.next();
        if result.is_none() {
            let start = Instant::now();
            loop {
                if self.queue.aborted.load(Ordering::Relaxed) {
                    break;
                }
                match self.queue.receiver.recv_timeout(POLL_INTERVAL) {
                    Ok(batch) => {
                        self.batch = batch.into_iter();
                        result = self.batch.next();
                        if result.is_some() {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            self.dequeue_block_time_nanos.inc_by(start.elapsed().as_nanos() as i64);
        }
        if result.is_some() {
            self.pending_count.inc_by(-1);
        }
        self.dequeue_count.inc();
        result
    }
}

impl<T> Iterator for WorkQueueReader<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.get()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::stats::stats::InMemoryStats;
    use crate::worker::worker::Worker;

    fn new_queue<T: Send + 'static>(max_batch: usize) -> WorkQueue<T> {
        WorkQueue::new("queue", 1000, max_batch, &InMemoryStats::arc())
    }

    #[test]
    fn empty() {
        let q = new_queue::<String>(1);
        q.close();
        assert_eq!(None, q.get());
    }

    #[test]
    fn one_item() {
        let q = new_queue(1);
        q.accept("a");
        q.close();
        assert_eq!(Some("a"), q.get());
        assert_eq!(None, q.get());
    }

    #[test]
    fn more_items_than_batch_size() {
        let q = new_queue(2);
        q.accept("a");
        q.accept("b");
        q.accept("c");
        q.close();
        assert_eq!(Some("a"), q.get());
        assert_eq!(Some("b"), q.get());
        assert_eq!(Some("c"), q.get());
        assert_eq!(None, q.get());
    }

    #[test]
    fn many_items() {
        let q = new_queue(100);
        for i in 0..950 {
            q.accept(i);
        }
        q.close();
        for i in 0..950 {
            assert_eq!(Some(i), q.get());
        }
        assert_eq!(None, q.get());
    }

    #[test]
    fn two_writers() {
        let q = new_queue::<usize>(2);
        let q2 = q.clone();
        Worker::new("worker", InMemoryStats::arc(), 2, move |i| {
            q2.accept(i);
            Ok(())
        })
        .await_done()
        .unwrap();
        q.close();
        assert_eq!(2, q.get_pending());
        let mut found = BTreeSet::new();
        for _ in 0..2 {
            found.insert(q.get().unwrap());
        }
        assert_eq!(None, q.get());
        assert_eq!(BTreeSet::from([0, 1]), found);
        assert_eq!(0, q.get_pending());
    }

    #[test]
    fn two_writers_many_elements() {
        let q = new_queue::<usize>(2);
        let q2 = q.clone();
        Worker::new("worker", InMemoryStats::arc(), 2, move |i| {
            q2.accept(i * 3);
            q2.accept(i * 3 + 1);
            q2.accept(i * 3 + 2);
            Ok(())
        })
        .await_done()
        .unwrap();
        q.close();
        assert_eq!(6, q.get_pending());
        let found: BTreeSet<usize> = (0..6).map(|_| q.get().unwrap()).collect();
        assert_eq!(None, q.get());
        assert_eq!((0..6).collect::<BTreeSet<_>>(), found);
        assert_eq!(0, q.get_pending());
    }

    #[test]
    fn reader_iterator_and_abort() {
        let q = new_queue::<i32>(1);
        let mut writer = q.thread_local_writer();
        writer.accept(1);
        writer.accept(2);
        drop(writer);
        q.close();
        assert_eq!(vec![1, 2], q.thread_local_reader().collect::<Vec<_>>());

        let q = WorkQueue::<i32>::new("q", 1, 1, &InMemoryStats::arc());
        let mut writer = q.thread_local_writer();
        writer.accept(1);
        let q2 = q.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            q2.abort();
        });
        // would block forever without abort
        writer.accept(2);
        t.join().unwrap();
        assert_eq!(None, q.thread_local_reader().get());
    }

    #[test]
    fn capacity() {
        let q = WorkQueue::<i32>::new("q", 10, 1, &InMemoryStats::arc());
        assert_eq!(10, q.get_capacity());
        let _w = q.thread_local_writer();
        let _r = q.thread_local_reader();
        assert_eq!(12, q.get_capacity());
    }
}
