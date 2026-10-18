//! Port of `archive/TileArchiveConfig.java`: definition for a tileset, parsed from a URI-like string.
//!
//! [`TileArchiveConfig::from`] accepts:
//! - A platform-specific absolute or relative path like `"./archive.mbtiles"` or `"C:\root\archive.mbtiles"`
//! - A URI pointing at a file, like `"file:///root/archive.pmtiles"`
//!
//! Both of these can also have archive-specific options added to the end, for example
//! `"output.mbtiles?compact=false&page_size=16384"`.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail};
use indexmap::IndexMap;

use crate::config::arguments::Arguments;
use crate::files::files_archive_utils;
use crate::geo::tile_order::TileOrder;
use crate::stream::stream_archive_utils;

/// Format of a tile archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    Mbtiles,
    Pmtiles,
    // should be before PBF in order to avoid collisions
    Files,
    Csv,
    /// identical to [`Format::Csv`] - except for the column separator
    Tsv,
    Proto,
    /// identical to [`Format::Proto`]
    Pbf,
    Json,
}

impl Format {
    pub fn values() -> &'static [Format] {
        &[
            Format::Mbtiles,
            Format::Pmtiles,
            Format::Files,
            Format::Csv,
            Format::Tsv,
            Format::Proto,
            Format::Pbf,
            Format::Json,
        ]
    }

    pub fn id(&self) -> &'static str {
        match self {
            Format::Mbtiles => "mbtiles",
            Format::Pmtiles => "pmtiles",
            Format::Files => "files",
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Proto => "proto",
            Format::Pbf => "pbf",
            Format::Json => "json",
        }
    }

    pub fn preferred_order(&self) -> TileOrder {
        match self {
            Format::Pmtiles => TileOrder::Hilbert,
            _ => TileOrder::Tms,
        }
    }

    /// TODO mbtiles could support append in the future by using insert statements with an "on conflict"-clause
    /// (i.e. upsert) and by creating tables only if they don't exist, yet.
    pub fn supports_append(&self) -> bool {
        !matches!(self, Format::Mbtiles | Format::Pmtiles)
    }

    pub fn supports_concurrent_writes(&self) -> bool {
        !matches!(self, Format::Mbtiles | Format::Pmtiles)
    }

    fn is_uri_supported(&self, path: Option<&str>) -> bool {
        let Some(path) = path else { return false };
        match self {
            // Java checks the whole path for '.', which misdetects paths with dots in parent directories; only the
            // file name is checked here
            Format::Files => path.ends_with('/') || path.contains('{') || !file_name(path).contains('.'),
            _ => path.ends_with(&format!(".{}", self.id())),
        }
    }

    fn is_query_format_supported(&self, query_format: Option<&str>) -> bool {
        query_format == Some(self.id())
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// Scheme for accessing the archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scheme {
    File,
}

impl Scheme {
    pub fn id(&self) -> &'static str {
        match self {
            Scheme::File => "file",
        }
    }
}

/// Definition for a tileset, parsed from a URI-like string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileArchiveConfig {
    /// The format of the archive, either inferred from the filename extension or the `?format=` query parameter.
    pub format: Format,
    /// Scheme for accessing the archive.
    pub scheme: Scheme,
    /// Full URI including scheme, location, and options.
    pub uri: String,
    /// Parsed query parameters from the definition string.
    pub options: IndexMap<String, String>,
    /// Decoded path component of `uri`.
    path: Option<String>,
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn percent_decode(s: &str, plus_as_space: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' && i + 3 <= bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if plus_as_space && b == b'+' { b' ' } else { b });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'A'..='Z'
            | 'a'..='z'
            | '0'..='9'
            | '-'
            | '.'
            | '_'
            | '~'
            | '/'
            | ':'
            | '@'
            | '!'
            | '$'
            | '&'
            | '\''
            | '('
            | ')'
            | '*'
            | '+'
            | ','
            | ';'
            | '=' => out.push(c),
            _ => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

/// Lexically normalize a path (resolve `.` and `..`) without touching the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    result.push("..");
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn absolutize(path: &str) -> PathBuf {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
    };
    normalize_path(&abs)
}

fn parse_query(query: Option<&str>) -> IndexMap<String, String> {
    let mut result = IndexMap::new();
    if let Some(query) = query {
        for part in query.split('&') {
            let (k, v) = match part.split_once('=') {
                Some((k, v)) => (percent_decode(k, true), percent_decode(v, true)),
                None => (percent_decode(part, true), "true".to_string()),
            };
            result.insert(k, v);
        }
    }
    result
}

/// Splits `scheme:rest` if `s` starts with a URI scheme (at least 2 characters to avoid windows drive letters).
fn split_scheme(s: &str) -> Option<(&str, &str)> {
    let idx = s.find(':')?;
    let scheme = &s[..idx];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if scheme.len() >= 2
        && first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    {
        Some((scheme, &s[idx + 1..]))
    } else {
        None
    }
}

impl TileArchiveConfig {
    /// Parses a string definition of a tileset from a URI-like string.
    pub fn from(string: &str) -> anyhow::Result<TileArchiveConfig> {
        let mut string = string.to_string();
        // unix paths parse fine as URIs, but need to explicitly parse windows paths with backslashes
        if string.contains('\\') {
            let (path, query) = match string.split_once('?') {
                Some((p, q)) => (p.to_string(), Some(q.to_string())),
                None => (string.clone(), None),
            };
            let abs = absolutize(&path);
            let mut s = format!("file://{}", percent_encode_path(&abs.to_string_lossy().replace('\\', "/")));
            if let Some(q) = query {
                s.push('?');
                s.push_str(&q);
            }
            string = s;
        }
        // be more generous and encode some characters for the users
        string = string.replace('{', "%7B").replace('}', "%7D");

        let (scheme, rest) = match split_scheme(&string) {
            Some((scheme, rest)) => (Some(scheme.to_string()), rest.to_string()),
            None => (None, string.clone()),
        };
        let rest = rest.split('#').next().unwrap_or("").to_string();
        let (raw_path, raw_query) = match rest.split_once('?') {
            Some((p, q)) => (p.to_string(), Some(q.to_string())),
            None => (rest.clone(), None),
        };

        let (scheme, path, uri) = match scheme {
            None => {
                let decoded = percent_decode(&raw_path, false);
                let mut base = absolutize(&decoded).to_string_lossy().into_owned();
                if decoded.ends_with('/') && !base.ends_with('/') {
                    base.push('/');
                }
                let mut uri = format!("file://{}", percent_encode_path(&base));
                if let Some(q) = &raw_query {
                    uri.push('?');
                    uri.push_str(q);
                }
                (Scheme::File, Some(base), uri)
            }
            Some(scheme) => {
                if scheme != Scheme::File.id() {
                    // still need a valid format first to mirror Java's error precedence
                    let path = percent_decode(&raw_path, false);
                    Self::get_format(Some(&path), &parse_query(raw_query.as_deref()), &string)?;
                    bail!("Unsupported scheme {scheme} from {string}");
                }
                let path_part = if let Some(after) = raw_path.strip_prefix("//") {
                    // skip (empty) authority
                    match after.find('/') {
                        Some(idx) => after[idx..].to_string(),
                        None => String::new(),
                    }
                } else {
                    raw_path.clone()
                };
                (Scheme::File, Some(percent_decode(&path_part, false)), string.clone())
            }
        };
        let options = parse_query(raw_query.as_deref());
        let format = Self::get_format(path.as_deref(), &options, &uri)?;
        Ok(TileArchiveConfig { format, scheme, uri, options, path })
    }

    /// Alias for [`TileArchiveConfig::from`] taking a path.
    pub fn from_path(path: &Path) -> anyhow::Result<TileArchiveConfig> {
        Self::from(&path.to_string_lossy())
    }

    fn get_format(path: Option<&str>, options: &IndexMap<String, String>, uri: &str) -> anyhow::Result<Format> {
        let format = options.get("format").map(String::as_str);
        for value in Format::values() {
            if value.is_query_format_supported(format) {
                return Ok(*value);
            }
        }
        if let Some(format) = format {
            bail!("Unsupported format {format} from {uri}");
        }
        for value in Format::values() {
            if value.is_uri_supported(path) {
                return Ok(*value);
            }
        }
        let ext =
            path.map(file_name).and_then(|p| p.rfind('.').map(|i| p[i + 1..].to_string())).filter(|e| !e.is_empty());
        Err(anyhow!("Unsupported format {} from {uri}", ext.as_deref().unwrap_or("null")))
    }

    /// Returns the local path on disk that this archive reads/writes to, or `None` if it is not on disk.
    pub fn get_local_path(&self) -> Option<PathBuf> {
        match self.scheme {
            Scheme::File => {
                let p = self.path.as_deref()?;
                let trimmed = if p.len() > 1 { p.trim_end_matches('/') } else { p };
                Some(PathBuf::from(if trimmed.is_empty() { "/" } else { trimmed }))
            }
        }
    }

    /// Returns the local *base* path for this archive, for which directories should be pre-created for.
    pub fn get_local_base_path(&self) -> Option<PathBuf> {
        let p = self.get_local_path()?;
        if self.format == Format::Files { Some(files_archive_utils::clean_base_path(&p)) } else { Some(p) }
    }

    /// Deletes the archive if possible.
    pub fn delete(&self) -> std::io::Result<()> {
        if let Some(p) = self.get_local_base_path() {
            if p.is_dir() {
                std::fs::remove_dir_all(&p)?;
            } else if p.exists() {
                std::fs::remove_file(&p)?;
            }
        }
        Ok(())
    }

    /// Returns `true` if the archive already exists, `false` otherwise.
    pub fn exists(&self) -> bool {
        self.exists_at(self.get_local_base_path().as_deref())
    }

    /// Returns `true` if the archive at `p` already exists, `false` otherwise.
    pub fn exists_at(&self, p: Option<&Path>) -> bool {
        let Some(p) = p else { return false };
        if self.format != Format::Files {
            p.exists()
        } else {
            // file-archive exists only if it has any contents
            std::fs::read_dir(p).map(|mut d| d.next().is_some()).unwrap_or(false)
        }
    }

    /// Returns the current size of this archive.
    pub fn size(&self) -> u64 {
        self.get_local_path().map(|p| crate::util::file_utils::size(&p).unwrap_or(0)).unwrap_or(0)
    }

    /// Returns options that return the value directly from the query parameters in the URI, or from `arguments`
    /// prefixed by `"<format>_"`.
    pub fn apply_fallbacks(&self, arguments: &Arguments) -> ArchiveOptions {
        ArchiveOptions::of_map(self.options.clone())
            .or_else(ArchiveOptions::from_arguments_with_prefix(arguments.clone(), self.format.id()))
    }

    pub fn get_path_for_multi_threaded_writer(&self, index: usize) -> anyhow::Result<PathBuf> {
        let path = self.get_local_path().ok_or_else(|| anyhow!("no local path for {}", self.uri))?;
        match self.format {
            Format::Csv | Format::Tsv | Format::Json | Format::Proto | Format::Pbf => {
                Ok(stream_archive_utils::construct_indexed_path(&path, index))
            }
            Format::Files => Ok(path),
            other => bail!("not supported by {other}"),
        }
    }
}

/// Normalizes an option key like Java `Arguments`: lowercase with `.`, `-` and `_` mapped to `_`.
pub fn normalize_key(key: &str) -> String {
    key.chars().map(|c| if matches!(c, '.' | '-' | '_') { '_' } else { c.to_ascii_lowercase() }).collect()
}

#[derive(Clone, Debug)]
enum OptionsLayer {
    Map(IndexMap<String, String>),
    Args { args: Arguments, prefix: Option<String>, allowed: Option<Vec<String>> },
}

impl OptionsLayer {
    fn get(&self, option: &str) -> Option<String> {
        let normalized = normalize_key(option);
        match self {
            OptionsLayer::Map(map) => map.get(&normalized).cloned(),
            OptionsLayer::Args { args, prefix, allowed } => {
                if let Some(allowed) = allowed
                    && !allowed.contains(&normalized)
                {
                    return None;
                }
                let key = match prefix {
                    Some(prefix) => normalize_key(&format!("{prefix}_{option}")),
                    None => normalized,
                };
                args.get_arg(&key).or_else(|| {
                    // tolerate un-normalized keys in the backing arguments
                    let dashed = key.replace('_', "-");
                    args.get_arg(&dashed)
                })
            }
        }
    }
}

/// Layered view of archive options: URI query parameters falling back to (prefixed) command-line [`Arguments`].
///
/// Mirrors the subset of Java's `Arguments` API used by the archive implementations (`Arguments.of(map)`,
/// `orElse`, `withPrefix`, `subset`, `getString`, `getBoolean`, `getInteger`), including `"key|legacy_key"`
/// alternatives.
///
/// TODO(port): replace with `crate::config::arguments::Arguments` once its full API (`or_else`, `with_prefix`,
/// `subset`, typed getters) is ported.
#[derive(Clone, Debug, Default)]
pub struct ArchiveOptions {
    layers: Vec<OptionsLayer>,
}

impl ArchiveOptions {
    pub fn empty() -> ArchiveOptions {
        ArchiveOptions::default()
    }

    /// Options from a map of (un-normalized) keys to values.
    pub fn of_map<K: AsRef<str>, V: Into<String>>(map: impl IntoIterator<Item = (K, V)>) -> ArchiveOptions {
        let map = map.into_iter().map(|(k, v)| (normalize_key(k.as_ref()), v.into())).collect();
        ArchiveOptions { layers: vec![OptionsLayer::Map(map)] }
    }

    /// Options from key/value pairs, like Java `Arguments.of("k1", "v1", "k2", "v2")`.
    pub fn of_pairs(pairs: &[(&str, &str)]) -> ArchiveOptions {
        Self::of_map(pairs.iter().map(|(k, v)| (*k, *v)))
    }

    pub fn from_arguments(args: Arguments) -> ArchiveOptions {
        ArchiveOptions { layers: vec![OptionsLayer::Args { args, prefix: None, allowed: None }] }
    }

    /// Equivalent of `arguments.withPrefix(prefix)`: looks up `<prefix>_<key>`.
    pub fn from_arguments_with_prefix(args: Arguments, prefix: &str) -> ArchiveOptions {
        ArchiveOptions { layers: vec![OptionsLayer::Args { args, prefix: Some(prefix.to_string()), allowed: None }] }
    }

    /// Equivalent of `arguments.subset(keys...)`.
    pub fn from_arguments_subset(args: Arguments, keys: &[&str]) -> ArchiveOptions {
        let allowed = keys.iter().map(|k| normalize_key(k)).collect();
        ArchiveOptions { layers: vec![OptionsLayer::Args { args, prefix: None, allowed: Some(allowed) }] }
    }

    /// Returns options that use values from `self`, falling back to `other`.
    pub fn or_else(mut self, other: ArchiveOptions) -> ArchiveOptions {
        self.layers.extend(other.layers);
        self
    }

    fn get(&self, option: &str) -> Option<String> {
        self.layers.iter().find_map(|l| l.get(option))
    }

    /// Raw value of `key` (which may contain `|`-separated deprecated alternatives), trimmed.
    pub fn get_arg(&self, key: &str) -> Option<String> {
        for (i, option) in key.split('|').enumerate() {
            let option = option.trim();
            if let Some(value) = self.get(option) {
                if i != 0 {
                    tracing::warn!("Argument '{option}' is deprecated");
                }
                return Some(value.trim().to_string());
            }
        }
        None
    }

    pub fn get_string_opt(&self, key: &str, description: &str) -> Option<String> {
        let value = self.get_arg(key);
        tracing::debug!("argument: {key}={value:?} ({description})");
        value
    }

    pub fn get_string(&self, key: &str, description: &str, default_value: &str) -> String {
        self.get_string_opt(key, description).unwrap_or_else(|| default_value.to_string())
    }

    pub fn get_boolean(&self, key: &str, description: &str, default_value: bool) -> bool {
        match self.get_string_opt(key, description) {
            Some(v) => v.eq_ignore_ascii_case("true"),
            None => default_value,
        }
    }

    pub fn get_integer(&self, key: &str, description: &str, default_value: i64) -> anyhow::Result<i64> {
        match self.get_string_opt(key, description) {
            None => Ok(default_value),
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "false" => Ok(0),
                "true" => Ok(default_value),
                other => other.parse().map_err(|e| anyhow!("invalid integer for {key}: {other} ({e})")),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(p: &str) -> PathBuf {
        absolutize(p)
    }

    #[test]
    fn test_mbtiles() {
        let config = TileArchiveConfig::from("output.mbtiles").unwrap();
        assert_eq!(Format::Mbtiles, config.format);
        assert_eq!(Scheme::File, config.scheme);
        assert!(config.options.is_empty());
        assert_eq!(Some(abs("output.mbtiles")), config.get_local_path());
    }

    #[test]
    fn test_mbtiles_with_options() {
        let config = TileArchiveConfig::from("output.mbtiles?compact=true").unwrap();
        assert_eq!(Format::Mbtiles, config.format);
        assert_eq!(Scheme::File, config.scheme);
        assert_eq!(IndexMap::from([("compact".to_string(), "true".to_string())]), config.options);
        assert_eq!(Some(abs("output.mbtiles")), config.get_local_path());
    }

    #[test]
    fn test_pmtiles() {
        assert_eq!(Format::Pmtiles, TileArchiveConfig::from("output.pmtiles").unwrap().format);
        assert_eq!(Format::Pmtiles, TileArchiveConfig::from("output.mbtiles?format=pmtiles").unwrap().format);
        assert_eq!(Format::Pmtiles, TileArchiveConfig::from("file:///output.mbtiles?format=pmtiles").unwrap().format);
        assert_eq!(
            Some(PathBuf::from("/output.mbtiles")),
            TileArchiveConfig::from("file:///output.mbtiles?format=pmtiles").unwrap().get_local_path()
        );
    }

    #[test]
    fn test_by_format_param() {
        for format in Format::values() {
            let config = TileArchiveConfig::from(&format!("output?format={}", format.id())).unwrap();
            assert_eq!(*format, config.format);
            assert_eq!(Scheme::File, config.scheme);
            assert_eq!(Some(abs("output")), config.get_local_path());
            assert_eq!(IndexMap::from([("format".to_string(), format.id().to_string())]), config.options);
        }
    }

    #[test]
    fn test_get_path_for_multi_threaded_writer() {
        for format in Format::values() {
            let config = TileArchiveConfig::from(&format!("output?format={}", format.id())).unwrap();
            if !format.supports_concurrent_writes() {
                assert!(config.get_path_for_multi_threaded_writer(0).is_err());
                assert!(config.get_path_for_multi_threaded_writer(1).is_err());
            } else {
                let local = config.get_local_path().unwrap();
                assert_eq!(local, config.get_path_for_multi_threaded_writer(0).unwrap());
                let p = config.get_path_for_multi_threaded_writer(1).unwrap();
                match format {
                    Format::Files => assert_eq!(local, p),
                    _ => assert_eq!(local.parent().unwrap().join("output1"), p),
                }
            }
        }
    }

    #[test]
    fn test_exists_for_files_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("outdir");
        let config = TileArchiveConfig::from(&format!("{}?format=files", out.display())).unwrap();
        assert!(!config.exists());
        std::fs::create_dir(&out).unwrap();
        assert!(!config.exists());
        std::fs::write(out.join("1"), b"").unwrap();
        assert!(config.exists());
    }

    #[test]
    fn test_exists_for_non_files_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out.mbtiles");
        let config = TileArchiveConfig::from(&out.to_string_lossy()).unwrap();
        assert!(!config.exists());
        std::fs::write(&out, b"").unwrap();
        assert!(config.exists());
    }

    #[test]
    fn test_path_mapping() {
        for (path, format) in [
            // standard cases
            ("output", Format::Files),
            ("output.mbtiles", Format::Mbtiles),
            ("output.pmtiles", Format::Pmtiles),
            ("output.pbf", Format::Pbf),
            ("output.proto", Format::Proto),
            ("output.json", Format::Json),
            ("output.csv", Format::Csv),
            ("output.tsv", Format::Tsv),
            // special cases
            ("output.mbtiles/", Format::Files), // trailing slash => files - regardless of the extension
            ("output/", Format::Files),
            ("output.mbtiles/?format=proto", Format::Proto), // format query param has precedence
            ("tiles/{x}/{y}/{z}.pbf", Format::Files),
        ] {
            assert_eq!(format, TileArchiveConfig::from(path).unwrap().format, "{path}");
        }
    }

    #[test]
    fn test_unsupported() {
        assert!(TileArchiveConfig::from("output.xyz").is_err());
        assert!(TileArchiveConfig::from("output?format=xyz").is_err());
        assert!(TileArchiveConfig::from("http://output.mbtiles").is_err());
    }

    #[test]
    fn test_local_base_path() {
        let tmp = tempfile::tempdir().unwrap();
        for (path, base) in [
            ("a/output.mbtiles", "a/output.mbtiles"),
            ("a/tiles/{x}/{y}/{z}.pbf", "a/tiles"),
            ("a/tiles/{x}/{y}/{z}.pbf?format=proto", "a/tiles/{x}/{y}/{z}.pbf"),
        ] {
            let config = TileArchiveConfig::from(&format!("{}/{path}", tmp.path().display())).unwrap();
            assert_eq!(Some(tmp.path().join(base)), config.get_local_base_path(), "{path}");
        }
    }

    #[test]
    fn test_local_path() {
        let tmp = tempfile::tempdir().unwrap();
        for (path, local) in [
            ("a/output.mbtiles", "a/output.mbtiles"),
            ("a/tiles/{x}/{y}/{z}.pbf", "a/tiles/{x}/{y}/{z}.pbf"),
            ("a/tiles/{x}/{y}/{z}.pbf?format=proto", "a/tiles/{x}/{y}/{z}.pbf"),
        ] {
            let config = TileArchiveConfig::from(&format!("{}/{path}", tmp.path().display())).unwrap();
            assert_eq!(Some(tmp.path().join(local)), config.get_local_path(), "{path}");
        }
    }

    #[test]
    fn test_options_fallbacks() {
        let args = Arguments::of_pairs([("mbtiles_compact", "false"), ("other", "1")]);
        let config = TileArchiveConfig::from("output.mbtiles?page_size=1").unwrap();
        let options = config.apply_fallbacks(&args);
        assert!(!options.get_boolean("compact", "", true));
        assert_eq!("1", options.get_string("page-size", "", "x"));
        assert_eq!(None, options.get_arg("other"));
        assert_eq!(Some("false".to_string()), options.get_arg("missing|compact"));
        let subset = ArchiveOptions::from_arguments_subset(args, &["other"]);
        assert_eq!(Some("1".to_string()), subset.get_arg("other"));
        assert_eq!(None, subset.get_arg("mbtiles_compact"));
    }
}
