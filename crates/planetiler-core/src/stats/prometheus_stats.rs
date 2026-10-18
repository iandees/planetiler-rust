//! Port of `stats/PrometheusStats.java`: a [`Stats`] implementation that pushes metrics to a
//! [prometheus](https://prometheus.io/) instance through a [push gateway](https://github.com/prometheus/pushgateway).
//!
//! Java uses the prometheus simpleclient library; this port keeps its own minimal registry and serializes it using the
//! prometheus text exposition format (version 0.0.4). Metric names match the Java version (including the `jvm_*`
//! thread metrics) so the same grafana dashboards work.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};

use crate::stats::counter::Counter;
use crate::stats::process_info::{self, ThreadState};
use crate::stats::stats::{LabeledSupplier, MonitoredFile, NumberSupplier, SizeSupplier, Stats};
use crate::stats::timers::Timers;

const BASE: &str = "planetiler_";
const NANOS_PER_SECOND: f64 = 1e9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetricType {
    Counter,
    Gauge,
    Histogram,
}

impl MetricType {
    fn as_str(&self) -> &'static str {
        match self {
            MetricType::Counter => "counter",
            MetricType::Gauge => "gauge",
            MetricType::Histogram => "histogram",
        }
    }
}

struct Sample {
    name: String,
    labels: Vec<(String, String)>,
    value: f64,
}

struct MetricFamily {
    name: String,
    help: String,
    kind: MetricType,
    samples: Vec<Sample>,
}

impl MetricFamily {
    fn gauge(name: impl Into<String>, help: impl Into<String>, value: f64) -> MetricFamily {
        let name = name.into();
        MetricFamily {
            samples: vec![Sample { name: name.clone(), labels: vec![], value }],
            name,
            help: help.into(),
            kind: MetricType::Gauge,
        }
    }

    fn counter(name: impl Into<String>, help: impl Into<String>, value: f64) -> MetricFamily {
        let mut family = Self::counter_family(name, help);
        family.add_counter_sample(vec![], value);
        family
    }

    fn counter_family(name: impl Into<String>, help: impl Into<String>) -> MetricFamily {
        let name = name.into();
        let name = name.strip_suffix("_total").map(str::to_string).unwrap_or(name);
        MetricFamily { name, help: help.into(), kind: MetricType::Counter, samples: vec![] }
    }

    fn add_counter_sample(&mut self, labels: Vec<(String, String)>, value: f64) {
        self.samples.push(Sample { name: format!("{}_total", self.name), labels, value });
    }

    fn write(&self, out: &mut String) {
        let _ = writeln!(out, "# HELP {} {}", self.name, escape_help(&self.help));
        let _ = writeln!(out, "# TYPE {} {}", self.name, self.kind.as_str());
        for sample in &self.samples {
            out.push_str(&sample.name);
            if !sample.labels.is_empty() {
                out.push('{');
                for (i, (k, v)) in sample.labels.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    let _ = write!(out, "{k}=\"{}\"", escape_label(v));
                }
                out.push('}');
            }
            let _ = writeln!(out, " {}", double_to_go_string(sample.value));
        }
    }
}

fn escape_help(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n")
}

fn escape_label(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\"', "\\\"").replace('\n', "\\n")
}

/// Formats a double like the Java prometheus client (`1.0`, `0.5`, `+Inf`).
fn double_to_go_string(d: f64) -> String {
    if d == f64::INFINITY {
        "+Inf".to_string()
    } else if d == f64::NEG_INFINITY {
        "-Inf".to_string()
    } else if d.is_nan() {
        "NaN".to_string()
    } else if d.fract() == 0.0 && d.abs() < 1e15 {
        format!("{d:.1}")
    } else {
        format!("{d}")
    }
}

/// Replaces characters that are not allowed in prometheus metric names with `_` (Java's
/// `Collector.sanitizeMetricName`).
pub fn sanitize_metric_name(name: &str) -> String {
    let mut out: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() || c == ':' || c == '_' { c } else { '_' }).collect();
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// A counter with labels.
struct LabeledCounter {
    name: String,
    help: String,
    label_names: Vec<&'static str>,
    values: RwLock<HashMap<Vec<String>, Arc<AtomicI64>>>,
}

impl LabeledCounter {
    fn new(name: &str, help: &str, label_names: Vec<&'static str>) -> LabeledCounter {
        LabeledCounter { name: name.into(), help: help.into(), label_names, values: RwLock::new(HashMap::new()) }
    }

    fn inc(&self, labels: Vec<String>, amount: i64) {
        if let Some(counter) = self.values.read().get(&labels) {
            counter.fetch_add(amount, Ordering::Relaxed);
            return;
        }
        self.values.write().entry(labels).or_default().fetch_add(amount, Ordering::Relaxed);
    }

    fn collect(&self) -> MetricFamily {
        let mut family = MetricFamily::counter_family(&self.name, &self.help);
        let values = self.values.read();
        let mut entries: Vec<_> = values.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (labels, value) in entries {
            let labels = self.label_names.iter().zip(labels).map(|(k, v)| (k.to_string(), v.clone())).collect();
            family.add_counter_sample(labels, value.load(Ordering::Relaxed) as f64);
        }
        family
    }
}

#[derive(Default, Clone)]
struct HistogramData {
    buckets: Vec<u64>,
    sum: f64,
    count: u64,
}

/// A histogram with a single `zoom` label.
struct Histogram {
    name: String,
    help: String,
    bounds: Vec<f64>,
    values: Mutex<BTreeMap<u32, HistogramData>>,
}

impl Histogram {
    fn observe(&self, zoom: u32, value: f64) {
        let mut values = self.values.lock();
        let data = values
            .entry(zoom)
            .or_insert_with(|| HistogramData { buckets: vec![0; self.bounds.len() + 1], ..Default::default() });
        let idx = self.bounds.iter().position(|b| value <= *b).unwrap_or(self.bounds.len());
        data.buckets[idx] += 1;
        data.sum += value;
        data.count += 1;
    }

    fn collect(&self) -> MetricFamily {
        let mut samples = vec![];
        for (zoom, data) in self.values.lock().iter() {
            let zoom = zoom.to_string();
            let mut cumulative = 0;
            for (i, count) in data.buckets.iter().enumerate() {
                cumulative += count;
                let le = self.bounds.get(i).map_or_else(|| "+Inf".to_string(), |b| double_to_go_string(*b));
                samples.push(Sample {
                    name: format!("{}_bucket", self.name),
                    labels: vec![("zoom".into(), zoom.clone()), ("le".into(), le)],
                    value: cumulative as f64,
                });
            }
            samples.push(Sample {
                name: format!("{}_count", self.name),
                labels: vec![("zoom".into(), zoom.clone())],
                value: data.count as f64,
            });
            samples.push(Sample {
                name: format!("{}_sum", self.name),
                labels: vec![("zoom".into(), zoom)],
                value: data.sum,
            });
        }
        MetricFamily { name: self.name.clone(), help: self.help.clone(), kind: MetricType::Histogram, samples }
    }
}

type Collector = Box<dyn Fn() -> Vec<MetricFamily> + Send + Sync>;

struct PushConfig {
    url: String,
    auth: Option<String>,
}

struct Pusher {
    stop: crossbeam_channel::Sender<()>,
    handle: std::thread::JoinHandle<()>,
}

/// A [`Stats`] implementation that pushes metrics to a prometheus push gateway.
pub struct PrometheusStats {
    timers: Timers,
    job: String,
    files_to_monitor: Mutex<BTreeMap<String, MonitoredFile>>,
    data_error_counters: Mutex<BTreeMap<String, i64>>,
    heap_objects_to_monitor: Mutex<BTreeMap<String, SizeSupplier>>,
    collectors: RwLock<Vec<Collector>>,
    processed_elements: LabeledCounter,
    data_errors: LabeledCounter,
    emitted_features: LabeledCounter,
    tiles_written_bytes: Histogram,
    threads: Mutex<BTreeMap<i64, ThreadState>>,
    logged_disk_error: AtomicBool,
    push_config: Option<PushConfig>,
    pusher: Mutex<Option<Pusher>>,
}

impl PrometheusStats {
    /// Constructs a new instance but does not start pushing (for tests).
    pub fn new(job: &str) -> PrometheusStats {
        Self::with_push_config(job, None)
    }

    fn with_push_config(job: &str, push_config: Option<PushConfig>) -> PrometheusStats {
        PrometheusStats {
            timers: Timers::new(),
            job: job.to_string(),
            files_to_monitor: Mutex::new(BTreeMap::new()),
            data_error_counters: Mutex::new(BTreeMap::new()),
            heap_objects_to_monitor: Mutex::new(BTreeMap::new()),
            collectors: RwLock::new(Vec::new()),
            processed_elements: LabeledCounter::new(
                &format!("{BASE}renderer_elements_processed"),
                "Number of source elements processed",
                vec!["type", "layer", "zoom"],
            ),
            data_errors: LabeledCounter::new(
                &format!("{BASE}bad_input_data"),
                "Number of data inconsistencies encountered in source data",
                vec!["type"],
            ),
            emitted_features: LabeledCounter::new(
                &format!("{BASE}renderer_features_emitted"),
                "Features enqueued for writing to feature DB",
                vec!["zoom", "layer"],
            ),
            tiles_written_bytes: Histogram {
                name: format!("{BASE}archive_tile_written_bytes"),
                help: "Written tile sizes by zoom level".into(),
                bounds: vec![1_000.0, 10_000.0, 100_000.0, 500_000.0],
                values: Mutex::new(BTreeMap::new()),
            },
            threads: Mutex::new(BTreeMap::new()),
            logged_disk_error: AtomicBool::new(false),
            push_config,
            pusher: Mutex::new(None),
        }
    }

    /// Returns a new `PrometheusStats` that pushes to `destination` every `interval` (minimum 5 seconds).
    ///
    /// `destination` may include basic auth credentials, i.e. `https://user:password@host:port`.
    pub fn create_and_start_pushing(
        destination: &str,
        job: &str,
        interval: Duration,
    ) -> anyhow::Result<Arc<PrometheusStats>> {
        let (url, auth) = split_user_info(destination)?;
        let url = format!("{}/metrics/job/{}", url.trim_end_matches('/'), url_encode(job));
        let stats = Arc::new(Self::with_push_config(job, Some(PushConfig { url, auth })));
        stats.push();
        let (stop, stopped) = crossbeam_channel::bounded::<()>(1);
        let weak: Weak<PrometheusStats> = Arc::downgrade(&stats);
        let interval = interval.max(Duration::from_secs(5));
        let handle = std::thread::Builder::new().name("prometheus-pusher".into()).spawn(move || {
            while let Err(crossbeam_channel::RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                match weak.upgrade() {
                    Some(stats) => stats.push(),
                    None => break,
                }
            }
        })?;
        *stats.pusher.lock() = Some(Pusher { stop, handle });
        Ok(stats)
    }

    /// The job ID metrics are pushed under.
    pub fn job(&self) -> &str {
        &self.job
    }

    fn push(&self) {
        let Some(config) = &self.push_config else {
            return;
        };
        let body = self.get_metrics_as_string();
        let mut request = ureq::put(&config.url).header("Content-Type", "text/plain; version=0.0.4; charset=utf-8");
        if let Some(auth) = &config.auth {
            request = request.header("Authorization", auth.as_str());
        }
        if let Err(e) = request.send(body.as_str()) {
            tracing::error!("Error pushing stats to prometheus: {e}");
        }
    }

    fn register(&self, collector: Collector) {
        self.collectors.write().push(collector);
    }

    /// Returns the full payload that would be sent to the push gateway right now.
    pub fn get_metrics_as_string(&self) -> String {
        let mut families = vec![];
        families.extend(self.thread_details());
        families.extend(self.in_progress_tasks());
        families.extend(self.file_sizes());
        families.extend(self.heap_object_sizes());
        families.push(MetricFamily {
            name: "jvm_memory_pool_post_gc_bytes_total".into(),
            help: "Memory used by each pool after last GC".into(),
            kind: MetricType::Gauge,
            samples: vec![],
        });
        families.push(self.processed_elements.collect());
        families.push(self.data_errors.collect());
        families.push(self.emitted_features.collect());
        families.push(self.tiles_written_bytes.collect());
        for collector in self.collectors.read().iter() {
            families.extend(collector());
        }
        let mut out = String::new();
        for family in families {
            family.write(&mut out);
        }
        out
    }

    fn gauge_metric(name: &str, value: f64) -> MetricFamily {
        MetricFamily::gauge(format!("{BASE}{name}"), format!("{BASE}{name} value"), value)
    }

    /// Stats on all tasks being timed through [`Stats::timers`].
    fn in_progress_tasks(&self) -> Vec<MetricFamily> {
        let mut result = vec![];
        for (name, stage) in self.timers.all() {
            let timer = &stage.timer;
            result.push(Self::gauge_metric(&format!("{name}_running"), if timer.running() { 1.0 } else { 0.0 }));
            let time = timer.elapsed();
            result.push(Self::gauge_metric(&format!("{name}_elapsed_time_seconds"), time.wall.as_secs_f64()));
            result.push(Self::gauge_metric(
                &format!("{name}_cpu_time_seconds"),
                time.cpu.unwrap_or_default().as_secs_f64(),
            ));
        }
        result
    }

    /// Stats on all file sizes being monitored through [`Stats::monitor_file`].
    fn file_sizes(&self) -> Vec<MetricFamily> {
        let files = self.files_to_monitor.lock().clone();
        if files.is_empty() {
            return vec![];
        }
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let mut results = vec![];
        for (name, file) in files {
            let name = sanitize_metric_name(&name);
            let size = (file.size_provider)();
            results.push(MetricFamily::gauge(
                format!("{BASE}file_{name}_size_bytes"),
                format!("Size of {name} in bytes"),
                size as f64,
            ));
            if file.path.exists() {
                let path = std::fs::canonicalize(&file.path).unwrap_or(file.path.clone());
                let disk = disks
                    .list()
                    .iter()
                    .filter(|d| path.starts_with(d.mount_point()))
                    .max_by_key(|d| d.mount_point().as_os_str().len());
                if let Some(disk) = disk {
                    results.push(MetricFamily::gauge(
                        format!("{BASE}file_{name}_total_space_bytes"),
                        "Total space available on disk",
                        disk.total_space() as f64,
                    ));
                    results.push(MetricFamily::gauge(
                        format!("{BASE}file_{name}_unallocated_space_bytes"),
                        "Unallocated space on disk",
                        disk.available_space() as f64,
                    ));
                    results.push(MetricFamily::gauge(
                        format!("{BASE}file_{name}_usable_space_bytes"),
                        "Usable space on disk",
                        disk.available_space() as f64,
                    ));
                } else if !self.logged_disk_error.swap(true, Ordering::Relaxed) {
                    tracing::warn!("unable to get usable space on device for {}", file.path.display());
                }
            }
        }
        results
    }

    /// Stats on all in-memory object sizes being monitored through [`Stats::monitor_in_memory_object`].
    fn heap_object_sizes(&self) -> Vec<MetricFamily> {
        self.heap_objects_to_monitor
            .lock()
            .iter()
            .map(|(name, supplier)| {
                let name = sanitize_metric_name(name);
                MetricFamily::gauge(
                    format!("{BASE}heap_object_{name}_size_bytes"),
                    format!("Bytes of memory used by {name}"),
                    supplier() as f64,
                )
            })
            .collect()
    }

    /// More detailed stats on CPU usage by thread, and process-level stats.
    fn thread_details(&self) -> Vec<MetricFamily> {
        let mut mfs = vec![
            MetricFamily::gauge(
                "jvm_available_processors",
                "Number of processors available to this process",
                process_info::get_available_processors() as f64,
            ),
            MetricFamily::gauge(
                "jvm_system_load_avg",
                "System load average over the last minute",
                process_info::get_system_load_average(),
            ),
            MetricFamily::counter(
                "process_cpu_seconds_total",
                "Total user and system CPU time spent in seconds.",
                process_info::get_process_cpu_time().unwrap_or_default().as_secs_f64(),
            ),
            MetricFamily::gauge(
                "process_resident_memory_bytes",
                "Resident memory size in bytes.",
                process_info::get_on_heap_used_memory_bytes() as f64,
            ),
        ];
        let mut cpu = MetricFamily::counter_family("jvm_thread_cpu_time_seconds", "CPU time used by each thread");
        let mut user = MetricFamily::counter_family("jvm_thread_user_time_seconds", "User time used by each thread");
        let mut threads = self.threads.lock();
        threads.extend(process_info::get_thread_stats());
        // always include the current thread so there is at least one sample
        let current = process_info::get_current_thread_state();
        threads.insert(current.id, current);
        for thread in threads.values() {
            let labels = vec![("name".to_string(), thread.name.clone()), ("id".to_string(), thread.id.to_string())];
            user.add_counter_sample(labels.clone(), thread.user_time.as_nanos() as f64 / NANOS_PER_SECOND);
            cpu.add_counter_sample(labels, thread.cpu_time.as_nanos() as f64 / NANOS_PER_SECOND);
        }
        mfs.push(cpu);
        mfs.push(user);
        mfs
    }
}

impl Stats for PrometheusStats {
    fn wrote_tile(&self, zoom: u32, bytes: usize) {
        self.tiles_written_bytes.observe(zoom, bytes as f64);
    }

    fn timers(&self) -> &Timers {
        &self.timers
    }

    fn monitored_files(&self) -> BTreeMap<String, MonitoredFile> {
        self.files_to_monitor.lock().clone()
    }

    fn monitor_file_with_size(&self, name: &str, path: PathBuf, size_provider: SizeSupplier) {
        self.files_to_monitor.lock().insert(name.to_string(), MonitoredFile { path, size_provider });
    }

    fn monitor_in_memory_object(&self, name: &str, estimate: SizeSupplier) {
        self.heap_objects_to_monitor.lock().insert(name.to_string(), estimate);
    }

    fn counter(&self, name: &str, supplier: NumberSupplier) {
        let name = format!("{BASE}{}", sanitize_metric_name(name));
        self.register(Box::new(move || vec![MetricFamily::counter(name.clone(), "", supplier())]));
    }

    fn labeled_counter(&self, name: &str, label: &str, supplier: LabeledSupplier) {
        let name = format!("{BASE}{}", sanitize_metric_name(name));
        let label = label.to_string();
        self.register(Box::new(move || {
            let mut family = MetricFamily::counter_family(name.clone(), "");
            for (key, value) in supplier() {
                family.add_counter_sample(vec![(label.clone(), key)], value as f64);
            }
            vec![family]
        }));
    }

    fn long_counter(&self, name: &str) -> Counter {
        let counter = Counter::new();
        let c = counter.clone();
        self.counter(name, Arc::new(move || c.get() as f64));
        counter
    }

    fn nano_counter(&self, name: &str) -> Counter {
        let counter = Counter::new();
        let c = counter.clone();
        self.counter(name, Arc::new(move || c.get() as f64 / NANOS_PER_SECOND));
        counter
    }

    fn gauge(&self, name: &str, supplier: NumberSupplier) {
        let name = format!("{BASE}{}", sanitize_metric_name(name));
        self.register(Box::new(move || vec![MetricFamily::gauge(name.clone(), "", supplier())]));
    }

    fn data_errors(&self) -> BTreeMap<String, i64> {
        self.data_error_counters.lock().clone()
    }

    fn processed_element(&self, elem_type: &str, layer: &str, zoom: u32) {
        self.processed_elements.inc(vec![elem_type.to_string(), layer.to_string(), zoom.to_string()], 1);
    }

    fn data_error(&self, error_code: &str) {
        *self.data_error_counters.lock().entry(error_code.to_string()).or_insert(0) += 1;
        self.data_errors.inc(vec![error_code.to_string()], 1);
    }

    fn emitted_features(&self, z: u32, layer: &str, num_features: usize) {
        self.emitted_features.inc(vec![z.to_string(), layer.to_string()], num_features as i64);
    }

    fn close(&self) {
        if let Some(pusher) = self.pusher.lock().take() {
            drop(pusher.stop);
            let _ = pusher.handle.join();
        }
        self.push();
    }
}

/// Splits basic-auth credentials out of a URL, returning the URL without them and an `Authorization` header value.
fn split_user_info(destination: &str) -> anyhow::Result<(String, Option<String>)> {
    let Some((scheme, rest)) = destination.split_once("://") else {
        anyhow::bail!("Invalid push gateway URL: {destination}");
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    if let Some((user_info, host)) = authority.rsplit_once('@') {
        let auth = user_info
            .split_once(':')
            .map(|(user, pass)| format!("Basic {}", base64_encode(format!("{user}:{pass}").as_bytes())));
        Ok((format!("{scheme}://{host}{path}"), auth))
    } else {
        Ok((destination.to_string(), None))
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(CHARS[(n >> 18) as usize & 63] as char);
        out.push(CHARS[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { CHARS[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { CHARS[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NUMBER: &str = r"[0-9\.E\-+]+$";

    fn assert_contains_stat(regex: &str, stats: &PrometheusStats) {
        assert_contains_stat_str(regex, &stats.get_metrics_as_string());
    }

    fn assert_contains_stat_str(regex: &str, stats: &str) {
        let pattern = regex::Regex::new(regex).unwrap();
        for line in stats.lines() {
            if !line.starts_with('#') && pattern.is_match(line) {
                return;
            }
        }
        panic!("could not find {regex} in:\n{stats}");
    }

    #[test]
    fn initial_stat() {
        let stats = PrometheusStats::new("job");
        let metrics = stats.get_metrics_as_string();
        for re in [
            r"^jvm_thread_cpu_time_seconds_total\{",
            r"^jvm_thread_user_time_seconds_total\{",
            r"^jvm_system_load_avg ",
            &format!("^jvm_available_processors {NUMBER}"),
        ] {
            assert_contains_stat_str(re, &metrics);
        }
    }

    #[test]
    fn timer() {
        let stats = PrometheusStats::new("job");
        let timer = stats.start_stage("task1");
        assert_contains_stat("^planetiler_task1_running 1", &stats);
        assert_contains_stat(&format!("^planetiler_task1_elapsed_time_seconds {NUMBER}"), &stats);
        assert_contains_stat(&format!("^planetiler_task1_cpu_time_seconds {NUMBER}"), &stats);
        timer.stop();
        assert_contains_stat("^planetiler_task1_running 0", &stats);
        assert_contains_stat(&format!("^planetiler_task1_elapsed_time_seconds {NUMBER}"), &stats);
        assert_contains_stat(&format!("^planetiler_task1_cpu_time_seconds {NUMBER}"), &stats);
        assert!(!stats.timers().all().get("task1").unwrap().timer.running());
    }

    #[test]
    fn gauge() {
        let stats = PrometheusStats::new("job");
        stats.gauge_value("gauge1", 1.0);
        stats.gauge("gauge2", Arc::new(|| 2.0));
        assert_contains_stat("^planetiler_gauge1 1", &stats);
        assert_contains_stat("^planetiler_gauge2 2", &stats);
    }

    #[test]
    fn processed_element() {
        let stats = PrometheusStats::new("job");
        stats.processed_element("type1", "layer1", 0);
        stats.processed_element("type1", "layer1", 0);
        stats.processed_element("type1", "layer2", 0);
        assert_contains_stat(r"^planetiler_renderer_elements_processed_total\{.*layer1.* 2", &stats);
        assert_contains_stat(r"^planetiler_renderer_elements_processed_total\{.*layer2.* 1", &stats);
    }

    #[test]
    fn data_error() {
        let stats = PrometheusStats::new("job");
        stats.data_error("err1");
        stats.data_error("err1");
        stats.data_error("err2");
        assert_contains_stat(r"^planetiler_bad_input_data_total\{.*err1.* 2", &stats);
        assert_contains_stat(r"^planetiler_bad_input_data_total\{.*err2.* 1", &stats);
    }

    #[test]
    fn emitted_features() {
        let stats = PrometheusStats::new("job");
        stats.emitted_features(0, "layer1", 2);
        stats.emitted_features(0, "layer1", 2);
        stats.emitted_features(0, "layer2", 1);
        assert_contains_stat(r"^planetiler_renderer_features_emitted_total\{.*layer1.* 4", &stats);
        assert_contains_stat(r"^planetiler_renderer_features_emitted_total\{.*layer2.* 1", &stats);
    }

    #[test]
    fn wrote_tile() {
        let stats = PrometheusStats::new("job");
        stats.wrote_tile(0, 10);
        stats.wrote_tile(0, 10_000);
        assert_contains_stat(r#"^planetiler_archive_tile_written_bytes_bucket\{.*le="1000\..* 1"#, &stats);
        assert_contains_stat(r#"^planetiler_archive_tile_written_bytes_bucket\{.*le="10000\..* 2"#, &stats);
    }

    #[test]
    fn monitor_file() {
        let dir = tempfile::tempdir().unwrap();
        let stats = PrometheusStats::new("job");
        stats.monitor_file("test", dir.path().to_path_buf());
        assert_contains_stat("^planetiler_file_test_size_bytes 0", &stats);
        std::fs::write(dir.path().join("data"), "abc").unwrap();
        assert_contains_stat("^planetiler_file_test_size_bytes [0-9]", &stats);
    }

    #[test]
    fn monitor_in_memory_object() {
        let stats = PrometheusStats::new("job");
        stats.monitor_in_memory_object("test", Arc::new(|| 10));
        assert_contains_stat("^planetiler_heap_object_test_size_bytes 10", &stats);
    }

    #[test]
    fn capture_data_errors() {
        let stats = PrometheusStats::new("job");
        assert!(stats.data_errors().is_empty());
        stats.data_error("a");
        stats.data_error("a");
        stats.data_error("b");
        assert_eq!(BTreeMap::from([("a".to_string(), 2), ("b".to_string(), 1)]), stats.data_errors());
    }

    #[test]
    fn counter() {
        let stats = PrometheusStats::new("job");
        stats.counter("counter1", Arc::new(|| 1.0));
        stats.labeled_counter(
            "counter2",
            "label",
            Arc::new(|| BTreeMap::from([("value1".to_string(), 1), ("value2".to_string(), 2)])),
        );
        let long_counter = stats.long_counter("long");
        long_counter.inc_by(100);
        assert_eq!(100, long_counter.get());
        stats.nano_counter("nanos").inc_by(500_000_000);
        assert_contains_stat("^planetiler_counter1_total 1", &stats);
        assert_contains_stat(r#"^planetiler_counter2_total\{.*label="value1".* 1"#, &stats);
        assert_contains_stat(r#"^planetiler_counter2_total\{.*label="value2".* 2"#, &stats);
        assert_contains_stat("^planetiler_long_total 100", &stats);
        assert_contains_stat("^planetiler_nanos_total 0.5", &stats);
    }

    #[test]
    fn user_info() {
        let (url, auth) = split_user_info("https://user:pass@host:9091/path").unwrap();
        assert_eq!("https://host:9091/path", url);
        assert_eq!(Some("Basic dXNlcjpwYXNz".to_string()), auth);
        let (url, auth) = split_user_info("http://host:9091").unwrap();
        assert_eq!("http://host:9091", url);
        assert_eq!(None, auth);
    }

    #[test]
    fn sanitize() {
        assert_eq!("a_b_c", sanitize_metric_name("a-b.c"));
        assert_eq!("_1a", sanitize_metric_name("1a"));
    }

    #[test]
    fn push_to_gateway() {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut content_length = 0;
            let mut auth = None;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length: ") {
                    content_length = v.parse().unwrap();
                }
                if lower.starts_with("authorization: ") {
                    auth = Some(line["authorization: ".len()..].to_string());
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            let mut stream = reader.into_inner();
            stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").unwrap();
            (request_line, auth, String::from_utf8(body).unwrap())
        });
        let stats = PrometheusStats::create_and_start_pushing(
            &format!("http://user:pass@127.0.0.1:{port}"),
            "job",
            Duration::from_secs(60),
        )
        .unwrap();
        let (request_line, auth, body) = server.join().unwrap();
        assert_eq!("PUT /metrics/job/job HTTP/1.1", request_line.trim_end());
        assert_eq!(Some("Basic dXNlcjpwYXNz".to_string()), auth);
        assert!(body.contains("jvm_available_processors"));
        // stop the pusher thread without pushing again (nothing is listening anymore)
        if let Some(pusher) = stats.pusher.lock().take() {
            drop(pusher.stop);
            pusher.handle.join().unwrap();
        }
    }
}
