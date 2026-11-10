//! Port of `reader/remote/LocalRangeFile.java`: a [`RangeFile`] that reads from a file on local disk.
//!
//! Opens the file for each read so that it holds no resources between reads and does not need to be closed.

use std::any::Any;
use std::path::{Path, PathBuf};

use crate::reader::remote::range_file::{LazyPending, Pending, RangeFile};
use crate::util::downloader::ByteRange;
use crate::util::file_utils;

pub struct LocalRangeFile {
    path: PathBuf,
}

impl LocalRangeFile {
    pub fn new(path: &Path) -> LocalRangeFile {
        LocalRangeFile { path: path.to_path_buf() }
    }

    fn read_at(path: &Path, offset: u64, length: usize) -> anyhow::Result<Vec<u8>> {
        let file = std::fs::File::open(path)?;
        Ok(file_utils::read_fully(&file, offset, length)?)
    }
}

impl RangeFile for LocalRangeFile {
    fn length(&self) -> anyhow::Result<u64> {
        Ok(std::fs::metadata(&self.path)?.len())
    }

    fn read(&self, offset: u64, length: usize) -> anyhow::Result<Vec<u8>> {
        Self::read_at(&self.path, offset, length)
    }

    fn read_later(&self, ranges: &[ByteRange]) -> anyhow::Result<Vec<Box<dyn Pending>>> {
        Ok(ranges
            .iter()
            .map(|range| {
                let (path, range) = (self.path.clone(), *range);
                Box::new(LazyPending(move || Self::read_at(&path, range.offset, usize::try_from(range.length)?)))
                    as Box<dyn Pending>
            })
            .collect())
    }

    fn close(&self) -> anyhow::Result<()> {
        // nothing to release
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}
