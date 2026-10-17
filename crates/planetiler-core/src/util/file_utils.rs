//! Port of `util/FileUtils.java`.
//!
//! TODO(port): remaining FileUtils helpers.

use std::io;
use std::path::Path;

/// Size of a file, or total size of all files in a directory, in bytes.
pub fn size(path: &Path) -> io::Result<u64> {
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        let mut total = 0;
        for entry in std::fs::read_dir(path)? {
            total += size(&entry?.path()).unwrap_or(0);
        }
        Ok(total)
    } else {
        Ok(meta.len())
    }
}
