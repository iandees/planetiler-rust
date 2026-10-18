//! Port of `util/CompareArchives.java`: compares the contents of two tile archives.
//!
//! To run: `planetiler compare [options] {path/to/archive1} {path/to/archive2}`
//!
//! Options: `--verbose` (log every differing feature), `--ignore-ids` (don't compare feature IDs), `--examples=N`
//! (example differing features to log per layer, default 5), `--strict` (default true: also fail on archive-level
//! differences such as metadata).
//!
//! Difference from Java: tiles with identical bytes (or identical decompressed bytes) are skipped without decoding.
//! Other tiles are decoded and compared as a *normalized multiset of features* per layer instead of command by
//! command (the check used to compare this port's output against Java planetiler):
//!
//! - layers are paired by name, so the order of layers in a tile doesn't matter;
//! - features in a layer are compared as a multiset keyed on `(geometry type, normalized geometry, tags[, id])`, so
//!   feature order doesn't matter;
//! - tags are resolved through the layer key/value tables and compared as a map (table order and tag order don't
//!   matter; integers compare by value whatever their encoding);
//! - geometries are decoded to absolute tile coordinates (so different command streams for the same geometry are
//!   equal); points of a multipoint and lines of a multilinestring are sorted; polygons are split into polygons by
//!   ring orientation (exterior rings have positive MVT area), every ring is rotated to start at its smallest vertex
//!   (orientation is kept, it is meaningful in MVT), holes are sorted within their polygon and polygons are sorted;
//! - feature IDs are part of the key unless `--ignore-ids`; differing features that match once the ID is dropped are
//!   reported separately as "id only".
//!
//! The result reports, per layer, how many features are only in archive 1 / only in archive 2, per zoom, plus a few
//! examples. Tiles whose bytes differ but whose normalized contents are equal are counted in
//! [`CompareResult::equivalent_tiles`].

use std::collections::{BTreeMap, HashSet};
use std::fmt::{Debug, Write as _};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use prost::Message;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::archive::tile::Tile;
use crate::archive::tile_archive_config::{Format, TileArchiveConfig};
use crate::archive::tile_archives;
use crate::archive::tile_compression::TileCompression;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::geo::tile_coord::TileCoord;
use crate::pmtiles::readable_pmtiles::ReadablePmtiles;
use crate::proto::vector_tile as vt;
use crate::util::gzip::gunzip;

/// A comparison failure that prevents comparing the whole archives.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct FatalComparisonFailure(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CompareResult {
    pub total: u64,
    pub tile_diffs: u64,
    /// Tiles whose bytes differ but whose decoded contents are the same (for example a different order of the layer
    /// key/value tables). These don't count as tile diffs.
    pub equivalent_tiles: u64,
    pub archive_diffs: Vec<String>,
    pub tile_diff_types: BTreeMap<String, u64>,
    /// Layer-level issues other than differing features (`version`, `extent`): number of tiles per issue.
    pub tile_diffs_by_layer: BTreeMap<String, BTreeMap<String, u64>>,
    /// Differing features per layer (after normalization).
    pub feature_diffs: BTreeMap<String, LayerFeatureDiffs>,
}

impl CompareResult {
    /// Total number of features only in archive 1 and only in archive 2 over all layers.
    pub fn differing_features(&self) -> (u64, u64) {
        self.feature_diffs.values().fold((0, 0), |(a, b), d| (a + d.only_in_1, b + d.only_in_2))
    }
}

/// Features of one layer that have no equal (normalized) feature in the same tile of the other archive.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct LayerFeatureDiffs {
    pub only_in_1: u64,
    pub only_in_2: u64,
    /// Pairs of differing features (one from each side, same tile) that are equal except for the feature ID. Always 0
    /// with `ignore_ids`.
    pub id_only: u64,
    /// Number of tiles where this layer has a differing feature.
    pub tiles: u64,
    /// `zoom -> (only_in_1, only_in_2)`.
    pub by_zoom: BTreeMap<u32, (u64, u64)>,
    /// Up to [`CompareOptions::examples`] descriptions of differing features from the first tiles (in tile order).
    pub examples: Vec<(TileCoord, String)>,
}

/// Options for [`compare`].
#[derive(Clone, Debug)]
pub struct CompareOptions {
    /// Log each tile diff and each differing feature.
    pub verbose: bool,
    /// Don't compare feature IDs.
    pub ignore_ids: bool,
    /// Example differing features to keep per layer.
    pub examples: usize,
}

impl Default for CompareOptions {
    fn default() -> Self {
        CompareOptions { verbose: false, ignore_ids: false, examples: 5 }
    }
}

struct CompareArchives<'a> {
    diff_types: Mutex<BTreeMap<String, u64>>,
    diffs_by_layer: Mutex<BTreeMap<String, BTreeMap<String, u64>>>,
    feature_diffs: Mutex<BTreeMap<String, LayerFeatureDiffs>>,
    archive_diffs: Mutex<Vec<String>>,
    input1: &'a TileArchiveConfig,
    input2: &'a TileArchiveConfig,
    options: &'a CompareOptions,
}

/// Compares two archives.
///
/// Returns a [`FatalComparisonFailure`] error if a comparison failure is encountered that prevents comparing the
/// whole archives.
pub fn compare(
    archive_config1: &TileArchiveConfig,
    archive_config2: &TileArchiveConfig,
    config: &PlanetilerConfig,
    options: &CompareOptions,
) -> anyhow::Result<CompareResult> {
    CompareArchives {
        diff_types: Mutex::new(BTreeMap::new()),
        diffs_by_layer: Mutex::new(BTreeMap::new()),
        feature_diffs: Mutex::new(BTreeMap::new()),
        archive_diffs: Mutex::new(Vec::new()),
        input1: archive_config1,
        input2: archive_config2,
        options,
    }
    .get_result(config)
}

/// Formats `zoom -> (a, b)` as `z13 578/578, z14 1/1`.
fn format_zooms(by_zoom: &BTreeMap<u32, (u64, u64)>) -> String {
    by_zoom.iter().map(|(z, (a, b))| format!("z{z} {a}/{b}")).collect::<Vec<_>>().join(", ")
}

/// Logs a comparison result and returns the process exit code.
pub fn log_result(result: &CompareResult, strict: bool) -> i32 {
    if !result.tile_diffs_by_layer.is_empty() {
        info!("Layer diffs (tiles):");
    }
    for (layer, diffs) in &result.tile_diffs_by_layer {
        info!("  \"{layer}\" layer");
        let mut entries: Vec<_> = diffs.iter().collect();
        entries.sort_by_key(|(_, v)| **v);
        for (k, v) in entries {
            info!("    {k}: {v}");
        }
    }
    if !result.tile_diff_types.is_empty() {
        info!("Tile diffs by type:");
    }
    let mut entries: Vec<_> = result.tile_diff_types.iter().collect();
    entries.sort_by_key(|(_, v)| **v);
    for (k, v) in entries {
        info!("  {k}: {v}");
    }
    if !result.archive_diffs.is_empty() {
        info!("Archive diffs:");
    }
    for diff in &result.archive_diffs {
        info!("  {diff}");
    }
    if !result.feature_diffs.is_empty() {
        info!("Examples of differing features:");
        for (layer, d) in &result.feature_diffs {
            for (coord, example) in &d.examples {
                info!("  {layer} {}/{}/{} {example}", coord.z, coord.x, coord.y);
            }
        }
        info!("Differing features per layer (only in archive 1 / only in archive 2):");
        let width = result.feature_diffs.keys().map(String::len).max().unwrap_or(0).max(5);
        info!("  {:width$} {:>9} {:>9} {:>9} {:>7}  by zoom", "layer", "only in 1", "only in 2", "id only", "tiles");
        for (layer, d) in &result.feature_diffs {
            info!(
                "  {layer:width$} {:>9} {:>9} {:>9} {:>7}  {}",
                d.only_in_1,
                d.only_in_2,
                d.id_only,
                d.tiles,
                format_zooms(&d.by_zoom)
            );
        }
    }
    let (only1, only2) = result.differing_features();
    info!("Total tiles: {}", result.total);
    if result.equivalent_tiles > 0 {
        info!("Tiles with different bytes but equivalent normalized contents: {}", result.equivalent_tiles);
    }
    info!(
        "Tile diffs: {} ({:.1}% of all tiles)",
        result.tile_diffs,
        result.tile_diffs as f64 * 100.0 / result.total.max(1) as f64
    );
    info!("Differing features: {only1} only in archive 1, {only2} only in archive 2");
    if result.tile_diffs > 0 || (strict && !result.archive_diffs.is_empty()) { 1 } else { 0 }
}

/// `CompareArchives.main`: `compare [options] {path/to/archive1} {path/to/archive2}`. Returns the process exit code.
pub fn main<S: AsRef<str>>(args: &[S]) -> anyhow::Result<i32> {
    if args.len() < 2 {
        eprintln!("Usage: compare [options] {{path/to/archive1}} {{path/to/archive2}}");
        return Ok(1);
    }
    // last 2 args are paths to the archives
    let input1 = TileArchiveConfig::from(args[args.len() - 2].as_ref())?;
    let input2 = TileArchiveConfig::from(args[args.len() - 1].as_ref())?;
    let arguments = crate::config::arguments::Arguments::from_args_or_config_file(&args[..args.len() - 2])?;
    let options = CompareOptions {
        verbose: arguments.get_boolean("verbose", "log each tile diff and differing feature", false),
        ignore_ids: arguments.get_boolean("ignore_ids", "don't compare feature IDs", false),
        examples: arguments.get_integer("examples", "example differing features to log per layer", 5)?.max(0) as usize,
    };
    let strict = arguments.get_boolean("strict", "set to false to only fail on tile diffs", true);
    let config = PlanetilerConfig::from(&arguments)?;
    info!("archive 1: {}", input1.uri);
    info!("archive 2: {}", input2.uri);
    match compare(&input1, &input2, &config, &options) {
        Ok(result) => Ok(log_result(&result, strict)),
        Err(e) if e.is::<FatalComparisonFailure>() => {
            tracing::error!("Error comparing archives {e}");
            Ok(1)
        }
        Err(e) => Err(e),
    }
}

struct Diff {
    a: Option<Tile>,
    b: Option<Tile>,
}

fn decompress(bytes: &[u8], compression: TileCompression) -> anyhow::Result<Vec<u8>> {
    match compression {
        TileCompression::Gzip => Ok(gunzip(bytes)?),
        TileCompression::None => Ok(bytes.to_vec()),
        TileCompression::Unknown => Err(FatalComparisonFailure("Unknown compression".into()).into()),
    }
}

fn decode(decompressed: &[u8]) -> anyhow::Result<vt::Tile> {
    Ok(vt::Tile::decode(decompressed)?)
}

impl CompareArchives<'_> {
    fn get_result(&self, config: &PlanetilerConfig) -> anyhow::Result<CompareResult> {
        self.compare_archive("format", &self.input1.format, &self.input2.format);
        let (compression1, compression2) = {
            let reader1 = tile_archives::new_reader(self.input1, config)?;
            let reader2 = tile_archives::new_reader(self.input2, config)?;
            let metadata1 = reader1.metadata()?;
            let metadata2 = reader2.metadata()?;
            self.compare_archive("metadata", &metadata1, &metadata2);
            if self.input1.format == Format::Pmtiles && self.input2.format == Format::Pmtiles {
                let header1 = ReadablePmtiles::new_read_from_file(&self.local_path(self.input1)?)?.header().clone();
                let header2 = ReadablePmtiles::new_read_from_file(&self.local_path(self.input2)?)?.header().clone();
                self.compare_archive("pmtiles header", &header1, &header2);
            }
            let c1 = metadata1.and_then(|m| m.tile_compression).unwrap_or(TileCompression::Unknown);
            let c2 = metadata2.and_then(|m| m.tile_compression).unwrap_or(TileCompression::Unknown);
            if !self.compare_archive("tile compression", &c1, &c2) {
                warn!("Will compare decompressed tile contents instead");
            }
            (c1, c2)
        };

        let order = self.input1.format.preferred_order();
        let order2 = self.input2.format.preferred_order();
        if order != order2 {
            return Err(FatalComparisonFailure(format!(
                "Archive orders must be the same to compare, got {order} and {order2}"
            ))
            .into());
        }
        let total = AtomicU64::new(0);
        let diffs = AtomicU64::new(0);
        let equivalent = AtomicU64::new(0);
        let threads = config.feature_process_threads.max(1);
        let (tx, rx) = crossbeam_channel::bounded::<Vec<Diff>>(50);

        std::thread::scope(|scope| -> anyhow::Result<()> {
            let producer = scope.spawn(move || -> anyhow::Result<()> {
                let reader1 = tile_archives::new_reader(self.input1, config)?;
                let reader2 = tile_archives::new_reader(self.input2, config)?;
                let mut tiles1 = reader1.get_all_tiles()?;
                let mut tiles2 = reader2.get_all_tiles()?;
                let mut tile1 = tiles1.next().transpose()?;
                let mut tile2 = tiles2.next().transpose()?;
                let mut batch = Vec::with_capacity(1_000);
                while tile1.is_some() || tile2.is_some() {
                    let diff = match (&tile1, &tile2) {
                        (None, Some(_)) => Diff { a: None, b: tile2.take() },
                        (Some(_), None) => Diff { a: tile1.take(), b: None },
                        (Some(t1), Some(t2)) => {
                            if t1.coord == t2.coord {
                                Diff { a: tile1.take(), b: tile2.take() }
                            } else if order.encode(&t1.coord) < order.encode(&t2.coord) {
                                Diff { a: tile1.take(), b: None }
                            } else {
                                Diff { a: None, b: tile2.take() }
                            }
                        }
                        (None, None) => unreachable!(),
                    };
                    if tile1.is_none() {
                        tile1 = tiles1.next().transpose()?;
                    }
                    if tile2.is_none() {
                        tile2 = tiles2.next().transpose()?;
                    }
                    batch.push(diff);
                    if batch.len() >= 1_000 && tx.send(std::mem::take(&mut batch)).is_err() {
                        return Ok(());
                    }
                }
                if !batch.is_empty() {
                    let _ = tx.send(batch);
                }
                Ok(())
            });
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    let rx = rx.clone();
                    let (total, diffs, equivalent) = (&total, &diffs, &equivalent);
                    scope.spawn(move || -> anyhow::Result<()> {
                        let same_compression = compression1 == compression2;
                        for batch in rx {
                            for diff in batch {
                                total.fetch_add(1, Ordering::Relaxed);
                                match (diff.a, diff.b) {
                                    (None, Some(b)) => {
                                        self.record_tile_diff(&b.coord, "archive 1 missing tile");
                                        diffs.fetch_add(1, Ordering::Relaxed);
                                    }
                                    (Some(a), None) => {
                                        self.record_tile_diff(&a.coord, "archive 2 missing tile");
                                        diffs.fetch_add(1, Ordering::Relaxed);
                                    }
                                    (Some(a), Some(b)) => {
                                        let (kind, tile1, tile2) = if same_compression {
                                            if a.bytes == b.bytes {
                                                continue;
                                            }
                                            (
                                                "different contents",
                                                decode(&decompress(&a.bytes, compression1)?)?,
                                                decode(&decompress(&b.bytes, compression2)?)?,
                                            )
                                        } else {
                                            let decompressed1 = decompress(&a.bytes, compression1)?;
                                            let decompressed2 = decompress(&b.bytes, compression2)?;
                                            if decompressed1 == decompressed2 {
                                                continue;
                                            }
                                            (
                                                "different decompressed contents",
                                                decode(&decompressed1)?,
                                                decode(&decompressed2)?,
                                            )
                                        };
                                        let issues = compare_tiles(&tile1, &tile2, self.options.ignore_ids);
                                        if issues.is_empty() {
                                            equivalent.fetch_add(1, Ordering::Relaxed);
                                        } else {
                                            self.record_tile_diff(&a.coord, kind);
                                            diffs.fetch_add(1, Ordering::Relaxed);
                                            self.record_issues(&a.coord, issues);
                                        }
                                    }
                                    (None, None) => {}
                                }
                            }
                        }
                        Ok(())
                    })
                })
                .collect();
            drop(rx);
            let mut first_error = None;
            for worker in workers {
                if let Err(e) = worker.join().expect("compare worker panicked") {
                    first_error.get_or_insert(e);
                }
            }
            if let Err(e) = producer.join().expect("compare producer panicked") {
                first_error.get_or_insert(e);
            }
            match first_error {
                Some(e) => Err(e),
                None => Ok(()),
            }
        })?;

        // tiles with equivalent contents but different bytes guarantee a byte mismatch, so skip that check
        if self.archive_diffs.lock().is_empty()
            && diffs.load(Ordering::Relaxed) == 0
            && equivalent.load(Ordering::Relaxed) == 0
        {
            let path1 = self.input1.get_local_path();
            let path2 = self.input2.get_local_path();
            if let (Some(path1), Some(path2)) = (path1, path2)
                && path1.is_file()
                && path2.is_file()
            {
                info!("No diffs so far, comparing bytes in {} vs. {}", path1.display(), path2.display());
                self.compare_files(&path1, &path2)?;
            }
        }
        Ok(CompareResult {
            total: total.load(Ordering::Relaxed),
            tile_diffs: diffs.load(Ordering::Relaxed),
            equivalent_tiles: equivalent.load(Ordering::Relaxed),
            archive_diffs: self.archive_diffs.lock().clone(),
            tile_diff_types: self.diff_types.lock().clone(),
            tile_diffs_by_layer: self.diffs_by_layer.lock().clone(),
            feature_diffs: std::mem::take(&mut *self.feature_diffs.lock()),
        })
    }

    fn local_path(&self, config: &TileArchiveConfig) -> anyhow::Result<std::path::PathBuf> {
        config.get_local_path().ok_or_else(|| anyhow::anyhow!("{} is not local", config.uri))
    }

    fn compare_files(&self, path1: &Path, path2: &Path) -> anyhow::Result<()> {
        let size = std::fs::metadata(path1)?.len();
        if self.compare_archive("archive size", &size, &std::fs::metadata(path2)?.len()) {
            let mut is1 = std::io::BufReader::new(std::fs::File::open(path1)?);
            let mut is2 = std::io::BufReader::new(std::fs::File::open(path2)?);
            let mut bytes1 = vec![0u8; 8192];
            let mut bytes2 = vec![0u8; 8192];
            let mut n = 0u64;
            while n < size {
                let len = is1.read(&mut bytes1)?;
                if len == 0 {
                    break;
                }
                let mut len2 = 0;
                while len2 < len {
                    let r = is2.read(&mut bytes2[len2..len])?;
                    if r == 0 {
                        break;
                    }
                    len2 += r;
                }
                if len2 != len {
                    let message = format!("Expected to read {len} bytes from {} but got {len2}", path2.display());
                    warn!("{message}");
                    self.archive_diffs.lock().push(message);
                    return Ok(());
                }
                if let Some(mismatch) = (0..len).find(|&i| bytes1[i] != bytes2[i]) {
                    self.archive_diffs.lock().push(format!("mismatch at byte {}", mismatch as u64 + n));
                    warn!("Archives mismatch ay byte {}", mismatch as u64 + n);
                    return Ok(());
                }
                n += len as u64;
            }
            info!("No mismatches! Analyzed {n} / {size} bytes");
        }
        Ok(())
    }

    fn compare_archive<T: PartialEq + Debug>(&self, name: &str, a: &T, b: &T) -> bool {
        if a == b {
            return true;
        }
        warn!("archive1 and archive2 have different {name}\narchive1: {a:?}\narchive2: {b:?}");
        self.archive_diffs.lock().push(name.to_string());
        false
    }

    fn record_issues(&self, coord: &TileCoord, issues: TileIssues) {
        for issue in issues.tile {
            self.record_tile_diff(coord, &issue);
        }
        for (layer, issue) in issues.layer {
            self.record_layer_diff(coord, &layer, &issue);
        }
        if issues.features.is_empty() {
            return;
        }
        let max_examples = self.options.examples;
        let mut all = self.feature_diffs.lock();
        for (layer, d) in issues.features {
            if self.options.verbose {
                for f in d.only_in_1.iter().map(|f| ("1", f)).chain(d.only_in_2.iter().map(|f| ("2", f))) {
                    debug!("{coord} layer {layer} only in archive {}: {}", f.0, f.1.describe());
                }
            }
            let entry = all.entry(layer).or_default();
            let (n1, n2) = (d.only_in_1.len() as u64, d.only_in_2.len() as u64);
            entry.only_in_1 += n1;
            entry.only_in_2 += n2;
            entry.id_only += d.id_only;
            entry.tiles += 1;
            let zoom = entry.by_zoom.entry(coord.z).or_default();
            zoom.0 += n1;
            zoom.1 += n2;
            // keep the examples from the first tiles in tile order so the output is deterministic
            if max_examples > 0
                && (entry.examples.len() < max_examples || entry.examples.last().is_some_and(|(c, _)| coord < c))
            {
                // alternate sides so a tile with many differences shows both versions
                for i in 0..d.only_in_1.len().max(d.only_in_2.len()).min(max_examples) {
                    for (n, fs) in [("1", &d.only_in_1), ("2", &d.only_in_2)] {
                        if let Some(f) = fs.get(i) {
                            entry.examples.push((*coord, format!("only in {n}: {}", f.describe())));
                        }
                    }
                }
                entry.examples.sort_by_key(|(c, _)| *c);
                entry.examples.truncate(max_examples);
            }
        }
    }

    fn record_layer_diff(&self, coord: &TileCoord, layer: &str, issue: &str) {
        *self.diffs_by_layer.lock().entry(layer.to_string()).or_default().entry(issue.to_string()).or_default() += 1;
        if self.options.verbose {
            debug!("{coord} layer {layer} {issue}");
        }
    }

    fn record_tile_diff(&self, coord: &TileCoord, issue: &str) {
        *self.diff_types.lock().entry(issue.to_string()).or_default() += 1;
        if self.options.verbose {
            debug!("{coord} {issue}");
        }
    }
}

/// Differences found between the decoded contents of two tiles.
#[derive(Default)]
struct TileIssues {
    /// Tile-level issues (missing layers).
    tile: Vec<String>,
    /// `(layer, issue)` pairs for layer-level differences other than features (`version`, `extent`).
    layer: Vec<(String, String)>,
    /// `(layer, differing features)` for layers with differing features.
    features: Vec<(String, LayerDiff)>,
}

impl TileIssues {
    fn is_empty(&self) -> bool {
        self.tile.is_empty() && self.layer.is_empty() && self.features.is_empty()
    }

    /// Records a layer diff if `value1 != value2`.
    fn compare_values<T: PartialEq + ?Sized>(&mut self, layer: &str, name: &str, value1: &T, value2: &T) {
        if value1 != value2 {
            self.layer.push((layer.to_string(), name.to_string()));
        }
    }
}

/// The differing features of one layer in one tile.
struct LayerDiff {
    only_in_1: Vec<FeatureKey>,
    only_in_2: Vec<FeatureKey>,
    id_only: u64,
}

/// A decoded vector tile value. Integers are compared by value regardless of whether they were encoded as `int`,
/// `uint` or `sint` (Java decodes all three to a `Long`); floats compare by bits.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum DecodedValue {
    String(String),
    Float(u32),
    Double(u64),
    Int(i128),
    Bool(bool),
    Null,
}

impl From<&vt::tile::Value> for DecodedValue {
    fn from(v: &vt::tile::Value) -> Self {
        if let Some(s) = &v.string_value {
            DecodedValue::String(s.clone())
        } else if let Some(f) = v.float_value {
            DecodedValue::Float(f.to_bits())
        } else if let Some(d) = v.double_value {
            DecodedValue::Double(d.to_bits())
        } else if let Some(i) = v.int_value {
            DecodedValue::Int(i as i128)
        } else if let Some(u) = v.uint_value {
            DecodedValue::Int(u as i128)
        } else if let Some(s) = v.sint_value {
            DecodedValue::Int(s as i128)
        } else if let Some(b) = v.bool_value {
            DecodedValue::Bool(b)
        } else {
            DecodedValue::Null
        }
    }
}

impl std::fmt::Display for DecodedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodedValue::String(s) => write!(f, "{s:?}"),
            DecodedValue::Float(b) => write!(f, "{}", f32::from_bits(*b)),
            DecodedValue::Double(b) => write!(f, "{}", f64::from_bits(*b)),
            DecodedValue::Int(i) => write!(f, "{i}"),
            DecodedValue::Bool(b) => write!(f, "{b}"),
            DecodedValue::Null => f.write_str("null"),
        }
    }
}

type DecodedTags = Vec<(Option<String>, Option<DecodedValue>)>;

/// The tags of `feature` resolved through the layer's key/value tables and sorted, so features compare equal
/// regardless of how the tables or the tags are ordered. Out-of-range indices decode to `None`.
fn decoded_tags(layer: &vt::tile::Layer, feature: &vt::tile::Feature) -> DecodedTags {
    let mut tags: DecodedTags = feature
        .tags
        .chunks(2)
        .map(|pair| {
            let key = layer.keys.get(pair[0] as usize).cloned();
            let value = pair.get(1).and_then(|v| layer.values.get(*v as usize)).map(DecodedValue::from);
            (key, value)
        })
        .collect();
    tags.sort();
    tags
}

type Point = (i64, i64);

/// A geometry in a canonical form: absolute tile coordinates, independent of ring start points, ring order, and the
/// order of the parts of multi-geometries.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum NormGeometry {
    /// Sorted points.
    Points(Vec<Point>),
    /// Sorted lines (each line keeps its direction).
    Lines(Vec<Vec<Point>>),
    /// Sorted polygons, each `[exterior, holes...]` with holes sorted and every ring rotated to start at its smallest
    /// vertex (orientation is kept).
    Polygons(Vec<Vec<Vec<Point>>>),
    /// Unknown geometry type or a command stream that doesn't decode: compared as raw commands.
    Raw(Vec<u32>),
}

fn zigzag(v: u32) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Decodes an MVT command stream into its parts (`MoveTo` starts a part), or `None` if it is malformed.
fn decode_parts(commands: &[u32]) -> Option<Vec<Vec<Point>>> {
    let mut parts: Vec<Vec<Point>> = Vec::new();
    let (mut x, mut y) = (0i64, 0i64);
    let mut i = 0;
    while i < commands.len() {
        let (id, count) = (commands[i] & 7, (commands[i] >> 3) as usize);
        i += 1;
        match id {
            1 | 2 => {
                let coords = commands.get(i..i + count * 2)?;
                i += count * 2;
                for &[dx, dy] in coords.as_chunks::<2>().0 {
                    x += zigzag(dx);
                    y += zigzag(dy);
                    if id == 1 {
                        parts.push(vec![(x, y)]);
                    } else {
                        parts.last_mut()?.push((x, y));
                    }
                }
            }
            7 => {}
            _ => return None,
        }
    }
    Some(parts)
}

/// Twice the signed area of a ring in MVT tile coordinates (y down): positive for exterior rings.
fn signed_area2(ring: &[Point]) -> i128 {
    let n = ring.len();
    (0..n)
        .map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % n]);
            a.0 as i128 * b.1 as i128 - b.0 as i128 * a.1 as i128
        })
        .sum()
}

/// Rotates a (implicitly closed) ring to its lexicographically smallest rotation that starts at its smallest vertex.
fn rotate_ring(ring: &mut Vec<Point>) {
    // an explicit closing point would make rotations unequal
    if ring.len() > 1 && ring.first() == ring.last() {
        ring.pop();
    }
    let n = ring.len();
    let Some(&min) = ring.iter().min() else { return };
    let best = {
        let r: &[Point] = ring;
        let rotation = |start: usize| (0..n).map(move |k| r[(start + k) % n]);
        let mut best: Option<usize> = None;
        for start in (0..n).filter(|&i| r[i] == min) {
            best = match best {
                Some(b) if !rotation(start).lt(rotation(b)) => Some(b),
                _ => Some(start),
            };
        }
        best.unwrap_or(0)
    };
    ring.rotate_left(best);
}

fn normalize_geometry(geom_type: i32, commands: &[u32]) -> NormGeometry {
    let Some(mut parts) = decode_parts(commands) else { return NormGeometry::Raw(commands.to_vec()) };
    match vt::tile::GeomType::try_from(geom_type) {
        Ok(vt::tile::GeomType::Point) => {
            let mut points: Vec<Point> = parts.into_iter().flatten().collect();
            points.sort_unstable();
            NormGeometry::Points(points)
        }
        Ok(vt::tile::GeomType::Linestring) => {
            parts.sort_unstable();
            NormGeometry::Lines(parts)
        }
        Ok(vt::tile::GeomType::Polygon) => {
            let mut polygons: Vec<Vec<Vec<Point>>> = Vec::new();
            for mut ring in parts {
                let exterior = signed_area2(&ring) > 0;
                rotate_ring(&mut ring);
                match polygons.last_mut() {
                    Some(polygon) if !exterior => polygon.push(ring),
                    _ => polygons.push(vec![ring]),
                }
            }
            for polygon in &mut polygons {
                polygon[1..].sort_unstable();
            }
            polygons.sort_unstable();
            NormGeometry::Polygons(polygons)
        }
        _ => NormGeometry::Raw(commands.to_vec()),
    }
}

/// The normalized identity of a feature: two features are the same if their keys are equal. Ordered by tags first so
/// sorted differing features from both sides line up.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct FeatureKey {
    tags: DecodedTags,
    geom_type: i32,
    id: Option<u64>,
    geometry: NormGeometry,
}

impl FeatureKey {
    fn new(layer: &vt::tile::Layer, feature: &vt::tile::Feature, ignore_ids: bool) -> FeatureKey {
        FeatureKey {
            tags: decoded_tags(layer, feature),
            geom_type: feature.r#type() as i32,
            id: if ignore_ids { None } else { Some(feature.id()) },
            geometry: normalize_geometry(feature.r#type() as i32, &feature.geometry),
        }
    }

    /// A one-line summary: type, ID, size of the geometry, and tags.
    fn describe(&self) -> String {
        let mut s = vt::tile::GeomType::try_from(self.geom_type)
            .map(|t| t.as_str_name().to_lowercase())
            .unwrap_or_else(|_| format!("type {}", self.geom_type));
        if let Some(id) = self.id {
            let _ = write!(s, " id={id}");
        }
        let _ = match &self.geometry {
            NormGeometry::Points(p) => write!(s, " {} points", p.len()),
            NormGeometry::Lines(l) => {
                write!(s, " {} lines {} points", l.len(), l.iter().map(Vec::len).sum::<usize>())
            }
            NormGeometry::Polygons(p) => write!(
                s,
                " {} polygons {} rings {} points",
                p.len(),
                p.iter().map(Vec::len).sum::<usize>(),
                p.iter().flatten().map(Vec::len).sum::<usize>()
            ),
            NormGeometry::Raw(c) => write!(s, " {} raw commands", c.len()),
        };
        let first = match &self.geometry {
            NormGeometry::Points(p) => p.first(),
            NormGeometry::Lines(l) => l.first().and_then(|l| l.first()),
            NormGeometry::Polygons(p) => p.first().and_then(|p| p.first()).and_then(|r| r.first()),
            NormGeometry::Raw(_) => None,
        };
        if let Some((x, y)) = first {
            let _ = write!(s, " first ({x} {y})");
        }
        let tags: Vec<String> = self
            .tags
            .iter()
            .map(|(k, v)| {
                let v = v.as_ref().map_or("?".to_string(), DecodedValue::to_string);
                format!("{}={v}", k.as_deref().unwrap_or("?"))
            })
            .collect();
        let _ = write!(s, " {{{}}}", tags.join(", "));
        s
    }
}

fn compare_tiles(proto1: &vt::Tile, proto2: &vt::Tile, ignore_ids: bool) -> TileIssues {
    let mut issues = TileIssues::default();
    let names1: HashSet<&str> = proto1.layers.iter().map(|l| l.name.as_str()).collect();
    let names2: HashSet<&str> = proto2.layers.iter().map(|l| l.name.as_str()).collect();
    for layer in &proto1.layers {
        if !names2.contains(layer.name.as_str()) {
            issues.tile.push(format!("tile layers 2 missing {}", layer.name));
        }
    }
    for layer in &proto2.layers {
        if !names1.contains(layer.name.as_str()) {
            issues.tile.push(format!("tile layers 1 missing {}", layer.name));
        }
    }
    // pair layers by name: the order of layers within a tile doesn't affect rendering. A layer present on one side
    // only is compared against an empty layer so its features count as differing features.
    let mut layers: BTreeMap<&str, (Vec<&vt::tile::Layer>, Vec<&vt::tile::Layer>)> = BTreeMap::new();
    for layer in &proto1.layers {
        layers.entry(&layer.name).or_default().0.push(layer);
    }
    for layer in &proto2.layers {
        layers.entry(&layer.name).or_default().1.push(layer);
    }
    for (name, (layers1, layers2)) in layers {
        if let ([layer1], [layer2]) = (&layers1[..], &layers2[..]) {
            if layer1 == layer2 {
                continue;
            }
            issues.compare_values(name, "version", &layer1.version, &layer2.version);
            issues.compare_values(name, "extent", &layer1.extent(), &layer2.extent());
        }
        if let Some(diff) = compare_layer_features(&layers1, &layers2, ignore_ids) {
            issues.features.push((name.to_string(), diff));
        }
    }
    issues
}

/// Compares the features of the layers with the same name as multisets of [`FeatureKey`]s.
fn compare_layer_features(
    layers1: &[&vt::tile::Layer],
    layers2: &[&vt::tile::Layer],
    ignore_ids: bool,
) -> Option<LayerDiff> {
    let features = |layers: &[&vt::tile::Layer]| -> Vec<FeatureKey> {
        let mut keys: Vec<FeatureKey> =
            layers.iter().flat_map(|l| l.features.iter().map(|f| FeatureKey::new(l, f, ignore_ids))).collect();
        keys.sort_unstable();
        keys
    };
    let (keys1, keys2) = (features(layers1), features(layers2));
    if keys1 == keys2 {
        return None;
    }
    let (only_in_1, only_in_2) = sorted_difference(keys1, keys2);
    // pairs of differing features that only differ in their ID
    let id_only = if ignore_ids {
        0
    } else {
        let strip = |keys: &[FeatureKey]| -> Vec<FeatureKey> {
            let mut stripped: Vec<FeatureKey> = keys.iter().map(|k| FeatureKey { id: None, ..k.clone() }).collect();
            stripped.sort_unstable();
            stripped
        };
        let (rest1, _) = sorted_difference(strip(&only_in_1), strip(&only_in_2));
        (only_in_1.len() - rest1.len()) as u64
    };
    Some(LayerDiff { only_in_1, only_in_2, id_only })
}

/// Multiset differences `(a - b, b - a)` of two sorted vectors.
fn sorted_difference<T: Ord>(a: Vec<T>, b: Vec<T>) -> (Vec<T>, Vec<T>) {
    let (mut only_a, mut only_b) = (Vec::new(), Vec::new());
    let (mut a, mut b) = (a.into_iter().peekable(), b.into_iter().peekable());
    loop {
        match (a.peek(), b.peek()) {
            (Some(x), Some(y)) => match x.cmp(y) {
                std::cmp::Ordering::Less => only_a.push(a.next().unwrap()),
                std::cmp::Ordering::Greater => only_b.push(b.next().unwrap()),
                std::cmp::Ordering::Equal => {
                    a.next();
                    b.next();
                }
            },
            (Some(_), None) => only_a.push(a.next().unwrap()),
            (None, Some(_)) => only_b.push(b.next().unwrap()),
            (None, None) => return (only_a, only_b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::tile_archive_metadata::{Coordinate, TileArchiveMetadata};
    use crate::archive::tile_encoding_result::TileEncodingResult;
    use crate::archive::tile_format::TileFormat;
    use crate::archive::writeable_tile_archive::WriteableTileArchive;
    use crate::geo::tile_order::TileOrder;
    use crate::pmtiles::writeable_pmtiles::WriteablePmtiles;
    use crate::util::gzip::gzip_zlib as gzip;

    fn metadata(compression: TileCompression) -> TileArchiveMetadata {
        let bounds = *crate::geo::geo_utils::WORLD_LAT_LON_BOUNDS;
        TileArchiveMetadata {
            name: Some("Null".into()),
            type_: Some("baselayer".into()),
            format: Some(TileFormat::Mvt),
            bounds: Some(bounds),
            center: Some(Coordinate::new(0.0, 0.0, 0.0)),
            minzoom: Some(0),
            maxzoom: Some(14),
            tile_compression: Some(compression),
            ..Default::default()
        }
    }

    fn vt_tile(value: &str, id: u64) -> Vec<u8> {
        vt::Tile {
            layers: vec![vt::tile::Layer {
                version: 2,
                name: "layer1".into(),
                features: vec![vt::tile::Feature { id: Some(id), ..Default::default() }],
                keys: vec!["key1".into()],
                values: vec![vt::tile::Value { string_value: Some(value.into()), ..Default::default() }],
                extent: None,
            }],
        }
        .encode_to_vec()
    }

    fn value(s: &str) -> vt::tile::Value {
        vt::tile::Value { string_value: Some(s.into()), ..Default::default() }
    }

    fn feature(id: u64, tags: Vec<u32>, geom_type: vt::tile::GeomType, geometry: Vec<u32>) -> vt::tile::Feature {
        vt::tile::Feature { id: Some(id), tags, r#type: Some(geom_type as i32), geometry }
    }

    fn layer(name: &str, keys: &[&str], values: &[&str], features: Vec<vt::tile::Feature>) -> vt::tile::Layer {
        vt::tile::Layer {
            version: 2,
            name: name.into(),
            features,
            keys: keys.iter().map(|k| k.to_string()).collect(),
            values: values.iter().map(|v| value(v)).collect(),
            extent: Some(4096),
        }
    }

    fn point(id: u64, tags: Vec<u32>) -> vt::tile::Feature {
        feature(id, tags, vt::tile::GeomType::Point, vec![9, 2, 2])
    }

    fn compare_tile_bytes(tile1: &vt::Tile, tile2: &vt::Tile) -> CompareResult {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.pmtiles"), tmp.path().join("b.pmtiles"));
        write(&a, &[(0, gzip(&tile1.encode_to_vec()).unwrap())], TileCompression::Gzip);
        write(&b, &[(0, gzip(&tile2.encode_to_vec()).unwrap())], TileCompression::Gzip);
        compare(&config_for(&a), &config_for(&b), &PlanetilerConfig::defaults(), &CompareOptions::default()).unwrap()
    }

    fn write(path: &Path, tiles: &[(u64, Vec<u8>)], compression: TileCompression) {
        let mut archive = WriteablePmtiles::new_write_to_file(path).unwrap();
        archive.initialize().unwrap();
        {
            let mut w = archive.new_tile_writer().unwrap();
            for (id, data) in tiles {
                w.write(TileEncodingResult::new(TileOrder::Hilbert.decode(*id), data.clone(), None)).unwrap();
            }
            w.close().unwrap();
        }
        archive.finish(&metadata(compression)).unwrap();
        archive.close().unwrap();
    }

    fn config_for(path: &Path) -> TileArchiveConfig {
        TileArchiveConfig::from(&path.to_string_lossy()).unwrap()
    }

    fn compare_tiles_with(tile1: &vt::Tile, tile2: &vt::Tile, options: &CompareOptions) -> CompareResult {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.pmtiles"), tmp.path().join("b.pmtiles"));
        write(&a, &[(0, gzip(&tile1.encode_to_vec()).unwrap())], TileCompression::Gzip);
        write(&b, &[(0, gzip(&tile2.encode_to_vec()).unwrap())], TileCompression::Gzip);
        compare(&config_for(&a), &config_for(&b), &PlanetilerConfig::defaults(), options).unwrap()
    }

    /// `(only_in_1, only_in_2, id_only)` per layer.
    fn feature_counts(result: &CompareResult) -> BTreeMap<String, (u64, u64, u64)> {
        result.feature_diffs.iter().map(|(l, d)| (l.clone(), (d.only_in_1, d.only_in_2, d.id_only))).collect()
    }

    fn counts(entries: &[(&str, (u64, u64, u64))]) -> BTreeMap<String, (u64, u64, u64)> {
        entries.iter().map(|(l, c)| (l.to_string(), *c)).collect()
    }

    fn zz(v: i32) -> u32 {
        ((v << 1) ^ (v >> 31)) as u32
    }

    /// Encodes parts given in absolute coordinates as MVT commands, with a `ClosePath` after each part if `close`.
    fn encode(parts: &[&[(i32, i32)]], close: bool) -> Vec<u32> {
        let (mut x, mut y) = (0, 0);
        let mut out = vec![];
        for part in parts {
            for (i, &(px, py)) in part.iter().enumerate() {
                match i {
                    0 => out.push(9),
                    1 => out.push(((part.len() as u32 - 1) << 3) | 2),
                    _ => {}
                }
                out.extend([zz(px - x), zz(py - y)]);
                (x, y) = (px, py);
            }
            if close {
                out.push(15);
            }
        }
        out
    }

    fn polygon_tile(features: Vec<Vec<&[(i32, i32)]>>) -> vt::Tile {
        use vt::tile::GeomType::Polygon;
        let features = features.iter().map(|rings| feature(1, vec![], Polygon, encode(rings, true))).collect();
        vt::Tile { layers: vec![layer("l", &[], &[], features)] }
    }

    // exterior rings are clockwise on screen (positive area with y down), holes counterclockwise
    const SQUARE: &[(i32, i32)] = &[(0, 0), (10, 0), (10, 10), (0, 10)];
    const SQUARE_ROTATED: &[(i32, i32)] = &[(10, 10), (0, 10), (0, 0), (10, 0)];
    const SQUARE_REVERSED: &[(i32, i32)] = &[(0, 0), (0, 10), (10, 10), (10, 0)];
    const HOLE1: &[(i32, i32)] = &[(1, 1), (1, 2), (2, 2), (2, 1)];
    const HOLE2: &[(i32, i32)] = &[(5, 5), (5, 6), (6, 6), (6, 5)];
    const HOLE2_ROTATED: &[(i32, i32)] = &[(6, 6), (6, 5), (5, 5), (5, 6)];
    const SQUARE2: &[(i32, i32)] = &[(20, 0), (30, 0), (30, 10), (20, 10)];

    #[test]
    fn test_compare_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.pmtiles"), tmp.path().join("b.pmtiles"));
        let tile1 = vt_tile("value1", 1);
        let tile2 = vt_tile("value2", 2);
        write(&a, &[(0, vec![0xa, 0x2]), (2, gzip(&tile1).unwrap()), (4, vec![0xa, 0x2])], TileCompression::Gzip);
        write(
            &b,
            &[(1, vec![0xa, 0x2]), (2, gzip(&tile2).unwrap()), (3, vec![0xa, 0x2]), (4, vec![0xa, 0x2])],
            TileCompression::Gzip,
        );
        let result =
            compare(&config_for(&a), &config_for(&b), &PlanetilerConfig::defaults(), &CompareOptions::default())
                .unwrap();
        assert_eq!(
            CompareResult {
                total: 5,
                tile_diffs: 4,
                equivalent_tiles: 0,
                archive_diffs: vec!["pmtiles header".into()],
                tile_diff_types: BTreeMap::from([
                    ("archive 2 missing tile".into(), 1),
                    ("archive 1 missing tile".into(), 2),
                    ("different contents".into(), 1),
                ]),
                tile_diffs_by_layer: BTreeMap::new(),
                feature_diffs: result.feature_diffs.clone(),
            },
            result
        );
        assert_eq!(counts(&[("layer1", (1, 1, 1))]), feature_counts(&result));
        let d = &result.feature_diffs["layer1"];
        assert_eq!((1, BTreeMap::from([(1, (1, 1))])), (d.tiles, d.by_zoom.clone()));
        assert_eq!(2, d.examples.len(), "{:?}", d.examples);
        assert!(d.examples[0].1.starts_with("only in 1: unknown id=1 0 raw commands {}"), "{d:?}");
        assert_eq!(1, log_result(&result, false));
    }

    #[test]
    fn test_compare_archives_different_compression() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.pmtiles"), tmp.path().join("b.pmtiles"));
        let a1 = vec![0xa, 0x2];
        let b1 = gzip(&a1).unwrap();
        let a2 = vt_tile("value1", 1);
        let b2 = gzip(&vt_tile("value2", 2)).unwrap();
        write(&a, &[(0, a1.clone()), (2, a2), (4, a1.clone())], TileCompression::None);
        write(&b, &[(1, b1.clone()), (2, b2), (3, b1.clone()), (4, b1)], TileCompression::Gzip);
        let result =
            compare(&config_for(&a), &config_for(&b), &PlanetilerConfig::defaults(), &CompareOptions::default())
                .unwrap();
        assert_eq!(vec!["metadata", "pmtiles header", "tile compression"], result.archive_diffs);
        assert_eq!(
            BTreeMap::from([
                ("archive 2 missing tile".into(), 1),
                ("archive 1 missing tile".into(), 2),
                ("different decompressed contents".into(), 1),
            ]),
            result.tile_diff_types
        );
        assert_eq!((5, 4), (result.total, result.tile_diffs));
        assert_eq!(counts(&[("layer1", (1, 1, 1))]), feature_counts(&result));
    }

    #[test]
    fn test_compare_archives_same() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.pmtiles"), tmp.path().join("b.pmtiles"));
        let a1 = vec![0xa, 0x2];
        write(&a, &[(0, a1.clone()), (2, a1.clone())], TileCompression::Gzip);
        write(&b, &[(0, a1.clone()), (2, a1)], TileCompression::Gzip);
        let result =
            compare(&config_for(&a), &config_for(&b), &PlanetilerConfig::defaults(), &CompareOptions::default())
                .unwrap();
        assert_eq!(CompareResult { total: 2, ..Default::default() }, result);
    }

    #[test]
    fn test_key_value_table_order_is_equivalent() {
        // same decoded tags {a: x, b: y} with the key and value tables in a different order
        let tile1 = vt::Tile { layers: vec![layer("l", &["a", "b"], &["x", "y"], vec![point(1, vec![0, 0, 1, 1])])] };
        let tile2 = vt::Tile { layers: vec![layer("l", &["b", "a"], &["y", "x"], vec![point(1, vec![1, 1, 0, 0])])] };
        let result = compare_tile_bytes(&tile1, &tile2);
        assert_eq!(0, result.tile_diffs, "{result:?}");
        assert_eq!(1, result.equivalent_tiles);
        assert!(result.tile_diff_types.is_empty(), "{result:?}");
        assert!(result.tile_diffs_by_layer.is_empty(), "{result:?}");
        assert!(result.feature_diffs.is_empty(), "{result:?}");
        assert!(!result.archive_diffs.iter().any(|d| d.starts_with("mismatch")), "{result:?}");
        assert_eq!(0, log_result(&result, false));
    }

    #[test]
    fn test_tag_order_is_equivalent() {
        // same tables, tags listed in a different order in the feature
        let tile1 = vt::Tile { layers: vec![layer("l", &["a", "b"], &["x", "y"], vec![point(1, vec![0, 0, 1, 1])])] };
        let tile2 = vt::Tile { layers: vec![layer("l", &["a", "b"], &["x", "y"], vec![point(1, vec![1, 1, 0, 0])])] };
        let result = compare_tile_bytes(&tile1, &tile2);
        assert_eq!((0, 1), (result.tile_diffs, result.equivalent_tiles), "{result:?}");
    }

    #[test]
    fn test_different_tag_value_is_a_diff() {
        let tile1 = vt::Tile { layers: vec![layer("l", &["a"], &["x"], vec![point(1, vec![0, 0])])] };
        let tile2 = vt::Tile { layers: vec![layer("l", &["a"], &["z"], vec![point(1, vec![0, 0])])] };
        let result = compare_tile_bytes(&tile1, &tile2);
        assert_eq!(1, result.tile_diffs);
        assert_eq!(0, result.equivalent_tiles);
        assert_eq!(counts(&[("l", (1, 1, 0))]), feature_counts(&result));
        assert_eq!(
            vec![
                "only in 1: point id=1 1 points first (1 1) {a=\"x\"}",
                "only in 2: point id=1 1 points first (1 1) {a=\"z\"}"
            ],
            result.feature_diffs["l"].examples.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_layers_paired_by_name() {
        let roads = |value: &str| layer("roads", &["k"], &[value], vec![point(2, vec![0, 0])]);
        let water = || layer("water", &["k"], &["v"], vec![point(1, vec![0, 0])]);
        // layer order alone is not a difference
        let result = compare_tile_bytes(
            &vt::Tile { layers: vec![water(), roads("a")] },
            &vt::Tile { layers: vec![roads("a"), water()] },
        );
        assert_eq!((0, 1), (result.tile_diffs, result.equivalent_tiles), "{result:?}");
        assert!(result.tile_diff_types.is_empty(), "{result:?}");
        // a real difference is attributed to the right layer, with no spurious diffs from comparing by position
        let result = compare_tile_bytes(
            &vt::Tile { layers: vec![water(), roads("a")] },
            &vt::Tile { layers: vec![roads("b"), water()] },
        );
        assert_eq!(1, result.tile_diffs);
        assert_eq!(BTreeMap::from([("different contents".to_string(), 1)]), result.tile_diff_types);
        assert_eq!(counts(&[("roads", (1, 1, 0))]), feature_counts(&result));
        // missing layers are still reported, and their features count as differing features
        let result =
            compare_tile_bytes(&vt::Tile { layers: vec![water(), roads("a")] }, &vt::Tile { layers: vec![water()] });
        assert_eq!(
            BTreeMap::from([("tile layers 2 missing roads".to_string(), 1), ("different contents".to_string(), 1)]),
            { result.tile_diff_types.clone() }
        );
        assert_eq!(counts(&[("roads", (1, 0, 0))]), feature_counts(&result));
    }

    #[test]
    fn test_integer_encodings_are_equivalent() {
        let with_value = |v: vt::tile::Value| vt::Tile {
            layers: vec![vt::tile::Layer { values: vec![v], ..layer("l", &["n"], &[], vec![point(1, vec![0, 0])]) }],
        };
        let result = compare_tile_bytes(
            &with_value(vt::tile::Value { int_value: Some(5), ..Default::default() }),
            &with_value(vt::tile::Value { sint_value: Some(5), ..Default::default() }),
        );
        assert_eq!((0, 1), (result.tile_diffs, result.equivalent_tiles), "{result:?}");
    }

    #[test]
    fn test_geometry_commands_that_decode_the_same_are_equivalent() {
        use vt::tile::GeomType::Linestring;
        // MoveTo(0,0) LineTo(1,0) LineTo(1,1) as one LineTo command with 2 points vs two LineTo commands
        let line = |geometry: Vec<u32>| vt::Tile {
            layers: vec![layer("l", &[], &[], vec![feature(1, vec![], Linestring, geometry)])],
        };
        let result = compare_tile_bytes(&line(vec![9, 0, 0, 18, 2, 0, 0, 2]), &line(vec![9, 0, 0, 10, 2, 0, 10, 0, 2]));
        assert_eq!((0, 1), (result.tile_diffs, result.equivalent_tiles), "{result:?}");
        // a different geometry is a diff
        let result = compare_tile_bytes(&line(vec![9, 0, 0, 18, 2, 0, 0, 2]), &line(vec![9, 0, 0, 18, 2, 0, 0, 4]));
        assert_eq!(1, result.tile_diffs);
        assert_eq!(counts(&[("l", (1, 1, 0))]), feature_counts(&result));
        // the order of the lines of a multilinestring doesn't matter, their direction does
        let a: &[(i32, i32)] = &[(0, 0), (5, 5)];
        let b: &[(i32, i32)] = &[(10, 0), (10, 5), (12, 7)];
        let b_reversed: &[(i32, i32)] = &[(12, 7), (10, 5), (10, 0)];
        let result = compare_tile_bytes(&line(encode(&[a, b], false)), &line(encode(&[b, a], false)));
        assert_eq!((0, 1), (result.tile_diffs, result.equivalent_tiles), "{result:?}");
        let result = compare_tile_bytes(&line(encode(&[a, b], false)), &line(encode(&[a, b_reversed], false)));
        assert_eq!(1, result.tile_diffs, "{result:?}");
    }

    #[test]
    fn test_multipoint_order_is_equivalent() {
        use vt::tile::GeomType::Point;
        let points = |geometry: Vec<u32>| vt::Tile {
            layers: vec![layer("l", &[], &[], vec![feature(1, vec![], Point, geometry)])],
        };
        // MoveTo x2: (1,1) (3,3) vs (3,3) (1,1)
        let result = compare_tile_bytes(&points(vec![17, 2, 2, 4, 4]), &points(vec![17, 6, 6, 3, 3]));
        assert_eq!((0, 1), (result.tile_diffs, result.equivalent_tiles), "{result:?}");
    }

    #[test]
    fn test_ring_start_point_is_equivalent() {
        let result = compare_tile_bytes(&polygon_tile(vec![vec![SQUARE]]), &polygon_tile(vec![vec![SQUARE_ROTATED]]));
        assert_eq!((0, 1), (result.tile_diffs, result.equivalent_tiles), "{result:?}");
        // holes too
        let result = compare_tile_bytes(
            &polygon_tile(vec![vec![SQUARE, HOLE2]]),
            &polygon_tile(vec![vec![SQUARE_ROTATED, HOLE2_ROTATED]]),
        );
        assert_eq!((0, 1), (result.tile_diffs, result.equivalent_tiles), "{result:?}");
    }

    #[test]
    fn test_ring_orientation_matters() {
        // a reversed ring is a hole in MVT, not the same polygon
        let result = compare_tile_bytes(&polygon_tile(vec![vec![SQUARE]]), &polygon_tile(vec![vec![SQUARE_REVERSED]]));
        assert_eq!(1, result.tile_diffs, "{result:?}");
        assert_eq!(counts(&[("l", (1, 1, 0))]), feature_counts(&result));
    }

    #[test]
    fn test_ring_order_is_equivalent() {
        // holes in a different order
        let result = compare_tile_bytes(
            &polygon_tile(vec![vec![SQUARE, HOLE1, HOLE2]]),
            &polygon_tile(vec![vec![SQUARE_ROTATED, HOLE2_ROTATED, HOLE1]]),
        );
        assert_eq!((0, 1), (result.tile_diffs, result.equivalent_tiles), "{result:?}");
        // polygons of a multipolygon in a different order
        let result = compare_tile_bytes(
            &polygon_tile(vec![vec![SQUARE, HOLE1, SQUARE2]]),
            &polygon_tile(vec![vec![SQUARE2, SQUARE_ROTATED, HOLE1]]),
        );
        assert_eq!((0, 1), (result.tile_diffs, result.equivalent_tiles), "{result:?}");
        // a hole attached to a different exterior is a different geometry
        let result = compare_tile_bytes(
            &polygon_tile(vec![vec![SQUARE, HOLE1, SQUARE2]]),
            &polygon_tile(vec![vec![SQUARE, SQUARE2, HOLE1]]),
        );
        assert_eq!(1, result.tile_diffs, "{result:?}");
    }

    #[test]
    fn test_feature_order_is_equivalent() {
        let result = compare_tile_bytes(
            &polygon_tile(vec![vec![SQUARE], vec![SQUARE2], vec![SQUARE]]),
            &polygon_tile(vec![vec![SQUARE2], vec![SQUARE_ROTATED], vec![SQUARE]]),
        );
        assert_eq!((0, 1), (result.tile_diffs, result.equivalent_tiles), "{result:?}");
        // features are a multiset: a duplicate counts
        let result = compare_tile_bytes(
            &polygon_tile(vec![vec![SQUARE], vec![SQUARE2], vec![SQUARE]]),
            &polygon_tile(vec![vec![SQUARE2], vec![SQUARE]]),
        );
        assert_eq!(counts(&[("l", (1, 0, 0))]), feature_counts(&result));
    }

    #[test]
    fn test_feature_ids() {
        let tile = |ids: &[u64]| vt::Tile {
            layers: vec![layer("l", &["a"], &["x", "y"], ids.iter().map(|id| point(*id, vec![0, 0])).collect())],
        };
        let result = compare_tile_bytes(&tile(&[1, 2]), &tile(&[3, 2]));
        assert_eq!(1, result.tile_diffs);
        assert_eq!(counts(&[("l", (1, 1, 1))]), feature_counts(&result));
        // id order alone doesn't matter
        let result = compare_tile_bytes(&tile(&[1, 2]), &tile(&[2, 1]));
        assert_eq!((0, 1), (result.tile_diffs, result.equivalent_tiles), "{result:?}");
        // with ignore_ids
        let options = CompareOptions { ignore_ids: true, ..Default::default() };
        let result = compare_tiles_with(&tile(&[1, 2]), &tile(&[3, 2]), &options);
        assert_eq!((0, 1), (result.tile_diffs, result.equivalent_tiles), "{result:?}");
        // an id difference together with a geometry difference is not "id only"
        let other =
            vt::Tile { layers: vec![layer("l", &["a"], &["x"], vec![point(1, vec![0, 0]), point(5, vec![0, 0])])] };
        let moved = vt::Tile {
            layers: vec![layer(
                "l",
                &["a"],
                &["x"],
                vec![point(1, vec![0, 0]), feature(6, vec![0, 0], vt::tile::GeomType::Point, vec![9, 4, 4])],
            )],
        };
        assert_eq!(counts(&[("l", (1, 1, 0))]), feature_counts(&compare_tile_bytes(&other, &moved)));
    }

    #[test]
    fn test_examples() {
        let tile = |ids: &[u64]| vt::Tile {
            layers: vec![layer("l", &["a"], &["x"], ids.iter().map(|id| point(*id, vec![0, 0])).collect())],
        };
        let options = CompareOptions { examples: 3, ..Default::default() };
        let result = compare_tiles_with(&tile(&[1, 2, 3, 4]), &tile(&[5, 6, 7, 8]), &options);
        let examples: Vec<&str> = result.feature_diffs["l"].examples.iter().map(|(_, e)| e.as_str()).collect();
        assert_eq!(
            vec![
                "only in 1: point id=1 1 points first (1 1) {a=\"x\"}",
                "only in 2: point id=5 1 points first (1 1) {a=\"x\"}",
                "only in 1: point id=2 1 points first (1 1) {a=\"x\"}",
            ],
            examples
        );
    }

    #[test]
    fn test_rotate_ring() {
        let rotated = |ring: &[Point]| {
            let mut ring = ring.to_vec();
            rotate_ring(&mut ring);
            ring
        };
        assert_eq!(vec![(0, 0), (1, 0), (1, 1)], rotated(&[(1, 1), (0, 0), (1, 0)]));
        // explicit closing point is dropped
        assert_eq!(vec![(0, 0), (1, 0), (1, 1)], rotated(&[(1, 0), (1, 1), (0, 0), (1, 0)]));
        // a vertex visited twice: the smallest rotation among those starting there
        let a = [(0, 0), (5, 1), (3, 3), (0, 0), (2, 5), (1, 3)];
        let b = [(0, 0), (2, 5), (1, 3), (0, 0), (5, 1), (3, 3)];
        assert_eq!(rotated(&a), rotated(&b));
        assert!(rotated(&[]).is_empty());
    }

    #[test]
    fn test_malformed_geometry_is_compared_raw() {
        use vt::tile::GeomType::Linestring;
        let line = |geometry: Vec<u32>| vt::Tile {
            layers: vec![layer("l", &[], &[], vec![feature(1, vec![], Linestring, geometry)])],
        };
        // LineTo without MoveTo
        let result = compare_tile_bytes(&line(vec![10, 2, 2]), &line(vec![10, 4, 4]));
        assert_eq!(counts(&[("l", (1, 1, 0))]), feature_counts(&result));
        assert!(result.feature_diffs["l"].examples[0].1.contains("3 raw commands"), "{result:?}");
    }
}
