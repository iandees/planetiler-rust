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

use crate::geo::geo_utils;
use crate::reader::simple_reader::{
    FeatureSink, ReaderFeature, SimpleReader, default_decode_threads, extract_from_zip,
};
use crate::util::file_utils;
use crate::value::{Tags, Value};

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
        let conn = Connection::open_with_flags(
            &self.db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
        )?;
        Ok(conn)
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
    fn read_table(
        &self,
        conn: &Connection,
        table: &str,
        first_id: i64,
        emit: &mut dyn FnMut(Vec<ReaderFeature>) -> bool,
    ) -> Result<bool> {
        const BATCH: usize = 512;
        let mut stmt = conn.prepare(&format!("SELECT * FROM {table};"))?;
        let columns: Vec<String> = stmt.column_names().into_iter().map(str::to_string).collect();
        let Some(geometry_column) = columns.iter().position(|c| c == "GEOMETRY") else {
            return Ok(true);
        };
        let layer = Some(table.to_string());
        let mut rows = stmt.query([])?;
        let mut id = first_id;
        let mut batch = Vec::with_capacity(BATCH);
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
            let geometry = geo_utils::read_wkb(geometry).map_err(|e| anyhow!("{e} in {table} (row {id})"))?;
            let mut tags = Tags::with_capacity(columns.len() - 1);
            for (c, name) in columns.iter().enumerate() {
                if c != geometry_column {
                    tags.insert(name.clone(), sqlite_value(row.get_ref(c)?));
                }
            }
            batch.push(ReaderFeature { geometry, tags, source_layer: layer.clone(), id });
            if batch.len() >= BATCH && !emit(std::mem::replace(&mut batch, Vec::with_capacity(BATCH))) {
                return Ok(false);
            }
        }
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
                            if !self.read_table(&conn, table, *first_id, &mut |batch| tx.send(Ok(batch)).is_ok())? {
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
    fn valid_table_names() {
        assert!(is_valid_table_name("ne_10m_admin_0_countries"));
        assert!(is_valid_table_name("NE_110M_LAND"));
        assert!(!is_valid_table_name("ne_"));
        assert!(!is_valid_table_name("ne_a-b"));
        assert!(!is_valid_table_name("geometry_columns"));
    }
}
