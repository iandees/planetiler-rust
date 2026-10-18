//! Port of `stream/WriteableStreamArchive.java`: base for all kinds of simple file streams. This is primarily useful
//! when the file is a named pipe. In that case data can directly be transformed and consumed by other programs.
//!
//! Writing can be parallelized across multiple files (tile_write_threads). For the first file the base path is used.
//! For consecutive files 1, 2, ... is appended to the base path.
//!
//! ```text
//! # create the pipes
//! mkfifo /tmp/data/output.csv
//! mkfifo /tmp/data/output.csv1
//! mkfifo /tmp/data/output.csv2
//! # start the consumers
//! consumer_program < /tmp/data/output.csv
//! consumer_program < /tmp/data/output.csv1
//! consumer_program < /tmp/data/output.csv2
//!
//! # now run planetiler with the options --append --output=/tmp/data/output.csv --tile_write_threads=3
//! ```

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use anyhow::Context;
use parking_lot::Mutex;

use crate::stream::stream_archive_config::StreamArchiveConfig;
use crate::stream::stream_archive_utils;

/// Writer that counts bytes written into a shared counter.
struct CountingWriter<W> {
    inner: W,
    counter: Arc<AtomicU64>,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.counter.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Handle to the primary output stream that does not close it when dropped (Java `CloseShieldOutputStream`).
#[derive(Clone)]
pub struct SharedOutput(Arc<Mutex<CountingWriter<File>>>);

impl Write for SharedOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.0.lock().write_all(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().flush()
    }
}

/// Shared state of stream archives: the primary output stream (also used for metadata) and a factory for additional
/// output streams, one per tile writer.
pub struct StreamArchiveBase {
    bytes_written: Arc<AtomicU64>,
    base_path: PathBuf,
    append: bool,
    primary: SharedOutput,
    tile_writer_counter: AtomicUsize,
    pub config: StreamArchiveConfig,
}

impl StreamArchiveBase {
    pub fn new(path: &Path, config: StreamArchiveConfig) -> anyhow::Result<StreamArchiveBase> {
        let bytes_written = Arc::new(AtomicU64::new(0));
        let append = config.append_to_file;
        let file = Self::open(path, append)?;
        Ok(StreamArchiveBase {
            primary: SharedOutput(Arc::new(Mutex::new(CountingWriter { inner: file, counter: bytes_written.clone() }))),
            bytes_written,
            base_path: path.to_path_buf(),
            append,
            tile_writer_counter: AtomicUsize::new(0),
            config,
        })
    }

    fn open(path: &Path, append: bool) -> anyhow::Result<File> {
        let mut options = OpenOptions::new();
        options.write(true);
        if append {
            options.append(true);
        } else {
            options.create_new(true);
        }
        options.open(path).with_context(|| format!("failed to open {}", path.display()))
    }

    /// The output stream for the next tile writer: the primary stream for the first writer, else a new file.
    pub fn next_output_stream(&self) -> anyhow::Result<Box<dyn Write + Send>> {
        let index = self.tile_writer_counter.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            Ok(Box::new(self.primary_output_stream()))
        } else {
            let path = stream_archive_utils::construct_indexed_path(&self.base_path, index);
            let file = Self::open(&path, self.append)?;
            Ok(Box::new(CountingWriter { inner: file, counter: self.bytes_written.clone() }))
        }
    }

    /// The primary output stream. The stream of the first writer must be closed by the archive and not the tile writer
    /// since the primary stream can be used to send meta data, as well.
    pub fn primary_output_stream(&self) -> SharedOutput {
        self.primary.clone()
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }

    pub fn close(&self) -> anyhow::Result<()> {
        self.primary.0.lock().flush()?;
        Ok(())
    }
}
