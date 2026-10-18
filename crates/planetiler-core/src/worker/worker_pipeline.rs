//! Port of `worker/WorkerPipeline.java`: a mini-framework for chaining sequential steps that run in dedicated threads
//! with a queue between each.
//!
//! For example:
//!
//! ```no_run
//! # use planetiler_core::worker::worker_pipeline::WorkerPipeline;
//! # use planetiler_core::stats::stats::InMemoryStats;
//! # fn main() -> anyhow::Result<()> {
//! let pipeline = WorkerPipeline::start("name", InMemoryStats::arc())
//!     .read_from("reader", vec![1, 2, 3])
//!     .add_buffer("reader_queue", 10)
//!     .add_worker("process", 2, |prev, next| {
//!         for i in prev {
//!             next.accept(i * 2);
//!         }
//!         Ok(())
//!     })
//!     .add_buffer("writer_queue", 10)
//!     .sink_to_consumer("writer", 1, |result| println!("{result}"));
//! pipeline.await_done()?;
//! # Ok(())
//! # }
//! ```
//!
//! To do any forking/joining, construct and wire up queues and each sequence of steps manually.
//!
//! If any step fails, every queue in the pipeline is [aborted](WorkQueue::abort) so the remaining threads can exit,
//! and [`WorkerPipeline::await_done`] returns the first error.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::stats::progress_loggers::ProgressLoggers;
use crate::stats::stats::StatsRef;
use crate::worker::Done;
use crate::worker::work_queue::{QueueHandle, WorkQueue, WorkQueueReader, WorkQueueWriter};
use crate::worker::worker::Worker;

/// One step of a running pipeline, used for logging.
#[derive(Clone)]
pub struct PipelineStage {
    /// Name of the step (without the pipeline prefix).
    pub name: String,
    /// Queue this step reads from, if any.
    pub input_queue: Option<Arc<dyn QueueHandle>>,
    /// Threads running this step, if any.
    pub worker: Option<Worker>,
}

/// A running pipeline of steps; wait for it to finish with [`WorkerPipeline::await_done`] or
/// [`WorkerPipeline::await_and_log`].
#[derive(Clone)]
pub struct WorkerPipeline {
    name: String,
    stages: Vec<PipelineStage>,
    done: Done,
}

impl WorkerPipeline {
    /// Returns a new pipeline builder where all worker and queue names will start with `{prefix}_`.
    pub fn start(prefix: &str, stats: StatsRef) -> Empty {
        Empty { prefix: prefix.to_string(), stats }
    }

    /// Name of the last step in this pipeline.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// All steps of this pipeline, from first to last.
    pub fn stages(&self) -> &[PipelineStage] {
        &self.stages
    }

    /// Handle that completes when every step has finished, or fails when any step fails.
    pub fn done(&self) -> Done {
        self.done.clone()
    }

    /// Blocks until all work has been completed by all steps of this pipeline, logging progress at a fixed
    /// `log_interval`.
    pub fn await_and_log(&self, loggers: &ProgressLoggers, log_interval: Duration) -> anyhow::Result<()> {
        loggers.await_and_log(&self.done, log_interval)
    }

    /// Blocks until all work has been completed by all steps of this pipeline, returning the first error if any step
    /// fails.
    pub fn await_done(&self) -> anyhow::Result<()> {
        self.done.wait()
    }
}

/// Builder for a new pipeline that does not yet have any steps.
pub struct Empty {
    prefix: String,
    stats: StatsRef,
}

/// Intermediate step while building a pipeline that requires a queue to buffer items before adding the next step.
pub struct Bufferable<T> {
    #[allow(clippy::type_complexity)]
    build: Box<dyn FnOnce(&str, usize, usize) -> Builder<T>>,
}

impl<T> Bufferable<T> {
    /// Adds a [`WorkQueue`] that groups items into batches of `batch_size` before enqueueing them to reduce contention
    /// when many threads are reading or writing to the queue simultaneously.
    pub fn add_buffer_batched(self, name: &str, size: usize, batch_size: usize) -> Builder<T> {
        (self.build)(name, size, batch_size)
    }

    /// Adds a [`WorkQueue`] with batching disabled.
    pub fn add_buffer(self, name: &str, size: usize) -> Builder<T> {
        self.add_buffer_batched(name, size, 1)
    }
}

/// Internal state shared by all steps of the pipeline under construction.
struct Graph {
    stages: Vec<PipelineStage>,
    /// Handle that completes when every step so far has completed.
    done: Done,
    queues: Vec<Arc<dyn QueueHandle>>,
}

impl Empty {
    /// Adds an initial step that runs `producer` in `threads` worker threads to produce items.
    pub fn from_generator_threads<T, F>(self, name: &str, threads: usize, producer: F) -> Bufferable<T>
    where
        T: Send + 'static,
        F: Fn(&mut WorkQueueWriter<T>) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        let Empty { prefix, stats } = self;
        let name = name.to_string();
        Bufferable {
            build: Box::new(move |queue_name, size, batch_size| {
                let next_queue = WorkQueue::new(&format!("{prefix}_{queue_name}"), size, batch_size, &stats);
                let q = next_queue.clone();
                let worker = Worker::new(&format!("{prefix}_{name}"), stats.clone(), threads, move |_| {
                    let mut writer = q.thread_local_writer();
                    producer(&mut writer)
                });
                let queue_handle: Arc<dyn QueueHandle> = Arc::new(next_queue.clone());
                let graph = Graph {
                    stages: vec![PipelineStage { name: name.clone(), input_queue: None, worker: Some(worker.clone()) }],
                    done: worker.done(),
                    queues: vec![queue_handle.clone()],
                };
                Builder::new(prefix, stats, graph, next_queue, Some(worker))
            }),
        }
    }

    /// Adds an initial step that runs `producer` in 1 worker thread to produce items.
    pub fn from_generator<T, F>(self, name: &str, producer: F) -> Bufferable<T>
    where
        T: Send + 'static,
        F: Fn(&mut WorkQueueWriter<T>) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        self.from_generator_threads(name, 1, producer)
    }

    /// Adds an initial step that reads all items from `iterable` in a single worker thread.
    pub fn read_from<I>(self, name: &str, iterable: I) -> Bufferable<I::Item>
    where
        I: IntoIterator,
        I::IntoIter: Send + 'static,
        I::Item: Send + 'static,
    {
        let iter = Mutex::new(Some(iterable.into_iter()));
        self.from_generator(name, move |next| {
            if let Some(iter) = iter.lock().take() {
                for item in iter {
                    next.accept(item);
                }
            }
            Ok(())
        })
    }

    /// Populates an initial queue with `items` for subsequent steps to process but does not start a worker thread.
    pub fn read_from_tiny<T: Send + 'static>(self, name: &str, items: Vec<T>) -> Builder<T> {
        let queue = WorkQueue::new(&format!("{}_{name}", self.prefix), items.len(), 1, &self.stats);
        {
            let mut writer = queue.thread_local_writer();
            for item in items {
                writer.accept(item);
            }
        }
        queue.close();
        self.read_from_queue(queue)
    }

    /// Starts the pipeline from a queue that is populated externally, and does not start any threads.
    pub fn read_from_queue<T: Send + 'static>(self, input: WorkQueue<T>) -> Builder<T> {
        let graph = Graph { stages: vec![], done: Done::completed(), queues: vec![Arc::new(input.clone())] };
        Builder::new(self.prefix, self.stats, graph, input, None)
    }
}

/// A step while building a pipeline that needs a subsequent step to process elements of type `O`.
pub struct Builder<O> {
    prefix: String,
    stats: StatsRef,
    graph: Graph,
    output_queue: WorkQueue<O>,
}

impl<O: Send + 'static> Builder<O> {
    fn new(
        prefix: String,
        stats: StatsRef,
        mut graph: Graph,
        output_queue: WorkQueue<O>,
        worker: Option<Worker>,
    ) -> Builder<O> {
        // close the output queue after this step and every previous one finish successfully
        if worker.is_some() {
            let q = output_queue.clone();
            graph.done = graph.done.then_run(move || {
                q.close();
                Ok(())
            });
        }
        Builder { prefix, stats, graph, output_queue }
    }

    /// The queue that the next step will read from.
    pub fn output_queue(&self) -> &WorkQueue<O> {
        &self.output_queue
    }

    /// Runs `step` simultaneously in `threads` threads which consumes items and emits new ones that must be buffered.
    pub fn add_worker<O2, F>(self, name: &str, threads: usize, step: F) -> Bufferable<O2>
    where
        O2: Send + 'static,
        F: Fn(&mut WorkQueueReader<O>, &mut WorkQueueWriter<O2>) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        let name = name.to_string();
        Bufferable {
            build: Box::new(move |queue_name, size, batch_size| {
                let Builder { prefix, stats, mut graph, output_queue } = self;
                let next_queue = WorkQueue::new(&format!("{prefix}_{queue_name}"), size, batch_size, &stats);
                let (input, output) = (output_queue.clone(), next_queue.clone());
                let worker = Worker::new(&format!("{prefix}_{name}"), stats.clone(), threads, move |_| {
                    let mut reader = input.thread_local_reader();
                    let mut writer = output.thread_local_writer();
                    step(&mut reader, &mut writer)
                });
                graph.stages.push(PipelineStage {
                    name: name.clone(),
                    input_queue: Some(Arc::new(output_queue)),
                    worker: Some(worker.clone()),
                });
                graph.queues.push(Arc::new(next_queue.clone()));
                graph.done = Done::join_all([worker.done(), graph.done]);
                Builder::new(prefix, stats, graph, next_queue, Some(worker))
            }),
        }
    }

    /// Runs `step` simultaneously in `threads` threads that consume items but do not emit any.
    pub fn sink_to<F>(self, name: &str, threads: usize, step: F) -> WorkerPipeline
    where
        F: Fn(&mut WorkQueueReader<O>) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        let Builder { prefix, stats, mut graph, output_queue } = self;
        let input = output_queue.clone();
        let worker = Worker::new(&format!("{prefix}_{name}"), stats, threads, move |_| {
            let mut reader = input.thread_local_reader();
            step(&mut reader)
        });
        graph.stages.push(PipelineStage {
            name: name.to_string(),
            input_queue: Some(Arc::new(output_queue)),
            worker: Some(worker.clone()),
        });
        let done = Done::join_all([worker.done(), graph.done]);
        let queues = graph.queues;
        done.on_complete(move |result| {
            if result.is_err() {
                for queue in &queues {
                    queue.abort();
                }
            }
        });
        WorkerPipeline { name: name.to_string(), stages: graph.stages, done }
    }

    /// Runs `threads` simultaneous worker threads that consume items from the previous step and invoke `consumer` for
    /// each one.
    pub fn sink_to_consumer<F>(self, name: &str, threads: usize, consumer: F) -> WorkerPipeline
    where
        F: Fn(O) + Send + Sync + 'static,
    {
        self.sink_to(name, threads, move |prev| {
            for item in prev {
                consumer(item);
            }
            Ok(())
        })
    }

    // Java's `processAndWrite(inlineWrites, processThreads, writeThreads, FeatureGroup, handler)` is
    // `crate::collection::feature_group::process_and_write`.
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::stats::stats::InMemoryStats;

    fn result_set() -> (Arc<Mutex<BTreeSet<i32>>>, impl Fn(i32) + Send + Sync + 'static) {
        let set = Arc::new(Mutex::new(BTreeSet::new()));
        let s = set.clone();
        (set, move |i| {
            s.lock().insert(i);
        })
    }

    #[test]
    fn simple_pipeline() {
        let (result, add) = result_set();
        let pipeline = WorkerPipeline::start("test", InMemoryStats::arc())
            .from_generator("reader", |next| {
                next.accept(0);
                next.accept(1);
                Ok(())
            })
            .add_buffer("reader_queue", 1)
            .add_worker("process", 1, |prev, next| {
                for item in prev {
                    next.accept(item * 2 + 1);
                    next.accept(item * 2 + 2);
                }
                Ok(())
            })
            .add_buffer("writer_queue", 1)
            .sink_to_consumer("writer", 1, add);
        pipeline.await_and_log(&ProgressLoggers::create(), Duration::from_secs(1)).unwrap();
        assert_eq!(BTreeSet::from([1, 2, 3, 4]), *result.lock());
    }

    #[test]
    fn pipeline_from_queue() {
        let stats = InMemoryStats::arc();
        let queue = WorkQueue::new("readerqueue", 10, 1, &stats);
        let (result, add) = result_set();
        let pipeline = WorkerPipeline::start("test", stats)
            .read_from_queue(queue.clone())
            .add_worker("process", 1, |prev, next| {
                while let Some(item) = prev.get() {
                    next.accept(item * 2 + 1);
                    next.accept(item * 2 + 2);
                }
                Ok(())
            })
            .add_buffer("writer_queue", 1)
            .sink_to_consumer("writer", 1, add);
        std::thread::spawn(move || {
            queue.accept(0);
            queue.accept(1);
            queue.close();
        });
        pipeline.await_done().unwrap();
        assert_eq!(BTreeSet::from([1, 2, 3, 4]), *result.lock());
    }

    #[test]
    fn pipeline_from_iterator() {
        let (result, add) = result_set();
        let pipeline = WorkerPipeline::start("test", InMemoryStats::arc())
            .read_from("reader", vec![0, 1])
            .add_buffer("reader_queue", 1)
            .add_worker("process", 1, |prev, next| {
                for item in prev {
                    next.accept(item * 2 + 1);
                    next.accept(item * 2 + 2);
                }
                Ok(())
            })
            .add_buffer("writer_queue", 1)
            .sink_to_consumer("writer", 1, add);
        pipeline.await_and_log(&ProgressLoggers::create(), Duration::from_secs(1)).unwrap();
        assert_eq!(BTreeSet::from([1, 2, 3, 4]), *result.lock());
    }

    #[test]
    fn pipeline_from_tiny() {
        let (result, add) = result_set();
        let pipeline = WorkerPipeline::start("test", InMemoryStats::arc())
            .read_from_tiny("items", vec![0, 1])
            .sink_to_consumer("writer", 2, add);
        pipeline.await_done().unwrap();
        assert_eq!(BTreeSet::from([0, 1]), *result.lock());
    }

    #[test]
    fn throwing_exception_in_pipeline_handled_gracefully() {
        for failure_stage in 1..=3 {
            let pipeline = WorkerPipeline::start("test", InMemoryStats::arc())
                .from_generator("reader", move |next| {
                    if failure_stage == 1 {
                        anyhow::bail!("expected");
                    }
                    next.accept(0);
                    next.accept(1);
                    Ok(())
                })
                .add_buffer("reader_queue", 1)
                .add_worker("process", 1, move |prev, next| {
                    if failure_stage == 2 {
                        anyhow::bail!("expected");
                    }
                    for item in prev {
                        next.accept(item * 2 + 1);
                        next.accept(item * 2 + 2);
                    }
                    Ok(())
                })
                .add_buffer("writer_queue", 1)
                .sink_to("writer", 1, move |prev| {
                    for _ in prev {
                        if failure_stage == 3 {
                            anyhow::bail!("expected");
                        }
                    }
                    Ok(())
                });
            assert!(pipeline.await_done().is_err(), "stage {failure_stage}");
        }
    }

    #[test]
    fn batched_multithreaded_pipeline() {
        let sum = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let s = sum.clone();
        let pipeline = WorkerPipeline::start("test", InMemoryStats::arc())
            .from_generator_threads("reader", 2, |next| {
                for i in 0..10_000 {
                    next.accept(i);
                }
                Ok(())
            })
            .add_buffer_batched("reader_queue", 1000, 100)
            .add_worker("process", 3, |prev, next| {
                for item in prev {
                    next.accept(item as i64 * 2);
                }
                Ok(())
            })
            .add_buffer_batched("writer_queue", 1000, 100)
            .sink_to_consumer("writer", 2, move |i| {
                s.fetch_add(i, std::sync::atomic::Ordering::Relaxed);
            });
        pipeline.await_done().unwrap();
        assert_eq!(2 * 2 * (0..10_000i64).sum::<i64>(), sum.load(std::sync::atomic::Ordering::Relaxed));
    }
}
