//! Port of `util/TilesetSummaryStatistics.java`.
//!
//! Utility that computes min/max/average sizes for each vector tile layers at each zoom level, then computes combined
//! summary statistics at the end.
//!
//! Provide a [`TileWeightSource`] (`TileWeights` in Java) to compute weighted average tile sizes based on actual tile
//! traffic.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::config::planetiler_config::{MAX_MAXZOOM, MIN_MINZOOM};
use crate::geo::tile_coord::TileCoord;
use crate::util::tile_size_stats::LayerStats;
use crate::util::{ansi_colors, format};

const TOP_N_TILES: usize = 10;
const WARN_BYTES: i64 = 100_000;
const ERROR_BYTES: i64 = 500_000;
const NUM_ZOOMS: usize = (MAX_MAXZOOM - MIN_MINZOOM + 1) as usize;

/// Source of per-tile traffic weights (Java's `TileWeights`); implemented by
/// [`TileWeights`](crate::util::tile_weights::TileWeights).
pub trait TileWeightSource: Send + Sync {
    /// Weight of a single tile (0 if unknown).
    fn get_weight(&self, coord: &TileCoord) -> i64;
    /// Sum of all tile weights at a zoom level.
    fn get_zoom_weight(&self, zoom: u32) -> i64;
    /// True if there are no weights.
    fn is_empty(&self) -> bool;
}

/// Tile weights that are all 0, which results in unweighted averages.
#[derive(Default, Debug, Clone, Copy)]
pub struct NoTileWeights;

impl TileWeightSource for NoTileWeights {
    fn get_weight(&self, _coord: &TileCoord) -> i64 {
        0
    }
    fn get_zoom_weight(&self, _zoom: u32) -> i64 {
        0
    }
    fn is_empty(&self) -> bool {
        true
    }
}

fn tile_biggest_layers(tile: &TileSummary) -> String {
    let min_size = tile.layers.iter().map(|l| l.layer_bytes).max().unwrap_or(0);
    let mut layers: Vec<&LayerStats> = tile.layers.iter().filter(|d| d.layer_bytes >= min_size).collect();
    layers.sort_by_key(|d| -d.layer_bytes);
    layers
        .iter()
        .map(|d| format!("{}:{}", d.layer, format::storage(d.layer_bytes as f64, false)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Computes summary statistics over all tiles written to an archive.
pub struct TilesetSummaryStatistics {
    tile_weights: Arc<dyn TileWeightSource>,
    // instead of threads updating concurrent data structures, each thread gets a thread-local
    // Summary instance it can update without contention that are combined at the end.
    summaries: Mutex<Vec<Arc<Mutex<SummaryData>>>>,
}

impl Default for TilesetSummaryStatistics {
    fn default() -> Self {
        Self::new()
    }
}

impl TilesetSummaryStatistics {
    /// Stats without tile weights (unweighted averages).
    pub fn new() -> Self {
        Self::with_weights(Arc::new(NoTileWeights))
    }

    pub fn with_weights(tile_weights: Arc<dyn TileWeightSource>) -> Self {
        TilesetSummaryStatistics { tile_weights, summaries: Mutex::new(Vec::new()) }
    }

    /// Returns a combined [`Summary`] from each thread's [`Updater`].
    pub fn summary(&self) -> Summary {
        let mut result = Summary::new(self.tile_weights.clone());
        for summary in self.summaries.lock().iter() {
            result.data.merge_in(&summary.lock());
        }
        result
    }

    /// Logs biggest tiles, max layer size by zoom, and weighted average tile sizes.
    pub fn print_stats(&self, debug_url_pattern: &str) {
        tracing::debug!("Tile stats:");
        let result = self.summary();
        let overall_stats = result.get();
        tracing::debug!("Biggest tiles (gzipped)\n{}", overall_stats.format_biggest_tiles(debug_url_pattern));
        let already_listed: HashSet<TileCoord> = overall_stats.biggest_tiles().iter().map(|t| t.coord).collect();
        let other_tiles: Vec<TileSummary> = result
            .layers()
            .iter()
            .filter_map(|layer| result.get_layer(layer).biggest_tiles().into_iter().next())
            .filter(|tile| !already_listed.contains(&tile.coord) && tile.archived_size > WARN_BYTES)
            .collect();
        if !other_tiles.is_empty() {
            tracing::info!(
                "Other tiles with large layers\n{}",
                other_tiles
                    .iter()
                    .map(|tile| format!(
                        "{}/{}/{} ({}) {} ({})",
                        tile.coord.z,
                        tile.coord.x,
                        tile.coord.y,
                        format::storage(tile.archived_size as f64, false),
                        tile.coord.get_debug_url(debug_url_pattern),
                        tile_biggest_layers(tile)
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }

        tracing::debug!(
            "Max tile sizes\n{}\n{}\n{}",
            result.format_table(
                |n| {
                    let string = format!(" {}", format::storage(n, true));
                    if n as i64 > ERROR_BYTES {
                        ansi_colors::red(&string)
                    } else if n as i64 > WARN_BYTES {
                        ansi_colors::yellow(&string)
                    } else {
                        string
                    }
                },
                |cell| cell.max_size() as f64
            ),
            result.format_row_with(
                "full tile",
                |n| format::storage(n, false),
                |z| result.get_zoom(z).max_size() as f64,
                overall_stats.max_size() as f64
            ),
            result.format_row_with(
                "gzipped",
                |n| format::storage(n, false),
                |z| result.get_zoom(z).max_archived_size() as f64,
                overall_stats.max_archived_size() as f64
            )
        );
        tracing::debug!(
            "   Max tile: {} (gzipped: {})",
            format::storage(overall_stats.max_size() as f64, false),
            format::storage(overall_stats.max_archived_size() as f64, false)
        );
        tracing::debug!(
            "   Avg tile: {} (gzipped: {}) {}",
            format::storage(overall_stats.weighted_average_size(), false),
            format::storage(overall_stats.weighted_average_archived_size(), false),
            if self.tile_weights.is_empty() {
                "no tile weights, use --download-osm-tile-weights for weighted average"
            } else {
                "using weighted average based on OSM traffic"
            }
        );
        tracing::debug!("    # tiles: {}", format::integer(overall_stats.num_tiles()));
    }

    /// Returns an [`Updater`] that accepts individual tile layer stats from a thread that will eventually be combined
    /// into the final tileset report.
    pub fn thread_local_updater(&self) -> Updater {
        let summary = Arc::new(Mutex::new(SummaryData::default()));
        self.summaries.lock().push(summary.clone());
        Updater { summary, tile_weights: self.tile_weights.clone() }
    }
}

/// Java's `LongSummaryStatistics`.
#[derive(Clone, Debug)]
struct LongSummaryStatistics {
    count: i64,
    sum: i64,
    min: i64,
    max: i64,
}

impl Default for LongSummaryStatistics {
    fn default() -> Self {
        LongSummaryStatistics { count: 0, sum: 0, min: i64::MAX, max: i64::MIN }
    }
}

impl LongSummaryStatistics {
    fn accept(&mut self, value: i64) {
        self.count += 1;
        self.sum = self.sum.wrapping_add(value);
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }

    fn combine(&mut self, other: &LongSummaryStatistics) {
        self.count += other.count;
        self.sum = self.sum.wrapping_add(other.sum);
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }

    fn average(&self) -> f64 {
        if self.count > 0 { self.sum as f64 / self.count as f64 } else { 0.0 }
    }
}

/// Aggregated statistics for a layer/zoom, layer, zoom, or entire tileset.
#[derive(Clone, Debug, Default)]
pub struct Cell {
    archived_bytes: LongSummaryStatistics,
    bytes: LongSummaryStatistics,
    top_tiles: BinaryHeap<Reverse<TileSummary>>,
    weighted_bytes_sum: i64,
    weighted_archived_bytes_sum: i64,
    total_weight: i64,
    big_tile_cutoff: i64,
}

impl Cell {
    fn combine(a: &Cell, b: &Cell) -> Cell {
        let mut result = Cell::default();
        result.merge_in(a, 1.0);
        result.merge_in(b, 1.0);
        result
    }

    /// Max raw layer bytes (or tile size when aggregated over all layers).
    pub fn max_size(&self) -> i64 {
        self.bytes.max.max(0)
    }

    /// Max gzipped tile bytes (or 0 when broken-out by layer).
    pub fn max_archived_size(&self) -> i64 {
        self.archived_bytes.max.max(0)
    }

    /// Total tiles included in this aggregation.
    pub fn num_tiles(&self) -> i64 {
        self.bytes.count
    }

    /// Returns the biggest tiles in this aggregation by gzipped size (when aggregated over all layers) or raw size
    /// within an individual layer.
    pub fn biggest_tiles(&self) -> Vec<TileSummary> {
        let mut result: Vec<TileSummary> = self.top_tiles.iter().map(|t| t.0.clone()).collect();
        result.sort_by(|a, b| b.cmp(a));
        result
    }

    /// Returns average gzipped tile size in this aggregation, weighted by the tile weights provided.
    ///
    /// When multiple zoom-levels are combined, the weighted average respects the weight-per-zoom-level from tile
    /// weights so that low zoom tiles are not overweighted when analyzing a small extract.
    pub fn weighted_average_archived_size(&self) -> f64 {
        if self.total_weight == 0 {
            self.archived_bytes.average()
        } else {
            self.weighted_archived_bytes_sum as f64 / self.total_weight as f64
        }
    }

    /// Returns average raw (not gzipped) tile size in this aggregation, weighted by the tile weights provided.
    pub fn weighted_average_size(&self) -> f64 {
        if self.total_weight == 0 {
            self.bytes.average()
        } else {
            self.weighted_bytes_sum as f64 / self.total_weight as f64
        }
    }

    fn merge_in(&mut self, other: &Cell, weight: f64) {
        // Java: `long += long * double` compound assignment truncates back to long
        self.total_weight = (self.total_weight as f64 + other.total_weight as f64 * weight) as i64;
        self.weighted_bytes_sum = (self.weighted_bytes_sum as f64 + other.weighted_bytes_sum as f64 * weight) as i64;
        self.weighted_archived_bytes_sum =
            (self.weighted_archived_bytes_sum as f64 + other.weighted_archived_bytes_sum as f64 * weight) as i64;
        self.archived_bytes.combine(&other.archived_bytes);
        self.bytes.combine(&other.bytes);
        for big_tile in other.top_tiles.iter() {
            let t = &big_tile.0;
            self.accept_big_tile(t.coord, t.archived_size, || t.layers.clone());
        }
    }

    fn accept_big_tile(&mut self, coord: TileCoord, archived_bytes: i64, layers: impl FnOnce() -> Vec<LayerStats>) {
        if archived_bytes >= self.big_tile_cutoff {
            self.top_tiles.push(Reverse(TileSummary { coord, archived_size: archived_bytes, layers: layers() }));
            while self.top_tiles.len() > TOP_N_TILES {
                self.top_tiles.pop();
                if let Some(min) = self.top_tiles.peek() {
                    self.big_tile_cutoff = min.0.archived_size;
                }
            }
        }
    }

    pub fn format_biggest_tiles(&self, debug_url_pattern: &str) -> String {
        self.biggest_tiles()
            .iter()
            .enumerate()
            .map(|(index, tile)| {
                format!(
                    "{}. {}/{}/{} ({}) {} ({})",
                    index + 1,
                    tile.coord.z,
                    tile.coord.x,
                    tile.coord.y,
                    format::storage(tile.archived_size as f64, false),
                    tile.coord.get_debug_url(debug_url_pattern),
                    tile_biggest_layers(tile)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Statistics for a tile and its layers. Ordered by archived size, then tile coordinate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileSummary {
    pub coord: TileCoord,
    pub archived_size: i64,
    pub layers: Vec<LayerStats>,
}

impl TileSummary {
    pub fn with_size(&self, new_size: i64) -> TileSummary {
        TileSummary { archived_size: new_size, ..self.clone() }
    }
}

impl PartialOrd for TileSummary {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TileSummary {
    fn cmp(&self, other: &Self) -> Ordering {
        self.archived_size.cmp(&other.archived_size).then(self.coord.encoded.cmp(&other.coord.encoded))
    }
}

#[derive(Clone, Debug)]
struct SummaryData {
    by_tile: Vec<Cell>,
    by_layer: Vec<HashMap<String, Cell>>,
}

impl Default for SummaryData {
    fn default() -> Self {
        SummaryData { by_tile: vec![Cell::default(); NUM_ZOOMS], by_layer: vec![HashMap::new(); NUM_ZOOMS] }
    }
}

impl SummaryData {
    fn merge_in(&mut self, other: &SummaryData) {
        for z in 0..NUM_ZOOMS {
            self.by_tile[z].merge_in(&other.by_tile[z], 1.0);
        }
        for z in 0..NUM_ZOOMS {
            let our_map = &mut self.by_layer[z];
            for (layer, stats) in &other.by_layer[z] {
                match our_map.get_mut(layer) {
                    Some(existing) => *existing = Cell::combine(existing, stats),
                    None => {
                        our_map.insert(layer.clone(), stats.clone());
                    }
                }
            }
        }
    }
}

/// Overall summary statistics for a tileset, aggregated from all [`Updater`]s.
pub struct Summary {
    data: SummaryData,
    tile_weights: Arc<dyn TileWeightSource>,
}

impl Summary {
    fn new(tile_weights: Arc<dyn TileWeightSource>) -> Self {
        Summary { data: SummaryData::default(), tile_weights }
    }

    /// All the layers that appear in the tileset, sorted.
    pub fn layers(&self) -> Vec<String> {
        self.data.by_layer.iter().flat_map(|e| e.keys().cloned()).collect::<BTreeSet<_>>().into_iter().collect()
    }

    /// Returns the summary statistics for a layer at a zoom level.
    pub fn get_zoom_layer(&self, z: u32, layer: &str) -> Cell {
        self.data.by_layer[z as usize].get(layer).cloned().unwrap_or_default()
    }

    /// Returns the summary statistics for a layer from all zoom levels.
    pub fn get_layer(&self, layer: &str) -> Cell {
        let default = Cell::default();
        let cells: Vec<&Cell> = self.data.by_layer.iter().map(|e| e.get(layer).unwrap_or(&default)).collect();
        self.combine_zooms(&cells)
    }

    /// Returns the summary statistics for a zoom level from all layers.
    pub fn get_zoom(&self, z: u32) -> &Cell {
        &self.data.by_tile[z as usize]
    }

    /// Returns the summary statistics for the entire dataset by aggregating all layers and zoom-levels.
    pub fn get(&self) -> Cell {
        let cells: Vec<&Cell> = self.data.by_tile.iter().collect();
        self.combine_zooms(&cells)
    }

    /// Returns the minimum zoom a tile appears at in the tileset.
    pub fn min_zoom_with_data(&self) -> u32 {
        (0..NUM_ZOOMS).find(|&i| self.data.by_tile[i].num_tiles() > 0).map_or(MAX_MAXZOOM, |z| z as u32)
    }

    /// Returns the maximum zoom a tile appears at in the tileset.
    pub fn max_zoom_with_data(&self) -> u32 {
        (0..NUM_ZOOMS).rev().find(|&i| self.data.by_tile[i].num_tiles() > 0).map_or(MAX_MAXZOOM, |z| z as u32)
    }

    /// Returns the minimum zoom a specific layer appears at in the tileset.
    pub fn min_zoom_with_data_for_layer(&self, layer: &str) -> u32 {
        (0..NUM_ZOOMS).find(|&i| self.data.by_layer[i].contains_key(layer)).map_or(MAX_MAXZOOM, |z| z as u32)
    }

    fn combine_zooms(&self, by_tile: &[&Cell]) -> Cell {
        // aggregate Cells over zoom levels, but respect the global zoom-level weights from TileWeights
        let mut sum_weight = 0.0;
        let mut pre_sum_weight = 0.0;
        for (z, cell) in by_tile.iter().enumerate() {
            let zoom_weight = self.tile_weights.get_zoom_weight(z as u32);
            if cell.num_tiles() > 0 && zoom_weight > 0 {
                sum_weight += zoom_weight as f64;
                pre_sum_weight += cell.total_weight as f64;
            }
        }
        let no_data = sum_weight == 0.0 || pre_sum_weight == 0.0;
        let mut result = Cell::default();
        for (z, cell) in by_tile.iter().enumerate() {
            let zoom_weight = self.tile_weights.get_zoom_weight(z as u32);
            if (cell.num_tiles() > 0 && zoom_weight > 0) || no_data {
                let weight = if no_data {
                    1.0
                } else {
                    (zoom_weight as f64 / sum_weight) / (cell.total_weight as f64 / pre_sum_weight)
                };
                result.merge_in(cell, weight);
            }
        }
        result
    }

    fn sorted_layers(&self) -> Vec<String> {
        let mut layers = self.layers();
        layers.sort_by_key(|l| self.min_zoom_with_data_for_layer(l));
        layers
    }

    /// Format a row of the table using a numeric `formatter` applied to each zoom's stat and `last_column`.
    pub fn format_row_with(
        &self,
        first_column: &str,
        formatter: impl Fn(f64) -> String,
        extract_cells: impl Fn(u32) -> f64,
        last_column: f64,
    ) -> String {
        self.format_row(first_column, |z| formatter(extract_cells(z)), &formatter(last_column))
    }

    pub fn format_row(&self, first_column: &str, extract_stat: impl Fn(u32) -> String, last_column: &str) -> String {
        let mut builder = String::new();
        let min_zoom = self.min_zoom_with_data();
        let max_zoom = self.max_zoom_with_data();
        let layers = self.sorted_layers();
        let max_layer_length = layers.iter().map(|l| l.chars().count()).max().unwrap_or(0).max(9);
        builder.push_str(&format!("{first_column:>max_layer_length$}"));
        for z in min_zoom..=max_zoom {
            builder.push_str(&format!("{:>5}", extract_stat(z)));
            builder.push(' ');
        }
        builder.push_str(&format!("{last_column:>5}"));
        builder
    }

    pub fn format_table(&self, formatter: impl Fn(f64) -> String, extract_stat: impl Fn(&Cell) -> f64) -> String {
        let mut builder = String::new();
        let layers = self.sorted_layers();

        // header:   0 1 2 3 4 ... 16
        builder.push_str(&self.format_row("", |z| format!("z{z}"), "all"));
        builder.push('\n');

        // each row: layer
        for layer in &layers {
            builder.push_str(&self.format_row_with(
                layer,
                &formatter,
                |z| extract_stat(&self.get_zoom_layer(z, layer)),
                extract_stat(&self.get_layer(layer)),
            ));
            builder.push('\n');
        }
        builder.trim_end().to_string()
    }
}

/// Thread local updater that accepts individual statistics for each tile.
pub struct Updater {
    summary: Arc<Mutex<SummaryData>>,
    tile_weights: Arc<dyn TileWeightSource>,
}

impl Updater {
    pub fn record_tile(&self, coord: TileCoord, archived_bytes: i64, layer_stats: &[LayerStats]) {
        let mut summary = self.summary.lock();
        let summary = &mut *summary;
        let z = coord.z as usize;
        let weight = self.tile_weights.get_weight(&coord);
        let tile_stat = &mut summary.by_tile[z];
        tile_stat.archived_bytes.accept(archived_bytes);
        tile_stat.accept_big_tile(coord, archived_bytes, || layer_stats.to_vec());
        tile_stat.total_weight += weight;
        tile_stat.weighted_archived_bytes_sum += weight * archived_bytes;

        let layer_stat = &mut summary.by_layer[z];
        let mut sum = 0i64;
        for layer in layer_stats {
            if !layer_stat.contains_key(&layer.layer) {
                layer_stat.insert(layer.layer.clone(), Cell::default());
            }
            let cell = layer_stat.get_mut(&layer.layer).expect("inserted above");
            cell.bytes.accept(layer.layer_bytes);
            cell.accept_big_tile(coord, layer.layer_bytes, || layer_stats.to_vec());
            sum += layer.layer_bytes;
            cell.weighted_bytes_sum += weight * layer.layer_bytes;
            cell.total_weight += weight;
        }
        let tile_stat = &mut summary.by_tile[z];
        tile_stat.weighted_bytes_sum += weight * sum;
        tile_stat.bytes.accept(sum);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_abs_diff_eq;

    #[derive(Default)]
    struct TestWeights {
        by_zoom: HashMap<u32, i64>,
        weights: HashMap<TileCoord, i64>,
    }

    impl TestWeights {
        fn put(mut self, coord: TileCoord, weight: i64) -> Self {
            *self.weights.entry(coord).or_default() += weight;
            *self.by_zoom.entry(coord.z).or_default() += weight;
            self
        }
    }

    impl TileWeightSource for TestWeights {
        fn get_weight(&self, coord: &TileCoord) -> i64 {
            self.weights.get(coord).copied().unwrap_or(0)
        }
        fn get_zoom_weight(&self, zoom: u32) -> i64 {
            self.by_zoom.get(&zoom).copied().unwrap_or(0)
        }
        fn is_empty(&self) -> bool {
            self.by_zoom.values().all(|v| *v <= 0)
        }
    }

    fn ls(layer: &str, a: i64, b: i64, c: i64, d: i64, e: i64, f: i64) -> LayerStats {
        LayerStats::new(layer, a, b, c, d, e, f)
    }

    #[test]
    fn aggregate_tile_stats() {
        let tile_stats = TilesetSummaryStatistics::new();
        let updater1 = tile_stats.thread_local_updater();
        let updater2 = tile_stats.thread_local_updater();
        updater1.record_tile(TileCoord::of_xyz(0, 0, 1), 123, &[ls("a", 1, 2, 2, 3, 4, 5), ls("b", 6, 7, 7, 8, 9, 10)]);
        updater2.record_tile(TileCoord::of_xyz(0, 1, 1), 345, &[ls("b", 1, 2, 2, 3, 4, 5), ls("c", 6, 7, 7, 8, 9, 10)]);
        let summary = tile_stats.summary();
        assert_eq!(vec!["a", "b", "c"], summary.layers());
        assert_eq!(0, summary.get_zoom(0).max_size());
        assert_eq!(0, summary.get_zoom(0).num_tiles());
        assert_eq!(7, summary.get_zoom(1).max_size());
        assert_eq!(2, summary.get_zoom(1).num_tiles());

        assert_eq!(0, summary.get_zoom_layer(0, "a").max_size());
        assert_eq!(1, summary.get_zoom_layer(1, "a").max_size());
        assert_eq!(6, summary.get_zoom_layer(1, "b").max_size());
        assert_eq!(6, summary.get_zoom_layer(1, "c").max_size());

        assert_eq!(0, summary.get_zoom_layer(0, "a").num_tiles());
        assert_eq!(1, summary.get_zoom_layer(1, "a").num_tiles());
        assert_eq!(2, summary.get_zoom_layer(1, "b").num_tiles());
        assert_eq!(1, summary.get_zoom_layer(1, "c").num_tiles());

        assert_eq!(1, summary.get_layer("a").max_size());
        assert_eq!(6, summary.get_layer("b").max_size());
        assert_eq!(6, summary.get_layer("c").max_size());
        assert_eq!(1, summary.get_layer("a").num_tiles());
        assert_eq!(2, summary.get_layer("b").num_tiles());
        assert_eq!(1, summary.get_layer("c").num_tiles());

        assert_eq!(7, summary.get().max_size());
        assert_eq!(2, summary.get().num_tiles());

        updater1.record_tile(TileCoord::of_xyz(0, 0, 2), 0, &[ls("c", 10, 7, 7, 8, 9, 10)]);
        let to_string = |n: f64| (n as i64).to_string();
        assert_eq!(
            [
                "            z1    z2   all",
                "        a    1     0     1",
                "        b    6     0     6",
                "        c    6    10    10",
            ]
            .join("\n"),
            tile_stats.summary().format_table(to_string, |cell| cell.max_size() as f64)
        );
        assert_eq!(
            [
                "            z1    z2   all",
                "        a    1     0     1",
                "        b    2     0     2",
                "        c    1     1     2",
            ]
            .join("\n"),
            tile_stats.summary().format_table(to_string, |cell| cell.num_tiles() as f64)
        );
    }

    #[test]
    fn top_gzipped_tiles() {
        let tile_stats = TilesetSummaryStatistics::new();
        let updater1 = tile_stats.thread_local_updater();
        let updater2 = tile_stats.thread_local_updater();
        for i in 0..20 {
            (if i % 2 == 0 { &updater1 } else { &updater2 }).record_tile(TileCoord::decode(i), i as i64, &[]);
        }
        let expected: Vec<TileSummary> = (10..20)
            .rev()
            .map(|i| TileSummary { coord: TileCoord::decode(i), archived_size: i as i64, layers: vec![] })
            .collect();
        assert_eq!(expected, tile_stats.summary().get().biggest_tiles());
    }

    #[test]
    fn top_layer_tiles() {
        let tile_stats = TilesetSummaryStatistics::new();
        let updater1 = tile_stats.thread_local_updater();
        let updater2 = tile_stats.thread_local_updater();
        let mut summaries: Vec<TileSummary> = Vec::new();
        for i in 0..20i64 {
            let summary = TileSummary {
                coord: TileCoord::decode(i as u64),
                archived_size: i,
                layers: vec![ls("a", i * 2, i, i * 2, 0, 0, 0), ls("b", i * 3, i, i * 2, 0, 0, 0)],
            };
            (if i % 2 == 0 { &updater1 } else { &updater2 }).record_tile(
                summary.coord,
                summary.archived_size,
                &summary.layers,
            );
            summaries.insert(0, summary);
        }
        assert_eq!(
            summaries.iter().map(|d| d.with_size(d.coord.encoded as i64 * 2)).take(10).collect::<Vec<_>>(),
            tile_stats.summary().get_layer("a").biggest_tiles()
        );
        assert_eq!(
            summaries.iter().map(|d| d.with_size(d.coord.encoded as i64 * 3)).take(10).collect::<Vec<_>>(),
            tile_stats.summary().get_layer("b").biggest_tiles()
        );
        assert_eq!(
            "1. 2/3/1 (19) 2.5/33.25663/135 (b:57)
2. 2/3/2 (18) 2.5/-33.25663/135 (b:54)
3. 2/3/3 (17) 2.5/-75.78219/135 (b:51)
4. 2/2/0 (16) 2.5/75.78219/45 (b:48)
5. 2/2/1 (15) 2.5/33.25663/45 (b:45)
6. 2/2/2 (14) 2.5/-33.25663/45 (b:42)
7. 2/2/3 (13) 2.5/-75.78219/45 (b:39)
8. 2/1/0 (12) 2.5/75.78219/-45 (b:36)
9. 2/1/1 (11) 2.5/33.25663/-45 (b:33)
10. 2/1/2 (10) 2.5/-33.25663/-45 (b:30)",
            tile_stats.summary().get().format_biggest_tiles("{z}/{lat}/{lon}")
        );
    }

    #[test]
    fn tile_weights() {
        let tile_stats = TilesetSummaryStatistics::with_weights(Arc::new(
            TestWeights::default().put(TileCoord::of_xyz(0, 0, 0), 2).put(TileCoord::of_xyz(0, 0, 1), 1),
        ));
        let updater1 = tile_stats.thread_local_updater();
        let updater2 = tile_stats.thread_local_updater();
        updater1.record_tile(TileCoord::of_xyz(0, 0, 0), 100, &[ls("a", 10, 0, 0, 0, 0, 0)]);
        updater2.record_tile(
            TileCoord::of_xyz(0, 0, 1),
            200,
            &[ls("a", 20, 0, 0, 0, 0, 0), ls("b", 30, 0, 0, 0, 0, 0)],
        );
        // no stats
        updater2.record_tile(TileCoord::of_xyz(0, 0, 2), 400, &[ls("c", 40, 0, 0, 0, 0, 0)]);

        assert_eq!((100.0 * 2.0 + 200.0) / 3.0, tile_stats.summary().get().weighted_average_archived_size());
        assert_eq!((10.0 * 2.0 + 20.0) / 3.0, tile_stats.summary().get_layer("a").weighted_average_size());
        assert_eq!(30.0, tile_stats.summary().get_layer("b").weighted_average_size());
        assert_eq!(40.0, tile_stats.summary().get_layer("c").weighted_average_size());
    }

    #[test]
    fn tile_weights_scaled_by_zoom() {
        let tile_stats = TilesetSummaryStatistics::with_weights(Arc::new(
            TestWeights::default()
                .put(TileCoord::of_xyz(0, 0, 0), 90)
                .put(TileCoord::of_xyz(0, 0, 1), 8)
                .put(TileCoord::of_xyz(1, 0, 1), 2)
                .put(TileCoord::of_xyz(1, 0, 2), 50),
        ));
        let updater1 = tile_stats.thread_local_updater();
        let updater2 = tile_stats.thread_local_updater();
        updater1.record_tile(TileCoord::of_xyz(0, 0, 0), 100, &[ls("a", 10, 0, 0, 0, 0, 0)]);
        updater2.record_tile(
            TileCoord::of_xyz(0, 0, 1),
            200,
            &[ls("a", 20, 0, 0, 0, 0, 0), ls("b", 30, 0, 0, 0, 0, 0)],
        );

        let s = tile_stats.summary();
        assert_abs_diff_eq!(0.9 * 100.0 + 0.1 * 200.0, s.get().weighted_average_archived_size(), epsilon = 1.5);
        assert_abs_diff_eq!(0.9 * 10.0 + 0.1 * 50.0, s.get().weighted_average_size(), epsilon = 0.2);
        assert_abs_diff_eq!(0.9 * 10.0 + 0.1 * 20.0, s.get_layer("a").weighted_average_size(), epsilon = 0.2);
        assert_abs_diff_eq!(30.0, s.get_layer("b").weighted_average_size(), epsilon = 1e-5);
        assert_abs_diff_eq!(200.0, s.get_zoom(1).weighted_average_archived_size(), epsilon = 1e-5);
        assert_abs_diff_eq!(50.0, s.get_zoom(1).weighted_average_size(), epsilon = 1e-5);
    }

    #[test]
    fn storage_format() {
        assert_eq!("999", format::storage(999.0, false));
        assert_eq!("1.2k", format::storage(1234.0, false));
        assert_eq!("12k", format::storage(12345.0, false));
        assert_eq!("<1", format::storage(0.5, false));
        assert_eq!("  2M", format::storage(2_000_000.0, true));
        assert_eq!("1,234,567", format::integer(1_234_567));
    }

    #[test]
    fn print_stats_does_not_panic() {
        let tile_stats = TilesetSummaryStatistics::new();
        tile_stats.thread_local_updater().record_tile(
            TileCoord::of_xyz(0, 0, 1),
            200_000,
            &[ls("a", 300_000, 0, 0, 0, 0, 0)],
        );
        tile_stats.print_stats("{z}/{lat}/{lon}");
    }
}
