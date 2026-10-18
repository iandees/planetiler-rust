//! Port of `reader/GeoPackageReader.java`: reads features from the vector feature tables of an
//! [OGC GeoPackage](https://www.geopackage.org/spec/) file.
//!
//! Java uses the NGA GeoPackage library; this reads the sqlite tables directly with `rusqlite`:
//! - feature tables come from `gpkg_contents` (`data_type = 'features'`), each becomes a source layer
//! - the geometry column and SRS come from `gpkg_geometry_columns` / `gpkg_spatial_ref_sys`
//! - geometry blobs are parsed as [GeoPackageBinary](https://www.geopackage.org/spec/#gpb_format) (header + WKB) and
//!   reprojected to lat/lon (input coordinates are always treated as longitude-first like Java)
//! - every other column (including the primary key) becomes a tag
//! - when bounds are provided and the table has an R-tree spatial index, only features whose bounding box intersects
//!   the bounds are read
//!
//! Inputs inside a zip file are extracted to `tmp_dir` first. Rows are read sequentially (assigning IDs starting at 1
//! like Java) and geometries are decoded/reprojected by a pool of threads, then emitted in order.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use tracing::warn;

use crate::config::bounds::Bounds;
use crate::geo::crs::{Crs, MathTransform, decode_crs, find_math_transform};
use crate::geo::geo_utils;
use crate::reader::shapefile_reader::emit_in_order;
use crate::reader::simple_reader::{
    FeatureSink, ReaderFeature, SimpleReader, default_decode_threads, extract_from_zip, inverse_transform_envelope,
    transform_in_place,
};
use crate::util::file_utils;
use crate::value::{Tags, Value};

/// Reads features from a GeoPackage file.
pub struct GeoPackageReader {
    source_name: String,
    source_projection: Option<String>,
    input: PathBuf,
    db_path: PathBuf,
    extracted: Option<PathBuf>,
    keep_unzipped: bool,
    bounds: Option<Bounds>,
    decode_threads: usize,
}

/// A parsed GeoPackageBinary geometry header.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoPackageGeometryHeader {
    /// Spatial reference system ID from the header.
    pub srs_id: i32,
    /// True if the empty-geometry flag is set.
    pub empty: bool,
    /// Byte offset of the WKB geometry.
    pub wkb_offset: usize,
}

/// Parses the header of a [GeoPackageBinary](https://www.geopackage.org/spec/#gpb_format) geometry blob.
pub fn parse_geometry_header(blob: &[u8]) -> Result<GeoPackageGeometryHeader> {
    if blob.len() < 8 || &blob[0..2] != b"GP" {
        bail!("Invalid GeoPackage geometry: bad magic number");
    }
    let flags = blob[3];
    let little_endian = flags & 1 == 1;
    let envelope_bytes = match (flags >> 1) & 0b111 {
        0 => 0,
        1 => 32,
        2 | 3 => 48,
        4 => 64,
        other => bail!("Invalid GeoPackage geometry envelope indicator {other}"),
    };
    let empty = (flags >> 4) & 1 == 1;
    let srs = blob[4..8].try_into().unwrap();
    let srs_id = if little_endian { i32::from_le_bytes(srs) } else { i32::from_be_bytes(srs) };
    let wkb_offset = 8 + envelope_bytes;
    if blob.len() < wkb_offset {
        bail!("Invalid GeoPackage geometry: truncated header");
    }
    Ok(GeoPackageGeometryHeader { srs_id, empty, wkb_offset })
}

struct FeatureTable {
    name: String,
    geometry_column: String,
    transform: MathTransform,
    /// R-tree index table and the bbox to query it with (in the source CRS).
    index_query: Option<(String, [f64; 4])>,
}

struct RawRow {
    id: i64,
    wkb: Vec<u8>,
    tags: Tags,
}

impl GeoPackageReader {
    /// Opens a GeoPackage at `input` (possibly a path inside a zip file, which is extracted to `tmp_dir`).
    ///
    /// `source_projection` overrides the CRS from the file, and `bounds` (when not `None`) limits features read using
    /// the spatial index if one exists.
    pub fn new(
        source_projection: Option<&str>,
        source_name: &str,
        input: &Path,
        tmp_dir: &Path,
        keep_unzipped: bool,
        bounds: Option<&Bounds>,
    ) -> Result<GeoPackageReader> {
        if let Some(proj) = source_projection {
            decode_crs(proj, None).map_err(|_| anyhow!("Invalid CRS: {proj}"))?;
        }
        let extracted = extract_from_zip(input, "gpkg", tmp_dir, keep_unzipped)?;
        let db_path = extracted.clone().unwrap_or_else(|| input.to_path_buf());
        let reader = GeoPackageReader {
            source_name: source_name.to_string(),
            source_projection: source_projection.map(str::to_string),
            input: input.to_path_buf(),
            db_path,
            extracted: if keep_unzipped { None } else { extracted },
            keep_unzipped,
            bounds: bounds.cloned(),
            decode_threads: default_decode_threads(),
        };
        reader.feature_table_names().with_context(|| format!("opening geopackage {}", input.display()))?;
        Ok(reader)
    }

    /// Sets the number of threads used to decode geometries.
    pub fn with_decode_threads(mut self, threads: usize) -> Self {
        self.decode_threads = threads.max(1);
        self
    }

    fn open(&self) -> Result<Connection> {
        Ok(Connection::open_with_flags(
            &self.db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
        )?)
    }

    /// Names of all feature tables in this geopackage.
    pub fn feature_table_names(&self) -> Result<Vec<String>> {
        let conn = self.open()?;
        let mut stmt = conn.prepare("SELECT table_name FROM gpkg_contents WHERE data_type = 'features'")?;
        let names = stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(names)
    }

    fn source_crs(&self, conn: &Connection, srs_id: i64) -> Result<Crs> {
        let srs: Option<(Option<String>, Option<String>, Option<i64>)> = conn
            .query_row(
                "SELECT definition, organization, organization_coordsys_id FROM gpkg_spatial_ref_sys WHERE srs_id = ?1",
                [srs_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        // GeoPackage spec allows this to be 0 (undefined geographic CRS) or -1 (undefined cartesian CRS)
        let mut source_crs = srs.and_then(|(definition, org, code)| {
            definition.filter(|d| !d.trim().is_empty()).and_then(|d| Crs::parse_wkt(&d).ok()).or_else(|| {
                match (org, code) {
                    (Some(org), Some(code)) if org.eq_ignore_ascii_case("EPSG") && code > 0 => {
                        Crs::decode(&format!("EPSG:{code}"), true).ok()
                    }
                    _ => None,
                }
            })
        });
        if let Some(proj) = &self.source_projection {
            source_crs = Some(decode_crs(proj, source_crs.as_ref()).context("Bad reference system")?);
        } else if source_crs.is_none() {
            warn!(
                "No reference system provided in {}, please set it explicitly. Defaulting to EPSG:4326",
                self.input.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
            );
        }
        Ok(source_crs.unwrap_or_else(Crs::wgs84))
    }

    fn feature_table(&self, conn: &Connection, name: &str) -> Result<FeatureTable> {
        let (geometry_column, srs_id): (String, i64) = conn.query_row(
            "SELECT column_name, srs_id FROM gpkg_geometry_columns WHERE table_name = ?1",
            [name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let source_crs = self.source_crs(conn, srs_id)?;
        let transform = find_math_transform(&source_crs, &Crs::wgs84(), Some(true));
        let rtree = format!("rtree_{name}_{geometry_column}");
        let indexed = conn
            .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [&rtree], |_| Ok(()))
            .optional()?
            .is_some();
        let index_query = match &self.bounds {
            Some(bounds) if indexed && !bounds.is_world() => {
                let lat_lon = bounds.lat_lon();
                let env = if transform.is_identity() {
                    Some(lat_lon)
                } else {
                    inverse_transform_envelope(&transform, &lat_lon)
                };
                env.map(|e| (rtree, [e.min_x, e.max_x, e.min_y, e.max_y]))
            }
            _ => None,
        };
        Ok(FeatureTable { name: name.to_string(), geometry_column, transform, index_query })
    }

    /// Reads rows sequentially, sending batches of `(seq, rows)` to `emit`. Returns `false` from `emit` to stop.
    fn scan(&self, mut emit: impl FnMut(usize, &FeatureTable, Vec<RawRow>) -> bool) -> Result<()> {
        const BATCH: usize = 1024;
        let conn = self.open()?;
        let mut id = 0i64;
        let mut seq = 0usize;
        let mut logged_missing_geometry = false;
        for name in self.feature_table_names()? {
            let table = self.feature_table(&conn, &name)?;
            let quoted = quote_identifier(&table.name);
            let mut stmt = match &table.index_query {
                Some((rtree, _)) => conn.prepare(&format!(
                    "SELECT * FROM {quoted} WHERE rowid IN (SELECT id FROM {} WHERE maxx >= ?1 AND minx <= ?2 AND maxy >= ?3 AND miny <= ?4)",
                    quote_identifier(rtree)
                ))?,
                None => conn.prepare(&format!("SELECT * FROM {quoted}"))?,
            };
            let columns: Vec<String> = stmt.column_names().into_iter().map(str::to_string).collect();
            let declared_types: std::collections::HashMap<String, String> = conn
                .prepare(&format!("PRAGMA table_info({quoted})"))?
                .query_map([], |row| Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let declared: Vec<String> = columns
                .iter()
                .map(|c| declared_types.get(c).map(|t| t.to_ascii_uppercase()).unwrap_or_default())
                .collect();
            let geometry_index = columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case(&table.geometry_column))
                .ok_or_else(|| anyhow!("geometry column {} not found in {}", table.geometry_column, table.name))?;
            let mut rows = match &table.index_query {
                Some((_, b)) => stmt.query(rusqlite::params![b[0], b[1], b[2], b[3]])?,
                None => stmt.query([])?,
            };
            let mut batch = Vec::with_capacity(BATCH);
            while let Some(row) = rows.next()? {
                let wkb = match row.get_ref(geometry_index)? {
                    ValueRef::Blob(blob) => match parse_geometry_header(blob) {
                        Ok(header) if !header.empty && blob.len() > header.wkb_offset => {
                            Some(blob[header.wkb_offset..].to_vec())
                        }
                        Ok(_) => None,
                        Err(e) => return Err(e.context(format!("in table {}", table.name))),
                    },
                    _ => None,
                };
                let Some(wkb) = wkb else {
                    if !logged_missing_geometry {
                        logged_missing_geometry = true;
                        warn!("Geopackage file contains empty geometry: {}", self.db_path.display());
                    }
                    continue;
                };
                id += 1;
                let mut tags = crate::value::tags_with_capacity(columns.len() - 1);
                for (i, column) in columns.iter().enumerate() {
                    if i != geometry_index {
                        tags.insert(column.clone(), column_value(row.get_ref(i)?, &declared[i]));
                    }
                }
                batch.push(RawRow { id, wkb, tags });
                if batch.len() >= BATCH {
                    if !emit(seq, &table, std::mem::replace(&mut batch, Vec::with_capacity(BATCH))) {
                        return Ok(());
                    }
                    seq += 1;
                }
            }
            if !batch.is_empty() {
                if !emit(seq, &table, batch) {
                    return Ok(());
                }
                seq += 1;
            }
        }
        Ok(())
    }
}

fn decode_rows(table_name: &str, transform: &MathTransform, rows: Vec<RawRow>) -> Result<Vec<ReaderFeature>> {
    let layer: Option<std::sync::Arc<str>> = Some(table_name.into());
    rows.into_iter()
        .map(|row| {
            let mut geometry =
                geo_utils::read_wkb(&row.wkb).map_err(|e| anyhow!("{e} in {table_name} feature {}", row.id))?;
            if !transform.is_identity() {
                transform_in_place(&mut geometry, transform)?;
            }
            Ok(ReaderFeature { geometry, tags: row.tags, source_layer: layer.clone(), id: row.id })
        })
        .collect()
}

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Convert a column value to a tag based on its declared GeoPackage data type.
fn column_value(value: ValueRef<'_>, declared_type: &str) -> Value {
    match value {
        ValueRef::Integer(i) if declared_type == "BOOLEAN" => Value::Bool(i != 0),
        ValueRef::Real(d) if declared_type == "FLOAT" => Value::Float(d as f32),
        ValueRef::Integer(i) if declared_type == "FLOAT" => Value::Float(i as f32),
        ValueRef::Integer(i) if declared_type == "DOUBLE" || declared_type == "REAL" => Value::Double(i as f64),
        other => crate::reader::natural_earth_reader::sqlite_value(other),
    }
}

impl SimpleReader for GeoPackageReader {
    fn source_name(&self) -> &str {
        &self.source_name
    }

    fn feature_count(&self) -> Result<u64> {
        let conn = self.open()?;
        let mut count = 0;
        for name in self.feature_table_names()? {
            count += conn.query_row(&format!("SELECT COUNT(*) FROM {}", quote_identifier(&name)), [], |row| {
                row.get::<_, i64>(0)
            })? as u64;
        }
        Ok(count)
    }

    fn read_features(&self, next: &mut FeatureSink<'_>) -> Result<()> {
        let threads = self.decode_threads;
        if threads <= 1 {
            let mut error = None;
            self.scan(|_, table, rows| match decode_rows(&table.name, &table.transform, rows) {
                Ok(features) => {
                    for feature in features {
                        if let Err(e) = next(feature) {
                            error = Some(e);
                            return false;
                        }
                    }
                    true
                }
                Err(e) => {
                    error = Some(e);
                    false
                }
            })?;
            return error.map_or(Ok(()), Err);
        }
        std::thread::scope(|scope| -> Result<()> {
            type Work = (usize, String, MathTransform, Vec<RawRow>);
            let (raw_tx, raw_rx) = crossbeam_channel::bounded::<Work>(threads * 2);
            let (decoded_tx, decoded_rx) = crossbeam_channel::bounded(threads * 2);
            let reader = scope.spawn(move || {
                self.scan(|seq, table, rows| {
                    raw_tx.send((seq, table.name.clone(), table.transform.clone(), rows)).is_ok()
                })
            });
            for _ in 0..threads {
                let raw_rx = raw_rx.clone();
                let decoded_tx = decoded_tx.clone();
                scope.spawn(move || {
                    for (seq, name, transform, rows) in raw_rx {
                        if decoded_tx.send((seq, decode_rows(&name, &transform, rows))).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(raw_rx);
            drop(decoded_tx);
            let result = emit_in_order(decoded_rx, next);
            let read_result = reader.join().map_err(|_| anyhow!("geopackage reader thread panicked"))?;
            result.and(read_result)
        })
    }
}

impl Drop for GeoPackageReader {
    fn drop(&mut self) {
        if !self.keep_unzipped
            && let Some(extracted) = &self.extracted
        {
            file_utils::delete_file(extracted);
        }
    }
}

/// Entry points that run a profile on geopackages (Java's static `GeoPackageReader.process`).
impl GeoPackageReader {
    /// Renders map features for all elements from the geopackages at `source_paths` (`.gpkg` files or paths inside
    /// zip files, extracted to `tmp_dir`) based on the mapping logic defined in `profile`, writing them to `writer`.
    ///
    /// `source_projection` overrides the CRS declared in the geopackage. When `keep_unzipped` is true, extracted
    /// files are kept in `tmp_dir` to speed up subsequent runs.
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        source_projection: Option<&str>,
        source_name: &str,
        source_paths: Vec<PathBuf>,
        tmp_dir: &Path,
        writer: &std::sync::Arc<crate::collection::feature_group::FeatureGroup>,
        config: &std::sync::Arc<crate::config::planetiler_config::PlanetilerConfig>,
        profile: &std::sync::Arc<dyn crate::profile::Profile>,
        stats: crate::stats::stats::StatsRef,
        keep_unzipped: bool,
    ) -> Result<()> {
        let projection = source_projection.map(str::to_string);
        let name = source_name.to_string();
        let tmp_dir = tmp_dir.to_path_buf();
        let bounds = config.bounds.clone();
        crate::reader::source_feature_processor::process_files(
            source_name,
            source_paths,
            move |path: &Path| {
                GeoPackageReader::new(projection.as_deref(), &name, path, &tmp_dir, keep_unzipped, Some(&bounds))
            },
            writer,
            config,
            profile,
            stats,
        )
    }
}

#[cfg(test)]
mod tests {
    use geo::Centroid;

    use super::*;
    use crate::geo::envelope::Envelope;

    fn resource(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources").join(name)
    }

    #[test]
    fn read_geopackage() {
        let tmp = tempfile::tempdir().unwrap();
        let path_outside_zip = resource("geopackage.gpkg");
        let zip_path = resource("geopackage.gpkg.zip");
        let path_in_zip = file_utils::walk_path_with_pattern(&zip_path, "*.gpkg").unwrap().remove(0);
        for keep_unzipped in [true, false] {
            for path in [&path_outside_zip, &path_in_zip] {
                for proj in [None, Some("EPSG:4326")] {
                    let reader = GeoPackageReader::new(proj, "test", path, tmp.path(), keep_unzipped, None).unwrap();
                    for iter in 0..2 {
                        let id = format!("path={path:?} proj={proj:?} iter={iter}");
                        assert_eq!(86, reader.feature_count().unwrap(), "{id}");
                        let features = reader.read_all().unwrap();
                        assert_eq!(86, features.len(), "{id}");
                        let mut names = vec![];
                        for f in &features {
                            assert!(matches!(f.get_tag("name"), Some(Value::String(_))), "{id}");
                            assert_eq!(Some("stations"), f.source_layer.as_deref());
                            names.push(f.get_tag("name").unwrap().to_string_lossy().to_string());
                        }
                        assert!(names.contains(&"Van Dörn Street".to_string()), "{id}");
                        let gc = geo_types::GeometryCollection(features.iter().map(|f| f.geometry.clone()).collect());
                        let c = gc.centroid().unwrap();
                        assert!((c.x() - -77.0297995).abs() < 1e-5, "{id} {c:?}");
                        assert!((c.y() - 38.9119684).abs() < 1e-5, "{id} {c:?}");
                        assert_eq!((1..=86).collect::<Vec<i64>>(), features.iter().map(|f| f.id).collect::<Vec<_>>());
                        assert!(features[0].tags.contains_key("fid"));
                        assert!(!features[0].tags.contains_key("geom"));
                    }
                    let extracted = reader.extracted.clone();
                    drop(reader);
                    if let Some(extracted) = extracted {
                        assert!(!extracted.exists());
                    }
                }
            }
        }
    }

    #[test]
    fn read_geopackage_spatial_index() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, expected) in [("geopackage.gpkg", 4), ("geopackage-unindexed.gpkg", 86)] {
            let bounds = Bounds::new(Some(Envelope::new(-77.0306, -77.0192, 38.8894, 38.9014)));
            let reader =
                GeoPackageReader::new(Some("EPSG:4326"), "test", &resource(name), tmp.path(), false, Some(&bounds))
                    .unwrap();
            assert_eq!(86, reader.feature_count().unwrap(), "{name}");
            assert_eq!(expected, reader.read_all().unwrap().len(), "{name}");
        }
    }

    #[test]
    fn read_empty_geopackage() {
        let tmp = tempfile::tempdir().unwrap();
        let reader =
            GeoPackageReader::new(None, "test", &resource("empty-geom.gpkg"), tmp.path(), false, None).unwrap();
        assert_eq!(1, reader.feature_count().unwrap());
        assert_eq!(0, reader.read_all().unwrap().len());
    }

    #[test]
    fn single_threaded() {
        let tmp = tempfile::tempdir().unwrap();
        let reader = GeoPackageReader::new(None, "test", &resource("geopackage.gpkg"), tmp.path(), false, None)
            .unwrap()
            .with_decode_threads(1);
        assert_eq!(86, reader.read_all().unwrap().len());
    }

    #[test]
    fn invalid_crs() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            GeoPackageReader::new(Some("garbage"), "test", &resource("geopackage.gpkg"), tmp.path(), false, None)
                .is_err()
        );
    }

    #[test]
    fn geometry_header() {
        let header = parse_geometry_header(
            &[b'G', b'P', 0, 0x03, 0xE6, 0x10, 0, 0].iter().copied().chain([0u8; 32]).collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(GeoPackageGeometryHeader { srs_id: 4326, empty: false, wkb_offset: 40 }, header);
        assert!(parse_geometry_header(b"XX000000").is_err());
    }
}

#[cfg(test)]
mod process_tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::reader::source_feature_processor::test_support::*;
    use crate::util::file_utils;

    #[test]
    fn process_geopackage_with_profile() {
        let resources = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources");
        let in_zip =
            file_utils::walk_path_with_pattern(&resources.join("geopackage.gpkg.zip"), "*.gpkg").unwrap().remove(0);
        for (write_threads, path) in [(false, resources.join("geopackage.gpkg")), (true, in_zip)] {
            let tmp = tempfile::tempdir().unwrap();
            let profile = Arc::new(CollectingProfile::default());
            let output = run_with_group(config(write_threads), profile.clone(), |group, config, profile, stats| {
                GeoPackageReader::process(
                    None,
                    "gpkg",
                    vec![path.clone()],
                    tmp.path(),
                    group,
                    config,
                    profile,
                    stats,
                    false,
                )
            });
            profile.assert_saw("gpkg", &[("stations", 86)]);
            assert_eq!(86, output.len());
            assert_eq!(vec!["gpkg"], *profile.finished.lock());
        }
    }
}
