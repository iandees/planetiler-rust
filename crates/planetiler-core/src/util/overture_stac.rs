//! Port of `util/OvertureStac.java`: queries the [Overture STAC catalog](https://stac.overturemaps.org/) to find the
//! parquet file URLs for a theme/type, keeping only files whose bbox intersects the requested bounds.
//!
//! This avoids downloading the full Overture dataset when only a small area is needed. The URLs can be passed straight
//! to [`Downloader`](crate::util::downloader::Downloader).

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::config::bounds::Bounds;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::geo::envelope::Envelope;

pub const DEFAULT_CATALOG_URL: &str = "https://stac.overturemaps.org/catalog.json";

#[derive(Clone, Debug, Default, Deserialize)]
pub struct StacLink {
    #[serde(default)]
    pub rel: Option<String>,
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub latest: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct StacCatalog {
    #[serde(default)]
    pub latest: Option<String>,
    #[serde(default)]
    pub links: Vec<StacLink>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SpatialExtent {
    #[serde(default)]
    pub bbox: Option<Vec<Option<Vec<f64>>>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct StacExtent {
    #[serde(default)]
    pub spatial: Option<SpatialExtent>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct StacCollection {
    #[serde(default)]
    pub links: Vec<StacLink>,
    #[serde(default)]
    pub extent: Option<StacExtent>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct StacAsset {
    #[serde(default)]
    pub href: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct StacItem {
    #[serde(default)]
    pub bbox: Option<Vec<f64>>,
    #[serde(default)]
    pub assets: BTreeMap<String, StacAsset>,
}

/// Fetches the body of a URL as a string.
pub type Fetcher = Arc<dyn Fn(&str) -> anyhow::Result<String> + Send + Sync>;

/// Client for the Overture STAC catalog.
pub struct OvertureStac {
    fetcher: Fetcher,
}

impl OvertureStac {
    /// Creates a client backed by real HTTP requests using timeouts and user agent from `config`.
    pub fn create(config: &PlanetilerConfig) -> OvertureStac {
        let config = config.clone();
        OvertureStac::with_fetcher(Arc::new(move |url| crate::util::downloader::fetch_string(url, &config)))
    }

    /// Creates a client that fetches JSON with `fetcher` (for tests).
    pub fn with_fetcher(fetcher: Fetcher) -> OvertureStac {
        OvertureStac { fetcher }
    }

    fn fetch<T: DeserializeOwned>(&self, url: &str) -> anyhow::Result<T> {
        let json = (self.fetcher)(url).with_context(|| format!("Failed to fetch STAC URL: {url}"))?;
        serde_json::from_str(&json).with_context(|| format!("Failed to parse STAC URL: {url}"))
    }

    /// Returns HTTPS URLs of parquet files for `theme`/`type` in the latest Overture release whose STAC item bbox
    /// intersects `bounds`.
    pub fn get_parquet_urls(&self, theme: &str, type_: &str, bounds: &Bounds) -> anyhow::Result<Vec<String>> {
        self.get_parquet_urls_from(DEFAULT_CATALOG_URL, theme, type_, bounds)
    }

    /// Same as [`OvertureStac::get_parquet_urls`] starting from a custom root catalog URL.
    pub fn get_parquet_urls_from(
        &self,
        catalog_url: &str,
        theme: &str,
        type_: &str,
        bounds: &Bounds,
    ) -> anyhow::Result<Vec<String>> {
        tracing::info!("Fetching Overture STAC catalog from {catalog_url}");
        let catalog: StacCatalog = self.fetch(catalog_url)?;
        let latest_catalog_url = resolve_latest_catalog_url(&catalog, catalog_url)?;
        tracing::info!("Using Overture release catalog: {latest_catalog_url}");
        let release_catalog: StacCatalog = self.fetch(&latest_catalog_url)?;

        let Some(theme_catalog_url) = resolve_child_url(&release_catalog, &latest_catalog_url, theme) else {
            bail!("Overture theme '{theme}' not found in catalog {latest_catalog_url}");
        };
        let theme_catalog: StacCatalog = self.fetch(&theme_catalog_url)?;

        let Some(collection_url) = resolve_child_url(&theme_catalog, &theme_catalog_url, type_) else {
            bail!("Overture type '{type_}' not found in theme '{theme}' catalog {theme_catalog_url}");
        };
        let collection: StacCollection = self.fetch(&collection_url)?;

        let lat_lon_bounds = if bounds.is_world() { None } else { Some(bounds.lat_lon()) };
        if let Some(filter) = &lat_lon_bounds
            && !collection_extent_intersects(&collection, filter)
        {
            tracing::debug!("Skipping collection {collection_url} - extent does not intersect bounds");
            return Ok(vec![]);
        }

        let item_urls: Vec<String> = collection
            .links
            .iter()
            .filter(|l| l.rel.as_deref() == Some("item"))
            .map(|l| resolve_url(&collection_url, l.href.as_deref().unwrap_or("")))
            .collect();

        let results: Vec<anyhow::Result<Option<String>>> = std::thread::scope(|scope| {
            let handles: Vec<_> = item_urls
                .iter()
                .map(|item_url| {
                    scope.spawn(move || -> anyhow::Result<Option<String>> {
                        let item: StacItem = self.fetch(item_url)?;
                        if let Some(filter) = &lat_lon_bounds
                            && !item_bbox_intersects(&item, filter)
                        {
                            return Ok(None);
                        }
                        // Prefer AWS, fall back to Azure.
                        let url = get_asset_href(&item, "aws").or_else(|| get_asset_href(&item, "azure"));
                        if url.is_none() {
                            tracing::warn!("No parquet asset found in STAC item {item_url}");
                        }
                        Ok(url)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("STAC fetch thread panicked"))))
                .collect()
        });
        let mut urls = vec![];
        for result in results {
            if let Some(url) = result? {
                urls.push(url);
            }
        }
        tracing::info!("Found {} parquet files for theme={theme} type={type_} within bounds", urls.len());
        Ok(urls)
    }
}

/// Returns the URL of the latest release catalog linked from the root `catalog`.
pub fn resolve_latest_catalog_url(catalog: &StacCatalog, base_url: &str) -> anyhow::Result<String> {
    for link in &catalog.links {
        if link.rel.as_deref() == Some("child") && link.latest {
            return Ok(resolve_url(base_url, link.href.as_deref().unwrap_or("")));
        }
    }
    if let Some(latest) = &catalog.latest {
        return Ok(resolve_url(base_url, &format!("./{latest}/catalog.json")));
    }
    bail!("Could not find latest Overture release in catalog {base_url}")
}

/// Returns the URL of the child link whose title or first href path segment matches `name` (case-insensitive).
pub fn resolve_child_url(catalog: &StacCatalog, base_url: &str, name: &str) -> Option<String> {
    for link in &catalog.links {
        if link.rel.as_deref() != Some("child") {
            continue;
        }
        let href = link.href.as_deref().unwrap_or("");
        let title = link.title.as_deref().unwrap_or("");
        if name.eq_ignore_ascii_case(title) || name.eq_ignore_ascii_case(href_segment(href)) {
            return Some(resolve_url(base_url, href));
        }
    }
    None
}

/// Returns the first meaningful path segment from a relative href like `"./buildings/catalog.json"`.
fn href_segment(href: &str) -> &str {
    let stripped = href.strip_prefix("./").unwrap_or(href);
    stripped.split('/').next().unwrap_or(stripped)
}

fn bbox_envelope(bbox: &[f64]) -> Envelope {
    Envelope::new(bbox[0], bbox[2], bbox[1], bbox[3])
}

/// Returns false only if the collection's declared extent is known and does not overlap `filter`.
fn collection_extent_intersects(collection: &StacCollection, filter: &Envelope) -> bool {
    let Some(bboxes) = collection.extent.as_ref().and_then(|e| e.spatial.as_ref()).and_then(|s| s.bbox.as_ref()) else {
        return true;
    };
    if bboxes.is_empty() {
        return true;
    }
    bboxes.iter().flatten().filter(|b| b.len() >= 4).any(|b| filter.intersects(&bbox_envelope(b)))
}

/// Returns true if the STAC item's bbox overlaps `filter` (or the item has no bbox).
pub fn item_bbox_intersects(item: &StacItem, filter: &Envelope) -> bool {
    match &item.bbox {
        Some(bbox) if bbox.len() >= 4 => filter.intersects(&bbox_envelope(bbox)),
        _ => true,
    }
}

fn get_asset_href(item: &StacItem, key: &str) -> Option<String> {
    item.assets.get(key).and_then(|a| a.href.clone()).filter(|h| !h.trim().is_empty())
}

/// Resolves `href` relative to `base` like `java.net.URI.resolve`.
pub fn resolve_url(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let scheme_end = base.find("://").map_or(0, |i| i + 3);
    let host_end = base[scheme_end..].find('/').map_or(base.len(), |i| i + scheme_end);
    let origin = &base[..host_end];
    let base_path = &base[host_end..];
    let mut segments: Vec<&str> = if href.starts_with('/') {
        vec![]
    } else {
        let dir = base_path.rfind('/').map_or("", |i| &base_path[..i]);
        dir.split('/').filter(|s| !s.is_empty()).collect()
    };
    let parts: Vec<&str> = href.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        match *part {
            "" | "." if i + 1 < parts.len() => {}
            "." => {}
            ".." => {
                segments.pop();
            }
            p => segments.push(p),
        }
    }
    let trailing = if href.ends_with('/') || href.ends_with("/.") || href.ends_with("/..") { "/" } else { "" };
    format!("{origin}/{}{trailing}", segments.join("/"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn stub_stac(stubs: &[(&str, &str)]) -> OvertureStac {
        let stubs: HashMap<String, String> = stubs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        OvertureStac::with_fetcher(Arc::new(move |url| {
            stubs.get(url).cloned().ok_or_else(|| anyhow::anyhow!("No stub for URL: {url}"))
        }))
    }

    fn parse<T: DeserializeOwned>(json: &str) -> T {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn latest_link_by_flag() {
        let catalog: StacCatalog = parse(
            r#"{"links": [
              {"rel": "child", "href": "./2026-01-01.0/catalog.json"},
              {"rel": "child", "href": "./2026-02-18.0/catalog.json", "latest": true}
            ]}"#,
        );
        assert_eq!(
            "https://stac.overturemaps.org/2026-02-18.0/catalog.json",
            resolve_latest_catalog_url(&catalog, "https://stac.overturemaps.org/catalog.json").unwrap()
        );
    }

    #[test]
    fn latest_fallback_to_top_level_field() {
        let catalog: StacCatalog = parse(
            r#"{"latest": "2026-02-18.0", "links": [
              {"rel": "child", "href": "./2026-01-01.0/catalog.json"},
              {"rel": "child", "href": "./2026-02-18.0/catalog.json"}
            ]}"#,
        );
        assert_eq!(
            "https://stac.overturemaps.org/2026-02-18.0/catalog.json",
            resolve_latest_catalog_url(&catalog, "https://stac.overturemaps.org/catalog.json").unwrap()
        );
    }

    #[test]
    fn latest_not_found() {
        let catalog: StacCatalog = parse(r#"{ "links": [{"rel": "root", "href": "./catalog.json"}] }"#);
        assert!(resolve_latest_catalog_url(&catalog, "https://stac.overturemaps.org/catalog.json").is_err());
    }

    #[test]
    fn resolve_child_by_title() {
        let catalog: StacCatalog = parse(
            r#"{"links": [
              {"rel": "child", "href": "./buildings/catalog.json", "title": "buildings"},
              {"rel": "child", "href": "./places/catalog.json", "title": "places"}
            ]}"#,
        );
        assert_eq!(
            Some("https://stac.overturemaps.org/2026-02-18.0/buildings/catalog.json".to_string()),
            resolve_child_url(&catalog, "https://stac.overturemaps.org/2026-02-18.0/catalog.json", "buildings")
        );
    }

    #[test]
    fn resolve_child_by_href_segment() {
        let catalog: StacCatalog = parse(
            r#"{"links": [
              {"rel": "child", "href": "./transportation/catalog.json"},
              {"rel": "child", "href": "./base/catalog.json"}
            ]}"#,
        );
        assert_eq!(
            Some("https://stac.overturemaps.org/2026-02-18.0/transportation/catalog.json".to_string()),
            resolve_child_url(&catalog, "https://stac.overturemaps.org/2026-02-18.0/catalog.json", "transportation")
        );
    }

    #[test]
    fn resolve_child_not_found() {
        let catalog: StacCatalog =
            parse(r#"{ "links": [{"rel": "child", "href": "./buildings/catalog.json", "title": "buildings"}] }"#);
        assert_eq!(
            None,
            resolve_child_url(&catalog, "https://stac.overturemaps.org/2026-02-18.0/catalog.json", "places")
        );
    }

    #[test]
    fn bbox_intersects() {
        let monaco = Envelope::new(7.3, 7.5, 43.7, 43.8);
        let west: StacItem = parse(r#"{"bbox": [-179.9, -84.3, -2.8, -22.5]}"#);
        assert!(!item_bbox_intersects(&west, &monaco));
        let europe: StacItem = parse(r#"{"bbox": [-10.0, 35.0, 30.0, 70.0]}"#);
        assert!(item_bbox_intersects(&europe, &monaco));
        let missing: StacItem = parse(r#"{"id": "some-item"}"#);
        assert!(item_bbox_intersects(&missing, &monaco));
    }

    #[test]
    fn urls() {
        assert_eq!(
            "https://other.example.com/foo.json",
            resolve_url("https://stac.overturemaps.org/catalog.json", "https://other.example.com/foo.json")
        );
        assert_eq!(
            "https://stac.overturemaps.org/2026-02-18.0/catalog.json",
            resolve_url("https://stac.overturemaps.org/catalog.json", "./2026-02-18.0/catalog.json")
        );
        assert_eq!(
            "https://stac.overturemaps.org/2026-02-18.0/buildings/catalog.json",
            resolve_url("https://stac.overturemaps.org/2026-02-18.0/catalog.json", "./buildings/catalog.json")
        );
        assert_eq!("https://a/x/c.json", resolve_url("https://a/x/y/z.json", "../c.json"));
        assert_eq!("https://a/c.json", resolve_url("https://a/x/y/z.json", "/c.json"));
    }

    const CATALOG: (&str, &str) = (
        "https://stac.example.com/catalog.json",
        r#"{"latest": "2026-02-18.0", "links": [
          {"rel": "child", "href": "./2026-02-18.0/catalog.json", "latest": true}
        ]}"#,
    );
    const RELEASE: (&str, &str) = (
        "https://stac.example.com/2026-02-18.0/catalog.json",
        r#"{"links": [{"rel": "child", "href": "./buildings/catalog.json", "title": "buildings"}]}"#,
    );
    const THEME: (&str, &str) = (
        "https://stac.example.com/2026-02-18.0/buildings/catalog.json",
        r#"{"links": [{"rel": "child", "href": "./building/collection.json", "title": "building"}]}"#,
    );
    const COLLECTION_URL: &str = "https://stac.example.com/2026-02-18.0/buildings/building/collection.json";
    const ITEM_BASE: &str = "https://stac.example.com/2026-02-18.0/buildings/building/";

    #[test]
    fn get_parquet_urls_filters_by_bbox() {
        let west = format!("{ITEM_BASE}item-west.json");
        let europe = format!("{ITEM_BASE}item-europe.json");
        let stac = stub_stac(&[
            CATALOG,
            RELEASE,
            THEME,
            (
                COLLECTION_URL,
                r#"{"links": [{"rel": "item", "href": "./item-west.json"}, {"rel": "item", "href": "./item-europe.json"}]}"#,
            ),
            (
                &west,
                r#"{"bbox": [-179.9, -84.3, -2.8, -22.5], "assets": {"aws": {"href": "https://s3.example.com/part-west.parquet"}}}"#,
            ),
            (
                &europe,
                r#"{"bbox": [-10.0, 35.0, 30.0, 70.0], "assets": {"aws": {"href": "https://s3.example.com/part-europe.parquet"}}}"#,
            ),
        ]);
        let monaco = Bounds::new(Some(Envelope::new(7.3, 7.5, 43.7, 43.8)));
        let result = stac
            .get_parquet_urls_from("https://stac.example.com/catalog.json", "buildings", "building", &monaco)
            .unwrap();
        assert_eq!(vec!["https://s3.example.com/part-europe.parquet"], result);
    }

    #[test]
    fn get_parquet_urls_world_bounds_includes_all() {
        let a = format!("{ITEM_BASE}item-a.json");
        let b = format!("{ITEM_BASE}item-b.json");
        let stac = stub_stac(&[
            CATALOG,
            RELEASE,
            THEME,
            (
                COLLECTION_URL,
                r#"{"links": [{"rel": "item", "href": "./item-a.json"}, {"rel": "item", "href": "./item-b.json"}]}"#,
            ),
            (
                &a,
                r#"{"bbox": [-179.9, -84.3, -2.8, -22.5], "assets": {"aws": {"href": "https://s3.example.com/part-a.parquet"}}}"#,
            ),
            (
                &b,
                r#"{"bbox": [-10.0, 35.0, 30.0, 70.0], "assets": {"aws": {"href": "https://s3.example.com/part-b.parquet"}}}"#,
            ),
        ]);
        let result = stac
            .get_parquet_urls_from("https://stac.example.com/catalog.json", "buildings", "building", &Bounds::world())
            .unwrap();
        assert_eq!(vec!["https://s3.example.com/part-a.parquet", "https://s3.example.com/part-b.parquet"], result);
    }

    #[test]
    fn get_parquet_urls_falls_back_to_azure() {
        let item = format!("{ITEM_BASE}item-azure.json");
        let stac = stub_stac(&[
            CATALOG,
            RELEASE,
            THEME,
            (COLLECTION_URL, r#"{"links": [{"rel": "item", "href": "./item-azure.json"}]}"#),
            (
                &item,
                r#"{"bbox": [-10.0, 35.0, 30.0, 70.0], "assets": {"azure": {"href": "https://azure.example.com/part-a.parquet"}}}"#,
            ),
        ]);
        let result = stac
            .get_parquet_urls_from("https://stac.example.com/catalog.json", "buildings", "building", &Bounds::world())
            .unwrap();
        assert_eq!(vec!["https://azure.example.com/part-a.parquet"], result);
    }

    #[test]
    fn get_parquet_urls_skips_item_with_no_asset() {
        let item = format!("{ITEM_BASE}item-no-asset.json");
        let stac = stub_stac(&[
            CATALOG,
            RELEASE,
            THEME,
            (COLLECTION_URL, r#"{"links": [{"rel": "item", "href": "./item-no-asset.json"}]}"#),
            (&item, r#"{"bbox": [-10.0, 35.0, 30.0, 70.0], "assets": {}}"#),
        ]);
        let result = stac
            .get_parquet_urls_from("https://stac.example.com/catalog.json", "buildings", "building", &Bounds::world())
            .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn unknown_theme_or_type_fails() {
        let stac = stub_stac(&[CATALOG, RELEASE]);
        assert!(
            stac.get_parquet_urls_from(
                "https://stac.example.com/catalog.json",
                "nonexistent",
                "building",
                &Bounds::world()
            )
            .is_err()
        );
        let stac = stub_stac(&[CATALOG, RELEASE, THEME]);
        assert!(
            stac.get_parquet_urls_from(
                "https://stac.example.com/catalog.json",
                "buildings",
                "nonexistent",
                &Bounds::world()
            )
            .is_err()
        );
    }
}
