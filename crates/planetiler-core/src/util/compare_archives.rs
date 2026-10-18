//! Port of `util/CompareArchives.java`: compares the contents of two tile archives.
//!
//! To run (Java): `java -jar planetiler.jar compare [options] {path/to/archive1} {path/to/archive2}`

use std::collections::BTreeMap;
use std::fmt::Debug;
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
                    let (total, diffs) = (&total, &diffs);
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
                                        if same_compression {
                                            if a.bytes != b.bytes {
                                                self.record_tile_diff(&a.coord, "different contents");
                                                diffs.fetch_add(1, Ordering::Relaxed);
                                                self.compare_tiles(
                                                    &a.coord,
                                                    &decode(&decompress(&a.bytes, compression1)?)?,
                                                    &decode(&decompress(&b.bytes, compression2)?)?,
                                                );
                                            }
                                        } else {
                                            let decompressed1 = decompress(&a.bytes, compression1)?;
                                            let decompressed2 = decompress(&b.bytes, compression2)?;
                                            if decompressed1 != decompressed2 {
                                                self.record_tile_diff(&a.coord, "different decompressed contents");
                                                diffs.fetch_add(1, Ordering::Relaxed);
                                                self.compare_tiles(
                                                    &a.coord,
                                                    &decode(&decompressed1)?,
                                                    &decode(&decompressed2)?,
                                                );
                                            }
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

        if self.archive_diffs.lock().is_empty() && diffs.load(Ordering::Relaxed) == 0 {
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

    fn compare_tiles(&self, coord: &TileCoord, proto1: &vt::Tile, proto2: &vt::Tile) {
        self.compare_layer_names(coord, proto1, proto2);
        for (layer1, layer2) in proto1.layers.iter().zip(proto2.layers.iter()) {
            self.compare_layer(coord, layer1, layer2);
        }
    }

    fn compare_layer(&self, coord: &TileCoord, layer1: &vt::tile::Layer, layer2: &vt::tile::Layer) {
        let name = &layer1.name;
        self.compare_values(coord, name, "version", &layer1.version, &layer2.version);
        self.compare_values(coord, name, "extent", &layer1.extent(), &layer2.extent());
        self.compare_list(coord, name, "keys list", &layer1.keys, &layer2.keys, |k| k.clone().into_bytes());
        self.compare_list(coord, name, "values list", &layer1.values, &layer2.values, |v| v.encode_to_vec());
        if self.compare_values(coord, name, "features count", &layer1.features.len(), &layer2.features.len()) {
            let ids1: Vec<u64> = layer1.features.iter().map(|f| f.id()).collect();
            let ids2: Vec<u64> = layer2.features.iter().map(|f| f.id()).collect();
            // Java compares `Set.of(ids1)` (a set containing the list) and then the lists themselves
            if self.compare_values(coord, name, "feature ids", &ids1, &ids2)
                && self.compare_values(coord, name, "feature order", &ids1, &ids2)
            {
                for (feature1, feature2) in layer1.features.iter().zip(layer2.features.iter()) {
                    self.compare_feature(coord, name, feature1, feature2);
                }
            }
        }
    }

    fn compare_feature(&self, coord: &TileCoord, layer: &str, f1: &vt::tile::Feature, f2: &vt::tile::Feature) {
        self.compare_values(coord, layer, "feature id", &f1.id(), &f2.id());
        self.compare_geometry(coord, layer, f1, f2);
        self.compare_values(coord, layer, "feature tags", &f1.tags.len(), &f2.tags.len());
    }

    fn compare_geometry(&self, coord: &TileCoord, layer: &str, f1: &vt::tile::Feature, f2: &vt::tile::Feature) {
        if self.compare_values(coord, layer, "feature type", &f1.r#type(), &f2.r#type()) {
            let geom_type = f1.r#type().as_str_name().to_lowercase();
            // TODO(port): when the command streams differ, Java decodes both geometries (VectorGeometry.decode) and
            // compares JTS geometry type, num geometries and polygon rings; requires the vector_tile.rs port.
            self.compare_values(
                coord,
                layer,
                &format!("feature {geom_type} geometry commands"),
                &f1.geometry,
                &f2.geometry,
            );
        }
    }

    fn compare_layer_names(&self, coord: &TileCoord, proto1: &vt::Tile, proto2: &vt::Tile) {
        let layers1: Vec<&String> = proto1.layers.iter().map(|l| &l.name).collect();
        let layers2: Vec<&String> = proto2.layers.iter().map(|l| &l.name).collect();
        self.compare_list_detailed(coord, "tile layers", &layers1, &layers2);
    }

    fn compare_list<T: PartialEq + Debug>(
        &self,
        coord: &TileCoord,
        layer: &str,
        name: &str,
        value1: &[T],
        value2: &[T],
        key: impl Fn(&T) -> Vec<u8>,
    ) -> bool {
        let set1: std::collections::BTreeSet<Vec<u8>> = value1.iter().map(&key).collect();
        let set2: std::collections::BTreeSet<Vec<u8>> = value2.iter().map(&key).collect();
        self.compare_values(coord, layer, &format!("{name} unique values"), &set1, &set2)
            && self.compare_values(coord, layer, &format!("{name} order"), &value1, &value2)
    }

    fn compare_list_detailed<T: PartialEq + Debug + std::fmt::Display>(
        &self,
        coord: &TileCoord,
        name: &str,
        value1: &[T],
        value2: &[T],
    ) {
        if value1 != value2 {
            let mut missing = false;
            for layer in value1 {
                if !value2.contains(layer) {
                    self.record_tile_diff(coord, &format!("{name} 2 missing {layer}"));
                    missing = true;
                }
            }
            for layer in value2 {
                if !value1.contains(layer) {
                    self.record_tile_diff(coord, &format!("{name} 1 missing {layer}"));
                    missing = true;
                }
            }
            if !missing {
                self.record_tile_diff(coord, &format!("{name} different order"));
            }
        }
    }

    fn compare_values<T: PartialEq + ?Sized>(
        &self,
        coord: &TileCoord,
        layer: &str,
        name: &str,
        value1: &T,
        value2: &T,
    ) -> bool {
        if value1 != value2 {
            self.record_layer_diff(coord, layer, name);
            return false;
        }
        true
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
        BTreeMap::from([(
            "layer1".to_string(),
            BTreeMap::from([("values list unique values".to_string(), 1), ("feature ids".to_string(), 1)]),
        )])
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
}
