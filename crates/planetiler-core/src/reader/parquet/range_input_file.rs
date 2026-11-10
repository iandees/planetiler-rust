//! Port of `reader/parquet/RangeInputFile.java`: adapts a [`RangeFile`] to the parquet crate's [`ChunkReader`],
//! serving reads from bytes that were already fetched where possible.

use std::io::Read;
use std::sync::Arc;

use bytes::Bytes;
use parquet::errors::ParquetError;
use parquet::file::reader::{ChunkReader, Length};

use crate::reader::remote::range_file::RangeFile;

/// How much a [`RangeReader`] reads at once.
const READ_AHEAD_BYTES: u64 = 1 << 20;

/// A [`RangeFile`] that parquet can read from.
#[derive(Clone)]
pub struct RangeInputFile {
    file: Arc<dyn RangeFile>,
    length: u64,
    /// A row group fetched ahead of time and its offset in the file, so parquet's reads of its column chunks don't
    /// each go to the file.
    prefetched: Option<(u64, Bytes)>,
}

impl RangeInputFile {
    /// Reads from `file`, using `prefetched` (the offset in the file of its first byte and the bytes) where possible.
    pub fn new(file: Arc<dyn RangeFile>, length: u64, prefetched: Option<(u64, Bytes)>) -> RangeInputFile {
        RangeInputFile { file, length, prefetched }
    }

    /// Reads from `file` with nothing prefetched.
    pub fn of(file: Arc<dyn RangeFile>) -> anyhow::Result<RangeInputFile> {
        let length = file.length()?;
        Ok(RangeInputFile::new(file, length, None))
    }

    /// Returns the prefetched bytes from `position` to the end of them, if `position` is in them.
    fn prefetched_from(&self, position: u64) -> Option<Bytes> {
        let (prefetched_offset, prefetched) = self.prefetched.as_ref()?;
        (position >= *prefetched_offset && position < *prefetched_offset + prefetched.len() as u64)
            .then(|| prefetched.slice((position - prefetched_offset) as usize..))
    }

    fn read_bytes(&self, offset: u64, len: usize) -> parquet::errors::Result<Bytes> {
        if offset + len as u64 > self.length {
            return Err(ParquetError::EOF(format!(
                "Unexpected EOF at {} reading {len} bytes at {offset}",
                self.length
            )));
        }
        if let Some((prefetched_offset, prefetched)) = &self.prefetched
            && offset >= *prefetched_offset
            && offset + len as u64 <= *prefetched_offset + prefetched.len() as u64
        {
            let start = (offset - prefetched_offset) as usize;
            return Ok(prefetched.slice(start..start + len));
        }
        self.file.read(offset, len).map(Bytes::from).map_err(|e| ParquetError::External(e.into()))
    }
}

impl Length for RangeInputFile {
    fn len(&self) -> u64 {
        self.length
    }
}

impl ChunkReader for RangeInputFile {
    type T = RangeReader;

    fn get_read(&self, start: u64) -> parquet::errors::Result<RangeReader> {
        Ok(RangeReader { input: self.clone(), position: start, buffer: Bytes::new(), buffer_start: start })
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<Bytes> {
        self.read_bytes(start, length)
    }
}

/// Sequential reader over a [`RangeInputFile`] that reads ahead so small reads don't each go to the file.
pub struct RangeReader {
    input: RangeInputFile,
    position: u64,
    buffer: Bytes,
    buffer_start: u64,
}

impl Read for RangeReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() || self.position >= self.input.length {
            return Ok(0);
        }
        let buffered_end = self.buffer_start + self.buffer.len() as u64;
        if self.position < self.buffer_start || self.position >= buffered_end {
            self.buffer = match self.input.prefetched_from(self.position) {
                Some(prefetched) => prefetched,
                None => {
                    let length = READ_AHEAD_BYTES.max(out.len() as u64).min(self.input.length - self.position);
                    self.input.read_bytes(self.position, length as usize).map_err(std::io::Error::other)?
                }
            };
            self.buffer_start = self.position;
        }
        let start = (self.position - self.buffer_start) as usize;
        let n = out.len().min(self.buffer.len() - start);
        out[..n].copy_from_slice(&self.buffer[start..start + n]);
        self.position += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::remote::local_range_file::LocalRangeFile;

    fn input(prefetched: Option<(u64, Bytes)>) -> (tempfile::TempDir, RangeInputFile) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let contents: Vec<u8> = (0..100u8).collect();
        std::fs::write(&path, contents).unwrap();
        let file: Arc<dyn RangeFile> = Arc::new(LocalRangeFile::new(&path));
        (dir, RangeInputFile::new(file, 100, prefetched))
    }

    #[test]
    fn reads_bytes_from_file_and_prefetched() {
        // the prefetched bytes differ from the file so the test shows which one a read came from
        let (_dir, input) = input(Some((10, Bytes::from(vec![255u8; 10]))));
        assert_eq!(100, input.len());
        assert_eq!(&[0, 1, 2][..], &input.get_bytes(0, 3).unwrap()[..]);
        assert_eq!(&[255, 255][..], &input.get_bytes(12, 2).unwrap()[..]);
        // partly prefetched reads come from the file
        assert_eq!(&[9, 10][..], &input.get_bytes(9, 2).unwrap()[..]);
        assert!(input.get_bytes(99, 2).is_err());
    }

    #[test]
    fn sequential_reader() {
        let (_dir, input) = input(None);
        let mut reader = input.get_read(95).unwrap();
        let mut out = vec![];
        reader.read_to_end(&mut out).unwrap();
        assert_eq!((95..100u8).collect::<Vec<_>>(), out);
        let mut reader = input.get_read(0).unwrap();
        let mut two = [0u8; 2];
        reader.read_exact(&mut two).unwrap();
        reader.read_exact(&mut two).unwrap();
        assert_eq!([2, 3], two);
    }
}
