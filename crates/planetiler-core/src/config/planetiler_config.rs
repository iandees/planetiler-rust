//! Port of `config/PlanetilerConfig.java`: holds planetiler configuration options.
//!
//! Fields are the snake_case equivalent of the Java record components.

use std::path::PathBuf;
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

impl PlanetilerConfig {
    /// Default configuration, equivalent to `PlanetilerConfig.from(Arguments.of())` in Java.
    pub fn defaults() -> PlanetilerConfig {
        let threads = num_cpus::get();
        let feature_write_threads = 1.max((threads.saturating_sub(16)) / 32 + 1);
        let feature_process_threads = 1.max(if threads < 8 { threads } else { threads - feature_write_threads });
        let tmp_dir = PathBuf::from("data").join("tmp");
        PlanetilerConfig {
            arguments: Arguments::of(),
            bounds: Bounds::world(),
            threads,
            feature_write_threads,
            feature_process_threads,
            feature_read_threads: if threads < 32 { 1 } else { 2 },
            tile_write_threads: 1,
            log_interval: Duration::from_secs(10),
            minzoom: MIN_MINZOOM,
            maxzoom: DEFAULT_MAXZOOM,
            maxzoom_for_rendering: DEFAULT_MAXZOOM,
            force: false,
            append: false,
            compress_temp_storage: false,
            mmap_temp_storage: true,
            sort_max_readers: 6,
            sort_max_writers: 6,
            node_map_type: "sparsearray".into(),
            node_map_storage: "mmap".into(),
            node_map_madvise: true,
            multipolygon_geometry_storage: "mmap".into(),
            multipolygon_geometry_madvise: true,
            http_user_agent: "Planetiler downloader (https://github.com/onthegomap/planetiler)".into(),
            http_timeout: Duration::from_secs(30),
            http_retries: 5,
            http_retry_wait: Duration::from_secs(5),
            download_chunk_size_mb: 100,
            download_threads: 1,
            download_max_bandwidth: 0.0,
            min_feature_size_at_max_zoom: 256.0 / 4096.0,
            min_feature_size_below_max_zoom: 1.0,
            simplify_tolerance_at_max_zoom: 256.0 / 4096.0,
            simplify_tolerance_below_max_zoom: 0.1,
            osm_lazy_reads: true,
            skip_filled_tiles: false,
            tile_warning_size_bytes: 1024 * 1024,
            color: None,
            keep_unzipped_sources: false,
            tile_compression: TileCompression::Gzip,
            tile_format: TileFormat::Mvt,
            exclude_ids: false,
            mlt_fsst: false,
            mlt_fast_pfor: false,
            mlt_tessellate_polygons: false,
            mlt_polygon_outline: false,
            mlt_reorder_feature: false,
            mlt_shared_dictionaries: false,
            output_layer_stats: false,
            debug_url_pattern: "https://onthegomap.github.io/planetiler-demo/#{z}/{lat}/{lon}".into(),
            tile_weights: tmp_dir.parent().unwrap_or(&tmp_dir).join("tile_weights.tsv.gz"),
            tmp_dir,
            max_point_buffer: f64::INFINITY,
            log_jts_exceptions: false,
            feature_source_id_multiplier: 10,
            extra_name_tags: vec![],
            reuse_feature_db: false,
            parallel_temp_io: false,
        }
    }

    /// Minimum feature size in tile pixels to emit at `zoom`.
    pub fn min_feature_size(&self, zoom: u32) -> f64 {
        if zoom >= self.maxzoom { self.min_feature_size_at_max_zoom } else { self.min_feature_size_below_max_zoom }
    }

    /// Simplification tolerance in tile pixels at `zoom`.
    pub fn tolerance(&self, zoom: u32) -> f64 {
        if zoom >= self.maxzoom { self.simplify_tolerance_at_max_zoom } else { self.simplify_tolerance_below_max_zoom }
    }
}

impl Default for PlanetilerConfig {
    fn default() -> Self {
        Self::defaults()
    }
}
