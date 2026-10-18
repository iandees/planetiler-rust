//! Port of `mbtiles/Mbtiles.java`: interface into an mbtiles sqlite file containing tiles and metadata about the
//! tileset.
//!
//! See the [MBTiles Specification](https://github.com/mapbox/mbtiles-spec/blob/master/1.3/spec.md).
//!
//! In "compact" mode (the default), tile data is deduplicated into a `tiles_data` table referenced from a
//! `tiles_shallow` table, and `tiles` is a view joining both.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Context;
use parking_lot::{Mutex, MutexGuard};
use rusqlite::{Connection, OpenFlags, Statement};
use tracing::{debug, error, info, warn};

use crate::archive::readable_tile_archive::{ArchiveIter, ReadableTileArchive};
use crate::archive::tile::Tile;
use crate::archive::tile_archive_config::ArchiveOptions;
use crate::archive::tile_archive_metadata::TileArchiveMetadata;
use crate::archive::tile_archive_metadata_de_ser;
use crate::archive::tile_encoding_result::TileEncodingResult;
use crate::archive::writeable_tile_archive::{TileWriter, WriteableTileArchive};
use crate::geo::tile_coord::TileCoord;
use crate::geo::tile_order::TileOrder;

// Options that can be set through "file.mbtiles?compact=true" query parameters
// or "file.mbtiles" with "--mbtiles-compact=true" command-line flag
pub const COMPACT_DB: &str = "compact";
pub const SKIP_INDEX_CREATION: &str = "no_index";
pub const VACUUM_ANALYZE: &str = "vacuum_analyze";

pub const LEGACY_COMPACT_DB: &str = "compact_db";
pub const LEGACY_SKIP_INDEX_CREATION: &str = "skip_mbtiles_index_creation";
pub const LEGACY_VACUUM_ANALYZE: &str = "optimize_db";

// https://www.sqlite.org/src/artifact?ci=trunk&filename=magic.txt
const MBTILES_APPLICATION_ID: i64 = 0x4d504258;

const TILES_TABLE: &str = "tiles";
const TILES_COL_X: &str = "tile_column";
const TILES_COL_Y: &str = "tile_row";
const TILES_COL_Z: &str = "zoom_level";
const TILES_COL_DATA: &str = "tile_data";

const TILES_DATA_TABLE: &str = "tiles_data";
const TILES_DATA_COL_DATA_ID: &str = "tile_data_id";
const TILES_DATA_COL_DATA: &str = "tile_data";

const TILES_SHALLOW_TABLE: &str = "tiles_shallow";
const TILES_SHALLOW_COL_X: &str = TILES_COL_X;
const TILES_SHALLOW_COL_Y: &str = TILES_COL_Y;
const TILES_SHALLOW_COL_Z: &str = TILES_COL_Z;
const TILES_SHALLOW_COL_DATA_ID: &str = TILES_DATA_COL_DATA_ID;

const METADATA_TABLE: &str = "metadata";
const METADATA_COL_NAME: &str = "name";
const METADATA_COL_VALUE: &str = "value";

const MAX_PARAMETERS_IN_PREPARED_STATEMENT: usize = 999;

/// Pragmas that can be overridden through archive options (equivalent of the xerial `SQLiteConfig.Pragma` values).
const PRAGMAS: &[&str] = &[
    "application_id",
    "auto_vacuum",
    "busy_timeout",
    "cache_size",
    "case_sensitive_like",
    "count_changes",
    "default_cache_size",
    "defer_foreign_keys",
    "empty_result_callbacks",
    "encoding",
    "foreign_keys",
    "full_column_names",
    "fullfsync",
    "incremental_vacuum",
    "journal_mode",
    "journal_size_limit",
    "legacy_alter_table",
    "legacy_file_format",
    "locking_mode",
    "max_page_count",
    "mmap_size",
    "page_size",
    "read_uncommitted",
    "recursive_triggers",
    "reverse_unordered_selects",
    "secure_delete",
    "short_column_names",
    "synchronous",
    "temp_store",
    "temp_store_directory",
    "user_version",
];

/// Runs a statement (like a pragma) that may return rows, discarding them.
fn run(conn: &Connection, sql: &str) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([])?;
    while rows.next()?.is_some() {}
    Ok(())
}

fn flip_y(y: u32, z: u32) -> i64 {
    (1i64 << z) - 1 - y as i64
}

fn result_coord(z: i64, x: i64, raw_y: i64) -> TileCoord {
    TileCoord::of_xyz(x as u32, ((1i64 << z) - 1 - raw_y) as u32, z as u32)
}

/// A row of a paged tiles query: (z, x, raw y, data).
type PageRow = (i64, i64, i64, Option<Vec<u8>>);

pub struct Mbtiles {
    connection: Mutex<Connection>,
    compact_db: bool,
    skip_index_creation: bool,
    vacuum_analyze: bool,
    path: Option<PathBuf>,
}

impl Mbtiles {
    fn new(connection: Connection, arguments: &ArchiveOptions, path: Option<PathBuf>) -> Mbtiles {
        Mbtiles {
            connection: Mutex::new(connection),
            compact_db: arguments.get_boolean(
                &format!("{COMPACT_DB}|{LEGACY_COMPACT_DB}"),
                "mbtiles: reduce the DB size by separating and deduping the tile data",
                true,
            ),
            skip_index_creation: arguments.get_boolean(
                &format!("{SKIP_INDEX_CREATION}|{LEGACY_SKIP_INDEX_CREATION}"),
                "mbtiles: skip adding index to sqlite DB",
                false,
            ),
            vacuum_analyze: arguments.get_boolean(
                &format!("{VACUUM_ANALYZE}|{LEGACY_VACUUM_ANALYZE}"),
                "mbtiles: vacuum analyze sqlite DB after writing",
                false,
            ),
            path,
        }
    }

    fn configure(conn: &Connection, defaults: &[(&str, String)], args: &ArchiveOptions) -> anyhow::Result<()> {
        let mut pragmas: Vec<(String, String)> = defaults.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        for pragma in PRAGMAS {
            if let Some(value) = args.get_arg(pragma) {
                info!("Setting custom mbtiles sqlite pragma {pragma}={value}");
                pragmas.retain(|(k, _)| k != pragma);
                pragmas.push((pragma.to_string(), value));
            }
        }
        for (k, v) in pragmas {
            run(conn, &format!("PRAGMA {k}={v}")).with_context(|| format!("failed to set pragma {k}={v}"))?;
        }
        Ok(())
    }

    /// Returns a new mbtiles file that won't get written to disk. Useful for toy use-cases like unit tests.
    pub fn new_in_memory_database(compact_db: bool) -> Mbtiles {
        Self::new_in_memory_database_with_options(&ArchiveOptions::of_pairs(&[(
            COMPACT_DB,
            if compact_db { "true" } else { "false" },
        )]))
        .expect("in-memory database")
    }

    /// Returns an in-memory database with extra mbtiles and pragma options set from `options`.
    pub fn new_in_memory_database_with_options(options: &ArchiveOptions) -> anyhow::Result<Mbtiles> {
        let conn = Connection::open_in_memory()?;
        Self::configure(&conn, &[("application_id", MBTILES_APPLICATION_ID.to_string())], options)?;
        Ok(Self::new(conn, options, None))
    }

    /// Returns a new connection to an mbtiles file optimized for fast bulk writes with extra mbtiles and pragma
    /// options set from `options`.
    pub fn new_write_to_file_database(path: &Path, options: &ArchiveOptions) -> anyhow::Result<Mbtiles> {
        let conn = Connection::open(path).with_context(|| format!("Unable to open {}", path.display()))?;
        Self::configure(
            &conn,
            &[
                ("journal_mode", "OFF".into()),
                ("synchronous", "OFF".into()),
                ("cache_size", "1000000".into()), // 1GB
                ("locking_mode", "EXCLUSIVE".into()),
                ("temp_store", "MEMORY".into()),
                ("application_id", MBTILES_APPLICATION_ID.to_string()),
            ],
            options,
        )?;
        Ok(Self::new(conn, options, Some(path.to_path_buf())))
    }

    /// Returns a new connection to an mbtiles file optimized for reads.
    pub fn new_read_only_database(path: &Path) -> anyhow::Result<Mbtiles> {
        Self::new_read_only_database_with_options(path, &ArchiveOptions::empty())
    }

    /// Returns a new connection to an mbtiles file optimized for reads with extra mbtiles and pragma options set from
    /// `options`.
    pub fn new_read_only_database_with_options(path: &Path, options: &ArchiveOptions) -> anyhow::Result<Mbtiles> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("Unable to open {}", path.display()))?;
        Self::configure(
            &conn,
            &[("cache_size", "100000".into()), ("locking_mode", "EXCLUSIVE".into()), ("page_size", "32768".into())],
            options,
        )?;
        Ok(Self::new(conn, options, None))
    }

    /// The underlying sqlite connection.
    pub fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock()
    }

    pub fn skip_index_creation(&self) -> bool {
        self.skip_index_creation
    }

    pub fn compact_db(&self) -> bool {
        self.compact_db
    }

    fn execute(&self, queries: &[String]) -> anyhow::Result<&Self> {
        let conn = self.connection.lock();
        for query in queries {
            debug!("Execute mbtiles: {query}");
            run(&conn, query).with_context(|| format!("Error executing queries {}", queries.join(",")))?;
        }
        Ok(self)
    }

    /// Creates the required tables (and views) but skips index creation on some tables. Those indexes should be added
    /// later manually as described in [`Mbtiles::get_manual_index_creation_statements`].
    pub fn create_tables_without_indexes(&self) -> anyhow::Result<&Self> {
        self.create_tables(true)
    }

    /// Creates the required tables (and views) including all indexes.
    pub fn create_tables_with_indexes(&self) -> anyhow::Result<&Self> {
        self.create_tables(false)
    }

    fn create_tables(&self, skip_index_creation: bool) -> anyhow::Result<&Self> {
        let mut ddl = vec![
            format!("create table {METADATA_TABLE} ({METADATA_COL_NAME} text, {METADATA_COL_VALUE} text);"),
            format!("create unique index name on {METADATA_TABLE} ({METADATA_COL_NAME});"),
        ];
        if self.compact_db {
            // "primary key without rowid" results in a clustered index which is much more compact and performant
            // (r/w) than "unique" which results in a non-clustered index
            let pk = if skip_index_creation {
                String::new()
            } else {
                format!(", primary key({TILES_SHALLOW_COL_Z},{TILES_SHALLOW_COL_X},{TILES_SHALLOW_COL_Y})")
            };
            ddl.push(format!(
                "create table {TILES_SHALLOW_TABLE} (\n  {TILES_SHALLOW_COL_Z} integer,\n  {TILES_SHALLOW_COL_X} \
                 integer,\n  {TILES_SHALLOW_COL_Y} integer,\n  {TILES_SHALLOW_COL_DATA_ID} integer\n  {pk}\n) {}",
                if skip_index_creation { "" } else { "without rowid" }
            ));
            // here it's not worth to skip the "primary key"/index - doing so even hurts write performance
            ddl.push(format!(
                "create table {TILES_DATA_TABLE} (\n  {TILES_DATA_COL_DATA_ID} integer primary key,\n  \
                 {TILES_DATA_COL_DATA} blob\n)"
            ));
            ddl.push(format!(
                "create view {TILES_TABLE} AS\nselect\n  {TILES_SHALLOW_TABLE}.{TILES_SHALLOW_COL_Z} as \
                 {TILES_COL_Z},\n  {TILES_SHALLOW_TABLE}.{TILES_SHALLOW_COL_X} as {TILES_COL_X},\n  \
                 {TILES_SHALLOW_TABLE}.{TILES_SHALLOW_COL_Y} as {TILES_COL_Y},\n  \
                 {TILES_DATA_TABLE}.{TILES_DATA_COL_DATA} as {TILES_COL_DATA}\nfrom {TILES_SHALLOW_TABLE}\njoin \
                 {TILES_DATA_TABLE} on {TILES_SHALLOW_TABLE}.{TILES_SHALLOW_COL_DATA_ID} = \
                 {TILES_DATA_TABLE}.{TILES_DATA_COL_DATA_ID}"
            ));
        } else {
            // here "primary key (with rowid)" is much more compact than a "primary key without rowid" because the tile
            // data is part of the table
            let pk = if skip_index_creation {
                String::new()
            } else {
                format!(", primary key({TILES_COL_Z},{TILES_COL_X},{TILES_COL_Y})")
            };
            ddl.push(format!(
                "create table {TILES_TABLE} (\n  {TILES_COL_Z} integer,\n  {TILES_COL_X} integer,\n  {TILES_COL_Y} \
                 integer,\n  {TILES_COL_DATA} blob\n  {pk}\n)"
            ));
        }
        self.execute(&ddl)
    }

    /// Returns the DDL statements to create the indexes manually when the option to skip index creation was chosen.
    pub fn get_manual_index_creation_statements(&self) -> Vec<String> {
        if self.compact_db {
            vec![format!(
                "create unique index tiles_shallow_index on {TILES_SHALLOW_TABLE} ({TILES_SHALLOW_COL_Z}, \
                 {TILES_SHALLOW_COL_X}, {TILES_SHALLOW_COL_Y})"
            )]
        } else {
            vec![format!(
                "create unique index tile_index on {TILES_TABLE} ({TILES_COL_Z}, {TILES_COL_X}, {TILES_COL_Y})"
            )]
        }
    }

    pub fn vacuum_analyze(&self) -> anyhow::Result<&Self> {
        self.execute(&["VACUUM;".to_string(), "ANALYZE;".to_string()])
    }

    /// Returns the contents of the metadata table.
    pub fn metadata_table(&self) -> Metadata<'_> {
        Metadata { mbtiles: self }
    }

    /// Keyset-paginated query over the `tiles` table/view ordered by (z, x, raw y).
    ///
    /// Avoids materializing the whole table and avoids holding a statement across iterator calls (which would borrow
    /// the connection mutex).
    fn paged_query<'a, T: 'a>(
        &'a self,
        with_data: bool,
        page_size: usize,
        map: impl Fn(i64, i64, i64, Option<Vec<u8>>) -> T + 'a,
    ) -> ArchiveIter<'a, T> {
        let data_col = if with_data { format!(", {TILES_COL_DATA}") } else { String::new() };
        let first = format!(
            "select {TILES_COL_Z}, {TILES_COL_X}, {TILES_COL_Y}{data_col} from {TILES_TABLE} order by {TILES_COL_Z}, \
             {TILES_COL_X}, {TILES_COL_Y} limit {page_size}"
        );
        let next = format!(
            "select {TILES_COL_Z}, {TILES_COL_X}, {TILES_COL_Y}{data_col} from {TILES_TABLE} where ({TILES_COL_Z}, \
             {TILES_COL_X}, {TILES_COL_Y}) > (?, ?, ?) order by {TILES_COL_Z}, {TILES_COL_X}, {TILES_COL_Y} limit \
             {page_size}"
        );
        let mut last: Option<(i64, i64, i64)> = None;
        let mut buffer: std::vec::IntoIter<PageRow> = Vec::new().into_iter();
        let mut done = false;
        Box::new(std::iter::from_fn(move || {
            loop {
                if let Some((z, x, y, data)) = buffer.next() {
                    return Some(Ok(map(z, x, y, data)));
                }
                if done {
                    return None;
                }
                let page: rusqlite::Result<Vec<PageRow>> = (|| {
                    let conn = self.connection.lock();
                    let mut stmt = conn.prepare_cached(if last.is_some() { &next } else { &first })?;
                    let read = |row: &rusqlite::Row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, if with_data { row.get(3)? } else { None }))
                    };
                    let rows = match last {
                        Some((z, x, y)) => stmt.query_map([z, x, y], read)?.collect::<rusqlite::Result<Vec<_>>>()?,
                        None => stmt.query_map([], read)?.collect::<rusqlite::Result<Vec<_>>>()?,
                    };
                    Ok(rows)
                })();
                match page {
                    Err(e) => {
                        done = true;
                        return Some(Err(anyhow::Error::new(e).context("Could not read tiles from mbtiles file")));
                    }
                    Ok(rows) => {
                        if rows.len() < page_size {
                            done = true;
                        }
                        if let Some((z, x, y, _)) = rows.last() {
                            last = Some((*z, *x, *y));
                        }
                        buffer = rows.into_iter();
                    }
                }
            }
        }))
    }
}

impl WriteableTileArchive for Mbtiles {
    fn deduplicates(&self) -> bool {
        self.compact_db
    }

    fn tile_order(&self) -> TileOrder {
        TileOrder::Tms
    }

    fn initialize(&mut self) -> anyhow::Result<()> {
        if self.skip_index_creation {
            self.create_tables_without_indexes()?;
            info!(
                "Skipping index creation. Add later by executing: {}",
                self.get_manual_index_creation_statements().join(" ; ")
            );
        } else {
            self.create_tables_with_indexes()?;
        }
        Ok(())
    }

    /// Returns a writer that queues up inserts into the tile database(s) into large batches before executing them.
    fn new_tile_writer(&self) -> anyhow::Result<Box<dyn TileWriter + Send + '_>> {
        if self.compact_db {
            Ok(Box::new(BatchedCompactTileWriter {
                shallow: BatchedTableWriter::new(&self.connection, TILES_SHALLOW_TABLE, &SHALLOW_COLUMNS, false),
                data: BatchedTableWriter::new(&self.connection, TILES_DATA_TABLE, &DATA_COLUMNS, true),
                tile_data_id_by_hash: HashMap::with_capacity(1_000),
                tile_data_id_counter: 1,
            }))
        } else {
            Ok(Box::new(BatchedNonCompactTileWriter {
                table: BatchedTableWriter::new(&self.connection, TILES_TABLE, &TILE_COLUMNS, false),
            }))
        }
    }

    fn finish(&mut self, metadata: &TileArchiveMetadata) -> anyhow::Result<()> {
        self.metadata_table().set(metadata);
        if self.vacuum_analyze {
            self.vacuum_analyze()?;
        }
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.path.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len()).unwrap_or(0)
    }
}

impl ReadableTileArchive for Mbtiles {
    fn get_tile(&self, x: u32, y: u32, z: u32) -> anyhow::Result<Option<Vec<u8>>> {
        let conn = self.connection.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {TILES_COL_DATA} FROM {TILES_TABLE} WHERE {TILES_COL_X}=? AND {TILES_COL_Y}=? AND {TILES_COL_Z}=?"
        ))?;
        let mut rows = stmt.query([x as i64, flip_y(y, z), z as i64]).context("Could not get tile")?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    fn get_all_tile_coords(&self) -> anyhow::Result<ArchiveIter<'_, TileCoord>> {
        Ok(self.paged_query(false, 10_000, |z, x, y, _| result_coord(z, x, y)))
    }

    fn get_all_tiles(&self) -> anyhow::Result<ArchiveIter<'_, Tile>> {
        Ok(self.paged_query(true, 1_000, |z, x, y, data| Tile::new(result_coord(z, x, y), data.unwrap_or_default())))
    }

    fn metadata(&self) -> anyhow::Result<Option<TileArchiveMetadata>> {
        Ok(Some(self.metadata_table().get()?))
    }
}

/// Data contained in the metadata table.
pub struct Metadata<'a> {
    mbtiles: &'a Mbtiles,
}

impl Metadata<'_> {
    /// Inserts a row into the metadata table that sets `name=value`.
    pub fn set_metadata(&self, name: &str, value: &str) -> &Self {
        if value.len() > 1_000 {
            debug!(
                "Set mbtiles metadata: {name}={}... {} more characters",
                &value[..value.floor_char_boundary(1_000)],
                value.len() - 1_000
            );
        } else {
            debug!("Set mbtiles metadata: {name}={value}");
        }
        let conn = self.mbtiles.connection.lock();
        let result = conn.execute(
            &format!("INSERT INTO {METADATA_TABLE} ({METADATA_COL_NAME},{METADATA_COL_VALUE}) VALUES(?, ?);"),
            [name, value],
        );
        if let Err(e) = result {
            error!("Error setting metadata {name}={value}: {e}");
        }
        self
    }

    /// Returns all key-value pairs from the metadata table.
    pub fn get_all(&self) -> BTreeMap<String, String> {
        let conn = self.mbtiles.connection.lock();
        let result = (|| -> rusqlite::Result<BTreeMap<String, String>> {
            let mut stmt =
                conn.prepare(&format!("SELECT {METADATA_COL_NAME}, {METADATA_COL_VALUE} FROM {METADATA_TABLE}"))?;
            let rows =
                stmt.query_map([], |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?)))?;
            let mut map = BTreeMap::new();
            for row in rows {
                if let (Some(k), Some(v)) = row? {
                    map.insert(k, v);
                }
            }
            Ok(map)
        })();
        result.unwrap_or_else(|e| {
            warn!("Error retrieving metadata: {e}");
            BTreeMap::new()
        })
    }

    /// Inserts rows into the metadata table that set all of the well-known metadata keys from `metadata` and passes
    /// through the raw values of any options not explicitly called out in the MBTiles specification.
    pub fn set(&self, metadata: &TileArchiveMetadata) -> &Self {
        for (k, v) in tile_archive_metadata_de_ser::to_mbtiles_map(metadata) {
            self.set_metadata(&k, &v);
        }
        self
    }

    /// Returns a [`TileArchiveMetadata`] parsed from all the rows in the metadata table.
    pub fn get(&self) -> anyhow::Result<TileArchiveMetadata> {
        tile_archive_metadata_de_ser::from_string_map(&self.get_all(), false)
    }
}

const TILE_COLUMNS: [&str; 4] = [TILES_COL_Z, TILES_COL_X, TILES_COL_Y, TILES_COL_DATA];
const SHALLOW_COLUMNS: [&str; 4] =
    [TILES_SHALLOW_COL_Z, TILES_SHALLOW_COL_X, TILES_SHALLOW_COL_Y, TILES_SHALLOW_COL_DATA_ID];
const DATA_COLUMNS: [&str; 2] = [TILES_DATA_COL_DATA_ID, TILES_DATA_COL_DATA];

trait BatchRow {
    /// Binds this row's values to `stmt` starting at parameter index `pos` (1-based), returning the next index.
    fn bind(&self, stmt: &mut Statement, pos: usize) -> rusqlite::Result<usize>;
}

/// Contents of a row of the tiles table.
struct TileRow {
    coord: TileCoord,
    data: Vec<u8>,
}

impl BatchRow for TileRow {
    fn bind(&self, stmt: &mut Statement, pos: usize) -> rusqlite::Result<usize> {
        let c = self.coord;
        stmt.raw_bind_parameter(pos, c.z as i64)?;
        stmt.raw_bind_parameter(pos + 1, c.x as i64)?;
        // flip Y
        stmt.raw_bind_parameter(pos + 2, flip_y(c.y, c.z))?;
        stmt.raw_bind_parameter(pos + 3, &self.data)?;
        Ok(pos + 4)
    }
}

/// Contents of a row of the tiles_shallow table.
struct TileShallowEntry {
    coord: TileCoord,
    tile_data_id: i32,
}

impl BatchRow for TileShallowEntry {
    fn bind(&self, stmt: &mut Statement, pos: usize) -> rusqlite::Result<usize> {
        let c = self.coord;
        stmt.raw_bind_parameter(pos, c.z as i64)?;
        stmt.raw_bind_parameter(pos + 1, c.x as i64)?;
        // flip Y
        stmt.raw_bind_parameter(pos + 2, flip_y(c.y, c.z))?;
        stmt.raw_bind_parameter(pos + 3, self.tile_data_id)?;
        Ok(pos + 4)
    }
}

/// Contents of a row of the tiles_data table.
struct TileDataEntry {
    tile_data_id: i32,
    tile_data: Vec<u8>,
}

impl BatchRow for TileDataEntry {
    fn bind(&self, stmt: &mut Statement, pos: usize) -> rusqlite::Result<usize> {
        stmt.raw_bind_parameter(pos, self.tile_data_id)?;
        stmt.raw_bind_parameter(pos + 1, &self.tile_data)?;
        Ok(pos + 2)
    }
}

/// Queues up inserts into large batches, using one multi-row `INSERT` prepared statement with at most 999 parameters.
struct BatchedTableWriter<'a, T> {
    connection: &'a Mutex<Connection>,
    batch: Vec<T>,
    batch_limit: usize,
    table_name: &'static str,
    insert_ignore: bool,
    values_placeholder: String,
    columns_csv: String,
    count: u64,
}

impl<'a, T: BatchRow> BatchedTableWriter<'a, T> {
    fn new(
        connection: &'a Mutex<Connection>,
        table_name: &'static str,
        columns: &[&str],
        insert_ignore: bool,
    ) -> BatchedTableWriter<'a, T> {
        let batch_limit = MAX_PARAMETERS_IN_PREPARED_STATEMENT / columns.len();
        BatchedTableWriter {
            connection,
            batch: Vec::with_capacity(batch_limit),
            batch_limit,
            table_name,
            insert_ignore,
            values_placeholder: format!("({})", vec!["?"; columns.len()].join(",")),
            columns_csv: columns.join(","),
            count: 0,
        }
    }

    fn insert_sql(&self, size: usize) -> String {
        format!(
            "INSERT {} INTO {} ({}) VALUES {};",
            if self.insert_ignore { "OR IGNORE" } else { "" },
            self.table_name,
            self.columns_csv,
            vec![self.values_placeholder.as_str(); size].join(", ")
        )
    }

    /// Queue-up a write or flush to disk if enough are waiting.
    fn write(&mut self, item: T) -> anyhow::Result<()> {
        self.count += 1;
        self.batch.push(item);
        if self.batch.len() >= self.batch_limit {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let sql = self.insert_sql(self.batch.len());
        let conn = self.connection.lock();
        let mut stmt = conn.prepare_cached(&sql).context("Could not create prepared statement")?;
        let mut pos = 1;
        for item in &self.batch {
            pos = item.bind(&mut stmt, pos)?;
        }
        stmt.raw_execute().context("Error flushing batch")?;
        self.batch.clear();
        Ok(())
    }
}

struct BatchedNonCompactTileWriter<'a> {
    table: BatchedTableWriter<'a, TileRow>,
}

impl TileWriter for BatchedNonCompactTileWriter<'_> {
    fn write(&mut self, encoding_result: TileEncodingResult) -> anyhow::Result<()> {
        self.table.write(TileRow { coord: encoding_result.coord, data: encoding_result.tile_data })
    }

    fn close(&mut self) -> anyhow::Result<()> {
        self.table.flush()
    }
}

struct BatchedCompactTileWriter<'a> {
    shallow: BatchedTableWriter<'a, TileShallowEntry>,
    data: BatchedTableWriter<'a, TileDataEntry>,
    tile_data_id_by_hash: HashMap<u64, i32>,
    tile_data_id_counter: i32,
}

impl TileWriter for BatchedCompactTileWriter<'_> {
    fn write(&mut self, encoding_result: TileEncodingResult) -> anyhow::Result<()> {
        let (tile_data_id, write_data) = match encoding_result.tile_data_hash {
            Some(hash) => match self.tile_data_id_by_hash.get(&hash) {
                Some(&id) => (id, false),
                None => {
                    let id = self.tile_data_id_counter;
                    self.tile_data_id_counter += 1;
                    self.tile_data_id_by_hash.insert(hash, id);
                    (id, true)
                }
            },
            None => {
                let id = self.tile_data_id_counter;
                self.tile_data_id_counter += 1;
                (id, true)
            }
        };
        if write_data {
            self.data.write(TileDataEntry { tile_data_id, tile_data: encoding_result.tile_data })?;
        }
        self.shallow.write(TileShallowEntry { coord: encoding_result.coord, tile_data_id })
    }

    fn close(&mut self) -> anyhow::Result<()> {
        self.shallow.flush()?;
        self.data.flush()
    }

    fn print_stats(&self) {
        let shallow = self.shallow.count;
        let data = self.data.count;
        debug!("Shallow tiles written: {shallow}");
        debug!("Tile data written: {data} ({:.1}% omitted)", (1.0 - data as f64 / shallow as f64) * 100.0);
        debug!("Unique tile hashes: {}", self.tile_data_id_by_hash.len());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::archive::test_fixtures::{assert_same_json, get_tiles};
    use crate::archive::tile_archive_metadata::{Coordinate, FieldType, TileArchiveMetadataJson, VectorLayer};
    use crate::archive::tile_compression::TileCompression;
    use crate::archive::tile_format::TileFormat;
    use crate::geo::envelope::Envelope;

    const TILES_BATCH: usize = MAX_PARAMETERS_IN_PREPARED_STATEMENT / 4;
    const TILES_SHALLOW_BATCH: usize = MAX_PARAMETERS_IN_PREPARED_STATEMENT / 4;
    const TILES_DATA_BATCH: usize = MAX_PARAMETERS_IN_PREPARED_STATEMENT / 2;

    fn is_compact_db(db: &Mbtiles) -> bool {
        db.connection()
            .query_row("select count(*) from sqlite_master where type='view' and name='tiles'", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
            > 0
    }

    fn get_tiles_data_count(db: &Mbtiles) -> i64 {
        let table = if is_compact_db(db) { "tiles_data" } else { "tiles" };
        db.connection().query_row(&format!("select count(*) from {table}"), [], |r| r.get(0)).unwrap()
    }

    fn test_write_tiles(
        path: Option<&Path>,
        how_many: usize,
        skip_index_creation: bool,
        optimize: bool,
        compact_db: bool,
    ) {
        let options = ArchiveOptions::of_pairs(&[("compact", if compact_db { "true" } else { "false" })]);
        let db = match path {
            None => Mbtiles::new_in_memory_database_with_options(&options).unwrap(),
            Some(p) => Mbtiles::new_write_to_file_database(p, &options).unwrap(),
        };
        if skip_index_creation {
            db.create_tables_without_indexes().unwrap();
        } else {
            db.create_tables_with_indexes().unwrap();
        }
        assert_eq!(None, db.get_tile(0, 0, 0).unwrap());
        let mut expected = BTreeSet::new();
        {
            let mut writer = db.new_tile_writer().unwrap();
            for i in 0..how_many {
                let data_hash = i - (i % 2);
                let data_base = (how_many + data_hash) as i32;
                let entry = Tile::new(TileCoord::of_xyz(i as u32, i as u32 + 1, 14), data_base.to_le_bytes().to_vec());
                writer
                    .write(TileEncodingResult::new(entry.coord, entry.bytes.clone(), Some(data_hash as u64)))
                    .unwrap();
                expected.insert(entry);
            }
            writer.close().unwrap();
        }
        if optimize {
            db.vacuum_analyze().unwrap();
        }
        let all = get_tiles(&db);
        assert_eq!(how_many, all.len());
        assert_eq!(expected, all);
        let coords: BTreeSet<TileCoord> = db.get_all_tile_coords().unwrap().map(|c| c.unwrap()).collect();
        assert_eq!(expected.iter().map(|t| t.coord).collect::<BTreeSet<_>>(), coords);
        for entry in &expected {
            let c = entry.coord;
            assert_eq!(Some(entry.bytes.clone()), db.get_tile(c.x, c.y, c.z).unwrap());
        }
        assert_eq!(compact_db, is_compact_db(&db));
        if compact_db {
            assert_eq!(how_many.div_ceil(2) as i64, get_tiles_data_count(&db));
        }
    }

    #[test]
    fn test_write_tiles_different_size_in_non_compact_mode() {
        for how_many in [0, 1, TILES_BATCH, TILES_BATCH + 1, 2 * TILES_BATCH, 2 * TILES_BATCH + 1] {
            test_write_tiles(None, how_many, false, false, false);
        }
    }

    #[test]
    fn test_write_tiles_different_size_in_compact_mode() {
        for how_many in [
            0,
            1,
            TILES_DATA_BATCH,
            TILES_DATA_BATCH + 1,
            2 * TILES_DATA_BATCH,
            2 * TILES_DATA_BATCH + 1,
            TILES_SHALLOW_BATCH,
            TILES_SHALLOW_BATCH + 1,
            2 * TILES_SHALLOW_BATCH,
            2 * TILES_SHALLOW_BATCH + 1,
        ] {
            test_write_tiles(None, how_many, false, false, true);
        }
    }

    #[test]
    fn test_paging() {
        // more than one page of coords and tiles
        test_write_tiles(None, 25_001, false, false, true);
    }

    #[test]
    fn test_skip_index_creation() {
        test_write_tiles(None, 10, true, false, false);
    }

    #[test]
    fn test_vacuum_analyze() {
        test_write_tiles(None, 10, false, true, false);
    }

    #[test]
    fn test_write_to_file() {
        let tmp = tempfile::tempdir().unwrap();
        test_write_tiles(Some(&tmp.path().join("archive.mbtiles")), 10, false, false, true);
    }

    #[test]
    fn test_custom_pragma() {
        let db = Mbtiles::new_in_memory_database_with_options(&ArchiveOptions::of_pairs(&[
            ("cache-size", "123"),
            ("garbage", "456"),
        ]))
        .unwrap();
        let result: i64 = db.connection().query_row("pragma cache_size", [], |r| r.get(0)).unwrap();
        assert_eq!(123, result);
    }

    #[test]
    fn test_manual_index_creation_statements() {
        for compact_db in [true, false] {
            let db = Mbtiles::new_in_memory_database(compact_db);
            db.create_tables_without_indexes().unwrap();
            let stmts = db.get_manual_index_creation_statements();
            assert!(!stmts.is_empty());
            for stmt in stmts {
                db.connection().execute(&stmt, []).unwrap();
            }
        }
    }

    fn metadata_with_json(json: Option<TileArchiveMetadataJson>) -> TileArchiveMetadata {
        TileArchiveMetadata::new(
            Some("MyName".into()),
            Some("MyDescription".into()),
            Some("MyAttribution".into()),
            Some("MyVersion".into()),
            Some("baselayer".into()),
            Some(TileFormat::Mvt),
            Some(Envelope::new(1.0, 2.0, 3.0, 4.0)),
            Some(Coordinate::new(5.0, 6.0, 7.0)),
            Some(8),
            Some(9),
            json,
            BTreeMap::from([("other key".into(), "other value".into())]),
            Some(TileCompression::Gzip),
        )
    }

    fn round_trip_metadata(metadata: &TileArchiveMetadata, expected: &TileArchiveMetadata) {
        let db = Mbtiles::new_in_memory_database(true);
        db.create_tables_without_indexes().unwrap();
        let table = db.metadata_table();
        table.set(metadata);
        assert_eq!(*expected, table.get().unwrap());
    }

    fn one_layer() -> Option<TileArchiveMetadataJson> {
        Some(TileArchiveMetadataJson::new(vec![VectorLayer::new("MyLayer", BTreeMap::new())]))
    }

    #[test]
    fn test_round_trip_metadata() {
        let m = metadata_with_json(one_layer());
        round_trip_metadata(&m, &m);
    }

    #[test]
    fn test_metadata_without_compression_assumes_gzip() {
        let input = TileArchiveMetadata { tile_compression: None, ..metadata_with_json(one_layer()) };
        round_trip_metadata(&input, &metadata_with_json(one_layer()));
    }

    #[test]
    fn test_round_trip_minimal_metadata() {
        let empty = TileArchiveMetadata {
            format: Some(TileFormat::Mvt),
            tile_compression: Some(TileCompression::Gzip),
            ..Default::default()
        };
        round_trip_metadata(&empty, &empty);
        let db = Mbtiles::new_in_memory_database(true);
        db.create_tables_without_indexes().unwrap();
        assert_eq!(Some(empty), ReadableTileArchive::metadata(&db).unwrap());
    }

    fn test_metadata_json(json: TileArchiveMetadataJson, expected: &str) {
        let db = Mbtiles::new_in_memory_database(true);
        db.create_tables_without_indexes().unwrap();
        let metadata = db.metadata_table();
        metadata.set(&metadata_with_json(Some(json)));
        let actual = metadata.get_all().remove("json").unwrap();
        assert_same_json(expected, &actual);
    }

    #[test]
    fn test_metadata_json_no_layers() {
        test_metadata_json(TileArchiveMetadataJson::new(vec![]), r#"{"vector_layers": []}"#);
    }

    #[test]
    fn test_full_metadata_json() {
        test_metadata_json(
            TileArchiveMetadataJson::new(vec![
                VectorLayer::new(
                    "full",
                    BTreeMap::from([
                        ("NUMBER_FIELD".into(), FieldType::Number),
                        ("STRING_FIELD".into(), FieldType::String),
                        ("boolean field".into(), FieldType::Boolean),
                    ]),
                )
                .with_description("full description")
                .with_minzoom(0)
                .with_maxzoom(5),
                VectorLayer::new("partial", BTreeMap::new()),
            ]),
            r#"{
              "vector_layers": [
                {
                  "id": "full",
                  "description": "full description",
                  "minzoom": 0,
                  "maxzoom": 5,
                  "fields": {
                    "NUMBER_FIELD": "Number",
                    "STRING_FIELD": "String",
                    "boolean field": "Boolean"
                  }
                },
                {
                  "id": "partial",
                  "fields": {}
                }
              ]
            }"#,
        );
    }

    #[test]
    fn test_writeable_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.mbtiles");
        let mut db = Mbtiles::new_write_to_file_database(&path, &ArchiveOptions::empty()).unwrap();
        db.initialize().unwrap();
        {
            let mut w = db.new_tile_writer().unwrap();
            w.write(TileEncodingResult::new(TileCoord::of_xyz(0, 0, 0), vec![1, 2], Some(1))).unwrap();
            w.close().unwrap();
        }
        let m = metadata_with_json(one_layer());
        db.finish(&m).unwrap();
        drop(db);
        let db = Mbtiles::new_read_only_database(&path).unwrap();
        assert_eq!(Some(m), ReadableTileArchive::metadata(&db).unwrap());
        assert_eq!(Some(vec![1, 2]), db.get_tile(0, 0, 0).unwrap());
    }
}
