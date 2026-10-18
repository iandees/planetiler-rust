//! Port of `reader/osm/PbfFieldDecoder.java` (originally from Osmosis): decodes the lower level PBF data structures.

use std::sync::Arc;

use crate::proto::osmpbf::PrimitiveBlock;

const COORDINATE_SCALING_FACTOR: f64 = 0.000000001;

/// Decodes coordinates, timestamps and strings of a [`PrimitiveBlock`].
#[derive(Clone, Debug)]
pub struct PbfFieldDecoder {
    strings: Vec<Arc<str>>,
    coord_granularity: i64,
    coord_latitude_offset: i64,
    coord_longitude_offset: i64,
    date_granularity: i64,
}

impl PbfFieldDecoder {
    /// Creates a decoder for `block`, taking ownership of (and clearing) its string table.
    pub fn new(block: &mut PrimitiveBlock) -> PbfFieldDecoder {
        let strings = std::mem::take(&mut block.stringtable.s)
            .into_iter()
            .map(|s| match String::from_utf8(s) {
                Ok(s) => Arc::from(s),
                Err(e) => Arc::from(String::from_utf8_lossy(e.as_bytes())),
            })
            .collect();
        PbfFieldDecoder {
            strings,
            coord_granularity: block.granularity() as i64,
            coord_latitude_offset: block.lat_offset(),
            coord_longitude_offset: block.lon_offset(),
            date_granularity: block.date_granularity() as i64,
        }
    }

    /// Creates a decoder for coordinates and timestamps only (no string table), used by the hand-written decoder in
    /// [`crate::reader::osm::pbf_decoder`] which manages strings itself.
    pub fn with_params(granularity: i64, lat_offset: i64, lon_offset: i64, date_granularity: i64) -> PbfFieldDecoder {
        PbfFieldDecoder {
            strings: Vec::new(),
            coord_granularity: granularity,
            coord_latitude_offset: lat_offset,
            coord_longitude_offset: lon_offset,
            date_granularity,
        }
    }

    /// True when the block uses the default granularity (100 nanodegrees) and no offsets.
    pub fn is_default_granularity(&self) -> bool {
        self.coord_granularity == 100 && self.coord_latitude_offset == 0 && self.coord_longitude_offset == 0
    }

    /// Decodes a raw latitude value into degrees.
    pub fn decode_latitude(&self, raw_latitude: i64) -> f64 {
        COORDINATE_SCALING_FACTOR * (self.coord_latitude_offset + self.coord_granularity * raw_latitude) as f64
    }

    /// Decodes a raw longitude value into degrees.
    pub fn decode_longitude(&self, raw_longitude: i64) -> f64 {
        COORDINATE_SCALING_FACTOR * (self.coord_longitude_offset + self.coord_granularity * raw_longitude) as f64
    }

    /// Decodes a raw timestamp into milliseconds since epoch.
    pub fn decode_timestamp(&self, raw_timestamp: i64) -> i64 {
        self.date_granularity * raw_timestamp
    }

    /// Decodes a string table index (empty string when out of range).
    pub fn decode_string(&self, raw_string: usize) -> &Arc<str> {
        static EMPTY: std::sync::LazyLock<Arc<str>> = std::sync::LazyLock::new(|| Arc::from(""));
        self.strings.get(raw_string).unwrap_or(&EMPTY)
    }
}
