//! Port of `config/PlanetilerConfig.java`: holds planetiler configuration options.
//!
//! Fields are the snake_case equivalent of the Java record components.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::archive::tile_compression::TileCompression;
use crate::archive::tile_format::TileFormat;
use crate::config::arguments::Arguments;
use crate::config::bounds::Bounds;

pub const MIN_MINZOOM: u32 = 0;
pub const MAX_MAXZOOM: u32 = 16;
pub const DEFAULT_MAXZOOM: u32 = 14;

#[derive(Clone, Debug)]
pub struct PlanetilerConfig {
    pub arguments: Arguments,
    pub bounds: Bounds,
    pub threads: usize,
    pub feature_write_threads: usize,
    pub feature_process_threads: usize,
    pub feature_read_threads: usize,
    pub tile_write_threads: usize,
    pub log_interval: Duration,
    pub minzoom: u32,
    pub maxzoom: u32,
    pub maxzoom_for_rendering: u32,
    pub force: bool,
    pub append: bool,
    pub compress_temp_storage: bool,
    pub mmap_temp_storage: bool,
    pub sort_max_readers: usize,
    pub sort_max_writers: usize,
    pub node_map_type: String,
    pub node_map_storage: String,
    pub node_map_madvise: bool,
    pub multipolygon_geometry_storage: String,
    pub multipolygon_geometry_madvise: bool,
    pub http_user_agent: String,
    pub http_timeout: Duration,
    pub http_retries: u32,
    pub http_retry_wait: Duration,
    pub download_chunk_size_mb: u64,
    pub download_threads: usize,
    pub download_max_bandwidth: f64,
    pub min_feature_size_at_max_zoom: f64,
    pub min_feature_size_below_max_zoom: f64,
    pub simplify_tolerance_at_max_zoom: f64,
    pub simplify_tolerance_below_max_zoom: f64,
    pub osm_lazy_reads: bool,
    pub skip_filled_tiles: bool,
    pub tile_warning_size_bytes: usize,
    pub color: Option<bool>,
    pub keep_unzipped_sources: bool,
    pub tile_compression: TileCompression,
    pub tile_format: TileFormat,
    pub exclude_ids: bool,
    pub mlt_fsst: bool,
    pub mlt_fast_pfor: bool,
    pub mlt_tessellate_polygons: bool,
    pub mlt_polygon_outline: bool,
    pub mlt_reorder_feature: bool,
    pub mlt_shared_dictionaries: bool,
    pub output_layer_stats: bool,
    pub debug_url_pattern: String,
    pub tmp_dir: PathBuf,
    pub tile_weights: PathBuf,
    pub max_point_buffer: f64,
    pub log_jts_exceptions: bool,
    pub feature_source_id_multiplier: i64,
    pub extra_name_tags: Vec<String>,
    pub reuse_feature_db: bool,
    pub parallel_temp_io: bool,
}

const STORAGE_IDS: &str = "[ram, mmap, direct]";
const NODE_MAP_TYPE_IDS: &str = "[noop, sortedtable, sparsearray, array]";
const APPEND_FORMATS: &str = "[files, csv, tsv, proto, pbf, json]";

fn to_u32(value: i32, name: &str) -> anyhow::Result<u32> {
    u32::try_from(value).map_err(|_| anyhow::anyhow!("{name} must be >= 0, was {value}"))
}

fn to_usize(value: i32, name: &str) -> anyhow::Result<usize> {
    usize::try_from(value).map_err(|_| anyhow::anyhow!("{name} must be >= 0, was {value}"))
}

impl PlanetilerConfig {
    /// Default configuration, equivalent to `PlanetilerConfig.from(Arguments.of())` in Java.
    pub fn defaults() -> PlanetilerConfig {
        PlanetilerConfig::from(&Arguments::of()).expect("default config is valid")
    }

    /// Parses all config options from `arguments`, validating them.
    pub fn from(arguments: &Arguments) -> anyhow::Result<PlanetilerConfig> {
        // use --madvise and --storage options as default for temp storage, but allow users to override them explicitly
        // for multipolygon geometries or node locations
        let default_madvise = arguments.get_boolean(
            "madvise",
            "default value for whether to use linux madvise(random) to improve memory-mapped read performance for temporary storage",
            true,
        );
        // nodemap_storage was previously the only option, so if that's set make it the default
        let fallback_temp_storage = arguments.get_arg_or("nodemap_storage", "mmap");
        let default_temp_storage = arguments.get_string(
            "storage",
            &format!("default storage type for temporary data, one of {STORAGE_IDS}"),
            &fallback_temp_storage,
        );
        let parallel_temp_io = arguments.get_boolean(
            "parallel_tmp_io",
            "Use unlimited parallelism reading/writing temp files (ie. if using tmpfs or ramfs for temp storage)",
            false,
        );
        let threads = arguments.threads()?;
        // defaults: <48 cpus=1 writer, 48-80=2 writers, 80-112=3 writers, 112-144=4 writers, ...
        let default_write_threads = if parallel_temp_io { 1 } else { 1.max((threads as i32 - 16) / 32 + 1) };
        let feature_write_threads = to_usize(
            arguments.get_integer(
                "write_threads",
                "number of threads to use when writing temp features",
                default_write_threads,
            )?,
            "write_threads",
        )?;
        let default_process_threads =
            1.max(if threads < 8 { threads as i32 } else { threads as i32 - feature_write_threads as i32 });
        let feature_process_threads = to_usize(
            arguments.get_integer(
                "process_threads",
                "number of threads to use when processing input features",
                default_process_threads,
            )?,
            "process_threads",
        )?;
        let mut bounds = Bounds::new(arguments.bounds("bounds", "bounds")?);
        if let Some(polygon_file) =
            arguments.file_opt("polygon", "a .poly file that limits output to tiles intersecting the shape")
        {
            // TODO(port): parse with reader::osm::poly_file_reader once it is ported, then call bounds.set_shape(shape)
            let _ = &mut bounds;
            anyhow::bail!("--polygon is not supported yet: {}", polygon_file.display());
        }

        let minzoom = arguments.get_integer("minzoom", "minimum zoom level", MIN_MINZOOM as i32)?;
        let maxzoom = arguments.get_integer(
            "maxzoom",
            &format!("maximum zoom level up to {MAX_MAXZOOM}"),
            DEFAULT_MAXZOOM as i32,
        )?;
        let render_maxzoom = arguments.get_integer(
            "render_maxzoom",
            &format!("maximum rendering zoom level up to {MAX_MAXZOOM}"),
            maxzoom.max(DEFAULT_MAXZOOM as i32),
        )?;
        if minzoom > maxzoom {
            anyhow::bail!("Minzoom cannot be greater than maxzoom");
        }
        if minzoom < MIN_MINZOOM as i32 {
            anyhow::bail!("Minzoom must be >= {MIN_MINZOOM}, was {minzoom}");
        }
        if maxzoom > MAX_MAXZOOM as i32 {
            anyhow::bail!("Max zoom must be <= {MAX_MAXZOOM}, was {maxzoom}");
        }
        let tmp_dir = arguments.file("tmpdir|tmp", "temp directory", PathBuf::from("data").join("tmp"));
        let extra_name_tags =
            arguments.get_list::<&str>("extra_name_tags", "Extra name tags to copy from OSM to output", &[]);
        let mlt_advanced =
            arguments.get_boolean("mlt_advanced", "Use FSST and FastPFOR encoding schemes when tile format=MLT", false);

        let feature_read_threads = to_usize(
            arguments.get_integer(
                "feature_read_threads",
                "number of threads to use when reading features at tile write time",
                if threads < 32 { 1 } else { 2 },
            )?,
            "feature_read_threads",
        )?;
        let tile_write_threads = to_usize(
            arguments.get_integer(
                "tile_write_threads",
                &format!("number of threads used to write tiles - only supported by {APPEND_FORMATS}"),
                1,
            )?,
            "tile_write_threads",
        )?;
        let log_interval = arguments.get_duration("loginterval", "time between logs", "10s")?;
        let force = arguments.get_boolean("force", "overwriting output file and ignore disk/RAM warnings", false);
        let append = arguments.get_boolean(
            "append",
            &format!("append to the output file - only supported by {APPEND_FORMATS}"),
            false,
        );
        let compress_temp_storage = arguments.get_boolean(
            "compress_temp|gzip_temp",
            "compress temporary feature storage (uses more CPU, but less disk space)",
            false,
        );
        let mmap_temp_storage = arguments.get_boolean("mmap_temp", "use memory-mapped IO for temp feature files", true);
        let default_sort = if parallel_temp_io { feature_process_threads as i32 } else { 6 };
        let sort_max_readers = to_usize(
            arguments.get_integer(
                "sort_max_readers",
                "maximum number of concurrent read threads to use when sorting chunks",
                default_sort,
            )?,
            "sort_max_readers",
        )?;
        let sort_max_writers = to_usize(
            arguments.get_integer(
                "sort_max_writers",
                "maximum number of concurrent write threads to use when sorting chunks",
                default_sort,
            )?,
            "sort_max_writers",
        )?;
        let node_map_type = arguments.get_string(
            "nodemap_type",
            &format!("type of node location map, one of {NODE_MAP_TYPE_IDS}"),
            "sparsearray",
        );
        let node_map_storage = arguments.get_string(
            "nodemap_storage",
            &format!("storage for node location map, one of {STORAGE_IDS}"),
            &default_temp_storage,
        );
        let node_map_madvise =
            arguments.get_boolean("nodemap_madvise", "use linux madvise(random) for node locations", default_madvise);
        let multipolygon_geometry_storage = arguments.get_string(
            "multipolygon_geometry_storage",
            &format!("storage for multipolygon geometries, one of {STORAGE_IDS}"),
            &default_temp_storage,
        );
        let multipolygon_geometry_madvise = arguments.get_boolean(
            "multipolygon_geometry_madvise",
            "use linux madvise(random) for temporary multipolygon geometry storage",
            default_madvise,
        );
        let http_user_agent = arguments.get_string(
            "http_user_agent",
            "User-Agent header to set when downloading files over HTTP",
            "Planetiler downloader (https://github.com/onthegomap/planetiler)",
        );
        let http_timeout =
            arguments.get_duration("http_timeout", "Timeout to use when downloading files over HTTP", "30s")?;
        let http_retries =
            arguments.get_integer("http_retries", "Retries to use when downloading files over HTTP", 5)?;
        if http_retries < 0 {
            anyhow::bail!("HTTP Retries must be >= 0, was {http_retries}");
        }
        let http_retry_wait =
            arguments.get_duration("http_retry_wait", "How long to wait before retrying HTTP request", "5s")?;
        let download_chunk_size_mb = arguments.get_long(
            "download_chunk_size_mb",
            "Size of file chunks to download in parallel in megabytes",
            100,
        )?;
        let download_threads = to_usize(
            arguments.get_integer(
                "download_threads",
                "Number of parallel threads to use when downloading each file",
                1,
            )?,
            "download_threads",
        )?;
        let download_max_bandwidth = crate::util::parse::bandwidth_str(&arguments.get_string(
            "download_max_bandwidth",
            "Maximum bandwidth to consume when downloading files in units mb/s, mbps, kbps, etc.",
            "",
        ))?;
        let min_feature_size_at_max_zoom = arguments.get_double(
            "min_feature_size_at_max_zoom",
            "Default value for the minimum size in tile pixels of features to emit at the maximum zoom level to allow for overzooming",
            256.0 / 4096.0,
        )?;
        let min_feature_size_below_max_zoom = arguments.get_double(
            "min_feature_size",
            "Default value for the minimum size in tile pixels of features to emit below the maximum zoom level",
            1.0,
        )?;
        let simplify_tolerance_at_max_zoom = arguments.get_double(
            "simplify_tolerance_at_max_zoom",
            "Default value for the tile pixel tolerance to use when simplifying features at the maximum zoom level to allow for overzooming",
            256.0 / 4096.0,
        )?;
        let simplify_tolerance_below_max_zoom = arguments.get_double(
            "simplify_tolerance",
            "Default value for the tile pixel tolerance to use when simplifying features below the maximum zoom level",
            0.1,
        )?;
        let osm_lazy_reads =
            arguments.get_boolean("osm_lazy_reads", "Read OSM blocks from disk in worker threads", true);
        let skip_filled_tiles = arguments.get_boolean(
            "skip_filled_tiles",
            "Skip writing tiles containing only polygon fills to the output",
            false,
        );
        let tile_warning_size_bytes = (arguments.get_double(
            "tile_warning_size_mb",
            "Maximum size in megabytes of a tile to emit a warning about",
            1.0,
        )? * 1024.0
            * 1024.0) as usize;
        let color = arguments.get_boolean_object("color", "Color the terminal output");
        let keep_unzipped_sources =
            arguments.get_boolean("keep_unzipped", "keep unzipped sources by default after reading", false);
        let tile_compression = TileCompression::from_id(&arguments.get_string(
            "tile_compression",
            &format!(
                "the tile compression, one of {:?}",
                TileCompression::available_values().iter().map(|c| c.id()).collect::<Vec<_>>()
            ),
            "gzip",
        ))?;
        let tile_format = TileFormat::from_id(&arguments.get_string(
            "tile_format",
            &format!(
                "the tile format, one of {:?}",
                TileFormat::available_values().iter().map(|c| c.id()).collect::<Vec<_>>()
            ),
            "mvt",
        ))?;
        let exclude_ids =
            arguments.get_boolean("exclude_ids", "Exclude feature IDs from generated vector tiles", false);
        let mlt_fast_pfor =
            arguments.get_boolean("mlt_fastpfor", "Use FastPFOR encoding when tile format=MLT", mlt_advanced);
        let mlt_fsst = arguments.get_boolean("mlt_fsst", "Use FSST encoding when tile format=MLT", mlt_advanced);
        let mlt_tessellate_polygons = arguments.get_boolean(
            "mlt_tessellate_polygons",
            "Pre-triangulate polygons when tile format=MLT so that clients do not need to",
            false,
        );
        let mlt_polygon_outline = arguments.get_boolean(
            "mlt_polygon_outline",
            "Store polygon outlines together with pre-triangulated polygons",
            false,
        );
        let mlt_reorder_feature = arguments.get_boolean(
            "mlt_reorder_features",
            "Allow re-ordering output features within each layer when tile format=MLT to reduce tile sizes",
            false,
        );
        let mlt_shared_dictionaries = arguments.get_boolean(
            "mlt_shared_dict",
            "Share dictionaries between string fields when tile format=MLT",
            false,
        );
        let output_layer_stats =
            arguments.get_boolean("output_layerstats", "output a tsv.gz file for each tile/layer size", false);
        let debug_url_pattern = arguments.get_string(
            "debug_url",
            "debug url to use for displaying tiles with {z} {lat} {lon} placeholders",
            "https://onthegomap.github.io/planetiler-demo/#{z}/{lat}/{lon}",
        );
        let tile_weights = arguments.file(
            "tile_weights",
            "tsv.gz file with columns z,x,y,loads to generate weighted average tile size stat",
            tmp_dir.parent().unwrap_or(Path::new("")).join("tile_weights.tsv.gz"),
        );
        let max_point_buffer = arguments.get_double(
            "max_point_buffer",
            "Additional global limit for the max tile pixels to include points outside tile bounds of all layers. Set to a lower value to reduce tile size for \
             clients that handle label collisions across tiles (most web and native clients). NOTE: Do not reduce if you need to support \
             raster tile rendering",
            f64::INFINITY,
        )?;
        let log_jts_exceptions =
            arguments.get_boolean("log_jts_exceptions", "Emit verbose details to debug JTS geometry errors", false);
        let feature_source_id_multiplier = arguments.get_integer(
            "feature_source_id_multiplier",
            "Set vector tile feature IDs to (featureId * thisValue) + sourceId \
             where sourceId is 1 for OSM nodes, 2 for ways, 3 for relations, and 0 for other sources. Set to false to disable.",
            10,
        )? as i64;
        let reuse_feature_db = arguments.get_boolean(
            "reuse_featuredb",
            "Reuse existing feature DB on disk, skipping source reading stages (for iterating on post-processing logic)",
            false,
        );

        Ok(PlanetilerConfig {
            arguments: arguments.clone(),
            bounds,
            threads,
            feature_write_threads,
            feature_process_threads,
            feature_read_threads,
            tile_write_threads,
            log_interval,
            minzoom: to_u32(minzoom, "minzoom")?,
            maxzoom: to_u32(maxzoom, "maxzoom")?,
            maxzoom_for_rendering: to_u32(render_maxzoom, "render_maxzoom")?,
            force,
            append,
            compress_temp_storage,
            mmap_temp_storage,
            sort_max_readers,
            sort_max_writers,
            node_map_type,
            node_map_storage,
            node_map_madvise,
            multipolygon_geometry_storage,
            multipolygon_geometry_madvise,
            http_user_agent,
            http_timeout,
            http_retries: http_retries as u32,
            http_retry_wait,
            download_chunk_size_mb: download_chunk_size_mb.max(0) as u64,
            download_threads,
            download_max_bandwidth,
            min_feature_size_at_max_zoom,
            min_feature_size_below_max_zoom,
            simplify_tolerance_at_max_zoom,
            simplify_tolerance_below_max_zoom,
            osm_lazy_reads,
            skip_filled_tiles,
            tile_warning_size_bytes,
            color,
            keep_unzipped_sources,
            tile_compression,
            tile_format,
            exclude_ids,
            mlt_fsst,
            mlt_fast_pfor,
            mlt_tessellate_polygons,
            mlt_polygon_outline,
            mlt_reorder_feature,
            mlt_shared_dictionaries,
            output_layer_stats,
            debug_url_pattern,
            tmp_dir,
            tile_weights,
            max_point_buffer,
            log_jts_exceptions,
            feature_source_id_multiplier,
            extra_name_tags,
            reuse_feature_db,
            parallel_temp_io,
        })
    }

    /// Minimum feature size in tile pixels to emit at `zoom`.
    pub fn min_feature_size(&self, zoom: u32) -> f64 {
        if zoom >= self.maxzoom_for_rendering {
            self.min_feature_size_at_max_zoom
        } else {
            self.min_feature_size_below_max_zoom
        }
    }

    /// Simplification tolerance in tile pixels at `zoom`.
    pub fn tolerance(&self, zoom: u32) -> f64 {
        if zoom >= self.maxzoom_for_rendering {
            self.simplify_tolerance_at_max_zoom
        } else {
            self.simplify_tolerance_below_max_zoom
        }
    }

    /// Maximum time an HTTP request can take including retries.
    pub fn max_http_request_duration(&self) -> Duration {
        self.http_timeout * (self.http_retries + 1) + self.http_retry_wait * self.http_retries
    }
}

impl Default for PlanetilerConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let config = PlanetilerConfig::defaults();
        assert_eq!(0, config.minzoom);
        assert_eq!(14, config.maxzoom);
        assert_eq!(14, config.maxzoom_for_rendering);
        assert!(config.threads >= 2);
        assert_eq!(PathBuf::from("data").join("tmp"), config.tmp_dir);
        assert_eq!(PathBuf::from("data").join("tile_weights.tsv.gz"), config.tile_weights);
        assert_eq!("mmap", config.node_map_storage);
        assert_eq!("sparsearray", config.node_map_type);
        assert_eq!(TileCompression::Gzip, config.tile_compression);
        assert_eq!(TileFormat::Mvt, config.tile_format);
        assert_eq!(1024 * 1024, config.tile_warning_size_bytes);
        assert_eq!(f64::INFINITY, config.max_point_buffer);
        assert_eq!(None, config.color);
        assert_eq!(0.0, config.download_max_bandwidth);
        assert_eq!(Duration::from_secs(10), config.log_interval);
        assert_eq!(Duration::from_secs(30 * 6 + 5 * 5), config.max_http_request_duration());
        assert!(config.bounds.is_world());
    }

    #[test]
    fn parse_options() {
        let config = PlanetilerConfig::from(&Arguments::of_pairs([
            ("minzoom", "2"),
            ("maxzoom", "10"),
            ("storage", "ram"),
            ("tmp", "/tmp/x"),
            ("threads", "16"),
            ("bounds", "1,2,3,4"),
            ("download_max_bandwidth", "1mb/s"),
            ("mlt_advanced", "true"),
            ("color", "false"),
            ("extra_name_tags", "a, b"),
            ("feature_source_id_multiplier", "false"),
        ]))
        .unwrap();
        assert_eq!(2, config.minzoom);
        assert_eq!(10, config.maxzoom);
        assert_eq!(14, config.maxzoom_for_rendering);
        assert_eq!("ram", config.node_map_storage);
        assert_eq!("ram", config.multipolygon_geometry_storage);
        assert_eq!(PathBuf::from("/tmp/x"), config.tmp_dir);
        assert_eq!(PathBuf::from("/tmp/tile_weights.tsv.gz"), config.tile_weights);
        assert_eq!(16, config.threads);
        assert_eq!(1, config.feature_write_threads);
        assert_eq!(15, config.feature_process_threads);
        assert_eq!(1_000_000.0, config.download_max_bandwidth);
        assert!(config.mlt_fsst && config.mlt_fast_pfor);
        assert_eq!(Some(false), config.color);
        assert_eq!(vec!["a", "b"], config.extra_name_tags);
        assert_eq!(0, config.feature_source_id_multiplier);
        assert!(!config.bounds.is_world());
        // below render maxzoom (14) use the regular tolerance
        assert_eq!(0.1, config.tolerance(10));
        assert_eq!(256.0 / 4096.0, config.tolerance(14));
    }

    #[test]
    fn validation() {
        assert!(PlanetilerConfig::from(&Arguments::of_pairs([("minzoom", "5"), ("maxzoom", "4")])).is_err());
        assert!(PlanetilerConfig::from(&Arguments::of_pairs([("maxzoom", "17")])).is_err());
        assert!(PlanetilerConfig::from(&Arguments::of_pairs([("minzoom", "-1")])).is_err());
        assert!(PlanetilerConfig::from(&Arguments::of_pairs([("http_retries", "-1")])).is_err());
        assert!(PlanetilerConfig::from(&Arguments::of_pairs([("tile_format", "bogus")])).is_err());
        assert!(PlanetilerConfig::from(&Arguments::of_pairs([("download_max_bandwidth", "1 parsecs")])).is_err());
    }
}
