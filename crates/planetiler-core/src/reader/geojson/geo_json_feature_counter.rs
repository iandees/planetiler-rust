//! Port of `reader/geojson/GeoJsonFeatureCounter.java`: internal streaming utility to count the number of features
//! in a geojson document.
//!
//! To simplify processing, it counts the number of `geometry` fields on objects, and descends into any `features`
//! array to traverse FeatureCollections. This will result in the correct count for valid geojson, but may be off for
//! invalid geojson.

use std::io::Read;

use anyhow::Result;

use crate::reader::geojson::geo_json_feature_iterator::{JsonParser, Token};

/// Count the features in a geojson document.
pub fn count(input: impl Read) -> Result<u64> {
    let mut count = 0;
    let mut parser = JsonParser::new(input);
    while !parser.is_closed() {
        match parser.next_token()? {
            Some(Token::StartArray) => parser.skip_children()?,
            Some(Token::FieldName(name)) => {
                parser.next_token()?;
                if name == "geometry" {
                    parser.skip_children()?;
                    count += 1;
                } else if name != "features" {
                    parser.skip_children()?;
                }
            }
            _ => {}
        }
    }
    Ok(count)
}
