//! Port of `stats/Stats.java`: the interface for collecting runtime statistics.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::stats::counter::Counter;
use crate::stats::timers::{Finishable, Timers};

/// Supplies a live numeric value for a gauge or counter.
pub type NumberSupplier = Arc<dyn Fn() -> f64 + Send + Sync>;
/// Supplies live per-label counter values.
pub type LabeledSupplier = Arc<dyn Fn() -> BTreeMap<String, i64> + Send + Sync>;
/// Supplies the current size of a monitored file.
pub type SizeSupplier = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A file whose size should be reported in stats.
#[derive(Clone)]
pub struct MonitoredFile {
    pub path: PathBuf,
    pub size_provider: SizeSupplier,
}

/// Collects statistics about a planetiler run. Implementations must be thread-safe.
///
/// Shared as `Arc<dyn Stats>`.
pub trait Stats: Send + Sync {
    /// Log a summary of timings for each stage.
    fn print_summary(&self) {
        self.timers().print_summary();
    }

    /// Start timing a named stage and log when it starts and finishes.
    fn start_stage(&self, name: &str) -> Finishable {
        self.start_stage_with_log(name, true)
    }

    /// Start timing a named stage without logging.
    fn start_stage_quietly(&self, name: &str) -> Finishable {
        self.start_stage_with_log(name, false)
    }

    fn start_stage_with_log(&self, name: &str, log: bool) -> Finishable {
        if log {
            tracing::info!("");
            tracing::info!("Starting...");
        }
        self.timers().start_timer(name, log)
    }

    /// Record that a tile was written.
    fn wrote_tile(&self, zoom: u32, bytes: usize);

    /// The timers tracking each stage.
    fn timers(&self) -> &Timers;

    /// Files being monitored for size.
    fn monitored_files(&self) -> BTreeMap<String, MonitoredFile>;

    /// Track the size of a file over time.
    fn monitor_file(&self, name: &str, path: PathBuf) {
        let p = path.clone();
        self.monitor_file_with_size(name, path, Arc::new(move || crate::util::file_utils::size(&p).unwrap_or(0)));
    }

    fn monitor_file_with_size(&self, name: &str, path: PathBuf, size_provider: SizeSupplier);

    /// Track the estimated memory usage of an in-memory object.
    fn monitor_in_memory_object(&self, name: &str, estimate: SizeSupplier);

    /// Register a counter whose value is computed from `supplier`.
    fn counter(&self, name: &str, supplier: NumberSupplier);

    /// Register a set of counters with a label, values computed from `supplier`.
    fn labeled_counter(&self, name: &str, label: &str, supplier: LabeledSupplier);

    /// Create a counter that can be incremented from many threads.
    fn long_counter(&self, name: &str) -> Counter;

    /// Create a counter of nanoseconds that can be incremented from many threads.
    fn nano_counter(&self, name: &str) -> Counter;

    /// Register a gauge whose value is computed from `supplier`.
    fn gauge(&self, name: &str, supplier: NumberSupplier);

    /// Set a gauge to a fixed value.
    fn gauge_value(&self, name: &str, value: f64) {
        self.gauge(name, Arc::new(move || value));
    }

    /// Counts of data errors by error code.
    fn data_errors(&self) -> BTreeMap<String, i64>;

    /// Record that an input element was processed and emitted features to a layer.
    fn processed_element(&self, elem_type: &str, layer: &str, zoom: u32);

    /// Record that a data error occurred, for example a geometry that could not be processed.
    fn data_error(&self, error_code: &str);

    /// Record the number of features emitted to a layer at a zoom.
    fn emitted_features(&self, z: u32, layer: &str, num_features: usize);
}

/// Shared handle to a [`Stats`] implementation.
pub type StatsRef = Arc<dyn Stats>;

/// Stats implementation that keeps counters in memory, for tests and simple runs (Java's `Stats.InMemory`).
#[derive(Default)]
pub struct InMemoryStats {
    timers: Timers,
    monitored_files: parking_lot::Mutex<BTreeMap<String, MonitoredFile>>,
    data_errors: parking_lot::Mutex<BTreeMap<String, i64>>,
}

impl InMemoryStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn arc() -> StatsRef {
        Arc::new(Self::new())
    }
}

impl Stats for InMemoryStats {
    fn wrote_tile(&self, _zoom: u32, _bytes: usize) {}

    fn timers(&self) -> &Timers {
        &self.timers
    }

    fn monitored_files(&self) -> BTreeMap<String, MonitoredFile> {
        self.monitored_files.lock().clone()
    }

    fn monitor_file_with_size(&self, name: &str, path: PathBuf, size_provider: SizeSupplier) {
        self.monitored_files.lock().insert(name.to_string(), MonitoredFile { path, size_provider });
    }

    fn monitor_in_memory_object(&self, _name: &str, _estimate: SizeSupplier) {}

    fn counter(&self, _name: &str, _supplier: NumberSupplier) {}

    fn labeled_counter(&self, _name: &str, _label: &str, _supplier: LabeledSupplier) {}

    fn long_counter(&self, _name: &str) -> Counter {
        Counter::new()
    }

    fn nano_counter(&self, _name: &str) -> Counter {
        Counter::new()
    }

    fn gauge(&self, _name: &str, _supplier: NumberSupplier) {}

    fn data_errors(&self) -> BTreeMap<String, i64> {
        self.data_errors.lock().clone()
    }

    fn processed_element(&self, _elem_type: &str, _layer: &str, _zoom: u32) {}

    fn data_error(&self, error_code: &str) {
        *self.data_errors.lock().entry(error_code.to_string()).or_insert(0) += 1;
    }

    fn emitted_features(&self, _z: u32, _layer: &str, _num_features: usize) {}
}
