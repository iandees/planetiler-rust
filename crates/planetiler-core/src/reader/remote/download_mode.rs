//! Port of `reader/remote/DownloadMode.java`: how to get a source that is not on local disk yet, from the `download`
//! and `download_<source>` args.

use std::fmt;

/// How to get a source that is not on local disk yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DownloadMode {
    /// Only read local files.
    None,
    /// Read the parts the map bounds need from cloud-optimized sources with range requests and keep them in a sparse
    /// local file for later runs; download other sources whole.
    Cached,
    /// Read the parts the map bounds need from cloud-optimized sources with range requests without storing them on
    /// disk; download other sources whole.
    Streaming,
    /// Download every source whole.
    Full,
}

impl DownloadMode {
    /// Parses `false`, `cached`, `streaming` or `full`, where `true` means `cached`.
    ///
    /// # Errors
    ///
    /// If `value` is none of those.
    pub fn parse(value: &str) -> anyhow::Result<DownloadMode> {
        match value.to_lowercase().as_str() {
            "false" | "none" => Ok(DownloadMode::None),
            "true" | "cached" => Ok(DownloadMode::Cached),
            "streaming" => Ok(DownloadMode::Streaming),
            "full" => Ok(DownloadMode::Full),
            _ => anyhow::bail!("Unrecognized download mode: {value}, expected false, cached, streaming or full"),
        }
    }

    /// Returns true if sources missing from local disk should be fetched.
    pub fn enabled(&self) -> bool {
        *self != DownloadMode::None
    }

    /// Returns true if cloud-optimized sources should be read with range requests instead of downloaded whole.
    pub fn fetch_while_processing(&self) -> bool {
        matches!(self, DownloadMode::Cached | DownloadMode::Streaming)
    }

    /// The lowercase name of the mode, as it can be passed to [`DownloadMode::parse`].
    pub fn name(&self) -> &'static str {
        match self {
            DownloadMode::None => "none",
            DownloadMode::Cached => "cached",
            DownloadMode::Streaming => "streaming",
            DownloadMode::Full => "full",
        }
    }
}

impl fmt::Display for DownloadMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        for (value, expected) in [
            ("false", DownloadMode::None),
            ("none", DownloadMode::None),
            ("true", DownloadMode::Cached),
            ("cached", DownloadMode::Cached),
            ("Streaming", DownloadMode::Streaming),
            ("full", DownloadMode::Full),
        ] {
            assert_eq!(expected, DownloadMode::parse(value).unwrap(), "{value}");
        }
    }

    #[test]
    fn rejects_unknown_mode() {
        assert!(DownloadMode::parse("sometimes").is_err());
    }

    #[test]
    fn predicates() {
        assert!(!DownloadMode::None.enabled());
        assert!(DownloadMode::Full.enabled());
        assert!(DownloadMode::Cached.fetch_while_processing());
        assert!(DownloadMode::Streaming.fetch_while_processing());
        assert!(!DownloadMode::Full.fetch_while_processing());
        assert_eq!("streaming", DownloadMode::Streaming.to_string());
    }
}
