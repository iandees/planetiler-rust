//! Port of `reader/osm/OsmNodeBoundsProvider.java`: extracts the lat/lon bounds of all the points in an `.osm.pbf`
//! file.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::config::bounds::BoundsProvider;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::geo::envelope::Envelope;
use crate::reader::osm::osm_block_source::OsmBlockSource;
use crate::reader::osm::osm_element::OsmElement;
use crate::reader::osm::osm_input_file::OsmInputFile;
use crate::reader::osm::osm_phaser::{OsmPhaser, Phase};
use crate::stats::progress_loggers::ProgressLoggers;
use crate::stats::stats::StatsRef;
use crate::worker::worker_pipeline::WorkerPipeline;

/// A [`BoundsProvider`] that scans every node in an OSM file to compute its bounds.
pub struct OsmNodeBoundsProvider {
    file: OsmInputFile,
    config: PlanetilerConfig,
    stats: StatsRef,
}

impl OsmNodeBoundsProvider {
    pub fn new(file: OsmInputFile, config: PlanetilerConfig, stats: StatsRef) -> OsmNodeBoundsProvider {
        OsmNodeBoundsProvider { file, config, stats }
    }

    /// Scans the input file, returning the bounds of every node (a null envelope if there are none).
    pub fn compute_lat_lon_bounds(&self) -> anyhow::Result<Envelope> {
        tracing::warn!(
            "Bounds not found in header for {:?} and --bounds not provided, parsing bounds from nodes",
            self.file.path().file_name().unwrap_or_default()
        );
        let timer = self.stats.start_stage("osm_bounds");

        // If the node location writer supports parallel writes, then parse, process, and write node locations from
        // worker threads
        let parse_threads = self.config.threads.saturating_sub(1).max(1);
        let phaser = Arc::new(OsmPhaser::new(parse_threads));
        let envelopes: Arc<Mutex<Vec<Envelope>>> = Arc::default();
        let source = Arc::new(self.file.get()?);

        let pipeline = {
            let envelopes = envelopes.clone();
            let phaser = phaser.clone();
            WorkerPipeline::start("osm_bounds", self.stats.clone())
                .from_generator("read", move |next| {
                    source.for_each_block(&mut |block| {
                        next.accept(block);
                        Ok(())
                    })
                })
                .add_buffer("pbf_blocks", parse_threads * 2)
                .sink_to("process", parse_threads, move |blocks| {
                    let mut envelope = Envelope::null();
                    let mut worker = phaser.for_worker();
                    for block in blocks {
                        for element in block.decode_elements()? {
                            match element {
                                OsmElement::Node(node) => {
                                    worker.arrive(Phase::Nodes)?;
                                    envelope.expand_to_include(node.lon, node.lat);
                                }
                                OsmElement::Way(_) => worker.arrive(Phase::Ways)?,
                                OsmElement::Relation(_) => worker.arrive(Phase::Relations)?,
                                OsmElement::Other(_) => {}
                            }
                        }
                    }
                    worker.close()?;
                    envelopes.lock().push(envelope);
                    Ok(())
                })
        };

        let (p1, p2, p3) = (phaser.clone(), phaser.clone(), phaser.clone());
        let loggers = ProgressLoggers::create()
            .add_rate_counter_colored("nodes", move || p1.nodes(), true)
            .add_rate_counter_colored("ways", move || p2.ways(), true)
            .add_rate_counter_colored("rels", move || p3.relations(), true)
            .new_line()
            .add_process_stats()
            .new_line()
            .add_pipeline_stats(&pipeline);

        pipeline.await_and_log(&loggers, self.config.log_interval)?;
        phaser.print_summary();
        timer.stop();

        let mut all_bounds = Envelope::null();
        for env in envelopes.lock().iter() {
            all_bounds.expand_to_include_env(env);
        }
        Ok(all_bounds)
    }
}

impl BoundsProvider for OsmNodeBoundsProvider {
    fn get_lat_lon_bounds(&self) -> Option<Envelope> {
        match self.compute_lat_lon_bounds() {
            Ok(env) => Some(env),
            Err(e) => {
                tracing::error!("Error computing bounds from nodes in {:?}: {e:#}", self.file.path());
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::stats::stats::InMemoryStats;

    #[test]
    fn test() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources/monaco-latest.osm.pbf");
        let provider = OsmNodeBoundsProvider::new(
            OsmInputFile::new(path, false),
            PlanetilerConfig::defaults(),
            InMemoryStats::arc(),
        );
        assert_eq!(Some(Envelope::new(7.4016897, 7.5002447, 43.5165358, 43.7543341)), provider.get_lat_lon_bounds());
    }
}
