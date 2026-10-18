//! Port of `reader/SourceFeatureProcessor.java`.
//!
//! Coordinates reading and processing of one or more paths that are grouped under a single source name. Paths are read
//! in parallel by [`SimpleReader`]s constructed by a reader factory (`feature_read_threads` threads), and features are
//! handed to `feature_process_threads` processing threads.
//!
//! In Java the processing step runs `profile.processFeature(sourceFeature, featureCollector)` and renders the result
//! into a `FeatureGroup`. Here the processing step is a caller-provided closure (one instance per processing thread,
//! so it can own thread-local state like a feature renderer and writer).
//!
//! TODO(port): once `Profile`, `FeatureCollector`, `FeatureRenderer` and `FeatureGroup` are ported, add the Java
//! `processFiles(sourceName, paths, readerFactory, writer, config, profile, stats)` entry point on top of
//! [`process_files`]: build a per-thread closure that converts each [`ReaderFeature`] into a `SimpleFeature`, runs
//! `profile.process_feature` and renders into `writer.writer_for_thread()` (Java's `processAndWrite`), add the `write`
//! rate counter / file size to the progress loggers, and call `profile.finish(source_name, ...)` after the pipeline
//! completes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use tracing::error;

use crate::config::planetiler_config::PlanetilerConfig;
use crate::geo::envelope::Envelope;
use crate::reader::simple_reader::{ReaderFeature, SimpleReader};
use crate::stats::progress_loggers::ProgressLoggers;
use crate::stats::stats::StatsRef;
use crate::worker::worker_pipeline::WorkerPipeline;

/// Process one feature (called from a processing thread).
pub type ProcessFn = Box<dyn FnMut(ReaderFeature) -> anyhow::Result<()> + Send>;

/// Coordinates reading features from `source_paths` with readers created by `reader_factory`, and processing them with
/// per-thread closures created by `processor_factory`.
pub struct SourceFeatureProcessor<R, F>
where
    R: SimpleReader + 'static,
    F: Fn(&Path) -> anyhow::Result<R> + Send + Sync + 'static,
{
    source_name: String,
    reader_factory: Arc<F>,
    stats: StatsRef,
}

impl<R, F> SourceFeatureProcessor<R, F>
where
    R: SimpleReader + 'static,
    F: Fn(&Path) -> anyhow::Result<R> + Send + Sync + 'static,
{
    pub fn new(source_name: &str, reader_factory: F, stats: StatsRef) -> Self {
        SourceFeatureProcessor { source_name: source_name.to_string(), reader_factory: Arc::new(reader_factory), stats }
    }

    /// Returns the total number of features in all `source_paths`.
    pub fn get_feature_count(&self, source_paths: &[PathBuf]) -> anyhow::Result<u64> {
        let mut count = 0;
        for path in source_paths {
            count += (self.reader_factory)(path)?.feature_count()?;
        }
        Ok(count)
    }

    /// Reads every feature from `source_paths` and passes the ones whose envelope intersects the configured bounds to
    /// a processing closure. `processor_factory` is invoked once per processing thread.
    ///
    /// Errors returned by a processing closure are logged and the feature is skipped (like Java, which logs
    /// `Error processing <feature>` and continues). Errors reading inputs fail the whole pipeline.
    pub fn process_files<P>(
        &self,
        source_paths: Vec<PathBuf>,
        config: &PlanetilerConfig,
        processor_factory: P,
    ) -> anyhow::Result<()>
    where
        P: Fn() -> ProcessFn + Send + Sync + 'static,
    {
        let timer = self.stats.start_stage(&self.source_name);
        let feature_count = self.get_feature_count(&source_paths)?;
        let read_threads = config.feature_read_threads.max(1);
        let process_threads = config.feature_process_threads.max(1);
        let lat_lon_bounds: Envelope = config.bounds.lat_lon();
        let features_read = Arc::new(AtomicI64::new(0));

        let factory = self.reader_factory.clone();
        let counter = features_read.clone();
        let pipeline = WorkerPipeline::start(&self.source_name, self.stats.clone())
            .read_from_tiny("source_paths", source_paths)
            .add_worker("read", read_threads, move |paths, next| {
                for path in paths {
                    let reader = factory(&path)?;
                    reader.read_features(&mut |feature| {
                        next.accept(feature);
                        Ok(())
                    })?;
                }
                Ok(())
            })
            .add_buffer("process_queue", 1000)
            .sink_to("process", process_threads, move |prev| {
                let mut process = processor_factory();
                for feature in prev {
                    counter.fetch_add(1, Ordering::Relaxed);
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
            .add_rate_percent_counter("read", feature_count as i64, move || counter.load(Ordering::Relaxed), true)
            .new_line()
            .add_process_stats()
            .new_line()
            .add_pipeline_stats(&pipeline);
        pipeline.await_and_log(&loggers, config.log_interval)?;
        timer.stop();
        Ok(())
    }
}

/// Convenience wrapper around [`SourceFeatureProcessor::process_files`] (Java's static `processFiles`).
pub fn process_files<R, F, P>(
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
    SourceFeatureProcessor::new(source_name, reader_factory, stats).process_files(
        source_paths,
        config,
        processor_factory,
    )
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::*;
    use crate::config::bounds::Bounds;
    use crate::stats::stats::InMemoryStats;
    use crate::value::Tags;

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
            for i in 0..self.n {
                next(ReaderFeature::new(geo_types::Point::new(self.x, i as f64).into(), Tags::new(), None, i))?;
            }
            Ok(())
        }
    }

    #[test]
    fn processes_all_features_in_bounds() {
        let mut config = PlanetilerConfig::defaults();
        config.bounds = Bounds::new(Some(Envelope::new(-10.0, 10.0, -90.0, 90.0)));
        let processor = SourceFeatureProcessor::new(
            "test",
            |path: &Path| {
                let x = if path.ends_with("a") { 0.0 } else { 50.0 };
                Ok(TestReader { n: 10, x })
            },
            InMemoryStats::arc(),
        );
        assert_eq!(20, processor.get_feature_count(&[PathBuf::from("a"), PathBuf::from("b")]).unwrap());
        let result = Arc::new(Mutex::new(Vec::new()));
        let r = result.clone();
        processor
            .process_files(vec![PathBuf::from("a"), PathBuf::from("b")], &config, move || {
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
}
