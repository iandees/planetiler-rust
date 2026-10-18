//! Port of `reader/geojson/GeoJsonFeature.java`.

use geo_types::Geometry;

use crate::reader::with_tags::WithTags;
use crate::value::Tags;

/// Feature read from a geojson document.
#[derive(Clone, Debug, PartialEq)]
pub struct GeoJsonFeature {
    /// The parsed geometry from the `geometry` field, or an empty geometry if it was invalid.
    pub geometry: Geometry<f64>,
    /// The parsed map from the `properties` field, or an empty map if properties are missing.
    pub tags: Tags,
}

impl WithTags for GeoJsonFeature {
    fn tags(&self) -> &Tags {
        &self.tags
    }
}
