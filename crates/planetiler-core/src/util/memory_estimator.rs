//! Port of `util/MemoryEstimator.java`: utilities to estimate the size of in-memory objects.
//!
//! Java needs explicit object-header/pointer accounting; in Rust the estimates are simpler (no object headers), but
//! the same helpers are provided so callers can report approximate memory usage through [`HasEstimate`].

/// Something that can estimate how many bytes of memory it is using.
pub trait HasEstimate {
    fn estimate_memory_usage_bytes(&self) -> u64;
}

impl<T: HasEstimate + ?Sized> HasEstimate for std::sync::Arc<T> {
    fn estimate_memory_usage_bytes(&self) -> u64 {
        (**self).estimate_memory_usage_bytes()
    }
}

impl<T: HasEstimate + ?Sized> HasEstimate for Box<T> {
    fn estimate_memory_usage_bytes(&self) -> u64 {
        (**self).estimate_memory_usage_bytes()
    }
}

pub const INT_BYTES: u64 = 4;
pub const LONG_BYTES: u64 = 8;
pub const POINTER_BYTES: u64 = 8;
/// Fixed overhead of a `Vec`/`String` (pointer, capacity, length).
pub const VEC_HEADER_BYTES: u64 = 24;

/// Estimated size of an optional object.
pub fn estimate_size<T: HasEstimate + ?Sized>(object: Option<&T>) -> u64 {
    object.map_or(0, |o| o.estimate_memory_usage_bytes())
}

/// Estimated size of a string, including its header.
pub fn estimate_string_size(string: Option<&str>) -> u64 {
    string.map_or(0, |s| VEC_HEADER_BYTES + s.len() as u64)
}

/// Estimated size of an array with `length` entries of `entry_size` bytes each.
pub fn estimate_array_size(length: usize, entry_size: u64) -> u64 {
    VEC_HEADER_BYTES + entry_size * length as u64
}

/// Estimated size of a `Vec<T>` including unused capacity (but not heap memory owned by the elements).
pub fn estimate_vec_size<T>(vec: &Vec<T>) -> u64 {
    estimate_array_size(vec.capacity(), size_of::<T>() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(u64);
    impl HasEstimate for Fixed {
        fn estimate_memory_usage_bytes(&self) -> u64 {
            self.0
        }
    }

    #[test]
    fn estimates() {
        assert_eq!(0, estimate_size::<Fixed>(None));
        assert_eq!(10, estimate_size(Some(&Fixed(10))));
        assert_eq!(10, std::sync::Arc::new(Fixed(10)).estimate_memory_usage_bytes());
        assert_eq!(VEC_HEADER_BYTES + 3, estimate_string_size(Some("abc")));
        assert_eq!(VEC_HEADER_BYTES + 80, estimate_array_size(10, LONG_BYTES));
        assert_eq!(VEC_HEADER_BYTES + 80, estimate_vec_size(&Vec::<u64>::with_capacity(10)));
    }
}
