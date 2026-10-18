//! Port of `stats/DefaultStats.java`: holder for the default [`Stats`] implementation to use for this process.
//!
//! [`Stats`]: crate::stats::stats::Stats

use std::sync::{Arc, LazyLock};

use parking_lot::RwLock;

use crate::stats::stats::{InMemoryStats, StatsRef};

static DEFAULT: LazyLock<RwLock<Option<StatsRef>>> = LazyLock::new(|| RwLock::new(None));

/// Serializes tests that change the process-wide default stats instance.
#[cfg(test)]
pub(crate) static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Returns the default stats instance, creating an in-memory one if none has been set.
pub fn get() -> StatsRef {
    if let Some(stats) = DEFAULT.read().as_ref() {
        return stats.clone();
    }
    DEFAULT.write().get_or_insert_with(|| Arc::new(InMemoryStats::new())).clone()
}

/// Returns the default stats instance if one has been set.
pub fn get_if_set() -> Option<StatsRef> {
    DEFAULT.read().clone()
}

/// Sets the default stats instance.
pub fn set(stats: StatsRef) {
    *DEFAULT.write() = Some(stats);
}
