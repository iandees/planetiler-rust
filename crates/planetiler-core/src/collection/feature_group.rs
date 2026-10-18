//! Port of `collection/FeatureGroup.java`.

/// Number of bits the sort key gets packed into.
pub const SORT_KEY_BITS: u32 = 22;
/// Maximum allowed sort key.
pub const SORT_KEY_MAX: i32 = (1 << (SORT_KEY_BITS - 1)) - 1;
/// Minimum allowed sort key.
pub const SORT_KEY_MIN: i32 = -(1 << (SORT_KEY_BITS - 1));
