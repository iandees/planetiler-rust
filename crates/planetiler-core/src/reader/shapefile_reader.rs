//! Port of `reader/ShapefileReader.java`: reads features from the geometries contained in an ESRI shapefile.
//!
//! Java delegates to GeoTools' `ShapefileDataStore`. This is a self-contained streaming reader for the `.shp`
//! (geometries) and `.dbf` (attributes) files, plus `.prj` (coordinate reference system) and `.cpg` (attribute
//! charset). Inputs can be a `.shp` file on disk, a path inside a `.zip` file (as returned by
//! [`walk_path_with_pattern`](crate::util::file_utils::walk_path_with_pattern), for example
//! `water-polygons.zip/water-polygons/water_polygons.shp`), or a `.zip` file containing a single shapefile. Zip entries
//! are streamed without being extracted.
//!
//! Records are read sequentially by one thread and decoded/reprojected in parallel by a pool of decode threads, then
//! emitted to the consumer in file order.
//!
//! Geometries are built the same way as GeoTools: point shapefiles produce `Point`s, multipoint shapefiles
//! `MultiPoint`s, polyline shapefiles `MultiLineString`s and polygon shapefiles `MultiPolygon`s (clockwise rings are
//! shells, counter-clockwise rings are holes assigned to the smallest shell that contains them).
//!
//! DBF attributes map to: `C` -> string (right-trimmed), `N` without decimals -> integer, `N`/`F` with decimals ->
//! double, `L` -> boolean, `D` -> `YYYY-MM-DD` string (Java uses `java.util.Date`), `I`/`+` -> integer, `O` -> double;
//! blank values are null.
//!
//! See the [ESRI Shapefile Specification](https://www.esri.com/content/dam/esrisites/sitecore-archive/Files/Pdfs/library/whitepapers/pdfs/shapefile.pdf).

use std::collections::BTreeMap;
use std::io::{self, BufReader, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use geo_types::{Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};
use tracing::warn;

use crate::config::bounds::Bounds;
use crate::geo::crs::{Crs, MathTransform, decode_crs, find_math_transform};
use crate::geo::envelope::Envelope;
use crate::reader::simple_reader::{
    FeatureSink, ReaderFeature, SimpleReader, default_decode_threads, inverse_transform_envelope,
};
use crate::util::file_utils;
use crate::value::{Tags, Value};

/// Where the shapefile components live.
#[derive(Clone, Debug)]
enum Input {
    Disk { shp: PathBuf, dbf: Option<PathBuf> },
    Zip { zip: PathBuf, shp: String, dbf: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Charset {
    Utf8,
    Latin1,
}

impl Charset {
    fn from_cpg(cpg: &str) -> Charset {
        let normalized = cpg.trim().to_ascii_uppercase().replace(['-', '_'], "");
        match normalized.as_str() {
            "UTF8" | "65001" => Charset::Utf8,
            _ => Charset::Latin1,
        }
    }

    fn decode(self, bytes: &[u8]) -> String {
        match self {
            Charset::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
            Charset::Latin1 => bytes.iter().map(|&b| b as char).collect(),
        }
    }
}

/// A column in a dbf file.
#[derive(Clone, Debug)]
struct DbfField {
    name: String,
    kind: u8,
    offset: usize,
    length: usize,
    decimals: u8,
}

#[derive(Clone, Debug, Default)]
struct DbfHeader {
    num_records: u64,
    header_length: usize,
    record_length: usize,
    fields: Vec<DbfField>,
}

/// Reads features from an ESRI shapefile.
pub struct ShapefileReader {
    source_name: String,
    layer: std::sync::Arc<str>,
    input: Input,
    transform: Option<MathTransform>,
    /// Only include records whose bounding box intersects this envelope (in the source CRS).
    source_filter: Option<Envelope>,
    charset: Charset,
    dbf: DbfHeader,
    decode_threads: usize,
}

impl ShapefileReader {
    /// Opens the shapefile at `input`, reading all features. `source_projection` overrides the CRS from the `.prj`
    /// file.
    pub fn new(source_projection: Option<&str>, source_name: &str, input: &Path) -> Result<ShapefileReader> {
        Self::with_bounds(source_projection, source_name, input, &Bounds::world())
    }

    /// Opens the shapefile at `input`, only reading features whose bounding box intersects `bounds`.
    pub fn with_bounds(
        source_projection: Option<&str>,
        source_name: &str,
        input: &Path,
        bounds: &Bounds,
    ) -> Result<ShapefileReader> {
        let input_desc = resolve_input(input)?;
        let layer = match &input_desc {
            Input::Disk { shp, .. } => file_name(shp),
            Input::Zip { shp, .. } => shp.rsplit('/').next().unwrap_or(shp).to_string(),
        };
        let layer = strip_suffix_ignore_case(&layer, ".shp").to_string();

        let prj = read_sibling(&input_desc, "prj")?.map(|b| String::from_utf8_lossy(&b).into_owned());
        let cpg = read_sibling(&input_desc, "cpg")?.map(|b| String::from_utf8_lossy(&b).into_owned());
        let charset = cpg.as_deref().map_or(Charset::Latin1, Charset::from_cpg);

        let source_crs = match prj.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(wkt) => match Crs::parse_wkt(wkt) {
                Ok(crs) => Some(crs),
                Err(e) if source_projection.is_some() => {
                    warn!("Failed to parse projection from {} ({e}), using {source_projection:?}", file_name(input));
                    None
                }
                Err(e) => return Err(e).context(format!("Bad reference system in {}", input.display())),
            },
            None => None,
        };
        let source_crs = match source_projection {
            Some(proj) => decode_crs(proj, source_crs.as_ref()).context("Bad reference system")?,
            None => source_crs.unwrap_or_else(|| {
                warn!("No .prj file found for {}, assuming EPSG:4326", input.display());
                Crs::wgs84()
            }),
        };
        let transform = find_math_transform(&source_crs, &Crs::wgs84(), None);
        let source_filter = if bounds.is_world() {
            None
        } else if transform.is_identity() {
            Some(bounds.lat_lon())
        } else {
            inverse_transform_envelope(&transform, &bounds.lat_lon())
        };
        let transform = if transform.is_identity() { None } else { Some(transform) };

        let dbf = read_dbf_header(&input_desc, charset)?;
        Ok(ShapefileReader {
            source_name: source_name.to_string(),
            layer: layer.into(),
            input: input_desc,
            transform,
            source_filter,
            charset,
            dbf,
            decode_threads: default_decode_threads(),
        })
    }

    /// Sets the number of threads used to decode geometries and attributes.
    pub fn with_decode_threads(mut self, threads: usize) -> Self {
        self.decode_threads = threads.max(1);
        self
    }

    /// Name of the layer features are emitted to (the `.shp` file name without extension).
    pub fn layer(&self) -> &str {
        &self.layer
    }

    /// Opens the `.shp` and `.dbf` streams and invokes `f` with them.
    fn with_streams<T>(&self, f: impl FnOnce(&mut dyn Read, Option<&mut dyn Read>) -> Result<T>) -> Result<T> {
        const BUF: usize = 1 << 20;
        match &self.input {
            Input::Disk { shp, dbf } => {
                let mut shp = BufReader::with_capacity(BUF, std::fs::File::open(shp)?);
                match dbf {
                    Some(dbf) => {
                        let mut dbf = BufReader::with_capacity(BUF, std::fs::File::open(dbf)?);
                        f(&mut shp, Some(&mut dbf))
                    }
                    None => f(&mut shp, None),
                }
            }
            Input::Zip { zip, shp, dbf } => {
                let mut shp_zip = open_zip(zip)?;
                let mut shp = BufReader::with_capacity(BUF, shp_zip.by_name(shp)?);
                match dbf {
                    Some(dbf) => {
                        let mut dbf_zip = open_zip(zip)?;
                        let mut dbf = BufReader::with_capacity(BUF, dbf_zip.by_name(dbf)?);
                        f(&mut shp, Some(&mut dbf))
                    }
                    None => f(&mut shp, None),
                }
            }
        }
    }

    /// Reads raw records sequentially, sending batches of records that pass the filters to `emit`. Returns `false` from
    /// `emit` to stop reading.
    fn scan(&self, mut emit: impl FnMut(RawBatch) -> bool) -> Result<()> {
        const BATCH_BYTES: usize = 4 << 20;
        const BATCH_RECORDS: usize = 4096;
        let record_length = self.dbf.record_length;
        let header_length = self.dbf.header_length;
        let warned_multipatch = AtomicBool::new(false);
        self.with_streams(|shp, dbf| {
            let mut header = [0u8; 100];
            shp.read_exact(&mut header).context("reading shapefile header")?;
            if i32::from_be_bytes(header[0..4].try_into().unwrap()) != 9994 {
                bail!("Not a shapefile: bad file code");
            }
            let mut dbf = dbf;
            if let Some(dbf) = dbf.as_deref_mut() {
                io::copy(&mut dbf.take(header_length as u64), &mut io::sink())?;
            }
            let mut id = 0i64;
            let mut seq = 0usize;
            let mut batch = RawBatch::new(seq);
            let mut dbf_done = dbf.is_none();
            loop {
                let mut record_header = [0u8; 8];
                if !read_exact_or_eof(shp, &mut record_header)? {
                    break;
                }
                let content_length = i32::from_be_bytes(record_header[4..8].try_into().unwrap());
                if content_length < 0 {
                    bail!("Invalid shapefile record length {content_length}");
                }
                let content_length = content_length as usize * 2;
                let shp_start = batch.shp.len();
                batch.shp.resize(shp_start + content_length, 0);
                shp.read_exact(&mut batch.shp[shp_start..]).context("reading shapefile record")?;
                let dbf_start = batch.dbf.len();
                if !dbf_done && let Some(dbf) = dbf.as_deref_mut() {
                    batch.dbf.resize(dbf_start + record_length, 0);
                    if !read_exact_or_eof(dbf, &mut batch.dbf[dbf_start..])? {
                        batch.dbf.truncate(dbf_start);
                        dbf_done = true;
                    }
                }
                let shp_range = shp_start..batch.shp.len();
                let dbf_range = dbf_start..batch.dbf.len();
                let deleted = batch.dbf.get(dbf_start) == Some(&b'*');
                let content = &batch.shp[shp_range.clone()];
                let shape_type =
                    if content.len() >= 4 { i32::from_le_bytes(content[0..4].try_into().unwrap()) } else { 0 };
                let include = !deleted
                    && shape_type != 0
                    && if shape_type == 31 {
                        if !warned_multipatch.swap(true, Ordering::Relaxed) {
                            warn!("MultiPatch shapes are not supported, skipping them in {}", self.layer);
                        }
                        false
                    } else {
                        match &self.source_filter {
                            Some(filter) => record_envelope(shape_type, content).is_some_and(|e| e.intersects(filter)),
                            None => true,
                        }
                    };
                if include {
                    id += 1;
                    batch.records.push(RawRecord { id, shp: shp_range, dbf: dbf_range });
                } else {
                    batch.shp.truncate(shp_start);
                    batch.dbf.truncate(dbf_start);
                }
                if batch.shp.len() >= BATCH_BYTES || batch.records.len() >= BATCH_RECORDS {
                    seq += 1;
                    let full = std::mem::replace(&mut batch, RawBatch::new(seq));
                    if !emit(full) {
                        return Ok(());
                    }
                }
            }
            if !batch.records.is_empty() {
                emit(batch);
            }
            Ok(())
        })
    }

    fn decode_batch(&self, batch: &RawBatch) -> Result<Vec<ReaderFeature>> {
        let mut result = Vec::with_capacity(batch.records.len());
        for record in &batch.records {
            let content = &batch.shp[record.shp.clone()];
            let geometry = decode_shape(content, self.transform.as_ref())
                .with_context(|| format!("Error decoding record {} of {}", record.id, self.layer))?;
            let Some(geometry) = geometry else { continue };
            let tags = self.decode_attributes(&batch.dbf[record.dbf.clone()]);
            result.push(ReaderFeature { geometry, tags, source_layer: Some(self.layer.clone()), id: record.id });
        }
        Ok(result)
    }

    fn decode_attributes(&self, record: &[u8]) -> Tags {
        let mut tags = Tags::with_capacity(self.dbf.fields.len());
        for field in &self.dbf.fields {
            let value = record
                .get(field.offset..field.offset + field.length)
                .map_or(Value::Null, |bytes| decode_dbf_value(field, bytes, self.charset));
            tags.insert(field.name.clone(), value);
        }
        tags
    }
}

impl SimpleReader for ShapefileReader {
    fn source_name(&self) -> &str {
        &self.source_name
    }

    fn feature_count(&self) -> Result<u64> {
        if self.source_filter.is_none() {
            if !self.dbf.fields.is_empty() || self.dbf.num_records > 0 {
                return Ok(self.dbf.num_records);
            }
            if let Some(shx) = read_sibling(&self.input, "shx")? {
                return Ok((shx.len().saturating_sub(100) / 8) as u64);
            }
        }
        let mut count = 0;
        self.scan(|batch| {
            count += batch.records.len() as u64;
            true
        })?;
        Ok(count)
    }

    fn read_features(&self, next: &mut FeatureSink<'_>) -> Result<()> {
        let threads = self.decode_threads;
        if threads <= 1 {
            let mut error = None;
            self.scan(|batch| match self.decode_batch(&batch) {
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
            let (raw_tx, raw_rx) = crossbeam_channel::bounded::<RawBatch>(threads * 2);
            let (decoded_tx, decoded_rx) =
                crossbeam_channel::bounded::<(usize, Result<Vec<ReaderFeature>>)>(threads * 2);
            let reader = scope.spawn(move || self.scan(|batch| raw_tx.send(batch).is_ok()));
            for _ in 0..threads {
                let raw_rx = raw_rx.clone();
                let decoded_tx = decoded_tx.clone();
                scope.spawn(move || {
                    for batch in raw_rx {
                        let result = self.decode_batch(&batch);
                        if decoded_tx.send((batch.seq, result)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(raw_rx);
            drop(decoded_tx);
            let result = emit_in_order(decoded_rx, next);
            let read_result = reader.join().map_err(|_| anyhow!("shapefile reader thread panicked"))?;
            result.and(read_result)
        })
    }
}

/// Receives `(sequence, batch)` pairs in any order and emits batches to `next` in sequence order. Dropping the
/// receiver on error stops the producers.
pub(crate) fn emit_in_order(
    rx: crossbeam_channel::Receiver<(usize, Result<Vec<ReaderFeature>>)>,
    next: &mut FeatureSink<'_>,
) -> Result<()> {
    let mut pending: BTreeMap<usize, Vec<ReaderFeature>> = BTreeMap::new();
    let mut expected = 0usize;
    for (seq, batch) in rx {
        pending.insert(seq, batch?);
        while let Some(features) = pending.remove(&expected) {
            for feature in features {
                next(feature)?;
            }
            expected += 1;
        }
    }
    for (_, features) in pending {
        for feature in features {
            next(feature)?;
        }
    }
    Ok(())
}

struct RawRecord {
    id: i64,
    shp: Range<usize>,
    dbf: Range<usize>,
}

struct RawBatch {
    seq: usize,
    shp: Vec<u8>,
    dbf: Vec<u8>,
    records: Vec<RawRecord>,
}

impl RawBatch {
    fn new(seq: usize) -> RawBatch {
        RawBatch { seq, shp: Vec::new(), dbf: Vec::new(), records: Vec::new() }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn strip_suffix_ignore_case<'a>(s: &'a str, suffix: &str) -> &'a str {
    if s.len() >= suffix.len() && s[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix) {
        &s[..s.len() - suffix.len()]
    } else {
        s
    }
}

fn open_zip(zip: &Path) -> Result<zip::ZipArchive<BufReader<std::fs::File>>> {
    let file = std::fs::File::open(zip).with_context(|| format!("opening {}", zip.display()))?;
    Ok(zip::ZipArchive::new(BufReader::new(file))?)
}

/// Figure out where the `.shp` and `.dbf` files are for `input`.
fn resolve_input(input: &Path) -> Result<Input> {
    if file_utils::has_extension(input, "zip") && input.is_file() {
        let entries = file_utils::list_zip_entries(input)?;
        let shp = entries
            .iter()
            .find(|e| e.to_ascii_lowercase().ends_with(".shp"))
            .ok_or_else(|| anyhow!("No .shp file found inside {}", input.display()))?
            .clone();
        let dbf = find_zip_sibling(&entries, &shp, "dbf");
        return Ok(Input::Zip { zip: input.to_path_buf(), shp, dbf });
    }
    if let Some((zip, entry)) = file_utils::split_zip_path(input) {
        let entries = file_utils::list_zip_entries(&zip)?;
        let entry = entry.trim_start_matches('/').to_string();
        if !entries.contains(&entry) {
            bail!("{entry} not found in {}", zip.display());
        }
        let dbf = find_zip_sibling(&entries, &entry, "dbf");
        return Ok(Input::Zip { zip, shp: entry, dbf });
    }
    if !input.is_file() {
        bail!("Shapefile not found: {}", input.display());
    }
    let dbf = find_disk_sibling(input, "dbf");
    Ok(Input::Disk { shp: input.to_path_buf(), dbf })
}

fn find_zip_sibling(entries: &[String], shp: &str, extension: &str) -> Option<String> {
    let stem = &shp[..shp.len() - 4];
    entries
        .iter()
        .find(|e| {
            e.len() == shp.len()
                && e.starts_with(stem)
                && e[stem.len()..].eq_ignore_ascii_case(&format!(".{extension}"))
        })
        .cloned()
}

fn find_disk_sibling(shp: &Path, extension: &str) -> Option<PathBuf> {
    [extension.to_ascii_lowercase(), extension.to_ascii_uppercase()]
        .into_iter()
        .map(|ext| shp.with_extension(ext))
        .find(|p| p.is_file())
}

/// Reads a small sibling file (`.prj`, `.cpg`, `.shx`) fully, if it exists.
fn read_sibling(input: &Input, extension: &str) -> Result<Option<Vec<u8>>> {
    match input {
        Input::Disk { shp, .. } => match find_disk_sibling(shp, extension) {
            Some(path) => Ok(Some(std::fs::read(path)?)),
            None => Ok(None),
        },
        Input::Zip { zip, shp, .. } => {
            let entries = file_utils::list_zip_entries(zip)?;
            match find_zip_sibling(&entries, shp, extension) {
                Some(entry) => Ok(Some(file_utils::read_zip_entry(zip, &entry)?)),
                None => Ok(None),
            }
        }
    }
}

fn read_dbf_header(input: &Input, charset: Charset) -> Result<DbfHeader> {
    fn parse(reader: &mut dyn Read, charset: Charset) -> Result<DbfHeader> {
        let mut header = [0u8; 32];
        reader.read_exact(&mut header).context("reading dbf header")?;
        let num_records = u32::from_le_bytes(header[4..8].try_into().unwrap()) as u64;
        let header_length = u16::from_le_bytes(header[8..10].try_into().unwrap()) as usize;
        let record_length = u16::from_le_bytes(header[10..12].try_into().unwrap()) as usize;
        let mut rest = vec![0u8; header_length.saturating_sub(32)];
        reader.read_exact(&mut rest).context("reading dbf field descriptors")?;
        let mut fields = vec![];
        let mut offset = 1; // deletion flag
        for descriptor in rest.as_chunks::<32>().0 {
            if descriptor[0] == 0x0D {
                break;
            }
            let name_end = descriptor[..11].iter().position(|&b| b == 0).unwrap_or(11);
            let name = charset.decode(&descriptor[..name_end]).trim().to_string();
            let kind = descriptor[11].to_ascii_uppercase();
            let length = descriptor[16] as usize;
            let decimals = descriptor[17];
            fields.push(DbfField { name, kind, offset, length, decimals });
            offset += length;
        }
        Ok(DbfHeader { num_records, header_length, record_length, fields })
    }
    match input {
        Input::Disk { dbf: Some(dbf), .. } => parse(&mut BufReader::new(std::fs::File::open(dbf)?), charset),
        Input::Zip { zip, dbf: Some(dbf), .. } => {
            let mut archive = open_zip(zip)?;
            let mut entry = archive.by_name(dbf)?;
            parse(&mut entry, charset)
        }
        _ => Ok(DbfHeader::default()),
    }
}

fn read_exact_or_eof(reader: &mut dyn Read, buf: &mut [u8]) -> Result<bool> {
    let mut read = 0;
    while read < buf.len() {
        match reader.read(&mut buf[read..]) {
            Ok(0) if read == 0 => return Ok(false),
            Ok(0) => bail!("Unexpected end of file"),
            Ok(n) => read += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

fn decode_dbf_value(field: &DbfField, bytes: &[u8], charset: Charset) -> Value {
    fn trim(bytes: &[u8]) -> &[u8] {
        let start = bytes.iter().position(|&b| b != b' ' && b != 0).unwrap_or(bytes.len());
        let end = bytes.iter().rposition(|&b| b != b' ' && b != 0).map_or(start, |e| e + 1);
        &bytes[start..end.max(start)]
    }
    match field.kind {
        b'C' => {
            let end = bytes.iter().rposition(|&b| b != b' ' && b != 0).map_or(0, |e| e + 1);
            Value::str(charset.decode(&bytes[..end]))
        }
        b'N' | b'F' => {
            let trimmed = trim(bytes);
            if trimmed.is_empty() || trimmed.iter().all(|&b| b == b'*' || b == b'.') {
                return Value::Null;
            }
            let Ok(s) = std::str::from_utf8(trimmed) else { return Value::Null };
            if field.kind == b'N'
                && field.decimals == 0
                && let Ok(i) = s.parse::<i64>()
            {
                return Value::Int(i);
            }
            s.parse::<f64>().map_or(Value::Null, Value::Double)
        }
        b'L' => match trim(bytes).first() {
            Some(b'T' | b't' | b'Y' | b'y') => Value::Bool(true),
            Some(b'F' | b'f' | b'N' | b'n') => Value::Bool(false),
            _ => Value::Null,
        },
        b'D' => {
            let trimmed = trim(bytes);
            if trimmed.len() == 8 && trimmed.iter().all(u8::is_ascii_digit) {
                let s = std::str::from_utf8(trimmed).unwrap_or_default();
                Value::str(format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8]))
            } else {
                Value::Null
            }
        }
        b'I' | b'+' if bytes.len() == 4 => Value::Int(i32::from_le_bytes(bytes.try_into().unwrap()) as i64),
        b'O' if bytes.len() == 8 => Value::Double(f64::from_le_bytes(bytes.try_into().unwrap())),
        _ => Value::Null,
    }
}

/// Bounding box of a record from its header, without decoding the geometry.
fn record_envelope(shape_type: i32, content: &[u8]) -> Option<Envelope> {
    let f = |i: usize| content.get(i..i + 8).map(|b| f64::from_le_bytes(b.try_into().unwrap()));
    match shape_type {
        1 | 11 | 21 => Some(Envelope::of_point(f(4)?, f(12)?)),
        _ => Some(Envelope::new(f(4)?, f(20)?, f(12)?, f(28)?)),
    }
}

/// Little-endian cursor over a shapefile record.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn i32(&mut self) -> Result<i32> {
        let b = self.data.get(self.pos..self.pos + 4).ok_or_else(|| anyhow!("truncated shapefile record"))?;
        self.pos += 4;
        Ok(i32::from_le_bytes(b.try_into().unwrap()))
    }

    fn count(&mut self) -> Result<usize> {
        let n = self.i32()?;
        if n < 0 {
            bail!("negative count in shapefile record");
        }
        Ok(n as usize)
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    fn coords(&mut self, n: usize, transform: Option<&MathTransform>) -> Result<Vec<Coord<f64>>> {
        let bytes = self.data.get(self.pos..self.pos + n * 16).ok_or_else(|| anyhow!("truncated shapefile record"))?;
        self.pos += n * 16;
        let mut result = Vec::with_capacity(n);
        for chunk in bytes.as_chunks::<16>().0 {
            let x = f64::from_le_bytes(chunk[0..8].try_into().unwrap());
            let y = f64::from_le_bytes(chunk[8..16].try_into().unwrap());
            result.push(Coord { x, y });
        }
        if let Some(t) = transform {
            for c in &mut result {
                *c = t.transform(*c)?;
            }
        }
        Ok(result)
    }
}

/// Decode a shapefile record (starting with the shape type) into a lat/lon geometry, or `None` for null shapes.
fn decode_shape(content: &[u8], transform: Option<&MathTransform>) -> Result<Option<Geometry<f64>>> {
    let mut cursor = Cursor { data: content, pos: 0 };
    let shape_type = cursor.i32()?;
    Ok(Some(match shape_type {
        0 => return Ok(None),
        1 | 11 | 21 => {
            let c = cursor.coords(1, transform)?;
            Geometry::Point(Point(c[0]))
        }
        8 | 18 | 28 => {
            cursor.skip(32);
            let n = cursor.count()?;
            Geometry::MultiPoint(MultiPoint(cursor.coords(n, transform)?.into_iter().map(Point).collect()))
        }
        3 | 13 | 23 | 5 | 15 | 25 => {
            cursor.skip(32);
            let num_parts = cursor.count()?;
            let num_points = cursor.count()?;
            let mut parts = Vec::with_capacity(num_parts);
            for _ in 0..num_parts {
                parts.push(cursor.count()?);
            }
            let coords = cursor.coords(num_points, transform)?;
            let mut rings: Vec<Vec<Coord<f64>>> = Vec::with_capacity(num_parts);
            let mut coords = coords.into_iter();
            for i in 0..num_parts {
                let start = parts[i].min(num_points);
                let end = parts.get(i + 1).copied().unwrap_or(num_points).clamp(start, num_points);
                rings.push(coords.by_ref().take(end - start).collect());
            }
            if matches!(shape_type, 3 | 13 | 23) {
                Geometry::MultiLineString(MultiLineString(rings.into_iter().map(LineString).collect()))
            } else {
                Geometry::MultiPolygon(build_polygons(rings))
            }
        }
        other => bail!("Unsupported shape type {other}"),
    }))
}

/// Twice the signed area of a ring (positive = counter-clockwise).
fn signed_area(ring: &[Coord<f64>]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    // shift to the first point to reduce floating point error
    let origin = ring[0];
    ring.windows(2).map(|w| (w[0].x - origin.x) * (w[1].y - origin.y) - (w[1].x - origin.x) * (w[0].y - origin.y)).sum()
}

fn ring_envelope(ring: &[Coord<f64>]) -> Envelope {
    let mut env = Envelope::null();
    for c in ring {
        env.expand_to_include(c.x, c.y);
    }
    env
}

fn point_in_ring(p: Coord<f64>, ring: &[Coord<f64>]) -> bool {
    let mut inside = false;
    let n = ring.len();
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (ring[i], ring[j]);
        if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Assemble polygon rings the way GeoTools' `PolygonHandler` does: clockwise rings are shells, counter-clockwise
/// rings are holes, each hole goes in the smallest shell that contains it (or becomes a shell if none do).
fn build_polygons(rings: Vec<Vec<Coord<f64>>>) -> MultiPolygon<f64> {
    let mut shells = vec![];
    let mut holes = vec![];
    for mut ring in rings {
        if ring.len() < 3 {
            continue;
        }
        if ring.first() != ring.last() {
            ring.push(ring[0]);
        }
        if ring.len() < 4 {
            continue;
        }
        if signed_area(&ring) > 0.0 { holes.push(ring) } else { shells.push(ring) }
    }
    if shells.len() == 1 {
        let shell = shells.pop().unwrap();
        return MultiPolygon(vec![Polygon::new(LineString(shell), holes.into_iter().map(LineString).collect())]);
    }
    if shells.is_empty() && holes.len() == 1 {
        return MultiPolygon(vec![Polygon::new(LineString(holes.pop().unwrap()), vec![])]);
    }
    let shell_envelopes: Vec<Envelope> = shells.iter().map(|s| ring_envelope(s)).collect();
    let mut holes_for_shells: Vec<Vec<LineString<f64>>> = vec![vec![]; shells.len()];
    let mut extra_shells = vec![];
    for hole in holes {
        let hole_env = ring_envelope(&hole);
        let mut best: Option<usize> = None;
        for (i, shell) in shells.iter().enumerate() {
            let shell_env = &shell_envelopes[i];
            if !shell_env.contains(&hole_env) {
                continue;
            }
            let test_point = hole.iter().find(|c| !shell.contains(c)).copied().unwrap_or(hole[0]);
            if point_in_ring(test_point, shell) && best.is_none_or(|b| shell_envelopes[b].contains(shell_env)) {
                best = Some(i);
            }
        }
        match best {
            Some(i) => holes_for_shells[i].push(LineString(hole)),
            None => {
                let mut reversed = hole;
                reversed.reverse();
                extra_shells.push(Polygon::new(LineString(reversed), vec![]));
            }
        }
    }
    let mut polygons: Vec<Polygon<f64>> =
        shells.into_iter().zip(holes_for_shells).map(|(shell, holes)| Polygon::new(LineString(shell), holes)).collect();
    polygons.extend(extra_shells);
    MultiPolygon(polygons)
}

/// Entry points that run a profile on shapefiles (Java's static `ShapefileReader.processWithProjection`).
impl ShapefileReader {
    /// Renders map features for all elements from the shapefiles at `source_paths` (`.shp` files, paths inside zip
    /// files, or zip files containing one shapefile) based on the mapping logic defined in `profile`, writing them
    /// to `writer`.
    ///
    /// `source_projection` overrides the CRS from the `.prj` file (e.g. `EPSG:3857`). Only features whose bounding
    /// box intersects `config.bounds` are read.
    pub fn process_with_projection(
        source_projection: Option<&str>,
        source_name: &str,
        source_paths: Vec<PathBuf>,
        writer: &std::sync::Arc<crate::collection::feature_group::FeatureGroup>,
        config: &std::sync::Arc<crate::config::planetiler_config::PlanetilerConfig>,
        profile: &std::sync::Arc<dyn crate::profile::Profile>,
        stats: crate::stats::stats::StatsRef,
    ) -> Result<()> {
        let projection = source_projection.map(str::to_string);
        let name = source_name.to_string();
        let bounds = config.bounds.clone();
        crate::reader::source_feature_processor::process_files(
            source_name,
            source_paths,
            move |path: &Path| ShapefileReader::with_bounds(projection.as_deref(), &name, path, &bounds),
            writer,
            config,
            profile,
            stats,
        )
    }

    /// [`ShapefileReader::process_with_projection`] using the projection from each shapefile's `.prj` file.
    pub fn process(
        source_name: &str,
        source_paths: Vec<PathBuf>,
        writer: &std::sync::Arc<crate::collection::feature_group::FeatureGroup>,
        config: &std::sync::Arc<crate::config::planetiler_config::PlanetilerConfig>,
        profile: &std::sync::Arc<dyn crate::profile::Profile>,
        stats: crate::stats::stats::StatsRef,
    ) -> Result<()> {
        Self::process_with_projection(None, source_name, source_paths, writer, config, profile, stats)
    }
}

#[cfg(test)]
mod tests {
    use geo::Centroid;

    use super::*;

    const ENV_CENTER: (f64, f64) = (-77.12911152370515, 38.79930767201779);
    const NUM_IN_ENV: usize = 18;

    fn env() -> Envelope {
        let mut env = Envelope::of_point(ENV_CENTER.0, ENV_CENTER.1);
        env.expand_by(0.1);
        env
    }

    fn resource(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources").join(name)
    }

    fn centroid(features: &[ReaderFeature]) -> Coord<f64> {
        let gc = geo_types::GeometryCollection(features.iter().map(|f| f.geometry.clone()).collect());
        gc.centroid().unwrap().0
    }

    fn test_read_shapefile(path: &Path) {
        let reader = ShapefileReader::new(None, "test", path).unwrap();
        for i in 1..=2 {
            assert_eq!(86, reader.feature_count().unwrap());
            let features = reader.read_all().unwrap();
            let mut names = vec![];
            for f in &features {
                assert!(matches!(f.get_tag("name"), Some(Value::String(_))));
                assert_eq!(Some("stations"), f.source_layer.as_deref());
                names.push(f.get_tag("name").unwrap().to_string_lossy().to_string());
            }
            let env = env();
            assert_eq!(
                NUM_IN_ENV,
                features
                    .iter()
                    .filter(|f| match &f.geometry {
                        Geometry::Point(p) => env.contains_point(p.x(), p.y()),
                        _ => false,
                    })
                    .count()
            );
            assert_eq!(86, features.len());
            assert!(names.contains(&"Van Dörn Street".to_string()));
            let c = centroid(&features);
            assert!((c.x - -77.0297995).abs() < 1e-5, "iter {i}");
            assert!((c.y - 38.9119684).abs() < 1e-5, "iter {i}");
            let ids: Vec<i64> = features.iter().map(|f| f.id).collect();
            assert_eq!((1..=86).collect::<Vec<_>>(), ids);
        }
    }

    #[test]
    fn read_shapefile_in_zip() {
        test_read_shapefile(&resource("shapefile.zip").join("shapefile").join("stations.shp"));
        test_read_shapefile(&resource("shapefile.zip"));
    }

    #[test]
    fn read_shapefile_unzipped() {
        let dir = tempfile::tempdir().unwrap();
        file_utils::unzip_file(&resource("shapefile.zip"), dir.path()).unwrap();
        test_read_shapefile(&dir.path().join("shapefile").join("stations.shp"));
    }

    #[test]
    fn read_shapefile_single_thread() {
        let reader = ShapefileReader::new(None, "test", &resource("shapefile.zip")).unwrap().with_decode_threads(1);
        assert_eq!(86, reader.read_all().unwrap().len());
    }

    #[test]
    fn read_shapefile_with_bounding_box() {
        let dir = tempfile::tempdir().unwrap();
        file_utils::unzip_file(&resource("shapefile.zip"), dir.path()).unwrap();
        let path = dir.path().join("shapefile").join("stations.shp");
        let reader = ShapefileReader::with_bounds(None, "test", &path, &Bounds::new(Some(env()))).unwrap();
        for i in 1..=2 {
            assert_eq!(NUM_IN_ENV as u64, reader.feature_count().unwrap());
            let features = reader.read_all().unwrap();
            for f in &features {
                assert!(matches!(f.get_tag("name"), Some(Value::String(_))));
                assert_eq!(Some("stations"), f.source_layer.as_deref());
            }
            assert_eq!(NUM_IN_ENV, features.len());
            let c = centroid(&features);
            assert!((c.x - -77.0934256).abs() < 1e-5, "iter {i}");
            assert!((c.y - 38.8509022).abs() < 1e-5, "iter {i}");
        }
    }

    #[test]
    fn read_shapefile_with_transverse_mercator_projection() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("test.shp");
        {
            let table = shapefile::dbase::TableWriterBuilder::new().add_numeric_field(
                shapefile::dbase::FieldName::try_from("value").unwrap(),
                10,
                0,
            );
            let mut writer = shapefile::Writer::from_path(&shp_path, table).unwrap();
            let mut record = shapefile::dbase::Record::default();
            record.insert("value".to_string(), shapefile::dbase::FieldValue::Numeric(Some(3.0)));
            writer.write_shape_and_record(&shapefile::Point::new(1.0, 2.0), &record).unwrap();
        }
        std::fs::write(
            dir.path().join("test.prj"),
            r#"PROJCS["SWEREF99_TM",GEOGCS["GCS_SWEREF99",DATUM["D_SWEREF99",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",15.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#,
        )
        .unwrap();
        let reader = ShapefileReader::new(None, "test", &shp_path).unwrap();
        assert_eq!(1, reader.feature_count().unwrap());
        let features = reader.read_all().unwrap();
        assert_eq!(1, features.len());
        let c = features[0].geometry.centroid().unwrap();
        assert!((c.x() - 10.5113).abs() < 1e-4, "{c:?}");
        assert!(c.y().abs() < 1e-4, "{c:?}");
        assert_eq!(Some(&Value::Int(3)), features[0].get_tag("value"));
        assert_eq!(Some("test"), features[0].source_layer.as_deref());
    }

    #[test]
    fn read_polygons_and_lines() {
        let dir = tempfile::tempdir().unwrap();
        let shp_path = dir.path().join("polys.shp");
        {
            let table = shapefile::dbase::TableWriterBuilder::new()
                .add_character_field(shapefile::dbase::FieldName::try_from("name").unwrap(), 20);
            let mut writer = shapefile::Writer::from_path(&shp_path, table).unwrap();
            let mut record = shapefile::dbase::Record::default();
            record.insert("name".to_string(), shapefile::dbase::FieldValue::Character(Some("a".into())));
            // shell clockwise + hole counter-clockwise
            let shell = vec![
                shapefile::Point::new(0.0, 0.0),
                shapefile::Point::new(0.0, 10.0),
                shapefile::Point::new(10.0, 10.0),
                shapefile::Point::new(10.0, 0.0),
                shapefile::Point::new(0.0, 0.0),
            ];
            let hole = vec![
                shapefile::Point::new(1.0, 1.0),
                shapefile::Point::new(2.0, 1.0),
                shapefile::Point::new(2.0, 2.0),
                shapefile::Point::new(1.0, 2.0),
                shapefile::Point::new(1.0, 1.0),
            ];
            let shell2: Vec<_> = shell.iter().map(|p| shapefile::Point::new(p.x + 20.0, p.y)).collect();
            let polygon = shapefile::Polygon::with_rings(vec![
                shapefile::PolygonRing::Outer(shell),
                shapefile::PolygonRing::Outer(shell2),
                shapefile::PolygonRing::Inner(hole),
            ]);
            writer.write_shape_and_record(&polygon, &record).unwrap();
        }
        let reader = ShapefileReader::new(None, "test", &shp_path).unwrap();
        let features = reader.read_all().unwrap();
        assert_eq!(1, features.len());
        let Geometry::MultiPolygon(mp) = &features[0].geometry else { panic!("{:?}", features[0].geometry) };
        assert_eq!(2, mp.0.len());
        assert_eq!(1, mp.0[0].interiors().len());
        assert_eq!(0, mp.0[1].interiors().len());
        assert_eq!(Some(&Value::str("a")), features[0].get_tag("name"));
    }
}

#[cfg(test)]
mod process_tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::reader::source_feature_processor::test_support::*;

    #[test]
    fn process_shapefile_with_profile() {
        let zip = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources/shapefile.zip");
        for (write_threads, proj) in [(false, None), (true, Some("EPSG:4326"))] {
            let profile = Arc::new(CollectingProfile::default());
            let output = run_with_group(config(write_threads), profile.clone(), |group, config, profile, stats| {
                ShapefileReader::process_with_projection(
                    proj,
                    "shapefile",
                    vec![zip.clone()],
                    group,
                    config,
                    profile,
                    stats,
                )
            });
            profile.assert_saw("shapefile", &[("stations", 86)]);
            assert_eq!(86, output.len());
            assert!(output.iter().all(|(_, layer, _)| layer == "stations"));
            assert_eq!(vec!["shapefile"], *profile.finished.lock());
        }
    }
}
