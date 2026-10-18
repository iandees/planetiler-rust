//! Port of `reader/parquet/ParquetReader.java`: reads features from one or more
//! [geoparquet](https://github.com/opengeospatial/geoparquet/blob/main/format-specs/geoparquet.md) files.
//!
//! If files don't contain geoparquet metadata then it tries to get geometry from `geometry`, `wkb_geometry` or
//! `wkt_geometry` fields. Row groups ("blocks") of all files are processed in parallel by
//! `feature_process_threads` threads.
//!
//! [`ParquetReader::process`] (Java's `process(sourcePaths, writer, config)`) runs the profile on each feature and
//! renders into a `FeatureGroup`; [`ParquetReader::process_with`] hands features to caller-provided per-thread
//! closures instead.
//!
//! Like Java, each processing thread reads whole blocks but passes its work through a shared [`Distributor`] so
//! threads that run out of blocks help with the others' (for example one stuck on a huge row group). While every
//! thread is busy, work stays on the thread that read it. Each thread keeps its own profile processor and
//! `FeatureGroup` encoder/sink, so the only shared state in the hot path is the distributor's `done` counter (and
//! its bounded queue once some threads are idle). Difference from Java: the unit of work handed off is a chunk of up
//! to 256 decoded arrow rows ([`RowChunk`]), not a built feature; see `Progress::process_blocks`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use tracing::{error, info};

use crate::collection::feature_group::FeatureGroup;
use crate::config::bounds::Bounds;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::expression::expression::PartialInput;
use crate::profile::Profile;
use crate::reader::parquet::parquet_feature::ParquetFeature;
use crate::reader::parquet::parquet_input_file::{Block, IdGenerator, ParquetInputFile, RowChunk};
use crate::reader::source_feature_processor::{finish_source, process_and_write_with_profile};
use crate::stats::counter::{Counter, ThreadCounter};
use crate::stats::progress_loggers::ProgressLoggers;
use crate::stats::stats::StatsRef;
use crate::util::file_utils::DiskBacked;
use crate::util::format;
use crate::value::{Tags, Value};
use crate::worker::distributor::Distributor;
use crate::worker::worker_pipeline::{Builder, WorkerPipeline};

pub const DEFAULT_LAYER: &str = "features";

/// Maximum number of rows in one unit of work handed to another thread: big enough that the handoff (a queue
/// operation and often a thread wake-up) is negligible next to processing the rows, small enough to balance the end of
/// the stage finely.
const CHUNK_ROWS: usize = 256;

/// Maximum number of chunks waiting in the [`Distributor`] queue for an idle thread (about 16k rows; Java queues up to
/// 1,000 features).
const DISTRIBUTOR_CAPACITY: usize = 64;

/// Computes the layer name for a file from its hive partition fields.
pub type LayerGenerator = Arc<dyn Fn(&Tags) -> Value + Send + Sync>;

/// Processes one feature (called from a processing thread).
pub type ParquetProcessFn = Box<dyn FnMut(ParquetFeature) -> Result<()> + Send>;

/// Progress counters shared by the processing threads of one run.
#[derive(Clone)]
struct Progress {
    blocks: Counter,
    features: Counter,
    /// Number of blocks currently being processed, by layer.
    working_on: Arc<Mutex<BTreeMap<String, i64>>>,
}

impl Progress {
    /// Decodes `block` into row chunks passed to `f`, updating the block counters.
    fn read_block(
        &self,
        block: &Block,
        blocks: &ThreadCounter,
        f: &mut dyn FnMut(RowChunk) -> Result<()>,
    ) -> Result<()> {
        let layer = block.layer();
        {
            let mut working_on = self.working_on.lock();
            match working_on.get_mut(layer) {
                Some(count) => *count += 1,
                None => {
                    working_on.insert(layer.to_string(), 1);
                }
            }
        }
        block.read_chunks(CHUNK_ROWS, f)?;
        blocks.inc();
        if let Some(count) = self.working_on.lock().get_mut(layer) {
            *count -= 1;
        }
        Ok(())
    }

    /// The body of one processing thread: reads every block from `blocks_in` and passes each feature to `process`.
    ///
    /// With a `distributor` (Java's `Distributor<ParquetFeature>`), the decoded rows go through it in chunks of
    /// [`CHUNK_ROWS`]: while every thread is busy this thread converts and processes them itself, once some threads
    /// have run out of blocks they take chunks from this one; when this thread runs out of blocks it processes
    /// chunks from the others until they are all done (Java's `consumer.close()`). Difference from Java: Java hands
    /// off individual features after the reading thread has built them. Here the unit is a chunk of decoded arrow
    /// rows, so building each feature's tags and ID (about as much work as running a simple profile on it) also moves
    /// to the idle threads instead of staying serial on the thread that reads a big row group, and one handoff covers
    /// many cheap features.
    ///
    /// The first `process` error stops reading and is returned; chunks handed to this thread after that are
    /// dropped (the pipeline is failing anyway).
    fn process_blocks(
        &self,
        blocks_in: impl IntoIterator<Item = Block>,
        distributor: Option<&Distributor<RowChunk>>,
        process: &mut dyn FnMut(ParquetFeature) -> Result<()>,
    ) -> Result<()> {
        // avoid contention trying to get the thread-local counters by getting them once when thread starts
        let (blocks, elements) = (self.blocks.counter_for_thread(), self.features.counter_for_thread());
        let error = RefCell::new(None);
        let mut consume = |chunk: RowChunk| {
            if error.borrow().is_some() {
                return;
            }
            let result = chunk.read(&mut |feature| {
                let result = process(feature);
                elements.inc();
                result
            });
            if let Err(e) = result {
                *error.borrow_mut() = Some(e);
            }
        };
        // stops reading after an error; the error itself stays in `error` (so later chunks are skipped) until the end
        let check = || {
            if error.borrow().is_some() { Err(anyhow::anyhow!("processing a parquet feature failed")) } else { Ok(()) }
        };
        let read_all = |accept: &mut dyn FnMut(RowChunk)| -> Result<()> {
            for block in blocks_in {
                self.read_block(&block, &blocks, &mut |chunk| {
                    accept(chunk);
                    check()
                })?;
            }
            Ok(())
        };
        let result = match distributor {
            None => read_all(&mut consume),
            Some(distributor) => {
                let mut consumer = distributor.for_thread(consume);
                let result = read_all(&mut |chunk| consumer.accept(chunk));
                // do work for other threads that are still processing blocks (or just discard it after an error)
                consumer.close();
                result
            }
        };
        match error.into_inner() {
            Some(e) => Err(e),
            None => result,
        }
    }
}

/// Reads features from geoparquet files.
pub struct ParquetReader {
    source_name: String,
    id_generator: Option<IdGenerator>,
    layer_generator: Option<LayerGenerator>,
    hive_partitioning: bool,
    load_balancing: bool,
    stats: StatsRef,
}

/// Returns `key=value` fields from the components of `path` (hive partitioning), or `None` if there are none.
pub fn get_hive_partition_fields(path: &Path) -> Option<Tags> {
    let mut fields = Tags::default();
    for part in path.iter() {
        let string = part.to_string_lossy();
        if string.contains('=') {
            let mut parts = string.split('=');
            if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
                fields.insert(k.to_string(), Value::str(v));
            }
        }
    }
    if fields.is_empty() { None } else { Some(fields) }
}

impl ParquetReader {
    pub fn new(source_name: &str, stats: StatsRef) -> ParquetReader {
        Self::with_options(source_name, stats, None, None, false)
    }

    /// `get_id` computes feature IDs from tags, `get_layer` computes the layer name from hive partition fields (when
    /// `hive_partitioning` is enabled).
    pub fn with_options(
        source_name: &str,
        stats: StatsRef,
        get_id: Option<IdGenerator>,
        get_layer: Option<LayerGenerator>,
        hive_partitioning: bool,
    ) -> ParquetReader {
        ParquetReader {
            source_name: source_name.to_string(),
            id_generator: get_id,
            layer_generator: get_layer,
            hive_partitioning,
            load_balancing: true,
            stats,
        }
    }

    /// Sets whether features from large blocks are handed off to processing threads that have run out of blocks of
    /// their own (the default, like Java). With `false`, each thread processes the features of the blocks it reads.
    pub fn with_load_balancing(mut self, load_balancing: bool) -> ParquetReader {
        self.load_balancing = load_balancing;
        self
    }

    fn new_distributor(&self) -> Option<Distributor<RowChunk>> {
        self.load_balancing.then(|| Distributor::create_with_capacity(DISTRIBUTOR_CAPACITY))
    }

    fn get_layer_name(&self, path: &Path) -> String {
        if self.hive_partitioning
            && let Some(generator) = &self.layer_generator
        {
            let fields = get_hive_partition_fields(path).unwrap_or_default();
            let layer = generator(&fields);
            if !layer.is_null() {
                return layer.to_string_lossy().into_owned();
            }
        }
        DEFAULT_LAYER.to_string()
    }

    /// Opens every input file (skipping `_SUCCESS` marker files), dropping ones that `cares_about` returns false for
    /// or that are outside `bounds`.
    pub fn open_files(
        &self,
        source_paths: &[PathBuf],
        bounds: &Bounds,
        cares_about: &dyn Fn(&PartialInput) -> bool,
    ) -> Result<Vec<ParquetInputFile>> {
        let mut result = vec![];
        for path in source_paths {
            if path.file_name().is_some_and(|n| n == "_SUCCESS") {
                continue;
            }
            let hive_partition_fields = if self.hive_partitioning { get_hive_partition_fields(path) } else { None };
            let layer = self.get_layer_name(path);
            let file = ParquetInputFile::with_options(
                &self.source_name,
                &layer,
                path,
                None,
                bounds,
                hive_partition_fields,
                self.id_generator.clone(),
            )?;
            if !file.should_skip(cares_about) {
                result.push(file);
            }
        }
        Ok(result)
    }

    /// Renders map features for every feature in `source_paths` within `config.bounds` based on the mapping logic
    /// defined in `profile`, writing them to `writer` (Java's `ParquetReader.process(sourcePaths, writer, config)`),
    /// then calls [`Profile::finish`].
    ///
    /// Files and row groups that `profile.cares_about` rules out are skipped without being read. Blocks (row groups)
    /// are processed in parallel on `config.feature_process_threads` threads; each [`ParquetFeature`] is passed to the
    /// profile directly as a [`SourceFeature`](crate::reader::source_feature::SourceFeature), so geometries are only
    /// decoded if the profile asks for them. Does nothing if `profile.cares_about_source(source_name)` is false.
    pub fn process(
        &self,
        source_paths: &[PathBuf],
        writer: &Arc<FeatureGroup>,
        config: &Arc<PlanetilerConfig>,
        profile: &Arc<dyn Profile>,
    ) -> Result<()> {
        if !profile.cares_about_source(&self.source_name) {
            info!("Skipping {} because the profile does not use it", self.source_name);
            return Ok(());
        }
        let cares_about = |input: &PartialInput| profile.cares_about(input);
        let stats = self.stats.clone();
        let distributor = self.new_distributor();
        self.run(source_paths, config, &cares_about, Some(writer), |builder, progress| {
            process_and_write_with_profile(builder, writer, config, profile, &stats, move |prev, process| {
                progress.process_blocks(prev, distributor.as_ref(), &mut |feature| process(&feature))
            })
        })?;
        // hook for profile to do any post-processing after this source is read
        finish_source(&self.source_name, writer, profile.as_ref(), config, &self.stats)
    }

    /// Reads every feature in `source_paths` within `config.bounds`, processing them on `config.feature_process_threads`
    /// threads with closures created by `processor_factory` (one per thread). Errors from a processing closure are
    /// logged and the feature skipped.
    pub fn process_with<P>(
        &self,
        source_paths: &[PathBuf],
        config: &PlanetilerConfig,
        cares_about: &dyn Fn(&PartialInput) -> bool,
        processor_factory: P,
    ) -> Result<()>
    where
        P: Fn() -> ParquetProcessFn + Send + Sync + 'static,
    {
        let process_threads = config.feature_process_threads.max(1);
        let distributor = self.new_distributor();
        self.run(source_paths, config, cares_about, None, |builder, progress| {
            builder.sink_to("process", process_threads, move |prev| {
                let mut process = processor_factory();
                progress.process_blocks(prev, distributor.as_ref(), &mut |feature| {
                    let id = feature.id();
                    if let Err(e) = process(feature) {
                        error!("Error processing parquet feature {id}: {e:#}");
                    }
                    Ok(())
                })
            })
        })
    }

    /// Opens the input files, starts the pipeline that `build` creates from the blocks of every file, and logs
    /// progress until it completes.
    fn run(
        &self,
        source_paths: &[PathBuf],
        config: &PlanetilerConfig,
        cares_about: &dyn Fn(&PartialInput) -> bool,
        writer: Option<&Arc<FeatureGroup>>,
        build: impl FnOnce(Builder<Block>, Progress) -> WorkerPipeline,
    ) -> Result<()> {
        let timer = self.stats.start_stage(&self.source_name);
        let start = std::time::Instant::now();
        let input_files = self.open_files(source_paths, &config.bounds, cares_about)?;
        // don't show % complete on features when a filter is present because to determine total # elements would
        // take an expensive initial query, and % complete on blocks gives a good enough proxy
        let feature_count: u64 = if input_files.iter().any(ParquetInputFile::has_filter) {
            0
        } else {
            input_files.iter().map(|f| f.count()).sum()
        };
        let block_count: usize = input_files.iter().map(ParquetInputFile::block_count).sum();
        let progress = Progress {
            blocks: Counter::new_multi_thread_counter(),
            features: Counter::new_multi_thread_counter(),
            working_on: Arc::new(Mutex::new(BTreeMap::new())),
        };
        let input_blocks: Vec<Block> = input_files.iter().flat_map(ParquetInputFile::get).collect();

        let builder =
            WorkerPipeline::start(&self.source_name, self.stats.clone()).read_from_tiny("blocks", input_blocks);
        let pipeline = build(builder, progress.clone());

        let (features, blocks, working) =
            (progress.features.clone(), progress.blocks.clone(), progress.working_on.clone());
        let mut loggers = ProgressLoggers::create()
            .add_rate_percent_counter("read", feature_count as i64, move || features.get(), true)
            .add_rate_percent_counter("blocks", block_count as i64, move || blocks.get(), false);
        if let Some(writer) = writer {
            let (written, file) = (writer.clone(), writer.clone());
            loggers = loggers
                .add_rate_counter("write", move || written.num_features_written() as i64)
                .add_file_size(file as Arc<dyn DiskBacked + Send + Sync>);
        }
        let loggers = loggers
            .new_line()
            .add(move || {
                let working = working.lock();
                let mut entries: Vec<_> = working.iter().filter(|(_, v)| **v > 0).collect();
                entries.sort_by(|a, b| b.1.cmp(a.1));
                entries.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(", ")
            })
            .new_line()
            .add_process_stats()
            .new_line()
            .add_pipeline_stats(&pipeline);
        pipeline.await_and_log(&loggers, config.log_interval)?;
        drop(loggers);
        drop(pipeline);

        let count = progress.features.get();
        let elapsed = start.elapsed().as_secs_f64().max(1e-9);
        info!(
            "Processed {} parquet features ({}/s, {} blocks, {} files) in {:.1}s",
            format::integer(count),
            format::numeric(count as f64 / elapsed, false),
            format::integer(progress.blocks.get()),
            format::integer(input_files.len() as i64),
            elapsed
        );
        timer.stop();
        Ok(())
    }

    /// Reads (and decodes the geometry of) every feature in `source_paths` within `bounds` using `threads` threads,
    /// passing each one to `sink` (which may be called concurrently from different threads).
    pub fn read_features(
        &self,
        source_paths: &[PathBuf],
        bounds: &Bounds,
        threads: usize,
        sink: &(dyn Fn(ParquetFeature) -> Result<()> + Sync),
    ) -> Result<()> {
        let files = self.open_files(source_paths, bounds, &|_| true)?;
        let blocks: Vec<Block> = files.iter().flat_map(ParquetInputFile::get).collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads.max(1).min(blocks.len().max(1)))
                .map(|_| {
                    scope.spawn(|| -> Result<()> {
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(block) = blocks.get(i) else { return Ok(()) };
                            block.read(&mut |f| sink(f))?;
                        }
                    })
                })
                .collect();
            for handle in handles {
                handle.join().map_err(|_| anyhow::anyhow!("parquet reader thread panicked"))??;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use geo::Centroid;

    use super::*;
    use crate::geo::geo_utils;
    use crate::reader::with_tags::WithTags;
    use crate::stats::stats::InMemoryStats;
    use crate::tags;
    use crate::util::parse::parse_double_or_null;

    fn bostons() -> Vec<PathBuf> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources/parquet");
        let mut result: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("boston"))
            .collect();
        result.sort();
        result
    }

    #[test]
    fn read_overture_parquet() {
        for path in bostons() {
            let ids = Arc::new(Mutex::new(vec![]));
            let geoms = Arc::new(Mutex::new(vec![]));
            let height = Arc::new(Mutex::new(0.0));
            let reader = ParquetReader::new("source", InMemoryStats::arc());
            let (i, g, h) = (ids.clone(), geoms.clone(), height.clone());
            reader
                .process_with(std::slice::from_ref(&path), &PlanetilerConfig::defaults(), &|_| true, move || {
                    let (i, g, h) = (i.clone(), g.clone(), h.clone());
                    Box::new(move |feature: ParquetFeature| {
                        i.lock().push(feature.get_string("id").unwrap());
                        *h.lock() += feature.get_tag("height").and_then(parse_double_or_null).unwrap_or(0.0);
                        g.lock().push(feature.lat_lon_geometry()?.clone());
                        Ok(())
                    })
                })
                .unwrap();
            let mut ids = ids.lock().clone();
            ids.sort();
            assert_eq!(
                vec![
                    "08b2a306638a0fff02001c5b97636c80",
                    "08b2a306638a0fff0200a75c80c3d54b",
                    "08b2a306638a0fff0200d1814977faca"
                ],
                ids,
                "{path:?}"
            );
            let center = geo_types::GeometryCollection(geoms.lock().clone()).centroid().unwrap();
            assert!((center.x() - -71.07448).abs() < 1e-5 && (center.y() - 42.35626).abs() < 1e-5, "{center:?}");
            assert!((*height.lock() - 4.7).abs() < 1e-9, "{path:?}");
        }
    }

    /// Java `ParquetReaderTest.testReadOvertureParquet`: runs a profile through [`ParquetReader::process`].
    #[test]
    fn read_overture_parquet_with_profile() {
        use crate::feature_collector::FeatureCollector;
        use crate::reader::source_feature::SourceFeature;
        use crate::reader::source_feature_processor::test_support::{config, run_with_group};

        #[derive(Default)]
        struct OvertureProfile {
            ids: Mutex<Vec<String>>,
            geoms: Mutex<Vec<geo_types::Geometry<f64>>>,
            height: Mutex<f64>,
            finished: Mutex<Vec<String>>,
        }
        impl Profile for OvertureProfile {
            fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
                assert_eq!(Some("source"), source_feature.source());
                assert_eq!(Some(DEFAULT_LAYER), source_feature.source_layer());
                assert!(source_feature.as_any_feature().downcast_ref::<ParquetFeature>().is_some());
                self.ids.lock().push(source_feature.get_string("id").unwrap());
                *self.height.lock() += source_feature.get_tag("height").and_then(parse_double_or_null).unwrap_or(0.0);
                self.geoms.lock().push(source_feature.lat_lon_geometry().unwrap().clone());
                features.centroid("buildings").set_zoom_range(0, 0);
            }

            fn finish(
                &self,
                source_name: &str,
                _factory: &crate::feature_collector::Factory,
                _next: &mut crate::profile::FeatureConsumer<'_>,
            ) {
                self.finished.lock().push(source_name.to_string());
            }
        }

        for (i, path) in bostons().into_iter().enumerate() {
            let profile = Arc::new(OvertureProfile::default());
            let output = run_with_group(config(i % 2 == 0), profile.clone(), |group, config, dyn_profile, stats| {
                ParquetReader::new("source", stats).process(std::slice::from_ref(&path), group, config, dyn_profile)
            });
            let mut ids = profile.ids.lock().clone();
            ids.sort();
            assert_eq!(
                vec![
                    "08b2a306638a0fff02001c5b97636c80",
                    "08b2a306638a0fff0200a75c80c3d54b",
                    "08b2a306638a0fff0200d1814977faca"
                ],
                ids,
                "{path:?}"
            );
            let center = geo_types::GeometryCollection(profile.geoms.lock().clone()).centroid().unwrap();
            assert!((center.x() - -71.07448).abs() < 1e-5 && (center.y() - 42.35626).abs() < 1e-5, "{center:?}");
            assert!((*profile.height.lock() - 4.7).abs() < 1e-9, "{path:?}");
            assert_eq!(3, output.len(), "{path:?}");
            assert_eq!(vec!["source"], *profile.finished.lock());
        }
    }

    /// `profile.cares_about` rules out the file so nothing gets read.
    #[test]
    fn process_skips_files_profile_does_not_care_about() {
        use crate::feature_collector::FeatureCollector;
        use crate::reader::source_feature::SourceFeature;
        use crate::reader::source_feature_processor::test_support::{config, run_with_group};

        struct IgnoreAll;
        impl Profile for IgnoreAll {
            fn process_feature(&self, _source_feature: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {
                panic!("should not be called");
            }

            fn cares_about(&self, input: &PartialInput) -> bool {
                !input.source.contains("source")
            }
        }
        let output = run_with_group(config(false), Arc::new(IgnoreAll), |group, config, profile, stats| {
            ParquetReader::new("source", stats).process(&bostons(), group, config, profile)
        });
        assert!(output.is_empty());
    }

    /// Writes a WKB GeoParquet file with one row group per entry of `row_groups` (its number of rows); row `i` of the
    /// file gets `id = first_id + i` and a small square polygon whose position depends on the id.
    fn write_geoparquet(path: &Path, first_id: i64, row_groups: &[usize]) {
        use arrow::array::{BinaryArray, Int64Array, StringArray};
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        use parquet::file::metadata::KeyValue;
        use parquet::file::properties::WriterProperties;

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("geometry", DataType::Binary, false),
        ]));
        let geo = r#"{"version": "1.0.0", "primary_column": "geometry", "columns": {"geometry": {"encoding": "WKB", "geometry_types": ["Polygon"]}}}"#;
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(vec![KeyValue::new("geo".to_string(), geo.to_string())]))
            .set_max_row_group_row_count(None)
            .build();
        let mut writer =
            ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema.clone(), Some(props)).unwrap();
        let mut id = first_id;
        for &rows in row_groups {
            let ids: Vec<i64> = (id..id + rows as i64).collect();
            id += rows as i64;
            let names: Vec<Option<String>> = ids.iter().map(|i| (i % 3 != 0).then(|| format!("name {i}"))).collect();
            let geoms: Vec<Vec<u8>> = ids
                .iter()
                .map(|i| {
                    let (x, y) = ((i % 97) as f64 * 0.5 - 24.0, (i % 89) as f64 * 0.4 - 17.0);
                    let ring = vec![(x, y), (x + 0.3, y), (x + 0.3, y + 0.3), (x, y + 0.3), (x, y)];
                    geo_utils::write_wkb(&geo_types::Polygon::new(ring.into(), vec![]).into()).unwrap()
                })
                .collect();
            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(Int64Array::from(ids)),
                    Arc::new(StringArray::from(names)),
                    Arc::new(BinaryArray::from_iter_values(geoms.iter())),
                ],
            )
            .unwrap();
            writer.write(&batch).unwrap();
            writer.flush().unwrap();
        }
        writer.close().unwrap();
    }

    /// One huge row group and many tiny ones: the output must not depend on whether features get handed off between
    /// threads, or on the number of threads; with load balancing, idle threads must take over part of the big block.
    #[test]
    fn uneven_row_groups_same_output_with_and_without_load_balancing() {
        use std::collections::HashSet;
        use std::thread::ThreadId;

        use crate::feature_collector::FeatureCollector;
        use crate::reader::source_feature::SourceFeature;
        use crate::reader::source_feature_processor::test_support::{config, run_with_group};

        // bigger than one 4,096-row arrow batch, so the big row group is decoded as more than one batch
        const BIG: usize = 5_000;
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.parquet"), dir.path().join("b.parquet"));
        let mut groups_a = vec![3, BIG];
        groups_a.extend(std::iter::repeat_n(5, 20));
        write_geoparquet(&a, 0, &groups_a);
        write_geoparquet(&b, 1_000_000, &[7; 40]);
        assert_eq!(22, ParquetInputFile::new("s", "l", &a).unwrap().block_count());

        /// Emits each feature's polygon (z2-4) and centroid (z0-1); features from the big row group are slow to
        /// process, and the threads that processed them are recorded.
        #[derive(Default)]
        struct SlowBigBlock {
            big_threads: Mutex<HashSet<ThreadId>>,
            /// `(id tag, feature id)` of every feature
            seen: Mutex<Vec<(i64, i64)>>,
        }
        impl Profile for SlowBigBlock {
            fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
                let id = source_feature.get_long("id");
                self.seen.lock().push((id, source_feature.id()));
                if (3..3 + BIG as i64).contains(&id) {
                    self.big_threads.lock().insert(std::thread::current().id());
                    let start = std::time::Instant::now();
                    while start.elapsed() < std::time::Duration::from_micros(40) {
                        std::hint::spin_loop();
                    }
                }
                features
                    .polygon("polygons")
                    .set_zoom_range(2, 4)
                    .set_min_pixel_size(0.0)
                    .set_attr("id", id)
                    .inherit_attr_from_source("name");
                features.centroid("points").set_zoom_range(0, 1).set_attr("id", id);
            }
        }

        let run = |threads: usize, write_threads: bool, load_balancing: bool| {
            let profile = Arc::new(SlowBigBlock::default());
            let mut config = config(write_threads);
            config.feature_process_threads = threads;
            let paths = vec![a.clone(), b.clone()];
            let mut output = run_with_group(config, profile.clone(), |group, config, dyn_profile, stats| {
                ParquetReader::new("source", stats).with_load_balancing(load_balancing).process(
                    &paths,
                    group,
                    config,
                    dyn_profile,
                )
            });
            output.sort_by_cached_key(|f| format!("{f:?}"));
            let mut seen = profile.seen.lock().clone();
            seen.sort_unstable();
            let big_threads = profile.big_threads.lock().len();
            (output, seen, big_threads)
        };

        // feature IDs (no ID generator) hash the file, row group index and 1-based row index within the row group
        let mut ids = vec![];
        for (path, first_id, groups) in [(&a, 0, &groups_a[..]), (&b, 1_000_000, &[7; 40][..])] {
            let file_hash = crate::util::hashing::fnv1a64(path.to_string_lossy().as_bytes());
            let mut id = first_id;
            for (index, &rows) in groups.iter().enumerate() {
                let block_hash = crate::util::hashing::fnv1a64_with_init(file_hash, &(index as i32).to_be_bytes());
                for row in 1..=rows as i64 {
                    ids.push((id, crate::util::hashing::fnv1a64_with_init(block_hash, &row.to_be_bytes()) as i64));
                    id += 1;
                }
            }
        }
        ids.sort_unstable();
        let (expected, expected_seen, _) = run(1, false, false);
        assert_eq!(ids, expected_seen);
        // every feature has a centroid at z0 and z1 and a polygon in at least one tile at z2-4
        assert!(expected.len() >= ids.len() * 5, "{}", expected.len());
        for (threads, write_threads, load_balancing) in
            [(1, false, true), (1, true, true), (6, false, false), (6, false, true), (6, true, true)]
        {
            let (output, seen, big_threads) = run(threads, write_threads, load_balancing);
            let case = format!("threads={threads} write_threads={write_threads} load_balancing={load_balancing}");
            assert_eq!(expected_seen, seen, "{case}");
            assert!(expected == output, "{case}: output differs");
            if load_balancing && threads > 1 {
                assert!(big_threads > 1, "{case}: big block was processed by {big_threads} thread(s)");
            } else if !load_balancing {
                assert_eq!(1, big_threads, "{case}");
            }
        }
    }

    /// The first error from processing a feature is returned, and nothing is processed after it.
    #[test]
    fn process_blocks_stops_at_first_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.parquet");
        write_geoparquet(&path, 0, &[300, 300, 300]);
        for load_balancing in [false, true] {
            let progress = Progress {
                blocks: Counter::new_multi_thread_counter(),
                features: Counter::new_multi_thread_counter(),
                working_on: Arc::new(Mutex::new(BTreeMap::new())),
            };
            let blocks = ParquetInputFile::new("source", "layer", &path).unwrap().get();
            let distributor = load_balancing.then(|| Distributor::create_with_capacity(DISTRIBUTOR_CAPACITY));
            let mut calls = 0;
            let result = progress.process_blocks(blocks, distributor.as_ref(), &mut |_| {
                calls += 1;
                if calls == 10 { Err(anyhow::anyhow!("boom")) } else { Ok(()) }
            });
            assert_eq!("boom", result.unwrap_err().to_string(), "load_balancing={load_balancing}");
            assert_eq!(10, calls, "load_balancing={load_balancing}");
        }
    }

    #[test]
    fn read_features_parallel() {
        let reader = ParquetReader::new("source", InMemoryStats::arc());
        let count = std::sync::atomic::AtomicUsize::new(0);
        reader
            .read_features(&bostons(), &Bounds::world(), 4, &|f| {
                f.lat_lon_geometry()?;
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        assert_eq!(3 * bostons().len(), count.into_inner());
    }

    #[test]
    fn hive_partition_fields() {
        assert_eq!(None, get_hive_partition_fields(Path::new("")));
        assert_eq!(None, get_hive_partition_fields(Path::new("a")));
        assert_eq!(None, get_hive_partition_fields(&Path::new("a").join("b")));
        assert_eq!(Some(tags! {"c" => "d"}), get_hive_partition_fields(&Path::new("a").join("b").join("c=d")));
        assert_eq!(
            Some(tags! {"c" => "d", "e" => "f"}),
            get_hive_partition_fields(&Path::new("a").join("b").join("c=d").join("e=f"))
        );
        assert_eq!(
            Some(tags! {"a" => "b", "c" => "d", "e" => "f"}),
            get_hive_partition_fields(&Path::new("a=b").join("b").join("c=d").join("e=f"))
        );
    }

    #[test]
    fn layer_from_hive_partitions() {
        let reader = ParquetReader::with_options(
            "source",
            InMemoryStats::arc(),
            None,
            Some(Arc::new(|tags: &Tags| tags.get("type").cloned().unwrap_or_default())),
            true,
        );
        assert_eq!("building", reader.get_layer_name(&Path::new("theme=buildings").join("type=building").join("x")));
        assert_eq!(DEFAULT_LAYER, reader.get_layer_name(Path::new("x")));
    }
}
