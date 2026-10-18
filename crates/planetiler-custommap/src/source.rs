//! Port of `Source.java`: a parsed source definition from a config file.

use std::path::PathBuf;
use std::sync::LazyLock;

use regex::Regex;

use crate::configschema::DataSourceType;

/// A parsed source definition from a config file, with any expressions in `url`, `local_path` and `projection`
/// evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub id: String,
    pub source_type: DataSourceType,
    pub url: Option<String>,
    pub local_path: Option<PathBuf>,
    pub projection: Option<String>,
}

static SCHEME: LazyLock<Regex> = LazyLock::new(|| Regex::new("^https?://").unwrap());
// Java: [\W&&[^.]]+ (ASCII non-word characters except '.')
static NON_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9_.]+").unwrap());

impl Source {
    pub fn new(
        id: impl Into<String>,
        source_type: DataSourceType,
        url: Option<String>,
        local_path: Option<PathBuf>,
        projection: Option<String>,
    ) -> Source {
        Source { id: id.into(), source_type, url, local_path, projection }
    }

    /// File name to download `url` to when no `local_path` is set.
    pub fn default_file_url(&self) -> String {
        let url = self.url.as_deref().unwrap_or("");
        let without_scheme = SCHEME.replace(url, "");
        let mut result = NON_WORD.replace_all(&without_scheme, "_").into_owned();
        if self.source_type == DataSourceType::Osm && !result.ends_with(".pbf") {
            result.push_str(".osm.pbf");
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_url() {
        let source = |url: &str| Source::new("osm", DataSourceType::Osm, Some(url.into()), None, None);
        assert_eq!("geofabrik_rhode_island.osm.pbf", source("geofabrik:rhode-island").default_file_url());
        assert_eq!("example.com_file.osm.pbf", source("https://example.com/file.osm.pbf").default_file_url());
        let shp = Source::new("s", DataSourceType::Shapefile, Some("http://a.b/c-d.zip".into()), None, None);
        assert_eq!("a.b_c_d.zip", shp.default_file_url());
    }
}
