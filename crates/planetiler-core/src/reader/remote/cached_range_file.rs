//! Port of `reader/remote/CachedRangeFile.java`: a [`RangeFile`] that reads a remote file with HTTP range requests and
//! keeps the bytes it fetched in a local sparse file, so later runs read them from disk.
//!
//! The cache is the file at `path` plus two small files next to it: `path.meta` with the version of the remote file
//! (length, ETag, Last-Modified) and `path.ranges` listing the byte ranges fetched so far. Once every byte has been
//! fetched the `.ranges` file is deleted, leaving a complete copy of the remote file at `path`; `.meta` stays so a
//! later refresh can tell whether the remote file changed.

use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, bail};
use parking_lot::Mutex;

use crate::reader::remote::http_range_file::{ByteListener, HttpRangeFile};
use crate::reader::remote::range_file::{LazyPending, Pending, RangeFile};
use crate::util::downloader::{ByteRange, DownloadError, Downloader, RemoteVersion, download_error};
use crate::util::file_utils;
use crate::util::request_planner::Plan;

/// Fetched bytes must be on disk before their range gets recorded, or a crash could leave a range of zeros marked as
/// fetched. Syncing once per this many bytes instead of once per range keeps that cheap; a crash only loses the
/// ranges recorded since the last sync, which get fetched again.
const SYNC_BYTES: u64 = 64 << 20;

/// A set of `[start, end)` byte ranges where touching ranges are merged (like Guava's `TreeRangeSet`).
#[derive(Default, Clone, Debug)]
struct RangeSet {
    /// start -> end
    ranges: BTreeMap<u64, u64>,
}

impl RangeSet {
    fn add(&mut self, mut start: u64, mut end: u64) {
        if start >= end {
            return;
        }
        // merge with the range that starts before this one if it touches
        if let Some((&s, &e)) = self.ranges.range(..=start).next_back()
            && e >= start
        {
            start = s;
            end = end.max(e);
        }
        // merge with all the ranges that start inside it or right after it
        let overlapping: Vec<(u64, u64)> = self.ranges.range(start..=end).map(|(&s, &e)| (s, e)).collect();
        for (s, e) in overlapping {
            end = end.max(e);
            self.ranges.remove(&s);
        }
        self.ranges.insert(start, end);
    }

    /// Returns true if one range of the set contains all of `[start, end)`.
    fn encloses(&self, start: u64, end: u64) -> bool {
        start >= end || self.ranges.range(..=start).next_back().is_some_and(|(_, &e)| e >= end)
    }

    fn clear(&mut self) {
        self.ranges.clear();
    }

    fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Returns the parts of `[0, length)` not in this set.
    fn missing(&self, length: u64) -> Vec<ByteRange> {
        let mut result = vec![];
        let mut position = 0;
        for (&start, &end) in &self.ranges {
            if start >= length {
                break;
            }
            if start > position {
                result.push(ByteRange::new(position, start - position));
            }
            position = position.max(end);
        }
        if position < length {
            result.push(ByteRange::new(position, length - position));
        }
        result
    }
}

struct CacheState {
    /// can be set later, for example after a full download of a file with no content-length
    length: i64,
    cached: RangeSet,
    to_sync: RangeSet,
    unsynced_bytes: u64,
    ranges_file: Option<File>,
}

/// The local file and the ranges of it that have been fetched, shared with the callbacks that store fetched bytes.
struct Cache {
    ranges_path: PathBuf,
    channel: File,
    state: Mutex<CacheState>,
    syncing: AtomicBool,
    closed: AtomicBool,
}

impl Cache {
    fn is_cached(&self, offset: u64, length: u64) -> bool {
        self.state.lock().cached.encloses(offset, offset + length)
    }

    /// Stores `bytes`, which are at `position` in the remote file. Safe to call from several threads at once for
    /// different bytes.
    fn write(&self, position: u64, bytes: &[u8]) -> std::io::Result<()> {
        file_utils::write_fully(&self.channel, bytes, position)?;
        let needs_sync = {
            let mut state = self.state.lock();
            if state.ranges_file.is_none() {
                return Ok(());
            }
            let end = position + bytes.len() as u64;
            state.cached.add(position, end);
            state.to_sync.add(position, end);
            state.unsynced_bytes += bytes.len() as u64;
            if self.mark_finished_if_complete(&mut state)? {
                return Ok(());
            }
            state.unsynced_bytes >= SYNC_BYTES
        };
        // Syncing can take seconds while a download writes GB/s, so other requests keep writing meanwhile instead of
        // waiting on the lock, and skip syncing while another one does: the next sync records their ranges.
        if needs_sync && self.syncing.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            let result = self.sync();
            self.syncing.store(false, Ordering::SeqCst);
            result?;
        }
        Ok(())
    }

    // once every byte is here the file is a complete copy of the remote file; the caller holds the lock
    fn mark_finished_if_complete(&self, state: &mut CacheState) -> std::io::Result<bool> {
        // an unknown length (< 0) waits for set_length
        if state.length < 0 || (state.length > 0 && !state.cached.encloses(0, state.length as u64)) {
            return Ok(false);
        }
        self.channel.sync_data()?;
        state.ranges_file = None;
        match std::fs::remove_file(&self.ranges_path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        Ok(true)
    }

    fn sync(&self) -> std::io::Result<()> {
        let mut lines = String::new();
        {
            let mut state = self.state.lock();
            if state.ranges_file.is_none() || state.to_sync.is_empty() {
                return Ok(());
            }
            for (start, end) in &state.to_sync.ranges {
                lines.push_str(&format!("{start} {}\n", end - start));
            }
            state.to_sync.clear();
            state.unsynced_bytes = 0;
        }
        // the bytes of these ranges were written before they got added, so this puts all of them on disk
        self.channel.sync_data()?;
        let mut state = self.state.lock();
        if let Some(ranges_file) = &mut state.ranges_file {
            ranges_file.write_all(lines.as_bytes())?;
        }
        Ok(())
    }

    fn read(&self, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
        file_utils::read_fully(&self.channel, offset, length)
    }
}

/// A [`RangeFile`] that caches what it reads from a remote file on disk.
pub struct CachedRangeFile {
    path: PathBuf,
    remote: Option<HttpRangeFile>,
    cache: Arc<Cache>,
}

/// Returns the file that lists the cached ranges of a partially downloaded file at `path`.
fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

impl CachedRangeFile {
    fn new(
        path: &Path,
        remote: Option<HttpRangeFile>,
        length: i64,
        ranges: Vec<ByteRange>,
        new_file: bool,
    ) -> anyhow::Result<CachedRangeFile> {
        let ranges_path = Self::ranges_path(path);
        let mut cached = RangeSet::default();
        for range in ranges {
            cached.add(range.offset, range.end());
        }
        let channel = if new_file {
            OpenOptions::new().create_new(true).read(true).write(true).open(path)
        } else {
            OpenOptions::new().read(true).write(true).open(path)
        }
        .with_context(|| format!("opening {}", path.display()))?;
        let ranges_file = OpenOptions::new()
            .append(true)
            .open(&ranges_path)
            .with_context(|| format!("opening {}", ranges_path.display()))?;
        Ok(CachedRangeFile {
            path: path.to_path_buf(),
            remote,
            cache: Arc::new(Cache {
                ranges_path,
                channel,
                state: Mutex::new(CacheState {
                    length,
                    cached,
                    to_sync: RangeSet::default(),
                    unsynced_bytes: 0,
                    ranges_file: Some(ranges_file),
                }),
                syncing: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            }),
        })
    }

    /// Returns the file that lists the cached ranges of a partially downloaded file at `path`.
    pub fn ranges_path(path: &Path) -> PathBuf {
        sibling_with_suffix(path, ".ranges")
    }

    /// Returns the file that stores the version of the remote file cached at `path`.
    pub fn meta_path(path: &Path) -> PathBuf {
        sibling_with_suffix(path, ".meta")
    }

    /// Returns true if `path` is a partially downloaded file from a previous [`CachedRangeFile`].
    pub fn is_partial(path: &Path) -> bool {
        Self::ranges_path(path).exists()
    }

    /// Opens a cache of the remote file at `url` at `path`, reusing what a previous run fetched if the remote file
    /// has not changed since then, or starting over if it has. A complete file at `path` gets replaced. Starting over
    /// fetches the last `tail_bytes` bytes along the way (see [`HttpRangeFile::open`]).
    ///
    /// # Errors
    ///
    /// If the cache can't be read or created, or the remote file can't be opened, or with
    /// [`DownloadError::RangesNotSupported`] if the server does not support range requests.
    pub fn open(path: &Path, url: &str, tail_bytes: usize, downloader: &Downloader) -> anyhow::Result<CachedRangeFile> {
        if !Self::is_partial(path) {
            return Self::create_new_with_remote(path, HttpRangeFile::open(downloader, url, tail_bytes)?);
        }
        // reusing a partial cache only needs the remote file's version, the tail is in the cache already
        let remote = HttpRangeFile::open(downloader, url, 0)?;
        if let Some(previous) = read_meta(path)
            && path.exists()
            && remote.is_same_version(&previous)
        {
            return Self::new(path, Some(remote), previous.length, read_ranges(path)?, false);
        }
        tracing::info!("{url} changed since it was partially downloaded to {}, starting over", path.display());
        Self::create_new_with_remote(path, remote)
    }

    /// Returns true if `path` is a complete copy of the remote file at `url` and, with `refresh`, of its current
    /// version (comparing the size of the file when it has no version from a previous run).
    ///
    /// # Errors
    ///
    /// If `refresh` and the remote file can't be opened.
    pub fn is_complete_copy(path: &Path, url: &str, refresh: bool, downloader: &Downloader) -> anyhow::Result<bool> {
        Ok(path.exists() && !Self::is_partial(path) && (!refresh || is_up_to_date(path, url, downloader)?))
    }

    fn create_new_with_remote(path: &Path, remote: HttpRangeFile) -> anyhow::Result<CachedRangeFile> {
        let version = remote.version().clone();
        let result = Self::create_new(path, Some(remote.clone()), &version)?;
        // keep the bytes that came with the request that opened the file so they don't get fetched again
        let prefetched = remote.prefetched();
        result.write(prefetched.offset, &prefetched.bytes)?;
        Ok(result)
    }

    /// Creates an empty cache at `path` of `version` of a remote file, replacing what was there.
    pub fn create(path: &Path, version: &RemoteVersion) -> anyhow::Result<CachedRangeFile> {
        Self::create_new(path, None, version)
    }

    fn create_new(
        path: &Path,
        remote: Option<HttpRangeFile>,
        version: &RemoteVersion,
    ) -> anyhow::Result<CachedRangeFile> {
        file_utils::delete(&[path, Self::ranges_path(path).as_path(), Self::meta_path(path).as_path()]);
        file_utils::create_parent_directories(&[path])?;
        // the ranges file marks the cache as partial, so it gets created before any data is written
        std::fs::write(Self::ranges_path(path), "")?;
        std::fs::write(Self::meta_path(path), serde_json::to_vec(version)?)?;
        Self::new(path, remote, version.length, vec![], true)
    }

    /// Opens a cache at `path` left by a previous run without connecting to the remote server, so only the ranges
    /// fetched before can be read.
    ///
    /// # Errors
    ///
    /// If `path` is not a partially downloaded file.
    pub fn open_offline(path: &Path) -> anyhow::Result<CachedRangeFile> {
        let previous = read_meta(path);
        match previous {
            Some(previous) if Self::is_partial(path) => {
                Self::new(path, None, previous.length, read_ranges(path)?, false)
            }
            _ => bail!("{} is not a partially downloaded file", path.display()),
        }
    }

    /// Returns the URL of the remote file after following redirects.
    ///
    /// # Panics
    ///
    /// If this was opened offline.
    pub fn url(&self) -> &str {
        self.remote.as_ref().expect("no remote file").url()
    }

    /// Returns the length of the file in bytes (-1 if not known yet).
    pub fn length_bytes(&self) -> u64 {
        self.cache.state.lock().length.max(0) as u64
    }

    /// Returns the number of bytes of the file not fetched yet.
    pub fn bytes_missing(&self) -> u64 {
        self.missing_ranges().iter().map(|r| r.length).sum()
    }

    /// Fetches every byte not fetched yet, telling `progress` how many bytes arrived as they do, after which `path`
    /// is a complete copy of the remote file.
    ///
    /// # Errors
    ///
    /// If a request fails after all retries or the file can't be written.
    pub fn fetch_missing(&self, progress: Arc<dyn Fn(u64) + Send + Sync>) -> anyhow::Result<()> {
        let Some(remote) = &self.remote else { bail!("{} was opened offline", self.path.display()) };
        let cache = self.cache.clone();
        let listener: ByteListener = Arc::new(move |position, bytes| {
            cache.write(position, bytes)?;
            progress(bytes.len() as u64);
            Ok(())
        });
        remote.fetch(&self.missing_ranges(), listener)?.wait()
    }

    fn missing_ranges(&self) -> Vec<ByteRange> {
        let state = self.cache.state.lock();
        state.cached.missing(state.length.max(0) as u64)
    }

    /// Stores `bytes`, which are at `position` in the remote file. Safe to call from several threads at once for
    /// different bytes.
    pub fn write(&self, position: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.cache.write(position, bytes)
    }

    /// Sets the length of a cache [created](CachedRangeFile::create) without one to `length`, after every byte of the
    /// remote file has been written, which makes `path` a complete copy of it.
    pub fn set_length(&self, length: i64) -> anyhow::Result<()> {
        let mut state = self.cache.state.lock();
        let meta_path = Self::meta_path(&self.path);
        let version: RemoteVersion = serde_json::from_slice(&std::fs::read(&meta_path)?)?;
        std::fs::write(&meta_path, serde_json::to_vec(&RemoteVersion { length, ..version })?)?;
        state.length = length;
        if state.ranges_file.is_some() {
            self.cache.mark_finished_if_complete(&mut state)?;
        }
        Ok(())
    }

    fn read_later_from_disk(&self, range: ByteRange) -> Box<dyn Pending> {
        let (cache, path) = (self.cache.clone(), self.path.clone());
        // cached bytes stay cached, so only ranges missing while offline get here without being cached
        Box::new(LazyPending(move || {
            if cache.is_cached(range.offset, range.length) {
                Ok(cache.read(range.offset, usize::try_from(range.length)?)?)
            } else {
                bail!(
                    "{} is missing bytes {}-{}, run with --download to fetch them",
                    path.display(),
                    range.offset,
                    range.end()
                )
            }
        }))
    }
}

impl RangeFile for CachedRangeFile {
    // offline (no remote) nothing gets fetched: reading missing bytes fails instead
    fn needs_fetch(&self, offset: u64, length: u64) -> bool {
        self.remote.is_some() && !self.cache.is_cached(offset, length)
    }

    fn plan_fetch(&self, ranges: &[ByteRange]) -> Plan {
        match &self.remote {
            None => Plan::EMPTY,
            Some(remote) => {
                let missing: Vec<ByteRange> =
                    ranges.iter().copied().filter(|r| !self.cache.is_cached(r.offset, r.length)).collect();
                remote.plan_fetch(&missing)
            }
        }
    }

    fn bytes_fetched(&self) -> u64 {
        self.remote.as_ref().map_or(0, |r| r.bytes_fetched())
    }

    fn requests_made(&self) -> u64 {
        self.remote.as_ref().map_or(0, |r| r.requests_made())
    }

    fn length(&self) -> anyhow::Result<u64> {
        Ok(self.length_bytes())
    }

    fn read(&self, offset: u64, length: usize) -> anyhow::Result<Vec<u8>> {
        self.read_later(&[ByteRange::new(offset, length as u64)])?.remove(0).get()
    }

    fn read_later(&self, ranges: &[ByteRange]) -> anyhow::Result<Vec<Box<dyn Pending>>> {
        let mut from_remote: HashMap<ByteRange, Box<dyn Pending>> = HashMap::new();
        let missing: Vec<ByteRange> =
            ranges.iter().copied().filter(|r| !self.cache.is_cached(r.offset, r.length)).collect();
        if let Some(remote) = &self.remote
            && !missing.is_empty()
        {
            let cache = self.cache.clone();
            let listener: ByteListener = Arc::new(move |position, bytes| cache.write(position, bytes));
            let pending = remote.read_later_with(&missing, Some(listener))?;
            for (range, pending) in missing.into_iter().zip(pending) {
                from_remote.insert(range, pending);
            }
        }
        Ok(ranges
            .iter()
            .map(|range| from_remote.remove(range).unwrap_or_else(|| self.read_later_from_disk(*range)))
            .collect())
    }

    fn close(&self) -> anyhow::Result<()> {
        if self.cache.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(remote) = &self.remote {
            remote.close()?;
        }
        self.cache.sync()?;
        let mut state = self.cache.state.lock();
        // an empty file never gets a write that would finish it
        if state.ranges_file.is_some() {
            self.cache.mark_finished_if_complete(&mut state)?;
        }
        state.ranges_file = None;
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn describe(&self) -> String {
        self.remote.as_ref().map_or_else(|| self.path.display().to_string(), |r| r.describe())
    }
}

impl Drop for CachedRangeFile {
    fn drop(&mut self) {
        if let Err(e) = self.close() {
            tracing::warn!("Error closing {}: {e:#}", self.path.display());
        }
    }
}

fn is_up_to_date(path: &Path, url: &str, downloader: &Downloader) -> anyhow::Result<bool> {
    let local = match read_meta(path) {
        Some(meta) => meta,
        None => RemoteVersion { length: std::fs::metadata(path)?.len() as i64, etag: None, last_modified: None },
    };
    match HttpRangeFile::open(downloader, url, 0) {
        Ok(range_file) => {
            let same = range_file.is_same_version(&local);
            range_file.close()?;
            Ok(same)
        }
        Err(e) => match download_error(&e) {
            Some(DownloadError::RangesNotSupported { version, .. }) => Ok(downloader.is_same_version(&local, version)),
            _ => Err(e),
        },
    }
}

fn read_meta(path: &Path) -> Option<RemoteVersion> {
    serde_json::from_slice(&std::fs::read(CachedRangeFile::meta_path(path)).ok()?).ok()
}

fn read_ranges(path: &Path) -> anyhow::Result<Vec<ByteRange>> {
    let contents = std::fs::read_to_string(CachedRangeFile::ranges_path(path))?;
    let mut ranges = vec![];
    for line in contents.lines() {
        let parts: Vec<&str> = line.split(' ').collect();
        // a line cut short by a crash while it was being written can only under-report a range, which is safe
        if let [start, length] = parts[..]
            && let (Ok(start), Ok(length)) = (start.parse::<u64>(), length.parse::<u64>())
        {
            ranges.push(ByteRange::new(start, length));
        }
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_set() {
        let mut set = RangeSet::default();
        set.add(10, 20);
        set.add(30, 40);
        assert!(set.encloses(12, 18));
        assert!(!set.encloses(15, 35));
        assert_eq!(vec![ByteRange::new(0, 10), ByteRange::new(20, 10), ByteRange::new(40, 10)], set.missing(50));
        // touching ranges merge
        set.add(20, 30);
        assert!(set.encloses(10, 40));
        assert_eq!(vec![ByteRange::new(0, 10), ByteRange::new(40, 10)], set.missing(50));
        set.add(0, 100);
        assert!(set.missing(100).is_empty());
        assert_eq!(1, set.ranges.len());
        // empty ranges are always enclosed
        assert!(RangeSet::default().encloses(5, 5));
    }
}
