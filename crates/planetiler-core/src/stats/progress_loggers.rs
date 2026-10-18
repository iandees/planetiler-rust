//! Port of `stats/ProgressLoggers.java`: logs the progress of a long-running task (percent complete, queue sizes, CPU
//! and memory usage, etc.)
//!
//! Build a logger with the consuming builder methods, then call [`ProgressLoggers::log`] periodically, or
//! [`ProgressLoggers::await_and_log`] to log until a [`Done`] handle completes:
//!
//! ```no_run
//! # use planetiler_core::stats::progress_loggers::ProgressLoggers;
//! # use planetiler_core::stats::counter::Counter;
//! let nodes = Counter::new();
//! let n = nodes.clone();
//! let loggers = ProgressLoggers::create()
//!     .add_rate_counter("nodes", move || n.get())
//!     .new_line()
//!     .add_process_stats();
//! loggers.log();
//! ```
//!
//! All loggers are evaluated with interior mutability, so a `ProgressLoggers` can be shared between threads.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::stats::process_info::{self, ThreadState};
use crate::util::ansi_colors::{blue, green, red, yellow};
use crate::util::file_utils::DiskBacked;
use crate::util::format::{self, pad_left, pad_right};
use crate::util::memory_estimator::HasEstimate;
use crate::worker::Done;
use crate::worker::work_queue::QueueHandle;
use crate::worker::worker::Worker;
use crate::worker::worker_pipeline::WorkerPipeline;

type LogFn = Box<dyn Fn() -> String + Send + Sync>;

const LINE_SEPARATOR: &str = "\n";

enum Item {
    /// Rendered as `" {name}: {value}"`.
    Named(String, LogFn),
    /// Part of a worker pipeline (queue or thread pool), rendered as-is.
    Pipeline(LogFn),
    /// Anything else, rendered as-is.
    Raw(LogFn),
}

impl Item {
    fn render(&self) -> String {
        match self {
            Item::Named(name, f) => format!(" {name}: {}", f()),
            Item::Pipeline(f) | Item::Raw(f) => f(),
        }
    }
}

/// Logs the progress of a long-running task.
#[derive(Default)]
pub struct ProgressLoggers {
    loggers: Vec<Item>,
}

/// Tracks the change in a value between calls.
struct Delta {
    last_value: i64,
    last_time: Instant,
}

impl Delta {
    fn new(value: i64) -> Mutex<Delta> {
        Mutex::new(Delta { last_value: value, last_time: Instant::now() })
    }

    /// Returns (value diff, time diff in seconds) since the last call, treating a decrease as a reset to 0.
    fn update(&mut self, value_now: i64) -> (i64, f64) {
        let now = Instant::now();
        let time_diff = now.duration_since(self.last_time).as_secs_f64();
        let mut value_diff = value_now - self.last_value;
        if value_diff < 0 {
            value_diff = value_now;
        }
        self.last_value = value_now;
        self.last_time = now;
        (value_diff, time_diff)
    }
}

impl ProgressLoggers {
    /// Returns a new empty logger.
    pub fn create() -> ProgressLoggers {
        ProgressLoggers::default()
    }

    /// Adds `" name: [ numCompleted rate/s ]"` to the logger.
    pub fn add_rate_counter(self, name: &str, get_value: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.add_rate_counter_colored(name, get_value, false)
    }

    /// Adds `" name: [ numCompleted rate/s ]"` to the logger, colored green if `color` and rate > 0.
    pub fn add_rate_counter_colored(
        mut self,
        name: &str,
        get_value: impl Fn() -> i64 + Send + Sync + 'static,
        color: bool,
    ) -> Self {
        let delta = Delta::new(get_value());
        self.loggers.push(Item::Named(
            name.to_string(),
            Box::new(move || {
                let value_now = get_value();
                let (value_diff, time_diff) = delta.lock().update(value_now);
                let result = format!(
                    "[ {} {}/s ]",
                    format::numeric(value_now, true),
                    format::numeric(value_diff as f64 / time_diff, true)
                );
                if color && value_diff > 0 { green(&result) } else { result }
            }),
        ));
        self
    }

    /// Adds `" name: [ numCompleted pctComplete% rate/s ]"` to the logger where `total` is the total number of items
    /// to process.
    pub fn add_rate_percent_counter(
        self,
        name: &str,
        total: i64,
        get_value: impl Fn() -> i64 + Send + Sync + 'static,
        color: bool,
    ) -> Self {
        self.add_rate_percent_counter_with_format(name, total, get_value, |n| format::numeric(n, true), color)
    }

    /// Adds `" name: [ numCompleted pctComplete% rate/s ]"` to the logger where `total` is the total number of bytes
    /// to process.
    pub fn add_storage_rate_percent_counter(
        self,
        name: &str,
        total: i64,
        get_value: impl Fn() -> i64 + Send + Sync + 'static,
        color: bool,
    ) -> Self {
        self.add_rate_percent_counter_with_format(name, total, get_value, |n| format::storage(n, true), color)
    }

    /// Adds `" name: [ numCompleted pctComplete% rate/s ]"` to the logger, formatting numbers with `formatter`.
    pub fn add_rate_percent_counter_with_format(
        mut self,
        name: &str,
        total: i64,
        get_value: impl Fn() -> i64 + Send + Sync + 'static,
        formatter: impl Fn(f64) -> String + Send + Sync + 'static,
        color: bool,
    ) -> Self {
        // if there's no total, we can't show progress so fall back to rate logger instead
        if total == 0 {
            return self.add_rate_counter_colored(name, get_value, color);
        }
        let delta = Delta::new(get_value());
        self.loggers.push(Item::Named(
            name.to_string(),
            Box::new(move || {
                let value_now = get_value();
                let (value_diff, time_diff) = delta.lock().update(value_now);
                let result = format!(
                    "[ {} {} {}/s ]",
                    formatter(value_now as f64),
                    pad_left(&format::percent(value_now as f64 / total as f64), 4),
                    formatter(value_diff as f64 / time_diff)
                );
                if color && value_diff > 0 { green(&result) } else { result }
            }),
        ));
        self
    }

    /// Adds `" name: [ numComplete / total pctComplete% ]"` to the logger.
    pub fn add_percent_counter(
        mut self,
        name: &str,
        total: i64,
        get_value: impl Fn() -> i64 + Send + Sync + 'static,
    ) -> Self {
        self.loggers.push(Item::Named(
            name.to_string(),
            Box::new(move || {
                let value_now = get_value();
                format!(
                    "[ {} / {} {} ]",
                    pad_left(&value_now.to_string(), 3),
                    pad_left(&total.to_string(), 3),
                    pad_left(&format::percent(value_now as f64 / total as f64), 4)
                )
            }),
        ));
        self
    }

    /// Adds the current number of items in a queue and the queue's capacity to the output.
    pub fn add_queue_stats(mut self, queue: Arc<dyn QueueHandle>) -> Self {
        self.loggers.push(Item::Pipeline(Box::new(move || {
            format!(
                " -> {}",
                pad_left(
                    &format!(
                        "({}/{})",
                        format::numeric(queue.get_pending(), false),
                        format::numeric(queue.get_capacity(), false)
                    ),
                    9
                )
            )
        })));
        self
    }

    /// Adds a fixed string to the output.
    pub fn add_str(mut self, obj: &str) -> Self {
        let s = obj.to_string();
        self.loggers.push(Item::Raw(Box::new(move || s.clone())));
        self
    }

    /// Adds the result of calling `obj` to the output.
    #[allow(clippy::should_implement_trait)]
    pub fn add(mut self, obj: impl Fn() -> String + Send + Sync + 'static) -> Self {
        self.loggers.push(Item::Raw(Box::new(obj)));
        self
    }

    /// Adds the current size of a file on disk.
    pub fn add_file_size_path(self, file: PathBuf) -> Self {
        self.add(move || {
            let bytes = std::fs::metadata(&file).map_or_else(|_| "-".to_string(), |m| format::storage(m.len(), false));
            format!(" {}", pad_right(&bytes, 5))
        })
    }

    /// Adds the current size of something stored on disk.
    pub fn add_file_size(self, file: Arc<dyn DiskBacked + Send + Sync>) -> Self {
        self.add(move || format!(" {}", pad_right(&format::storage(file.disk_usage_bytes(), false), 5)))
    }

    /// Adds `" name: size"` for something stored on disk.
    pub fn add_named_file_size(mut self, name: &str, file: Arc<dyn DiskBacked + Send + Sync>) -> Self {
        self.loggers
            .push(Item::Named(name.to_string(), Box::new(move || format::storage(file.disk_usage_bytes(), true))));
        self
    }

    /// Adds the name and total of disk and memory usage of `thing`.
    pub fn add_file_size_and_ram<T: DiskBacked + HasEstimate + Send + Sync + 'static>(
        self,
        name: &str,
        thing: Arc<T>,
    ) -> Self {
        let name = name.to_string();
        self.add(move || {
            let bytes = thing.disk_usage_bytes() + thing.estimate_memory_usage_bytes();
            let sep = if name.trim().is_empty() { "" } else { ": " };
            format!(" {name}{sep}{}", pad_right(&format::storage(bytes, false), 5))
        })
    }

    /// Adds the current estimated size of an in-memory object to the output.
    pub fn add_in_memory_object(mut self, name: &str, object: Arc<dyn HasEstimate + Send + Sync>) -> Self {
        self.loggers.push(Item::Named(
            name.to_string(),
            Box::new(move || format::storage(object.estimate_memory_usage_bytes(), true)),
        ));
        self
    }

    /// Adds the average number of CPUs used since the last log, and memory usage of the process.
    ///
    /// Java also logs GC time and post-GC memory usage, which do not apply in Rust; the `gc` entry is kept (always
    /// `0%`) so the log format stays the same.
    pub fn add_process_stats(self) -> Self {
        let mut this = self.add_delta_logger(
            "cpus",
            || process_info::get_process_cpu_time().unwrap_or_default(),
            |num| blue(&format::decimal(num)),
        );
        this = this.add_delta_logger("gc", process_info::get_gc_time, |num| {
            let formatted = format::percent(num);
            if num > 0.6 {
                red(&formatted)
            } else if num > 0.3 {
                yellow(&formatted)
            } else {
                formatted
            }
        });
        this.loggers.push(Item::Named(
            "mem".to_string(),
            Box::new(|| {
                format!(
                    "{}/{}",
                    format::storage(process_info::get_on_heap_used_memory_bytes(), false),
                    format::storage(process_info::get_max_memory_bytes(), false)
                )
            }),
        ));
        this
    }

    /// Adds a logger that keeps track of the value each time it is invoked and logs the rate of change.
    fn add_delta_logger(
        mut self,
        name: &str,
        supplier: impl Fn() -> Duration + Send + Sync + 'static,
        fmt: impl Fn(f64) -> String + Send + Sync + 'static,
    ) -> Self {
        let state = Mutex::new((supplier(), Instant::now()));
        self.loggers.push(Item::Named(
            name.to_string(),
            Box::new(move || {
                let current_value = supplier();
                let current_time = Instant::now();
                let mut state = state.lock();
                let elapsed = current_time.duration_since(state.1).as_nanos().max(1) as f64;
                let rate = current_value.saturating_sub(state.0).as_nanos() as f64 / elapsed;
                *state = (current_value, current_time);
                pad_left(&fmt(rate), 3)
            }),
        ));
        self
    }

    /// Adds the CPU utilization since the last log of every thread whose name starts with `prefix`.
    pub fn add_thread_pool_stats(mut self, name: &str, prefix: &str) -> Self {
        let first = !matches!(self.loggers.last(), Some(Item::Pipeline(_)));
        let state: Mutex<(BTreeMap<i64, ThreadState>, Instant)> =
            Mutex::new((process_info::get_thread_stats(), Instant::now()));
        let name = name.to_string();
        let prefix = prefix.to_string();
        self.loggers.push(Item::Pipeline(Box::new(move || {
            let mut state = state.lock();
            let (last_threads, last_time) = &mut *state;
            let new_threads = process_info::get_thread_stats();
            let mut old_and_new = last_threads.clone();
            old_and_new.extend(new_threads.iter().map(|(k, v)| (*k, v.clone())));
            let current_time = Instant::now();
            let time_diff = current_time.duration_since(*last_time).as_nanos().max(1) as f64;
            let percents: Vec<String> = old_and_new
                .values()
                .filter(|t| t.name.starts_with(&prefix))
                .map(|thread| {
                    if !new_threads.contains_key(&thread.id) {
                        return " -%".to_string();
                    }
                    let last = last_threads.get(&thread.id).map_or(Duration::ZERO, |t| t.cpu_time);
                    pad_left(&format::percent(thread.cpu_time.saturating_sub(last).as_nanos() as f64 / time_diff), 3)
                })
                .collect();
            *last_time = current_time;
            last_threads.extend(new_threads);
            format!("{}{name}({})", if first { " " } else { " -> " }, percents.join(" "))
        })));
        self
    }

    /// Adds the CPU utilization since last log of every thread in a [`Worker`] pool.
    pub fn add_thread_pool_stats_for_worker(self, name: &str, worker: &Worker) -> Self {
        self.add_thread_pool_stats(name, &format!("{}-", worker.prefix()))
    }

    /// Adds the alternating worker thread pool / queue / worker thread pool stats for the pipeline to the output.
    pub fn add_pipeline_stats(mut self, pipeline: &WorkerPipeline) -> Self {
        for stage in pipeline.stages() {
            if let Some(queue) = &stage.input_queue {
                self = self.add_queue_stats(queue.clone());
            }
            if let Some(worker) = &stage.worker {
                self = self.add_thread_pool_stats_for_worker(&stage.name, worker);
            }
        }
        self
    }

    /// Adds a line break to the output.
    pub fn new_line(self) -> Self {
        self.add_str(LINE_SEPARATOR)
    }

    /// Logs the current progress at info level.
    pub fn log(&self) {
        tracing::info!("{}", self.get_log());
    }

    /// Returns the current progress log line.
    pub fn get_log(&self) -> String {
        let joined: String = self.loggers.iter().map(Item::render).collect();
        // indent continuation lines
        let mut result = String::with_capacity(joined.len());
        let mut lines = joined.split(LINE_SEPARATOR);
        if let Some(first) = lines.next() {
            result.push_str(first);
        }
        for line in lines {
            result.push_str(LINE_SEPARATOR);
            result.push_str("    ");
            result.push_str(line.trim_start());
        }
        result
    }

    /// Logs at a fixed `log_interval` until `done` completes, then logs one last time. Returns the error if `done`
    /// failed.
    pub fn await_and_log(&self, done: &Done, log_interval: Duration) -> anyhow::Result<()> {
        loop {
            match done.wait_timeout(log_interval) {
                Some(result) => {
                    self.log();
                    return result;
                }
                None => self.log(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use super::*;
    use crate::stats::stats::InMemoryStats;
    use crate::util::ansi_colors;

    fn assert_eventually_equals(expected: &str, actual: impl Fn() -> String) {
        for _ in 0..100 {
            if actual() == expected {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(expected, actual());
    }

    #[test]
    fn log_worker_pipeline() {
        let continue_latch = Arc::new(Barrier::new(6));
        let ready = Arc::new(Barrier::new(6));
        let (r1, c1) = (ready.clone(), continue_latch.clone());
        let (r2, c2) = (ready.clone(), continue_latch.clone());
        let (r3, c3) = (ready.clone(), continue_latch.clone());
        let pipeline = WorkerPipeline::start("pipeline", InMemoryStats::arc())
            .from_generator::<i32, _>("reader", move |_next| {
                r1.wait();
                c1.wait();
                Ok(())
            })
            .add_buffer("reader_queue", 10)
            .add_worker::<i32, _>("worker", 2, move |_a, _b| {
                r2.wait();
                c2.wait();
                Ok(())
            })
            .add_buffer("writer_queue", 10)
            .sink_to("writer", 2, move |_a| {
                r3.wait();
                c3.wait();
                Ok(())
            });

        let loggers = ProgressLoggers::create().new_line().add_pipeline_stats(&pipeline);
        ready.wait();

        let re = regex::Regex::new("[ 0-9][0-9]%").unwrap();
        assert_eventually_equals(
            "\n    reader( 0%) ->    (0/13) -> worker( 0%  0%) ->    (0/14) -> writer( 0%  0%)",
            || re.replace_all(&loggers.get_log(), " 0%").to_string(),
        );
        continue_latch.wait();
        pipeline.await_and_log(&loggers, Duration::from_secs(10)).unwrap();
        assert_eventually_equals(
            "\n    reader( -%) ->    (0/13) -> worker( -%  -%) ->    (0/14) -> writer( -%  -%)",
            || loggers.get_log(),
        );
    }

    #[test]
    fn counters() {
        ansi_colors::set_use_colors(false);
        let counter = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let (c1, c2, c3) = (counter.clone(), counter.clone(), counter.clone());
        let loggers = ProgressLoggers::create()
            .add_rate_counter("rate", move || c1.load(std::sync::atomic::Ordering::Relaxed))
            .add_rate_percent_counter("pct", 100, move || c2.load(std::sync::atomic::Ordering::Relaxed), false)
            .add_percent_counter("done", 100, move || c3.load(std::sync::atomic::Ordering::Relaxed))
            .add_str(" fixed")
            .add_process_stats();
        counter.store(50, std::sync::atomic::Ordering::Relaxed);
        let log = loggers.get_log();
        assert!(log.starts_with(" rate: [   50 "), "{log}");
        assert!(log.contains(" pct: [   50  50% "), "{log}");
        assert!(log.contains(" done: [  50 / 100  50% ]"), "{log}");
        assert!(log.contains(" fixed cpus: "), "{log}");
        assert!(log.contains(" gc:  0% mem: "), "{log}");
    }
}
