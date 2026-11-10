//! Port of `reader/remote/RangeFile.java`: random access reads from a local file, or a remote one using HTTP range
//! requests.
//!
//! Implementations are thread-safe. Unlike Java, a [`RangeFile`] is handled as an `Arc<dyn RangeFile>`.

use std::any::Any;
use std::path::Path;
use std::sync::Arc;

use crate::reader::remote::cached_range_file::CachedRangeFile;
use crate::reader::remote::download_mode::DownloadMode;
use crate::reader::remote::http_range_file::HttpRangeFile;
use crate::reader::remote::local_range_file::LocalRangeFile;
use crate::util::downloader::{ByteRange, DownloadError, Downloader, download_error};
use crate::util::request_planner::Plan;
use crate::worker::Done;

/// Random access reads from a local file, or a remote one using HTTP range requests.
pub trait RangeFile: Send + Sync + Any {
    /// Returns true if reading the `length` bytes at `offset` would fetch them from the remote file or false for
    /// local files and bytes cached already.
    fn needs_fetch(&self, _offset: u64, _length: u64) -> bool {
        false
    }

    /// Returns the requests [`RangeFile::read_later`] would make right now to read `ranges`, or an empty plan for
    /// local files.
    fn plan_fetch(&self, _ranges: &[ByteRange]) -> Plan {
        Plan::EMPTY
    }

    /// Returns the number of bytes fetched from the remote file so far, not counting retries; 0 for local files.
    fn bytes_fetched(&self) -> u64 {
        0
    }

    /// Returns the number of requests made to the remote file so far, not counting retries.
    fn requests_made(&self) -> u64 {
        0
    }

    /// Returns the size of the file in bytes.
    fn length(&self) -> anyhow::Result<u64>;

    /// Returns `length` bytes starting at `offset`.
    fn read(&self, offset: u64, length: usize) -> anyhow::Result<Vec<u8>>;

    /// Returns a handle per range in `ranges` (which must be in ascending order and must not overlap) to read its
    /// bytes later.
    ///
    /// Remote implementations fetch the ranges with the requests [`RequestPlanner`](crate::util::request_planner)
    /// plans for them, which can span several ranges. A request gets sent once the first range it covers
    /// [starts](Pending::start), and its bytes get read off the network only as far as the ranges that started, so
    /// callers start ranges in order shortly before they need them. Local implementations read the bytes when
    /// [`Pending::get`] is called.
    fn read_later(&self, ranges: &[ByteRange]) -> anyhow::Result<Vec<Box<dyn Pending>>>;

    /// Releases the file, failing reads that haven't started.
    fn close(&self) -> anyhow::Result<()>;

    /// For tests to check which kind of file was opened.
    fn as_any(&self) -> &dyn Any;

    /// Describes where this file is, for logs and error messages.
    fn describe(&self) -> String;
}

/// Bytes requested by [`RangeFile::read_later`].
pub trait Pending: Send + Sync {
    /// Says [`Pending::get`] will be called soon, so the bytes can start arriving now.
    fn start(&self) {}

    /// Starts these bytes if necessary, and returns when they have all arrived. Only the first call returns them.
    fn get(&self) -> anyhow::Result<Vec<u8>>;

    /// Completes after bytes are fetched.
    fn ready(&self) -> Done {
        Done::completed()
    }
}

/// A [`Pending`] that does its work in [`Pending::get`].
pub struct LazyPending<F: Fn() -> anyhow::Result<Vec<u8>> + Send + Sync>(pub F);

impl<F: Fn() -> anyhow::Result<Vec<u8>> + Send + Sync> Pending for LazyPending<F> {
    fn get(&self) -> anyhow::Result<Vec<u8>> {
        (self.0)()
    }
}

/// A [`Pending`] with bytes that are already there.
pub struct ReadyPending(pub parking_lot::Mutex<Option<Vec<u8>>>);

impl ReadyPending {
    pub fn new(bytes: Vec<u8>) -> ReadyPending {
        ReadyPending(parking_lot::Mutex::new(Some(bytes)))
    }
}

impl Pending for ReadyPending {
    fn get(&self) -> anyhow::Result<Vec<u8>> {
        Ok(self.0.lock().take().unwrap_or_default())
    }
}

/// Opens a source that should be at `path` on local disk.
///
/// Reads a partially downloaded file left by [`CachedRangeFile`] without connecting to the remote server.
///
/// # Errors
///
/// If `path` is a partially downloaded file that can't be read.
pub fn open_local(path: &Path) -> anyhow::Result<Arc<dyn RangeFile>> {
    // if download is disabled and partial file doesn't contain ranges we need, it will fail while reading
    if CachedRangeFile::is_partial(path) {
        Ok(Arc::new(CachedRangeFile::open_offline(path)?))
    } else {
        Ok(Arc::new(LocalRangeFile::new(path)))
    }
}

/// Opens a source with a local copy at `path` and a remote copy at `url`: reads it from `path` if it is complete, or
/// from `url` with range requests according to `mode` otherwise, fetching its last `tail_bytes` bytes with the
/// request that opens it (see [`HttpRangeFile::open`]).
///
/// With `refresh`, a complete local copy that a previous [`DownloadMode::Cached`] run fetched gets replaced if the
/// remote file changed since then. If the server does not support range requests, the whole file gets downloaded to
/// `path` first.
///
/// # Errors
///
/// If the remote file can't be opened.
pub fn open(
    path: &Path,
    url: &str,
    tail_bytes: usize,
    mode: DownloadMode,
    refresh: bool,
    downloader: &Downloader,
) -> anyhow::Result<Arc<dyn RangeFile>> {
    if !mode.fetch_while_processing() {
        return open_local(path);
    }
    let result: anyhow::Result<Arc<dyn RangeFile>> = (|| {
        if CachedRangeFile::is_complete_copy(path, url, refresh, downloader)? {
            return Ok(Arc::new(LocalRangeFile::new(path)) as Arc<dyn RangeFile>);
        }
        if mode == DownloadMode::Streaming {
            Ok(Arc::new(HttpRangeFile::open(downloader, url, tail_bytes)?))
        } else {
            Ok(Arc::new(CachedRangeFile::open(path, url, tail_bytes, downloader)?))
        }
    })();
    match result {
        Err(e) if matches!(download_error(&e), Some(DownloadError::RangesNotSupported { .. })) => {
            downloader.download_whole(url, path)?;
            Ok(Arc::new(LocalRangeFile::new(path)))
        }
        other => other,
    }
}
