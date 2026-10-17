//! Port of `archive/TileFormat.java`.

use std::fmt;
use std::str::FromStr;

/// Encoding format of each tile in an archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum TileFormat {
    /// Mapbox Vector Tile
    #[default]
    Mvt,
    /// MapLibre Tile
    Mlt,
    Unknown,
}

impl TileFormat {
    pub fn id(&self) -> &'static str {
        match self {
            TileFormat::Mvt => "mvt",
            TileFormat::Mlt => "mlt",
            TileFormat::Unknown => "unknown",
        }
    }

    /// Value used in the mbtiles `format` metadata field.
    pub fn mbtiles(&self) -> Option<&'static str> {
        match self {
            TileFormat::Mvt => Some("pbf"),
            TileFormat::Mlt => Some("application/vnd.maplibre-tile"),
            TileFormat::Unknown => None,
        }
    }

    pub fn available_values() -> &'static [TileFormat] {
        &[TileFormat::Mvt, TileFormat::Mlt]
    }

    pub fn find_by_id(id: &str) -> Option<TileFormat> {
        Self::available_values().iter().copied().find(|c| c.id() == id || c.mbtiles() == Some(id))
    }

    pub fn from_id(id: &str) -> anyhow::Result<TileFormat> {
        Self::find_by_id(id).ok_or_else(|| {
            anyhow::anyhow!(
                "invalid format ID; expected one of {:?}",
                Self::available_values().iter().map(|c| c.id()).collect::<Vec<_>>()
            )
        })
    }
}

impl fmt::Display for TileFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

impl FromStr for TileFormat {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        Self::from_id(s)
    }
}
