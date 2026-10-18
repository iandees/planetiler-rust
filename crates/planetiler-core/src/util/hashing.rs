//! Port of `util/Hashing.java`: static hash functions.
//!
//! Java returns signed `int`/`long`; these return the same bits as `u32`/`u64` (cast with `as i64` if you need the
//! Java value).

/// Initial hash for the FNV-1 and FNV-1a 32-bit hash function.
pub const FNV1_32_INIT: u32 = 0x811c9dc5;
const FNV1_PRIME_32: u32 = 16777619;

/// Initial hash for the FNV-1 and FNV-1a 64-bit hash function.
pub const FNV1_64_INIT: u64 = 0xcbf29ce484222325;
const FNV1_PRIME_64: u64 = 1099511628211;

/// Computes the FNV-1a 32-bit hash of `data` starting from `init_hash` (useful to hash multiple chunks consecutively;
/// the first call must start with [`FNV1_32_INIT`]).
pub fn fnv1a32_with_init(init_hash: u32, data: &[u8]) -> u32 {
    let mut hash = init_hash;
    for &b in data {
        hash ^= b as u32;
        hash = hash.wrapping_mul(FNV1_PRIME_32);
    }
    hash
}

/// Computes the FNV-1a 32-bit hash of `data`.
pub fn fnv1a32(data: &[u8]) -> u32 {
    fnv1a32_with_init(FNV1_32_INIT, data)
}

/// Computes the FNV-1a 64-bit hash of `data` starting from `init_hash` (useful to hash multiple chunks consecutively;
/// the first call must start with [`FNV1_64_INIT`]).
pub fn fnv1a64_with_init(init_hash: u64, data: &[u8]) -> u64 {
    let mut hash = init_hash;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV1_PRIME_64);
    }
    hash
}

/// Computes the FNV-1a 64-bit hash of `data`.
pub fn fnv1a64(data: &[u8]) -> u64 {
    fnv1a64_with_init(FNV1_64_INIT, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hasher<T: PartialEq + std::fmt::Debug>(hash_fn: impl Fn(&[u8]) -> T, init: T) {
        assert_eq!(hash_fn(&[]), hash_fn(&[]));
        assert_eq!(hash_fn(&[1]), hash_fn(&[1]));
        assert_eq!(hash_fn(&[1, 2]), hash_fn(&[1, 2]));
        assert_ne!(hash_fn(&[1]), hash_fn(&[2]));
        assert_ne!(hash_fn(&[1]), hash_fn(&[1, 1]));
        assert_eq!(init, hash_fn(&[]));
    }

    #[test]
    fn test_fnv1a32() {
        test_hasher(fnv1a32, FNV1_32_INIT);
        assert_eq!(123, fnv1a32_with_init(123, &[]));
        assert_eq!(0xe40c292c, fnv1a32(b"a"));
    }

    #[test]
    fn test_fnv1a64() {
        test_hasher(fnv1a64, FNV1_64_INIT);
        assert_eq!(123, fnv1a64_with_init(123, &[]));
        assert_eq!(0xaf63dc4c8601ec8c, fnv1a64(b"a"));
    }
}
