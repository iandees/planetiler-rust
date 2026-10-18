//! Port of `reader/NaturalEarthReader.java`: reads features from the geometries contained in a
//! [Natural Earth](https://www.naturalearthdata.com/) sqlite distribution.
//!
//! The input can be the `.sqlite` file itself or a `.zip` file containing it, which is extracted to `tmp_dir` first.
//! Every table matching `ne_[a-z0-9_]+` (case-insensitive) with a `GEOMETRY` column becomes a source layer of the same
//! name, every other column becomes a tag. Like Java, feature IDs are assigned sequentially starting at 1 across all
//! tables (in table-name order), skipping rows without a geometry.
//!
//! Unlike Java, tables are read in parallel by separate read-only connections. Features within a table are emitted in
//! row order, but features from different tables may interleave.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow};
use parking_lot::Mutex;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use tracing::{trace, warn};

use crate::geo::envelope::Envelope;
use crate::geo::geo_utils;
use crate::reader::simple_reader::{
    FeatureSink, ReaderFeature, SimpleReader, default_decode_threads, extract_from_zip,
};
use crate::util::file_utils;
use crate::value::Value;

/// Reads features from a Natural Earth sqlite file.
pub struct NaturalEarthReader {
    source_name: String,
    db_path: PathBuf,
    /// Extracted sqlite file to delete when this reader is dropped.
    extracted: Option<PathBuf>,
    keep_unzipped: bool,
    threads: usize,
    /// Cached `(table, number of rows with geometry)`.
    table_counts: Mutex<Option<Vec<(String, u64)>>>,
}

fn is_valid_table_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.len() > 3
        && lower.starts_with("ne_")
        && lower[3..].bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Memory-mapped I/O size for read connections; capped by SQLITE_MAX_MMAP_SIZE (2 GB), larger files fall back to
/// regular reads past that offset.
const MMAP_SIZE: i64 = 1 << 40;

/// Opens a read-only connection to the sqlite file at `path`, set up so that many threads with their own connection can
/// read concurrently without contending on sqlite's global locks.
///
/// The bundled sqlite is built with `SQLITE_ENABLE_MEMORY_MANAGEMENT`, so every connection shares one page cache
/// guarded by a global mutex, and allocations take another global mutex to track memory statistics. Reading through
/// memory-mapped I/O bypasses the page cache for read-only transactions, and memory statistics are switched off
/// (this only works before sqlite is first initialized in the process; otherwise it is a no-op).
pub(crate) fn open_sqlite_for_parallel_reads(path: &Path) -> Result<Connection> {
    static DISABLE_MEMSTATUS: std::sync::Once = std::sync::Once::new();
    DISABLE_MEMSTATUS.call_once(|| {
        // SAFETY: sqlite3_config is documented as safe to call before sqlite3_initialize (it returns SQLITE_MISUSE
        // otherwise); no connection is being opened concurrently from this call
        unsafe {
            rusqlite::ffi::sqlite3_config(rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS, 0 as std::ffi::c_int);
        }
    });
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.pragma_update(None, "mmap_size", MMAP_SIZE)?;
    Ok(conn)
}

/// Convert a sqlite column value to a tag value (like the sqlite JDBC driver's `getObject`).
pub(crate) fn sqlite_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(d) => Value::Double(d),
        ValueRef::Text(t) => Value::str(String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => Value::Bytes(b.into()),
    }
}

impl NaturalEarthReader {
    /// Opens a Natural Earth sqlite file, or zip file containing one (extracted into `tmp_dir`). If `keep_unzipped` is
    /// true, the extracted file is kept after this reader is dropped and reused next time if it is newer than the zip
    /// file.
    pub fn new(source_name: &str, input: &Path, tmp_dir: &Path, keep_unzipped: bool) -> Result<NaturalEarthReader> {
        crate::util::log_util::set_stage(source_name);
        let extracted = if file_utils::has_extension(input, "zip") {
            extract_from_zip(input, "sqlite", tmp_dir, keep_unzipped)?
        } else {
            None
        };
        let db_path = extracted.clone().unwrap_or_else(|| input.to_path_buf());
        let reader = NaturalEarthReader {
            source_name: source_name.to_string(),
            db_path,
            extracted: if keep_unzipped { None } else { extracted },
            keep_unzipped,
            threads: default_decode_threads(),
            table_counts: Mutex::new(None),
        };
        // fail fast if the file is not a sqlite database
        reader.table_names().with_context(|| format!("opening {}", input.display()))?;
        Ok(reader)
    }

    /// Sets the number of tables to read in parallel.
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads.max(1);
        self
    }

    /// Path to the (possibly extracted) sqlite file.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn open(&self) -> Result<Connection> {
        open_sqlite_for_parallel_reads(&self.db_path)
    }

    /// Returns the names of all tables matching `ne_[a-z0-9_]+`, sorted like JDBC `getTables` (by type, then name).
    pub fn table_names(&self) -> Result<Vec<String>> {
        let conn = self.open()?;
        let mut stmt =
            conn.prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'view') ORDER BY type, name")?;
        let names = stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(names.into_iter().filter(|n| is_valid_table_name(n)).collect())
    }

    fn table_counts(&self) -> Result<Vec<(String, u64)>> {
        let mut cached = self.table_counts.lock();
        if let Some(counts) = cached.as_ref() {
            return Ok(counts.clone());
        }
        let conn = self.open()?;
        let mut result = vec![];
        for table in self.table_names()? {
            // table name checked against a regex
            let count = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table} WHERE GEOMETRY IS NOT NULL;"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap_or(0); // maybe no GEOMETRY column?
            result.push((table, count.max(0) as u64));
        }
        *cached = Some(result.clone());
        Ok(result)
    }

    /// Reads one table, emitting batches of features to `emit`. Returns `false` from `emit` to stop.
    /// Rows whose geometry certainly does not intersect `bounds` are skipped without decoding them and reported to
    /// `skipped`.
    #[allow(clippy::too_many_arguments)]
    fn read_table(
        &self,
        conn: &Connection,
        table: &str,
        first_id: i64,
        bounds: Option<&Envelope>,
        skipped: &dyn Fn(u64),
        emit: &mut dyn FnMut(Vec<ReaderFeature>) -> bool,
    ) -> Result<bool> {
        const BATCH: usize = 512;
        let mut stmt = conn.prepare(&format!("SELECT * FROM {table};"))?;
        let columns: Vec<String> = stmt.column_names().into_iter().map(str::to_string).collect();
        let Some(geometry_column) = columns.iter().position(|c| c == "GEOMETRY") else {
            return Ok(true);
        };
        let layer: Option<std::sync::Arc<str>> = Some(table.into());
        let mut rows = stmt.query([])?;
        let mut id = first_id;
        let mut batch = Vec::with_capacity(BATCH);
        let mut num_skipped = 0;
        while let Some(row) = rows.next()? {
            let geometry = match row.get_ref(geometry_column)? {
                ValueRef::Blob(b) => b,
                ValueRef::Null => continue,
                other => {
                    warn!("Unexpected geometry type in {table}: {:?}", other.data_type());
                    continue;
                }
            };
            id += 1;
            if let Some(bounds) = bounds
                && geo_utils::wkb_envelope(geometry).is_some_and(|env| !env.intersects(bounds))
            {
                num_skipped += 1;
                continue;
            }
            let geometry = geo_utils::read_wkb(geometry).map_err(|e| anyhow!("{e} in {table} (row {id})"))?;
            let mut tags = crate::value::tags_with_capacity(columns.len() - 1);
            for (c, name) in columns.iter().enumerate() {
                if c != geometry_column {
                    tags.insert(name.clone(), sqlite_value(row.get_ref(c)?));
                }
            }
            batch.push(ReaderFeature { geometry, tags, source_layer: layer.clone(), id });
            if batch.len() >= BATCH {
                skipped(std::mem::take(&mut num_skipped));
                if !emit(std::mem::replace(&mut batch, Vec::with_capacity(BATCH))) {
                    return Ok(false);
                }
            }
        }
        skipped(num_skipped);
        if !batch.is_empty() {
            return Ok(emit(batch));
        }
        Ok(true)
    }
}

impl SimpleReader for NaturalEarthReader {
    fn source_name(&self) -> &str {
        &self.source_name
    }

    fn feature_count(&self) -> Result<u64> {
        Ok(self.table_counts()?.iter().map(|(_, c)| c).sum())
    }

    fn read_features(&self, next: &mut FeatureSink<'_>) -> Result<()> {
        self.read(None, &|_| {}, next)
    }

    fn read_features_within(
        &self,
        bounds: &Envelope,
        skipped: &(dyn Fn(u64) + Sync),
        next: &mut FeatureSink<'_>,
    ) -> Result<()> {
        self.read(Some(bounds), skipped, next)
    }
}

impl NaturalEarthReader {
    fn read(
        &self,
        bounds: Option<&Envelope>,
        skipped: &(dyn Fn(u64) + Sync),
        next: &mut FeatureSink<'_>,
    ) -> Result<()> {
        let counts = self.table_counts()?;
        // ID offsets so that IDs match Java's sequential assignment while reading tables in parallel
        let mut tables = Vec::with_capacity(counts.len());
        let mut offset = 0i64;
        for (table, count) in &counts {
            tables.push((table.as_str(), offset));
            offset += *count as i64;
        }
        let threads = self.threads.min(tables.len()).max(1);
        let next_table = AtomicUsize::new(0);
        std::thread::scope(|scope| -> Result<()> {
            let (tx, rx) = crossbeam_channel::bounded::<Result<Vec<ReaderFeature>>>(threads * 4);
            for _ in 0..threads {
                let tx = tx.clone();
                let tables = &tables;
                let next_table = &next_table;
                scope.spawn(move || {
                    let result = (|| -> Result<()> {
                        let conn = self.open()?;
                        loop {
                            let i = next_table.fetch_add(1, Ordering::Relaxed);
                            let Some((table, first_id)) = tables.get(i) else { return Ok(()) };
                            trace!("Naturalearth loading {i}/{}: {table}", tables.len());
                            conn.pragma_update(None, "mmap_size", MMAP_SIZE)?;
                            let more = self.read_table(&conn, table, *first_id, bounds, skipped, &mut |batch| {
                                tx.send(Ok(batch)).is_ok()
                            })?;
                            // Unmap the file after each table: pages of a memory-mapped file count as resident
                            // memory while they stay mapped, so keeping every connection's mapping until all tables
                            // are read made the whole ~850 MB file resident at once (most of Monaco's peak RSS).
                            conn.pragma_update(None, "mmap_size", 0)?;
                            if !more {
                                return Ok(());
                            }
                        }
                    })();
                    if let Err(e) = result {
                        let _ = tx.send(Err(e));
                    }
                });
            }
            drop(tx);
            for batch in rx {
                for feature in batch? {
                    next(feature)?;
                }
            }
            Ok(())
        })
    }
}

impl Drop for NaturalEarthReader {
    fn drop(&mut self) {
        if !self.keep_unzipped
            && let Some(extracted) = &self.extracted
        {
            file_utils::delete_file(extracted);
        }
    }
}

/// Entry points that run a profile on natural earth (Java's static `NaturalEarthReader.process`).
impl NaturalEarthReader {
    /// Renders map features for all elements from the natural earth sqlite file at `source_path` (or a zip file
    /// containing it, extracted to `tmp_dir`) based on the mapping logic defined in `profile`, writing them to
    /// `writer`. When `keep_unzipped` is true, the extracted file is kept to speed up subsequent runs.
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        source_name: &str,
        source_path: &Path,
        tmp_dir: &Path,
        writer: &std::sync::Arc<crate::collection::feature_group::FeatureGroup>,
        config: &std::sync::Arc<crate::config::planetiler_config::PlanetilerConfig>,
        profile: &std::sync::Arc<dyn crate::profile::Profile>,
        stats: crate::stats::stats::StatsRef,
        keep_unzipped: bool,
    ) -> Result<()> {
        let name = source_name.to_string();
        let tmp_dir = tmp_dir.to_path_buf();
        crate::reader::source_feature_processor::process_files(
            source_name,
            vec![source_path.to_path_buf()],
            move |path: &Path| NaturalEarthReader::new(&name, path, &tmp_dir, keep_unzipped),
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

    fn resource(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources").join(name)
    }

    fn check(reader: &NaturalEarthReader) {
        for i in 1..=2 {
            assert_eq!(7_679, reader.feature_count().unwrap(), "iter {i}");
            let mut points = vec![];
            let mut ids = vec![];
            reader
                .read_features(&mut |f| {
                    ids.push(f.id);
                    if f.source_layer.as_deref() == Some("ne_110m_geography_regions_elevation_points") {
                        assert!(
                            matches!(f.get_tag("elevation"), Some(Value::Double(_))),
                            "{:?}",
                            f.get_tag("elevation")
                        );
                        points.push(f.geometry);
                    }
                    Ok(())
                })
                .unwrap();
            assert_eq!(19, points.len());
            let c = geo_types::GeometryCollection(points).centroid().unwrap();
            assert!((c.x() - 14.22422).abs() < 5.0 && (c.y() - 12.994629).abs() < 5.0, "iter {i}: {c:?}");
            ids.sort();
            assert_eq!((1..=7_679).collect::<Vec<i64>>(), ids);
        }
    }

    #[test]
    fn read_natural_earth_zip() {
        for keep_unzipped in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let reader = NaturalEarthReader::new(
                "test",
                &resource("natural_earth_vector.sqlite.zip"),
                tmp.path(),
                keep_unzipped,
            )
            .unwrap();
            let extracted = reader.db_path().to_path_buf();
            assert!(extracted.starts_with(tmp.path()));
            check(&reader);
            drop(reader);
            assert_eq!(keep_unzipped, extracted.exists());
        }
    }

    #[test]
    fn read_natural_earth_sqlite() {
        let tmp = tempfile::tempdir().unwrap();
        file_utils::unzip_file(&resource("natural_earth_vector.sqlite.zip"), tmp.path()).unwrap();
        let path = tmp.path().join("packages/natural_earth_vector.sqlite");
        let reader = NaturalEarthReader::new("test", &path, tmp.path(), false).unwrap();
        check(&reader);
        let single = NaturalEarthReader::new("test", &path, tmp.path(), false).unwrap().with_threads(1);
        let mut first = vec![];
        single
            .read_features(&mut |f| {
                if first.len() < 3 {
                    first.push((f.id, f.source_layer.clone()));
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(1, first[0].0);
        drop(reader);
        assert!(path.exists());
    }

    #[test]
    fn read_natural_earth_within_bounds_skips_the_same_features_the_processor_would() {
        let tmp = tempfile::tempdir().unwrap();
        let reader =
            NaturalEarthReader::new("test", &resource("natural_earth_vector.sqlite.zip"), tmp.path(), false).unwrap();
        let bounds = Envelope::new(0.0, 20.0, 0.0, 30.0);
        let mut expected = vec![];
        reader
            .read_features(&mut |f| {
                if Envelope::of_geometry(&f.geometry).intersects(&bounds) {
                    expected.push((f.id, f.geometry));
                }
                Ok(())
            })
            .unwrap();
        let skipped = std::sync::atomic::AtomicU64::new(0);
        let mut actual = vec![];
        reader
            .read_features_within(&bounds, &|n| _ = skipped.fetch_add(n, Ordering::Relaxed), &mut |f| {
                if Envelope::of_geometry(&f.geometry).intersects(&bounds) {
                    actual.push((f.id, f.geometry));
                }
                Ok(())
            })
            .unwrap();
        expected.sort_by_key(|(id, _)| *id);
        actual.sort_by_key(|(id, _)| *id);
        assert!(!expected.is_empty() && expected.len() < 7_679, "{}", expected.len());
        assert_eq!(expected, actual);
        let skipped = skipped.load(Ordering::Relaxed);
        assert!(skipped > 0 && skipped <= 7_679 - expected.len() as u64, "{skipped}");
    }

    #[test]
    fn valid_table_names() {
        assert!(is_valid_table_name("ne_10m_admin_0_countries"));
        assert!(is_valid_table_name("NE_110M_LAND"));
        assert!(!is_valid_table_name("ne_"));
        assert!(!is_valid_table_name("ne_a-b"));
        assert!(!is_valid_table_name("geometry_columns"));
    }
}

#[cfg(test)]
mod process_tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::reader::source_feature_processor::test_support::*;

    #[test]
    fn process_natural_earth_with_profile() {
        let zip = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources/natural_earth_vector.sqlite.zip");
        let tmp = tempfile::tempdir().unwrap();
        let profile = Arc::new(CollectingProfile::default());
        let output = run_with_group(config(true), profile.clone(), |group, config, profile, stats| {
            NaturalEarthReader::process("ne", &zip, tmp.path(), group, config, profile, stats, false)
        });
        // features entirely outside the web mercator latitude range are skipped like in Java
        let reader = NaturalEarthReader::new("ne", &zip, tmp.path(), false).unwrap();
        let world = *crate::geo::geo_utils::WORLD_LAT_LON_BOUNDS;
        let in_bounds = reader
            .read_all()
            .unwrap()
            .iter()
            .filter(|f| crate::geo::envelope::Envelope::of_geometry(&f.geometry).intersects(&world))
            .count();
        assert!(in_bounds > 7_600, "{in_bounds}");
        let seen = profile.seen.lock();
        assert_eq!(in_bounds, seen.len());
        assert!(seen.iter().all(|(source, _, _)| source == "ne"));
        assert_eq!(
            19,
            seen.iter().filter(|(_, layer, _)| layer == "ne_110m_geography_regions_elevation_points").count()
        );
        drop(seen);
        assert_eq!(
            19,
            output.iter().filter(|(_, layer, _)| layer == "ne_110m_geography_regions_elevation_points").count()
        );
        assert_eq!(vec!["ne"], *profile.finished.lock());
    }
}
