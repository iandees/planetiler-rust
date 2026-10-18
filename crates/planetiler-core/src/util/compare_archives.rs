//! Port of `util/CompareArchives.java`: compares the contents of two tile archives.
//!
//! To run: `planetiler compare [options] {path/to/archive1} {path/to/archive2}`
//!
//! Difference from Java: when two tiles have different bytes, this decodes them and only reports a tile diff if the
//! decoded contents differ. Layers are paired by name (not position), feature tags are compared as decoded key/value
//! pairs (so a different order of the layer `keys`/`values` tables or of the layers themselves doesn't count), and
//! geometries whose command streams differ but decode to the same geometry are equal. Tiles that only differ in
//! encoding are counted in [`CompareResult::equivalent_tiles`].

use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use geo_types::{Geometry, Polygon};
use parking_lot::Mutex;
use prost::Message;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::archive::tile::Tile;
use crate::archive::tile_archive_config::{Format, TileArchiveConfig};
use crate::archive::tile_archives;
use crate::archive::tile_compression::TileCompression;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::geo::geometry_type::GeometryType;
use crate::geo::tile_coord::TileCoord;
use crate::pmtiles::readable_pmtiles::ReadablePmtiles;
use crate::proto::vector_tile as vt;
use crate::util::gzip::gunzip;
use crate::vector_tile::VectorGeometry;

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
    pub tile_diffs_by_layer: BTreeMap<String, BTreeMap<String, u64>>,
}

struct CompareArchives<'a> {
    diff_types: Mutex<BTreeMap<String, u64>>,
    diffs_by_layer: Mutex<BTreeMap<String, BTreeMap<String, u64>>>,
    archive_diffs: Mutex<Vec<String>>,
    input1: &'a TileArchiveConfig,
    input2: &'a TileArchiveConfig,
    verbose: bool,
}

/// Compares two archives.
///
/// Returns a [`FatalComparisonFailure`] error if a comparison failure is encountered that prevents comparing the
/// whole archives.
pub fn compare(
    archive_config1: &TileArchiveConfig,
    archive_config2: &TileArchiveConfig,
    config: &PlanetilerConfig,
    verbose: bool,
) -> anyhow::Result<CompareResult> {
    CompareArchives {
        diff_types: Mutex::new(BTreeMap::new()),
        diffs_by_layer: Mutex::new(BTreeMap::new()),
        archive_diffs: Mutex::new(Vec::new()),
        input1: archive_config1,
        input2: archive_config2,
        verbose,
    }
    .get_result(config)
}

/// Logs a comparison result like Java `CompareArchives.main` and returns the process exit code.
pub fn log_result(result: &CompareResult, strict: bool) -> i32 {
    info!("Detailed diffs:");
    for (layer, diffs) in &result.tile_diffs_by_layer {
        info!("  \"{layer}\" layer");
        let mut entries: Vec<_> = diffs.iter().collect();
        entries.sort_by_key(|(_, v)| **v);
        for (k, v) in entries {
            info!("    {k}: {v}");
        }
    }
    let mut entries: Vec<_> = result.tile_diff_types.iter().collect();
    entries.sort_by_key(|(_, v)| **v);
    for (k, v) in entries {
        info!("  {k}: {v}");
    }
    for diff in &result.archive_diffs {
        info!("  {diff}");
    }
    info!("Total tiles: {}", result.total);
    if result.equivalent_tiles > 0 {
        info!("Tiles with different bytes but equivalent decoded contents: {}", result.equivalent_tiles);
    }
    info!(
        "Tile diffs: {} ({:.1}% of all tiles)",
        result.tile_diffs,
        result.tile_diffs as f64 * 100.0 / result.total.max(1) as f64
    );
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
    let verbose = arguments.get_boolean("verbose", "log each tile diff", false);
    let strict = arguments.get_boolean("strict", "set to false to only fail on tile diffs", true);
    let config = PlanetilerConfig::from(&arguments)?;
    match compare(&input1, &input2, &config, verbose) {
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
                                        let issues = compare_tiles(&tile1, &tile2);
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
    }

    fn record_layer_diff(&self, coord: &TileCoord, layer: &str, issue: &str) {
        *self.diffs_by_layer.lock().entry(layer.to_string()).or_default().entry(issue.to_string()).or_default() += 1;
        if self.verbose {
            debug!("{coord} layer {layer} {issue}");
        }
    }

    fn record_tile_diff(&self, coord: &TileCoord, issue: &str) {
        *self.diff_types.lock().entry(issue.to_string()).or_default() += 1;
        if self.verbose {
            debug!("{coord} {issue}");
        }
    }
}

/// Differences found between the decoded contents of two tiles.
#[derive(Default)]
struct TileIssues {
    /// Tile-level issues (missing layers).
    tile: Vec<String>,
    /// `(layer, issue)` pairs.
    layer: Vec<(String, String)>,
}

impl TileIssues {
    fn is_empty(&self) -> bool {
        self.tile.is_empty() && self.layer.is_empty()
    }

    fn push(&mut self, layer: &str, name: &str) {
        self.layer.push((layer.to_string(), name.to_string()));
    }

    /// Records a layer diff if `value1 != value2` and returns true if they are equal.
    fn compare_values<T: PartialEq + ?Sized>(&mut self, layer: &str, name: &str, value1: &T, value2: &T) -> bool {
        if value1 != value2 {
            self.push(layer, name);
            return false;
        }
        true
    }
}

/// A decoded vector tile value. Integers are compared by value regardless of whether they were encoded as `int`,
/// `uint` or `sint` (Java decodes all three to a `Long`).
#[derive(Debug, PartialEq)]
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

type DecodedTags = Vec<(Option<String>, Option<DecodedValue>)>;

/// The tags of `feature` resolved through the layer's key/value tables and sorted by key, so features compare equal
/// regardless of how the tables are ordered. Out-of-range indices decode to `None`.
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
    tags.sort_by(|a, b| a.0.cmp(&b.0));
    tags
}

fn compare_tiles(proto1: &vt::Tile, proto2: &vt::Tile) -> TileIssues {
    let mut issues = TileIssues::default();
    let layers1: Vec<&String> = proto1.layers.iter().map(|l| &l.name).collect();
    let layers2: Vec<&String> = proto2.layers.iter().map(|l| &l.name).collect();
    for layer in &layers1 {
        if !layers2.contains(layer) {
            issues.tile.push(format!("tile layers 2 missing {layer}"));
        }
    }
    for layer in &layers2 {
        if !layers1.contains(layer) {
            issues.tile.push(format!("tile layers 1 missing {layer}"));
        }
    }
    // pair layers by name: the order of layers within a tile doesn't affect rendering
    let mut by_name: HashMap<&str, &vt::tile::Layer> = HashMap::new();
    for layer in &proto2.layers {
        by_name.entry(&layer.name).or_insert(layer);
    }
    for layer1 in &proto1.layers {
        if let Some(layer2) = by_name.remove(layer1.name.as_str()) {
            compare_layer(&mut issues, layer1, layer2);
        }
    }
    issues
}

fn compare_layer(issues: &mut TileIssues, layer1: &vt::tile::Layer, layer2: &vt::tile::Layer) {
    let name = &layer1.name;
    issues.compare_values(name, "version", &layer1.version, &layer2.version);
    issues.compare_values(name, "extent", &layer1.extent(), &layer2.extent());
    if issues.compare_values(name, "features count", &layer1.features.len(), &layer2.features.len()) {
        let ids1: Vec<u64> = layer1.features.iter().map(|f| f.id()).collect();
        let ids2: Vec<u64> = layer2.features.iter().map(|f| f.id()).collect();
        let (mut sorted1, mut sorted2) = (ids1.clone(), ids2.clone());
        sorted1.sort_unstable();
        sorted2.sort_unstable();
        if issues.compare_values(name, "feature ids", &sorted1, &sorted2)
            && issues.compare_values(name, "feature order", &ids1, &ids2)
        {
            for (feature1, feature2) in layer1.features.iter().zip(layer2.features.iter()) {
                issues.compare_values(name, "feature id", &feature1.id(), &feature2.id());
                compare_geometry(issues, name, feature1, feature2);
                let (tags1, tags2) = (decoded_tags(layer1, feature1), decoded_tags(layer2, feature2));
                issues.compare_values(name, "feature tags", &tags1, &tags2);
            }
        }
    }
}

fn compare_geometry(issues: &mut TileIssues, layer: &str, f1: &vt::tile::Feature, f2: &vt::tile::Feature) {
    if !issues.compare_values(layer, "feature type", &f1.r#type(), &f2.r#type()) || f1.geometry == f2.geometry {
        return;
    }
    let geom_type = GeometryType::from_protobuf(f1.r#type());
    let to_geometry = |f: &vt::tile::Feature| {
        VectorGeometry::new(f.geometry.iter().map(|c| *c as i32).collect::<Vec<_>>(), geom_type, 0).decode()
    };
    let commands_diff = format!("feature {} geometry commands", f1.r#type().as_str_name().to_lowercase());
    match (to_geometry(f1), to_geometry(f2)) {
        // different commands that decode to the same geometry (for example split repeated commands) are equivalent
        (Ok(geom1), Ok(geom2)) if geom1 == geom2 => {}
        (Ok(geom1), Ok(geom2)) => {
            issues.push(layer, &commands_diff);
            compare_decoded_geometry(issues, layer, &geom1, &geom2);
        }
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!("Error decoding geometry: {e}");
            issues.push(layer, &commands_diff);
        }
    }
}

/// JTS `Geometry.getGeometryType()` names.
fn jts_type(g: &Geometry<f64>) -> &'static str {
    match g {
        Geometry::Point(_) => "Point",
        Geometry::MultiPoint(_) => "MultiPoint",
        Geometry::LineString(_) | Geometry::Line(_) => "LineString",
        Geometry::MultiLineString(_) => "MultiLineString",
        Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => "Polygon",
        Geometry::MultiPolygon(_) => "MultiPolygon",
        Geometry::GeometryCollection(_) => "GeometryCollection",
    }
}

fn num_geometries(g: &Geometry<f64>) -> usize {
    match g {
        Geometry::MultiPoint(m) => m.0.len(),
        Geometry::MultiLineString(m) => m.0.len(),
        Geometry::MultiPolygon(m) => m.0.len(),
        Geometry::GeometryCollection(c) => c.0.len(),
        _ => 1,
    }
}

/// Port of Java's `compareGeometry(Geometry, Geometry)`: geometry type, number of geometries, and polygon rings.
fn compare_decoded_geometry(issues: &mut TileIssues, layer: &str, geom1: &Geometry<f64>, geom2: &Geometry<f64>) {
    let geometry_type = jts_type(geom1);
    issues.compare_values(layer, "feature JTS geometry type", geometry_type, jts_type(geom2));
    issues.compare_values(layer, "feature num geometries", &num_geometries(geom1), &num_geometries(geom2));
    match (geom1, geom2) {
        (Geometry::MultiPolygon(m1), Geometry::MultiPolygon(m2)) => {
            for (p1, p2) in m1.0.iter().zip(m2.0.iter()) {
                compare_polygon(issues, layer, geometry_type, p1, p2);
            }
        }
        (Geometry::Polygon(p1), Geometry::Polygon(p2)) => compare_polygon(issues, layer, geometry_type, p1, p2),
        _ => {}
    }
}

fn compare_polygon(issues: &mut TileIssues, layer: &str, geom_type: &str, p1: &Polygon<f64>, p2: &Polygon<f64>) {
    issues.compare_values(layer, &format!("{geom_type} exterior ring geometry"), p1.exterior(), p2.exterior());
    let (holes1, holes2) = (p1.interiors(), p2.interiors());
    if issues.compare_values(layer, &format!("{geom_type} num interior rings"), &holes1.len(), &holes2.len()) {
        for (r1, r2) in holes1.iter().zip(holes2.iter()) {
            issues.compare_values(layer, &format!("{geom_type} interior ring geometry"), r1, r2);
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
        compare(&config_for(&a), &config_for(&b), &PlanetilerConfig::defaults(), false).unwrap()
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

    fn expected_layer_diffs() -> BTreeMap<String, BTreeMap<String, u64>> {
        BTreeMap::from([("layer1".to_string(), BTreeMap::from([("feature ids".to_string(), 1)]))])
    }

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
        let result = compare(&config_for(&a), &config_for(&b), &PlanetilerConfig::defaults(), false).unwrap();
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
                tile_diffs_by_layer: expected_layer_diffs(),
            },
            result
        );
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
        let result = compare(&config_for(&a), &config_for(&b), &PlanetilerConfig::defaults(), false).unwrap();
        assert_eq!(
            CompareResult {
                total: 5,
                tile_diffs: 4,
                equivalent_tiles: 0,
                archive_diffs: vec!["metadata".into(), "pmtiles header".into(), "tile compression".into()],
                tile_diff_types: BTreeMap::from([
                    ("archive 2 missing tile".into(), 1),
                    ("archive 1 missing tile".into(), 2),
                    ("different decompressed contents".into(), 1),
                ]),
                tile_diffs_by_layer: expected_layer_diffs(),
            },
            result
        );
    }

    #[test]
    fn test_compare_archives_same() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.pmtiles"), tmp.path().join("b.pmtiles"));
        let a1 = vec![0xa, 0x2];
        write(&a, &[(0, a1.clone()), (2, a1.clone())], TileCompression::Gzip);
        write(&b, &[(0, a1.clone()), (2, a1)], TileCompression::Gzip);
        let result = compare(&config_for(&a), &config_for(&b), &PlanetilerConfig::defaults(), false).unwrap();
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
        assert!(!result.archive_diffs.iter().any(|d| d.starts_with("mismatch")), "{result:?}");
        assert_eq!(0, log_result(&result, false));
    }

    #[test]
    fn test_different_tag_value_is_a_diff() {
        let tile1 = vt::Tile { layers: vec![layer("l", &["a"], &["x"], vec![point(1, vec![0, 0])])] };
        let tile2 = vt::Tile { layers: vec![layer("l", &["a"], &["z"], vec![point(1, vec![0, 0])])] };
        let result = compare_tile_bytes(&tile1, &tile2);
        assert_eq!(1, result.tile_diffs);
        assert_eq!(0, result.equivalent_tiles);
        assert_eq!(BTreeMap::from([("l".to_string(), BTreeMap::from([("feature tags".to_string(), 1)]))]), {
            result.tile_diffs_by_layer
        });
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
        assert_eq!(
            BTreeMap::from([("roads".to_string(), BTreeMap::from([("feature tags".to_string(), 1)]))]),
            result.tile_diffs_by_layer
        );
        // missing layers are still reported
        let result =
            compare_tile_bytes(&vt::Tile { layers: vec![water(), roads("a")] }, &vt::Tile { layers: vec![water()] });
        assert_eq!(
            BTreeMap::from([("tile layers 2 missing roads".to_string(), 1), ("different contents".to_string(), 1)]),
            { result.tile_diff_types }
        );
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
        assert_eq!(
            BTreeMap::from([(
                "l".to_string(),
                BTreeMap::from([("feature linestring geometry commands".to_string(), 1)])
            )]),
            result.tile_diffs_by_layer
        );
    }

    #[test]
    fn test_polygon_ring_diffs() {
        use vt::tile::GeomType::Polygon;
        let square = |size: u32| vec![9, 0, 0, 26, size * 2, 0, 0, size * 2, size * 2 - 1, 0, 15];
        let tile = |geometry: Vec<u32>| vt::Tile {
            layers: vec![layer("l", &[], &[], vec![feature(1, vec![], Polygon, geometry)])],
        };
        let result = compare_tile_bytes(&tile(square(1)), &tile(square(2)));
        assert_eq!(
            BTreeMap::from([(
                "l".to_string(),
                BTreeMap::from([
                    ("feature polygon geometry commands".to_string(), 1),
                    ("Polygon exterior ring geometry".to_string(), 1)
                ])
            )]),
            result.tile_diffs_by_layer
        );
    }
}
