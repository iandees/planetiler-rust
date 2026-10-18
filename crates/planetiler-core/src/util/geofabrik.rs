//! Port of `util/Geofabrik.java`: searches the [Geofabrik Download Server](https://download.geofabrik.de/) for a
//! `.osm.pbf` download URL by name.
//!
//! See <https://download.geofabrik.de/technical.html> for details of the JSON index.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use anyhow::bail;
use serde::Deserialize;

use crate::config::planetiler_config::PlanetilerConfig;

const INDEX_URL: &str = "https://download.geofabrik.de/index-v1-nogeom.json";

static INDEX: OnceLock<IndexJson> = OnceLock::new();

/// Properties of one extract in the Geofabrik index.
#[derive(Clone, Debug, Deserialize)]
pub struct PropertiesJson {
    pub id: String,
    #[serde(default)]
    pub parent: Option<String>,
    pub name: String,
    #[serde(default)]
    pub urls: BTreeMap<String, String>,
    #[serde(rename = "iso3166-1:alpha2", default)]
    pub iso3166_1: Option<Vec<String>>,
    #[serde(rename = "iso3166-2", default)]
    pub iso3166_2: Option<Vec<String>>,
}

impl PropertiesJson {
    fn ids(&self) -> Vec<&str> {
        let mut result = vec![self.id.as_str(), self.name.as_str()];
        for list in [&self.iso3166_1, &self.iso3166_2].into_iter().flatten() {
            result.extend(list.iter().map(String::as_str));
        }
        result
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct FeatureJson {
    pub properties: PropertiesJson,
}

/// The Geofabrik JSON index.
#[derive(Clone, Debug, Deserialize)]
pub struct IndexJson {
    pub features: Vec<FeatureJson>,
}

/// Fetches the Geofabrik index (once, then cached) and searches for a `.osm.pbf` resource where the ID or name field
/// contains all the tokens in `search_query`.
///
/// Prefers exact ID matches, then exact name matches, then names that contain all the tokens. Fails if there are no
/// matches or more than one match.
pub fn get_download_url(search_query: &str, config: &PlanetilerConfig) -> anyhow::Result<String> {
    let index = match INDEX.get() {
        Some(index) => index,
        None => {
            let json = crate::util::downloader::fetch_string(INDEX_URL, config)?;
            let _ = INDEX.set(parse_index_json(&json)?);
            INDEX.get().expect("index set")
        }
    };
    search_index_for_download_url(search_query, index)
}

/// Parses the Geofabrik JSON index.
pub fn parse_index_json(json: &str) -> anyhow::Result<IndexJson> {
    Ok(serde_json::from_str(json)?)
}

fn tokenize(input: &str) -> BTreeSet<String> {
    input
        .to_lowercase()
        .split(|c: char| !c.is_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Searches `index` for a `.osm.pbf` URL matching `search_query`.
pub fn search_index_for_download_url(search_query: &str, index: &IndexJson) -> anyhow::Result<String> {
    let search_tokens = tokenize(search_query);
    let mut approx_name = vec![];
    let mut id = vec![];
    let mut exact_name = vec![];
    for feature in &index.features {
        let properties = &feature.properties;
        if properties.urls.contains_key("pbf") {
            if properties.ids().iter().any(|i| tokenize(i) == search_tokens) {
                id.push(properties);
            } else if tokenize(&properties.name) == search_tokens {
                exact_name.push(properties);
            } else if tokenize(&properties.name).is_superset(&search_tokens) {
                approx_name.push(properties);
            }
        }
    }
    for (name, values) in
        [("exact ID matches", id), ("exact name matches", exact_name), ("approximate name matches", approx_name)]
    {
        if values.len() > 1 {
            bail!(
                "Multiple {name} for '{search_query}': {}",
                values.iter().map(|d| d.id.as_str()).collect::<Vec<_>>().join(", ")
            );
        } else if let Some(only) = values.first() {
            return Ok(only.urls["pbf"].clone());
        }
    }
    bail!("No matches for '{search_query}'")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feature(id: &str, parent: &str, iso: &str, iso_key: &str, name: &str, pbf: &str) -> String {
        let iso = if iso.is_empty() { String::new() } else { format!(r#""{iso_key}": ["{iso}"],"#) };
        format!(
            r#"{{"type": "Feature", "properties": {{"id": "{id}", "parent": "{parent}", {iso} "name": "{name}",
              "urls": {{"pbf": "{pbf}", "bz2": "https://download.geofabrik.de/x.osm.bz2"}}}}}}"#
        )
    }

    fn index() -> IndexJson {
        let features = [
            feature(
                "afghanistan",
                "asia",
                "AF",
                "iso3166-1:alpha2",
                "Afghanistan",
                "https://download.geofabrik.de/asia/afghanistan-latest.osm.pbf",
            ),
            feature(
                "georgia",
                "europe",
                "GE",
                "iso3166-1:alpha2",
                "Georgia",
                "https://download.geofabrik.de/europe/georgia-latest.osm.pbf",
            ),
            feature(
                "us/georgia",
                "north-america",
                "US-GA",
                "iso3166-2",
                "Georgia",
                "https://download.geofabrik.de/north-america/us/georgia-latest.osm.pbf",
            ),
            feature(
                "us/massachusetts",
                "north-america",
                "US-MA",
                "iso3166-2",
                "us/massachusetts",
                "https://download.geofabrik.de/north-america/us/massachusetts-latest.osm.pbf",
            ),
            feature(
                "us/west-virginia",
                "north-america",
                "US-WV",
                "iso3166-2",
                "us/west-virginia",
                "https://download.geofabrik.de/north-america/us/west-virginia-latest.osm.pbf",
            ),
            feature(
                "west-sussex",
                "england",
                "",
                "",
                "West Sussex",
                "https://download.geofabrik.de/europe/great-britain/england/west-sussex-latest.osm.pbf",
            ),
        ];
        parse_index_json(&format!(r#"{{"type": "FeatureCollection", "features": [{}]}}"#, features.join(","))).unwrap()
    }

    #[test]
    fn found() {
        let index = index();
        for (search, expected) in [
            ("afghanistan", "https://download.geofabrik.de/asia/afghanistan-latest.osm.pbf"),
            ("af", "https://download.geofabrik.de/asia/afghanistan-latest.osm.pbf"),
            ("ge", "https://download.geofabrik.de/europe/georgia-latest.osm.pbf"),
            ("us-ga", "https://download.geofabrik.de/north-america/us/georgia-latest.osm.pbf"),
            ("us ga", "https://download.geofabrik.de/north-america/us/georgia-latest.osm.pbf"),
            ("us/ga", "https://download.geofabrik.de/north-america/us/georgia-latest.osm.pbf"),
            ("us_ga", "https://download.geofabrik.de/north-america/us/georgia-latest.osm.pbf"),
            ("us/georgia", "https://download.geofabrik.de/north-america/us/georgia-latest.osm.pbf"),
            ("west virginia", "https://download.geofabrik.de/north-america/us/west-virginia-latest.osm.pbf"),
            ("us/west-virginia", "https://download.geofabrik.de/north-america/us/west-virginia-latest.osm.pbf"),
            ("west sussex", "https://download.geofabrik.de/europe/great-britain/england/west-sussex-latest.osm.pbf"),
        ] {
            assert_eq!(expected, search_index_for_download_url(search, &index).unwrap(), "{search}");
        }
    }

    #[test]
    fn not_found() {
        assert!(search_index_for_download_url("monaco", &index()).is_err());
    }

    #[test]
    fn ambiguous() {
        assert!(search_index_for_download_url("georgia", &index()).is_err());
    }
}
