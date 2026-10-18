//! Port of `reader/parquet/ParquetInputFile.java`: reads features from a single
//! [geoparquet](https://github.com/opengeospatial/geoparquet/blob/main/format-specs/geoparquet.md) file.
//!
//! A file is split into [`Block`]s (one per row group) that can be read independently from different threads. When
//! bounds are provided, row groups are pruned using the statistics of the covering bbox columns and rows are
//! filtered by those columns; if there is no covering bbox the decoded geometries are post-filtered instead.

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder};
use parquet::basic::{ConvertedType, Type as PhysicalType};
use parquet::column::reader::ColumnReader;
use parquet::file::reader::{FileReader, RowGroupReader};
use parquet::file::serialized_reader::SerializedFileReader;
use parquet::schema::types::SchemaDescPtr;
use tracing::warn;

use crate::config::bounds::Bounds;
use crate::expression::expression::PartialInput;
use crate::geo::envelope::Envelope;
use crate::geo::geometry_type::GeometryType;
use crate::reader::parquet::geo_arrow::GeoArrowColumn;
use crate::reader::parquet::geo_parquet_metadata::{FilterPredicate, GeoParquetMetadata};
use crate::reader::parquet::geometry_reader::{GeometryEncoding, GeometryReader};
use crate::reader::parquet::interval::Interval;
use crate::reader::parquet::parquet_feature::ParquetFeature;
use crate::reader::parquet::parquet_record_converter::ParquetRecordConverter;
use crate::util::hashing;
use crate::value::{Tags, Value};

/// Computes a feature ID from its tags.
pub type IdGenerator = Arc<dyn Fn(&Tags) -> Value + Send + Sync>;

const BATCH_SIZE: usize = 4096;

struct Inner {
    path: Arc<PathBuf>,
    source: Arc<str>,
    layer: Arc<str>,
    metadata: ArrowReaderMetadata,
    schema: SchemaDescPtr,
    filter: Option<FilterPredicate>,
    id_generator: Option<IdGenerator>,
    extra_fields: Option<Tags>,
    geometry_reader: Arc<GeometryReader>,
    geometry_types: HashSet<GeometryType>,
    post_filter_bounds: Option<Envelope>,
    out_of_bounds: bool,
    count: u64,
    block_count: usize,
    converter: ParquetRecordConverter,
    /// arrow column index of the primary geometry, when it is geoarrow-encoded and decoded straight from arrow
    geoarrow_column: Option<(usize, crate::reader::parquet::geo_arrow::GeoArrowEncoding)>,
    /// `(name, leaf column index)` of top-level INTERVAL columns that arrow can't fully decode
    interval_columns: Vec<(String, usize)>,
}

/// Reads features from a single geoparquet file.
#[derive(Clone)]
pub struct ParquetInputFile {
    inner: Arc<Inner>,
}

/// Java `ParquetInputFile.hashToLong`.
pub fn hash_to_long(value: &Value) -> i64 {
    match value {
        Value::String(s) => hashing::fnv1a64(s.as_bytes()) as i64,
        Value::Bytes(b) => hashing::fnv1a64(b) as i64,
        Value::Int(i) => *i,
        Value::Float(f) => f.to_bits() as i32 as i64,
        Value::Double(d) => d.to_bits() as i64,
        Value::Null => 0,
        other => hashing::fnv1a64(other.to_string().as_bytes()) as i64,
    }
}

impl ParquetInputFile {
    /// Opens `path`, reading every feature.
    pub fn new(source: &str, layer: &str, path: &Path) -> Result<ParquetInputFile> {
        Self::with_options(source, layer, path, None, &Bounds::world(), None, None)
    }

    /// Opens `path` with an optional row `filter`, only reading features in `bounds`, adding `extra_fields` (hive
    /// partition fields) to every feature and computing IDs with `id_generator`.
    pub fn with_options(
        source: &str,
        layer: &str,
        path: &Path,
        filter: Option<FilterPredicate>,
        bounds: &Bounds,
        extra_fields: Option<Tags>,
        id_generator: Option<IdGenerator>,
    ) -> Result<ParquetInputFile> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let metadata = ArrowReaderMetadata::load(&file, ArrowReaderOptions::new())
            .with_context(|| format!("reading parquet metadata from {}", path.display()))?;
        let parquet_metadata = metadata.metadata().clone();
        let file_metadata = parquet_metadata.file_metadata();
        let schema = file_metadata.schema_descr_ptr();
        let geoparquet = GeoParquetMetadata::parse(file_metadata)
            .with_context(|| format!("reading geoparquet metadata from {}", path.display()))?;
        let geometry_reader = GeometryReader::new(&geoparquet)?;
        let geometry_types = geoparquet.geometry_types()?;
        let converter = ParquetRecordConverter::new(metadata.schema(), &schema, Some(&geoparquet))
            .with_context(|| format!("in {}", path.display()))?;
        let mut filter = filter;
        let mut out_of_bounds = false;
        let mut post_filter_bounds = None;
        if !bounds.is_world() {
            let primary = geoparquet.primary_column_metadata()?;
            if !primary.envelope().intersects(&bounds.lat_lon()) {
                out_of_bounds = true;
            } else if let Some(bbox_filter) = primary.bbox_filter(&schema, bounds) {
                filter = Some(match filter {
                    Some(f) => FilterPredicate::and(f, bbox_filter),
                    None => bbox_filter,
                });
            } else {
                warn!("No covering column specified in geoparquet metadata, fall back to post-filtering");
                post_filter_bounds = Some(bounds.lat_lon());
            }
        }
        let count = if out_of_bounds {
            0
        } else {
            parquet_metadata
                .row_groups()
                .iter()
                .filter(|rg| !filter.as_ref().is_some_and(|f| f.can_drop(rg)))
                .map(|rg| rg.num_rows().max(0) as u64)
                .sum()
        };
        let block_count = if out_of_bounds { 0 } else { parquet_metadata.num_row_groups() };
        let geoarrow_column = match geometry_reader.primary_encoding() {
            Some(GeometryEncoding::GeoArrow(e)) => {
                metadata.schema().index_of(&geometry_reader.geometry_column).ok().map(|i| (i, e))
            }
            _ => None,
        };
        let interval_columns = schema
            .root_schema()
            .get_fields()
            .iter()
            .filter(|f| {
                f.is_primitive()
                    && f.get_physical_type() == PhysicalType::FIXED_LEN_BYTE_ARRAY
                    && f.get_basic_info().converted_type() == ConvertedType::INTERVAL
            })
            .filter_map(|f| {
                let leaf = schema.columns().iter().position(|c| c.path().parts() == [f.name().to_string()])?;
                Some((f.name().to_string(), leaf))
            })
            .collect();
        Ok(ParquetInputFile {
            inner: Arc::new(Inner {
                path: Arc::new(path.to_path_buf()),
                source: source.into(),
                layer: layer.into(),
                metadata,
                schema,
                filter,
                id_generator,
                extra_fields,
                geometry_reader: Arc::new(geometry_reader),
                geometry_types,
                post_filter_bounds,
                out_of_bounds,
                count,
                block_count,
                converter,
                geoarrow_column,
                interval_columns,
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    pub fn layer(&self) -> &str {
        &self.inner.layer
    }

    pub fn geometry_reader(&self) -> &GeometryReader {
        &self.inner.geometry_reader
    }

    /// True if a filter (user-provided or from bounds) is applied to rows.
    pub fn has_filter(&self) -> bool {
        self.inner.filter.is_some()
    }

    /// What is known ahead of time about all features in this file, for `Profile.caresAbout`.
    pub fn partial_input(&self) -> PartialInput {
        PartialInput::new(
            [self.inner.source.to_string()],
            [self.inner.layer.to_string()],
            self.inner.extra_fields.clone().unwrap_or_default(),
            self.inner.geometry_types.iter().copied(),
        )
    }

    /// Java `shouldSkip(profile)`: true if this file is outside the bounds or `cares_about` (`profile.caresAbout`)
    /// returns false for [`ParquetInputFile::partial_input`].
    pub fn should_skip(&self, cares_about: &dyn Fn(&PartialInput) -> bool) -> bool {
        self.inner.out_of_bounds || !cares_about(&self.partial_input())
    }

    /// Number of records that will be read (after row group pruning).
    pub fn count(&self) -> u64 {
        self.inner.count
    }

    /// Number of blocks (row groups).
    pub fn block_count(&self) -> usize {
        self.inner.block_count
    }

    /// The blocks of this file, which can be read independently.
    pub fn get(&self) -> Vec<Block> {
        if self.inner.out_of_bounds {
            return vec![];
        }
        (0..self.inner.metadata.metadata().num_row_groups())
            .map(|index| Block { file: self.inner.clone(), index })
            .collect()
    }
}

/// A row group of a parquet file.
#[derive(Clone)]
pub struct Block {
    file: Arc<Inner>,
    index: usize,
}

fn read_interval_column(reader: &dyn RowGroupReader, leaf: usize, num_rows: usize) -> Result<Vec<Option<Interval>>> {
    let ColumnReader::FixedLenByteArrayColumnReader(mut column) = reader.get_column_reader(leaf)? else {
        return Ok(vec![None; num_rows]);
    };
    let mut def_levels = Vec::with_capacity(num_rows);
    let mut values = Vec::with_capacity(num_rows);
    let mut records = 0;
    while records < num_rows {
        let (read, _, _) = column.read_records(num_rows - records, Some(&mut def_levels), None, &mut values)?;
        if read == 0 {
            break;
        }
        records += read;
    }
    let mut values = values.into_iter();
    Ok(def_levels
        .into_iter()
        .map(|d| if d > 0 { values.next().and_then(|v| Interval::from_parquet_bytes(v.data())) } else { None })
        .collect())
}

impl Block {
    pub fn file_name(&self) -> &Path {
        &self.file.path
    }

    pub fn layer(&self) -> &str {
        &self.file.layer
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// Reads all features in this block into a vector.
    pub fn features(&self) -> Result<Vec<ParquetFeature>> {
        let mut result = vec![];
        self.read(&mut |f| {
            result.push(f);
            Ok(())
        })?;
        Ok(result)
    }

    /// Reads every feature in this block that passes the filters, passing each to `next`.
    pub fn read(&self, next: &mut dyn FnMut(ParquetFeature) -> Result<()>) -> Result<()> {
        let file = &self.file;
        let row_group = file.metadata.metadata().row_group(self.index);
        if file.filter.as_ref().is_some_and(|f| f.can_drop(row_group)) {
            return Ok(());
        }
        let num_rows = row_group.num_rows().max(0) as usize;
        let mut intervals = vec![];
        if !file.interval_columns.is_empty() {
            let reader = SerializedFileReader::new(File::open(file.path.as_ref())?)?;
            let rg = reader.get_row_group(self.index)?;
            for (name, leaf) in &file.interval_columns {
                intervals.push((name.as_str(), read_interval_column(rg.as_ref(), *leaf, num_rows)?));
            }
        }
        let reader =
            ParquetRecordBatchReaderBuilder::new_with_metadata(File::open(file.path.as_ref())?, file.metadata.clone())
                .with_row_groups(vec![self.index])
                .with_batch_size(BATCH_SIZE)
                .build()?;
        let file_hash = hashing::fnv1a64(file.path.to_string_lossy().as_bytes());
        let block_hash = hashing::fnv1a64_with_init(file_hash, &(self.index as i32).to_be_bytes());
        let mut i: i64 = 0;
        for batch in reader {
            let batch = batch?;
            let mask = file.filter.as_ref().map(|f| f.evaluate(&batch)).transpose()?;
            let geoarrow = match file.geoarrow_column {
                Some((idx, encoding)) => Some((idx, GeoArrowColumn::new(encoding, batch.column(idx))?)),
                None => None,
            };
            for row in 0..batch.num_rows() {
                i += 1;
                if mask.as_ref().is_some_and(|m| !m[row]) {
                    continue;
                }
                let mut tags = file.converter.convert_row_skipping(&batch, row, geoarrow.as_ref().map(|(idx, _)| *idx));
                for (name, values) in &intervals {
                    if let Some(Some(interval)) = values.get(i as usize - 1) {
                        tags.insert(name.to_string(), Value::str(interval.to_string()));
                    }
                }
                if let Some(extra) = &file.extra_fields {
                    for (k, v) in extra {
                        tags.insert(k.clone(), v.clone());
                    }
                }
                let id = match &file.id_generator {
                    Some(generator) => hash_to_long(&generator(&tags)),
                    None => hashing::fnv1a64_with_init(block_hash, &i.to_be_bytes()) as i64,
                };
                let mut feature = ParquetFeature::new(
                    file.source.clone(),
                    file.layer.clone(),
                    id,
                    file.geometry_reader.clone(),
                    tags,
                    file.path.clone(),
                    Some(file.schema.clone()),
                );
                if let Some((_, column)) = &geoarrow {
                    feature = feature.with_geometry(column.get(row));
                }
                if let Some(bounds) = &file.post_filter_bounds {
                    match feature.lat_lon_geometry() {
                        Ok(geom) => {
                            if !Envelope::of_geometry(geom).intersects(bounds) {
                                continue;
                            }
                        }
                        Err(e) => {
                            warn!("Error reading geometry to post-filter bounds: {e}");
                            continue;
                        }
                    }
                }
                next(feature)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::expression::expression::Expression;
    use crate::reader::parquet::geo_parquet_metadata::{CompareOp, FilterValue};
    use crate::reader::with_tags::WithTags;
    use crate::tags;

    pub(crate) fn parquet_resource(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources/parquet").join(name)
    }

    pub(crate) fn bostons() -> Vec<PathBuf> {
        let mut result: Vec<PathBuf> = std::fs::read_dir(parquet_resource(""))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.starts_with("boston") && name.ends_with(".parquet")
            })
            .collect();
        result.sort();
        assert!(!result.is_empty());
        result
    }

    fn ids(file: &ParquetInputFile) -> HashSet<String> {
        let mut ids = HashSet::new();
        for block in file.get() {
            for item in block.features().unwrap() {
                ids.insert(item.get_string("id").unwrap());
            }
        }
        ids
    }

    #[test]
    fn read_boston() {
        for path in bostons() {
            for i in 0..3 {
                let file = ParquetInputFile::new("parquet", "layer", &path).unwrap();
                let mut ids = HashSet::new();
                for block in file.get() {
                    for item in block.features().unwrap() {
                        ids.insert(item.get_string("id").unwrap());
                        let geometry_column = &file.geometry_reader().geometry_column;
                        assert!(!item.has_tag(geometry_column));
                        assert!(item.get_tag(geometry_column).is_none());
                        assert!(item.can_be_polygon(), "{path:?}");
                        assert!(item.lat_lon_geometry().is_ok(), "{path:?}");
                    }
                }
                assert_eq!(3, ids.len(), "{path:?} iter {i}");
            }
        }
    }

    #[test]
    fn read_boston_with_profile_filter() {
        let path = parquet_resource("boston.parquet");
        let filter = crate::expression::expression::match_any("type", ["value"]);
        let cares_about = |input: &PartialInput| filter.partial_evaluate(input) != Expression::FALSE;
        let file1 = ParquetInputFile::with_options(
            "parquet",
            "layer",
            &path,
            None,
            &Bounds::world(),
            Some(tags! {"type" => "value"}),
            None,
        )
        .unwrap();
        let file2 = ParquetInputFile::with_options(
            "parquet",
            "layer",
            &path,
            None,
            &Bounds::world(),
            Some(tags! {"type" => "other"}),
            None,
        )
        .unwrap();
        assert!(!file1.should_skip(&cares_about));
        assert!(file2.should_skip(&cares_about));
    }

    fn bounds(min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Bounds {
        Bounds::new(Some(Envelope::new(min_x, max_x, min_y, max_y)))
    }

    #[test]
    fn read_boston_with_bbox_filter_covering() {
        for path in bostons() {
            let file = ParquetInputFile::with_options(
                "parquet",
                "layer",
                &path,
                None,
                &bounds(-71.0747653629, -71.0741656634, 42.3560968301, 42.3564346282),
                None,
                None,
            )
            .unwrap();
            assert_eq!(3, ids(&file).len(), "{path:?}");
        }
    }

    #[test]
    fn read_boston_with_bbox_filter_covering_and_other_filter() {
        for path in bostons() {
            let file = ParquetInputFile::with_options(
                "parquet",
                "layer",
                &path,
                Some(FilterPredicate::compare(&["height"], CompareOp::Gt, FilterValue::Double(3.0))),
                &bounds(-71.0747653629, -71.0741656634, 42.3560968301, 42.3564346282),
                None,
                None,
            )
            .unwrap();
            assert_eq!(1, ids(&file).len(), "{path:?}");
        }
    }

    #[test]
    fn read_boston_with_bbox_filter_not_covering() {
        for path in bostons() {
            let file = ParquetInputFile::with_options(
                "parquet",
                "layer",
                &path,
                None,
                &bounds(-72.0747653629, -72.0741656634, 42.3560968301, 42.3564346282),
                None,
                None,
            )
            .unwrap();
            assert_eq!(0, ids(&file).len(), "{path:?}");
        }
    }

    #[test]
    fn read_all_data_types() {
        let file = ParquetInputFile::new("parquet", "layer", &parquet_resource("all_data_types.parquet")).unwrap();
        let mut features = vec![];
        for block in file.get() {
            features.extend(block.features().unwrap());
        }
        assert_eq!(1, features.len());
        let map = features[0].tags();
        let get = |k: &str| map.get(k).cloned().unwrap_or(Value::Null);
        assert_eq!(Value::Int(9223372036854775807), get("bigint"));
        assert_eq!(Value::Bytes(b"1011".to_vec().into()), get("blob"));
        assert_eq!(Value::Bool(true), get("boolean"));
        assert_eq!(Value::str("2000-01-01"), get("date"));
        assert_eq!(Value::Double(123456.789), get("decimal"));
        assert_eq!(Value::Double(123456.789), get("double"));
        assert_eq!(Value::Double(92233720368547758079223372036854775807.0), get("hugeint"));
        assert_eq!(Value::Int(123), get("integer"));
        assert_eq!(Value::str(Interval::of(1, 2, 3_000).to_string()), get("interval"));
        assert!((get("real").as_f64().unwrap() - 123.456).abs() < 1e-3);
        assert_eq!(Value::Int(1234), get("smallint"));
        assert_eq!(Value::str("05:30:10.123"), get("time"));
        assert_eq!(Value::str("2000-01-01T10:30:10.123Z"), get("timestamp_with_timezone"));
        assert_eq!(Value::str("2000-01-01T10:30:10.123Z"), get("timestamp"));
        assert_eq!(Value::Int(123), get("tinyint"));
        assert_eq!(Value::Int(9223372036854775807), get("ubigint"));
        assert_eq!(Value::Double(92233720368547758079223372036854775807.0), get("uhugeint"));
        assert_eq!(Value::Int(123), get("uinteger"));
        assert_eq!(Value::Int(123), get("usmallint"));
        assert_eq!(Value::Int(123), get("utinyint"));
        assert_eq!(Value::str("606362d9-012a-4949-b91a-1ab439951671"), get("uuid"));
        assert_eq!(Value::str("string"), get("varchar"));
        assert_eq!(Value::list([1, 2, 3, 4].map(Value::Int)), get("list_of_items"));
        assert_eq!(Value::map(tags! {"1" => "one", "2" => "two", "3" => "three"}), get("map"));
        assert_eq!(Value::list([1, 2, 3].map(Value::Int)), get("array"));
        assert_eq!(Value::map(tags! {"i" => 42, "j" => "a"}), get("struct"));
        assert_eq!(
            Value::list([Value::map(tags! {
                "a" => Value::list([]),
                "b" => Value::list([
                    Value::map(tags! {"i" => 43, "j" => "a"}),
                    Value::map(tags! {"i" => 43, "j" => "b"}),
                ]),
            })]),
            get("complex")
        );
    }

    #[test]
    fn read_nested() {
        let file = ParquetInputFile::new("parquet", "layer", &parquet_resource("boston.parquet")).unwrap();
        let mut xmins = HashSet::new();
        let mut update_times = HashSet::new();
        for block in file.get() {
            for item in block.features().unwrap() {
                xmins.insert(item.get_tag_nested("bbox.xmin").unwrap().as_f64().unwrap().to_bits());
                update_times.insert(item.get_struct("update_time").as_timestamp().unwrap().timestamp_millis());
            }
        }
        assert_eq!(
            HashSet::from([-71.0743637084961f64, -71.07461547851562, -71.07460021972656].map(f64::to_bits)),
            xmins
        );
        assert_eq!(HashSet::from([1596647976000i64, 1624238059000, 1625971545000]), update_times);
    }

    #[test]
    fn hash_ids() {
        assert_eq!(1, hash_to_long(&Value::Int(1)));
        assert_eq!(0, hash_to_long(&Value::Null));
        assert_eq!(hashing::fnv1a64(b"abc") as i64, hash_to_long(&Value::str("abc")));
        assert_eq!(1.5f64.to_bits() as i64, hash_to_long(&Value::Double(1.5)));
    }

    mod geoarrow {
        use arrow::array::{ArrayRef, Float64Array, ListArray, StructArray};
        use arrow::buffer::OffsetBuffer;
        use arrow::datatypes::{DataType, Field, Fields, Schema};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        use parquet::file::metadata::KeyValue;
        use parquet::file::properties::WriterProperties;

        use super::*;
        use crate::geo::geo_utils;

        fn coords(xy: &[(f64, f64)]) -> ArrayRef {
            let fields = Fields::from(vec![
                Field::new("x", DataType::Float64, false),
                Field::new("y", DataType::Float64, false),
            ]);
            Arc::new(StructArray::new(
                fields,
                vec![
                    Arc::new(Float64Array::from(xy.iter().map(|c| c.0).collect::<Vec<_>>())),
                    Arc::new(Float64Array::from(xy.iter().map(|c| c.1).collect::<Vec<_>>())),
                ],
                None,
            ))
        }

        /// Wrap `values` in a list array with the given list sizes.
        fn list(values: ArrayRef, sizes: &[usize]) -> ArrayRef {
            let field = Arc::new(Field::new("element", values.data_type().clone(), true));
            Arc::new(ListArray::new(field, OffsetBuffer::from_lengths(sizes.iter().copied()), values, None))
        }

        fn write(dir: &Path, encoding: &str, geometry: ArrayRef) -> PathBuf {
            let path = dir.join(format!("{encoding}.parquet"));
            let schema = Arc::new(Schema::new(vec![
                Field::new("id", DataType::Float64, false),
                Field::new("geometry", geometry.data_type().clone(), true),
            ]));
            let n = geometry.len();
            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(Float64Array::from((0..n).map(|i| i as f64).collect::<Vec<_>>())), geometry],
            )
            .unwrap();
            let geo =
                format!(r#"{{"primary_column": "geometry", "columns": {{"geometry": {{"encoding": "{encoding}"}}}}}}"#);
            let props = WriterProperties::builder()
                .set_key_value_metadata(Some(vec![KeyValue::new("geo".to_string(), geo)]))
                .build();
            let mut writer = ArrowWriter::try_new(File::create(&path).unwrap(), schema, Some(props)).unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();
            path
        }

        fn read(path: &Path) -> Vec<String> {
            let file = ParquetInputFile::new("source", "layer", path).unwrap();
            let mut result = vec![];
            for block in file.get() {
                for feature in block.features().unwrap() {
                    assert!(!feature.has_tag("geometry"));
                    result.push(geo_utils::to_wkt(feature.lat_lon_geometry().unwrap()));
                }
            }
            result
        }

        #[test]
        fn geoarrow_encodings() {
            let dir = tempfile::tempdir().unwrap();
            let ring = |o: f64| vec![(o, o), (o + 1.0, o), (o + 1.0, o + 1.0), (o, o), (o, o)];
            let ring_coords: Vec<(f64, f64)> = [ring(0.0), ring(0.25)].concat();
            let cases: Vec<(&str, ArrayRef, Vec<&str>)> = vec![
                ("point", coords(&[(1.0, 2.0), (3.0, 4.0)]), vec!["POINT(1 2)", "POINT(3 4)"]),
                ("linestring", list(coords(&[(1.0, 2.0), (3.0, 4.0)]), &[2]), vec!["LINESTRING(1 2,3 4)"]),
                ("multipoint", list(coords(&[(1.0, 2.0), (3.0, 4.0)]), &[2]), vec!["MULTIPOINT((1 2),(3 4))"]),
                (
                    "multilinestring",
                    list(list(coords(&[(1.0, 2.0), (3.0, 4.0), (5.0, 6.0), (7.0, 8.0)]), &[2, 2]), &[2]),
                    vec!["MULTILINESTRING((1 2,3 4),(5 6,7 8))"],
                ),
                (
                    "polygon",
                    list(list(coords(&ring_coords), &[5, 5]), &[2]),
                    vec!["POLYGON((0 0,1 0,1 1,0 0,0 0),(0.25 0.25,1.25 0.25,1.25 1.25,0.25 0.25,0.25 0.25))"],
                ),
                (
                    "multipolygon",
                    list(list(list(coords(&ring_coords), &[5, 5]), &[1, 1]), &[2]),
                    vec!["MULTIPOLYGON(((0 0,1 0,1 1,0 0,0 0)),((0.25 0.25,1.25 0.25,1.25 1.25,0.25 0.25,0.25 0.25)))"],
                ),
            ];
            for (encoding, geometry, expected) in cases {
                let path = write(dir.path(), encoding, geometry);
                let actual: Vec<String> =
                    read(&path).into_iter().map(|s| s.replace(", ", ",").replace(" (", "(")).collect();
                assert_eq!(expected, actual, "{encoding}");
            }
        }

        #[test]
        fn geoarrow_schema_validation() {
            let dir = tempfile::tempdir().unwrap();
            let point = coords(&[(1.0, 2.0)]);
            let line = list(coords(&[(1.0, 2.0), (3.0, 4.0)]), &[2]);
            let poly = list(list(coords(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]), &[4]), &[1]);
            let multipoly = list(poly.clone(), &[1]);
            let arrays = [point, line, poly, multipoly];
            for (encoding, valid) in [
                ("point", 0),
                ("multipoint", 1),
                ("linestring", 1),
                ("multilinestring", 2),
                ("polygon", 2),
                ("multipolygon", 3),
            ] {
                for (i, array) in arrays.iter().enumerate() {
                    let subdir = dir.path().join(format!("{encoding}{i}"));
                    std::fs::create_dir_all(&subdir).unwrap();
                    let path = write(&subdir, encoding, array.clone());
                    let result = ParquetInputFile::new("source", "layer", &path);
                    assert_eq!(i == valid, result.is_ok(), "{encoding} {i}");
                }
            }
        }

        #[test]
        fn wkb_requires_binary() {
            let dir = tempfile::tempdir().unwrap();
            let path = write(dir.path(), "WKB", coords(&[(1.0, 2.0)]));
            assert!(ParquetInputFile::new("source", "layer", &path).is_err());
        }
    }
}
