//! Port of `util/Gzip.java`: in-memory gzip compression helpers.

use std::io::{self, Read, Write};

use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;

thread_local! {
    static COMPRESSOR: std::cell::RefCell<libdeflater::Compressor> =
        std::cell::RefCell::new(libdeflater::Compressor::new(libdeflater::CompressionLvl::default()));
}

/// Gzip-compresses `input` with the default compression level (6).
///
/// Uses libdeflate (a thread-local compressor) which is several times faster than zlib at the same level and
/// produces a valid gzip stream of similar size; the bytes differ from Java's `GZIPOutputStream`.
pub fn gzip(input: &[u8]) -> io::Result<Vec<u8>> {
    COMPRESSOR.with(|c| {
        let mut compressor = c.borrow_mut();
        let mut out = vec![0u8; compressor.gzip_compress_bound(input.len())];
        let len = compressor.gzip_compress(input, &mut out).map_err(|e| io::Error::other(format!("{e:?}")))?;
        out.truncate(len);
        Ok(out)
    })
}

/// Gzip-compresses `input` with zlib's deflate implementation at the default level (like Java's `Gzip.gzip`).
pub fn gzip_zlib(input: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(input.len()), Compression::default());
    encoder.write_all(input)?;
    encoder.finish()
}

/// Decompresses gzipped `zipped` bytes, reading every member of a multi-member stream (like Java's
/// `GZIPInputStream`).
pub fn gunzip(zipped: &[u8]) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();
    MultiGzDecoder::new(zipped).read_to_end(&mut result)?;
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
        assert_eq!(string.as_bytes(), &gunzip(&gzip_zlib(string.as_bytes()).unwrap()).unwrap()[..]);
        let large: Vec<u8> = (0..100_000u32).flat_map(|i| (i % 251).to_le_bytes()).collect();
        assert_eq!(large, gunzip(&gzip(&large).unwrap()).unwrap());
    }
}
