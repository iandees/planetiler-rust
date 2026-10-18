//! Port of `collection/AppendStoreMmap.java`: an append-only array of primitives backed by a memory-mapped file.
//!
//! Writes go through a buffered file writer. On the first read the writer is flushed and closed, and the whole file
//! is memory-mapped read-only (optionally with `madvise(MADV_RANDOM)`). Java splits the mapping into 1GB segments
//! because `ByteBuffer` is int-indexed; on 64-bit Rust a single mapping is used instead.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Context;
use memmap2::Mmap;
use parking_lot::Mutex;

use super::append_store::{AppendStore, Ints, Longs, check_index_in_bounds};
use super::append_store_ram::Primitive;

/// Memory-maps `file` read-only, returning `None` for empty files. Applies `madvise(MADV_RANDOM)` when `madvise`
/// is set, logging once if unsupported.
pub(crate) fn map_read_only(file: &File, madvise: bool) -> std::io::Result<Option<Mmap>> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(None);
    }
    // SAFETY: the file is private to this process (a temp file we created) and is not modified or truncated while
    // the mapping is alive.
    let map = unsafe { Mmap::map(file)? };
    if madvise {
        advise_random(&map);
    }
    Ok(Some(map))
}

/// Applies `madvise(MADV_RANDOM)` to a mapping, logging once if it is unavailable.
pub(crate) fn advise_random(map: &Mmap) {
    #[cfg(unix)]
    {
        static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if map.advise(memmap2::Advice::Random).is_err() && !FAILED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::info!(
                "madvise not available on this system - node location lookup may be slower when less free RAM is available"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = map;
}

/// An append-only array of `T` written to `path` then memory-mapped for reads.
pub struct AppendStoreMmap<T: Primitive> {
    path: PathBuf,
    madvise: bool,
    writer: Mutex<Option<BufWriter<File>>>,
    map: OnceLock<Option<Mmap>>,
    size: u64,
    _marker: std::marker::PhantomData<T>,
}

impl<T: Primitive> AppendStoreMmap<T> {
    /// Creates a new store writing to `path` (creating parent directories if needed).
    pub fn new(path: &Path, madvise: bool) -> anyhow::Result<AppendStoreMmap<T>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {parent:?}"))?;
        }
        let file = File::create(path).with_context(|| format!("Could not create append store file {path:?}"))?;
        Ok(AppendStoreMmap {
            path: path.to_path_buf(),
            madvise,
            writer: Mutex::new(Some(BufWriter::with_capacity(1 << 16, file))),
            map: OnceLock::new(),
            size: 0,
            _marker: std::marker::PhantomData,
        })
    }

    #[inline]
    fn append(&mut self, value: T) -> anyhow::Result<()> {
        let Some(writer) = self.writer.get_mut() else {
            anyhow::bail!("Cannot append to {:?} after it has been read", self.path);
        };
        let mut buf = [0u8; 8];
        value.write_ne(&mut buf[..T::BYTES]);
        writer.write_all(&buf[..T::BYTES]).with_context(|| format!("Error writing to {:?}", self.path))?;
        self.size += 1;
        Ok(())
    }

    /// Stops writing and memory-maps the file. Called automatically on the first read.
    fn mapped(&self) -> Option<&Mmap> {
        self.map
            .get_or_init(|| {
                let result: anyhow::Result<Option<Mmap>> = (|| {
                    if let Some(writer) = self.writer.lock().take() {
                        writer.into_inner().map_err(|e| e.into_error())?;
                    }
                    let file = File::open(&self.path)?;
                    Ok(map_read_only(&file, self.madvise)?)
                })();
                result.unwrap_or_else(|e| panic!("Unable to memory-map {:?}: {e:?}", self.path))
            })
            .as_ref()
    }

    #[inline]
    fn get(&self, index: u64) -> T {
        check_index_in_bounds(index, self.size);
        let map = self.mapped().expect("non-empty store has a mapping");
        let start = index as usize * T::BYTES;
        T::read_ne(&map[start..start + T::BYTES])
    }
}

impl<T: Primitive> AppendStore for AppendStoreMmap<T> {
    fn size(&self) -> u64 {
        self.size
    }

    fn disk_usage_bytes(&self) -> u64 {
        crate::util::file_utils::size(&self.path).unwrap_or(0)
    }
}

impl Ints for AppendStoreMmap<i32> {
    #[inline]
    fn append_int(&mut self, value: i32) -> anyhow::Result<()> {
        self.append(value)
    }

    #[inline]
    fn get_int(&self, index: u64) -> i32 {
        self.get(index)
    }
}

impl Longs for AppendStoreMmap<i64> {
    #[inline]
    fn append_long(&mut self, value: i64) -> anyhow::Result<()> {
        self.append(value)
    }

    #[inline]
    fn get_long(&self, index: u64) -> i64 {
        self.get(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Primitive` whose encoded width differs from its in-memory size: decoding must use `BYTES`, not
    /// `size_of::<T>()`, and goes through bounds-checked slices.
    #[derive(Clone, Copy, Default, Debug, PartialEq)]
    struct ThreeBytes(u32);

    impl Primitive for ThreeBytes {
        const BYTES: usize = 3;
        fn write_ne(self, out: &mut [u8]) {
            out.copy_from_slice(&self.0.to_le_bytes()[..3]);
        }
        fn read_ne(bytes: &[u8]) -> Self {
            ThreeBytes(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]))
        }
    }

    #[test]
    fn decodes_with_encoded_width() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AppendStoreMmap::<ThreeBytes>::new(&dir.path().join("store"), false).unwrap();
        for i in 0..1000u32 {
            store.append(ThreeBytes(i * 7)).unwrap();
        }
        for i in 0..1000u32 {
            assert_eq!(ThreeBytes(i * 7), store.get(i as u64));
        }
        assert_eq!(3000, store.disk_usage_bytes());
    }
}
