//! Port of `Planetiler.java`: high-level API for creating a new map that ties together lower-level utilities in a way
//! that is suitable for the most common use-cases.
//!
//! ```no_run
//! use planetiler_core::config::arguments::Arguments;
//! use planetiler_core::planetiler::Planetiler;
//! use planetiler_core::profile::NullProfile;
//!
//! # fn main() -> anyhow::Result<()> {
//! Planetiler::create(Arguments::from_env_or_args(&std::env::args().skip(1).collect::<Vec<_>>()))?
//!     .set_profile(NullProfile)
//!     .add_osm_source("osm", "source.osm.pbf")?
//!     .set_output("output.mbtiles")?
//!     .run()
//! # }
//! ```
//!
//! Each builder method mutates the runner and returns it for more chaining (methods that read arguments or may fail
//! return `anyhow::Result<&mut Self>`).
//!
//! Differences from Java:
//! - `run()` returns an error instead of throwing, and `--help` / `--version` return `Ok(())` instead of calling
//!   `System.exit(0)`; `--tests=<file>` returns an error when a test case fails (Java exits with status 1).
//! - Stage tasks get a [`StageContext`] with the feature group, profile, config, stats and archive metadata instead of
//!   capturing the runner.
//! - `PlanetilerConfig` is shared as an `Arc`. When the map bounds are inferred from the OSM input (header or node
//!   scan), the runner swaps in a new config with those bounds; a profile constructed *before* that (profiles are
//!   built at the start of `run()` like Java) keeps the `Arc` it was given without the inferred bounds. Everything the
//!   runner drives (readers, renderer, archive writer, metadata) uses the final config.
//! - Wikidata translations loaded during `run()` are attached through a deferred provider that
//!   [`Planetiler::translations`] registers up front, since `Translations` is shared immutably (`Arc`) once a profile
//!   holds it.
//! - `free_<name>_after_read` inputs are deleted after reading like Java, but are not subtracted from the write-phase
//!   disk estimate (resource amounts are unsigned).

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};

use crate::archive::tile_archive_config::TileArchiveConfig;
use crate::archive::tile_archive_metadata::TileArchiveMetadata;
use crate::archive::tile_archive_writer;
use crate::archive::tile_archives;
use crate::collection::feature_group::FeatureGroup;
use crate::collection::{long_long_map, long_long_multimap};
use crate::config::arguments::Arguments;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::profile::Profile;
use crate::reader::osm::osm_input_file::OsmInputFile;
use crate::reader::osm::osm_node_bounds_provider::OsmNodeBoundsProvider;
use crate::reader::osm::osm_reader::OsmReader;
use crate::reader::parquet::parquet_input_file::IdGenerator;
use crate::reader::parquet::parquet_reader::{LayerGenerator, ParquetReader};
use crate::stats::process_info;
use crate::stats::stats::StatsRef;
use crate::stats::timers::Finishable;
use crate::util::build_info::BuildInfo;
use crate::util::downloader::Downloader;
use crate::util::file_utils::{self, DiskBacked};
use crate::util::format;
use crate::util::overture_stac::OvertureStac;
use crate::util::resource_usage::ResourceUsage;
use crate::util::translations::{TranslationProvider, Translations};
use crate::util::wikidata::{self, WikidataTranslations};
use crate::util::{ansi_colors, tile_size_stats, top_osm_tiles};
use crate::validator::java_profile_validator;
use crate::value::{Tags, Value};

/// The most common OSM languages.
pub const DEFAULT_LANGUAGES: &[&str] =
    &["en", "ru", "ar", "zh", "ja", "ko", "fr", "de", "fi", "pl", "es", "be", "br", "he"];

/// Everything a stage task has access to while it runs.
pub struct StageContext<'a> {
    /// ID of the stage being run.
    pub name: &'a str,
    pub feature_group: &'a Arc<FeatureGroup>,
    pub profile: &'a Arc<dyn Profile>,
    pub config: &'a Arc<PlanetilerConfig>,
    pub stats: &'a StatsRef,
    /// Metadata that will be written to the output archive (stages can add extra metadata entries).
    pub metadata: &'a mut TileArchiveMetadata,
    pub tmp_dir: &'a Path,
}

/// A task that runs during [`Planetiler::run`] (Java `RunnableThatThrows`).
pub type StageTask = Box<dyn FnOnce(&mut StageContext<'_>) -> anyhow::Result<()>>;

/// Builds the profile once the runner is configured (Java `setProfile(Function<Planetiler, Profile>)`).
pub type ProfileProvider = Box<dyn FnOnce(&mut Planetiler) -> anyhow::Result<Arc<dyn Profile>>>;

struct Stage {
    id: String,
    details: Vec<String>,
    task: StageTask,
}

enum DownloadUrls {
    Ready(Vec<(String, PathBuf)>),
    Pending(JoinHandle<anyhow::Result<Vec<(String, PathBuf)>>>),
}

struct ToDownload {
    id: String,
    urls: DownloadUrls,
}

struct InputPath {
    id: String,
    path: PathBuf,
    free_after_reading: bool,
}

/// A translation provider that can be filled in after [`Translations`] is shared (used for wikidata translations
/// that are loaded during [`Planetiler::run`]).
#[derive(Default)]
struct DeferredTranslationProvider(OnceLock<Arc<dyn TranslationProvider>>);

impl TranslationProvider for DeferredTranslationProvider {
    fn get_name_translations(&self, tags: &Tags) -> Option<Cow<'_, BTreeMap<String, String>>> {
        self.0.get().and_then(|p| p.get_name_translations(tags))
    }
}

/// Error wrapper matching Java's `PlanetilerException` message for failures during a stage.
fn stage_error(id: &str, e: anyhow::Error) -> anyhow::Error {
    e.context(format!("Error occurred during stage {id}"))
}

/// High-level runner that reads sources, renders features with a profile and writes an output archive.
pub struct Planetiler {
    stages: Vec<Stage>,
    to_download: Vec<ToDownload>,
    input_paths: Vec<InputPath>,
    overall_timer: Option<Finishable>,
    arguments: Arguments,
    stats: StatsRef,
    tmp_dir: PathBuf,
    node_db_path: PathBuf,
    multipolygon_path: PathBuf,
    feature_db_path: PathBuf,
    only_run_tests: Option<PathBuf>,
    download_sources: bool,
    refresh_sources: bool,
    only_download_sources: bool,
    parse_node_bounds: bool,
    profile: Option<Arc<dyn Profile>>,
    profile_provider: Option<ProfileProvider>,
    config: Arc<PlanetilerConfig>,
    osm_input_file: Option<OsmInputFile>,
    output: Option<TileArchiveConfig>,
    overwrite: bool,
    ran: bool,
    default_languages: Vec<String>,
    translations: Option<Arc<Translations>>,
    wikidata_provider: Arc<DeferredTranslationProvider>,
    wikidata_names_file: Option<PathBuf>,
    use_wikidata: bool,
    only_fetch_wikidata: bool,
    fetch_wikidata: bool,
    wikidata_max_age: Duration,
    wikidata_update_limit: usize,
    fetch_osm_tile_stats: bool,
}

impl Planetiler {
    /// Returns a new empty runner that will get configuration from `arguments`.
    pub fn create(arguments: Arguments) -> anyhow::Result<Planetiler> {
        let stats = arguments.get_stats()?;
        let overall_timer = stats.start_stage_quietly("overall");
        let config = PlanetilerConfig::from(&arguments)?;
        if let Some(color) = config.color {
            ansi_colors::set_use_colors(color);
        }
        let tmp_dir = config.tmp_dir.clone();
        let only_download_sources = arguments.get_boolean("only_download", "download source data then exit", false);
        let only_run_tests = arguments.file_opt("tests", "run test cases in a yaml then quit");
        let download_sources = only_download_sources || arguments.get_boolean("download", "download sources", false);
        let refresh_sources = arguments.get_boolean(
            "refresh_sources",
            "download new version of source files if they have changed",
            false,
        );
        let fetch_osm_tile_stats =
            arguments.get_boolean("download_osm_tile_weights", "download OSM tile weights file", download_sources);
        let node_db_path = arguments.file("temp_nodes", "temp node db location", tmp_dir.join("node.db"));
        let multipolygon_path =
            arguments.file("temp_multipolygons", "temp multipolygon db location", tmp_dir.join("multipolygon.db"));
        let feature_db_path = arguments.file("temp_features", "temp feature db location", tmp_dir.join("feature.db"));
        let parse_node_bounds =
            arguments.get_boolean("osm_parse_node_bounds", "parse bounds from OSM nodes instead of header", false);
        Ok(Planetiler {
            stages: vec![],
            to_download: vec![],
            input_paths: vec![],
            overall_timer: Some(overall_timer),
            arguments,
            stats,
            tmp_dir,
            node_db_path,
            multipolygon_path,
            feature_db_path,
            only_run_tests,
            download_sources,
            refresh_sources,
            only_download_sources,
            parse_node_bounds,
            profile: None,
            profile_provider: None,
            config: Arc::new(config),
            osm_input_file: None,
            output: None,
            overwrite: false,
            ran: false,
            default_languages: DEFAULT_LANGUAGES.iter().map(|s| s.to_string()).collect(),
            translations: None,
            wikidata_provider: Arc::default(),
            wikidata_names_file: None,
            use_wikidata: false,
            only_fetch_wikidata: false,
            fetch_wikidata: false,
            wikidata_max_age: Duration::ZERO,
            wikidata_update_limit: 0,
            fetch_osm_tile_stats,
        })
    }

    /// Adds a new `.osm.pbf` source that will be processed when [`Planetiler::run`] is called.
    ///
    /// To override the location of the `.osm.pbf` file, set `name_path=newpath.osm.pbf` in the arguments.
    pub fn add_osm_source(&mut self, name: &str, default_path: impl Into<PathBuf>) -> anyhow::Result<&mut Self> {
        self.add_osm_source_with_url(name, default_path, None)
    }

    /// Adds a new `.osm.pbf` source that will be processed when [`Planetiler::run`] is called.
    ///
    /// If the file does not exist and the `download=true` argument is set, then the file will first be downloaded
    /// from `default_url` (overridden by `name_url`). As a shortcut, `default_url` can be `geofabrik:monaco` to find an
    /// extract by name from the [Geofabrik download site](https://download.geofabrik.de/) or `aws:latest` to download
    /// the latest `planet.osm.pbf` from the [AWS Open Data Registry](https://registry.opendata.aws/osm/).
    pub fn add_osm_source_with_url(
        &mut self,
        name: &str,
        default_path: impl Into<PathBuf>,
        default_url: Option<&str>,
    ) -> anyhow::Result<&mut Self> {
        if self.osm_input_file.is_some() {
            // TODO: support more than one input OSM file
            bail!("Currently only one OSM input file is supported");
        }
        let path = self.get_path(name, "OSM input file", default_path.into(), default_url);
        let input_file = OsmInputFile::new(&path, self.config.osm_lazy_reads);
        self.osm_input_file = Some(input_file.clone());
        let node_db_path = self.node_db_path.clone();
        let multipolygon_path = self.multipolygon_path.clone();
        let source_name = name.to_string();
        let task = self.if_source_used(
            name,
            Box::new(move |ctx: &mut StageContext<'_>| {
                let header = input_file.get_header()?;
                ctx.metadata
                    .set_extra_metadata(
                        format!("planetiler:{source_name}:osmosisreplicationtime"),
                        header.instant.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                    )
                    .set_extra_metadata(
                        format!("planetiler:{source_name}:osmosisreplicationseq"),
                        header.osmosis_replication_sequence_number,
                    )
                    .set_extra_metadata(
                        format!("planetiler:{source_name}:osmosisreplicationurl"),
                        header.osmosis_replication_base_url,
                    );
                let config = ctx.config;
                let result = (|| -> anyhow::Result<()> {
                    let node_locations = long_long_map::from_config(
                        &config.node_map_type,
                        &config.node_map_storage,
                        &node_db_path,
                        config.node_map_madvise,
                    )?;
                    let multipolygons = long_long_multimap::new_replaceable_multimap_from_config(
                        &config.multipolygon_geometry_storage,
                        &multipolygon_path,
                        config.multipolygon_geometry_madvise,
                    )?;
                    let reader = OsmReader::new(
                        &source_name,
                        input_file.get()?,
                        Arc::from(node_locations),
                        Box::new(multipolygons),
                        ctx.profile.clone(),
                        ctx.stats.clone(),
                    );
                    reader.pass1(config)?;
                    reader.pass2(ctx.feature_group, config)?;
                    reader.close();
                    Ok(())
                })();
                file_utils::delete(&[&node_db_path, &multipolygon_path]);
                result
            }),
        );
        self.append_stage(Stage {
            id: name.to_string(),
            details: vec![
                format!("{name}_pass1: Pre-process OpenStreetMap input (store node locations then relation members)"),
                format!("{name}_pass2: Process OpenStreetMap nodes, ways, then relations"),
            ],
            task,
        })
    }

    /// Adds a new ESRI shapefile source that will be processed using a projection inferred from the shapefile when
    /// [`Planetiler::run`] is called.
    ///
    /// `default_path` can be a `.shp` file with the other shapefile components in the same directory, a directory, or
    /// a `.zip` file containing the shapefile components. Override with the `name_path` argument.
    pub fn add_shapefile_source(&mut self, name: &str, default_path: impl Into<PathBuf>) -> anyhow::Result<&mut Self> {
        self.add_shapefile_source_with_projection(None, name, default_path, None)
    }

    /// Like [`Planetiler::add_shapefile_source`] but downloads from `default_url` (overridden by `name_url`) if the
    /// file does not exist and `--download` is set.
    pub fn add_shapefile_source_with_url(
        &mut self,
        name: &str,
        default_path: impl Into<PathBuf>,
        default_url: Option<&str>,
    ) -> anyhow::Result<&mut Self> {
        self.add_shapefile_source_with_projection(None, name, default_path, default_url)
    }

    /// Adds a new ESRI shapefile source with an explicit `projection` (`None` to infer it from the `.prj` file). The
    /// projection can be any EPSG code (`EPSG:2056`, `urn:ogc:def:crs:EPSG::2056`, optionally with a `:lon_first` /
    /// `:lat_first` suffix), a PROJ string or WKT, see [`crate::geo::crs`].
    pub fn add_shapefile_source_with_projection(
        &mut self,
        projection: Option<&str>,
        name: &str,
        default_path: impl Into<PathBuf>,
        default_url: Option<&str>,
    ) -> anyhow::Result<&mut Self> {
        let path = self.get_path(name, "shapefile", default_path.into(), default_url);
        let projection = projection.map(str::to_string);
        let description = format!("Process features in {}", path.display());
        self.add_source_stage(name, &description, move |ctx| {
            let source_paths = if file_utils::has_extension(&path, "zip") || path.is_dir() {
                file_utils::walk_path_with_pattern(&path, "*.shp")?
            } else {
                vec![path.clone()]
            };
            simple_sources::process_shapefiles(projection.as_deref(), &source_paths, ctx)
        })
    }

    /// Adds a new ESRI shapefile glob source that will process all files under `base_path` (a directory or zip
    /// archive) matching `glob_pattern`. Zip files that match are searched for `*.shp` files.
    pub fn add_shapefile_glob_source(
        &mut self,
        projection: Option<&str>,
        name: &str,
        base_path: impl Into<PathBuf>,
        glob_pattern: &str,
        default_url: Option<&str>,
    ) -> anyhow::Result<&mut Self> {
        let base_path = base_path.into();
        let dir_path = self.get_path(name, "shapefile glob", base_path.clone(), default_url);
        let projection = projection.map(str::to_string);
        let pattern = glob_pattern.to_string();
        let description = format!("Process all files matching {}/{glob_pattern}", dir_path.display());
        self.add_source_stage(name, &description, move |ctx| {
            // like Java, walk the base path (not the `name_path` override)
            let source_paths = file_utils::walk_path_with_pattern_and_zip(&base_path, &pattern, |zip| {
                file_utils::walk_path_with_pattern(zip, "*.shp").unwrap_or_default()
            })?;
            simple_sources::process_shapefiles(projection.as_deref(), &source_paths, ctx)
        })
    }

    /// Adds a new OGC GeoPackage source that will be processed when [`Planetiler::run`] is called.
    ///
    /// If given a path to a ZIP file containing one or more GeoPackages, each `.gpkg` file within will be extracted to
    /// a temporary directory at runtime (or next to the zip with `<name>_keep_unzipped`). `projection` overrides the
    /// reference system of the feature tables (same syntax as
    /// [`Planetiler::add_shapefile_source_with_projection`]).
    pub fn add_geopackage_source(
        &mut self,
        projection: Option<&str>,
        name: &str,
        default_path: impl Into<PathBuf>,
        default_url: Option<&str>,
    ) -> anyhow::Result<&mut Self> {
        let path = self.get_path(name, "geopackage", default_path.into(), default_url);
        let keep_unzipped = self.get_keep_unzipped(name);
        let projection = projection.map(str::to_string);
        let description = format!("Process features in {}", path.display());
        self.add_source_stage(name, &description, move |ctx| {
            let source_paths = if file_utils::has_extension(&path, "zip") {
                file_utils::walk_path_with_pattern(&path, "*.gpkg")?
            } else {
                vec![path.clone()]
            };
            if source_paths.is_empty() {
                bail!("No .gpkg files found in {}", path.display());
            }
            let unzip_dir = if keep_unzipped { unzipped_sibling(&path) } else { ctx.tmp_dir.to_path_buf() };
            simple_sources::process_geopackages(projection.as_deref(), &source_paths, &unzip_dir, keep_unzipped, ctx)
        })
    }

    /// Adds a new GeoJSON or newline-delimited GeoJSON source that will be processed when [`Planetiler::run`] is
    /// called.
    pub fn add_geojson_source(
        &mut self,
        name: &str,
        default_path: impl Into<PathBuf>,
        default_url: Option<&str>,
    ) -> anyhow::Result<&mut Self> {
        let path = self.get_path(name, "geojson", default_path.into(), default_url);
        let description = format!("Process features in {}", path.display());
        self.add_source_stage(name, &description, move |ctx| simple_sources::process_geojson(&[path], ctx))
    }

    /// Adds a new Natural Earth sqlite file source (the `.sqlite` file or a `.zip` containing it) that will be
    /// processed when [`Planetiler::run`] is called. Deprecated in Java in favor of
    /// [`Planetiler::add_geopackage_source`].
    pub fn add_natural_earth_source(
        &mut self,
        name: &str,
        default_path: impl Into<PathBuf>,
        default_url: Option<&str>,
    ) -> anyhow::Result<&mut Self> {
        let path = self.get_path(name, "sqlite db", default_path.into(), default_url);
        let keep_unzipped = self.get_keep_unzipped(name);
        let description = format!("Process features in {}", path.display());
        self.add_source_stage(name, &description, move |ctx| {
            let unzip_dir = if keep_unzipped { unzipped_sibling(&path) } else { ctx.tmp_dir.to_path_buf() };
            simple_sources::process_natural_earth(&path, &unzip_dir, keep_unzipped, ctx)
        })
    }

    /// Adds a new [geoparquet](https://github.com/opengeospatial/geoparquet) source that will be processed when
    /// [`Planetiler::run`] is called.
    ///
    /// - `hive_partitioning`: parse extra feature tags from the file path, for example `{theme="buildings",
    ///   type="part"}` from `base/theme=buildings/type=part/file.parquet`
    /// - `get_id`: extracts a unique vector tile feature ID from each input feature (strings/binary are hashed)
    /// - `get_layer`: extracts the source layer from the properties of each input feature
    pub fn add_parquet_source(
        &mut self,
        name: &str,
        paths: Vec<PathBuf>,
        hive_partitioning: bool,
        get_id: Option<IdGenerator>,
        get_layer: Option<LayerGenerator>,
    ) -> anyhow::Result<&mut Self> {
        let resolved = paths.clone();
        self.add_parquet_source_lazy(name, paths, move || Ok(resolved), hive_partitioning, get_id, get_layer)
    }

    fn add_parquet_source_lazy(
        &mut self,
        name: &str,
        base: Vec<PathBuf>,
        paths: impl FnOnce() -> anyhow::Result<Vec<PathBuf>> + 'static,
        hive_partitioning: bool,
        get_id: Option<IdGenerator>,
        get_layer: Option<LayerGenerator>,
    ) -> anyhow::Result<&mut Self> {
        let free_after_reading = self.arguments.get_boolean(
            &format!("free_{name}_after_read"),
            &format!("delete {name} input file after reading to make space for output (reduces peak disk usage)"),
            false,
        );
        let prefix = common_parent_prefix(&base);
        for path in base {
            self.input_paths.push(InputPath { id: name.to_string(), path, free_after_reading });
        }
        let source_name = name.to_string();
        self.add_source_stage(name, &format!("Process features in {prefix}"), move |ctx| {
            let paths = paths()?;
            ParquetReader::with_options(&source_name, ctx.stats.clone(), get_id, get_layer, hive_partitioning).process(
                &paths,
                ctx.feature_group,
                ctx.config,
                ctx.profile,
            )
        })
    }

    /// Adds a new [Overture Maps](https://overturemaps.org/) source that reads from a mirror of overture data on disk
    /// under `root_path/theme=<theme>/type=<type>/*.parquet` (override with `<name>_path`), downloading the files
    /// from the STAC catalog first when `--download` or `--refresh_<name>` is set.
    pub fn add_overture_source(
        &mut self,
        name: &str,
        theme: &str,
        type_: &str,
        root_path: impl Into<PathBuf>,
    ) -> anyhow::Result<&mut Self> {
        let download_dir =
            self.arguments.file(&format!("{name}_path"), &format!("{name} overture download directory"), root_path);
        let refresh = self.arguments.get_boolean(
            &format!("refresh_{name}"),
            &format!("Download new version of {name} if changed"),
            self.refresh_sources,
        );
        let type_dir = download_dir.join(format!("theme={theme}")).join(format!("type={type_}"));
        if self.download_sources || refresh {
            // Kick off catalog traversal in the background; download() will wait for the result before running.
            file_utils::create_directory(&type_dir)?;
            let stac = OvertureStac::create(&self.config);
            let (theme, type_, dir, bounds) =
                (theme.to_string(), type_.to_string(), type_dir.clone(), self.config.bounds.clone());
            self.add_lazy_download(name, move || {
                let urls = stac.get_parquet_urls(&theme, &type_, &bounds)?;
                Ok(urls
                    .into_iter()
                    .map(|url| {
                        let file = url.rsplit('/').next().unwrap_or(&url).to_string();
                        (url, dir.join(file))
                    })
                    .filter(|(_, path)| refresh || !path.exists())
                    .collect())
            })?;
        }
        let find_dir = type_dir.clone();
        let id: IdGenerator = Arc::new(|tags: &Tags| tags.get("id").cloned().unwrap_or(Value::Null));
        let layer: LayerGenerator = Arc::new(|tags: &Tags| tags.get("type").cloned().unwrap_or(Value::Null));
        self.add_parquet_source_lazy(
            name,
            vec![type_dir],
            move || {
                if !find_dir.is_dir() {
                    return Ok(vec![]);
                }
                file_utils::walk_path_with_pattern(&find_dir, "*.parquet")
            },
            true,
            Some(id),
            Some(layer),
        )
    }

    /// Adds a new stage that will be invoked when [`Planetiler::run`] is called.
    ///
    /// - `name`: string to use in stats and logs to identify this stage
    /// - `description`: details to print when logging what stages will run
    pub fn add_stage(
        &mut self,
        name: &str,
        description: &str,
        task: impl FnOnce(&mut StageContext<'_>) -> anyhow::Result<()> + 'static,
    ) -> anyhow::Result<&mut Self> {
        self.append_stage(Stage {
            id: name.to_string(),
            details: vec![format!("{name}: {description}")],
            task: Box::new(task),
        })
    }

    /// Adds a stage like [`Planetiler::add_stage`] that is skipped when the profile does not care about source
    /// `name` ([`Profile::cares_about_source`]).
    pub fn add_source_stage(
        &mut self,
        name: &str,
        description: &str,
        task: impl FnOnce(&mut StageContext<'_>) -> anyhow::Result<()> + 'static,
    ) -> anyhow::Result<&mut Self> {
        let task = self.if_source_used(name, Box::new(task));
        self.append_stage(Stage { id: name.to_string(), details: vec![format!("{name}: {description}")], task })
    }

    /// Sets the default languages that will be used by [`Planetiler::translations`] when not overridden by the
    /// `languages` argument.
    pub fn set_default_languages<S: AsRef<str>>(&mut self, languages: &[S]) -> &mut Self {
        self.default_languages = languages.iter().map(|s| s.as_ref().to_string()).collect();
        self
    }

    /// Updates [`Planetiler::translations`] to use name translations fetched from wikidata based on the
    /// [wikidata tag](https://www.wikidata.org/wiki/Wikidata:OpenStreetMap) on OSM elements.
    ///
    /// When either `only_fetch_wikidata` or `fetch_wikidata` arguments are set to true, this downloads translations for
    /// every OSM element that the profile cares about and stores them to `default_wikidata_cache` (or the value of the
    /// `wikidata_cache` argument) before processing any sources.
    ///
    /// As long as `use_wikidata` is not set to false, then previously-downloaded wikidata translations will be loaded
    /// from the cache file, so you can run with `fetch_wikidata=true` once, then without it each subsequent run to only
    /// download translations once.
    pub fn fetch_wikidata_name_translations(
        &mut self,
        default_wikidata_cache: impl Into<PathBuf>,
    ) -> anyhow::Result<&mut Self> {
        let args = &self.arguments;
        self.only_fetch_wikidata =
            args.get_boolean("only_fetch_wikidata", "fetch wikidata translations then quit", self.only_fetch_wikidata);
        self.fetch_wikidata = self.only_fetch_wikidata
            || args.get_boolean("fetch_wikidata", "fetch wikidata translations then continue", self.fetch_wikidata);
        self.use_wikidata = self.fetch_wikidata || args.get_boolean("use_wikidata", "use wikidata translations", true);
        self.wikidata_names_file = Some(args.file("wikidata_cache", "wikidata cache file", default_wikidata_cache));
        self.wikidata_max_age = args.get_duration(
            "wikidata_max_age",
            "Maximum age of Wikidata translations (in ISO-8601 duration format PnDTnHnMn.nS; 0S = disabled)",
            "0s",
        )?;
        self.wikidata_update_limit = args
            .get_integer(
                "wikidata_update_limit",
                "Limit on how many old translations to update during one download (0 = disabled)",
                0,
            )?
            .max(0) as usize;
        Ok(self)
    }

    /// Returns the translations to use for names, configured from the `languages` and `transliterate` arguments
    /// (and the profile's default languages).
    pub fn translations(&mut self) -> Arc<Translations> {
        if let Some(t) = &self.translations {
            return t.clone();
        }
        let transliterate = self.arguments.get_boolean("transliterate", "attempt to transliterate latin names", false);
        let mut languages = self.arguments.get_list(
            "languages",
            "Languages to include labels for. \"default\" expands to the default set of languages configured by the profile. \"-lang\" excludes \"lang\". \"*\" includes every language not listed.",
            &self.default_languages,
        );
        if languages.iter().any(|l| l == "default") {
            languages.retain(|l| l != "default");
            languages.extend(self.default_languages.iter().cloned());
        }
        let mut translations = Translations::default_provider(&languages);
        translations
            .set_should_transliterate(transliterate)
            .set_extra_name_tags(&self.config.extra_name_tags)
            .add_fallback_translation_provider_arc(self.wikidata_provider.clone());
        let translations = Arc::new(translations);
        self.translations = Some(translations.clone());
        translations
    }

    fn append_stage(&mut self, stage: Stage) -> anyhow::Result<&mut Self> {
        if self.stages.iter().any(|other| other.id == stage.id) {
            bail!("Duplicate stage name: {}", stage.id);
        }
        self.stages.push(stage);
        Ok(self)
    }

    /// Returns whether to keep unzipped copies of source `name` (`<name>_keep_unzipped` argument).
    pub fn get_keep_unzipped(&self, name: &str) -> bool {
        self.arguments.get_boolean(
            &format!("{name}_keep_unzipped"),
            &format!("keep unzipped {name} after reading"),
            self.config.keep_unzipped_sources,
        )
    }

    /// Sets the profile implementation that controls how source features map to output map elements.
    pub fn set_profile(&mut self, profile: impl Profile + 'static) -> &mut Self {
        self.profile = Some(Arc::new(profile));
        self
    }

    /// Sets an already-shared profile.
    pub fn set_profile_arc(&mut self, profile: Arc<dyn Profile>) -> &mut Self {
        self.profile = Some(profile);
        self
    }

    /// Sets a profile that needs information from this runner (translations, config, stats) to be instantiated.
    ///
    /// Construction is deferred until [`Planetiler::profile`] or [`Planetiler::run`] is called.
    pub fn set_profile_provider(
        &mut self,
        provider: impl FnOnce(&mut Planetiler) -> anyhow::Result<Arc<dyn Profile>> + 'static,
    ) -> &mut Self {
        self.profile_provider = Some(Box::new(provider));
        self
    }

    /// Sets the location of the output archive to write rendered tiles to. Fails at [`Planetiler::run`] if the
    /// archive already exists (unless `--force` or `--append`).
    ///
    /// To override the location of the file, set `output=newpath` in the arguments. To set options for the output
    /// add `output.mbtiles?arg=value` or add command-line argument `mbtiles_arg=value`.
    pub fn set_output(&mut self, default_output_uri: impl AsRef<Path>) -> anyhow::Result<&mut Self> {
        let default = default_output_uri.as_ref().to_string_lossy().to_string();
        let uri = self.arguments.get_string("output", "output tile archive URI", &default);
        self.output = Some(TileArchiveConfig::from(&uri)?);
        Ok(self)
    }

    /// Legacy `setOutput(argument, fallback)`: reads the output from `output` or `argument`.
    pub fn set_output_with_argument(
        &mut self,
        argument: &str,
        fallback: impl AsRef<Path>,
    ) -> anyhow::Result<&mut Self> {
        let default = fallback.as_ref().to_string_lossy().to_string();
        let uri = self.arguments.get_string(&format!("output|{argument}"), "output tile archive path", &default);
        self.output = Some(TileArchiveConfig::from(&uri)?);
        Ok(self)
    }

    /// Like [`Planetiler::set_output`] but overwrites the archive if it already exists.
    pub fn overwrite_output(&mut self, default_output_uri: impl AsRef<Path>) -> anyhow::Result<&mut Self> {
        self.overwrite = true;
        self.set_output(default_output_uri)
    }

    /// Like [`Planetiler::set_output_with_argument`] but overwrites the archive if it already exists.
    pub fn overwrite_output_with_argument(
        &mut self,
        argument: &str,
        fallback: impl AsRef<Path>,
    ) -> anyhow::Result<&mut Self> {
        self.overwrite = true;
        self.set_output_with_argument(argument, fallback)
    }

    pub fn arguments(&self) -> &Arguments {
        &self.arguments
    }

    pub fn osm_input_file(&self) -> Option<&OsmInputFile> {
        self.osm_input_file.as_ref()
    }

    pub fn config(&self) -> &Arc<PlanetilerConfig> {
        &self.config
    }

    pub fn stats(&self) -> &StatsRef {
        &self.stats
    }

    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    /// The output archive, if set.
    pub fn output(&self) -> Option<&TileArchiveConfig> {
        self.output.as_ref()
    }

    /// Returns the profile, constructing it from the provider set with [`Planetiler::set_profile_provider`] if needed.
    pub fn profile(&mut self) -> anyhow::Result<Option<Arc<dyn Profile>>> {
        if self.profile.is_none()
            && let Some(provider) = self.profile_provider.take()
        {
            self.profile = Some(provider(self)?);
        }
        Ok(self.profile.clone())
    }

    fn if_source_used(&self, name: &str, task: StageTask) -> StageTask {
        let name = name.to_string();
        Box::new(move |ctx: &mut StageContext<'_>| {
            if ctx.profile.cares_about_source(&name) {
                task(ctx)
            } else {
                crate::util::log_util::set_stage(&name);
                tracing::info!("Skipping since profile does not use it");
                crate::util::log_util::clear_stage();
                Ok(())
            }
        })
    }

    /// Resolves the input path for source `name` (`<name>_path` argument), queues a download from `<name>_url` (or
    /// `default_url`) if `--download` or `--refresh_<name>` is set and the file is missing (or refreshing), and
    /// registers it as an input that must exist (Java `getPath`).
    pub fn get_path(&mut self, name: &str, type_: &str, default_path: PathBuf, default_url: Option<&str>) -> PathBuf {
        let args = &self.arguments;
        let path = args.file(&format!("{name}_path"), &format!("{name} {type_} path"), default_path);
        let refresh = args.get_boolean(
            &format!("refresh_{name}"),
            &format!("Download new version of {name} if changed"),
            self.refresh_sources,
        );
        let free_after_reading = args.get_boolean(
            &format!("free_{name}_after_read"),
            &format!("delete {name} input file after reading to make space for output (reduces peak disk usage)"),
            false,
        );
        if self.download_sources || refresh {
            let url_key = format!("{name}_url");
            let url_description = format!("{name} {type_} url");
            let url = match default_url {
                Some(default) => Some(args.get_string(&url_key, &url_description, default)),
                None => args.get_string_opt(&url_key, &url_description),
            };
            if let Some(url) = url
                && (!path.exists() || refresh)
            {
                self.to_download
                    .push(ToDownload { id: name.to_string(), urls: DownloadUrls::Ready(vec![(url, path.clone())]) });
            }
        }
        self.input_paths.push(InputPath { id: name.to_string(), path: path.clone(), free_after_reading });
        path
    }

    /// Registers an input path without download handling (used by sources like parquet that take explicit paths).
    pub fn add_input_path(&mut self, name: &str, path: PathBuf, free_after_reading: bool) -> &mut Self {
        self.input_paths.push(InputPath { id: name.to_string(), path, free_after_reading });
        self
    }

    /// Queues downloads that are resolved lazily on a background thread (e.g. an Overture STAC catalog traversal);
    /// [`Planetiler::run`] waits for the result before downloading.
    pub fn add_lazy_download(
        &mut self,
        name: &str,
        resolve: impl FnOnce() -> anyhow::Result<Vec<(String, PathBuf)>> + Send + 'static,
    ) -> anyhow::Result<&mut Self> {
        let handle = std::thread::Builder::new().name(format!("resolve-{name}")).spawn(resolve)?;
        self.to_download.push(ToDownload { id: name.to_string(), urls: DownloadUrls::Pending(handle) });
        Ok(self)
    }

    /// Whether `--download` (or `--only_download`) was set.
    pub fn download_sources(&self) -> bool {
        self.download_sources
    }

    /// Whether `--refresh_sources` was set.
    pub fn refresh_sources(&self) -> bool {
        self.refresh_sources
    }

    /// Reads all elements from all sources that have been added, generates map features according to the profile,
    /// and writes the rendered tiles to the output archive.
    pub fn run(&mut self) -> anyhow::Result<()> {
        let show_version = self.arguments.get_boolean("version", "show version then exit", false);
        let build_info = BuildInfo::get();
        tracing::info!("Planetiler build git hash: {}", build_info.githash.as_deref().unwrap_or("null"));
        tracing::info!("Planetiler build version: {}", build_info.version.as_deref().unwrap_or("null"));
        tracing::info!("Planetiler build timestamp: {}", build_info.build_time_string().as_deref().unwrap_or("null"));
        if show_version {
            return Ok(());
        }
        let Some(profile) = self.profile()? else { bail!("No profile specified") };
        let Some(output) = self.output.clone() else { bail!("No output specified") };
        if self.stages.is_empty() {
            bail!("No sources specified");
        }
        if self.ran {
            bail!("Can only run once");
        }
        self.ran = true;

        if self.arguments.get_boolean("help", "show arguments then exit", false) {
            return Ok(());
        } else if let Some(tests) = &self.only_run_tests {
            let success = java_profile_validator::validate(&*profile, tests, &self.config)?;
            if !success {
                bail!("Profile tests in {} failed", tests.display());
            }
            return Ok(());
        } else if self.only_download_sources {
            // don't check files if not generating map
        } else if self.config.append {
            if !output.format.supports_append() {
                bail!("cannot append to {}", output.format.id());
            }
            if !output.exists() {
                bail!("{} must exist when appending", output.uri);
            }
        } else if self.overwrite || self.config.force {
            output.delete().with_context(|| format!("deleting {}", output.uri))?;
        } else if output.exists() {
            bail!("{} already exists, use the --force argument to overwrite or --append.", output.uri);
        }

        let layer_stats_path = self.arguments.file(
            "layer_stats",
            "layer stats output path",
            // default to <output file>.layerstats.tsv.gz
            tile_size_stats::get_default_layerstats_path(
                &output.get_local_path().unwrap_or_else(|| PathBuf::from("output")),
            ),
        );

        let config = self.config.clone();
        if config.tile_write_threads < 1 {
            bail!("require tile_write_threads >= 1");
        }
        if config.tile_write_threads > 1 {
            if !output.format.supports_concurrent_writes() {
                bail!("{} doesn't support concurrent writes", output.format);
            }
            for i in 1..config.tile_write_threads {
                let p = output.get_path_for_multi_threaded_writer(i)?;
                if !config.append && (self.overwrite || config.force) {
                    file_utils::delete(&[&p]);
                }
                if config.append && !output.exists_at(Some(&p)) {
                    bail!("indexed archive \"{}\" must exist when appending", p.display());
                } else if !config.append && output.exists_at(Some(&p)) {
                    bail!("indexed archive \"{}\" must not exist when not appending", p.display());
                }
            }
        }

        // Keep reuse metadata next to feature.db so it survives directory cleanup and is easy to atomically manage.
        let feature_db_name =
            self.feature_db_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let string_encoder_path = self.feature_db_path.join(format!("{feature_db_name}.strings"));
        let chunk_manifest_path = self.feature_db_path.join(format!("{feature_db_name}.manifest"));
        let has_reusable_feature_db =
            self.feature_db_path.exists() && string_encoder_path.exists() && chunk_manifest_path.exists();
        let reuse_existing_feature_db = config.reuse_feature_db && has_reusable_feature_db;

        tracing::info!("Building {} profile into {} in these phases:", profile.name(), output.uri);
        if !self.to_download.is_empty() {
            let ids: Vec<&str> = self.to_download.iter().map(|d| d.id.as_str()).collect();
            tracing::info!("  download: Download sources {ids:?}");
        }
        if !self.only_download_sources && self.fetch_wikidata {
            tracing::info!("  wikidata: Fetch translations from wikidata query service");
        }
        if !self.only_download_sources && !self.only_fetch_wikidata {
            if reuse_existing_feature_db {
                tracing::info!(
                    "  reuse: Reusing existing feature DB at {} (skipping source reading stages)",
                    self.feature_db_path.display()
                );
            } else {
                for stage in &self.stages {
                    for details in &stage.details {
                        tracing::info!("  {details}");
                    }
                }
                tracing::info!("  sort: Sort rendered features by tile ID");
            }
            tracing::info!("  archive: Encode each tile and write to {}", output.uri);
        }

        if reuse_existing_feature_db {
            tracing::info!("Reusing existing feature DB state at {}", self.feature_db_path.display());
        } else {
            if config.reuse_feature_db {
                tracing::info!(
                    "--reuse_featuredb enabled with no previous reusable state; building feature DB for reuse"
                );
            }
            file_utils::delete(&[&self.feature_db_path]);
        }
        // in case any temp files are left from a previous run...
        file_utils::delete(&[&self.node_db_path, &self.multipolygon_path]);
        if !reuse_existing_feature_db {
            file_utils::delete(&[&self.tmp_dir]);
        }
        file_utils::create_directory(&self.tmp_dir)?;
        let mut parents = vec![self.node_db_path.clone(), self.feature_db_path.clone(), self.multipolygon_path.clone()];
        parents.extend(output.get_local_base_path());
        file_utils::create_parent_directories(&parents)?;

        if !self.to_download.is_empty() {
            self.download(&profile)?;
        }
        if self.fetch_osm_tile_stats {
            top_osm_tiles::download_precomputed(&config);
        }
        if !reuse_existing_feature_db {
            self.ensure_input_files_exist(&profile)?;
        }

        if self.fetch_wikidata {
            let infile = self.osm_input_file.as_ref().ok_or_else(|| anyhow!("no OSM input file to fetch wikidata"))?;
            let outfile = self.wikidata_names_file.as_ref().ok_or_else(|| anyhow!("no wikidata cache file"))?;
            wikidata::fetch_with_max_age(
                infile,
                outfile,
                &config,
                wikidata::profile_filter(profile.clone()),
                self.stats.clone(),
                self.wikidata_max_age,
                self.wikidata_update_limit,
            )?;
        }
        if self.use_wikidata
            && let Some(file) = &self.wikidata_names_file
        {
            // fills in the deferred provider registered by `translations()`
            let loaded: WikidataTranslations = wikidata::load(file);
            let _ = self.wikidata_provider.0.set(Arc::new(loaded));
        }
        if self.only_download_sources || self.only_fetch_wikidata {
            return Ok(()); // exit only if just fetching wikidata or downloading sources
        }

        if let Some(osm_input_file) = self.osm_input_file.clone()
            && !reuse_existing_feature_db
        {
            self.check_disk_space(&profile, &osm_input_file, &output)?;
            self.check_memory(&profile, &osm_input_file)?;
            if config.bounds.lat_lon_opt().is_none() {
                let mut new_config = (*self.config).clone();
                if !self.parse_node_bounds {
                    new_config.bounds.add_fallback_provider(&osm_input_file);
                }
                let node_bounds =
                    OsmNodeBoundsProvider::new(osm_input_file.clone(), new_config.clone(), self.stats.clone());
                new_config.bounds.add_fallback_provider(&node_bounds);
                self.config = Arc::new(new_config);
            }
        }
        let config = self.config.clone();
        // must construct this after bounds providers are added in order to infer bounds from the input source if not
        // provided
        let mut metadata = TileArchiveMetadata::from_profile(&*profile, &config);

        let mut archive = tile_archives::new_writer(&output, &config)
            .map_err(|e| e.context(format!("Unable to write to {}", output.uri)))?;
        let mut feature_group = FeatureGroup::new_disk_backed(
            archive.tile_order(),
            &self.feature_db_path,
            profile.clone(),
            config.clone(),
            self.stats.clone(),
            reuse_existing_feature_db,
        )?;
        self.stats.monitor_file("nodes", self.node_db_path.clone());
        self.stats.monitor_file("features", self.feature_db_path.clone());
        self.stats.monitor_file("multipolygons", self.multipolygon_path.clone());
        let archive_path = output.get_local_path().unwrap_or_default();
        self.stats.monitor_file("archive", archive_path.clone());

        if reuse_existing_feature_db {
            feature_group.load_string_encoders(&string_encoder_path)?;
            feature_group.init_from_manifest(&chunk_manifest_path)?;
        } else {
            let group = Arc::new(feature_group);
            for stage in std::mem::take(&mut self.stages) {
                let mut ctx = StageContext {
                    name: &stage.id,
                    feature_group: &group,
                    profile: &profile,
                    config: &config,
                    stats: &self.stats,
                    metadata: &mut metadata,
                    tmp_dir: &self.tmp_dir,
                };
                (stage.task)(&mut ctx).map_err(|e| stage_error(&stage.id, e))?;
            }
            feature_group =
                Arc::into_inner(group).ok_or_else(|| anyhow!("feature group still in use after reading sources"))?;

            tracing::info!("Deleting node.db to make room for output file");
            profile.release();
            for input_path in &self.input_paths {
                if input_path.free_after_reading {
                    tracing::info!(
                        "Deleting {} ({}) to make room for output file",
                        input_path.id,
                        input_path.path.display()
                    );
                    file_utils::delete(&[&input_path.path]);
                }
            }

            feature_group.prepare()?;
            if config.reuse_feature_db {
                feature_group.save_string_encoders(&string_encoder_path)?;
                feature_group.save_chunk_manifest(&chunk_manifest_path)?;
            }
        }

        let output_size = Arc::new(FileSize(archive_path));
        tile_archive_writer::write_output(
            &mut feature_group,
            archive.as_mut(),
            output_size,
            &metadata,
            Some(&layer_stats_path),
            &config,
            &self.stats,
        )
        .map_err(|e| e.context(format!("Unable to write to {}", output.uri)))?;
        archive.close().map_err(|e| e.context(format!("Unable to write to {}", output.uri)))?;
        drop(archive);
        drop(feature_group);

        if let Some(timer) = self.overall_timer.take() {
            timer.stop();
        }
        tracing::info!("FINISHED!");
        self.stats.print_summary();
        self.stats.close();
        Ok(())
    }

    fn check_disk_space(
        &self,
        profile: &Arc<dyn Profile>,
        osm_input_file: &OsmInputFile,
        output: &TileArchiveConfig,
    ) -> anyhow::Result<()> {
        let config = &self.config;
        let read_phase = ResourceUsage::new("read phase disk");
        let write_phase = ResourceUsage::new("write phase disk");
        let osm_size = osm_input_file.disk_usage_bytes();
        let node_map_size = OsmReader::estimate_node_location_usage_str(
            &config.node_map_type,
            &config.node_map_storage,
            osm_size,
            &self.tmp_dir,
        )?
        .disk_usage();
        let multipolygon_geometry_size = OsmReader::estimate_multipolygon_geometry_usage_str(
            &config.multipolygon_geometry_storage,
            osm_size,
            &self.tmp_dir,
        )?
        .disk_usage();
        let feature_size = profile.estimate_intermediate_disk_bytes(osm_size);
        let output_size = profile.estimate_output_bytes(osm_size);

        // node locations and multipolygon geometries only needed while reading inputs
        read_phase.add_disk(Some(&self.node_db_path), node_map_size, "temporary node location cache");
        read_phase.add_disk(
            Some(&self.multipolygon_path),
            multipolygon_geometry_size,
            "temporary multipolygon geometry cache",
        );
        // feature db persists across read/write phase
        read_phase.add_disk(Some(&self.feature_db_path), feature_size, "temporary feature storage");
        write_phase.add_disk(Some(&self.feature_db_path), feature_size, "temporary feature storage");
        // output only needed during write phase
        write_phase.add_disk(output.get_local_path().as_deref(), output_size, "archive output");
        // inputs marked free_<name>_after_read are deleted before the write phase
        for input_path in self.input_paths.iter().filter(|p| p.free_after_reading) {
            write_phase.free_disk(
                &input_path.path,
                file_utils::size_or_zero(&input_path.path),
                &format!("delete {} source after reading", input_path.id),
            );
        }

        read_phase.check_against_limits(config.force, true)?;
        write_phase.check_against_limits(config.force, true)?;
        Ok(())
    }

    fn check_memory(&self, profile: &Arc<dyn Profile>, osm_input_file: &OsmInputFile) -> anyhow::Result<()> {
        let config = &self.config;
        let check = ResourceUsage::new("read phase");
        let osm_size = osm_input_file.disk_usage_bytes();
        let node_map_usages = OsmReader::estimate_node_location_usage_str(
            &config.node_map_type,
            &config.node_map_storage,
            osm_size,
            &self.tmp_dir,
        )?;
        // Java passes nodeMapStorage here too
        let multipolygon_geometry_usages =
            OsmReader::estimate_multipolygon_geometry_usage_str(&config.node_map_storage, osm_size, &self.tmp_dir)?;
        let memory_mapped_files = node_map_usages.disk_usage() + multipolygon_geometry_usages.disk_usage();

        check
            .add_all(&node_map_usages)
            .add_all(&multipolygon_geometry_usages)
            .add_memory(profile.estimate_ram_required(osm_size), "temporary profile storage");

        check.check_against_limits(config.force, true)?;

        // check off-heap memory if we can get it
        if let Some(extra_memory) = process_info::get_system_free_memory_bytes() {
            if extra_memory < memory_mapped_files {
                tracing::warn!(
                    "Planetiler will use ~{} memory-mapped files for node locations and multipolygon geometries but the OS only has {} available to cache pages, this may slow the import down. To speed up, run on a machine with more memory.",
                    format::storage(memory_mapped_files, false),
                    format::storage(extra_memory, false)
                );
            } else {
                tracing::debug!(
                    "✓ {} temporary files and {} of free memory for OS to cache them",
                    format::storage(memory_mapped_files, false),
                    format::storage(extra_memory, false)
                );
            }
        }
        Ok(())
    }

    fn download(&mut self, profile: &Arc<dyn Profile>) -> anyhow::Result<()> {
        let timer = self.stats.start_stage("download");
        let mut downloader = Downloader::create((*self.config).clone());
        for to_download in std::mem::take(&mut self.to_download) {
            let urls = match to_download.urls {
                DownloadUrls::Ready(urls) => urls,
                DownloadUrls::Pending(handle) => {
                    handle.join().map_err(|_| anyhow!("resolving downloads for {} panicked", to_download.id))??
                }
            };
            if profile.cares_about_source(&to_download.id) {
                for (url, path) in urls {
                    downloader = downloader.add(&to_download.id, &url, path)?;
                }
            }
        }
        downloader.run()?;
        timer.stop();
        Ok(())
    }

    fn ensure_input_files_exist(&self, profile: &Arc<dyn Profile>) -> anyhow::Result<()> {
        for input_path in &self.input_paths {
            if profile.cares_about_source(&input_path.id) && !input_path.path.exists() {
                bail!("{} does not exist. Run with --download to fetch it", input_path.path.display());
            }
        }
        Ok(())
    }
}

/// Java's `StringUtils.getCommonPrefix(paths)` with the trailing partial path segment removed.
fn common_parent_prefix(paths: &[PathBuf]) -> String {
    let strings: Vec<String> = paths.iter().map(|p| p.to_string_lossy().to_string()).collect();
    let Some(first) = strings.first() else { return String::new() };
    let mut len = first.len();
    for s in &strings[1..] {
        len = first.bytes().zip(s.bytes()).take(len).take_while(|(a, b)| a == b).count();
    }
    let mut prefix = first[..len].to_string();
    let sep = std::path::MAIN_SEPARATOR;
    if let Some(idx) = prefix.rfind(sep) {
        prefix.truncate(idx);
    }
    prefix
}

/// `path.resolveSibling(path.getFileName() + "-unzipped")`.
fn unzipped_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push("-unzipped");
    path.with_file_name(name)
}

/// Glue between the runner and the simple readers.
mod simple_sources {
    use std::path::{Path, PathBuf};

    use super::StageContext;
    use crate::reader::geo_package_reader::GeoPackageReader;
    use crate::reader::geojson::geo_json_reader::GeoJsonReader;
    use crate::reader::natural_earth_reader::NaturalEarthReader;
    use crate::reader::shapefile_reader::ShapefileReader;

    pub fn process_shapefiles(
        projection: Option<&str>,
        paths: &[PathBuf],
        ctx: &mut StageContext<'_>,
    ) -> anyhow::Result<()> {
        ShapefileReader::process_with_projection(
            projection,
            ctx.name,
            paths.to_vec(),
            ctx.feature_group,
            ctx.config,
            ctx.profile,
            ctx.stats.clone(),
        )
    }

    pub fn process_geopackages(
        projection: Option<&str>,
        paths: &[PathBuf],
        unzip_dir: &Path,
        keep_unzipped: bool,
        ctx: &mut StageContext<'_>,
    ) -> anyhow::Result<()> {
        GeoPackageReader::process(
            projection,
            ctx.name,
            paths.to_vec(),
            unzip_dir,
            ctx.feature_group,
            ctx.config,
            ctx.profile,
            ctx.stats.clone(),
            keep_unzipped,
        )
    }

    pub fn process_geojson(paths: &[PathBuf], ctx: &mut StageContext<'_>) -> anyhow::Result<()> {
        GeoJsonReader::process(ctx.name, paths.to_vec(), ctx.feature_group, ctx.config, ctx.profile, ctx.stats.clone())
    }

    pub fn process_natural_earth(
        path: &Path,
        unzip_dir: &Path,
        keep_unzipped: bool,
        ctx: &mut StageContext<'_>,
    ) -> anyhow::Result<()> {
        NaturalEarthReader::process(
            ctx.name,
            path,
            unzip_dir,
            ctx.feature_group,
            ctx.config,
            ctx.profile,
            ctx.stats.clone(),
            keep_unzipped,
        )
    }
}

/// Reports the current size of a file for progress logging.
struct FileSize(PathBuf);

impl DiskBacked for FileSize {
    fn disk_usage_bytes(&self) -> u64 {
        file_utils::size_or_zero(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_languages() {
        let mut planetiler = Planetiler::create(Arguments::of_pairs([("languages", "default,en")])).unwrap();
        planetiler.set_default_languages(&["jbo", "tlh"]);
        let translations = planetiler.translations();
        assert!(translations.care_about_language("jbo"));
        assert!(translations.care_about_language("tlh"));
        assert!(translations.care_about_language("en"));
        assert!(!translations.care_about_language("fr"));
    }

    #[test]
    fn test_duplicate_stage() {
        let mut planetiler = Planetiler::create(Arguments::of()).unwrap();
        planetiler.add_stage("a", "first", |_| Ok(())).unwrap();
        assert!(planetiler.add_stage("a", "second", |_| Ok(())).is_err());
    }

    #[test]
    fn test_requires_profile_output_and_sources() {
        let mut planetiler = Planetiler::create(Arguments::of()).unwrap();
        assert!(planetiler.run().unwrap_err().to_string().contains("No profile"));
        planetiler.set_profile(crate::profile::NullProfile);
        assert!(planetiler.run().unwrap_err().to_string().contains("No output"));
        planetiler.set_output("/tmp/does-not-matter.mbtiles").unwrap();
        assert!(planetiler.run().unwrap_err().to_string().contains("No sources"));
    }
}
