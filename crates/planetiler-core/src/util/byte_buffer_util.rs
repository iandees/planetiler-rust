//! Port of `util/ByteBufferUtil.java`: utilities for working with memory-mapped files.
//!
//! Java needs reflection hacks to force-unmap a `MappedByteBuffer` (`ByteBufferUtil.free`); in Rust a
//! [`memmap2::Mmap`] is unmapped when it is dropped, so [`free`] just drops its arguments.

use std::fs::File;
use std::io;

use memmap2::{Mmap, MmapOptions};

use crate::util::madvise;

/// Values from <https://man7.org/linux/man-pages/man2/madvise.2.html>.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Madvice {
    Normal,
    Random,
    Sequential,
    WillNeed,
    DontNeed,
}

impl Madvice {
    /// The `POSIX_MADV_*` value.
    pub fn value(self) -> i32 {
        match self {
            Madvice::Normal => 0,
            Madvice::Random => 1,
            Madvice::Sequential => 2,
            Madvice::WillNeed => 3,
            Madvice::DontNeed => 4,
        }
    }
}

/// Attempts to invoke the native utility and logs a message if not available.
pub fn init() {
    if *madvise::PAGE_SIZE < 0 {
        tracing::info!("madvise not available on this system");
    }
}

/// Give a hint to the system how a mapped memory segment will be used so the OS can optimize performance.
///
/// Returns an error if madvise fails or is not available on this system.
pub fn posix_madvise(buffer: &Mmap, value: Madvice) -> io::Result<()> {
    if buffer.is_empty() {
        return Ok(());
    }
    // SAFETY: the region is exactly the live mapping owned by `buffer`.
    unsafe { madvise::posix_madvise(buffer.as_ptr(), buffer.len(), value.value()) }
}

/// Force-unmap memory-mapped file segments, so the file can safely be deleted. In Rust this simply drops them.
pub fn free<I: IntoIterator<Item = T>, T>(segments: I) {
    drop(segments.into_iter().collect::<Vec<_>>());
}

/// Same as [`map_file_filtered`] except map every segment without testing that it has data first.
pub fn map_file(file: &File, expected_length: u64, segment_bytes: u64, madvise: bool) -> io::Result<Vec<Mmap>> {
    Ok(map_file_filtered(file, expected_length, segment_bytes, madvise, |_| true)?.into_iter().flatten().collect())
}

/// Memory-map many read-only segments of a file.
///
/// - `expected_length`: expected number of bytes in the file
/// - `segment_bytes`: number of bytes in each segment
/// - `madvise`: `true` to use madvise random on the file to improve read performance
/// - `segment_has_data`: returns `false` when a segment index has no data, in which case it is not mapped and the
///   corresponding entry is `None`
pub fn map_file_filtered(
    file: &File,
    expected_length: u64,
    segment_bytes: u64,
    madvise: bool,
    segment_has_data: impl Fn(usize) -> bool,
) -> io::Result<Vec<Option<Mmap>>> {
    debug_assert_eq!(expected_length, file.metadata()?.len());
    let mut segment_count = expected_length / segment_bytes;
    if !expected_length.is_multiple_of(segment_bytes) {
        segment_count += 1;
    }
    let mut segments = Vec::with_capacity(segment_count as usize);
    let mut madvise_failed = false;
    let mut segment_start = 0;
    let mut i = 0;
    while segment_start < expected_length {
        let segment_length = segment_bytes.min(expected_length - segment_start);
        if segment_has_data(i) {
            // SAFETY: planetiler owns the temp files it maps and does not modify them while mapped.
            let buffer = unsafe { MmapOptions::new().offset(segment_start).len(segment_length as usize).map(file)? };
            if madvise && posix_madvise(&buffer, Madvice::Random).is_err() && !madvise_failed {
                tracing::info!(
                    "madvise not available on this system - node location lookup may be slower when less free RAM is available"
                );
                madvise_failed = true;
            }
            segments.push(Some(buffer));
        } else {
            segments.push(None);
        }
        segment_start += segment_bytes;
        i += 1;
    }
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(dir: &std::path::Path, data: &str) -> File {
        let path = dir.join("file");
        std::fs::File::create(&path).unwrap().write_all(data.as_bytes()).unwrap();
        File::open(path).unwrap()
    }

    #[test]
    fn test_madvise_and_unmap() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "test");
        let buffer = unsafe { Mmap::map(&file).unwrap() };
        let result = posix_madvise(&buffer, Madvice::Random);
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            result.unwrap();
        }
        assert_eq!(b"test", &buffer[..]);
        free([buffer]);
        std::fs::remove_file(dir.path().join("file")).unwrap();
    }

    #[test]
    fn test_map_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "test");
        let buffers = map_file(&file, 4, 2, true).unwrap();
        assert_eq!(2, buffers.len());
        assert_eq!(b"te", &buffers[0][..]);
        assert_eq!(b"st", &buffers[1][..]);
        free(buffers);
    }

    #[test]
    fn test_map_file_leftover_segment() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "test!");
        let buffers = map_file(&file, 5, 2, true).unwrap();
        assert_eq!(3, buffers.len());
        assert_eq!(b"te", &buffers[0][..]);
        assert_eq!(b"st", &buffers[1][..]);
        assert_eq!(b"!", &buffers[2][..]);
    }

    #[test]
    fn test_map_file_filter_out_segment() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "test!");
        let buffers = map_file_filtered(&file, 5, 2, true, |i| i != 0).unwrap();
        assert_eq!(3, buffers.len());
        assert!(buffers[0].is_none());
        assert!(buffers[1].is_some());
        assert!(buffers[2].is_some());
    }
}
