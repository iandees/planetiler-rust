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

use anyhow::{Context, bail};
use crossbeam_channel::{Receiver, Sender};

/// Reads the uncompressed contents of one stored or deflated zip entry from a memory-mapped zip file.
pub struct ZipEntryReader {
    mmap: memmap2::Mmap,
    pos: usize,
    end: usize,
    /// `None` for stored entries.
    inflate: Option<flate2::Decompress>,
    crc: crc32fast::Hasher,
    expected_crc: u32,
    expected_size: u64,
    size: u64,
    done: bool,
}

impl ZipEntryReader {
    /// Opens entry `entry` (leading `/` ignored) of the zip file at `zip_path`.
    pub fn open(zip_path: &Path, entry: &str) -> anyhow::Result<ZipEntryReader> {
        let file = std::fs::File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
        // SAFETY: the zip file is only read; like every mmap-based reader this assumes it is not truncated while open
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let (start, compressed, size, crc, method) = {
            let mut archive = zip::ZipArchive::new(io::Cursor::new(&mmap[..]))?;
            let entry = archive.by_name(entry.trim_start_matches('/'))?;
            if entry.encrypted() {
                bail!("Encrypted zip entries are not supported: {}", entry.name());
            }
            (
                entry.data_start() as usize,
                entry.compressed_size() as usize,
                entry.size(),
                entry.crc32(),
                entry.compression(),
            )
        };
        let inflate = match method {
            zip::CompressionMethod::Stored => None,
            zip::CompressionMethod::Deflated => Some(flate2::Decompress::new(false)),
            other => bail!("Unsupported zip compression method {other:?} for {entry}"),
        };
        let end = start.checked_add(compressed).filter(|&e| e <= mmap.len()).context("zip entry out of bounds")?;
        #[cfg(unix)]
        let _ = mmap.advise_range(memmap2::Advice::Sequential, start, compressed);
        Ok(ZipEntryReader {
            mmap,
            pos: start,
            end,
            inflate,
            crc: crc32fast::Hasher::new(),
            expected_crc: crc,
            expected_size: size,
            size: 0,
            done: false,
        })
    }

    /// Uncompressed size of the entry, from the zip directory.
    pub fn size(&self) -> u64 {
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
        let n = match &mut self.inflate {
            None => {
                let n = buf.len().min(self.end - self.pos);
                buf[..n].copy_from_slice(&self.mmap[self.pos..self.pos + n]);
                self.pos += n;
                n
            }
            Some(inflate) => loop {
                let (in_before, out_before) = (inflate.total_in(), inflate.total_out());
                let status = inflate
                    .decompress(&self.mmap[self.pos..self.end], buf, flate2::FlushDecompress::None)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                self.pos += (inflate.total_in() - in_before) as usize;
                let n = (inflate.total_out() - out_before) as usize;
                if n > 0 || status == flate2::Status::StreamEnd {
                    break n;
                }
                if self.pos >= self.end {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated deflate stream in zip entry"));
                }
            },
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

/// Opens `entry` of `zip_path` for fast sequential reading, inflating on a background thread.
pub fn open_with_read_ahead(zip_path: &Path, entry: &str) -> anyhow::Result<ReadAhead> {
    const CHUNK: usize = 4 << 20;
    Ok(ReadAhead::new(ZipEntryReader::open(zip_path, entry)?, CHUNK, 4))
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
    }
}
