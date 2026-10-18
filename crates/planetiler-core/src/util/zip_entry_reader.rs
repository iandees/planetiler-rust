//! Fast sequential readers for large zip entries (no Java equivalent; Java reads zip entries through a zip
//! `FileSystem`).
//!
//! [`ZipEntryReader`] memory-maps the zip file and inflates one entry straight from the mapped compressed bytes (no
//! intermediate `BufReader` copies), verifying its CRC-32 and size at the end like the `zip` crate does.
//! [`ReadAhead`] moves a reader onto a background thread that fills large chunks ahead of the consumer, so that
//! inflating (which is inherently sequential) overlaps with whatever the consumer does with the bytes (parsing
//! records, writing them to disk).

use std::io::{self, Read};
use std::path::Path;

use std::sync::Arc;

use anyhow::{Context, bail};
use crossbeam_channel::{Receiver, Sender};

use crate::util::parallel_inflate::{InflateInput, ParallelInflater};

/// Compressed bytes per chunk when inflating in parallel.
const PARALLEL_CHUNK_SIZE: usize = 1 << 20;

/// Compressed bytes of a zip entry in a zip file, memory-mapped piece by piece.
pub struct MappedEntry {
    file: std::fs::File,
    start: u64,
    len: usize,
}

impl MappedEntry {
    fn map(&self, range: std::ops::Range<usize>) -> io::Result<memmap2::Mmap> {
        if range.is_empty() {
            return memmap2::MmapOptions::new().len(0).map_anon().map(|m| m.make_read_only())?;
        }
        // SAFETY: the zip file is only read; like every mmap-based reader this assumes it is not truncated while open
        unsafe { memmap2::MmapOptions::new().offset(self.start + range.start as u64).len(range.len()).map(&self.file) }
    }
}

impl InflateInput for MappedEntry {
    type View = memmap2::Mmap;

    fn byte_len(&self) -> usize {
        self.len
    }

    fn view(self: &Arc<Self>, range: std::ops::Range<usize>) -> io::Result<memmap2::Mmap> {
        self.map(range)
    }
}

enum Inflate {
    Stored,
    Stream(Box<flate2::Decompress>),
    Parallel(Box<ParallelInflater<MappedEntry>>),
}

/// Only inflate entries in parallel when they are at least this big (compressed).
const MIN_PARALLEL_SIZE: usize = 16 << 20;

/// Reads the uncompressed contents of one stored or deflated zip entry from a memory-mapped zip file.
pub struct ZipEntryReader {
    /// The whole entry, for stored and streamed entries.
    data: memmap2::Mmap,
    /// Read position in `data`.
    pos: usize,
    inflate: Inflate,
    crc: crc32fast::Hasher,
    expected_crc: u32,
    expected_size: u64,
    size: u64,
    done: bool,
}

impl ZipEntryReader {
    /// Opens entry `entry` (leading `/` ignored) of the zip file at `zip_path`, inflating on the calling thread.
    pub fn open(zip_path: &Path, entry: &str) -> anyhow::Result<ZipEntryReader> {
        Self::open_with_threads(zip_path, entry, 1)
    }

    /// Opens entry `entry` of the zip file at `zip_path`; large deflated entries are inflated by `threads` background
    /// threads ([`ParallelInflater`]) when `threads > 1`.
    pub fn open_with_threads(zip_path: &Path, entry: &str, threads: usize) -> anyhow::Result<ZipEntryReader> {
        Self::open_with(zip_path, entry, threads, MIN_PARALLEL_SIZE)
    }

    fn open_with(zip_path: &Path, entry: &str, threads: usize, min_parallel: usize) -> anyhow::Result<ZipEntryReader> {
        let file = std::fs::File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
        let file_len = file.metadata()?.len();
        let (start, compressed, size, crc, method) = {
            let mut archive = zip::ZipArchive::new(io::BufReader::new(&file))?;
            let entry = archive.by_name(entry.trim_start_matches('/'))?;
            if entry.encrypted() {
                bail!("Encrypted zip entries are not supported: {}", entry.name());
            }
            (entry.data_start(), entry.compressed_size() as usize, entry.size(), entry.crc32(), entry.compression())
        };
        start.checked_add(compressed as u64).filter(|&e| e <= file_len).context("zip entry out of bounds")?;
        let parallel = threads > 1 && compressed >= min_parallel && method == zip::CompressionMethod::Deflated;
        let entry_data = Arc::new(MappedEntry { file, start, len: compressed });
        // the parallel inflater maps the parts it needs itself
        let data = entry_data.map(0..if parallel { 0 } else { compressed })?;
        #[cfg(unix)]
        let _ = data.advise(memmap2::Advice::Sequential);
        let inflate = match method {
            zip::CompressionMethod::Stored => Inflate::Stored,
            zip::CompressionMethod::Deflated if parallel => {
                Inflate::Parallel(Box::new(ParallelInflater::new(entry_data, threads, PARALLEL_CHUNK_SIZE)))
            }
            zip::CompressionMethod::Deflated => Inflate::Stream(Box::new(flate2::Decompress::new(false))),
            other => bail!("Unsupported zip compression method {other:?} for {entry}"),
        };
        Ok(ZipEntryReader {
            data,
            pos: 0,
            inflate,
            crc: crc32fast::Hasher::new(),
            expected_crc: crc,
            expected_size: size,
            size: 0,
            done: false,
        })
    }

    /// Uncompressed size of the entry, from the zip directory.
    pub fn uncompressed_size(&self) -> u64 {
        self.expected_size
    }

    fn finish(&mut self) -> io::Result<usize> {
        self.done = true;
        let crc = std::mem::take(&mut self.crc).finalize();
        if self.size != self.expected_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("zip entry size mismatch: expected {} got {}", self.expected_size, self.size),
            ));
        }
        if crc != self.expected_crc {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "zip entry CRC-32 mismatch"));
        }
        Ok(0)
    }
}

impl Read for ZipEntryReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        let data: &[u8] = &self.data;
        let n = match &mut self.inflate {
            Inflate::Stored => {
                let n = buf.len().min(data.len() - self.pos);
                buf[..n].copy_from_slice(&data[self.pos..self.pos + n]);
                self.pos += n;
                n
            }
            Inflate::Stream(inflate) => loop {
                let (in_before, out_before) = (inflate.total_in(), inflate.total_out());
                let status = inflate
                    .decompress(&data[self.pos..], buf, flate2::FlushDecompress::None)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                self.pos += (inflate.total_in() - in_before) as usize;
                let n = (inflate.total_out() - out_before) as usize;
                if n > 0 || status == flate2::Status::StreamEnd {
                    break n;
                }
                if self.pos >= data.len() {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated deflate stream in zip entry"));
                }
            },
            Inflate::Parallel(inflater) => inflater.read(buf)?,
        };
        if n == 0 {
            return self.finish();
        }
        self.crc.update(&buf[..n]);
        self.size += n as u64;
        if self.size > self.expected_size {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "zip entry larger than its declared size"));
        }
        Ok(n)
    }
}

/// A reader that reads `chunk_size` blocks from an inner reader on a background thread, up to `depth` blocks ahead
/// of the consumer. Dropping it stops the background thread after its current read.
pub struct ReadAhead {
    rx: Receiver<io::Result<Vec<u8>>>,
    recycle: Sender<Vec<u8>>,
    current: Vec<u8>,
    pos: usize,
}

impl ReadAhead {
    pub fn new(mut inner: impl Read + Send + 'static, chunk_size: usize, depth: usize) -> ReadAhead {
        let (tx, rx) = crossbeam_channel::bounded::<io::Result<Vec<u8>>>(depth.max(1));
        let (recycle, recycled) = crossbeam_channel::unbounded::<Vec<u8>>();
        std::thread::Builder::new()
            .name("read_ahead".to_string())
            .spawn(move || {
                loop {
                    let mut buf = recycled.try_recv().unwrap_or_default();
                    buf.resize(chunk_size, 0);
                    let mut filled = 0;
                    let result = loop {
                        if filled == buf.len() {
                            break Ok(());
                        }
                        match inner.read(&mut buf[filled..]) {
                            Ok(0) => break Ok(()),
                            Ok(n) => filled += n,
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                            Err(e) => break Err(e),
                        }
                    };
                    buf.truncate(filled);
                    let eof = filled < chunk_size;
                    if filled > 0 && tx.send(Ok(buf)).is_err() {
                        return;
                    }
                    if let Err(e) = result {
                        let _ = tx.send(Err(e));
                        return;
                    }
                    if eof {
                        return;
                    }
                }
            })
            .expect("spawn read_ahead thread");
        ReadAhead { rx, recycle, current: Vec::new(), pos: 0 }
    }
}

impl Read for ReadAhead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.current.len() {
            match self.rx.recv() {
                Ok(Ok(next)) => {
                    let old = std::mem::replace(&mut self.current, next);
                    let _ = self.recycle.send(old);
                    self.pos = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.current.len() - self.pos);
        buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Opens `entry` of `zip_path` for fast sequential reading: large deflated entries are inflated in parallel by
/// `available_parallelism` threads, others on one background thread.
pub fn open_with_read_ahead(zip_path: &Path, entry: &str) -> anyhow::Result<Box<dyn Read + Send>> {
    const CHUNK: usize = 4 << 20;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let reader = ZipEntryReader::open_with_threads(zip_path, entry, threads)?;
    Ok(if matches!(reader.inflate, Inflate::Parallel(_)) {
        Box::new(reader)
    } else {
        Box::new(ReadAhead::new(reader, CHUNK, 4))
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn write_zip(path: &Path, entries: &[(&str, &[u8], zip::CompressionMethod)]) {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, data, method) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default().compression_method(*method)).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn reads_stored_and_deflated_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.zip");
        let data: Vec<u8> = (0..3_000_000u32).flat_map(|i| (i % 1000).to_le_bytes()).collect();
        write_zip(
            &path,
            &[
                ("stored.bin", &data, zip::CompressionMethod::Stored),
                ("dir/deflated.bin", &data, zip::CompressionMethod::Deflated),
                ("empty", b"", zip::CompressionMethod::Deflated),
            ],
        );
        for entry in ["stored.bin", "/dir/deflated.bin"] {
            let mut out = vec![];
            ZipEntryReader::open(&path, entry).unwrap().read_to_end(&mut out).unwrap();
            assert_eq!(data, out, "{entry}");
            let mut out = vec![];
            ReadAhead::new(ZipEntryReader::open(&path, entry).unwrap(), 1000, 2).read_to_end(&mut out).unwrap();
            assert_eq!(data, out, "{entry} read ahead");
        }
        for threads in [2, 8] {
            let mut reader = ZipEntryReader::open_with(&path, "dir/deflated.bin", threads, 0).unwrap();
            assert!(matches!(reader.inflate, Inflate::Parallel(_)));
            let mut out = vec![];
            reader.read_to_end(&mut out).unwrap();
            assert_eq!(data, out, "parallel {threads}");
        }
        let mut out = vec![];
        open_with_read_ahead(&path, "empty").unwrap().read_to_end(&mut out).unwrap();
        assert!(out.is_empty());
        assert!(ZipEntryReader::open(&path, "missing").is_err());
    }

    #[test]
    fn detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.zip");
        let data = vec![7u8; 100_000];
        write_zip(&path, &[("stored.bin", &data, zip::CompressionMethod::Stored)]);
        let mut bytes = std::fs::read(&path).unwrap();
        let pos = bytes.windows(4).position(|w| w == [7, 7, 7, 7]).unwrap();
        bytes[pos + 10] = 8;
        std::fs::write(&path, bytes).unwrap();
        let mut out = vec![];
        let err = ZipEntryReader::open(&path, "stored.bin").unwrap().read_to_end(&mut out).unwrap_err();
        assert!(err.to_string().contains("CRC"), "{err}");
        let err = open_with_read_ahead(&path, "stored.bin").unwrap().read_to_end(&mut out).unwrap_err();
        assert!(err.to_string().contains("CRC"), "{err}");

        // flipping bits in a deflated entry fails one way or another (bad stream, size or CRC), in both decoders
        let data: Vec<u8> = (0..2_000_000u32).flat_map(|i| (i % 1000).to_le_bytes()).collect();
        write_zip(&path, &[("deflated.bin", &data, zip::CompressionMethod::Deflated)]);
        let mut bytes = std::fs::read(&path).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x55;
        std::fs::write(&path, bytes).unwrap();
        for threads in [1, 4] {
            let mut out = vec![];
            let result = ZipEntryReader::open_with(&path, "deflated.bin", threads, 0).unwrap().read_to_end(&mut out);
            assert!(result.is_err(), "threads {threads}");
        }
    }
}
