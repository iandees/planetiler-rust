//! Port of `stats/Timers.java`: tracks elapsed time of each stage of a planetiler run.

use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::stats::process_time::ProcessTime;
use crate::stats::timer::Timer;

/// Per-thread information recorded when a worker thread finishes during a stage.
#[derive(Clone, Debug)]
pub struct ThreadInfo {
    pub prefix: String,
    pub elapsed: Duration,
    pub cpu_time: Option<Duration>,
}

/// A named stage and the worker threads that ran during it.
#[derive(Clone, Debug)]
pub struct Stage {
    pub timer: Timer,
    pub thread_stats: Arc<Mutex<Vec<ThreadInfo>>>,
}

#[derive(Default)]
struct Inner {
    timers: IndexMap<String, Stage>,
    current_stage: Option<Stage>,
}

/// Thread-safe collection of stage timers.
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
    log: bool,
}

impl Finishable {
    pub fn stop(self) {
        let mut inner = self.timers.inner.lock();
        if let Some(stage) = inner.timers.get(&self.name) {
            stage.timer.stop();
        }
        inner.current_stage = self.last.clone();
        drop(inner);
        if self.log {
            tracing::info!("Finished in {}", self.timer.elapsed());
            for details in self.timers.get_stage_details(&self.name, true) {
                tracing::info!("  {details}");
            }
        }
    }

    pub fn elapsed(&self) -> ProcessTime {
        self.timer.elapsed()
    }
}

impl Timers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn print_summary(&self) {
        let all = self.all();
        let max_length = all.keys().map(|k| k.len()).max().unwrap_or(0);
        for (name, stage) in &all {
            let elapsed = stage.timer.elapsed();
            tracing::info!("\t{name:<max_length$} {elapsed}");
            if elapsed.wall > Duration::from_secs(1) {
                for detail in self.get_stage_details(name, true) {
                    tracing::info!("\t  {detail}");
                }
            }
        }
    }

    fn get_stage_details(&self, name: &str, pad: bool) -> Vec<String> {
        let Some(stage) = self.inner.lock().timers.get(name).cloned() else {
            return vec![];
        };
        let elapsed = stage.timer.elapsed();
        let stats = stage.thread_stats.lock().clone();
        let mut threads: Vec<String> = Vec::new();
        for t in &stats {
            if !threads.contains(&t.prefix) {
                threads.push(t.prefix.clone());
            }
        }
        let strip = |s: &str| s.replace(&format!("{name}_"), "");
        let max_length = if pad { threads.iter().map(|t| strip(t).len()).max().unwrap_or(0) + 1 } else { 0 };
        let mut result = Vec::new();
        for thread in threads {
            let states: Vec<&ThreadInfo> = stats.iter().filter(|t| t.prefix == thread).collect();
            let num = states.len() as u32;
            let cpu: Duration = states.iter().filter_map(|s| s.cpu_time).sum();
            let total_nanos = elapsed.wall.as_nanos() as f64 * num as f64;
            let mut line = format!(
                "{:<max_length$}{:>2}x({:.0}% {})",
                strip(&thread),
                num,
                100.0 * cpu.as_nanos() as f64 / total_nanos.max(1.0),
                crate::util::format::duration(cpu / num.max(1)),
            );
            let avg_elapsed: Duration = states.iter().map(|s| s.elapsed).sum::<Duration>() / num.max(1);
            let done = elapsed.wall.saturating_sub(avg_elapsed);
            if done > Duration::from_secs(1) {
                line.pop();
                line.push_str(&format!(" done:{})", crate::util::format::duration(done)));
            }
            result.push(line);
        }
        result
    }

    pub fn start_timer(&self, name: &str, log_start: bool) -> Finishable {
        let timer = Timer::start();
        let stage = Stage { timer: timer.clone(), thread_stats: Arc::new(Mutex::new(Vec::new())) };
        let mut inner = self.inner.lock();
        inner.timers.insert(name.to_string(), stage.clone());
        let last = inner.current_stage.replace(stage);
        drop(inner);
        if log_start {
            tracing::info!("");
            tracing::info!("Starting...");
        }
        Finishable { name: name.to_string(), timer, last, timers: self.clone(), log: log_start }
    }

    /// Record that a worker thread finished during the current stage.
    pub fn finished_worker(&self, prefix: &str, elapsed: Duration, cpu_time: Option<Duration>) {
        if let Some(stage) = &self.inner.lock().current_stage {
            stage.thread_stats.lock().push(ThreadInfo { prefix: prefix.to_string(), elapsed, cpu_time });
        }
    }

    pub fn all(&self) -> IndexMap<String, Stage> {
        self.inner.lock().timers.clone()
    }
}
