//! Port of `archive/TileCompression.java`.

use std::fmt;
use std::str::FromStr;

/// Compression applied to each tile in an archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TileCompression {
    None,
    #[default]
    Gzip,
    Unknown,
}

impl TileCompression {
    pub fn id(&self) -> &'static str {
        match self {
            TileCompression::None => "none",
            TileCompression::Gzip => "gzip",
            TileCompression::Unknown => "unknown",
        }
    }

    pub fn available_values() -> &'static [TileCompression] {
        &[TileCompression::None, TileCompression::Gzip]
    }

    pub fn find_by_id(id: &str) -> Option<TileCompression> {
        Self::available_values().iter().copied().find(|c| c.id() == id)
    }

    pub fn from_id(id: &str) -> anyhow::Result<TileCompression> {
        Self::find_by_id(id).ok_or_else(|| {
            anyhow::anyhow!(
                "invalid compression ID; expected one of {:?}",
                Self::available_values().iter().map(|c| c.id()).collect::<Vec<_>>()
            )
        })
    }
}

impl fmt::Display for TileCompression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

impl FromStr for TileCompression {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        Self::from_id(s)
    }
}
