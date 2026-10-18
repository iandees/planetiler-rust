//! Port of `util/AwsOsm.java`: finds `planet.osm.pbf` download URLs in public S3 buckets such as the
//! [AWS Open Data Registry](https://registry.opendata.aws/osm/) and [Overture Maps Foundation](https://overturemaps.org).
//!
//! Java parses the S3 `ListBucketResult` XML with Jackson; this port uses a minimal hand-written parser that only
//! extracts the `Contents/Key`, `NextContinuationToken` and `IsTruncated` elements.

use std::sync::LazyLock;

use anyhow::bail;
use parking_lot::Mutex;

use crate::config::planetiler_config::PlanetilerConfig;

const MAX_PAGES: usize = 100;

static OSM_PDS: LazyLock<AwsOsm> = LazyLock::new(|| AwsOsm::new("https://osm-pds.s3.amazonaws.com/"));
static OVERTURE: LazyLock<AwsOsm> = LazyLock::new(|| AwsOsm::new("https://overturemaps-us-west-2.s3.amazonaws.com/"));

/// One page of an S3 bucket listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexXml {
    /// Keys of each entry (`Contents/Key`).
    pub contents: Vec<String>,
    pub next_token: Option<String>,
    pub truncated: bool,
}

/// Searches an S3 bucket for `.osm.pbf` files. The bucket index is fetched once and cached.
pub struct AwsOsm {
    bucket_index_url: String,
    entries: Mutex<Vec<String>>,
}

impl AwsOsm {
    pub fn new(bucket_index_url: &str) -> AwsOsm {
        AwsOsm { bucket_index_url: bucket_index_url.to_string(), entries: Mutex::new(vec![]) }
    }

    /// The OSM planet bucket on the AWS Open Data Registry.
    pub fn osm_pds() -> &'static AwsOsm {
        &OSM_PDS
    }

    /// The Overture Maps bucket.
    pub fn overture() -> &'static AwsOsm {
        &OVERTURE
    }

    /// Fetches the bucket index and returns the URL of a `.osm.pbf` file where the snapshot matches `search_query`, or
    /// the latest snapshot if `search_query == "latest"`.
    pub fn get_download_url(&self, search_query: &str, config: &PlanetilerConfig) -> anyhow::Result<String> {
        let mut entries = self.entries.lock();
        if entries.is_empty() {
            let mut result = vec![];
            let mut next_page_param = Some(String::new());
            let mut page_num = 0;
            while let Some(param) = next_page_param {
                page_num += 1;
                if page_num > MAX_PAGES {
                    bail!("Too many entries in {} to page through", self.bucket_index_url);
                }
                let xml = crate::util::downloader::fetch_string(
                    &format!("{}?list-type=2{param}", self.bucket_index_url),
                    config,
                )?;
                let page = parse_index_xml(&xml);
                result.extend(page.contents);
                next_page_param = match (page.truncated, page.next_token) {
                    (true, Some(token)) => Some(format!("&continuation-token={}", url_encode(&token))),
                    _ => None,
                };
            }
            *entries = result;
        }
        self.search_index_for_download_url(search_query, &entries)
    }

    /// Searches bucket keys `index` for a `.osm.pbf` file matching `search_query`.
    pub fn search_index_for_download_url(&self, search_query: &str, index: &[String]) -> anyhow::Result<String> {
        if search_query.eq_ignore_ascii_case("latest") {
            index
                .iter()
                .filter(|key| key.ends_with(".osm.pbf"))
                .map(|key| format!("{}{key}", self.bucket_index_url))
                .max()
                .ok_or_else(|| anyhow::anyhow!("Unable to find latest AWS osm download URL"))
        } else {
            let suffix = format!("/planet-{search_query}.osm.pbf");
            let results: Vec<String> = index
                .iter()
                .filter(|key| key.ends_with(&suffix))
                .map(|key| format!("{}{key}", self.bucket_index_url))
                .collect();
            match results.len() {
                0 => bail!("Unable to find AWS osm download URL for {search_query}"),
                1 => Ok(results.into_iter().next().unwrap()),
                _ => bail!("Found multiple AWS osm download URLs for {search_query}: {results:?}"),
            }
        }
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.*".contains(&b) {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Returns the text content of every `<tag>...</tag>` element in `xml`.
fn elements<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut result = vec![];
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        result.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    result
}

/// Parses an S3 `ListBucketResult` XML document.
pub fn parse_index_xml(xml: &str) -> IndexXml {
    let contents = elements(xml, "Contents")
        .into_iter()
        .flat_map(|c| elements(c, "Key").into_iter().map(|k| unescape_xml(k.trim())).collect::<Vec<_>>())
        .collect();
    let next_token = elements(xml, "NextContinuationToken").first().map(|t| unescape_xml(t.trim()));
    let truncated = elements(xml, "IsTruncated").first().is_some_and(|t| t.trim().eq_ignore_ascii_case("true"));
    IndexXml { contents, next_token, truncated }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents(keys: &[&str]) -> String {
        keys.iter()
            .map(|k| {
                format!(
                    "<Contents><Key>{k}</Key><LastModified>2023-01-08T11:34:39.000Z</LastModified>\
                     <ETag>\"49a5e81ff968d16a0ac6e0b78c4e3303-924\"</ETag><Size>1</Size>\
                     <StorageClass>STANDARD</StorageClass></Contents>\n"
                )
            })
            .collect()
    }

    fn response() -> String {
        let mut keys = vec![];
        for date in ["230102", "230109", "230116", "230123"] {
            for ext in [".orc", ".osm.pbf", ".osm.pbf.md5", ".osm.pbf.torrent", ".osm.pbf.torrent.md5"] {
                keys.push(format!("2023/planet-{date}{ext}"));
            }
        }
        keys.push("2023/planet-230130.orc".into());
        keys.push("2023/planet-230130.osm.pbf".into());
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        format!(
            r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
              <Name>osm-pds</Name>
              <Prefix/>
              <NextContinuationToken>nextpage</NextContinuationToken>
              <KeyCount>1000</KeyCount>
              <MaxKeys>1000</MaxKeys>
              <IsTruncated>true</IsTruncated>
              {}
            </ListBucketResult>"#,
            contents(&keys)
        )
    }

    fn overture_response() -> String {
        format!(
            r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
              <Name>overturemaps-us-west-2</Name>
              <Prefix/>
              <KeyCount>7</KeyCount>
              <MaxKeys>1000</MaxKeys>
              <IsTruncated>false</IsTruncated>
              {}
            </ListBucketResult>"#,
            contents(&[
                "release/2023-04-02-alpha/building-extracts/2023-04-02-alpha-cook_county_il.geojsonseq.gz",
                "release/2023-04-02-alpha/building-extracts/2023-04-02-alpha-eastern_ma.geojsonseq.gz",
                "release/2023-04-02-alpha/building-extracts/2023-04-02-alpha-king_county_wa.geojsonseq.gz",
                "release/2023-04-02-alpha/building-extracts/2023-04-02-alpha-maricopa_and_pinal_counties_az.geojsonseq.gz",
                "release/2023-04-02-alpha/building-extracts/2023-04-02-alpha-orange_county_fl.geojsonseq.gz",
                "release/2023-04-02-alpha/building-extracts/2023-04-02-alpha-santa_clara_county_ca.geojsonseq.gz",
                "release/2023-04-02-alpha/planet-2023-04-02-alpha.osm.pbf",
            ])
        )
    }

    #[test]
    fn found() {
        let aws_osm = AwsOsm::new("https://base.url/");
        let index = parse_index_xml(&response());
        assert_eq!(Some("nextpage".to_string()), index.next_token);
        assert!(index.truncated);
        assert_eq!(
            "https://base.url/2023/planet-230102.osm.pbf",
            aws_osm.search_index_for_download_url("230102", &index.contents).unwrap()
        );
        assert_eq!(
            "https://base.url/2023/planet-230116.osm.pbf",
            aws_osm.search_index_for_download_url("230116", &index.contents).unwrap()
        );
    }

    #[test]
    fn latest() {
        let aws_osm = AwsOsm::new("https://base.url/");
        let index = parse_index_xml(&response());
        assert_eq!(
            "https://base.url/2023/planet-230130.osm.pbf",
            aws_osm.search_index_for_download_url("latest", &index.contents).unwrap()
        );
    }

    #[test]
    fn not_found() {
        let aws_osm = AwsOsm::new("https://base.url/");
        let index = parse_index_xml(&response());
        assert!(aws_osm.search_index_for_download_url("1231", &index.contents).is_err());
    }

    #[test]
    fn overture_maps() {
        let aws_osm = AwsOsm::new("https://base.url/");
        let index = parse_index_xml(&overture_response());
        assert!(!index.truncated);
        assert_eq!(None, index.next_token);
        let expected = "https://base.url/release/2023-04-02-alpha/planet-2023-04-02-alpha.osm.pbf";
        assert_eq!(expected, aws_osm.search_index_for_download_url("latest", &index.contents).unwrap());
        assert_eq!(expected, aws_osm.search_index_for_download_url("2023-04-02-alpha", &index.contents).unwrap());
    }

    #[test]
    fn xml_escapes() {
        let index = parse_index_xml("<Contents><Key>a&amp;b.osm.pbf</Key></Contents>");
        assert_eq!(vec!["a&b.osm.pbf".to_string()], index.contents);
        assert_eq!("a%2Fb+c", url_encode("a/b c"));
    }
}
