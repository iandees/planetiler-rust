//! Port of `reader/osm/OsmHeader.java`: data parsed from the header block of an OSM input file.

use chrono::{DateTime, Utc};

use crate::geo::envelope::Envelope;

#[derive(Clone, Debug, PartialEq)]
pub struct OsmHeader {
    pub bounds: Envelope,
    pub required_features: Vec<String>,
    pub optional_features: Vec<String>,
    pub writingprogram: String,
    pub source: String,
    pub instant: DateTime<Utc>,
    pub osmosis_replication_sequence_number: i64,
    pub osmosis_replication_base_url: String,
}
