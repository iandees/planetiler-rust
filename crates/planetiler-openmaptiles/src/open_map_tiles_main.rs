//! Port of `OpenMapTilesMain.java`: main entrypoint for generating a map using the OpenMapTiles schema.
//!
//! [`setup`] returns everything `OpenMapTilesMain.run` configures on the runner (default languages, wikidata
//! translations file, input sources with their default paths/URLs, output), [`configure`] applies it to a
//! [`Planetiler`] runner and [`run`] runs it; [`create_profile`] builds the profile once the runner has created
//! translations/config/stats.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use planetiler_core::config::arguments::Arguments;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::planetiler::Planetiler;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;

use crate::generated::open_map_tiles_schema;
use crate::open_map_tiles_profile::{
    LAKE_CENTERLINE_SOURCE, NATURAL_EARTH_SOURCE, OSM_SOURCE, OpenMapTilesProfile, WATER_POLYGON_SOURCE,
};

/// The kind of reader a source needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// `Planetiler.addOsmSource`
    Osm,
    /// `Planetiler.addShapefileSource` with an optional projection (i.e. `EPSG:3857`; `None` to read it from the
    /// shapefile's `.prj`).
    Shapefile { projection: Option<String> },
    /// `Planetiler.addNaturalEarthSource` (sqlite).
    NaturalEarth,
}

/// An input source: the runner uses `--<name>_path=...`/`--<name>_url=...` (or env/config equivalents) to override
/// the defaults, then downloads `default_url` to `default_path` if it does not exist (with `--download`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSpec {
    pub kind: SourceKind,
    pub name: &'static str,
    pub default_path: PathBuf,
    /// URL or `geofabrik:<area>`/`aws:latest` shortcut.
    pub default_url: String,
}

/// Everything `OpenMapTilesMain.run` configures on the `Planetiler` runner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenMapTilesSetup {
    pub default_languages: Vec<&'static str>,
    /// `fetchWikidataNameTranslations(...)` path.
    pub wikidata_names_path: PathBuf,
    pub sources: Vec<SourceSpec>,
    /// `setOutput("mbtiles", ...)`: the argument name and default output path.
    pub output_argument: &'static str,
    pub default_output: PathBuf,
}

/// Acknowledgments logged after a successful run.
pub const ACKNOWLEDGMENTS: &str = "Acknowledgments
Generated vector tiles are produced work of OpenStreetMap data.
Such tiles are reusable under CC-BY license granted by OpenMapTiles team:
- https://github.com/openmaptiles/openmaptiles/#license
Maps made with these vector tiles must display a visible credit:
- © OpenMapTiles © OpenStreetMap contributors
Thanks to all free, open source software developers and Open Data Contributors!
";

/// Returns the sources and settings `OpenMapTilesMain.run` uses for `arguments`.
pub fn setup(arguments: &Arguments) -> OpenMapTilesSetup {
    let data_dir = Path::new("data");
    let sources_dir = arguments.file("download_dir", "download directory", data_dir.join("sources"));
    // use --area=... argument, AREA=... env var or area=... in config to set the region of the world to use
    // will be ignored if osm_path or osm_url are set
    let area = arguments.get_string(
        "area",
        "name of the extract to download if osm_url/osm_path not specified (i.e. 'monaco' 'rhode island' 'australia' or 'planet')",
        "monaco",
    );
    let osm_file_name = {
        let mut name = String::new();
        let mut in_junk = false;
        for c in area.chars() {
            if c.is_ascii_alphabetic() {
                name.push(c);
                in_junk = false;
            } else if !in_junk {
                name.push('_');
                in_junk = true;
            }
        }
        format!("{name}.osm.pbf")
    };
    let osm_url =
        if area.eq_ignore_ascii_case("planet") { "aws:latest".to_string() } else { format!("geofabrik:{area}") };

    OpenMapTilesSetup {
        default_languages: open_map_tiles_schema::LANGUAGES.to_vec(),
        wikidata_names_path: sources_dir.join("wikidata_names.json"),
        sources: vec![
            SourceSpec {
                kind: SourceKind::Shapefile { projection: Some("EPSG:3857".into()) },
                name: LAKE_CENTERLINE_SOURCE,
                default_path: sources_dir.join("lake_centerline.shp.zip"),
                // upstream is at https://github.com/acalcutt/osm-lakelines/releases/download/latest/lake_centerline.shp.zip ,
                // following is same URL as used in the OpenMapTiles (but SHP format), a mirror maintained by MapTiler
                default_url: "https://dev.maptiler.download/geodata/omt/lake_centerline.shp.zip".into(),
            },
            SourceSpec {
                kind: SourceKind::Shapefile { projection: None },
                name: WATER_POLYGON_SOURCE,
                default_path: sources_dir.join("water-polygons-split-3857.zip"),
                default_url: "https://osmdata.openstreetmap.de/download/water-polygons-split-3857.zip".into(),
            },
            SourceSpec {
                kind: SourceKind::NaturalEarth,
                name: NATURAL_EARTH_SOURCE,
                default_path: sources_dir.join("natural_earth_vector.sqlite.zip"),
                // upstream is at https://naciscdn.org/naturalearth/packages/natural_earth_vector.sqlite.zip ,
                // following is same URL as used in the OpenMapTiles, a mirror maintained by MapTiler
                default_url: "https://dev.maptiler.download/geodata/omt/natural_earth_vector.sqlite.zip".into(),
            },
            SourceSpec {
                kind: SourceKind::Osm,
                name: OSM_SOURCE,
                default_path: sources_dir.join(osm_file_name),
                default_url: osm_url,
            },
        ],
        // override with --mbtiles=... argument or MBTILES=... env var or mbtiles=... in a config file
        output_argument: "mbtiles",
        default_output: data_dir.join("output.mbtiles"),
    }
}

/// Creates the profile (Java defers this with `setProfile(OpenMapTilesProfile::new)` because it depends on data from
/// the runner).
pub fn create_profile(
    translations: Arc<Translations>,
    config: Arc<PlanetilerConfig>,
    stats: StatsRef,
) -> OpenMapTilesProfile {
    OpenMapTilesProfile::new(translations, config, stats)
}

/// Configures a [`Planetiler`] runner like `OpenMapTilesMain.run` without running it (useful for tests and the CLI).
pub fn configure(arguments: &Arguments) -> anyhow::Result<Planetiler> {
    let setup = setup(arguments);
    let mut planetiler = Planetiler::create(arguments.clone())?;
    planetiler
        .set_default_languages(&setup.default_languages)
        .fetch_wikidata_name_translations(&setup.wikidata_names_path)?
        // defer creation of the profile because it depends on data from the runner
        .set_profile_provider(|p| {
            let translations = p.translations();
            Ok(Arc::new(create_profile(translations, p.config().clone(), p.stats().clone())))
        });
    // override any of these with arguments: --osm_path=... or --osm_url=...
    // or OSM_PATH=... OSM_URL=... environmental argument
    // or osm_path=... osm_url=... in a config file
    for source in &setup.sources {
        let url = Some(source.default_url.as_str());
        match &source.kind {
            SourceKind::Shapefile { projection } => {
                planetiler.add_shapefile_source_with_projection(
                    projection.as_deref(),
                    source.name,
                    &source.default_path,
                    url,
                )?;
            }
            SourceKind::NaturalEarth => {
                planetiler.add_natural_earth_source(source.name, &source.default_path, url)?;
            }
            SourceKind::Osm => {
                planetiler.add_osm_source_with_url(source.name, &source.default_path, url)?;
            }
        }
    }
    // override with --mbtiles=... argument or MBTILES=... env var or mbtiles=... in a config file
    planetiler.set_output_with_argument(setup.output_argument, &setup.default_output)?;
    Ok(planetiler)
}

/// `OpenMapTilesMain.run`.
pub fn run(arguments: &Arguments) -> anyhow::Result<()> {
    configure(arguments)?.run()?;
    tracing::info!("{ACKNOWLEDGMENTS}");
    Ok(())
}

/// `OpenMapTilesMain.main`.
pub fn main<S: AsRef<str>>(args: &[S]) -> anyhow::Result<()> {
    run(&Arguments::from_args_or_config_file(args)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_sources() {
        let setup = setup(&Arguments::of());
        let names: Vec<_> = setup.sources.iter().map(|s| s.name).collect();
        assert_eq!(vec!["lake_centerlines", "water_polygons", "natural_earth", "osm"], names);
        assert_eq!(Path::new("data/sources/monaco.osm.pbf"), setup.sources[3].default_path);
        assert_eq!("geofabrik:monaco", setup.sources[3].default_url);
    }

    #[test]
    fn test_area_file_name() {
        let setup = setup(&Arguments::of_pairs([("area", "rhode island")]));
        assert_eq!(Path::new("data/sources/rhode_island.osm.pbf"), setup.sources[3].default_path);
        assert_eq!("geofabrik:rhode island", setup.sources[3].default_url);
        let planet = super::setup(&Arguments::of_pairs([("area", "planet")]));
        assert_eq!("aws:latest", planet.sources[3].default_url);
    }
}
