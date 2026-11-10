//! Port of `reader/remote/HttpRangeFile.java`: a [`RangeFile`] that reads a remote file with HTTP range requests
//! through [`Downloader`], without storing anything on disk.
//!
//! Each response gets checked against the version of the file seen when it was opened, and reads fail with a
//! [`DownloadError::RemoteFileChanged`] if the file changed since then (only its length gets checked with
//! `download_fail_if_changed=false`).

use std::any::Any;
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{anyhow, bail};
use parking_lot::Mutex;

use crate::reader::remote::range_file::{Pending, RangeFile, ReadyPending};
use crate::util::completable_futures::Semaphore;
use crate::util::downloader::{ByteRange, Downloader, RangeResponse, RemoteVersion};
use crate::util::request_planner::Plan;
use crate::worker::{Done, SharedError};

/// Receives every byte fetched, including gaps between requested ranges: its position in the file and the bytes.
pub type ByteListener = Arc<dyn Fn(u64, &[u8]) -> io::Result<()> + Send + Sync>;

struct Http {
    downloader: Downloader,
    url: String,
    version: RemoteVersion,
    prefetched: RangeResponse,
    bytes_fetched: AtomicU64,
    requests_made: AtomicU64,
    in_progress: Mutex<Vec<Arc<ResultDispatcher>>>,
}

/// A remote file read with HTTP range requests. Clones share the same file.
#[derive(Clone)]
pub struct HttpRangeFile(Arc<Http>);

impl HttpRangeFile {
    /// Opens `url` for reading, fetching its length and validators with a request for its last `tail_bytes` bytes (or
    /// its first byte if `tail_bytes` is 0), for formats like GeoParquet that keep their metadata at the end of the
    /// file.
    ///
    /// Fails with [`DownloadError::RangesNotSupported`](crate::util::downloader::DownloadError) if the server does
    /// not support range requests.
    pub fn open(downloader: &Downloader, url: &str, tail_bytes: usize) -> anyhow::Result<HttpRangeFile> {
        let prefetched = if tail_bytes > 0 {
            downloader.fetch_tail(url, tail_bytes)?
        } else {
            downloader.fetch_range(url, ByteRange::new(0, 1))?
        };
        Ok(HttpRangeFile(Arc::new(Http {
            downloader: downloader.clone(),
            url: prefetched.url.clone(),
            version: prefetched.version.clone(),
            bytes_fetched: AtomicU64::new(prefetched.bytes.len() as u64),
            requests_made: AtomicU64::new(1),
            prefetched,
            in_progress: Mutex::new(vec![]),
        })))
    }

    /// Returns the URL of the file after following redirects.
    pub fn url(&self) -> &str {
        &self.0.url
    }

    /// Returns the version of the remote file seen when it was opened.
    pub fn version(&self) -> &RemoteVersion {
        &self.0.version
    }

    /// Returns true if `other` is the same version of the file as this one: same length and, unless
    /// `download_fail_if_changed=false`, the same validators.
    pub fn is_same_version(&self, other: &RemoteVersion) -> bool {
        self.0.downloader.is_same_version(&self.0.version, other)
    }

    /// Returns the bytes fetched by the request that opened the file.
    pub fn prefetched(&self) -> &RangeResponse {
        &self.0.prefetched
    }

    fn is_prefetched(&self, offset: u64, length: u64) -> bool {
        let prefetched = &self.0.prefetched;
        offset >= prefetched.offset && offset + length <= prefetched.offset + prefetched.bytes.len() as u64
    }

    fn file_length(&self) -> u64 {
        self.0.version.length.max(0) as u64
    }

    /// Like [`RangeFile::read_later`], but also passes every byte fetched to `byte_listener`, including gaps between
    /// ranges (e.g. for a [`CachedRangeFile`](crate::reader::remote::cached_range_file::CachedRangeFile) to store
    /// them on disk).
    pub fn read_later_with(
        &self,
        ranges: &[ByteRange],
        byte_listener: Option<ByteListener>,
    ) -> anyhow::Result<Vec<Box<dyn Pending>>> {
        let mut result: Vec<Box<dyn Pending>> = vec![];
        let mut ranges_to_fetch = vec![];
        for range in ranges {
            if range.end() > self.file_length() {
                bail!("Range {range} out of bounds for {} ({} bytes)", self.0.url, self.0.version.length);
            }
            if self.is_prefetched(range.offset, range.length) {
                let start = (range.offset - self.0.prefetched.offset) as usize;
                result.push(Box::new(ReadyPending::new(
                    self.0.prefetched.bytes[start..start + range.length as usize].to_vec(),
                )));
            } else {
                let to_fetch = Arc::new(RequestedRange::new(*range));
                ranges_to_fetch.push(to_fetch.clone());
                result.push(Box::new(to_fetch));
            }
        }
        let dispatcher = Arc::new(ResultDispatcher::new(ranges_to_fetch, byte_listener));
        self.0.in_progress.lock().push(dispatcher.clone());
        let needed: Vec<ByteRange> = dispatcher.requested.iter().map(|r| r.range).collect();
        let plan = match self.0.downloader.planner().plan(&needed, self.file_length()) {
            Ok(plan) => plan,
            Err(e) => {
                self.0.in_progress.lock().retain(|d| !Arc::ptr_eq(d, &dispatcher));
                return Err(e);
            }
        };
        let done = self.fetch_plan(plan, dispatcher.clone());
        let file = self.0.clone();
        done.on_complete(move |outcome| {
            file.in_progress.lock().retain(|d| !Arc::ptr_eq(d, &dispatcher));
            if let Err(error) = outcome {
                dispatcher.fail(error.clone());
            }
        });
        Ok(result)
    }

    /// Starts fetching `ranges` of the file in the coalesced ranges the
    /// [`RequestPlanner`](crate::util::request_planner::RequestPlanner) plans for them, passing every byte fetched
    /// to `byte_listener` as it arrives, including gaps between ranges.
    pub fn fetch(&self, ranges: &[ByteRange], byte_listener: ByteListener) -> anyhow::Result<Done> {
        let plan = self.0.downloader.planner().plan(ranges, self.file_length())?;
        Ok(self.fetch_plan(plan, Arc::new(ResultDispatcher::new(vec![], Some(byte_listener)))))
    }

    /// Requests the coalesced ranges of `plan` in file order, at most `plan.max_in_flight` at once, each once the
    /// caller starts the first requested range of `dispatcher` in it.
    fn fetch_plan(&self, plan: Plan, dispatcher: Arc<ResultDispatcher>) -> Done {
        if plan.http_requests.is_empty() {
            return Done::completed();
        }
        let file = self.0.clone();
        let downloader = self.0.downloader.clone();
        downloader.clone().spawn(move || {
            let in_flight = Arc::new(Semaphore::new(plan.max_in_flight));
            let mut requests = vec![];
            // planner coalesces small requested ranges into fewer, larger ranges (potentially with gaps in them)
            for coalesced_range in plan.http_requests {
                // wait for caller to call start() on a range inside this coalesced range before taking a slot or
                // allocating memory
                let mut splitter = dispatcher.await_splitter(coalesced_range)?;
                let permit = in_flight.acquire_owned();
                // when a request fails for good, stop trying the rest of this file
                if dispatcher.failed.load(Ordering::SeqCst) {
                    break;
                }
                file.requests_made.fetch_add(1, Ordering::Relaxed);
                let (file, dispatcher, downloader) = (file.clone(), dispatcher.clone(), downloader.clone());
                requests.push(downloader.clone().spawn(move || {
                    let result = downloader.fetch(&file.url, &file.version, coalesced_range, &mut |position, chunk| {
                        file.bytes_fetched.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                        splitter(position, chunk)
                    });
                    // fail the requested ranges before freeing the slot so the next request sees it
                    let result = result.map_err(|e| {
                        let shared = SharedError(Arc::new(e));
                        dispatcher.fail(shared.clone());
                        anyhow::Error::new(shared)
                    });
                    drop(permit);
                    result
                }));
            }
            Done::join_all(requests).wait()
        })
    }
}

impl RangeFile for HttpRangeFile {
    fn needs_fetch(&self, offset: u64, length: u64) -> bool {
        !self.is_prefetched(offset, length)
    }

    fn plan_fetch(&self, ranges: &[ByteRange]) -> Plan {
        let needed: Vec<ByteRange> = ranges.iter().copied().filter(|r| self.needs_fetch(r.offset, r.length)).collect();
        self.0.downloader.planner().plan(&needed, self.file_length()).expect("ranges to fetch must be valid")
    }

    fn bytes_fetched(&self) -> u64 {
        self.0.bytes_fetched.load(Ordering::Relaxed)
    }

    fn requests_made(&self) -> u64 {
        self.0.requests_made.load(Ordering::Relaxed)
    }

    fn length(&self) -> anyhow::Result<u64> {
        Ok(self.file_length())
    }

    fn read(&self, offset: u64, length: usize) -> anyhow::Result<Vec<u8>> {
        self.read_later(&[ByteRange::new(offset, length as u64)])?.remove(0).get()
    }

    fn read_later(&self, ranges: &[ByteRange]) -> anyhow::Result<Vec<Box<dyn Pending>>> {
        self.read_later_with(ranges, None)
    }

    /// Fails the requested ranges still waiting for their bytes; requests in flight stop once they get to one.
    fn close(&self) -> anyhow::Result<()> {
        let dispatchers = self.0.in_progress.lock().clone();
        for dispatcher in dispatchers {
            dispatcher.fail(SharedError(Arc::new(anyhow!("{} closed", self.0.url))));
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn describe(&self) -> String {
        self.0.url.clone()
    }
}

/// Hands the bytes of the coalesced ranges fetched for one `read_later` call to the requested ranges they cover, and
/// every byte, gaps included, to `byte_listener` if present.
struct ResultDispatcher {
    requested: Vec<Arc<RequestedRange>>,
    byte_listener: Option<ByteListener>,
    failed: AtomicBool,
}

fn overlap(range: ByteRange, from_file_position: u64, to_file_position: u64) -> u64 {
    range.end().min(to_file_position).saturating_sub(range.offset.max(from_file_position))
}

impl ResultDispatcher {
    fn new(requested: Vec<Arc<RequestedRange>>, byte_listener: Option<ByteListener>) -> ResultDispatcher {
        ResultDispatcher { requested, byte_listener, failed: AtomicBool::new(false) }
    }

    /// Waits until the caller starts the first requested range in `coalesced_range`, then returns a sink that splits
    /// its bytes between `byte_listener` and the requested ranges it covers.
    fn await_splitter(
        &self,
        coalesced_range: ByteRange,
    ) -> anyhow::Result<impl FnMut(u64, &[u8]) -> io::Result<()> + Send + use<>> {
        let mut covered: VecDeque<Arc<RequestedRange>> = self
            .requested
            .iter()
            .filter(|requested| overlap(requested.range, coalesced_range.offset, coalesced_range.end()) > 0)
            .cloned()
            .collect();
        if let Some(first) = covered.front() {
            first.started.wait()?;
        }
        let byte_listener = self.byte_listener.clone();
        Ok(move |file_position: u64, chunk: &[u8]| {
            if let Some(listener) = &byte_listener {
                listener(file_position, chunk)?;
            }
            let chunk_end = file_position + chunk.len() as u64;
            while let Some(requested) = covered.front() {
                if requested.range.offset >= chunk_end {
                    break;
                }
                let copy_from = file_position.max(requested.range.offset);
                let offset_in_chunk = (copy_from - file_position) as usize;
                let copy_length = overlap(requested.range, file_position, chunk_end) as usize;
                requested.accept(copy_from, &chunk[offset_in_chunk..offset_in_chunk + copy_length])?;
                if requested.range.end() > chunk_end {
                    // the rest of this range comes in the next chunk
                    break;
                }
                // bytes arrive in order, so a range that ended in this chunk gets nothing from the chunks after it
                covered.pop_front();
            }
            Ok(())
        })
    }

    fn fail(&self, error: SharedError) {
        self.failed.store(true, Ordering::SeqCst);
        for requested in &self.requested {
            requested.fail(error.clone());
        }
    }
}

struct RangeBytes {
    bytes: Option<Vec<u8>>,
    remaining: u64,
}

/// One range requested by a caller, which may be a small part of a larger coalesced range request made to the remote
/// server.
struct RequestedRange {
    range: ByteRange,
    started: Done,
    done: Done,
    state: Mutex<RangeBytes>,
}

impl RequestedRange {
    fn new(range: ByteRange) -> RequestedRange {
        let requested = RequestedRange {
            range,
            started: Done::new(),
            done: Done::new(),
            state: Mutex::new(RangeBytes { bytes: None, remaining: range.length }),
        };
        if range.length == 0 {
            requested.state.lock().bytes = Some(vec![]);
            requested.done.complete(Ok(()));
        }
        requested
    }

    fn accept(&self, file_position: u64, chunk: &[u8]) -> io::Result<()> {
        // block the HTTP request fetching these bytes until the caller calls start()
        self.started.wait().map_err(io::Error::other)?;
        let mut state = self.state.lock();
        let length = self.range.length as usize;
        let bytes = state.bytes.get_or_insert_with(|| vec![0u8; length]);
        let offset_in_range = (file_position - self.range.offset) as usize;
        bytes[offset_in_range..offset_in_range + chunk.len()].copy_from_slice(chunk);
        state.remaining -= chunk.len() as u64;
        if state.remaining == 0 {
            drop(state);
            self.done.complete(Ok(()));
        }
        Ok(())
    }

    fn fail(&self, error: SharedError) {
        self.started.complete(Err(error.clone()));
        self.done.complete(Err(error));
    }
}

impl Pending for Arc<RequestedRange> {
    fn start(&self) {
        self.started.complete(Ok(()));
    }

    fn ready(&self) -> Done {
        self.done.clone()
    }

    fn get(&self) -> anyhow::Result<Vec<u8>> {
        self.start();
        self.done.wait()?;
        Ok(self.state.lock().bytes.take().unwrap_or_default())
    }
}
