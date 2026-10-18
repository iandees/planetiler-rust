//! Port of `stats/Timers.java`: a registry of tasks (stages of a planetiler run) that are being timed.

use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::stats::process_info::{self, ThreadState};
use crate::stats::process_time::ProcessTime;
use crate::stats::timer::Timer;
use crate::util::format;

/// Per-thread information recorded when a worker thread finishes during a stage.
#[derive(Clone, Debug)]
pub struct ThreadInfo {
    pub state: ThreadState,
    pub prefix: String,
    pub elapsed: Duration,
}

/// A named stage and the worker threads that ran during it.
#[derive(Clone, Debug)]
pub struct Stage {
    pub timer: Timer,
    pub thread_stats: Arc<Mutex<Vec<ThreadInfo>>>,
}

impl Stage {
    fn new(timer: Timer) -> Stage {
        Stage { timer, thread_stats: Arc::new(Mutex::new(Vec::new())) }
    }
}

#[derive(Default)]
struct Inner {
    timers: IndexMap<String, Stage>,
    current_stage: Option<Stage>,
}

/// Thread-safe collection of stage timers. Clones share the same registry.
#[derive(Default, Clone)]
pub struct Timers {
    inner: Arc<Mutex<Inner>>,
}

/// Handle returned when a stage starts; call [`Finishable::stop`] when it ends.
pub struct Finishable {
    name: String,
    timer: Timer,
    last: Option<Stage>,
    timers: Timers,
}

impl Finishable {
    /// Stops the timer and logs how long the stage took, and CPU usage of each worker thread pool in the stage.
    pub fn stop(self) {
        self.timer.stop();
        tracing::info!("Finished in {}", self.timer);
        for details in self.timers.get_stage_details(&self.name, true) {
            tracing::info!("  {details}");
        }
        self.timers.inner.lock().current_stage = self.last.clone();
    }

    pub fn elapsed(&self) -> ProcessTime {
        self.timer.elapsed()
    }
}

impl Timers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Log the time taken by each stage.
    pub fn print_summary(&self) {
        let all = self.all();
        let max_length = all.keys().map(|k| k.len()).max().unwrap_or(0);
        for (name, stage) in &all {
            let elapsed = stage.timer.elapsed();
            tracing::info!("\t{} {elapsed}", format::pad_right(name, max_length));
            if elapsed.wall > Duration::from_secs(1) {
                for detail in self.get_stage_details(name, true) {
                    tracing::info!("\t  {detail}");
                }
            }
        }
    }

    /// Returns lines describing CPU utilization of each worker thread pool that ran during stage `name`.
    pub fn get_stage_details(&self, name: &str, pad: bool) -> Vec<String> {
        let Some(stage) = self.inner.lock().timers.get(name).cloned() else {
            return vec![];
        };
        let elapsed = stage.timer.elapsed();
        let stats = stage.thread_stats.lock().clone();
        let mut threads: Vec<&str> = Vec::new();
        for t in &stats {
            if !threads.contains(&t.prefix.as_str()) {
                threads.push(&t.prefix);
            }
        }
        let strip_prefix = format!("{name}_");
        let strip = |s: &str| s.replace(&strip_prefix, "");
        let max_length = if pad { threads.iter().map(|t| strip(t).len()).max().unwrap_or(0) + 1 } else { 0 };
        let mut result = Vec::new();
        for thread in threads {
            let states: Vec<&ThreadInfo> = stats.iter().filter(|t| t.prefix == thread).collect();
            let num = states.len() as u32;
            let sum = states.iter().fold(ThreadState::default(), |acc, s| acc.plus(&s.state));
            let total_nanos = elapsed.wall.as_nanos() as f64 * num as f64;
            let mut line = format!(
                "{}{}x({} {}",
                format::pad_right(&strip(thread), max_length),
                format::pad_left(&num.to_string(), 2),
                format::percent(sum.cpu_time.as_nanos() as f64 / total_nanos),
                format::duration(sum.cpu_time / num),
            );
            let system_time = sum.cpu_time.saturating_sub(sum.user_time) / num;
            if system_time > Duration::from_secs(1) {
                line.push_str(&format!(" sys:{}", format::duration(system_time)));
            }
            let block_time = sum.blocking / num;
            if block_time > Duration::from_secs(1) {
                line.push_str(&format!(" block:{}", format::duration(block_time)));
            }
            let wait_time = sum.waiting / num;
            if wait_time > Duration::from_secs(1) {
                line.push_str(&format!(" wait:{}", format::duration(wait_time)));
            }
            let avg_elapsed: Duration = states.iter().map(|s| s.elapsed).sum::<Duration>() / num;
            let done = elapsed.wall.saturating_sub(avg_elapsed);
            if done > Duration::from_secs(1) {
                line.push_str(&format!(" done:{}", format::duration(done)));
            }
            line.push(')');
            result.push(line);
        }
        result
    }

    /// Starts timing a stage named `name`, logging that it started if `log_start` is true.
    pub fn start_timer(&self, name: &str, log_start: bool) -> Finishable {
        let timer = Timer::start();
        let stage = Stage::new(timer.clone());
        let mut inner = self.inner.lock();
        inner.timers.insert(name.to_string(), stage.clone());
        let last = inner.current_stage.replace(stage);
        drop(inner);
        if log_start {
            tracing::info!("");
            tracing::info!("Starting...");
        }
        Finishable { name: name.to_string(), timer, last, timers: self.clone() }
    }

    /// Record that a worker thread with `prefix` finished during the current stage after running for `elapsed`,
    /// capturing CPU usage of the current thread.
    pub fn finished_worker_thread(&self, prefix: &str, elapsed: Duration) {
        let stage = self.inner.lock().current_stage.clone();
        if let Some(stage) = stage {
            let state = process_info::get_current_thread_state();
            stage.thread_stats.lock().push(ThreadInfo { state, prefix: prefix.to_string(), elapsed });
        }
    }

    /// Record that a worker thread finished during the current stage with an explicitly-provided CPU time.
    pub fn finished_worker(&self, prefix: &str, elapsed: Duration, cpu_time: Option<Duration>) {
        let stage = self.inner.lock().current_stage.clone();
        if let Some(stage) = stage {
            let cpu = cpu_time.unwrap_or_default();
            let state = ThreadState { cpu_time: cpu, user_time: cpu, ..Default::default() };
            stage.thread_stats.lock().push(ThreadInfo { state, prefix: prefix.to_string(), elapsed });
        }
    }

    /// Returns a snapshot of all timers.
    pub fn all(&self) -> IndexMap<String, Stage> {
        self.inner.lock().timers.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timers() {
        let timers = Timers::new();
        assert!(timers.all().is_empty());
        timers.print_summary();

        let finish = timers.start_timer("task2", true);
        assert!(timers.all().get("task2").unwrap().timer.running());
        finish.stop();
        assert!(!timers.all().get("task2").unwrap().timer.running());

        timers.print_summary();
    }

    #[test]
    fn stage_details() {
        let timers = Timers::new();
        let finish = timers.start_timer("stage", false);
        timers.finished_worker("stage_worker", Duration::ZERO, Some(Duration::ZERO));
        timers.finished_worker_thread("stage_worker", Duration::ZERO);
        let details = timers.get_stage_details("stage", true);
        assert_eq!(1, details.len());
        assert!(details[0].starts_with("worker  2x("), "{}", details[0]);
        finish.stop();
    }
}
