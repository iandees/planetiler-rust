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
//! TODO(port): Java hands features from each block to a `Distributor` so threads that finish early help with large
//! row groups; here each thread processes whole blocks.

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
use crate::reader::parquet::parquet_input_file::{Block, IdGenerator, ParquetInputFile};
use crate::reader::source_feature_processor::{finish_source, process_and_write_with_profile};
use crate::stats::counter::{Counter, ThreadCounter};
use crate::stats::progress_loggers::ProgressLoggers;
use crate::stats::stats::StatsRef;
use crate::util::file_utils::DiskBacked;
use crate::util::format;
use crate::value::{Tags, Value};
use crate::worker::worker_pipeline::{Builder, WorkerPipeline};

pub const DEFAULT_LAYER: &str = "features";

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
    /// Reads every feature in `block` into `f`, updating the counters.
    fn read_block(
        &self,
        block: &Block,
        blocks: &ThreadCounter,
        elements: &ThreadCounter,
        f: &mut dyn FnMut(ParquetFeature) -> Result<()>,
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
        block.read(&mut |feature| {
            f(feature)?;
            elements.inc();
            Ok(())
        })?;
        blocks.inc();
        if let Some(count) = self.working_on.lock().get_mut(layer) {
            *count -= 1;
        }
        Ok(())
    }
}

/// Reads features from geoparquet files.
pub struct ParquetReader {
    source_name: String,
    id_generator: Option<IdGenerator>,
    layer_generator: Option<LayerGenerator>,
    hive_partitioning: bool,
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
            stats,
        }
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
        self.run(source_paths, config, &cares_about, Some(writer), |builder, progress| {
            process_and_write_with_profile(builder, writer, config, profile, &stats, move |prev, process| {
                let (blocks, elements) = (progress.blocks.counter_for_thread(), progress.features.counter_for_thread());
                for block in prev {
                    progress.read_block(&block, &blocks, &elements, &mut |feature| process(&feature))?;
                }
                Ok(())
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
        self.run(source_paths, config, cares_about, None, |builder, progress| {
            builder.sink_to("process", process_threads, move |prev| {
                let (blocks, elements) = (progress.blocks.counter_for_thread(), progress.features.counter_for_thread());
                let mut process = processor_factory();
                for block in prev {
                    progress.read_block(&block, &blocks, &elements, &mut |feature| {
                        let id = feature.id();
                        if let Err(e) = process(feature) {
                            error!("Error processing parquet feature {id}: {e:#}");
                        }
                        Ok(())
                    })?;
                }
                Ok(())
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
