//! Port of `config/Arguments.java`: lightweight abstraction over command-line arguments, environment variables, and
//! config files.
//!
//! TODO(port): this is a placeholder; port the full Java implementation.

use std::sync::Arc;

use indexmap::IndexMap;

/// Source of configuration options.
#[derive(Clone, Default)]
pub struct Arguments {
    values: Arc<IndexMap<String, String>>,
}

impl std::fmt::Debug for Arguments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.values.iter()).finish()
    }
}

impl Arguments {
    /// Empty arguments.
    pub fn of() -> Arguments {
        Arguments::default()
    }

    /// Arguments from key/value pairs.
    pub fn of_pairs<K: Into<String>, V: Into<String>>(pairs: impl IntoIterator<Item = (K, V)>) -> Arguments {
        Arguments { values: Arc::new(pairs.into_iter().map(|(k, v)| (k.into(), v.into())).collect()) }
    }

    /// Raw value of an argument, without logging.
    pub fn get_arg(&self, key: &str) -> Option<String> {
        self.values.get(key).cloned()
    }
}
