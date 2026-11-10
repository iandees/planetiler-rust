//! Reading local and remote files with random access: port of `reader/remote/*`.

pub mod cached_range_file;
pub mod download_mode;
pub mod http_range_file;
pub mod local_range_file;
pub mod range_file;

#[cfg(test)]
mod range_file_test;
