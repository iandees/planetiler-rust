//! Port of `util/Downloader.java`: a utility for making external HTTP requests and downloading remote HTTP files to
//! disk.
//!
//! All HTTP requests from planetiler should go through one shared [`Downloader`] instance, which sets a consistent user
//! agent and enforces the global `download_max_requests` and `download_max_bandwidth` limits.
//!
//! Downloads resume where a previous run stopped. After downloading a file once, it won't be downloaded again unless
//! the remote file changes.
//!
//! ```no_run
//! # use planetiler_core::util::downloader::Downloader;
//! # use planetiler_core::config::planetiler_config::PlanetilerConfig;
//! # fn main() -> anyhow::Result<()> {
//! let downloader = Downloader::create(PlanetilerConfig::defaults());
//! downloader
//!     .add("natural_earth", "http://url/of/natural_earth.zip", "natural_earth.zip")?
//!     .add("osm", "http://url/of/file.osm.pbf", "file.osm.pbf")?;
//! downloader.run()?;
//! # Ok(())
//! # }
//! ```
//!
//! A [`Downloader`] is cheap to clone: clones share the request and bandwidth limits.
//!
//! As a shortcut to find the URL of a file to download from the [Geofabrik download site](https://download.geofabrik.de/),
//! use `"geofabrik:extract name"` (i.e. `"geofabrik:monaco"`). Use `"aws:latest"` to download the latest
//! `planet.osm.pbf` from the [AWS Open Data Registry](https://registry.opendata.aws/osm/), or `"overture:latest"` for
//! the latest [Overture Maps Foundation](https://overturemaps.org/) release.
//!
//! HTTP access goes through the [`Transport`] trait so tests can substitute a stub (Java overrides `sendOverNetwork`
//! in a subclass instead), see `testing::stub_downloader`.
//!
//! Differences from Java: tasks run on OS threads instead of virtual threads, and sources that aren't `http(s)` URLs
//! are only supported by [`Downloader::open_stream`].

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::config::planetiler_config::PlanetilerConfig;
use crate::reader::remote::cached_range_file::CachedRangeFile;
use crate::reader::remote::range_file::RangeFile;
use crate::stats::counter::Counter;
use crate::stats::progress_loggers::ProgressLoggers;
use crate::util::completable_futures::{Permit, Semaphore};
use crate::util::format;
use crate::util::request_planner::RequestPlanner;
use crate::util::resource_usage::ResourceUsage;
use crate::worker::{Done, SharedError};

const MAX_REDIRECTS: usize = 5;
/// Read enough of an error response's body to say what went wrong, without filling the log with an HTML error page.
const MAX_ERROR_BODY_BYTES: usize = 500;
/// Write 1 MB blocks to disk instead of 16 KB to reduce CPU usage when downloading large files on big machines.
const BUFFER_SIZE: usize = 1 << 20;

/// A range of bytes in a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

impl ByteRange {
    pub const fn new(offset: u64, length: u64) -> ByteRange {
        ByteRange { offset, length }
    }

    /// Returns the offset just past the last byte.
    pub const fn end(&self) -> u64 {
        self.offset + self.length
    }
}

impl std::fmt::Display for ByteRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bytes {}-{}", self.offset, self.end().wrapping_sub(1))
    }
}

/// One version of a remote file: its length and the `ETag` / `Last-Modified` validators the server sent for it,
/// `None` when it didn't send one. The length is -1 if the server didn't send it.
///
/// Serialized as JSON to the `.meta` file of a [`CachedRangeFile`] with the same field names as Java.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteVersion {
    pub length: i64,
    pub etag: Option<String>,
    #[serde(rename = "lastModified")]
    pub last_modified: Option<String>,
}

impl RemoteVersion {
    pub fn new(length: i64, etag: Option<&str>, last_modified: Option<&str>) -> RemoteVersion {
        RemoteVersion { length, etag: etag.map(str::to_string), last_modified: last_modified.map(str::to_string) }
    }

    /// Returns true if `other` has the same length and none of the validators both versions have differ.
    pub fn matches(&self, other: &RemoteVersion) -> bool {
        fn same(a: &Option<String>, b: &Option<String>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
        }
        self.length == other.length && same(&self.etag, &other.etag) && same(&self.last_modified, &other.last_modified)
    }
}

/// The bytes of one range request.
#[derive(Clone, PartialEq, Eq)]
pub struct RangeResponse {
    pub url: String,
    pub version: RemoteVersion,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for RangeResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RangeResponse[url={}, version={:?}, offset={}, length={}]",
            self.url,
            self.version,
            self.offset,
            self.bytes.len()
        )
    }
}

/// Errors from a [`Downloader`] that callers handle differently from plain I/O errors.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// A server answered a range request with the whole file, with `version` from its response headers (length -1 if
    /// it didn't send its length).
    #[error("{url} does not support range requests")]
    RangesNotSupported { url: String, version: RemoteVersion },
    /// A remote file changed on the server while reading it in parts.
    #[error(
        "{url} changed on the server while reading it ({details}), re-run to read the new version. If this keeps happening, use --download-fail-if-changed=false"
    )]
    RemoteFileChanged { url: String, details: String },
    /// An unexpected HTTP response code from the server. 429 and 5xx (but not 501) errors are
    /// [retryable](DownloadError::retryable).
    #[error("Bad response from {url}: {response}")]
    BadResponse { url: String, response: String, status: u16, retry_after: Option<Duration> },
}

impl DownloadError {
    /// Returns true if trying the request again might work.
    pub fn retryable(&self) -> bool {
        match self {
            // 501 Not Implemented won't change on retry
            DownloadError::BadResponse { status, .. } => *status == 429 || (*status >= 500 && *status != 501),
            _ => false,
        }
    }
}

/// Returns the [`DownloadError`] that `error` is or was caused by, if any.
pub fn download_error(error: &anyhow::Error) -> Option<&DownloadError> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<DownloadError>()
            .or_else(|| cause.downcast_ref::<SharedError>().and_then(|shared| download_error(&shared.0)))
    })
}

/// Failure to store bytes that were fetched. Not worth fetching them again.
#[derive(Debug, thiserror::Error)]
#[error("Error storing downloaded bytes: {0}")]
pub struct StoreError(pub io::Error);

fn is_retryable(error: &anyhow::Error) -> bool {
    if let Some(download_error) = download_error(error) {
        return download_error.retryable();
    }
    if error.chain().any(|cause| cause.is::<StoreError>()) {
        return false;
    }
    error.chain().any(|cause| {
        cause.is::<io::Error>() || cause.downcast_ref::<SharedError>().is_some_and(|shared| is_retryable(&shared.0))
    })
}

/// Receiver for bytes fetched from a remote file: gets the bytes and their position in the file.
pub type Sink<'a> = &'a mut dyn FnMut(u64, &[u8]) -> io::Result<()>;

/// An HTTP request to send: a GET, or a POST if `body` is set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl Request {
    pub fn get(url: &str) -> Request {
        Request { url: url.to_string(), headers: vec![], body: None }
    }

    pub fn post(url: &str, body: &str) -> Request {
        Request { url: url.to_string(), headers: vec![], body: Some(body.to_string()) }
    }

    /// Returns the value of the header `name`, which is case-insensitive.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// An HTTP response. Dropping it closes the body and lets the next request start.
pub struct Response {
    pub url: String,
    pub status: u16,
    /// Header names in lowercase.
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
}

impl Response {
    pub fn new(url: &str, status: u16, headers: Vec<(String, String)>, body: Box<dyn Read + Send>) -> Response {
        Response {
            url: url.to_string(),
            status,
            headers: headers.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect(),
            body,
        }
    }

    /// Returns the value of the header `name`, which is case-insensitive.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn content_range(&self) -> Option<(u64, u64, u64)> {
        // Content-Range: bytes <start>-<end>/<length>
        let value = self.header("content-range")?.strip_prefix("bytes ")?;
        let (range, length) = value.split_once('/')?;
        let (start, end) = range.split_once('-')?;
        Some((start.parse().ok()?, end.parse().ok()?, length.parse().ok()?))
    }

    /// Returns the range of the file in a 206 response body.
    fn response_bytes(&self) -> anyhow::Result<ByteRange> {
        if self.status == 200 {
            return Err(DownloadError::RangesNotSupported { url: self.url.clone(), version: self.version() }.into());
        }
        match self.content_range() {
            Some((start, end, _)) => Ok(ByteRange::new(start, end - start + 1)),
            None => bail!("Bad Content-Range from {}: {}", self.url, self.header("content-range").unwrap_or("")),
        }
    }

    /// Returns the version of the file in this response, with length -1 if the server didn't send it.
    fn version(&self) -> RemoteVersion {
        let length = if self.status == 200 {
            self.header("content-length").and_then(|v| v.trim().parse().ok()).unwrap_or(-1)
        } else {
            self.content_range().map_or(-1, |(_, _, length)| length as i64)
        };
        RemoteVersion {
            length,
            etag: self.header("etag").map(str::to_string),
            last_modified: self.header("last-modified").map(str::to_string),
        }
    }

    /// Passes `length` bytes of the body, which start at `position` of the file, to `sink`.
    fn read_into(&mut self, mut position: u64, length: u64, sink: Sink<'_>) -> anyhow::Result<()> {
        let mut buffer = vec![0u8; (BUFFER_SIZE as u64).min(length) as usize];
        let end = position + length;
        while position < end {
            let wanted = (buffer.len() as u64).min(end - position) as usize;
            let read = read_up_to(&mut self.body, &mut buffer[..wanted])?;
            if read > 0 {
                accept(sink, position, &buffer[..read])?;
                position += read as u64;
            }
            if read < wanted {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("Unexpected EOF at {position}/{end} of {}", self.url),
                )
                .into());
            }
        }
        Ok(())
    }

    /// Passes the rest of the body, which starts at `position` of the file, to `sink`; returns its length.
    fn read_to_end(&mut self, mut position: u64, sink: Sink<'_>) -> anyhow::Result<u64> {
        let start = position;
        let mut buffer = vec![0u8; BUFFER_SIZE];
        loop {
            let read = read_up_to(&mut self.body, &mut buffer)?;
            if read == 0 {
                return Ok(position - start);
            }
            accept(sink, position, &buffer[..read])?;
            position += read as u64;
        }
    }
}

/// Reads until `buffer` is full or the end of the stream.
fn read_up_to(reader: &mut dyn Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buffer.len() {
        match reader.read(&mut buffer[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

fn accept(sink: Sink<'_>, position: u64, bytes: &[u8]) -> anyhow::Result<()> {
    // a failure to store the bytes is no reason to fetch them again
    sink(position, bytes).map_err(|e| StoreError(e).into())
}

/// Sends HTTP requests; the only thing that talks to the network, replaceable for tests.
pub trait Transport: Send + Sync {
    /// Sends `request` without following redirects, failing if connecting or any read takes longer than
    /// `http_timeout`.
    fn send(&self, request: &Request) -> io::Result<Response>;

    /// Waits `wait` before retrying a request; tests override it to retry right away.
    fn retry_sleep(&self, wait: Duration) {
        std::thread::sleep(wait);
    }
}

/// [`Transport`] that makes real HTTP requests with `ureq`, using timeouts and user agent from the config.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new(config: &PlanetilerConfig) -> UreqTransport {
        UreqTransport { agent: agent(config, 0) }
    }
}

fn agent(config: &PlanetilerConfig, max_redirects: u32) -> ureq::Agent {
    let max_connections = config.download_max_requests.max(config.download_threads).max(10);
    ureq::Agent::config_builder()
        .timeout_connect(Some(config.http_timeout))
        .timeout_recv_response(Some(config.http_timeout))
        .user_agent(config.http_user_agent.as_str())
        .max_redirects(max_redirects)
        .max_idle_connections(max_connections)
        .max_idle_connections_per_host(max_connections)
        .http_status_as_error(false)
        .build()
        .into()
}

impl Transport for UreqTransport {
    fn send(&self, request: &Request) -> io::Result<Response> {
        let response = match &request.body {
            Some(body) => {
                let mut builder = self.agent.post(&request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(name, value);
                }
                builder.send(body.as_str())
            }
            None => {
                let mut builder = self.agent.get(&request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(name, value);
                }
                builder.call()
            }
        }
        .map_err(|e| match e {
            ureq::Error::Io(e) => e,
            other => io::Error::other(other),
        })?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| Some((name.as_str().to_string(), value.to_str().ok()?.to_string())))
            .collect();
        Ok(Response::new(&request.url, status, headers, Box::new(response.into_body().into_reader())))
    }
}

/// Resolves a `Location` header relative to the URL that returned it.
fn resolve_location(base: &str, location: &str) -> String {
    if location.contains("://") {
        location.to_string()
    } else if let Some(path) = location.strip_prefix('/') {
        let scheme_end = base.find("://").map_or(0, |i| i + 3);
        let host_end = base[scheme_end..].find('/').map_or(base.len(), |i| i + scheme_end);
        format!("{}/{path}", &base[..host_end])
    } else {
        let dir_end = base.rfind('/').map_or(base.len(), |i| i + 1);
        format!("{}{location}", &base[..dir_end])
    }
}

/// Returns how long a `Retry-After` header value (seconds or an HTTP date, RFC 9110 10.2.3) says to wait from
/// `now_epoch_seconds`, or `None` if it can't be parsed.
pub fn parse_retry_after(value: &str, now_epoch_seconds: i64) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some(Duration::from_secs((date.timestamp() - now_epoch_seconds).max(0) as u64))
}

/// A resource to download.
pub struct ResourceToDownload {
    pub id: String,
    pub url: String,
    pub output: PathBuf,
    length: OnceLock<u64>,
    length_done: Done,
    progress: Counter,
}

impl ResourceToDownload {
    pub fn new(id: &str, url: &str, output: impl Into<PathBuf>) -> Arc<ResourceToDownload> {
        Arc::new(ResourceToDownload {
            id: id.to_string(),
            url: url.to_string(),
            output: output.into(),
            length: OnceLock::new(),
            length_done: Done::new(),
            progress: Counter::new(),
        })
    }

    /// Number of bytes downloaded so far.
    pub fn bytes_downloaded(&self) -> i64 {
        self.progress.get()
    }

    /// Size of the resource, once known (0 if it was already up-to-date).
    pub fn length(&self) -> Option<u64> {
        self.length.get().copied()
    }

    fn set_length(&self, length: u64) {
        let _ = self.length.set(length);
        self.length_done.complete(Ok(()));
    }
}

/// Limits throughput to a fixed number of bytes per second across all threads.
struct RateLimiter {
    bytes_per_second: f64,
    next_free: Mutex<Instant>,
}

impl RateLimiter {
    fn acquire(&self, bytes: usize) {
        let wait = {
            let mut next_free = self.next_free.lock();
            let now = Instant::now();
            let start = (*next_free).max(now);
            *next_free = start + Duration::from_secs_f64(bytes as f64 / self.bytes_per_second);
            start - now
        };
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

struct Inner {
    config: PlanetilerConfig,
    transport: Arc<dyn Transport>,
    planner: RequestPlanner,
    rate_limiter: Option<RateLimiter>,
    global_request_limiter: Arc<Semaphore>,
    disk_space_check: ResourceUsage,
    to_download: Mutex<Vec<Arc<ResourceToDownload>>>,
}

/// A response body that applies the bandwidth limit and frees its request slot when dropped.
struct LimitedReader {
    inner: Box<dyn Read + Send>,
    downloader: Arc<Inner>,
    _permit: Permit,
}

impl Read for LimitedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read > 0
            && let Some(limiter) = &self.downloader.rate_limiter
        {
            limiter.acquire(read);
        }
        Ok(read)
    }
}

/// Downloads files to disk and makes range requests over HTTP.
#[derive(Clone)]
pub struct Downloader {
    inner: Arc<Inner>,
}

impl Downloader {
    /// Returns a new downloader that makes real HTTP requests.
    pub fn create(config: PlanetilerConfig) -> Downloader {
        let max_request_bytes = config.download_chunk_size_mb * 1_000_000;
        let transport = Arc::new(UreqTransport::new(&config));
        Self::with_transport(config, max_request_bytes, transport)
    }

    /// Returns a new downloader that uses `transport` to send requests, making requests of at most
    /// `max_request_bytes`.
    pub fn with_transport(
        config: PlanetilerConfig,
        max_request_bytes: u64,
        transport: Arc<dyn Transport>,
    ) -> Downloader {
        let rate_limiter = (config.download_max_bandwidth > 0.0).then(|| RateLimiter {
            bytes_per_second: config.download_max_bandwidth,
            next_free: Mutex::new(Instant::now()),
        });
        Downloader {
            inner: Arc::new(Inner {
                planner: RequestPlanner::new(config.download_threads, max_request_bytes.max(1)),
                global_request_limiter: Arc::new(Semaphore::new(
                    config.download_max_requests.max(config.download_threads),
                )),
                rate_limiter,
                transport,
                disk_space_check: ResourceUsage::new("download"),
                to_download: Mutex::new(vec![]),
                config,
            }),
        }
    }

    /// Returns the config this downloader was created with.
    pub fn config(&self) -> &PlanetilerConfig {
        &self.inner.config
    }

    /// Returns the planner that decides which requests to make to fetch parts of a remote file.
    pub fn planner(&self) -> &RequestPlanner {
        &self.inner.planner
    }

    /// Returns an input stream reading `url` with a GET request, following redirects and retrying failures to
    /// connect. `file:` URLs are read from local disk.
    pub fn open_stream(&self, url: &str) -> anyhow::Result<Box<dyn Read + Send>> {
        if let Some(path) = url.strip_prefix("file://") {
            // local file:///... don't need retries or request limiting
            return Ok(Box::new(std::fs::File::open(path).with_context(|| format!("opening {url}"))?));
        }
        let response =
            self.with_retries(&format!("fetching {url}"), || 0, || self.get_following_redirects(url, None, None))?;
        Ok(response.body)
    }

    /// Reads the full body of `url` as a string.
    pub fn fetch_string(&self, url: &str) -> anyhow::Result<String> {
        let mut result = String::new();
        self.open_stream(url)?.read_to_string(&mut result)?;
        Ok(result)
    }

    /// Adds a new resource to download but does not start downloading it until [`Downloader::run`] is called.
    ///
    /// Only fetches files or parts of files that haven't been fetched yet.
    ///
    /// `id` is a short name to use for this download when logging progress. `url` is the external resource to fetch,
    /// `aws:latest` (for the latest planet .osm.pbf), `overture:latest` (for the latest Overture Maps release) or
    /// `geofabrik:extract-name` as a shortcut to look up a `.osm.pbf` Geofabrik extract URL by partial match on area
    /// name. `output` is where to download the file to.
    pub fn add(&self, id: &str, url: &str, output: impl Into<PathBuf>) -> anyhow::Result<&Downloader> {
        let url = if let Some(query) = url.strip_prefix("geofabrik:") {
            crate::util::geofabrik::get_download_url(query, self)?
        } else if let Some(query) = url.strip_prefix("aws:") {
            crate::util::aws_osm::AwsOsm::osm_pds().get_download_url(query, self)?
        } else if let Some(query) = url.strip_prefix("overture:") {
            crate::util::aws_osm::AwsOsm::overture().get_download_url(query, self)?
        } else {
            url.to_string()
        };
        self.inner.to_download.lock().push(ResourceToDownload::new(id, &url, output));
        Ok(self)
    }

    /// Starts downloading all resources added since the last call in parallel, logging progress until complete.
    /// Returns an error after all resources finish if any failed.
    pub fn run(&self) -> anyhow::Result<()> {
        let batch = std::mem::take(&mut *self.inner.to_download.lock());
        if batch.is_empty() {
            return Ok(());
        }
        let mut dones = vec![];
        for resource in &batch {
            let done = Done::new();
            dones.push(done.clone());
            let (this, resource) = (self.clone(), resource.clone());
            std::thread::Builder::new().name(format!("download-{}", resource.id)).spawn(move || {
                crate::util::log_util::set_stage_with_parent(Some("download"), &resource.id);
                match this.download_if_necessary(&resource) {
                    Ok(()) => done.complete(Ok(())),
                    Err(e) => done.fail(e),
                };
            })?;
        }
        // wait for all downloads to finish (even if one fails) before reporting errors
        let all = Done::new();
        {
            let remaining = Arc::new(std::sync::atomic::AtomicUsize::new(dones.len()));
            let first_error = Arc::new(Mutex::new(None));
            for done in &dones {
                let (all, remaining, first_error) = (all.clone(), remaining.clone(), first_error.clone());
                done.on_complete(move |r| {
                    if let Err(e) = r {
                        first_error.lock().get_or_insert_with(|| e.clone());
                    }
                    if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                        all.complete(first_error.lock().take().map_or(Ok(()), Err));
                    }
                });
            }
        }

        let mut grouped: BTreeMap<String, Vec<Arc<ResourceToDownload>>> = BTreeMap::new();
        for resource in &batch {
            grouped.entry(resource.id.clone()).or_default().push(resource.clone());
        }
        let mut loggers = ProgressLoggers::create();
        for (id, group) in grouped {
            let mut size = 0;
            for item in &group {
                match item.length_done.wait_timeout(self.inner.config.max_http_request_duration()) {
                    Some(Ok(())) => size += item.length().unwrap_or(0),
                    Some(Err(e)) => return Err(e.context(format!("Error getting size of {}", item.url))),
                    None => bail!("Timed out getting size of {}", item.url),
                }
            }
            loggers = loggers.add_storage_rate_percent_counter(
                &id,
                size as i64,
                move || group.iter().map(|r| r.bytes_downloaded()).sum(),
                true,
            );
        }
        loggers.add_str(" ").add_process_stats().await_and_log(&all, self.inner.config.log_interval)
    }

    /// Downloads `resource` unless the file on disk is already a complete, up-to-date copy of the remote file,
    /// resuming a partial download from a previous run.
    pub fn download_if_necessary(&self, resource: &ResourceToDownload) -> anyhow::Result<()> {
        let result = self.download(resource);
        if let Err(e) = &result {
            resource.length_done.fail(anyhow!("{e:#}"));
            tracing::error!("Error downloading {} to {}: {e:#}", resource.url, resource.output.display());
        }
        result
    }

    fn download(&self, resource: &ResourceToDownload) -> anyhow::Result<()> {
        match self.download_ranges(resource) {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!("Finished downloading {} to {}", resource.url, resource.output.display());
            }
            Err(e) if matches!(download_error(&e), Some(DownloadError::RangesNotSupported { .. })) => {
                self.download_whole_resource(resource)?;
                tracing::info!("Finished downloading {} to {}", resource.url, resource.output.display());
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Returns true if the file was up-to-date already.
    fn download_ranges(&self, resource: &ResourceToDownload) -> anyhow::Result<bool> {
        let output = &resource.output;
        if CachedRangeFile::is_complete_copy(output, &resource.url, true, self)? {
            tracing::info!("Skipping {}: {} already up-to-date", resource.id, output.display());
            resource.set_length(0);
            return Ok(true);
        }
        let resuming = CachedRangeFile::is_partial(output);
        // use CachedRangeFile for full downloads so that:
        // 1) full downloads are resumable
        // 2) switching from cached to full download skips bytes already downloaded (and vice versa if full was
        //    canceled)
        let file = CachedRangeFile::open(output, &resource.url, 0, self)?;
        let missing = file.bytes_missing();
        // progress of a resumed download continues from what previous runs fetched
        resource.progress.inc_by(file.length_bytes() as i64 - missing as i64);
        resource.set_length(file.length_bytes());
        let redirect_info =
            if file.url() == resource.url { String::new() } else { format!(" (redirected to {})", file.url()) };
        let resume_info = if resuming {
            format!(
                " (resuming, {}B of {}B left)",
                format::storage(missing as i64, false),
                format::storage(file.length_bytes() as i64, false)
            )
        } else {
            String::new()
        };
        tracing::info!("Downloading {}{redirect_info} to {}{resume_info}", resource.url, output.display());
        self.inner.disk_space_check.add_disk(Some(output), missing, &resource.id);
        self.inner.disk_space_check.check_against_limits(self.inner.config.force, false)?;
        let counter = resource.progress.clone();
        file.fetch_missing(Arc::new(move |bytes| counter.inc_by(bytes as i64)))?;
        file.close()?;
        Ok(false)
    }

    /// Downloads `url` to `output` with one GET request, for servers that don't support range requests; the
    /// download starts over if interrupted.
    pub fn download_whole(&self, url: &str, output: &Path) -> anyhow::Result<()> {
        let name = output.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        self.download_whole_resource(&ResourceToDownload::new(&name, url, output))?;
        tracing::info!("Finished downloading {url} to {}", output.display());
        Ok(())
    }

    fn download_whole_resource(&self, resource: &ResourceToDownload) -> anyhow::Result<()> {
        tracing::info!(
            "{} does not support range requests, downloading it to {} in one request that can't resume",
            resource.url,
            resource.output.display()
        );
        self.with_retries(
            &format!("downloading {}", resource.url),
            || 0,
            || {
                let mut written = 0u64;
                let mut response = self.get_following_redirects(&resource.url, None, None)?;
                let version = response.version();
                // servers that stream generated content may not send a Content-Length, so progress gets logged
                // without a total
                resource.set_length(version.length.max(0) as u64);
                let file = CachedRangeFile::create(&resource.output, &version)?;
                let result = (|| {
                    let mut sink = |position: u64, bytes: &[u8]| {
                        file.write(position, bytes)?;
                        written += bytes.len() as u64;
                        resource.progress.inc_by(bytes.len() as i64);
                        Ok(())
                    };
                    if version.length >= 0 {
                        response.read_into(0, version.length as u64, &mut sink)
                    } else {
                        // no Content-Length: read until the body ends, then record its length
                        let length = response.read_to_end(0, &mut sink)?;
                        file.set_length(length as i64)?;
                        Ok(())
                    }
                })();
                let closed = file.close();
                if let Err(e) = result {
                    resource.progress.inc_by(-(written as i64));
                    return Err(e);
                }
                closed?;
                Ok(())
            },
        )
    }

    /// Fetches the last `length` bytes of `url` (or all of it if it is shorter) with a range request.
    ///
    /// Fails with [`DownloadError::RangesNotSupported`] if the server does not support range requests.
    pub fn fetch_tail(&self, url: &str, length: usize) -> anyhow::Result<RangeResponse> {
        match self.request_range(url, &format!("bytes=-{length}")) {
            Ok(response) => Ok(response),
            Err(e) => {
                // GitHub release downloads answer suffix ranges with 501 Not Implemented
                let unsupported = match download_error(&e) {
                    Some(DownloadError::RangesNotSupported { .. }) => true,
                    Some(DownloadError::BadResponse { status, .. }) => *status == 501,
                    _ => false,
                };
                if !unsupported {
                    return Err(e);
                }
                // some servers don't support suffix ranges: get the file's length from its first byte, then ask
                // for the tail
                let file_length = self.fetch_range(url, ByteRange::new(0, 1))?.version.length.max(0) as u64;
                // if server doesn't support range requests at all, this will fail hard with RangesNotSupported
                let start = file_length.saturating_sub(length as u64);
                self.fetch_range(url, ByteRange::new(start, file_length - start))
            }
        }
    }

    /// Fetches `range` of `url` with one range request, following redirects.
    ///
    /// Fails with [`DownloadError::RangesNotSupported`] if the server does not support range requests.
    pub fn fetch_range(&self, url: &str, range: ByteRange) -> anyhow::Result<RangeResponse> {
        self.request_range(url, &format!("bytes={}-{}", range.offset, range.end() - 1))
    }

    /// Returns the bytes of one GET request for `range` of `url`, read into memory.
    fn request_range(&self, url: &str, range: &str) -> anyhow::Result<RangeResponse> {
        self.with_retries(
            &format!("fetching {range} of {url}"),
            || 0,
            || {
                let mut response = self.get_following_redirects(url, Some(range), None)?;
                let content_range = response.response_bytes()?;
                let mut bytes = vec![0u8; usize::try_from(content_range.length)?];
                response.read_into(content_range.offset, bytes.len() as u64, &mut |position, chunk| {
                    let start = (position - content_range.offset) as usize;
                    bytes[start..start + chunk.len()].copy_from_slice(chunk);
                    Ok(())
                })?;
                Ok(RangeResponse {
                    url: response.url.clone(),
                    version: response.version(),
                    offset: content_range.offset,
                    bytes,
                })
            },
        )
    }

    /// Fetches `range` of the remote file at `url` with one range request, passing the bytes to `sink` in order as
    /// they arrive, on the calling thread. A retry after a dropped connection continues from the next byte not
    /// passed to `sink`.
    ///
    /// Fails with [`DownloadError::RemoteFileChanged`] if the file is no longer `expected`.
    pub fn fetch(
        &self,
        url: &str,
        expected: &RemoteVersion,
        range: ByteRange,
        sink: &mut (dyn FnMut(u64, &[u8]) -> io::Result<()> + Send),
    ) -> anyhow::Result<()> {
        let bytes_done = AtomicU64::new(0);
        self.with_retries(
            &format!("fetching {range} of {url}"),
            || bytes_done.load(Ordering::Relaxed),
            || {
                let done = bytes_done.load(Ordering::Relaxed);
                let remaining = ByteRange::new(range.offset + done, range.length - done);
                let mut response = self.get_following_redirects(
                    url,
                    Some(&format!("bytes={}-{}", remaining.offset, range.end() - 1)),
                    Some(expected),
                )?;
                let content_range = response.response_bytes()?;
                if content_range != remaining {
                    bail!("Requested {remaining} of {url} but got {content_range}");
                }
                // double check precondition in case the server does not honor it or the file has no validators
                if !self.is_same_version(expected, &response.version()) {
                    return Err(DownloadError::RemoteFileChanged {
                        url: url.to_string(),
                        details: format!("{expected:?} -> {:?}", response.version()),
                    }
                    .into());
                }
                response.read_into(remaining.offset, remaining.length, &mut |position, chunk| {
                    sink(position, chunk)?;
                    bytes_done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    Ok(())
                })
            },
        )
    }

    /// Returns true if `actual` is the same version of a file as `expected` unless the user explicitly sets
    /// `download_fail_if_changed=false`.
    pub fn is_same_version(&self, expected: &RemoteVersion, actual: &RemoteVersion) -> bool {
        if self.inner.config.download_fail_if_changed {
            expected.matches(actual)
        } else {
            expected.length == actual.length
        }
    }

    /// Starts `task`, which makes requests through this downloader, on a thread of its own, so that tasks that
    /// mostly wait for the network can run at once. The returned handle fails with whatever `task` fails with.
    pub fn spawn(&self, task: impl FnOnce() -> anyhow::Result<()> + Send + 'static) -> Done {
        let done = Done::new();
        let handle = done.clone();
        let spawned = std::thread::Builder::new().name("download-task".to_string()).spawn(move || {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(task)) {
                Ok(Ok(())) => handle.complete(Ok(())),
                Ok(Err(e)) => handle.fail(e),
                Err(panic) => handle.fail(crate::worker::panic_to_error(panic)),
            };
        });
        if let Err(e) = spawned {
            done.fail(anyhow!("Unable to start thread: {e}"));
        }
        done
    }

    /// Sends `request` without following redirects and returns what `reader` reads from the body of a successful
    /// response, retrying like the other requests; reading the body gets retried too, since a connection can drop
    /// part way through it.
    pub fn send_with_retries<T>(
        &self,
        request: &Request,
        reader: impl Fn(&mut dyn Read) -> io::Result<T>,
    ) -> anyhow::Result<T> {
        self.with_retries(
            &format!("sending request to {}", request.url),
            || 0,
            || {
                let mut response = self.send_with_limits(request)?;
                if response.status / 100 != 2 {
                    return Err(bad_response(&mut response).into());
                }
                Ok(reader(&mut response.body)?)
            },
        )
    }

    /// Runs `attempt` until it succeeds or fails with something that isn't worth retrying. An attempt that failed
    /// after `progress` went up gets retried right away and the retries start over.
    fn with_retries<T>(
        &self,
        what: &str,
        progress: impl Fn() -> u64,
        mut attempt: impl FnMut() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut retry = 0i64;
        loop {
            let progress_before = progress();
            match attempt() {
                Ok(result) => return Ok(result),
                Err(e) => {
                    let retries_remaining = self.inner.config.http_retries as i64 - retry;
                    if !is_retryable(&e) {
                        return Err(e);
                    } else if progress() > progress_before {
                        // if we downloaded some bytes then the initial request succeeded and the server closed the
                        // idle connection; in that case, restart the request where we left off and don't count
                        // against our retries
                        retry = 0;
                    } else if retries_remaining <= 0 {
                        return Err(e);
                    } else {
                        self.wait_before_retry(what, retries_remaining, &e);
                        retry += 1;
                    }
                }
            }
        }
    }

    fn wait_before_retry(&self, what: &str, retries_remaining: i64, error: &anyhow::Error) {
        let config = &self.inner.config;
        let mut wait = config.http_retry_wait;
        let bad_response = download_error(error).filter(|e| matches!(e, DownloadError::BadResponse { .. }));
        let retry_after = match bad_response {
            Some(DownloadError::BadResponse { retry_after, .. }) => *retry_after,
            _ => None,
        };
        let mut reason = "";
        if let Some(retry_after) = retry_after {
            let capped = retry_after > config.http_retry_max_wait;
            let asked = if capped { config.http_retry_max_wait } else { retry_after };
            if asked > wait {
                wait = asked;
                reason = if capped { " (--http-retry-max-wait)" } else { " as the server asked" };
            }
        }
        let cause = match bad_response {
            Some(DownloadError::BadResponse { response, .. }) => format!("server responded: {response}"),
            _ => format!("{error:#}"),
        };
        tracing::warn!(
            "Error {what}, retrying in {}{reason}, {cause}, {retries_remaining} {} left",
            format::duration(wait),
            if retries_remaining == 1 { "retry" } else { "retries" }
        );
        self.inner.transport.retry_sleep(wait);
    }

    /// Sends a GET request for `range` of `url` (the whole file if `None`), following redirects, with
    /// `ETag`/`Last-Modified` preconditions, and returns a 200 or 206 response.
    fn get_following_redirects(
        &self,
        url: &str,
        range: Option<&str>,
        expected: Option<&RemoteVersion>,
    ) -> anyhow::Result<Response> {
        let mut url = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let mut request = Request::get(&url);
            if let Some(range) = range {
                request.headers.push(("Range".to_string(), range.to_string()));
            }
            if let Some(expected) = expected
                && self.inner.config.download_fail_if_changed
            {
                add_preconditions(&mut request, expected);
            }
            let mut response = self.send_with_limits(&request)?;
            let status = response.status;
            if status == 200 || status == 206 {
                return Ok(response);
            }
            if (300..400).contains(&status) {
                let location = response
                    .header("location")
                    .ok_or_else(|| anyhow!("Received {status} but no location header from {}", response.url))?;
                url = resolve_location(&url, location);
            } else if status == 412 {
                return Err(DownloadError::RemoteFileChanged {
                    url,
                    details: "this file changed on the server mid-download".to_string(),
                }
                .into());
            } else {
                return Err(bad_response(&mut response).into());
            }
        }
        bail!("Exceeded {MAX_REDIRECTS} redirects for {url}")
    }

    /// Sends `request` once it is one of the `download_max_requests` requests in flight, returning a response whose
    /// body counts against the bandwidth limit and must be dropped to let the next request start.
    fn send_with_limits(&self, request: &Request) -> io::Result<Response> {
        let permit = self.inner.global_request_limiter.acquire_owned();
        let response = self.inner.transport.send(request)?;
        Ok(Response {
            body: Box::new(LimitedReader { inner: response.body, downloader: self.inner.clone(), _permit: permit }),
            ..response
        })
    }
}

fn add_preconditions(request: &mut Request, expected: &RemoteVersion) {
    // make the server reject the request if the file changed instead of returning bytes from the new version
    // weak etags (W/"...") only guarantee equivalent content not identical bytes, so don't rely on them
    match (&expected.etag, &expected.last_modified) {
        (Some(etag), _) if !etag.starts_with("W/") => request.headers.push(("If-Match".to_string(), etag.clone())),
        (_, Some(last_modified)) => {
            request.headers.push(("If-Unmodified-Since".to_string(), last_modified.clone()));
        }
        _ => {}
    }
}

/// Returns the error for an error `response`, with the start of its body that says what went wrong.
fn bad_response(response: &mut Response) -> DownloadError {
    let mut body = vec![];
    let body = match response.body.by_ref().take(MAX_ERROR_BODY_BYTES as u64).read_to_end(&mut body) {
        Ok(_) => String::from_utf8_lossy(&body).split_whitespace().collect::<Vec<_>>().join(" "),
        Err(e) => format!("error reading body: {e}"),
    };
    let status = response.status;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let retry_after = response.header("retry-after").and_then(|value| parse_retry_after(value, now));
    DownloadError::BadResponse {
        url: response.url.clone(),
        response: format!(
            "{status}{}{}",
            retry_after.map(|d| format!(" (Retry-After: {})", format::duration(d))).unwrap_or_default(),
            if body.is_empty() { String::new() } else { format!(" {body}") }
        ),
        status,
        retry_after,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::arguments::Arguments;
    use crate::testing::stub_downloader::StubDownloader;

    const RANGE_BYTES: &[u8] = b"0123456789";

    fn config() -> PlanetilerConfig {
        PlanetilerConfig::defaults()
    }

    fn config_with(pairs: &[(&str, &str)]) -> PlanetilerConfig {
        PlanetilerConfig::from(&Arguments::of_pairs(pairs.iter().copied())).unwrap()
    }

    fn string(response: &RangeResponse) -> String {
        String::from_utf8(response.bytes.clone()).unwrap()
    }

    fn resource(url: &str, dest: &Path) -> Arc<ResourceToDownload> {
        ResourceToDownload::new("resource", url, dest)
    }

    fn download(downloader: &Downloader, url: &str, dest: &Path) -> anyhow::Result<()> {
        downloader.download_if_necessary(&resource(url, dest))
    }

    fn ranges(downloader: &StubDownloader) -> Vec<Option<String>> {
        downloader.requests().into_iter().map(|r| r.range).collect()
    }

    fn some(ranges: &[&str]) -> Vec<Option<String>> {
        ranges.iter().map(|r| Some(r.to_string())).collect()
    }

    fn fetch_into(
        downloader: &Downloader,
        version: &RemoteVersion,
        offset: u64,
        result: &mut [u8],
    ) -> anyhow::Result<()> {
        let len = result.len();
        downloader.fetch("http://url", version, ByteRange::new(offset, len as u64), &mut |position, chunk| {
            let start = (position - offset) as usize;
            result[start..start + chunk.len()].copy_from_slice(chunk);
            Ok(())
        })
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn download_with_and_without_ranges_and_redirects() {
        for (supports_ranges, redirects) in [(true, 0), (true, 1), (true, 4), (false, 0), (false, 2)] {
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("out");
            let url = "http://url";
            let initial_url = if redirects > 0 { format!("{url}#{redirects}") } else { url.to_string() };
            let downloader = StubDownloader::new(config());
            for i in (1..=redirects).rev() {
                let target = if i == 1 { url.to_string() } else { format!("{url}#{}", i - 1) };
                downloader.redirect(&format!("{url}#{i}"), &target);
            }
            if !supports_ranges {
                downloader.ignore_ranges();
            }

            // fails if no data
            assert!(download(&downloader, &initial_url, &dest).is_err());
            assert!(!dest.exists());

            // succeeds with data
            downloader.put(url, b"0123456789");
            download(&downloader, &initial_url, &dest).unwrap();
            assert_eq!("0123456789", read(&dest));
            assert!(!CachedRangeFile::is_partial(&dest));

            // does not download again if the remote file is the same
            downloader.clear_requests();
            download(&downloader, &initial_url, &dest).unwrap();
            assert_eq!("0123456789", read(&dest));
            if supports_ranges {
                // only the request that checks the remote file's version, through each redirect
                assert_eq!(vec![Some("bytes=0-0".to_string()); redirects + 1], ranges(&downloader));
            } else {
                // the whole file comes back for the requests that check its version, but none downloads it
                assert!(!ranges(&downloader).contains(&None), "{:?}", ranges(&downloader));
            }

            // downloads again if the remote file changes
            downloader.put(url, b"54321");
            download(&downloader, &initial_url, &dest).unwrap();
            assert_eq!("54321", read(&dest));
        }
    }

    #[test]
    fn retry_5xx() {
        for (failures, ok) in [(5, true), (6, false)] {
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("out");
            let downloader = StubDownloader::new(config());
            downloader.put("http://url", b"abc").fail_next(failures, 503);
            let result = download(&downloader, "http://url", &dest);
            if ok {
                result.unwrap();
                assert_eq!("abc", read(&dest));
            } else {
                let error = result.unwrap_err();
                assert!(error.chain().any(|c| c.is::<DownloadError>()), "{error:#}");
                assert!(!dest.exists());
            }
        }
    }

    #[test]
    fn download_fails_if_too_big() {
        // a server that reports a huge length for the file
        struct TooBig;
        impl Transport for TooBig {
            fn send(&self, request: &Request) -> io::Result<Response> {
                Ok(Response::new(
                    &request.url,
                    206,
                    vec![("Content-Range".to_string(), format!("bytes 0-0/{}", i64::MAX / 2))],
                    Box::new(io::Cursor::new(vec![0u8])),
                ))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let downloader = Downloader::with_transport(config(), 1_000_000, Arc::new(TooBig));
        let error = download(&downloader, "http://url", &dir.path().join("out")).unwrap_err();
        assert!(format!("{error:#}").contains("--force"), "{error:#}");
    }

    #[test]
    fn fetch_tail_and_range() {
        let downloader = StubDownloader::new(config());
        downloader.put_with_validators("http://url", RANGE_BYTES, Some("\"v1\""), Some("date1"));

        let tail = downloader.fetch_tail("http://url", 3).unwrap();
        assert_eq!("789", string(&tail));
        assert_eq!(7, tail.offset);
        assert_eq!(RemoteVersion::new(10, Some("\"v1\""), Some("date1")), tail.version);

        let mut result = [0u8; 4];
        fetch_into(&downloader, &tail.version, 2, &mut result).unwrap();
        assert_eq!(b"2345", &result);
    }

    #[test]
    fn fetch_range_follows_redirects() {
        let downloader = StubDownloader::new(config());
        downloader.redirect("http://a", "/b").put("http://a/b", RANGE_BYTES);
        let response = downloader.fetch_tail("http://a", 2).unwrap();
        assert_eq!("http://a/b", response.url);
        assert_eq!("89", string(&response));
    }

    #[test]
    fn fetch_range_sends_preconditions() {
        let downloader = StubDownloader::new(config());
        downloader.put_with_validators("http://url", RANGE_BYTES, Some("W/\"weak\""), Some("date1"));
        let version = downloader.fetch_tail("http://url", 1).unwrap().version;
        fetch_into(&downloader, &version, 0, &mut [0u8; 1]).unwrap();
        let changed = RemoteVersion::new(10, Some("\"strong\""), Some("date1"));
        let error = fetch_into(&downloader, &changed, 0, &mut [0u8; 1]).unwrap_err();
        assert!(matches!(download_error(&error), Some(DownloadError::RemoteFileChanged { .. })), "{error:#}");

        let requests = downloader.requests();
        // If-Match only works with strong ETags, so a weak one falls back to Last-Modified
        assert_eq!(
            crate::testing::stub_downloader::StubRequest {
                url: "http://url".into(),
                range: Some("bytes=0-0".into()),
                if_match: None,
                if_unmodified_since: Some("date1".into())
            },
            requests[1]
        );
        assert_eq!(Some("\"strong\"".to_string()), requests[2].if_match);
        assert_eq!(None, requests[2].if_unmodified_since);
    }

    #[test]
    fn fetch_range_fails_when_file_changed() {
        let downloader = StubDownloader::new(config());
        downloader.put_with_validators("http://url", RANGE_BYTES, Some("\"v1\""), None);
        let version = downloader.fetch_tail("http://url", 1).unwrap().version;
        downloader.put_with_validators("http://url", RANGE_BYTES, Some("\"v2\""), None);

        let error = fetch_into(&downloader, &version, 0, &mut [0u8; 1]).unwrap_err();
        assert!(matches!(download_error(&error), Some(DownloadError::RemoteFileChanged { .. })), "{error:#}");
        assert!(error.to_string().contains("--download-fail-if-changed=false"), "{error:#}");
    }

    #[test]
    fn fetch_range_retries_server_errors() {
        let downloader = StubDownloader::new(config());
        downloader.put("http://url", RANGE_BYTES).fail_next(2, 503);
        assert_eq!("9", string(&downloader.fetch_tail("http://url", 1).unwrap()));
        assert_eq!(3, downloader.requests().len());

        downloader.fail_next(config().http_retries as usize + 1, 429);
        assert!(downloader.fetch_tail("http://url", 1).is_err());
    }

    #[test]
    fn fetch_range_fails_without_range_support() {
        let downloader = StubDownloader::new(config());
        downloader.put("http://url", RANGE_BYTES).ignore_ranges();
        let error = downloader.fetch_tail("http://url", 1).unwrap_err();
        assert!(matches!(download_error(&error), Some(DownloadError::RangesNotSupported { .. })), "{error:#}");
        assert!(error.to_string().contains("does not support range requests"), "{error:#}");
    }

    #[test]
    fn remote_version_matches_only_validators_both_have() {
        let version = RemoteVersion::new(10, Some("\"a\""), Some("date1"));
        assert!(version.matches(&RemoteVersion::new(10, Some("\"a\""), Some("date1"))));
        assert!(version.matches(&RemoteVersion::new(10, None, None)));
        assert!(!version.matches(&RemoteVersion::new(11, Some("\"a\""), Some("date1"))));
        assert!(!version.matches(&RemoteVersion::new(10, Some("\"b\""), Some("date1"))));
        assert!(!version.matches(&RemoteVersion::new(10, None, Some("date2"))));
    }

    #[test]
    fn fetch_tail_without_suffix_range_support() {
        for rejects in [true, false] {
            let downloader = StubDownloader::new(config());
            downloader.put("http://url", RANGE_BYTES);
            if rejects {
                downloader.reject_suffix_ranges();
            } else {
                downloader.ignore_suffix_ranges();
            }
            let tail = downloader.fetch_tail("http://url", 3).unwrap();
            assert_eq!("789", string(&tail));
            assert_eq!(7, tail.offset);
            assert_eq!(10, tail.version.length);
            assert_eq!(some(&["bytes=-3", "bytes=0-0", "bytes=7-9"]), ranges(&downloader));
        }
    }

    #[test]
    fn fetch_resumes_after_dropped_connection() {
        let downloader = StubDownloader::new(config());
        downloader.put("http://url", RANGE_BYTES);
        let version = downloader.fetch_tail("http://url", 1).unwrap().version;
        downloader.clear_requests();
        downloader.truncate_next(1, 3);

        let mut result = [0u8; 8];
        fetch_into(&downloader, &version, 1, &mut result).unwrap();
        assert_eq!(b"12345678", &result);
        assert_eq!(some(&["bytes=1-8", "bytes=4-8"]), ranges(&downloader));
    }

    #[test]
    fn fetch_continues_right_away_while_making_progress() {
        // a streaming read can wait mid-body long enough for the server to close the connection as idle, which is no
        // reason to wait or give up
        let downloader = StubDownloader::new(config_with(&[("http_retries", "1")]));
        downloader.put("http://url", RANGE_BYTES);
        let version = downloader.fetch_tail("http://url", 1).unwrap().version;
        downloader.clear_requests();
        downloader.truncate_next(3, 2);

        let mut result = [0u8; 8];
        fetch_into(&downloader, &version, 1, &mut result).unwrap();
        assert_eq!(b"12345678", &result);
        assert_eq!(some(&["bytes=1-8", "bytes=3-8", "bytes=5-8", "bytes=7-8"]), ranges(&downloader));
        assert!(downloader.retry_waits().is_empty());
    }

    #[test]
    fn download_resumes_partial_file() {
        let contents: Vec<u8> = (0..3_000_000u32).map(|i| i as u8).collect();
        let downloader = StubDownloader::new(config());
        downloader.put("http://url", &contents);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        // a cached read (or an interrupted download) fetched part of the file, and its tail when it got opened
        {
            let partial = CachedRangeFile::open(&dest, "http://url", 1 << 20, &downloader).unwrap();
            crate::reader::remote::range_file::RangeFile::read(&partial, 0, 1000).unwrap();
            partial.close().unwrap();
        }
        assert!(CachedRangeFile::is_partial(&dest));
        downloader.clear_requests();

        let resource = resource("http://url", &dest);
        downloader.download_if_necessary(&resource).unwrap();

        assert!(!CachedRangeFile::is_partial(&dest));
        assert_eq!(contents, std::fs::read(&dest).unwrap());
        let tail_start = contents.len() - (1 << 20);
        assert_eq!(some(&["bytes=0-0", &format!("bytes=1000-{}", tail_start - 1)]), ranges(&downloader));
        // progress counts the whole file, starting from the bytes fetched before
        assert_eq!(Some(contents.len() as u64), resource.length());
        assert_eq!(contents.len() as i64, resource.bytes_downloaded());
    }

    #[test]
    fn download_starts_over_when_remote_file_changed() {
        let downloader = StubDownloader::new(config());
        downloader.put("http://url", RANGE_BYTES);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        download(&downloader, "http://url", &dest).unwrap();
        downloader.put("http://url", b"abcdefghij");

        download(&downloader, "http://url", &dest).unwrap();

        assert_eq!("abcdefghij", read(&dest));
    }

    #[test]
    fn download_without_range_support() {
        let downloader = StubDownloader::new(config());
        downloader.put("http://url", RANGE_BYTES).ignore_ranges();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");

        download(&downloader, "http://url", &dest).unwrap();

        assert_eq!("0123456789", read(&dest));
        assert!(!CachedRangeFile::is_partial(&dest));
        assert_eq!(None, downloader.requests().last().unwrap().range);
    }

    #[test]
    fn complete_file_without_version_compares_length() {
        let downloader = StubDownloader::new(config());
        downloader.put("http://url", RANGE_BYTES);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        // left by an older version of planetiler that didn't save the remote file's version
        std::fs::write(&dest, "same size!").unwrap();

        download(&downloader, "http://url", &dest).unwrap();

        assert_eq!("same size!", read(&dest));
    }

    #[test]
    fn open_stream() {
        let downloader = StubDownloader::new(config());
        downloader.redirect("http://a", "http://b").put("http://b", RANGE_BYTES);
        let mut bytes = vec![];
        downloader.open_stream("http://a").unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(RANGE_BYTES, bytes);
        assert!(downloader.open_stream("http://missing").is_err());
        assert_eq!(
            1,
            downloader.requests().iter().filter(|r| r.url == "http://missing").count(),
            "a 404 doesn't get retried"
        );
    }

    #[test]
    fn partial_download_completed_by_server_without_range_support() {
        let downloader = StubDownloader::new(config());
        downloader.put("http://url", RANGE_BYTES);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        {
            let partial = CachedRangeFile::open(&dest, "http://url", 0, &downloader).unwrap();
            crate::reader::remote::range_file::RangeFile::read(&partial, 0, 2).unwrap();
            partial.close().unwrap();
        }
        downloader.ignore_ranges();

        download(&downloader, "http://url", &dest).unwrap();

        assert_eq!("0123456789", read(&dest));
        assert!(!CachedRangeFile::is_partial(&dest));
    }

    #[test]
    fn retry_waits_as_long_as_the_server_asks() {
        let c = config();
        let downloader = StubDownloader::new(c.clone());
        downloader.put("http://url", RANGE_BYTES).fail_next_with_headers(1, 429, &[("Retry-After", "120")]);
        assert_eq!("9", string(&downloader.fetch_tail("http://url", 1).unwrap()));
        assert_eq!(vec![Duration::from_secs(120)], downloader.retry_waits());

        // a shorter wait than http_retry_wait doesn't make it retry sooner
        downloader.fail_next_with_headers(1, 503, &[("Retry-After", "0")]);
        downloader.fetch_tail("http://url", 1).unwrap();
        assert_eq!(c.http_retry_wait, *downloader.retry_waits().last().unwrap());
    }

    #[test]
    fn caps_wait_at_max_wait() {
        let c = config();
        let downloader = StubDownloader::new(c.clone());
        downloader.put("http://url", RANGE_BYTES).fail_next_with_headers(1, 429, &[("Retry-After", "3600")]);
        assert_eq!("9", string(&downloader.fetch_tail("http://url", 1).unwrap()));
        assert_eq!(vec![c.http_retry_max_wait], downloader.retry_waits());
    }

    #[test]
    fn parse_retry_after_values() {
        assert_eq!(Some(Duration::from_secs(120)), parse_retry_after("120", 1));
        assert_eq!(Some(Duration::from_secs(30)), parse_retry_after("Thu, 01 Jan 1970 00:00:31 GMT", 1));
        assert_eq!(Some(Duration::ZERO), parse_retry_after("Thu, 01 Jan 1970 00:00:00 GMT", 1));
        assert_eq!(None, parse_retry_after("soon", 1));
        assert_eq!(None, parse_retry_after("-5", 1));
        assert_eq!(None, parse_retry_after("", 1));
    }

    #[test]
    fn send_with_retries_reads_body_and_does_not_retry_client_errors() {
        let downloader = StubDownloader::new(config());
        downloader.respond_to_posts(|body| {
            if body == "bad" {
                (400, b"Query is malformed:\n line 1".to_vec())
            } else {
                (200, format!("result of {body}").into_bytes())
            }
        });
        downloader.fail_next(1, 503);
        let read_string = |body: &mut dyn Read| {
            let mut s = String::new();
            body.read_to_string(&mut s)?;
            Ok(s)
        };
        assert_eq!(
            "result of query",
            downloader.send_with_retries(&Request::post("http://url", "query"), read_string).unwrap()
        );

        downloader.clear_requests();
        let error = downloader.send_with_retries(&Request::post("http://url", "bad"), |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("400 Query is malformed: line 1"), "{error:#}");
        assert_eq!(1, downloader.requests().len(), "a 400 doesn't get retried");
    }

    #[test]
    fn open_stream_reads_file_urls() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        std::fs::write(&file, "local").unwrap();
        let downloader = StubDownloader::new(config());
        assert_eq!("local", downloader.fetch_string(&format!("file://{}", file.display())).unwrap());
        assert!(downloader.requests().is_empty());
    }

    #[test]
    fn download_without_range_support_or_length() {
        let downloader = StubDownloader::new(config());
        downloader.put("http://url", RANGE_BYTES).ignore_ranges().omit_content_length();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");

        download(&downloader, "http://url", &dest).unwrap();

        assert_eq!("0123456789", read(&dest));
        assert!(!CachedRangeFile::is_partial(&dest));
        assert!(read(&CachedRangeFile::meta_path(&dest)).contains("\"length\":10"));
    }

    #[test]
    fn interrupted_download_without_range_support_starts_over() {
        let downloader = StubDownloader::new(config_with(&[("http_retries", "0")]));
        downloader.put("http://url", RANGE_BYTES).ignore_ranges();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        downloader.truncate_next(3, 4);

        assert!(download(&downloader, "http://url", &dest).is_err());
        assert!(CachedRangeFile::is_partial(&dest));

        download(&downloader, "http://url", &dest).unwrap();
        assert_eq!("0123456789", read(&dest));
        assert!(!CachedRangeFile::is_partial(&dest));
    }

    #[test]
    fn download_empty_file_without_range_support() {
        let downloader = StubDownloader::new(config());
        downloader.put("http://url", b"").ignore_ranges();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");

        download(&downloader, "http://url", &dest).unwrap();

        assert_eq!(0, std::fs::metadata(&dest).unwrap().len());
        assert!(!CachedRangeFile::is_partial(&dest));
    }

    #[test]
    fn run_downloads_all() {
        let dir = tempfile::tempdir().unwrap();
        let downloader = StubDownloader::new(config());
        downloader.put("http://a", b"aaa").put("http://b", b"bb");
        downloader.add("a", "http://a", dir.path().join("a")).unwrap();
        downloader.add("b", "http://b", dir.path().join("sub").join("b")).unwrap();
        downloader.run().unwrap();
        assert_eq!("aaa", read(&dir.path().join("a")));
        assert_eq!("bb", read(&dir.path().join("sub").join("b")));

        // each call to run only downloads what was added since the last one
        downloader.put("http://c", b"c");
        downloader.clear_requests();
        downloader.add("c", "http://c", dir.path().join("c")).unwrap();
        downloader.run().unwrap();
        assert_eq!("c", read(&dir.path().join("c")));
        assert!(downloader.requests().iter().all(|r| r.url == "http://c"));
    }

    #[test]
    fn run_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let downloader = StubDownloader::new(config_with(&[("http_retries", "0")]));
        downloader.put("http://a", b"aaa");
        downloader.add("a", "http://a", dir.path().join("a")).unwrap();
        downloader.add("missing", "http://missing", dir.path().join("missing")).unwrap();
        assert!(downloader.run().is_err());
    }

    #[test]
    fn real_http_server() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            // opening the file makes a range request through the redirect, which fetches the whole 5 byte file
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut range = None;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        range = Some(value.trim().to_string());
                    }
                }
                let mut stream = reader.into_inner();
                let response = if request_line.contains("/redirect") {
                    "HTTP/1.1 302 Found\r\nLocation: /file\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_string()
                } else if range.is_some() {
                    "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 0-4/5\r\ncontent-length: 5\r\nconnection: close\r\n\r\nhello".to_string()
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nhello".to_string()
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        let downloader = Downloader::create(config());
        download(&downloader, &format!("http://127.0.0.1:{port}/redirect"), &dest).unwrap();
        server.join().unwrap();
        assert_eq!("hello", read(&dest));
    }

    #[test]
    fn location_resolution() {
        assert_eq!("http://a/b/c", resolve_location("http://a/x/y", "http://a/b/c"));
        assert_eq!("http://a/b/c", resolve_location("http://a/x/y", "/b/c"));
        assert_eq!("http://a/x/c", resolve_location("http://a/x/y", "c"));
    }
}
