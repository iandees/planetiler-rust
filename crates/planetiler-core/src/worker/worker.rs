//! Port of `worker/Worker.java`: executes a task in parallel across multiple named threads.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::stats::process_info;
use crate::stats::progress_loggers::ProgressLoggers;
use crate::stats::stats::StatsRef;
use crate::worker::{Done, panic_to_error};

static FIRST_WORKER_DIED: AtomicBool = AtomicBool::new(false);

/// A pool of `threads` threads named `{prefix}-1`, `{prefix}-2`, ... that each run a task once.
///
/// The threads are detached: if one fails, [`Worker::await_done`] returns its error immediately without waiting for
/// the others (Java interrupts the others; Rust threads cannot be interrupted so they keep running until their task
/// returns).
#[derive(Clone, Debug)]
pub struct Worker {
    prefix: String,
    done: Done,
}

impl Worker {
    /// Starts `threads` threads that each run `task` with their thread index from `0` to `threads - 1`.
    ///
    /// When each thread finishes successfully its CPU usage is recorded in the current stage of
    /// [`Stats::timers`](crate::stats::stats::Stats::timers).
    pub fn new<F>(prefix: &str, stats: StatsRef, threads: usize, task: F) -> Worker
    where
        F: Fn(usize) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        let threads = threads.max(1);
        stats.gauge_value(&format!("{prefix}_threads"), threads as f64);
        let task = Arc::new(task);
        let parent_stage = crate::util::log_util::get_stage();
        let mut dones = Vec::with_capacity(threads);
        for thread_id in 0..threads {
            let done = Done::new();
            dones.push(done.clone());
            let task = task.clone();
            let stats = stats.clone();
            let prefix_owned = prefix.to_string();
            let name = format!("{prefix}-{}", thread_id + 1);
            let parent_stage = parent_stage.clone();
            let spawned = std::thread::Builder::new().name(name.clone()).spawn(move || {
                process_info::register_current_thread();
                crate::util::log_util::set_stage_with_parent(parent_stage.as_deref(), &prefix_owned);
                tracing::trace!("Starting worker");
                let start = Instant::now();
                let result = match catch_unwind(AssertUnwindSafe(|| task(thread_id))) {
                    Ok(r) => r,
                    Err(payload) => Err(panic_to_error(payload)),
                };
                // release the task (and everything it captured) before signalling completion, so that once all
                // threads are done nothing they shared is still referenced from a worker thread
                drop(task);
                match result {
                    Ok(()) => {
                        stats.timers().finished_worker_thread(&prefix_owned, start.elapsed());
                        done.complete(Ok(()));
                    }
                    Err(e) => {
                        eprintln!("Worker {name} died");
                        // when one worker dies it may close resources causing others to die as well, so only log the
                        // first
                        if !FIRST_WORKER_DIED.swap(true, Ordering::Relaxed) {
                            eprintln!("{e:?}");
                        }
                        done.fail(e.context(format!("Worker {name} failed")));
                    }
                }
                tracing::trace!("Finished worker");
            });
            if let Err(e) = spawned {
                dones.last().unwrap().fail(anyhow::Error::new(e).context("Failed to spawn worker thread"));
            }
        }
        Worker { prefix: prefix.to_string(), done: Done::join_all(dones) }
    }

    /// Prefix of the names of threads in this pool.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Java-style alias for [`Worker::prefix`].
    pub fn get_prefix(&self) -> &str {
        &self.prefix
    }

    /// A handle that completes when all threads finish, or fails as soon as one fails.
    pub fn done(&self) -> Done {
        self.done.clone()
    }

    /// Blocks until all tasks are complete, logging `loggers` every `log_interval`.
    pub fn await_and_log(&self, loggers: &ProgressLoggers, log_interval: Duration) -> anyhow::Result<()> {
        loggers.await_and_log(&self.done, log_interval)
    }

    /// Blocks until all tasks are complete, returning the first error if any thread fails.
    pub fn await_done(&self) -> anyhow::Result<()> {
        self.done.wait()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::stats::InMemoryStats;

    #[test]
    fn exception_handled() {
        let start = Instant::now();
        let worker = Worker::new("prefix", InMemoryStats::arc(), 4, |worker_num| {
            if worker_num == 1 {
                anyhow::bail!("expected");
            } else {
                std::thread::sleep(Duration::from_millis(5000));
            }
            Ok(())
        });
        assert!(worker.await_done().is_err());
        assert!(start.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn panic_handled() {
        let worker = Worker::new("prefix", InMemoryStats::arc(), 2, |worker_num| {
            if worker_num == 1 {
                panic!("expected");
            }
            Ok(())
        });
        let err = worker.await_done().unwrap_err();
        assert!(format!("{err:#}").contains("expected"), "{err:#}");
    }

    #[test]
    fn runs_all_threads_with_names() {
        let names = Arc::new(parking_lot::Mutex::new(vec![]));
        let n = names.clone();
        let worker = Worker::new("pool", InMemoryStats::arc(), 3, move |i| {
            n.lock().push((i, std::thread::current().name().unwrap().to_string()));
            Ok(())
        });
        worker.await_done().unwrap();
        let mut names = names.lock().clone();
        names.sort();
        assert_eq!(vec![(0, "pool-1".to_string()), (1, "pool-2".to_string()), (2, "pool-3".to_string())], names);
    }

    #[test]
    fn records_finished_worker_in_stage() {
        let stats = InMemoryStats::arc();
        let stage = stats.start_stage_quietly("stage");
        Worker::new("stage_worker", stats.clone(), 2, |_| Ok(())).await_done().unwrap();
        let all = stats.timers().all();
        assert_eq!(2, all.get("stage").unwrap().thread_stats.lock().len());
        stage.stop();
    }
}
