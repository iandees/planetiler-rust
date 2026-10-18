//! Port of `ConfiguredMapMain.java`: main driver to create maps configured by a YAML file.
//!
//! Parses the config file into a [`ConfiguredProfile`] and resolves the sources it reads from. The `Planetiler`
//! orchestrator isn't ported yet, so [`configure`] returns the profile and sources for the caller to run.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use planetiler_core::config::arguments::Arguments;

use crate::configschema::{DataSourceType, SchemaConfig};
use crate::configured_profile::ConfiguredProfile;
use crate::contexts::{self, Root};
use crate::expression::ParseException;
use crate::samples;
use crate::source::Source;

/// A source to add to the planetiler run (Java `configureSource`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfiguredSource {
    pub id: String,
    pub source_type: DataSourceType,
    /// Where the source file is (or will be downloaded to).
    pub local_path: PathBuf,
    pub url: Option<String>,
    pub projection: Option<String>,
}

/// A schema loaded and configured from arguments.
pub struct ConfiguredMap {
    /// The root context with argument values (use `root.arguments()` to configure the planetiler run).
    pub root: Root,
    pub profile: Arc<ConfiguredProfile>,
    pub sources: Vec<ConfiguredSource>,
}

/// Loads a schema from a file, or if the file doesn't exist, from the bundled samples.
pub fn load_schema(schema_file: &str) -> anyhow::Result<SchemaConfig> {
    let path = Path::new(schema_file);
    if path.exists() {
        SchemaConfig::load(path)
    } else if let Some(contents) = samples::get(schema_file) {
        SchemaConfig::load_str(contents).with_context(|| format!("Unable to parse sample {schema_file}"))
    } else {
        anyhow::bail!("Schema file not found: {schema_file}")
    }
}

/// Resolves where to read `source` from: its `local_path`, or a file in `sources_dir` named after its `url`.
pub fn configure_source(sources_dir: &Path, source: &Source) -> Result<ConfiguredSource, ParseException> {
    let local_path = match &source.local_path {
        Some(path) => path.clone(),
        None => {
            if source.url.is_none() {
                return Err(ParseException::new(format!("Must provide either a url or path for {}", source.id)));
            }
            sources_dir.join(source.default_file_url())
        }
    };
    Ok(ConfiguredSource {
        id: source.id.clone(),
        source_type: source.source_type,
        local_path,
        url: source.url.clone(),
        projection: source.projection.clone(),
    })
}

/// Parses the `schema` argument into a profile and the list of sources it reads (everything `ConfiguredMapMain.main`
/// does before running planetiler).
pub fn configure(arguments: &Arguments) -> anyhow::Result<ConfiguredMap> {
    let sources_dir = Path::new("data").join("sources");
    let schema_file = arguments.get_required_string("schema", "Location of YML-format schema definition file")?;
    let schema = load_schema(&schema_file)?;
    // use default argument values from config file as fallback if not set from command-line or env vars
    let root = contexts::build_root_context(arguments, schema.args())?;
    let profile = ConfiguredProfile::new(schema, root.clone())?;
    let sources = profile
        .sources()?
        .iter()
        .map(|source| configure_source(&sources_dir, source))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ConfiguredMap { root, profile: Arc::new(profile), sources })
}

/// Main entrypoint (Java `ConfiguredMapMain.main`).
pub fn main<S: AsRef<str>>(args: &[S]) -> anyhow::Result<()> {
    let arguments = Arguments::from_env_or_args(args);
    let map = configure(&arguments)?;
    // TODO(port): run planetiler once the orchestrator is ported:
    //   let planetiler = Planetiler::create(map.root.arguments()).set_profile(map.profile);
    //   for source in map.sources: add_osm_source / add_shapefile_source(projection) / add_geopackage_source(projection)
    //   / add_geojson_source(id, local_path, url)
    //   planetiler.overwrite_output("data/output.mbtiles").run()
    anyhow::bail!(
        "planetiler orchestrator is not ported yet: configured profile '{}' with {} sources",
        planetiler_core::profile::Profile::name(&*map.profile),
        map.sources.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configure_bundled_sample() {
        let arguments = Arguments::of_pairs([("schema", "owg_simple.yml")]);
        let map = configure(&arguments).unwrap();
        assert_eq!(2, map.sources.len());
        let osm = map.sources.iter().find(|s| s.id == "osm").unwrap();
        assert_eq!(Path::new("data/sources/geofabrik_monaco.osm.pbf"), osm.local_path);
        assert!(load_schema("missing.yml").is_err());
    }
}
