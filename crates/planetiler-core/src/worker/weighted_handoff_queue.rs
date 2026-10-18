//! Port of `worker/WeightedHandoffQueue.java`: a high-performance blocking queue to hand off work from a single
//! producing thread to a single consuming thread.
//!
//! Each element has a weight and each batch has a target maximum weight to put more lightweight objects in a batch or
//! fewer heavy-weight ones.

use std::collections::VecDeque;

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;

struct WriteState<T> {
    batch: Vec<T>,
    cost: usize,
}

struct ReadState<T> {
    batch: VecDeque<T>,
    done: bool,
}

/// A single-producer single-consumer queue that batches items by weight.
///
/// Methods take `&self` so the queue can be shared (e.g. in an `Arc`) between the producing and consuming thread.
pub struct WeightedHandoffQueue<T> {
    sender: Sender<Option<Vec<T>>>,
    receiver: Receiver<Option<Vec<T>>>,
    write_limit: usize,
    write: Mutex<WriteState<T>>,
    read: Mutex<ReadState<T>>,
}

impl<T> WeightedHandoffQueue<T> {
    /// Creates a new queue with `outer` maximum number of pending batches and `inner` maximum batch weight.
    pub fn new(outer: usize, inner: usize) -> Self {
        let (sender, receiver) = crossbeam_channel::bounded(outer.max(1));
        WeightedHandoffQueue {
            sender,
            receiver,
            write_limit: inner,
            write: Mutex::new(WriteState { batch: Vec::new(), cost: 0 }),
            read: Mutex::new(ReadState { batch: VecDeque::new(), done: false }),
        }
    }

    /// Flushes pending items and signals to the reader that there are no more items.
    pub fn close(&self) {
        self.flush_writes(&mut self.write.lock());
        let _ = self.sender.send(None);
    }

    /// Adds `item` with weight `cost` to the queue, blocking if the queue is full.
    pub fn accept(&self, item: T, cost: usize) {
        let mut write = self.write.lock();
        if write.batch.capacity() == 0 {
            write.batch.reserve(self.write_limit / 2);
        }
        write.cost += cost;
        write.batch.push(item);
        if write.cost >= self.write_limit {
            self.flush_writes(&mut write);
        }
    }

    fn flush_writes(&self, write: &mut WriteState<T>) {
        if !write.batch.is_empty() {
            let batch = std::mem::take(&mut write.batch);
            write.cost = 0;
            let _ = self.sender.send(Some(batch));
        }
    }

    /// Returns the next item, blocking until one is available, or `None` after the queue is closed and empty.
    pub fn get(&self) -> Option<T> {
        let mut read = self.read.lock();
        if read.batch.is_empty() {
            while !read.done {
                match self.receiver.recv() {
                    Ok(Some(batch)) => {
                        if !batch.is_empty() {
                            read.batch = batch.into();
                            break;
                        }
                    }
                    Ok(None) | Err(_) => read.done = true,
                }
            }
        }
        read.batch.pop_front()
    }
}

impl<T> Iterator for &WeightedHandoffQueue<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        let q = WeightedHandoffQueue::<String>::new(1, 1);
        q.close();
        assert_eq!(None, q.get());
    }

    #[test]
    fn one_item() {
        let q = WeightedHandoffQueue::new(1, 1);
        q.accept("a", 1);
        assert_eq!(Some("a"), q.get());
        q.close();
        assert_eq!(None, q.get());
    }

    #[test]
    fn one_item_close_first() {
        let q = WeightedHandoffQueue::new(2, 1);
        q.accept("a", 1);
        q.close();
        assert_eq!(Some("a"), q.get());
        assert_eq!(None, q.get());
    }

    #[test]
    fn more_items_than_batch_size() {
        let q = WeightedHandoffQueue::new(3, 2);
        q.accept("a", 1);
        q.accept("b", 1);
        q.accept("c", 1);
        q.close();
        assert_eq!(Some("a"), q.get());
        assert_eq!(Some("b"), q.get());
        assert_eq!(Some("c"), q.get());
        assert_eq!(None, q.get());
    }

    #[test]
    fn many_items() {
        let q = WeightedHandoffQueue::new(100, 100);
        for i in 0..950 {
            q.accept(i, 1);
        }
        q.close();
        for i in 0..950 {
            assert_eq!(Some(i), q.get());
        }
        assert_eq!(None, q.get());
    }

    #[test]
    fn across_threads() {
        let q = std::sync::Arc::new(WeightedHandoffQueue::new(2, 10));
        let q2 = q.clone();
        let t = std::thread::spawn(move || {
            for i in 0..1000 {
                q2.accept(i, 3);
            }
            q2.close();
        });
        let items: Vec<i32> = (&*q).collect();
        t.join().unwrap();
        assert_eq!((0..1000).collect::<Vec<_>>(), items);
    }
}
