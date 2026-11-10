//! Port of `util/StubDownloader.java` (test support): a [`Downloader`] that answers requests from files held in memory
//! instead of a real server, so tests can read and download "remote" files without network access and check the
//! requests that were made. Any request for a URL it doesn't serve gets a 404, so nothing can reach the real network.
//!
//! Not `#[cfg(test)]`-gated so that tests of other crates can use it.

use std::collections::HashMap;
use std::io::{self, Cursor};
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use crate::config::planetiler_config::PlanetilerConfig;
use crate::util::downloader::{Downloader, Request, Response, Transport};

/// A request the stub received: its URL and the headers relevant to range requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StubRequest {
    pub url: String,
    pub range: Option<String>,
    pub if_match: Option<String>,
    pub if_unmodified_since: Option<String>,
}

struct StubFile {
    bytes: Vec<u8>,
    etag: Option<String>,
    last_modified: Option<String>,
}

type PostHandler = Box<dyn Fn(&str) -> (u16, Vec<u8>) + Send + Sync>;

/// The in-memory server behind a [`StubDownloader`].
#[derive(Default)]
pub struct StubTransport {
    files: Mutex<HashMap<String, StubFile>>,
    redirects: Mutex<HashMap<String, String>>,
    requests: Mutex<Vec<StubRequest>>,
    retry_waits: Mutex<Vec<Duration>>,
    failures_left: AtomicI64,
    failure_status: AtomicUsize,
    failure_headers: Mutex<Vec<(String, String)>>,
    truncations_left: AtomicI64,
    truncate_after_bytes: AtomicUsize,
    ignore_ranges: AtomicBool,
    ignore_suffix_ranges: AtomicBool,
    reject_suffix_ranges: AtomicBool,
    omit_content_length: AtomicBool,
    post_handler: Mutex<Option<PostHandler>>,
}

fn response(request: &Request, status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Response {
    Response::new(&request.url, status, headers, Box::new(Cursor::new(body)))
}

fn header(name: &str, value: impl ToString) -> (String, String) {
    (name.to_string(), value.to_string())
}

impl StubTransport {
    fn maybe_truncate(&self, mut body: Vec<u8>) -> Vec<u8> {
        if self.truncations_left.fetch_sub(1, Ordering::SeqCst) > 0 {
            body.truncate(self.truncate_after_bytes.load(Ordering::SeqCst));
        }
        body
    }
}

impl Transport for StubTransport {
    fn send(&self, request: &Request) -> io::Result<Response> {
        let url = request.url.as_str();
        let range = request.header("range").map(str::to_string);
        let if_match = request.header("if-match").map(str::to_string);
        let if_unmodified_since = request.header("if-unmodified-since").map(str::to_string);
        self.requests.lock().push(StubRequest {
            url: url.to_string(),
            range: range.clone(),
            if_match: if_match.clone(),
            if_unmodified_since: if_unmodified_since.clone(),
        });
        if self.failures_left.fetch_sub(1, Ordering::SeqCst) > 0 {
            let status = self.failure_status.load(Ordering::SeqCst) as u16;
            return Ok(response(request, status, self.failure_headers.lock().clone(), vec![]));
        }
        if let (Some(body), Some(handler)) = (&request.body, &*self.post_handler.lock()) {
            let (status, bytes) = handler(body);
            return Ok(response(request, status, vec![], bytes));
        }
        if let Some(target) = self.redirects.lock().get(url) {
            return Ok(response(request, 302, vec![header("Location", target)], vec![]));
        }
        let files = self.files.lock();
        let Some(file) = files.get(url) else {
            return Ok(response(request, 404, vec![], vec![]));
        };
        let mut validators = vec![];
        if let Some(etag) = &file.etag {
            validators.push(header("ETag", etag));
        }
        if let Some(last_modified) = &file.last_modified {
            validators.push(header("Last-Modified", last_modified));
        }
        if (if_match.is_some() && if_match != file.etag)
            || (if_unmodified_since.is_some() && if_unmodified_since != file.last_modified)
        {
            return Ok(response(request, 412, vec![], vec![]));
        }
        let suffix_range = range.as_deref().is_some_and(|r| r.starts_with("bytes=-"));
        let Some(range) = range.filter(|_| {
            !self.ignore_ranges.load(Ordering::SeqCst)
                && !(self.ignore_suffix_ranges.load(Ordering::SeqCst) && suffix_range)
        }) else {
            let mut headers = validators;
            if !self.omit_content_length.load(Ordering::SeqCst) {
                headers.push(header("Content-Length", file.bytes.len()));
            }
            return Ok(response(request, 200, headers, self.maybe_truncate(file.bytes.clone())));
        };
        if self.reject_suffix_ranges.load(Ordering::SeqCst) && suffix_range {
            return Ok(response(request, 501, vec![], vec![]));
        }
        let parsed = range.strip_prefix("bytes=").and_then(|r| r.split_once('-')).and_then(|(from, to)| {
            let number = |s: &str| if s.is_empty() { Some(None) } else { s.parse::<usize>().ok().map(Some) };
            Some((number(from)?, number(to)?))
        });
        let Some((from, to)) = parsed.filter(|(from, to)| from.is_some() || to.is_some()) else {
            return Ok(response(request, 416, vec![], vec![]));
        };
        let length = file.bytes.len();
        let (start, end) = match (from, to) {
            (None, Some(suffix)) => (length.saturating_sub(suffix), length.saturating_sub(1)),
            (Some(from), to) => (from, to.map_or(length.saturating_sub(1), |to| to.min(length.saturating_sub(1)))),
            (None, None) => unreachable!(),
        };
        if start >= length && length > 0 {
            return Ok(response(request, 416, vec![], vec![]));
        }
        let mut headers = validators;
        headers.push(header("Content-Range", format!("bytes {start}-{end}/{length}")));
        let body = file.bytes[start.min(length)..(end + 1).min(length)].to_vec();
        Ok(response(request, 206, headers, self.maybe_truncate(body)))
    }

    fn retry_sleep(&self, wait: Duration) {
        self.retry_waits.lock().push(wait);
    }
}

/// A [`Downloader`] that talks to an in-memory server.
pub struct StubDownloader {
    downloader: Downloader,
    stub: Arc<StubTransport>,
}

impl Deref for StubDownloader {
    type Target = Downloader;

    fn deref(&self) -> &Downloader {
        &self.downloader
    }
}

impl StubDownloader {
    pub fn new(config: PlanetilerConfig) -> StubDownloader {
        let max_request_bytes = config.download_chunk_size_mb * 1_000_000;
        Self::with_max_request_bytes(config, max_request_bytes)
    }

    pub fn with_max_request_bytes(config: PlanetilerConfig, max_request_bytes: u64) -> StubDownloader {
        let stub = Arc::new(StubTransport::default());
        StubDownloader { downloader: Downloader::with_transport(config, max_request_bytes, stub.clone()), stub }
    }

    /// Returns the downloader, to pass to code under test.
    pub fn downloader(&self) -> Downloader {
        self.downloader.clone()
    }

    /// Serves `bytes` at `url` with an `ETag` and `Last-Modified` derived from the contents.
    pub fn put(&self, url: &str, bytes: &[u8]) -> &Self {
        let hash = bytes.iter().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(*b as u32));
        self.put_with_validators(
            url,
            bytes,
            Some(&format!("\"{hash:x}\"")),
            Some(&format!("Thu, 01 Jan 2026 00:00:{:02} GMT", hash % 60)),
        )
    }

    pub fn put_with_validators(
        &self,
        url: &str,
        bytes: &[u8],
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> &Self {
        self.stub.files.lock().insert(
            url.to_string(),
            StubFile {
                bytes: bytes.to_vec(),
                etag: etag.map(str::to_string),
                last_modified: last_modified.map(str::to_string),
            },
        );
        self
    }

    pub fn redirect(&self, from: &str, to: &str) -> &Self {
        self.stub.redirects.lock().insert(from.to_string(), to.to_string());
        self
    }

    /// Fails the next `count` requests with `status`.
    pub fn fail_next(&self, count: usize, status: u16) -> &Self {
        self.fail_next_with_headers(count, status, &[])
    }

    pub fn fail_next_with_headers(&self, count: usize, status: u16, headers: &[(&str, &str)]) -> &Self {
        self.stub.failure_status.store(status as usize, Ordering::SeqCst);
        *self.stub.failure_headers.lock() = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.stub.failures_left.store(count as i64, Ordering::SeqCst);
        self
    }

    /// Cuts off the bodies of the next `count` responses after `bytes`, like a dropped connection.
    pub fn truncate_next(&self, count: usize, bytes: usize) -> &Self {
        self.stub.truncate_after_bytes.store(bytes, Ordering::SeqCst);
        self.stub.truncations_left.store(count as i64, Ordering::SeqCst);
        self
    }

    /// Answers range requests with the whole file.
    pub fn ignore_ranges(&self) -> &Self {
        self.stub.ignore_ranges.store(true, Ordering::SeqCst);
        self
    }

    /// Answers suffix range requests (`bytes=-N`) with the whole file.
    pub fn ignore_suffix_ranges(&self) -> &Self {
        self.stub.ignore_suffix_ranges.store(true, Ordering::SeqCst);
        self
    }

    /// Answers suffix range requests with 501, like GitHub release downloads.
    pub fn reject_suffix_ranges(&self) -> &Self {
        self.stub.reject_suffix_ranges.store(true, Ordering::SeqCst);
        self
    }

    /// Leaves the `Content-Length` header off responses with the whole file.
    pub fn omit_content_length(&self) -> &Self {
        self.stub.omit_content_length.store(true, Ordering::SeqCst);
        self
    }

    /// Answers requests with a body (POSTs) with the status and body `handler` returns for it.
    pub fn respond_to_posts(&self, handler: impl Fn(&str) -> (u16, Vec<u8>) + Send + Sync + 'static) -> &Self {
        *self.stub.post_handler.lock() = Some(Box::new(handler));
        self
    }

    /// Returns the requests received so far.
    pub fn requests(&self) -> Vec<StubRequest> {
        self.stub.requests.lock().clone()
    }

    pub fn clear_requests(&self) {
        self.stub.requests.lock().clear();
    }

    /// Returns how long the downloader waited before each retry (without waiting).
    pub fn retry_waits(&self) -> Vec<Duration> {
        self.stub.retry_waits.lock().clone()
    }

    /// Returns just the `Range` header of each request received so far.
    pub fn ranges(&self) -> Vec<Option<String>> {
        self.requests().into_iter().map(|r| r.range).collect()
    }
}
