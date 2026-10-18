//! Port of `collection/Storage.java`: storage method to use for [`LongLongMap`](super::long_long_map::LongLongMap)
//! and [`AppendStore`](super::append_store::AppendStore) implementations.

use std::fmt;
use std::path::{Path, PathBuf};

/// Storage method to use for long-long maps and append stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Storage {
    /// Primitive arrays stored on the heap.
    Ram,
    /// Memory-mapped files stored on disk.
    Mmap,
    /// Off-heap native memory. In Rust this is identical to [`Storage::Ram`], kept for config compatibility.
    Direct,
}

impl Storage {
    pub const VALUES: [Storage; 3] = [Storage::Ram, Storage::Mmap, Storage::Direct];

    /// The config string for this storage type.
    pub fn id(&self) -> &'static str {
        match self {
            Storage::Ram => "ram",
            Storage::Mmap => "mmap",
            Storage::Direct => "direct",
        }
    }

    /// Returns the storage type associated with `id` (case-insensitive) or an error if no match is found.
    pub fn from_id(id: &str) -> anyhow::Result<Storage> {
        let id = id.trim();
        Storage::VALUES
            .into_iter()
            .find(|s| s.id().eq_ignore_ascii_case(id))
            .ok_or_else(|| anyhow::anyhow!("Unexpected storage type: {id}"))
    }
}

impl fmt::Display for Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// Options for implementations that vary by [`Storage`] (Java `Storage.Params`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageParams {
    /// Where to store data (if mmap).
    pub path: PathBuf,
    /// Whether to use `madvise(MADV_RANDOM)` on memory-mapped files to improve random read performance.
    pub madvise: bool,
}

impl StorageParams {
    pub fn new(path: impl Into<PathBuf>, madvise: bool) -> StorageParams {
        StorageParams { path: path.into(), madvise }
    }

    /// Returns a copy of these params with `suffix` appended to [`StorageParams::path`] as a child path.
    pub fn resolve(&self, suffix: &str) -> StorageParams {
        StorageParams { path: self.path.join(suffix), madvise: self.madvise }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for StorageParams {
    fn default() -> Self {
        StorageParams { path: PathBuf::from("."), madvise: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_id() {
        assert_eq!(Storage::Ram, Storage::from_id("ram").unwrap());
        assert_eq!(Storage::Mmap, Storage::from_id(" MMAP ").unwrap());
        assert_eq!(Storage::Direct, Storage::from_id("direct").unwrap());
        assert!(Storage::from_id("other").is_err());
    }

    #[test]
    fn resolve() {
        let p = StorageParams::new("/tmp/a", true).resolve("b");
        assert_eq!(PathBuf::from("/tmp/a/b"), p.path);
        assert!(p.madvise);
    }
}
