//! Port of `util/Downloader.java`: a utility for downloading files to disk in parallel over HTTP.
//!
//! After downloading a file once, it won't be downloaded again unless the `Content-Length` of the resource changes.
//!
//! ```no_run
//! # use planetiler_core::util::downloader::Downloader;
//! # use planetiler_core::config::planetiler_config::PlanetilerConfig;
//! # fn main() -> anyhow::Result<()> {
//! Downloader::create(PlanetilerConfig::defaults())
//!     .add("natural_earth", "http://url/of/natural_earth.zip", "natural_earth.zip")?
//!     .add("osm", "http://url/of/file.osm.pbf", "file.osm.pbf")?
//!     .run()?;
//! # Ok(())
//! # }
//! ```
//!
//! As a shortcut to find the URL of a file to download from the [Geofabrik download site](https://download.geofabrik.de/),
//! use `"geofabrik:extract name"` (i.e. `"geofabrik:monaco"`). Use `"aws:latest"` to download the latest
//! `planet.osm.pbf` from the [AWS Open Data Registry](https://registry.opendata.aws/osm/), or `"overture:latest"` for
//! the latest [Overture Maps Foundation](https://overturemaps.org/) release.
//!
//! HTTP access goes through the [`HttpFetcher`] trait so tests can substitute a mock (Java overrides methods in an
//! anonymous subclass instead).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use parking_lot::Mutex;

use crate::config::planetiler_config::PlanetilerConfig;
use crate::stats::counter::Counter;
use crate::stats::progress_loggers::ProgressLoggers;
use crate::util::resource_usage::ResourceUsage;
use crate::worker::Done;

const MAX_REDIRECTS: usize = 5;
const BUFFER_SIZE: usize = 16384;

/// Result of an HTTP `HEAD` request for a resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceMetadata {
    /// URL to follow if this was a redirect.
    pub redirect: Option<String>,
    /// URL that this metadata is for.
    pub canonical_url: String,
    /// `Content-Length` of the resource, if known.
    pub size: Option<u64>,
    /// True if the server reported `Accept-Ranges: bytes`.
    pub accept_range: bool,
}

/// A readable response body.
pub type Stream = Box<dyn Read + Send>;

/// Low-level HTTP operations used by [`Downloader`], replaceable for tests.
///
/// Errors that should be retried must be (or wrap) `std::io::Error` or `ureq::Error`.
pub trait HttpFetcher: Send + Sync {
    /// Issues a `HEAD` request for `url` without following redirects.
    fn http_head(&self, url: &str) -> anyhow::Result<ResourceMetadata>;
    /// Opens a stream reading the full body of `url`.
    fn open_stream(&self, url: &str) -> anyhow::Result<Stream>;
    /// Opens a stream reading bytes `start` (inclusive) to `end` of `url`.
    fn open_stream_range(&self, url: &str, start: u64, end: u64) -> anyhow::Result<Stream>;
    /// Waits before retrying a failed request.
    fn retry_sleep(&self, wait: Duration) {
        std::thread::sleep(wait);
    }
}

/// [`HttpFetcher`] that makes real HTTP requests with `ureq`, using timeouts and user agent from the config.
pub struct UreqFetcher {
    agent: ureq::Agent,
}

impl UreqFetcher {
    pub fn new(config: &PlanetilerConfig) -> UreqFetcher {
        UreqFetcher { agent: agent(config, 0) }
    }
}

fn agent(config: &PlanetilerConfig, max_redirects: u32) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(config.http_timeout))
        .timeout_recv_response(Some(config.http_timeout))
        .user_agent(config.http_user_agent.as_str())
        .max_redirects(max_redirects)
        .http_status_as_error(false)
        .build()
        .into()
}

fn header<'a>(response: &'a ureq::http::Response<ureq::Body>, name: &str) -> Option<&'a str> {
    response.headers().get(name).and_then(|v| v.to_str().ok())
}

fn read_response(response: ureq::http::Response<ureq::Body>, url: &str) -> anyhow::Result<Stream> {
    let status = response.status().as_u16();
    if status == 404 {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("Not found: {url}")).into());
    }
    if !(200..300).contains(&status) {
        return Err(std::io::Error::other(format!("Bad response {status} from {url}")).into());
    }
    Ok(Box::new(response.into_body().into_reader()))
}

impl HttpFetcher for UreqFetcher {
    fn http_head(&self, url: &str) -> anyhow::Result<ResourceMetadata> {
        let response = self.agent.head(url).call()?;
        let status = response.status().as_u16();
        let mut redirect = None;
        let mut size = None;
        if (300..400).contains(&status) {
            match header(&response, "location") {
                Some(location) => redirect = Some(resolve_location(url, location)),
                None => bail!("Received {status} but no location header from {url}"),
            }
        } else if status != 200 {
            bail!("Bad response: {status}");
        } else {
            size = header(&response, "content-length").and_then(|v| v.trim().parse().ok());
        }
        let accept_range = response
            .headers()
            .get_all("accept-ranges")
            .iter()
            .any(|v| v.to_str().is_ok_and(|v| v.split(',').any(|p| p.trim() == "bytes")));
        Ok(ResourceMetadata { redirect, canonical_url: url.to_string(), size, accept_range })
    }

    fn open_stream(&self, url: &str) -> anyhow::Result<Stream> {
        read_response(self.agent.get(url).call()?, url)
    }

    fn open_stream_range(&self, url: &str, start: u64, end: u64) -> anyhow::Result<Stream> {
        read_response(self.agent.get(url).header("Range", format!("bytes={start}-{end}")).call()?, url)
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

/// Returns a stream reading from a remote URL (following redirects) with timeout and user-agent set from `config`.
pub fn open_stream(url: &str, config: &PlanetilerConfig) -> anyhow::Result<Stream> {
    let response =
        agent(config, MAX_REDIRECTS as u32).get(url).call().with_context(|| format!("Error fetching {url}"))?;
    read_response(response, url)
}

/// Reads the full body of a remote URL as a string.
pub fn fetch_string(url: &str, config: &PlanetilerConfig) -> anyhow::Result<String> {
    let mut result = String::new();
    open_stream(url, config)?.read_to_string(&mut result)?;
    Ok(result)
}

/// A resource to download.
pub struct ResourceToDownload {
    pub id: String,
    pub url: String,
    pub output: PathBuf,
    metadata: OnceLock<ResourceMetadata>,
    metadata_done: Done,
    progress: Counter,
}

impl ResourceToDownload {
    pub fn new(id: &str, url: &str, output: impl Into<PathBuf>) -> Arc<ResourceToDownload> {
        Arc::new(ResourceToDownload {
            id: id.to_string(),
            url: url.to_string(),
            output: output.into(),
            metadata: OnceLock::new(),
            metadata_done: Done::new(),
            progress: Counter::new(),
        })
    }

    /// Where the resource is downloaded to before being moved to `output`.
    pub fn tmp_path(&self) -> PathBuf {
        let mut name = self.output.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push("_inprogress");
        self.output.with_file_name(name)
    }

    /// Number of bytes downloaded so far.
    pub fn bytes_downloaded(&self) -> i64 {
        self.progress.get()
    }

    /// Metadata from the `HEAD` request, once available.
    pub fn metadata(&self) -> Option<&ResourceMetadata> {
        self.metadata.get()
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

/// Downloads files to disk in parallel over HTTP.
pub struct Downloader {
    config: PlanetilerConfig,
    to_download: Vec<Arc<ResourceToDownload>>,
    fetcher: Arc<dyn HttpFetcher>,
    chunk_size_bytes: u64,
    rate_limiter: Option<RateLimiter>,
}

fn is_retryable(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<std::io::Error>() || c.is::<ureq::Error>())
}

impl Downloader {
    /// Returns a new downloader that makes real HTTP requests.
    pub fn create(config: PlanetilerConfig) -> Downloader {
        let chunk_size = config.download_chunk_size_mb * 1_000_000;
        let fetcher = Arc::new(UreqFetcher::new(&config));
        Self::with_fetcher(config, chunk_size, fetcher)
    }

    /// Returns a new downloader that uses `fetcher` for HTTP requests, downloading ranges of `chunk_size_bytes`.
    pub fn with_fetcher(config: PlanetilerConfig, chunk_size_bytes: u64, fetcher: Arc<dyn HttpFetcher>) -> Downloader {
        let rate_limiter = (config.download_max_bandwidth > 0.0).then(|| RateLimiter {
            bytes_per_second: config.download_max_bandwidth,
            next_free: Mutex::new(Instant::now()),
        });
        Downloader { config, to_download: vec![], fetcher, chunk_size_bytes: chunk_size_bytes.max(1), rate_limiter }
    }

    /// Adds a new resource to download but does not start downloading it until [`Downloader::run`] is called.
    ///
    /// `url` can also be `aws:<snapshot>`, `overture:<release>` or `geofabrik:<extract name>`, which are resolved to a
    /// download URL immediately.
    pub fn add(mut self, id: &str, url: &str, output: impl Into<PathBuf>) -> anyhow::Result<Downloader> {
        let url = if let Some(query) = url.strip_prefix("geofabrik:") {
            crate::util::geofabrik::get_download_url(query, &self.config)?
        } else if let Some(query) = url.strip_prefix("aws:") {
            crate::util::aws_osm::AwsOsm::osm_pds().get_download_url(query, &self.config)?
        } else if let Some(query) = url.strip_prefix("overture:") {
            crate::util::aws_osm::AwsOsm::overture().get_download_url(query, &self.config)?
        } else {
            url.to_string()
        };
        self.to_download.push(ResourceToDownload::new(id, &url, output));
        Ok(self)
    }

    /// Downloads all resources in parallel, logging progress until complete. Returns an error after all resources
    /// finish if any failed.
    pub fn run(self) -> anyhow::Result<()> {
        let this = Arc::new(self);
        let mut dones = vec![];
        for resource in &this.to_download {
            let done = Done::new();
            dones.push(done.clone());
            let (this, resource) = (this.clone(), resource.clone());
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
            let remaining = Arc::new(AtomicUsize::new(dones.len()));
            let first_error = Arc::new(Mutex::new(None));
            if dones.is_empty() {
                all.complete(Ok(()));
            }
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
        for resource in &this.to_download {
            grouped.entry(resource.id.clone()).or_default().push(resource.clone());
        }
        let mut loggers = ProgressLoggers::create();
        for (id, group) in grouped {
            let mut size = 0;
            for item in &group {
                match item.metadata_done.wait_timeout(this.config.max_http_request_duration()) {
                    Some(Ok(())) => size += item.metadata().and_then(|m| m.size).unwrap_or(0),
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
        loggers.add_str(" ").add_process_stats().await_and_log(&all, this.config.log_interval)
    }

    /// Downloads `resource` unless the file on disk already has the same size as the remote resource.
    pub fn download_if_necessary(&self, resource: &ResourceToDownload) -> anyhow::Result<()> {
        let existing_size =
            if resource.output.exists() { crate::util::file_utils::size(&resource.output).ok() } else { None };
        let metadata = match self.http_head_follow_redirects(&resource.url, 0) {
            Ok(metadata) => {
                let _ = resource.metadata.set(metadata.clone());
                resource.metadata_done.complete(Ok(()));
                metadata
            }
            Err(e) => {
                resource.metadata_done.fail(anyhow::anyhow!("{e:#}"));
                return Err(e);
            }
        };
        if metadata.size.is_some() && metadata.size == existing_size {
            tracing::info!("Skipping {}: {} already up-to-date", resource.id, resource.output.display());
            return Ok(());
        }
        let tmp_path = resource.tmp_path();
        let result = (|| -> anyhow::Result<()> {
            let redirect_info = if metadata.canonical_url == resource.url {
                String::new()
            } else {
                format!(" (redirected to {})", metadata.canonical_url)
            };
            tracing::info!("Downloading {}{redirect_info} to {}", resource.url, resource.output.display());
            delete(&resource.output)?;
            if let Some(parent) = resource.output.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            delete(&tmp_path)?;
            if let Some(size) = metadata.size {
                check_disk_space(&tmp_path, size, &resource.id, self.config.force)?;
            }
            self.http_download(resource, &metadata, &tmp_path)?;
            std::fs::rename(&tmp_path, &resource.output)?;
            tracing::info!("Finished downloading {} to {}", resource.url, resource.output.display());
            Ok(())
        })();
        let _ = delete(&tmp_path);
        if let Err(e) = &result {
            tracing::error!("Error downloading {} to {}: {e:#}", resource.url, resource.output.display());
        }
        result
    }

    fn http_head_follow_redirects(&self, url: &str, redirects: usize) -> anyhow::Result<ResourceMetadata> {
        if redirects > MAX_REDIRECTS {
            bail!("Exceeded {redirects} redirects for {url}");
        }
        let response = self.http_head(url)?;
        match &response.redirect {
            Some(next) => self.http_head_follow_redirects(next, redirects + 1),
            None => Ok(response),
        }
    }

    fn http_head(&self, url: &str) -> anyhow::Result<ResourceMetadata> {
        let retries = self.config.http_retries;
        let mut retry = 0;
        loop {
            match self.fetcher.http_head(url) {
                Ok(m) => return Ok(m),
                Err(e) if retry < retries && is_retryable(&e) => {
                    tracing::warn!("Error fetching head for {url}, retries remaining: {} {e}", retries - retry);
                    self.fetcher.retry_sleep(self.config.http_retry_wait);
                }
                Err(e) => return Err(e),
            }
            retry += 1;
        }
    }

    fn http_download(
        &self,
        resource: &ResourceToDownload,
        metadata: &ResourceMetadata,
        tmp_path: &Path,
    ) -> anyhow::Result<()> {
        let canonical_url = &metadata.canonical_url;
        let ranges = metadata.accept_range && self.config.download_threads > 1;
        let file_size = metadata.size.unwrap_or(u64::MAX);
        let chunk_size = if ranges { self.chunk_size_bytes } else { file_size };
        let mut chunks = vec![];
        let mut start = 0;
        while start < file_size {
            let end = start.saturating_add(chunk_size).min(file_size);
            chunks.push((start, end));
            start = end;
        }
        File::create(tmp_path)?.set_len(metadata.size.unwrap_or(1))?;
        let next_chunk = AtomicUsize::new(0);
        let threads = if ranges { self.config.download_threads.min(chunks.len()).max(1) } else { 1 };
        std::thread::scope(|scope| -> anyhow::Result<()> {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| -> anyhow::Result<()> {
                        crate::util::log_util::set_stage_with_parent(Some("download"), &resource.id);
                        loop {
                            let idx = next_chunk.fetch_add(1, Ordering::Relaxed);
                            let Some(&(start, end)) = chunks.get(idx) else {
                                return Ok(());
                            };
                            self.download_range(resource, canonical_url, tmp_path, start, end, ranges)?;
                        }
                    })
                })
                .collect();
            let mut result = Ok(());
            for handle in handles {
                let r = handle.join().unwrap_or_else(|_| Err(anyhow::anyhow!("download thread panicked")));
                if result.is_ok() {
                    result = r;
                }
            }
            result
        })
    }

    fn download_range(
        &self,
        resource: &ResourceToDownload,
        url: &str,
        tmp_path: &Path,
        start: u64,
        end: u64,
        ranges: bool,
    ) -> anyhow::Result<()> {
        let counter = resource.progress.counter_for_thread();
        let retries = self.config.http_retries;
        let mut retry = 0;
        loop {
            let mut count_to_rewind = 0i64;
            let attempt = (|| -> anyhow::Result<()> {
                let mut file = OpenOptions::new().write(true).open(tmp_path)?;
                let mut input = if ranges || start > 0 {
                    self.fetcher.open_stream_range(url, start, end)?
                } else {
                    self.fetcher.open_stream(url)?
                };
                file.seek(SeekFrom::Start(start))?;
                let mut offset = start;
                let mut buffer = vec![0u8; BUFFER_SIZE];
                while offset < end {
                    let read = input.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    counter.inc_by(read as i64);
                    count_to_rewind += read as i64;
                    if let Some(limiter) = &self.rate_limiter {
                        limiter.acquire(read);
                    }
                    file.write_all(&buffer[..read])?;
                    offset += read as u64;
                }
                if offset < end && end != u64::MAX {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!("Unexpected EOF at {offset}/{end}"),
                    )
                    .into());
                }
                Ok(())
            })();
            match attempt {
                Ok(()) => return Ok(()),
                Err(e) if retry < retries && is_retryable(&e) => {
                    counter.inc_by(-count_to_rewind);
                    tracing::warn!("Error downloading {url}, retries remaining: {} {e}", retries - retry);
                    self.fetcher.retry_sleep(self.config.http_retry_wait);
                }
                Err(e) => return Err(e),
            }
            retry += 1;
        }
    }
}

fn delete(path: &Path) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Fails if there is not enough free disk space to write `size` bytes at `path`, unless `force` is set.
fn check_disk_space(path: &Path, size: u64, id: &str, force: bool) -> anyhow::Result<()> {
    ResourceUsage::new("download")
        .add_disk(Some(path), size, &format!("download {id}"))
        .check_against_limits(force, false)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicI64;

    use super::*;

    type Override = Box<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync>;

    struct MockFetcher {
        resources: Mutex<HashMap<String, Vec<u8>>>,
        supports_range: bool,
        supports_content_length: bool,
        override_bytes: Override,
        downloads: AtomicI64,
        slept: AtomicI64,
    }

    impl MockFetcher {
        fn bytes(&self, url: &str) -> anyhow::Result<Vec<u8>> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            let resources = self.resources.lock();
            let bytes = resources.get(url).ok_or_else(|| anyhow::anyhow!("no resource for {url}"))?;
            Ok((self.override_bytes)(bytes))
        }
    }

    impl HttpFetcher for MockFetcher {
        fn http_head(&self, url: &str) -> anyhow::Result<ResourceMetadata> {
            let parts: Vec<&str> = url.split('#').collect();
            if parts.len() > 1 {
                let redirect_num: u32 = parts[1].parse()?;
                let next =
                    if redirect_num <= 1 { parts[0].to_string() } else { format!("{}#{}", parts[0], redirect_num - 1) };
                return Ok(ResourceMetadata {
                    redirect: Some(next),
                    canonical_url: url.to_string(),
                    size: self.supports_content_length.then_some(0),
                    accept_range: self.supports_range,
                });
            }
            let resources = self.resources.lock();
            let bytes = resources.get(url).ok_or_else(|| anyhow::anyhow!("no resource for {url}"))?;
            Ok(ResourceMetadata {
                redirect: None,
                canonical_url: url.to_string(),
                size: self.supports_content_length.then_some(bytes.len() as u64),
                accept_range: self.supports_range,
            })
        }

        fn open_stream(&self, url: &str) -> anyhow::Result<Stream> {
            Ok(Box::new(std::io::Cursor::new(self.bytes(url)?)))
        }

        fn open_stream_range(&self, url: &str, start: u64, end: u64) -> anyhow::Result<Stream> {
            assert!(self.supports_range, "does not support range");
            let bytes = self.bytes(url)?;
            Ok(Box::new(std::io::Cursor::new(bytes[start as usize..end as usize].to_vec())))
        }

        fn retry_sleep(&self, _wait: Duration) {
            self.slept.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn mock(supports_range: bool, supports_content_length: bool, override_bytes: Override) -> Arc<MockFetcher> {
        Arc::new(MockFetcher {
            resources: Mutex::new(HashMap::new()),
            supports_range,
            supports_content_length,
            override_bytes,
            downloads: AtomicI64::new(0),
            slept: AtomicI64::new(0),
        })
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn size(path: &Path) -> u64 {
        crate::util::file_utils::size(path).unwrap_or(0)
    }

    #[test]
    fn download() {
        for (range, redirects, supports_content_length) in [
            (false, 0, true),
            (true, 0, true),
            (false, 1, true),
            (false, 2, true),
            (true, 4, true),
            (false, 0, false),
            (true, 0, false),
            (false, 1, false),
            (true, 1, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("out");
            let string = "0123456789";
            let url = "http://url";
            let initial_url = if redirects > 0 { format!("{url}#{redirects}") } else { url.to_string() };
            let fetcher = mock(range, supports_content_length, Box::new(|b| b.to_vec()));
            let downloader = Downloader::with_fetcher(PlanetilerConfig::defaults(), 2, fetcher.clone());

            // fails if no data
            let resource1 = ResourceToDownload::new("resource", &initial_url, &dest);
            assert!(downloader.download_if_necessary(&resource1).is_err());
            assert!(!dest.exists());
            assert_eq!(0, resource1.bytes_downloaded());

            // succeeds with data
            let resource2 = ResourceToDownload::new("resource", &initial_url, &dest);
            fetcher.resources.lock().insert(url.to_string(), string.as_bytes().to_vec());
            downloader.download_if_necessary(&resource2).unwrap();
            assert_eq!(string, read(&dest));
            assert_eq!(size(dir.path()), size(&dest));
            assert_eq!(10, resource2.bytes_downloaded());

            // does not re-request if size is the same
            if supports_content_length {
                fetcher.downloads.store(0, Ordering::SeqCst);
                let resource3 = ResourceToDownload::new("resource", &initial_url, &dest);
                downloader.download_if_necessary(&resource3).unwrap();
                assert_eq!(0, fetcher.downloads.load(Ordering::SeqCst));
                assert_eq!(string, read(&dest));
                assert_eq!(size(dir.path()), size(&dest));
                assert_eq!(0, resource3.bytes_downloaded());
            }

            // does re-download if size changes
            let resource4 = ResourceToDownload::new("resource", &initial_url, &dest);
            let new_content = "54321";
            fetcher.resources.lock().insert(url.to_string(), new_content.as_bytes().to_vec());
            downloader.download_if_necessary(&resource4).unwrap();
            assert!(fetcher.downloads.load(Ordering::SeqCst) > 0);
            assert_eq!(new_content, read(&dest));
            assert_eq!(size(dir.path()), size(&dest));
            assert_eq!(5, resource4.bytes_downloaded());
        }
    }

    #[test]
    fn download_ranges_in_parallel() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        let fetcher = mock(true, true, Box::new(|b| b.to_vec()));
        let content = "0123456789abcdefghij";
        fetcher.resources.lock().insert("http://url".to_string(), content.as_bytes().to_vec());
        let config =
            PlanetilerConfig::from(&crate::config::arguments::Arguments::of_pairs([("download_threads", "3")]))
                .unwrap();
        let downloader = Downloader::with_fetcher(config, 3, fetcher.clone());
        let resource = ResourceToDownload::new("resource", "http://url", &dest);
        downloader.download_if_necessary(&resource).unwrap();
        assert_eq!(content, read(&dest));
        assert_eq!(20, resource.bytes_downloaded());
        assert_eq!(7, fetcher.downloads.load(Ordering::SeqCst));
    }

    #[test]
    fn retry_5x_ok() {
        for (failures, ok) in [(5, true), (6, false)] {
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("out");
            let url = "http://url";
            let resource = ResourceToDownload::new("resource", url, &dest);
            let tries = Arc::new(AtomicI64::new(0));
            let t = tries.clone();
            let fetcher =
                mock(
                    true,
                    true,
                    Box::new(move |_| {
                        if t.fetch_add(1, Ordering::SeqCst) < failures { b"ab".to_vec() } else { b"abc".to_vec() }
                    }),
                );
            fetcher.resources.lock().insert(url.to_string(), b"abc".to_vec());
            let downloader = Downloader::with_fetcher(PlanetilerConfig::defaults(), 2, fetcher.clone());
            let result = downloader.download_if_necessary(&resource);
            if ok {
                result.unwrap();
                assert_eq!("abc", read(&dest));
                assert_eq!(size(dir.path()), size(&dest));
                assert_eq!(3, resource.bytes_downloaded());
            } else {
                let err = result.unwrap_err();
                assert!(err.chain().any(|c| c.is::<std::io::Error>()), "{err:#}");
                assert!(!dest.exists());
                assert_eq!(2, resource.bytes_downloaded());
            }
            assert_eq!(5, fetcher.slept.load(Ordering::SeqCst));
        }
    }

    struct TooBig;

    impl HttpFetcher for TooBig {
        fn http_head(&self, url: &str) -> anyhow::Result<ResourceMetadata> {
            Ok(ResourceMetadata {
                redirect: None,
                canonical_url: url.to_string(),
                size: Some(u64::MAX),
                accept_range: true,
            })
        }

        fn open_stream(&self, _url: &str) -> anyhow::Result<Stream> {
            panic!("Shouldn't get here");
        }

        fn open_stream_range(&self, _url: &str, _start: u64, _end: u64) -> anyhow::Result<Stream> {
            panic!("Shouldn't get here");
        }
    }

    #[test]
    fn download_fails_if_too_big() {
        let dir = tempfile::tempdir().unwrap();
        let downloader = Downloader::with_fetcher(PlanetilerConfig::defaults(), 2, Arc::new(TooBig));
        let resource = ResourceToDownload::new("resource", "http://url", dir.path().join("out"));
        let err = downloader.download_if_necessary(&resource).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
    }

    #[test]
    fn run_downloads_all() {
        let dir = tempfile::tempdir().unwrap();
        let fetcher = mock(false, true, Box::new(|b| b.to_vec()));
        fetcher.resources.lock().insert("http://a".to_string(), b"aaa".to_vec());
        fetcher.resources.lock().insert("http://b".to_string(), b"bb".to_vec());
        Downloader::with_fetcher(PlanetilerConfig::defaults(), 2, fetcher)
            .add("a", "http://a", dir.path().join("a"))
            .unwrap()
            .add("b", "http://b", dir.path().join("sub").join("b"))
            .unwrap()
            .run()
            .unwrap();
        assert_eq!("aaa", read(&dir.path().join("a")));
        assert_eq!("bb", read(&dir.path().join("sub").join("b")));
    }

    #[test]
    fn real_http_server() {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for _ in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                }
                let mut stream = reader.into_inner();
                let response = if request_line.contains("/redirect") {
                    "HTTP/1.1 302 Found\r\nLocation: /file\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        .to_string()
                } else if request_line.starts_with("HEAD") {
                    "HTTP/1.1 200 OK\r\ncontent-length: 5\r\naccept-ranges: bytes\r\nconnection: close\r\n\r\n"
                        .to_string()
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nhello".to_string()
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        let downloader = Downloader::create(PlanetilerConfig::defaults());
        let resource = ResourceToDownload::new("resource", &format!("http://127.0.0.1:{port}/redirect"), &dest);
        downloader.download_if_necessary(&resource).unwrap();
        server.join().unwrap();
        assert_eq!("hello", read(&dest));
        assert_eq!(format!("http://127.0.0.1:{port}/file"), resource.metadata().unwrap().canonical_url);
    }

    #[test]
    fn location_resolution() {
        assert_eq!("http://a/b/c", resolve_location("http://a/x/y", "http://a/b/c"));
        assert_eq!("http://a/b/c", resolve_location("http://a/x/y", "/b/c"));
        assert_eq!("http://a/x/c", resolve_location("http://a/x/y", "c"));
    }
}
