//! Port of `reader/SourceFeatureProcessor.java`.
//!
//! Coordinates reading and processing of one or more paths that are grouped under a single source name. Paths are read
//! in parallel by [`SimpleReader`]s constructed by a reader factory (`feature_read_threads` threads), and features are
//! handed to `feature_process_threads` processing threads.
//!
//! [`process_files`] is Java's static `processFiles(sourceName, paths, readerFactory, writer, config, profile,
//! stats)`: each processing thread moves every [`ReaderFeature`] into a [`SimpleFeature`], runs
//! [`Profile::process_feature`] and renders the result into the [`FeatureGroup`] (inline or through write threads,
//! see [`feature_group::process_and_write`]), then [`Profile::finish`] is called for the source.
//! [`SourceFeatureProcessor::process_files_with`] is the lower-level variant that hands each feature to a
//! caller-provided per-thread closure instead.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::{error, info};

use crate::collection::feature_group::{self, FeatureGroup, FeatureSink, RenderedFeatureEncoder};
use crate::config::planetiler_config::PlanetilerConfig;
use crate::feature_collector;
use crate::geo::envelope::Envelope;
use crate::profile::Profile;
#[cfg(doc)]
use crate::reader::simple_feature::SimpleFeature;
use crate::reader::simple_reader::{ReaderFeature, SimpleReader};
use crate::reader::source_feature::SourceFeature;
use crate::render::feature_renderer::FeatureRenderer;
use crate::stats::counter::Counter;
use crate::stats::progress_loggers::ProgressLoggers;
use crate::stats::stats::StatsRef;
use crate::util::file_utils::DiskBacked;
use crate::worker::work_queue::WorkQueueReader;
use crate::worker::worker_pipeline::{Builder, WorkerPipeline};

/// Process one feature (called from a processing thread).
pub type ProcessFn = Box<dyn FnMut(ReaderFeature) -> anyhow::Result<()> + Send>;

/// Callback a processing thread uses to run the profile on one source feature and write the result.
pub type ProcessSourceFeature<'a> = dyn FnMut(&dyn SourceFeature) -> anyhow::Result<()> + 'a;

/// Per-processing-thread state for running a profile on source features and writing the rendered output to a
/// [`FeatureGroup`] sink: a [`feature_collector::Factory`], this thread's encoder and a shared renderer.
pub(crate) struct ProfileProcessor {
    profile: Arc<dyn Profile>,
    factory: feature_collector::Factory,
    renderer: Arc<FeatureRenderer>,
    encoder: RenderedFeatureEncoder,
}

impl ProfileProcessor {
    pub(crate) fn new(
        profile: Arc<dyn Profile>,
        group: &FeatureGroup,
        renderer: Arc<FeatureRenderer>,
        config: Arc<PlanetilerConfig>,
        stats: StatsRef,
    ) -> ProfileProcessor {
        ProfileProcessor {
            profile,
            factory: feature_collector::Factory::new(config, stats),
            renderer,
            encoder: group.new_rendered_feature_encoder(),
        }
    }

    /// Runs the profile on `feature` and writes every rendered tile feature to `sink`.
    ///
    /// Like Java (which catches exceptions and logs `Error processing <feature>`), a panic inside the profile is
    /// logged and the feature skipped; errors writing to `sink` are returned.
    pub(crate) fn process(&mut self, feature: &dyn SourceFeature, sink: &mut dyn FeatureSink) -> anyhow::Result<()> {
        let ProfileProcessor { profile, factory, renderer, encoder } = self;
        let mut collector = factory.get(feature);
        let processed = std::panic::catch_unwind(AssertUnwindSafe(|| profile.process_feature(feature, &mut collector)));
        if let Err(panic) = processed {
            error!("Error processing {feature:?}: {}", panic_message(panic.as_ref()));
            return Ok(());
        }
        let mut result = Ok(());
        for renderable in collector.iter() {
            renderer.accept(renderable, &mut |rendered| {
                if result.is_ok() {
                    result = sink.write(encoder, &rendered);
                }
            });
            if result.is_err() {
                break;
            }
        }
        result
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_string()
    }
}

/// Hook for the profile to do any post-processing after source `source_name` is read: calls [`Profile::finish`]
/// and writes the features it emits to `group` from the calling thread.
pub(crate) fn finish_source(
    source_name: &str,
    group: &FeatureGroup,
    profile: &dyn Profile,
    config: &Arc<PlanetilerConfig>,
    stats: &StatsRef,
) -> anyhow::Result<()> {
    let renderer = FeatureRenderer::new(config.clone(), stats.clone());
    let mut writer = group.feature_writer()?;
    let factory = feature_collector::Factory::new(config.clone(), stats.clone());
    let mut result = Ok(());
    profile.finish(source_name, &factory, &mut |feature| {
        renderer.accept(feature, &mut |rendered| {
            if result.is_ok() {
                result = writer.write(&rendered);
            }
        })
    });
    result?;
    writer.close()
}

/// Wires `builder` (which emits source features) into processing threads that run `profile` on each feature and
/// write to `group`: Java's `processAndWrite(config.parallelTempIO(), processThreads, writeThreads, writer, ...)`
/// with a `FeatureCollector.Factory` and `FeatureRenderer` per thread.
///
/// `for_each` is invoked once per processing thread with the input queue and a callback that processes one source
/// feature; it is how the caller turns its queue items into `&dyn SourceFeature`s.
pub(crate) fn process_and_write_with_profile<O, F>(
    builder: Builder<O>,
    group: &Arc<FeatureGroup>,
    config: &Arc<PlanetilerConfig>,
    profile: &Arc<dyn Profile>,
    stats: &StatsRef,
    for_each: F,
) -> WorkerPipeline
where
    O: Send + 'static,
    F: Fn(&mut WorkQueueReader<O>, &mut ProcessSourceFeature<'_>) -> anyhow::Result<()> + Send + Sync + 'static,
{
    let renderer = Arc::new(FeatureRenderer::new(config.clone(), stats.clone()));
    let (handler_group, handler_config, profile, stats) =
        (group.clone(), config.clone(), profile.clone(), stats.clone());
    feature_group::process_and_write(
        builder,
        config.parallel_temp_io,
        config.feature_process_threads.max(1),
        config.feature_write_threads.max(1),
        group.clone(),
        move |prev, sink| {
            let mut processor = ProfileProcessor::new(
                profile.clone(),
                &handler_group,
                renderer.clone(),
                handler_config.clone(),
                stats.clone(),
            );
            for_each(prev, &mut |feature| processor.process(feature, sink))
        },
    )
}

/// Coordinates reading features from `source_paths` with readers created by `reader_factory`, and processing them
/// with a profile ([`SourceFeatureProcessor::process_files`]) or per-thread closures
/// ([`SourceFeatureProcessor::process_files_with`]).
pub struct SourceFeatureProcessor<R, F>
where
    R: SimpleReader + 'static,
    F: Fn(&Path) -> anyhow::Result<R> + Send + Sync + 'static,
{
    source_name: Arc<str>,
    reader_factory: Arc<F>,
    stats: StatsRef,
}

impl<R, F> SourceFeatureProcessor<R, F>
where
    R: SimpleReader + 'static,
    F: Fn(&Path) -> anyhow::Result<R> + Send + Sync + 'static,
{
    pub fn new(source_name: &str, reader_factory: F, stats: StatsRef) -> Self {
        SourceFeatureProcessor { source_name: source_name.into(), reader_factory: Arc::new(reader_factory), stats }
    }

    /// Returns the total number of features in all `source_paths`.
    pub fn get_feature_count(&self, source_paths: &[PathBuf]) -> anyhow::Result<u64> {
        Ok(self.open_readers(source_paths)?.1)
    }

    /// Opens a reader for every path and returns them with the total number of features.
    ///
    /// Java opens one reader per path to count features and another one to read them. Here the readers used to count
    /// are kept and handed to the read pipeline, so expensive setup (extracting a zipped sqlite file, inflating a
    /// zipped shapefile to filter its records by bounds) happens once per run instead of twice.
    fn open_readers(&self, source_paths: &[PathBuf]) -> anyhow::Result<(Vec<R>, u64)> {
        let mut count = 0;
        let mut readers = Vec::with_capacity(source_paths.len());
        for path in source_paths {
            let reader = (self.reader_factory)(path)?;
            count += reader.feature_count()?;
            readers.push(reader);
        }
        Ok((readers, count))
    }

    /// Starts a pipeline that reads every feature from `readers` on `feature_read_threads` threads into a
    /// `process_queue`. Readers may skip features outside `config.bounds` without decoding them
    /// ([`SimpleReader::read_features_within`]); those are added to `skipped`.
    fn read_pipeline(
        &self,
        readers: Vec<R>,
        config: &PlanetilerConfig,
        skipped: Arc<AtomicU64>,
    ) -> Builder<ReaderFeature> {
        let bounds = config.bounds.lat_lon();
        WorkerPipeline::start(&self.source_name, self.stats.clone())
            .read_from_tiny("source_paths", readers)
            .add_worker("read", config.feature_read_threads.max(1), move |readers, next| {
                let skipped = |n: u64| {
                    skipped.fetch_add(n, Ordering::Relaxed);
                };
                for reader in readers {
                    reader.read_features_within(&bounds, &skipped, &mut |feature| {
                        next.accept(feature);
                        Ok(())
                    })?;
                }
                Ok(())
            })
            .add_buffer("process_queue", 1000)
    }

    /// Renders map features for all elements in `source_paths` based on the mapping logic defined in `profile`,
    /// writing them to `writer` (Java's `processFiles(sourcePaths, writer, config)`), then calls
    /// [`Profile::finish`].
    ///
    /// Features whose lat/lon envelope does not intersect `config.bounds` are skipped. Profile panics are logged and
    /// the feature skipped; errors reading inputs or writing features fail the whole pipeline. Does nothing if
    /// `profile.cares_about_source(source_name)` is false.
    pub fn process_files(
        &self,
        source_paths: Vec<PathBuf>,
        writer: &Arc<FeatureGroup>,
        config: &Arc<PlanetilerConfig>,
        profile: &Arc<dyn Profile>,
    ) -> anyhow::Result<()> {
        if !profile.cares_about_source(&self.source_name) {
            info!("Skipping {} because the profile does not use it", self.source_name);
            return Ok(());
        }
        let timer = self.stats.start_stage(&self.source_name);
        let (readers, feature_count) = self.open_readers(&source_paths)?;
        let lat_lon_bounds: Envelope = config.bounds.lat_lon();
        let features_read = Counter::new_multi_thread_counter();

        let skipped = Arc::new(AtomicU64::new(0));
        let (counter, source) = (features_read.clone(), self.source_name.clone());
        let builder = self.read_pipeline(readers, config, skipped.clone());
        let pipeline =
            process_and_write_with_profile(builder, writer, config, profile, &self.stats, move |prev, process| {
                let counter = counter.counter_for_thread();
                for feature in prev {
                    counter.inc();
                    if Envelope::of_geometry(&feature.geometry).intersects(&lat_lon_bounds) {
                        process(&feature.into_simple_feature(&source))?;
                    }
                }
                Ok(())
            });

        let (counter, written, file) = (features_read.clone(), writer.clone(), writer.clone());
        let loggers = ProgressLoggers::create()
            .add_rate_percent_counter(
                "read",
                feature_count as i64,
                move || counter.get() + skipped.load(Ordering::Relaxed) as i64,
                true,
            )
            .add_rate_counter("write", move || written.num_features_written() as i64)
            .add_file_size(file as Arc<dyn DiskBacked + Send + Sync>)
            .new_line()
            .add_process_stats()
            .new_line()
            .add_pipeline_stats(&pipeline);
        pipeline.await_and_log(&loggers, config.log_interval)?;
        drop(pipeline);

        // hook for profile to do any post-processing after this source is read
        finish_source(&self.source_name, writer, profile.as_ref(), config, &self.stats)?;
        timer.stop();
        Ok(())
    }

    /// Reads every feature from `source_paths` and passes the ones whose envelope intersects the configured bounds to
    /// a processing closure. `processor_factory` is invoked once per processing thread.
    ///
    /// Errors returned by a processing closure are logged and the feature is skipped (like Java, which logs
    /// `Error processing <feature>` and continues). Errors reading inputs fail the whole pipeline.
    pub fn process_files_with<P>(
        &self,
        source_paths: Vec<PathBuf>,
        config: &PlanetilerConfig,
        processor_factory: P,
    ) -> anyhow::Result<()>
    where
        P: Fn() -> ProcessFn + Send + Sync + 'static,
    {
        let timer = self.stats.start_stage(&self.source_name);
        let (readers, feature_count) = self.open_readers(&source_paths)?;
        let process_threads = config.feature_process_threads.max(1);
        let lat_lon_bounds: Envelope = config.bounds.lat_lon();
        let features_read = Counter::new_multi_thread_counter();

        let skipped = Arc::new(AtomicU64::new(0));
        let counter = features_read.clone();
        let pipeline =
            self.read_pipeline(readers, config, skipped.clone()).sink_to("process", process_threads, move |prev| {
                let counter = counter.counter_for_thread();
                let mut process = processor_factory();
                for feature in prev {
                    counter.inc();
                    if Envelope::of_geometry(&feature.geometry).intersects(&lat_lon_bounds) {
                        let id = feature.id;
                        let layer = feature.source_layer.clone();
                        if let Err(e) = process(feature) {
                            error!("Error processing feature {id} (layer {layer:?}): {e:#}");
                        }
                    }
                }
                Ok(())
            });

        let counter = features_read.clone();
        let loggers = ProgressLoggers::create()
            .add_rate_percent_counter(
                "read",
                feature_count as i64,
                move || counter.get() + skipped.load(Ordering::Relaxed) as i64,
                true,
            )
            .new_line()
            .add_process_stats()
            .new_line()
            .add_pipeline_stats(&pipeline);
        pipeline.await_and_log(&loggers, config.log_interval)?;
        timer.stop();
        Ok(())
    }
}

/// Renders map features for all elements in `source_paths` read with readers from `reader_factory`, based on the
/// mapping logic defined in `profile`, into `writer` (Java's static `SourceFeatureProcessor.processFiles`).
///
/// `writer` must be shared (`Arc`) because processing threads write to it; once every source has been processed,
/// reclaim it with `Arc::into_inner` / `Arc::get_mut` and call [`FeatureGroup::prepare`].
pub fn process_files<R, F>(
    source_name: &str,
    source_paths: Vec<PathBuf>,
    reader_factory: F,
    writer: &Arc<FeatureGroup>,
    config: &Arc<PlanetilerConfig>,
    profile: &Arc<dyn Profile>,
    stats: StatsRef,
) -> anyhow::Result<()>
where
    R: SimpleReader + 'static,
    F: Fn(&Path) -> anyhow::Result<R> + Send + Sync + 'static,
{
    SourceFeatureProcessor::new(source_name, reader_factory, stats).process_files(source_paths, writer, config, profile)
}

/// Convenience wrapper around [`SourceFeatureProcessor::process_files_with`].
pub fn process_files_with<R, F, P>(
    source_name: &str,
    source_paths: Vec<PathBuf>,
    reader_factory: F,
    config: &PlanetilerConfig,
    stats: StatsRef,
    processor_factory: P,
) -> anyhow::Result<()>
where
    R: SimpleReader + 'static,
    F: Fn(&Path) -> anyhow::Result<R> + Send + Sync + 'static,
    P: Fn() -> ProcessFn + Send + Sync + 'static,
{
    SourceFeatureProcessor::new(source_name, reader_factory, stats).process_files_with(
        source_paths,
        config,
        processor_factory,
    )
}

/// Helpers for reader-level end-to-end tests: run a reader entry point against an in-memory [`FeatureGroup`] and
/// decode what was written.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::geo::tile_coord::TileCoord;
    use crate::geo::tile_order::TileOrder;
    use crate::stats::stats::InMemoryStats;
    use crate::value::Tags;
    use crate::vector_tile;

    /// One decoded output feature: tile, layer, attributes.
    pub(crate) type Output = (TileCoord, String, Tags);

    /// Runs `f` with a fresh in-memory feature group, then sorts the group and returns every feature written.
    pub(crate) fn run_with_group(
        config: PlanetilerConfig,
        profile: Arc<dyn Profile>,
        f: impl FnOnce(&Arc<FeatureGroup>, &Arc<PlanetilerConfig>, &Arc<dyn Profile>, StatsRef) -> anyhow::Result<()>,
    ) -> Vec<Output> {
        let config = Arc::new(config);
        let stats = InMemoryStats::arc();
        let group =
            Arc::new(FeatureGroup::new_in_memory(TileOrder::Tms, profile.clone(), config.clone(), stats.clone()));
        f(&group, &config, &profile, stats).unwrap();
        let mut group = Arc::into_inner(group).expect("feature group still shared");
        group.prepare().unwrap();
        let mut result = vec![];
        for tile in group.iter().unwrap() {
            for feature in vector_tile::decode(&tile.get_vector_tile().encode()).unwrap() {
                result.push((tile.tile_coord(), feature.layer.clone(), feature.tags.clone()));
            }
        }
        result
    }

    /// Records `(source, source layer, id)` of every feature it sees and the sources it was finished for, and emits
    /// the centroid of every feature at z0 into a layer named after its source layer.
    #[derive(Default)]
    pub(crate) struct CollectingProfile {
        pub(crate) seen: parking_lot::Mutex<Vec<(String, String, i64)>>,
        pub(crate) finished: parking_lot::Mutex<Vec<String>>,
    }

    impl CollectingProfile {
        /// Asserts that every feature came from `source`, and the number of features seen per layer.
        pub(crate) fn assert_saw(&self, source: &str, layer_counts: &[(&str, usize)]) {
            let seen = self.seen.lock();
            assert!(seen.iter().all(|(s, _, _)| s == source), "{seen:?}");
            let mut counts = std::collections::BTreeMap::<&str, usize>::new();
            for (_, layer, _) in seen.iter() {
                *counts.entry(layer.as_str()).or_default() += 1;
            }
            let expected: std::collections::BTreeMap<&str, usize> = layer_counts.iter().copied().collect();
            assert_eq!(expected, counts);
        }
    }

    impl Profile for CollectingProfile {
        fn process_feature(
            &self,
            source_feature: &dyn SourceFeature,
            features: &mut crate::feature_collector::FeatureCollector<'_>,
        ) {
            let layer = source_feature.source_layer().unwrap_or_default().to_string();
            self.seen.lock().push((
                source_feature.source().unwrap_or_default().to_string(),
                layer.clone(),
                source_feature.id(),
            ));
            features.centroid(&layer).set_zoom_range(0, 0).set_attr("id", source_feature.id());
        }

        fn finish(
            &self,
            source_name: &str,
            _factory: &feature_collector::Factory,
            _next: &mut crate::profile::FeatureConsumer<'_>,
        ) {
            self.finished.lock().push(source_name.to_string());
        }
    }

    /// A config that runs with a few threads, writing through write threads (`parallel_temp_io = false`) when
    /// `write_threads` is true.
    pub(crate) fn config(write_threads: bool) -> PlanetilerConfig {
        let mut config = PlanetilerConfig::defaults();
        config.feature_read_threads = 2;
        config.feature_process_threads = 2;
        config.feature_write_threads = 1;
        config.parallel_temp_io = !write_threads;
        config
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::test_support::*;
    use super::*;
    use crate::config::bounds::Bounds;
    use crate::expression::expression::ExpressionInput;
    use crate::feature_collector::FeatureCollector;
    use crate::geo::tile_coord::TileCoord;
    use crate::profile::{FeatureConsumer, NullProfile};
    use crate::stats::stats::InMemoryStats;
    use crate::tags;
    use crate::value::{Tags, Value};

    struct TestReader {
        n: i64,
        x: f64,
    }

    impl SimpleReader for TestReader {
        fn source_name(&self) -> &str {
            "test"
        }

        fn feature_count(&self) -> anyhow::Result<u64> {
            Ok(self.n as u64)
        }

        fn read_features(&self, next: &mut crate::reader::simple_reader::FeatureSink<'_>) -> anyhow::Result<()> {
            let layer: Option<Arc<str>> = Some(if self.x == 0.0 { "a" } else { "b" }.into());
            for i in 0..self.n {
                next(ReaderFeature::new(
                    geo_types::Point::new(self.x, i as f64).into(),
                    tags! {"i" => i},
                    layer.clone(),
                    i,
                ))?;
            }
            Ok(())
        }
    }

    fn test_reader(path: &Path) -> anyhow::Result<TestReader> {
        let x = if path.ends_with("a") { 0.0 } else { 50.0 };
        Ok(TestReader { n: 10, x })
    }

    #[test]
    fn processes_all_features_in_bounds() {
        let mut config = PlanetilerConfig::defaults();
        config.bounds = Bounds::new(Some(Envelope::new(-10.0, 10.0, -90.0, 90.0)));
        let processor = SourceFeatureProcessor::new("test", test_reader, InMemoryStats::arc());
        assert_eq!(20, processor.get_feature_count(&[PathBuf::from("a"), PathBuf::from("b")]).unwrap());
        let result = Arc::new(Mutex::new(Vec::new()));
        let r = result.clone();
        processor
            .process_files_with(vec![PathBuf::from("a"), PathBuf::from("b")], &config, move || {
                let r = r.clone();
                Box::new(move |f| {
                    r.lock().push(f.id);
                    Ok(())
                })
            })
            .unwrap();
        let mut ids = result.lock().clone();
        ids.sort();
        assert_eq!((0..10).collect::<Vec<_>>(), ids);
    }

    /// Java's `SourceFeatureProcessorTest.MockReader`: emits one empty point per path with the path as layer.
    struct MockReader {
        path: PathBuf,
        emitted: Arc<Mutex<Vec<String>>>,
    }

    impl SimpleReader for MockReader {
        fn source_name(&self) -> &str {
            "sourceName"
        }

        fn feature_count(&self) -> anyhow::Result<u64> {
            Ok(self.path.to_string_lossy().len() as u64)
        }

        fn read_features(&self, next: &mut crate::reader::simple_reader::FeatureSink<'_>) -> anyhow::Result<()> {
            let layer = self.path.to_string_lossy().into_owned();
            let empty = geo_types::Geometry::MultiPoint(geo_types::MultiPoint(vec![]));
            next(ReaderFeature::new(empty, Tags::new(), Some(layer.as_str().into()), 0))?;
            self.emitted.lock().push(layer);
            Ok(())
        }
    }

    #[test]
    fn test_count_features() {
        let emitted = Arc::new(Mutex::new(vec![]));
        let processor = SourceFeatureProcessor::new(
            "sourceName",
            move |path: &Path| Ok(MockReader { path: path.to_path_buf(), emitted: emitted.clone() }),
            InMemoryStats::arc(),
        );
        assert_eq!(6, processor.get_feature_count(&[PathBuf::from("1"), PathBuf::from("22"), "333".into()]).unwrap());
    }

    #[test]
    fn test_process_multiple_inputs() {
        let emitted = Arc::new(Mutex::new(vec![]));
        let e = emitted.clone();
        let output =
            run_with_group(PlanetilerConfig::defaults(), Arc::new(NullProfile), |group, config, profile, stats| {
                process_files(
                    "sourceName",
                    vec!["a".into(), "b".into(), "c".into()],
                    move |path: &Path| Ok(MockReader { path: path.to_path_buf(), emitted: e.clone() }),
                    group,
                    config,
                    profile,
                    stats,
                )
            });
        assert!(output.is_empty());
        let mut emitted = emitted.lock().clone();
        emitted.sort();
        assert_eq!(vec!["a", "b", "c"], emitted);
    }

    /// Emits a z0 point for every feature with its source layer and id, panics on id 3, and emits one extra feature
    /// from `finish`.
    #[derive(Default)]
    struct RecordingProfile {
        finished: Mutex<Vec<String>>,
        cares_about_source: bool,
    }

    impl Profile for RecordingProfile {
        fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
            assert_eq!(Some("test"), source_feature.source());
            if source_feature.id() == 3 {
                panic!("boom");
            }
            features
                .point("points")
                .set_zoom_range(0, 0)
                .set_attr("layer", source_feature.source_layer().unwrap().to_string())
                .set_attr("id", source_feature.id());
        }

        fn finish(&self, source_name: &str, factory: &feature_collector::Factory, next: &mut FeatureConsumer<'_>) {
            self.finished.lock().push(source_name.to_string());
            let feature = crate::reader::simple_feature::SimpleFeature::create(
                geo_types::Point::new(0.0, 0.0),
                Tags::new(),
                source_name,
                "finish",
                -1,
            );
            let mut collector = factory.get(&feature);
            collector.point("finish").set_zoom_range(0, 0).set_attr("source", source_name.to_string());
            for f in collector.iter() {
                next(f);
            }
        }

        fn cares_about_source(&self, _name: &str) -> bool {
            self.cares_about_source
        }
    }

    fn run_recording(write_threads: bool) -> (Arc<RecordingProfile>, Vec<Output>) {
        let profile = Arc::new(RecordingProfile { cares_about_source: true, ..Default::default() });
        let mut config = config(write_threads);
        config.bounds = Bounds::new(Some(Envelope::new(-10.0, 10.0, -90.0, 90.0)));
        let output = run_with_group(config, profile.clone(), |group, config, profile, stats| {
            process_files("test", vec!["a".into(), "b".into()], test_reader, group, config, profile, stats)
        });
        (profile, output)
    }

    #[test]
    fn test_runs_profile_and_writes_features() {
        for write_threads in [false, true] {
            let (profile, output) = run_recording(write_threads);
            assert_eq!(vec!["test"], *profile.finished.lock());
            let z0 = TileCoord::of_xyz(0, 0, 0);
            let mut points: Vec<i64> = output
                .iter()
                .filter(|(_, layer, _)| layer == "points")
                .map(|(tile, _, tags)| {
                    assert_eq!(z0, *tile);
                    // only features from "a" are in bounds
                    assert_eq!(Some(&Value::str("a")), tags.get("layer"));
                    match tags.get("id") {
                        Some(Value::Int(id)) => *id,
                        other => panic!("{other:?}"),
                    }
                })
                .collect();
            points.sort();
            // id 3 panicked in the profile and was skipped
            assert_eq!(vec![0, 1, 2, 4, 5, 6, 7, 8, 9], points);
            let finished: Vec<_> = output.iter().filter(|(_, layer, _)| layer == "finish").collect();
            assert_eq!(1, finished.len());
            assert_eq!(Some(&Value::str("test")), finished[0].2.get("source"));
        }
    }

    #[test]
    fn test_skips_sources_profile_does_not_care_about() {
        let profile = Arc::new(RecordingProfile::default());
        let output = run_with_group(config(false), profile.clone(), |group, config, profile, stats| {
            process_files("test", vec!["a".into()], test_reader, group, config, profile, stats)
        });
        assert!(output.is_empty());
        assert!(profile.finished.lock().is_empty());
    }

    #[test]
    fn test_reader_feature_into_simple_feature() {
        let source: Arc<str> = "source".into();
        let layer: Arc<str> = "layer".into();
        let feature = ReaderFeature::new(geo_types::Point::new(1.0, 2.0).into(), tags! {"a" => 1}, Some(layer), 7)
            .into_simple_feature(&source);
        assert_eq!(Some("source"), feature.source());
        assert_eq!(Some("layer"), feature.source_layer());
        assert_eq!(7, feature.id());
        assert!(crate::reader::with_geometry_type::WithGeometryType::is_point(&feature));
        assert_eq!(1, crate::reader::with_tags::WithTags::tags(&feature).len());
        // the source name is shared, not copied
        assert!(std::ptr::eq(source.as_ptr(), feature.source().unwrap().as_ptr()));
    }
}
