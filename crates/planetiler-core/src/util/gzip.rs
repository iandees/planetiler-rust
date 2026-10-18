//! Port of `util/Gzip.java`: in-memory gzip compression helpers.

use std::io::{self, Read, Write};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

/// Gzip-compresses `input` with the default compression level.
pub fn gzip(input: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(input.len()), Compression::default());
    encoder.write_all(input)?;
    encoder.finish()
}

/// Decompresses gzipped `zipped` bytes.
pub fn gunzip(zipped: &[u8]) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();
    GzDecoder::new(zipped).read_to_end(&mut result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip() {
        let string = "abcdef";
        let small = gzip(string.as_bytes()).unwrap();
        let big = gunzip(&small).unwrap();
        assert_eq!(string.as_bytes(), &big[..]);
        assert_ne!(small, big);
    }
}
