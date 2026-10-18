//! Port of `collection/ExternalMergeSort.java`: writes [`SortableFeature`]s to disk and uses merge sort to
//! efficiently sort much more data than fits in RAM.
//!
//! Writers append features to a "chunk" file that can be sorted with a fixed amount of RAM, then start writing to a
//! new chunk. [`FeatureSort::sort`] groups small chunks together, then sorts each group in memory in parallel
//! (limiting the number of concurrent readers/writers) and writes it back out. Reads do a k-way merge of the sorted
//! chunks using a min-heap of the next value from each. Writers record a record boundary every 32 MB, so an
//! uncompressed, memory-mapped group is read back into memory by several threads (a single 880 MB chunk took 0.26s
//! on one thread for Switzerland).
//!
//! Chunk file format (internal, native to this port): for each feature `key: i64 LE`, `len: u32 LE`, `len` bytes.
//! With `compress` the whole file is gzip-compressed (Java uses snappy). With `mmap` chunks are read through a
//! read-only memory map with `madvise(MADV_SEQUENTIAL)`; writes always go through a large buffered writer (Java
//! writes through a mapped buffer, which has no benefit in Rust).

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, bounded};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use memmap2::Mmap;
use parking_lot::Mutex;
use rayon::slice::ParallelSliceMut;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::feature_sort::{FeatureIter, FeatureSort, FeatureSortWriter};
use super::long_merger;
use super::sortable_feature::SortableFeature;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::stats::process_info;
use crate::stats::progress_loggers::ProgressLoggers;
use crate::stats::stats::StatsRef;
use crate::util::async_file_writer::AsyncFileWriter;
use crate::util::file_utils::DiskBacked;
use crate::worker::work_queue::QueueHandle;
use crate::worker::{Done, panic_to_error};

const MAX_CHUNK_SIZE: u64 = 2_000_000_000; // 2GB
const WRITE_BUFFER_BYTES: usize = 1 << 20;
const READ_BUFFER_BYTES: usize = 1 << 16;

/// Target size of each chunk file written in parallel when a sorted group is large.
const WRITE_PIECE_BYTES: u64 = 128 << 20;

/// Splits sorted `entries` into at most `pieces` ranges of similar length, only between entries with different keys
/// (so features with the same key stay in one chunk, in sorted order).
fn split_at_key_changes(entries: &[Entry], pieces: usize) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::with_capacity(pieces);
    let mut start = 0;
    for k in 1..pieces {
        let mut end = (entries.len() * k / pieces).max(start);
        while end > start && end < entries.len() && entries[end - 1].key == entries[end].key {
            end += 1;
        }
        if end > start && end < entries.len() {
            ranges.push(start..end);
            start = end;
        }
    }
    ranges.push(start..entries.len());
    ranges
}

/// Estimated bytes of RAM needed to sort a feature with `len` value bytes (same estimate as Java, so chunk sizes
/// match).
#[inline]
fn bytes_in_memory(len: usize) -> u64 {
    // pointer to feature + feature object overhead + long sort key + byte array pointer + byte array overhead + bytes
    (8 + 16 + 8 + 8 + 24 + len) as u64
}

/// A chunk file on disk.
#[derive(Clone, Debug)]
struct Chunk {
    path: PathBuf,
    item_count: u64,
    bytes_in_memory: u64,
    /// Record boundaries about every [`SPLIT_BYTES`] of the file (excluding the start), recorded while writing so
    /// the chunk can be read back by several threads. Empty for chunks from a manifest or written by the sort.
    splits: Vec<Split>,
}

/// A record boundary in a chunk file: its byte offset, the index of the record that starts there, and the total
/// value bytes of the records before it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Split {
    file_offset: u64,
    item: u64,
    data_offset: u64,
}

/// Bytes of a chunk file between recorded [`Split`]s.
const SPLIT_BYTES: u64 = if cfg!(test) { 256 } else { 32 << 20 };
/// Record header size in chunk files: key (8 bytes) and value length (4 bytes).
const HEADER_BYTES: u64 = 12;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChunkInfo {
    filename: String,
    item_count: u64,
    bytes_in_memory: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeManifest {
    chunks: Vec<ChunkInfo>,
    total_features: u64,
}

/// A disk-backed external merge sort, see the [module docs](self).
pub struct ExternalMergeSort {
    dir: PathBuf,
    stats: StatsRef,
    chunk_size_limit: u64,
    workers: usize,
    features: AtomicU64,
    chunks: Mutex<Vec<Chunk>>,
    chunk_num: AtomicUsize,
    compress: bool,
    mmap_io: bool,
    parallel_sort: bool,
    madvise: bool,
    reader_limit: usize,
    writer_limit: usize,
    keep_files: bool,
    log_interval: Duration,
    sorted: bool,
}

impl ExternalMergeSort {
    /// Creates a sorter in `temp_dir` using settings from `config` (Java `ExternalMergeSort(Path, boolean, ...)`).
    pub fn new(
        temp_dir: &Path,
        reuse_existing: bool,
        config: &PlanetilerConfig,
        stats: StatsRef,
    ) -> anyhow::Result<ExternalMergeSort> {
        Self::with_options(
            temp_dir,
            config.threads,
            MAX_CHUNK_SIZE.min(process_info::get_max_memory_bytes() / 3),
            config.compress_temp_storage,
            config.mmap_temp_storage,
            true,
            true,
            reuse_existing,
            config,
            stats,
        )
    }

    /// Creates a sorter with explicit options.
    #[allow(clippy::too_many_arguments)]
    pub fn with_options(
        dir: &Path,
        workers: usize,
        chunk_size_limit: u64,
        compress: bool,
        mmap: bool,
        parallel_sort: bool,
        madvise: bool,
        reuse_existing: bool,
        config: &PlanetilerConfig,
        stats: StatsRef,
    ) -> anyhow::Result<ExternalMergeSort> {
        let mut mmap = mmap;
        if compress && mmap {
            warn!("--compress-temp option not supported with --mmap-temp, falling back to --mmap-temp=false");
            mmap = false;
        }
        let mem_limit = process_info::get_max_memory_bytes() / 3;
        if chunk_size_limit > mem_limit {
            anyhow::bail!("Not enough memory for chunkSize={chunk_size_limit} limit={mem_limit}");
        }
        let max_workers_based_on_memory = 1.max(mem_limit / chunk_size_limit.max(1)) as usize;
        let workers = workers.min(max_workers_based_on_memory).max(1);
        if reuse_existing {
            info!("Reusing merge sort feature map directory at {}", dir.display());
        } else {
            info!(
                "Using merge sort feature map, chunk size={}mb max workers={}",
                chunk_size_limit / 1_000_000,
                workers
            );
            if dir.exists() {
                std::fs::remove_dir_all(dir).with_context(|| format!("Unable to delete {dir:?}"))?;
            }
        }
        std::fs::create_dir_all(dir).with_context(|| format!("Unable to initialize feature DB directory {dir:?}"))?;
        Ok(ExternalMergeSort {
            dir: dir.to_path_buf(),
            stats,
            chunk_size_limit,
            workers,
            features: AtomicU64::new(0),
            chunks: Mutex::new(Vec::new()),
            chunk_num: AtomicUsize::new(0),
            compress,
            mmap_io: mmap,
            parallel_sort,
            madvise,
            reader_limit: config.sort_max_readers.max(1),
            writer_limit: config.sort_max_writers.max(1),
            keep_files: config.reuse_feature_db || reuse_existing,
            log_interval: config.log_interval,
            sorted: false,
        })
    }

    /// Number of chunks on disk.
    pub fn chunks(&self) -> usize {
        self.chunks.lock().len()
    }

    fn new_chunk_writer(&self) -> anyhow::Result<ChunkWriter> {
        let num = self.chunk_num.fetch_add(1, Ordering::Relaxed) + 1;
        let path = self.dir.join(format!("chunk{num}"));
        Ok(ChunkWriter {
            out: open_writer(&path, self.compress)?,
            path,
            item_count: 0,
            bytes_in_memory: 0,
            file_bytes: 0,
            data_bytes: 0,
            splits: Vec::new(),
        })
    }

    fn open_reader(&self, chunk: &Chunk) -> io::Result<ChunkReader> {
        ChunkReader::open(chunk, self.mmap_io, self.compress, self.madvise)
    }

    /// Reads all chunks in `group` into memory, sorts, and writes the result to the first chunk's path, or for large
    /// groups to several chunks with consecutive key ranges (the first chunk's path plus `-1`, `-2`, ...) written in
    /// parallel. Writing a sorted group is mostly memory copying into the page cache, which ran at ~1.3 GB/s on one
    /// thread and made the sort stage use ~2-3 cores.
    fn sort_group(
        &self,
        group: Vec<Chunk>,
        read_sem: &Semaphore,
        write_sem: &Semaphore,
        timing: &Timing,
    ) -> anyhow::Result<Vec<Chunk>> {
        let item_count: u64 = group.iter().map(|c| c.item_count).sum();
        let bytes: u64 = group.iter().map(|c| c.bytes_in_memory).sum();
        let data_bytes = bytes.saturating_sub(item_count * bytes_in_memory(0));

        let mut sortable = {
            let _permit = read_sem.acquire();
            let start = std::time::Instant::now();
            let mut sortable = SortableChunk {
                data: Vec::with_capacity(data_bytes as usize),
                entries: Vec::with_capacity(item_count as usize),
            };
            if self.mmap_io && !self.compress && group.iter().any(|c| !c.splits.is_empty()) {
                read_group_parallel(&group, self.madvise, &mut sortable)?;
            } else {
                for chunk in &group {
                    let mut reader =
                        self.open_reader(chunk).with_context(|| format!("Unable to open {:?}", chunk.path))?;
                    reader.read_all_into(&mut sortable).with_context(|| format!("Error reading {:?}", chunk.path))?;
                }
            }
            for other in &group[1..] {
                let _ = std::fs::remove_file(&other.path);
            }
            if sortable.entries.len() as u64 != item_count {
                anyhow::bail!("Expected {item_count} features in {:?} got {}", group[0].path, sortable.entries.len());
            }
            timing.reading.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
            sortable
        };

        let start = std::time::Instant::now();
        sortable.sort(self.parallel_sort);
        timing.sorting.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);

        let first = &group[0].path;
        let pieces = split_at_key_changes(&sortable.entries, (data_bytes / WRITE_PIECE_BYTES).clamp(1, 8) as usize);
        let write_piece = |k: usize, entries: &[Entry]| -> anyhow::Result<Chunk> {
            let _permit = write_sem.acquire();
            let start = std::time::Instant::now();
            let path = if k == 0 {
                first.clone()
            } else {
                let mut name = first.file_name().unwrap_or_default().to_os_string();
                name.push(format!("-{k}"));
                first.with_file_name(name)
            };
            let mut out = open_writer(&path, self.compress)?;
            let mut piece_bytes = 0;
            for e in entries {
                write_feature(&mut out, e.key as i64, sortable.value(e))?;
                piece_bytes += bytes_in_memory(e.len as usize);
            }
            finish_writer(out)?;
            timing.writing.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
            Ok(Chunk { path, item_count: entries.len() as u64, bytes_in_memory: piece_bytes, splits: Vec::new() })
        };
        if pieces.len() == 1 {
            let chunk = write_piece(0, &sortable.entries)?;
            return Ok(vec![Chunk { bytes_in_memory: bytes, ..chunk }]);
        }
        std::thread::scope(|s| {
            let handles: Vec<_> = pieces
                .iter()
                .enumerate()
                .map(|(k, range)| {
                    let entries = &sortable.entries[range.clone()];
                    let write_piece = &write_piece;
                    s.spawn(move || write_piece(k, entries))
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("sort writer panicked")).collect()
        })
    }
}

/// The groups of chunks waiting to be sorted, reported like a work queue in progress logs.
struct GroupQueue {
    groups: Mutex<std::vec::IntoIter<(usize, Vec<Chunk>)>>,
    capacity: usize,
}

impl QueueHandle for GroupQueue {
    fn get_pending(&self) -> usize {
        self.groups.lock().len()
    }

    fn get_capacity(&self) -> usize {
        self.capacity
    }

    fn close(&self) {}

    fn abort(&self) {}
}

/// Size of the sort's temporary directory, for progress logs.
struct DirSize(PathBuf);

impl DiskBacked for DirSize {
    fn disk_usage_bytes(&self) -> u64 {
        crate::util::file_utils::size(&self.0).unwrap_or(0)
    }
}

#[derive(Default)]
struct Timing {
    reading: AtomicU64,
    writing: AtomicU64,
    sorting: AtomicU64,
}

/// A counting semaphore built on a bounded channel of tokens.
struct Semaphore {
    tx: Sender<()>,
    rx: Receiver<()>,
}

impl Semaphore {
    fn new(permits: usize) -> Semaphore {
        let (tx, rx) = bounded(permits);
        for _ in 0..permits {
            tx.send(()).unwrap();
        }
        Semaphore { tx, rx }
    }

    fn acquire(&self) -> Permit<'_> {
        self.rx.recv().expect("semaphore channel open");
        Permit(self)
    }
}

struct Permit<'a>(&'a Semaphore);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let _ = self.0.tx.send(());
    }
}

/// Threads used to read one group of chunks.
const READ_THREADS: usize = 8;

/// Reads (uncompressed) `group` into `sortable` like reading each chunk in order with [`ChunkReader`], but with
/// several threads, each parsing the records between two of the chunks' recorded [`Split`]s straight into its part
/// of the (preallocated) value buffer and entry list.
fn read_group_parallel(group: &[Chunk], madvise: bool, sortable: &mut SortableChunk) -> anyhow::Result<()> {
    use std::mem::MaybeUninit;
    struct Task<'a> {
        bytes: &'a [u8],
        items: u64,
        data_base: u64,
        data: &'a mut [MaybeUninit<u8>],
        entries: &'a mut [MaybeUninit<Entry>],
    }
    let mut maps = Vec::with_capacity(group.len());
    for chunk in group {
        let file = File::open(&chunk.path).with_context(|| format!("Unable to open {:?}", chunk.path))?;
        let len = file.metadata()?.len();
        let map = if len == 0 {
            None
        } else {
            // SAFETY: chunk files are private temp files that are not modified while being read.
            let map = unsafe { Mmap::map(&file)? };
            if madvise {
                advise_sequential(&map);
            }
            Some(map)
        };
        let data_len = len
            .checked_sub(chunk.item_count * HEADER_BYTES)
            .with_context(|| format!("truncated chunk file {:?}", chunk.path))?;
        maps.push((map, len, data_len));
    }
    let total_items: u64 = group.iter().map(|c| c.item_count).sum();
    let total_data: u64 = maps.iter().map(|m| m.2).sum();
    let (data_start, entries_start) = (sortable.data.len(), sortable.entries.len());
    sortable.data.reserve(total_data as usize);
    sortable.entries.reserve(total_items as usize);
    let mut data_rest = &mut sortable.data.spare_capacity_mut()[..total_data as usize];
    let mut entries_rest = &mut sortable.entries.spare_capacity_mut()[..total_items as usize];
    let mut tasks = Vec::new();
    let mut data_base = data_start as u64;
    for (chunk, (map, len, data_len)) in group.iter().zip(&maps) {
        let bytes: &[u8] = map.as_deref().unwrap_or(&[]);
        let end = Split { file_offset: *len, item: chunk.item_count, data_offset: *data_len };
        let mut start = Split::default();
        for &split in chunk.splits.iter().chain(std::iter::once(&end)) {
            anyhow::ensure!(
                split.file_offset >= start.file_offset
                    && split.item >= start.item
                    && split.data_offset >= start.data_offset
                    && split.file_offset <= *len
                    && split.data_offset <= *data_len
                    && split.item <= chunk.item_count,
                "inconsistent splits for {:?}",
                chunk.path
            );
            let items = split.item - start.item;
            let data = split.data_offset - start.data_offset;
            let (task_data, d) = std::mem::take(&mut data_rest).split_at_mut(data as usize);
            let (task_entries, e) = std::mem::take(&mut entries_rest).split_at_mut(items as usize);
            (data_rest, entries_rest) = (d, e);
            tasks.push(Task {
                bytes: &bytes[start.file_offset as usize..split.file_offset as usize],
                items,
                data_base: data_base + start.data_offset,
                data: task_data,
                entries: task_entries,
            });
            start = split;
        }
        data_base += data_len;
    }
    let parse = |task: Task| -> io::Result<()> {
        let truncated = || io::Error::new(io::ErrorKind::UnexpectedEof, "truncated chunk file");
        let (mut pos, mut out) = (0usize, 0usize);
        for entry in task.entries.iter_mut() {
            let header = task.bytes.get(pos..pos + HEADER_BYTES as usize).ok_or_else(truncated)?;
            let key = i64::from_le_bytes(header[..8].try_into().unwrap());
            let len = u32::from_le_bytes(header[8..].try_into().unwrap());
            let start = pos + HEADER_BYTES as usize;
            let value = task.bytes.get(start..start + len as usize).ok_or_else(truncated)?;
            let dst = task.data.get_mut(out..out + value.len()).ok_or_else(truncated)?;
            for (d, v) in dst.iter_mut().zip(value) {
                d.write(*v);
            }
            entry.write(Entry { key: key as u64, offset: task.data_base + out as u64, len });
            pos = start + value.len();
            out += value.len();
        }
        if pos != task.bytes.len() || out != task.data.len() || task.entries.len() as u64 != task.items {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk split does not match its records"));
        }
        Ok(())
    };
    let queue = Mutex::new(tasks);
    let threads = READ_THREADS.min(queue.lock().len());
    let results: Vec<io::Result<()>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| -> io::Result<()> {
                    loop {
                        let Some(task) = queue.lock().pop() else { return Ok(()) };
                        parse(task)?;
                    }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("chunk reader panicked")).collect()
    });
    for result in results {
        result.with_context(|| format!("Error reading {:?}", group[0].path))?;
    }
    // SAFETY: every task succeeded, which means it initialized all of its data bytes and entries, and the tasks
    // cover the first `total_data` bytes and `total_items` entries of the spare capacity
    unsafe {
        sortable.data.set_len(data_start + total_data as usize);
        sortable.entries.set_len(entries_start + total_items as usize);
    }
    Ok(())
}

/// Best-fit-decreasing bin packing of chunks by estimated memory size.
fn bin_pack(items: Vec<Chunk>, max_bin_size: u64) -> Vec<Vec<Chunk>> {
    crate::util::bin_pack::pack(items, max_bin_size as i64, |c| c.bytes_in_memory as i64)
}

/// An entry of a chunk being sorted in memory: the key plus the location of the value bytes in a shared buffer.
struct Entry {
    key: u64,
    offset: u64,
    len: u32,
}

/// All features in a group of chunks read into memory for sorting, stored in one contiguous buffer to avoid a heap
/// allocation per feature.
struct SortableChunk {
    data: Vec<u8>,
    entries: Vec<Entry>,
}

impl SortableChunk {
    #[inline]
    fn value(&self, e: &Entry) -> &[u8] {
        &self.data[e.offset as usize..e.offset as usize + e.len as usize]
    }

    fn sort(&mut self, parallel: bool) {
        let data = &self.data;
        let cmp = |a: &Entry, b: &Entry| {
            a.key.cmp(&b.key).then_with(|| {
                data[a.offset as usize..a.offset as usize + a.len as usize]
                    .cmp(&data[b.offset as usize..b.offset as usize + b.len as usize])
            })
        };
        if parallel {
            self.entries.par_sort_unstable_by(cmp);
        } else {
            self.entries.sort_unstable_by(cmp);
        }
    }

    #[inline]
    fn push(&mut self, key: i64, value: &[u8]) {
        let offset = self.data.len() as u64;
        self.data.extend_from_slice(value);
        self.entries.push(Entry { key: key as u64, offset, len: value.len() as u32 });
    }
}

/// Output for a chunk file.
enum ChunkOut {
    Plain(AsyncFileWriter),
    Gzip(BufWriter<GzEncoder<File>>),
}

impl Write for ChunkOut {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            ChunkOut::Plain(w) => w.write(buf),
            ChunkOut::Gzip(g) => g.write(buf),
        }
    }

    #[inline]
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            ChunkOut::Plain(w) => w.write_all(buf),
            ChunkOut::Gzip(g) => g.write_all(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            ChunkOut::Plain(w) => w.flush(),
            ChunkOut::Gzip(g) => g.flush(),
        }
    }
}

fn open_writer(path: &Path, compress: bool) -> anyhow::Result<ChunkOut> {
    let file = File::create(path).with_context(|| format!("Unable to create {path:?}"))?;
    Ok(if compress {
        ChunkOut::Gzip(BufWriter::with_capacity(WRITE_BUFFER_BYTES, GzEncoder::new(file, Compression::fast())))
    } else {
        ChunkOut::Plain(AsyncFileWriter::new(file, WRITE_BUFFER_BYTES, "chunk-io")?)
    })
}

fn finish_writer(out: ChunkOut) -> anyhow::Result<()> {
    match out {
        ChunkOut::Plain(w) => w.finish()?,
        ChunkOut::Gzip(g) => {
            g.into_inner().map_err(|e| e.into_error())?.finish()?;
        }
    }
    Ok(())
}

#[inline]
fn write_feature(out: &mut impl Write, key: i64, value: &[u8]) -> io::Result<()> {
    let len: u32 = value.len().try_into().map_err(|_| io::Error::other("feature too large"))?;
    let mut header = [0u8; 12];
    header[..8].copy_from_slice(&key.to_le_bytes());
    header[8..].copy_from_slice(&len.to_le_bytes());
    out.write_all(&header)?;
    out.write_all(value)
}

/// The chunk a thread-local writer is currently writing to.
struct ChunkWriter {
    path: PathBuf,
    out: ChunkOut,
    item_count: u64,
    bytes_in_memory: u64,
    file_bytes: u64,
    data_bytes: u64,
    splits: Vec<Split>,
}

/// Writer that a single thread can use to write features independent of writers used in other threads.
struct ThreadLocalWriter<'a> {
    sorter: &'a ExternalMergeSort,
    current: Option<ChunkWriter>,
}

impl ThreadLocalWriter<'_> {
    fn finish_chunk(&mut self) -> anyhow::Result<()> {
        if let Some(chunk) = self.current.take() {
            finish_writer(chunk.out).with_context(|| format!("Error closing {:?}", chunk.path))?;
            self.sorter.chunks.lock().push(Chunk {
                path: chunk.path,
                item_count: chunk.item_count,
                bytes_in_memory: chunk.bytes_in_memory,
                splits: chunk.splits,
            });
        }
        Ok(())
    }
}

impl FeatureSortWriter for ThreadLocalWriter<'_> {
    #[inline]
    fn accept_parts(&mut self, key: i64, value: &[u8]) -> anyhow::Result<()> {
        let chunk = match &mut self.current {
            Some(c) => c,
            None => self.current.insert(self.sorter.new_chunk_writer()?),
        };
        if chunk.file_bytes >= (chunk.splits.len() as u64 + 1) * SPLIT_BYTES {
            chunk.splits.push(Split {
                file_offset: chunk.file_bytes,
                item: chunk.item_count,
                data_offset: chunk.data_bytes,
            });
        }
        write_feature(&mut chunk.out, key, value).with_context(|| format!("Error writing to {:?}", chunk.path))?;
        chunk.file_bytes += HEADER_BYTES + value.len() as u64;
        chunk.data_bytes += value.len() as u64;
        chunk.item_count += 1;
        chunk.bytes_in_memory += bytes_in_memory(value.len());
        self.sorter.features.fetch_add(1, Ordering::Relaxed);
        if chunk.bytes_in_memory > self.sorter.chunk_size_limit {
            self.finish_chunk()?;
        }
        Ok(())
    }

    fn close(&mut self) -> anyhow::Result<()> {
        self.finish_chunk()
    }
}

impl Drop for ThreadLocalWriter<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.finish_chunk() {
            warn!("Error closing feature sort chunk: {e:?}");
        }
    }
}

/// Reads features from a chunk file.
enum ChunkReader {
    Mmap { map: Option<Mmap>, pos: usize, remaining: u64 },
    Buffered { input: Box<dyn Read + Send>, remaining: u64 },
}

impl ChunkReader {
    fn open(chunk: &Chunk, mmap: bool, compress: bool, madvise: bool) -> io::Result<ChunkReader> {
        let file = File::open(&chunk.path)?;
        if mmap {
            let len = file.metadata()?.len();
            let map = if len == 0 {
                None
            } else {
                // SAFETY: chunk files are private temp files that are not modified while being read.
                let map = unsafe { Mmap::map(&file)? };
                if madvise {
                    advise_sequential(&map);
                }
                Some(map)
            };
            Ok(ChunkReader::Mmap { map, pos: 0, remaining: chunk.item_count })
        } else {
            let input: Box<dyn Read + Send> = if compress {
                Box::new(BufReader::with_capacity(READ_BUFFER_BYTES, GzDecoder::new(BufReader::new(file))))
            } else {
                Box::new(BufReader::with_capacity(READ_BUFFER_BYTES, file))
            };
            Ok(ChunkReader::Buffered { input, remaining: chunk.item_count })
        }
    }

    /// Reads the next feature, calling `f(key, value)` with a borrowed value, or returns `false` when done.
    #[inline]
    fn next_with(&mut self, scratch: &mut Vec<u8>, f: impl FnOnce(i64, &[u8])) -> io::Result<bool> {
        match self {
            ChunkReader::Mmap { map, pos, remaining } => {
                if *remaining == 0 {
                    return Ok(false);
                }
                let bytes: &[u8] = map.as_deref().unwrap_or(&[]);
                let truncated = || io::Error::new(io::ErrorKind::UnexpectedEof, "truncated chunk file");
                let header = bytes.get(*pos..*pos + 12).ok_or_else(truncated)?;
                let key = i64::from_le_bytes(header[..8].try_into().unwrap());
                let len = u32::from_le_bytes(header[8..].try_into().unwrap()) as usize;
                let value = bytes.get(*pos + 12..*pos + 12 + len).ok_or_else(truncated)?;
                *pos += 12 + len;
                *remaining -= 1;
                f(key, value);
                Ok(true)
            }
            ChunkReader::Buffered { input, remaining } => {
                if *remaining == 0 {
                    return Ok(false);
                }
                let mut header = [0u8; 12];
                input.read_exact(&mut header)?;
                let key = i64::from_le_bytes(header[..8].try_into().unwrap());
                let len = u32::from_le_bytes(header[8..].try_into().unwrap()) as usize;
                scratch.resize(len, 0);
                input.read_exact(scratch)?;
                *remaining -= 1;
                f(key, scratch);
                Ok(true)
            }
        }
    }

    fn read_all_into(&mut self, sortable: &mut SortableChunk) -> io::Result<()> {
        let mut scratch = Vec::new();
        while self.next_with(&mut scratch, |k, v| sortable.push(k, v))? {}
        Ok(())
    }
}

fn advise_sequential(map: &Mmap) {
    #[cfg(unix)]
    {
        static FAILED: AtomicBool = AtomicBool::new(false);
        if map.advise(memmap2::Advice::Sequential).is_err() && !FAILED.swap(true, Ordering::Relaxed) {
            info!("madvise not available on this system to speed up temporary feature IO.");
        }
    }
    #[cfg(not(unix))]
    let _ = map;
}

/// Iterator over the features in a chunk. Panics if the file cannot be read.
struct ChunkIter {
    reader: ChunkReader,
    path: PathBuf,
    scratch: Vec<u8>,
}

impl Iterator for ChunkIter {
    type Item = SortableFeature;

    #[inline]
    fn next(&mut self) -> Option<SortableFeature> {
        let mut result = None;
        match self
            .reader
            .next_with(&mut self.scratch, |key, value| result = Some(SortableFeature::new(key, value.to_vec())))
        {
            Ok(_) => result,
            Err(e) => panic!("Error reading feature chunk {:?}: {e}", self.path),
        }
    }
}

impl FeatureSort for ExternalMergeSort {
    fn sort(&mut self) -> anyhow::Result<()> {
        let timer = self.stats.start_stage("sort");
        let chunks = std::mem::take(self.chunks.get_mut());
        let num_chunks = chunks.len();

        // we may end up with many small chunks because each thread-local writer starts a new one so group together
        // smaller chunks that can be sorted together in-memory to minimize the number of chunks readers deal with
        let groups = bin_pack(chunks, self.chunk_size_limit);
        info!("Grouped {num_chunks} chunks into {}", groups.len());

        let read_sem = Semaphore::new(self.reader_limit);
        let write_sem = Semaphore::new(self.writer_limit);
        let timing = Timing::default();
        let done_counter = Arc::new(AtomicUsize::new(0));
        let num_groups = groups.len();
        let queue = Arc::new(GroupQueue {
            groups: Mutex::new(groups.into_iter().enumerate().collect::<Vec<_>>().into_iter()),
            capacity: num_groups,
        });
        let results: Mutex<Vec<Option<anyhow::Result<Vec<Chunk>>>>> =
            Mutex::new((0..num_groups).map(|_| None).collect());
        let failed = AtomicBool::new(false);
        let threads = self.workers.min(num_groups).max(1);
        let this = &*self;

        // Java runs this as a `sort` worker pipeline (`item_queue` -> `worker` threads); these are scoped threads
        // named like that pipeline's threads so they can borrow the sorter, and they log the same way
        let prefix = "sort_worker";
        self.stats.gauge_value(&format!("{prefix}_threads"), threads as f64);
        let loggers = {
            let done_counter = done_counter.clone();
            ProgressLoggers::create()
                .add_percent_counter("chunks", num_groups as i64, move || done_counter.load(Ordering::Relaxed) as i64)
                .add_file_size(Arc::new(DirSize(self.dir.clone())))
                .new_line()
                .add_process_stats()
                .new_line()
                .add_queue_stats(queue.clone())
                .add_thread_pool_stats("worker", &format!("{prefix}-"))
        };

        let result = std::thread::scope(|s| {
            let mut dones = Vec::with_capacity(threads);
            for i in 0..threads {
                let (queue, results, read_sem, write_sem, timing, done_counter, failed) =
                    (&queue, &results, &read_sem, &write_sem, &timing, &done_counter, &failed);
                let done = Done::new();
                dones.push(done.clone());
                let stats = &self.stats;
                let spawned =
                    std::thread::Builder::new().name(format!("{prefix}-{}", i + 1)).spawn_scoped(s, move || {
                        process_info::register_current_thread();
                        let start = std::time::Instant::now();
                        while !failed.load(Ordering::Relaxed) {
                            let Some((idx, group)) = queue.groups.lock().next() else { break };
                            let result =
                                catch_unwind(AssertUnwindSafe(|| this.sort_group(group, read_sem, write_sem, timing)))
                                    .unwrap_or_else(|payload| Err(panic_to_error(payload)));
                            if result.is_err() {
                                failed.store(true, Ordering::Relaxed);
                            }
                            results.lock()[idx] = Some(result);
                            done_counter.fetch_add(1, Ordering::Relaxed);
                        }
                        stats.timers().finished_worker_thread(prefix, start.elapsed());
                        done.complete(Ok(()));
                    });
                if let Err(e) = spawned {
                    failed.store(true, Ordering::Relaxed);
                    dones.last().unwrap().fail(anyhow::Error::new(e).context("Failed to spawn sort worker thread"));
                }
            }
            loggers.await_and_log(&Done::join_all(dones), this.log_interval)
        });
        result?;

        let mut sorted_chunks = Vec::with_capacity(num_groups);
        let mut sorted_groups = 0;
        for result in results.into_inner().into_iter().flatten() {
            sorted_chunks.extend(result?);
            sorted_groups += 1;
        }
        if sorted_groups != num_groups {
            anyhow::bail!("Sort did not complete: {sorted_groups} of {num_groups} chunks sorted");
        }
        *self.chunks.get_mut() = sorted_chunks;
        self.sorted = true;
        timer.stop();
        info!(
            "read:{:.2}s write:{:.2}s sort:{:.2}s (summed over threads)",
            Duration::from_nanos(timing.reading.load(Ordering::Relaxed)).as_secs_f64(),
            Duration::from_nanos(timing.writing.load(Ordering::Relaxed)).as_secs_f64(),
            Duration::from_nanos(timing.sorting.load(Ordering::Relaxed)).as_secs_f64()
        );
        Ok(())
    }

    fn num_features_written(&self) -> u64 {
        self.features.load(Ordering::Relaxed)
    }

    fn writer_for_thread(&self) -> anyhow::Result<Box<dyn FeatureSortWriter + '_>> {
        if self.sorted {
            anyhow::bail!("Cannot write after sorting");
        }
        Ok(Box::new(ThreadLocalWriter { sorter: self, current: None }))
    }

    fn iterator(&self, shard: usize, shards: usize) -> anyhow::Result<FeatureIter> {
        if !self.sorted {
            anyhow::bail!("Must call sort() before iterating");
        }
        if shard >= shards {
            anyhow::bail!("Bad shard params: shard={shard} shards={shards}");
        }
        let chunks = self.chunks.lock();
        // k-way merge to interleave all the sorted chunks
        let mut iterators = Vec::new();
        for chunk in chunks.iter().skip(shard).step_by(shards) {
            if chunk.item_count > 0 {
                let reader = self.open_reader(chunk).with_context(|| format!("Unable to open {:?}", chunk.path))?;
                iterators.push(ChunkIter { reader, path: chunk.path.clone(), scratch: Vec::new() });
            }
        }
        Ok(Box::new(long_merger::merge_iterators(iterators, SortableFeature::compare_bytes)))
    }

    fn chunks_to_read(&self) -> usize {
        self.chunks()
    }

    fn disk_usage_bytes(&self) -> u64 {
        crate::util::file_utils::size(&self.dir).unwrap_or(0)
    }

    fn save_manifest(&self, path: &Path) -> anyhow::Result<()> {
        let chunks = self
            .chunks
            .lock()
            .iter()
            .map(|c| ChunkInfo {
                filename: c.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default(),
                item_count: c.item_count,
                bytes_in_memory: c.bytes_in_memory,
            })
            .collect();
        let manifest = MergeManifest { chunks, total_features: self.num_features_written() };
        let file = File::create(path).with_context(|| format!("Unable to save chunk manifest to {path:?}"))?;
        serde_json::to_writer(BufWriter::new(file), &manifest)
            .with_context(|| format!("Unable to save chunk manifest to {path:?}"))?;
        Ok(())
    }

    fn init_from_manifest(&mut self, path: &Path) -> anyhow::Result<()> {
        let file = File::open(path).with_context(|| format!("Unable to load chunk manifest from {path:?}"))?;
        let manifest: MergeManifest = serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("Unable to load chunk manifest from {path:?}"))?;
        self.chunk_num.store(0, Ordering::Relaxed);
        self.features.store(manifest.total_features, Ordering::Relaxed);
        *self.chunks.get_mut() = manifest
            .chunks
            .into_iter()
            .map(|c| Chunk {
                path: self.dir.join(c.filename),
                item_count: c.item_count,
                bytes_in_memory: c.bytes_in_memory,
                splits: Vec::new(),
            })
            .collect();
        self.sorted = true;
        Ok(())
    }
}

impl Drop for ExternalMergeSort {
    fn drop(&mut self) {
        // Java marks chunk files deleteOnExit unless the feature DB is being reused
        if !self.keep_files {
            for chunk in self.chunks.get_mut().iter() {
                let _ = std::fs::remove_file(&chunk.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_at_key_changes() {
        let entries =
            |keys: &[u64]| -> Vec<Entry> { keys.iter().map(|&key| Entry { key, offset: 0, len: 0 }).collect() };
        assert_eq!(vec![0..0], split_at_key_changes(&[], 4));
        assert_eq!(vec![0..4], split_at_key_changes(&entries(&[1, 2, 3, 4]), 1));
        assert_eq!(vec![0..2, 2..4], split_at_key_changes(&entries(&[1, 2, 3, 4]), 2));
        // never splits a run of equal keys
        assert_eq!(vec![0..3, 3..4], split_at_key_changes(&entries(&[1, 1, 1, 2]), 2));
        assert_eq!(vec![0..4], split_at_key_changes(&entries(&[1, 1, 1, 1]), 3));
        assert_eq!(vec![0..1, 1..2, 2..3], split_at_key_changes(&entries(&[1, 2, 3]), 8));
    }
    use crate::collection::array_long_min_heap::tests::Rng;
    use crate::stats::stats::InMemoryStats;

    fn entry(i: i64) -> SortableFeature {
        SortableFeature::new(i64::MIN + i, vec![i as u8, (1 + i) as u8])
    }

    fn new_sorter(dir: &Path, workers: usize, chunk_size_limit: u64, gzip: bool, mmap: bool) -> ExternalMergeSort {
        let config = PlanetilerConfig::defaults();
        ExternalMergeSort::with_options(
            dir,
            workers,
            chunk_size_limit,
            gzip,
            mmap,
            true,
            true,
            false,
            &config,
            InMemoryStats::arc(),
        )
        .unwrap()
    }

    #[test]
    fn empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut sorter = new_sorter(dir.path(), 1, 100, false, false);
        sorter.sort().unwrap();
        assert_eq!(Vec::<SortableFeature>::new(), sorter.to_list());
    }

    #[test]
    fn single() {
        let dir = tempfile::tempdir().unwrap();
        let mut sorter = new_sorter(dir.path(), 1, 100, false, false);
        sorter.writer_for_thread().unwrap().accept(entry(1)).unwrap();
        sorter.sort().unwrap();
        assert_eq!(vec![entry(1)], sorter.to_list());
    }

    fn write_and_sort(sorter: &mut ExternalMergeSort, writers: &[&[i64]]) {
        {
            let mut ws: Vec<_> = writers.iter().map(|_| sorter.writer_for_thread().unwrap()).collect();
            for (w, items) in ws.iter_mut().zip(writers) {
                for &i in *items {
                    w.accept(entry(i)).unwrap();
                }
            }
        }
        sorter.sort().unwrap();
    }

    /// Chunks with recorded splits (every 256 bytes in tests) are read by several threads; the result is the same as
    /// reading them sequentially.
    #[test]
    fn parallel_read_of_split_chunks() {
        let mut rng = Rng::new(42);
        let features: Vec<Vec<SortableFeature>> = (0..3)
            .map(|_| {
                (0..2000)
                    .map(|_| {
                        let len = (rng.next_u64() % 40) as usize;
                        let value: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
                        SortableFeature::new((rng.next_u64() % 500) as i64, value)
                    })
                    .collect()
            })
            .collect();
        let mut expected: Vec<SortableFeature> = features.iter().flatten().cloned().collect();
        expected.sort_by(|a, b| a.key.cmp(&b.key).then_with(|| a.value.cmp(&b.value)));
        for mmap in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let mut sorter = new_sorter(dir.path(), 2, 1 << 30, false, mmap);
            {
                let mut ws: Vec<_> = features.iter().map(|_| sorter.writer_for_thread().unwrap()).collect();
                for (w, items) in ws.iter_mut().zip(&features) {
                    for f in items {
                        w.accept(f.clone()).unwrap();
                    }
                }
            }
            assert!(sorter.chunks.lock().iter().all(|c| c.splits.len() > 10));
            sorter.sort().unwrap();
            assert_eq!(expected, sorter.to_list(), "mmap={mmap}");
        }
    }

    #[test]
    fn two_items_one_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let mut sorter = new_sorter(dir.path(), 1, 100, false, false);
        write_and_sort(&mut sorter, &[&[2, 1]]);
        assert_eq!(vec![entry(1), entry(2)], sorter.to_list());
        assert_eq!(1, sorter.chunks());
    }

    #[test]
    fn two_items_two_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let mut sorter = new_sorter(dir.path(), 1, 0, false, false);
        write_and_sort(&mut sorter, &[&[2, 1]]);
        assert_eq!(vec![entry(1), entry(2)], sorter.to_list());
        assert_eq!(2, sorter.chunks());
    }

    #[test]
    fn two_workers() {
        let dir = tempfile::tempdir().unwrap();
        let mut sorter = new_sorter(dir.path(), 2, 0, false, false);
        write_and_sort(&mut sorter, &[&[4, 3, 2, 1]]);
        assert_eq!(vec![entry(1), entry(2), entry(3), entry(4)], sorter.to_list());
    }

    #[test]
    fn two_writers() {
        let dir = tempfile::tempdir().unwrap();
        let mut sorter = new_sorter(dir.path(), 2, 0, false, false);
        write_and_sort(&mut sorter, &[&[4, 3], &[2, 1]]);
        assert_eq!(vec![entry(1), entry(2), entry(3), entry(4)], sorter.to_list());
    }

    #[test]
    fn multiple_writers_that_get_combined() {
        let dir = tempfile::tempdir().unwrap();
        let mut sorter = new_sorter(dir.path(), 2, 2_000_000, false, false);
        write_and_sort(&mut sorter, &[&[4, 3], &[2, 1], &[5, 6]]);
        assert_eq!((1..=6).map(entry).collect::<Vec<_>>(), sorter.to_list());
        assert_eq!(1, sorter.chunks());
    }

    #[test]
    fn writers_from_threads() {
        let dir = tempfile::tempdir().unwrap();
        let mut sorter = new_sorter(dir.path(), 4, 2_000, false, true);
        std::thread::scope(|s| {
            for t in 0..4 {
                let sorter = &sorter;
                s.spawn(move || {
                    let mut w = sorter.writer_for_thread().unwrap();
                    for i in (0..1000).filter(|i| i % 4 == t) {
                        w.accept(entry(i)).unwrap();
                    }
                });
            }
        });
        sorter.sort().unwrap();
        assert_eq!(1000, sorter.num_features_written());
        assert_eq!((0..1000).map(entry).collect::<Vec<_>>(), sorter.to_list());
    }

    #[test]
    fn many_items() {
        for (gzip, mmap) in [(false, false), (false, true), (true, false), (true, true)] {
            let dir = tempfile::tempdir().unwrap();
            let sorted: Vec<SortableFeature> = (0..10_000).map(entry).collect();
            let mut shuffled = sorted.clone();
            let mut rng = Rng::new(0);
            for i in (1..shuffled.len()).rev() {
                shuffled.swap(i, rng.next_int(i + 1));
            }
            let mut sorter = new_sorter(dir.path(), 2, 20_000, gzip, mmap);
            {
                let mut writer = sorter.writer_for_thread().unwrap();
                for f in shuffled {
                    writer.accept(f).unwrap();
                }
            }
            sorter.sort().unwrap();
            assert_eq!(sorted, sorter.to_list(), "gzip={gzip} mmap={mmap}");
            // parallel iterator gives the same result
            let par = sorter.parallel_iterator(InMemoryStats::arc(), 3).unwrap();
            assert_eq!(sorted, par.collect::<Vec<_>>(), "gzip={gzip} mmap={mmap}");
            // shards cover everything
            let mut all: Vec<_> = (0..3).flat_map(|s| sorter.iterator(s, 3).unwrap()).collect();
            all.sort();
            assert_eq!(sorted, all);
        }
    }

    #[test]
    fn reuse_after_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("sort");
        let manifest_path = dir.path().join("test.manifest");
        let mut sorter = new_sorter(&tmp, 1, 100, false, false);
        write_and_sort(&mut sorter, &[&[3, 1, 2]]);
        let expected = sorter.to_list();
        sorter.save_manifest(&manifest_path).unwrap();
        assert!(manifest_path.exists());

        // a new instance in reuse mode must NOT delete the chunk directory
        let config = PlanetilerConfig::defaults();
        let mut reused = ExternalMergeSort::new(&tmp, true, &config, InMemoryStats::arc()).unwrap();
        reused.init_from_manifest(&manifest_path).unwrap();
        assert_eq!(expected, reused.to_list());
        assert_eq!(3, reused.num_features_written());
    }

    #[test]
    fn reuse_manifest_missing_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config = PlanetilerConfig::defaults();
        let mut reused = ExternalMergeSort::new(dir.path(), true, &config, InMemoryStats::arc()).unwrap();
        assert!(reused.init_from_manifest(&dir.path().join("nonexistent.manifest")).is_err());
    }

    #[test]
    fn bin_pack_groups() {
        let chunk = |b| Chunk { path: PathBuf::new(), item_count: 1, bytes_in_memory: b, splits: Vec::new() };
        let groups = bin_pack(vec![chunk(5), chunk(5), chunk(3), chunk(20)], 10);
        let sizes: Vec<u64> = groups.iter().map(|g| g.iter().map(|c| c.bytes_in_memory).sum()).collect();
        assert_eq!(vec![20, 10, 3], sizes);
    }
}
