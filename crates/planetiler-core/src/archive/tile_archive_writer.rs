//! Port of `archive/TileArchiveWriter.java`: final stage of the map generation process that encodes vector tiles
//! using [`VectorTile`](crate::vector_tile::VectorTile) and writes them to a [`WriteableTileArchive`].
//!
//! [`write_output`] runs this pipeline (all threads are scoped, so the archive and feature group are only borrowed):
//!
//! ```text
//!                         ┌─> reader_queue ─> encode (N threads) ──┐ (each batch's result is handed back through a
//! read/merge (1 thread) ──┤                                       │  one-shot channel)
//!                         ├─> writer_queue ─> write (1+ threads) <─┘
//!                         └─> layerstats_queue ─> stats (1 thread, only with --output-layerstats)
//! ```
//!
//! The reader groups tiles into variable-size batches (one huge tile alone, or up to 1,000 small tiles / 10,000
//! features), the encoders encode batches in parallel, and the writer waits on each batch in the order the reader
//! produced them so tiles are written in [`TileOrder`](crate::geo::tile_order::TileOrder).

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::Scope;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use parking_lot::Mutex;

use crate::archive::tile_archive_metadata::TileArchiveMetadata;
use crate::archive::tile_compression::TileCompression;
use crate::archive::tile_encoding_result::TileEncodingResult;
use crate::archive::tile_format::TileFormat;
use crate::archive::writeable_tile_archive::WriteableTileArchive;
use crate::collection::feature_group::{FeatureGroup, TileFeatures, TileFeaturesIter};
use crate::config::planetiler_config::PlanetilerConfig;
use crate::geo::tile_coord::TileCoord;
use crate::stats::counter::Counter;
use crate::stats::process_info;
use crate::stats::progress_loggers::ProgressLoggers;
use crate::stats::stats::StatsRef;
use crate::stats::timer::Timer;
use crate::util::file_utils::DiskBacked;
use crate::util::format;
use crate::util::gzip::gzip;
use crate::util::hashing;
use crate::util::layer_attr_stats::{self, LayerAttrStats};
use crate::util::tile_size_stats::{self, LayerStats, TsvSerializer};
use crate::util::tile_weights::TileWeights;
use crate::util::tileset_summary_statistics::{self, TilesetSummaryStatistics};
use crate::worker::work_queue::{QueueHandle, WorkQueue};
use crate::worker::{Done, panic_to_error};

/// Maximum number of features in a batch handed to an encoder thread (a single tile can exceed this).
pub const MAX_FEATURES_PER_BATCH: u64 = 10_000;
/// Maximum number of tiles in a batch handed to an encoder thread.
pub const MAX_TILES_PER_BATCH: usize = 1_000;

/// Sentinel for "no tile written yet" in [`ArchiveWriter::last_tile_written`].
const NO_TILE: u64 = u64::MAX;

/// Generates a hash over an encoded and compressed tile.
///
/// Used as an optimization to avoid writing the same (mostly ocean) tiles over and over again.
pub fn generate_content_hash(bytes: &[u8]) -> u64 {
    hashing::fnv1a64(bytes)
}

/// A batch of tiles to be encoded by an encoder thread. The encoder sends the encoded tiles to the writer (and the
/// layer stats rows to the stats writer) through one-shot channels so the writers can consume batches in order.
struct EncodeJob {
    tiles: Vec<TileFeatures>,
    out: Sender<Vec<TileEncodingResult>>,
    layer_stats_out: Option<Sender<Vec<String>>>,
}

/// State shared by all threads of one [`write_output`] call (Java's `TileArchiveWriter` instance).
struct ArchiveWriter<'a> {
    archive: &'a dyn WriteableTileArchive,
    config: &'a PlanetilerConfig,
    stats: &'a StatsRef,
    features_processed: Counter,
    memoized_tiles: Counter,
    tiles_by_zoom: Arc<Vec<Counter>>,
    last_tile_written: Arc<AtomicU64>,
    tile_stats: TilesetSummaryStatistics,
    layer_attr_stats: LayerAttrStats,
    first_tile_writer: AtomicBool,
    deduplicates: bool,
}

/// Reads all `features`, encodes them in parallel, and writes them to `output`.
///
/// `features` is prepared (sorted) first if needed. `file_size` is used for progress logging only (the size of the
/// output archive). When `config.output_layer_stats` is set, per-layer stats rows are written to `layer_stats_path`
/// as a gzipped TSV (see [`tile_size_stats`]). Calls [`WriteableTileArchive::initialize`] before writing any tiles and
/// [`WriteableTileArchive::finish`] with `metadata` plus the layer attribute stats collected while encoding; the
/// caller is responsible for [`WriteableTileArchive::close`].
pub fn write_output(
    features: &mut FeatureGroup,
    output: &mut dyn WriteableTileArchive,
    file_size: Arc<dyn DiskBacked + Send + Sync>,
    metadata: &TileArchiveMetadata,
    layer_stats_path: Option<&Path>,
    config: &PlanetilerConfig,
    stats: &StatsRef,
) -> anyhow::Result<()> {
    let timer = stats.start_stage("archive");
    if config.tile_compression == TileCompression::Unknown {
        bail!("cannot compress \"UNKNOWN\"");
    }
    let layer_stats_path = if config.output_layer_stats {
        Some(layer_stats_path.context("output_layer_stats requires a layer stats path")?)
    } else {
        None
    };

    let chunks_to_read = features.chunks_to_read().max(1);
    let read_threads = config.feature_read_threads.min(chunks_to_read).max(1);
    let threads = config.threads.max(1);
    let process_threads = if threads < 8 { threads } else { threads - read_threads }.max(1);
    let tile_write_threads = config.tile_write_threads.max(1);

    // when using more than 1 read thread: (N read threads) -> (1 merge thread) -> ...
    // when using 1 read thread we just have: (1 read & merge thread) -> ...
    let (input_tiles, second_stage_name) = if read_threads == 1 {
        (features.iter()?, "read")
    } else {
        (features.parallel_iterator(read_threads)?, "merge")
    };
    let num_features_written = features.num_features_written();
    let features_disk_usage = features.disk_usage_bytes();

    output.initialize()?;
    let archive: &dyn WriteableTileArchive = output;
    let writer = ArchiveWriter::new(archive, config, stats);

    // a larger tile queue size helps keep cores busy, but needs a lot of RAM
    // 5k works fine with 100GB of RAM, so adjust the queue size down from there
    // but no less than 100
    let queue_size = 100.max((5_000.0 * process_info::get_max_memory_bytes() as f64 / 100_000_000_000.0) as usize);

    // To emit tiles in order, fork the input queue and send features to both the encoder and writer. The writer waits
    // on them to be encoded in the order they were received, and the encoder processes them in parallel. One batch
    // might take a long time to process, so make the queues very big to avoid idle encoding CPUs.
    let reader_queue: WorkQueue<EncodeJob> = WorkQueue::new("archive_reader_queue", queue_size, 1, stats);
    let writer_queue: WorkQueue<Receiver<Vec<TileEncodingResult>>> =
        WorkQueue::new("archive_writer_queue", queue_size, 1, stats);
    let layer_stats_queue: Option<WorkQueue<Receiver<Vec<String>>>> =
        layer_stats_path.is_some().then(|| WorkQueue::new("archive_layerstats_queue", queue_size, 1, stats));

    let all_queues: Vec<Arc<dyn QueueHandle>> = {
        let mut queues: Vec<Arc<dyn QueueHandle>> =
            vec![Arc::new(reader_queue.clone()), Arc::new(writer_queue.clone())];
        if let Some(q) = &layer_stats_queue {
            queues.push(Arc::new(q.clone()));
        }
        queues
    };

    let mut loggers = ProgressLoggers::create()
        .add_rate_percent_counter("features", num_features_written as i64, writer.features_processed_fn(), true)
        .add(move || format!(" {}", format::pad_right(&format::storage(features_disk_usage, false), 5)))
        .add_rate_counter("tiles", writer.tiles_emitted_fn())
        .add_file_size(file_size)
        .new_line()
        .add_process_stats()
        .new_line();
    if read_threads > 1 {
        loggers = loggers.add_thread_pool_stats("read", "read-");
    }
    loggers = loggers
        .add_thread_pool_stats(second_stage_name, &format!("archive_{second_stage_name}-"))
        .add_queue_stats(Arc::new(reader_queue.clone()))
        .add_thread_pool_stats("encode", "archive_encode-")
        .add_queue_stats(Arc::new(writer_queue.clone()))
        .add_thread_pool_stats("write", "archive_write-");
    if let Some(q) = &layer_stats_queue {
        loggers = loggers.add_queue_stats(Arc::new(q.clone())).add_thread_pool_stats("stats", "archive_stats-");
    }
    loggers = loggers.new_line().add(writer.last_tile_log_details_fn());

    let input_tiles = Mutex::new(Some(input_tiles));
    std::thread::scope(|scope| -> anyhow::Result<()> {
        let w = &writer;
        let read_done = spawn_workers(scope, &format!("archive_{second_stage_name}"), 1, stats, |_| {
            let tiles = input_tiles.lock().take().context("reader already started")?;
            let result = w.read_features_and_batch(tiles, &reader_queue, &writer_queue, layer_stats_queue.as_ref());
            reader_queue.close();
            writer_queue.close();
            if let Some(q) = &layer_stats_queue {
                q.close();
            }
            result
        });
        let encode_done =
            spawn_workers(scope, "archive_encode", process_threads, stats, |_| w.tile_encoder_sink(&reader_queue));
        let write_done =
            spawn_workers(scope, "archive_write", tile_write_threads, stats, |_| w.tile_writer(&writer_queue));
        let mut dones = vec![read_done, encode_done, write_done];
        if let (Some(q), Some(path)) = (&layer_stats_queue, layer_stats_path) {
            dones.push(spawn_workers(scope, "archive_stats", 1, stats, |_| tile_stats_writer(path, q)));
        }
        let done = Done::join_all(dones);
        // when any step fails, abort every queue so the remaining threads exit
        done.on_complete(move |result| {
            if result.is_err() {
                for queue in &all_queues {
                    queue.abort();
                }
            }
        });
        loggers.await_and_log(&done, config.log_interval)
    })?;

    let ArchiveWriter { tile_stats, layer_attr_stats, features_processed, .. } = writer;
    output
        .finish(&metadata.with_layer_stats(layer_attr_stats.get_tile_stats().into_iter().map(Into::into).collect()))?;
    print_tile_stats(&tile_stats, &features_processed, config);
    timer.stop();
    Ok(())
}

/// Starts `threads` scoped threads named `{prefix}-1`, `{prefix}-2`, ... that each run `task` once, like
/// [`Worker`](crate::worker::worker::Worker) but able to borrow from the enclosing scope.
fn spawn_workers<'scope, 'env, F>(
    scope: &'scope Scope<'scope, 'env>,
    prefix: &str,
    threads: usize,
    stats: &StatsRef,
    task: F,
) -> Done
where
    F: Fn(usize) -> anyhow::Result<()> + Send + Sync + 'scope,
{
    let threads = threads.max(1);
    stats.gauge_value(&format!("{prefix}_threads"), threads as f64);
    let task = Arc::new(task);
    let mut dones = Vec::with_capacity(threads);
    for thread_id in 0..threads {
        let done = Done::new();
        dones.push(done.clone());
        let task = task.clone();
        let stats = stats.clone();
        let prefix_owned = prefix.to_string();
        let name = format!("{prefix}-{}", thread_id + 1);
        let thread_done = done.clone();
        let spawned = std::thread::Builder::new().name(name.clone()).spawn_scoped(scope, move || {
            process_info::register_current_thread();
            let start = Instant::now();
            let result = match catch_unwind(AssertUnwindSafe(|| task(thread_id))) {
                Ok(r) => r,
                Err(payload) => Err(panic_to_error(payload)),
            };
            match result {
                Ok(()) => {
                    stats.timers().finished_worker_thread(&prefix_owned, start.elapsed());
                    thread_done.complete(Ok(()));
                }
                Err(e) => {
                    tracing::error!("Worker {name} died: {e:?}");
                    thread_done.fail(e.context(format!("Worker {name} failed")));
                }
            }
        });
        if let Err(e) = spawned {
            done.fail(anyhow::Error::new(e).context("Failed to spawn worker thread"));
        }
    }
    Done::join_all(dones)
}

/// Waits for an encoder to finish a batch, giving up if the pipeline was aborted (pending batches in an aborted queue
/// are never encoded, but their senders are not dropped until the queue is).
fn await_batch<T, Q>(batch: &Receiver<T>, queue: &WorkQueue<Q>) -> anyhow::Result<T> {
    loop {
        match batch.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => return Ok(result),
            Err(RecvTimeoutError::Timeout) if queue.is_aborted() => bail!("archive pipeline aborted"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("tile encoder failed"),
        }
    }
}

fn tile_stats_writer(path: &Path, queue: &WorkQueue<Receiver<Vec<String>>>) -> anyhow::Result<()> {
    let mut stats_writer = tile_size_stats::new_writer(path)
        .with_context(|| format!("Unable to write layer stats to {}", path.display()))?;
    stats_writer.write_all(tile_size_stats::header_row().as_bytes())?;
    for batch in queue.thread_local_reader() {
        let rows = await_batch(&batch, queue)?;
        for row in rows {
            stats_writer.write_all(row.as_bytes())?;
        }
    }
    stats_writer.finish()?.flush()?;
    Ok(())
}

/// Per-encoder-thread memo of the last encoded tile, to avoid re-encoding consecutive identical tiles (like large
/// ocean areas).
#[derive(Default)]
struct EncoderState {
    last_is_fill: bool,
    last_encoded_len: usize,
    last_layer_stats: Vec<LayerStats>,
    last_tile_data_hash: Option<u64>,
    /// Index into the current batch's results of the last encoded tile, or `None` if it was not emitted.
    last_result: Option<usize>,
}

impl<'a> ArchiveWriter<'a> {
    fn new(archive: &'a dyn WriteableTileArchive, config: &'a PlanetilerConfig, stats: &'a StatsRef) -> Self {
        let tile_weights = TileWeights::read_from_file(&config.tile_weights);
        let tiles_by_zoom: Arc<Vec<Counter>> = Arc::new((0..=config.maxzoom).map(|_| Counter::new()).collect());
        let by_zoom = tiles_by_zoom.clone();
        let (minzoom, maxzoom) = (config.minzoom, config.maxzoom);
        stats.labeled_counter(
            "archive_tiles_written",
            "zoom",
            Arc::new(move || (minzoom..=maxzoom).map(|z| (z.to_string(), by_zoom[z as usize].get())).collect()),
        );
        ArchiveWriter {
            archive,
            config,
            stats,
            features_processed: stats.long_counter("archive_features_processed"),
            memoized_tiles: stats.long_counter("archive_memoized_tiles"),
            tiles_by_zoom,
            last_tile_written: Arc::new(AtomicU64::new(NO_TILE)),
            tile_stats: TilesetSummaryStatistics::with_weights(Arc::new(tile_weights)),
            layer_attr_stats: LayerAttrStats::new(),
            first_tile_writer: AtomicBool::new(true),
            deduplicates: archive.deduplicates(),
        }
    }

    fn features_processed_fn(&self) -> impl Fn() -> i64 + Send + Sync + 'static {
        let counter = self.features_processed.clone();
        move || counter.get()
    }

    fn tiles_emitted_fn(&self) -> impl Fn() -> i64 + Send + Sync + 'static {
        let counters = self.tiles_by_zoom.clone();
        move || counters.iter().map(Counter::get).sum()
    }

    fn last_tile_log_details_fn(&self) -> impl Fn() -> String + Send + Sync + 'static {
        let last = self.last_tile_written.clone();
        let order = self.archive.tile_order();
        let extents = self.config.bounds.clone().tile_extents();
        let pattern = self.config.debug_url_pattern.clone();
        move || {
            let encoded = last.load(Ordering::Relaxed);
            let blurb = if encoded == NO_TILE {
                "n/a".to_string()
            } else {
                let tile = TileCoord::decode(encoded);
                format!(
                    "{}/{}/{} (z{} {}) {}",
                    tile.z,
                    tile.x,
                    tile.y,
                    tile.z,
                    format::percent(order.progress_on_level(&tile, &extents)),
                    tile.get_debug_url(&pattern)
                )
            };
            format!("last tile: {blurb}")
        }
    }

    fn read_features_and_batch(
        &self,
        mut input_tiles: TileFeaturesIter,
        reader_queue: &WorkQueue<EncodeJob>,
        writer_queue: &WorkQueue<Receiver<Vec<TileEncodingResult>>>,
        layer_stats_queue: Option<&WorkQueue<Receiver<Vec<String>>>>,
    ) -> anyhow::Result<()> {
        let mut encode_enqueuer = reader_queue.thread_local_writer();
        let mut writer_enqueuer = writer_queue.thread_local_writer();
        let mut stats_enqueuer = layer_stats_queue.map(|q| q.thread_local_writer());
        let mut emit = |tiles: Vec<TileFeatures>| {
            let (out, result) = bounded(1);
            let layer_stats_out = stats_enqueuer.as_mut().map(|enqueuer| {
                let (out, result) = bounded(1);
                enqueuer.accept(result);
                out
            });
            encode_enqueuer.accept(EncodeJob { tiles, out, layer_stats_out });
            // also send immediately to writer
            writer_enqueuer.accept(result);
        };

        let mut current_zoom = u32::MAX;
        let mut batch: Vec<TileFeatures> = Vec::new();
        let mut features_in_this_batch = 0u64;
        for tile in &mut input_tiles {
            let z = tile.tile_coord().z;
            if z != current_zoom {
                tracing::trace!("Starting z{z}");
                current_zoom = z;
                if reader_queue.is_aborted() {
                    break;
                }
            }
            let this_tile_features = tile.num_features_to_emit();
            if !batch.is_empty()
                && (batch.len() >= MAX_TILES_PER_BATCH
                    || features_in_this_batch + this_tile_features > MAX_FEATURES_PER_BATCH)
            {
                emit(std::mem::take(&mut batch));
                features_in_this_batch = 0;
                if reader_queue.is_aborted() {
                    break;
                }
            }
            features_in_this_batch += this_tile_features;
            batch.push(tile);
        }
        if !batch.is_empty() {
            emit(batch);
        }
        drop(emit);
        input_tiles.join()
    }

    fn tile_encoder_sink(&self, queue: &WorkQueue<EncodeJob>) -> anyhow::Result<()> {
        let mut state = EncoderState::default();
        let layer_stats_serializer = tile_size_stats::new_thread_local_serializer();
        let tile_stats_updater = self.tile_stats.thread_local_updater();
        let layer_attr_stats_updater = self.layer_attr_stats.handler_for_thread();
        for job in queue.thread_local_reader() {
            let (result, layer_stats_rows) = self.encode_batch(
                &job.tiles,
                &mut state,
                &tile_stats_updater,
                &layer_attr_stats_updater,
                &layer_stats_serializer,
            )?;
            drop(job.tiles);
            // hand result off to writer (the receivers are gone if the pipeline was aborted)
            let _ = job.out.send(result);
            if let Some(out) = job.layer_stats_out {
                let _ = out.send(layer_stats_rows);
            }
        }
        Ok(())
    }

    /// Encodes the tiles in a batch (in tile order). Returns the encoded tiles and layer stats TSV rows.
    fn encode_batch(
        &self,
        tiles: &[TileFeatures],
        state: &mut EncoderState,
        tile_stats_updater: &tileset_summary_statistics::Updater,
        layer_attr_stats_updater: &layer_attr_stats::Updater,
        layer_stats_serializer: &TsvSerializer,
    ) -> anyhow::Result<(Vec<TileEncodingResult>, Vec<String>)> {
        let config = self.config;
        let skip_filled = config.skip_filled_tiles;
        let include_ids = !config.exclude_ids;
        let mlt_options = crate::mlt::MltOptions::from_config(config);
        let mut result: Vec<TileEncodingResult> = Vec::with_capacity(tiles.len());
        let mut layer_stats_rows = Vec::new();
        let mut last: Option<&TileFeatures> = None;
        state.last_result = None;
        // each batch contains tiles ordered by tile-order ID ascending
        for tile_features in tiles {
            self.features_processed.inc_by(tile_features.num_features_processed() as i64);
            let coord = tile_features.tile_coord();
            let bytes = if tile_features.has_same_contents(last) {
                self.memoized_tiles.inc();
                state.last_result.map(|i| result[i].tile_data.clone())
            } else {
                let tile = tile_features.get_vector_tile_with_stats(Some(layer_attr_stats_updater));
                if tile.is_empty() {
                    continue;
                }
                let bytes = if skip_filled && {
                    state.last_is_fill = tile.contains_only_fills();
                    state.last_is_fill
                } {
                    state.last_layer_stats = Vec::new();
                    state.last_encoded_len = 0;
                    None
                } else {
                    let (encoded, layer_stats) = match config.tile_format {
                        TileFormat::Mlt => tile
                            .encode_mlt_with_layer_stats(include_ids, &mlt_options, Some(self.stats.as_ref()))
                            .with_context(|| format!("error encoding {coord} as MLT"))?,
                        TileFormat::Mvt | TileFormat::Unknown => tile.encode_with_layer_stats(include_ids),
                    };
                    let encoded_len = encoded.len();
                    let bytes = match config.tile_compression {
                        TileCompression::Gzip => gzip(&encoded)?,
                        TileCompression::None => encoded,
                        TileCompression::Unknown => bail!("cannot compress \"UNKNOWN\""),
                    };
                    if encoded_len > config.tile_warning_size_bytes {
                        tracing::warn!("{coord} {}kb uncompressed", encoded_len / 1024);
                    }
                    state.last_layer_stats = layer_stats;
                    state.last_encoded_len = encoded_len;
                    Some(bytes)
                };
                last = Some(tile_features);
                state.last_tile_data_hash = match &bytes {
                    Some(bytes) if self.deduplicates && tile.likely_to_be_duplicated() => {
                        Some(generate_content_hash(bytes))
                    }
                    _ => None,
                };
                state.last_result = None;
                bytes
            };
            if !(skip_filled && state.last_is_fill)
                && let Some(bytes) = bytes
            {
                tile_stats_updater.record_tile(coord, bytes.len() as i64, &state.last_layer_stats);
                if config.output_layer_stats {
                    layer_stats_rows.extend(layer_stats_serializer.format_output_rows(
                        coord,
                        bytes.len() as i64,
                        &state.last_layer_stats,
                    ));
                }
                state.last_result = Some(result.len());
                result.push(TileEncodingResult::with_stats(
                    coord,
                    bytes,
                    state.last_encoded_len,
                    state.last_tile_data_hash,
                    Vec::new(),
                ));
            }
        }
        Ok((result, layer_stats_rows))
    }

    fn tile_writer(&self, queue: &WorkQueue<Receiver<Vec<TileEncodingResult>>>) -> anyhow::Result<()> {
        let first_tile_writer = self.first_tile_writer.swap(false, Ordering::AcqRel);
        let order = self.archive.tile_order();
        let mut last_tile: Option<TileCoord> = None;
        let mut time: Option<Timer> = None;
        let mut current_z = u32::MAX;
        let mut tile_writer = self.archive.new_tile_writer()?;
        for batch in queue.thread_local_reader() {
            let encoded_tiles = await_batch(&batch, queue)?;
            for encoded_tile in encoded_tiles {
                let tile_coord = encoded_tile.coord;
                debug_assert!(
                    last_tile.is_none_or(|last| order.encode(&tile_coord) > order.encode(&last)),
                    "Tiles out of order {last_tile:?} before {tile_coord:?}"
                );
                last_tile = Some(tile_coord);
                let z = tile_coord.z;
                if z != current_z {
                    // for multiple writers the starting/finish log message of the _first_ tilewriter is not 100%
                    // accurate in terms of overall "zoom-progress", but it should be a "good-enough" indicator for
                    // "zoom-progress"-logging
                    if first_tile_writer {
                        match &time {
                            None => tracing::info!("Starting z{z}"),
                            Some(t) => tracing::info!("Finished z{current_z} in {}, now starting z{z}", t.stop()),
                        }
                    }
                    time = Some(Timer::start());
                    current_z = z;
                }
                let len = encoded_tile.tile_data.len();
                tile_writer.write(encoded_tile)?;
                self.stats.wrote_tile(z, len);
                if let Some(counter) = self.tiles_by_zoom.get(z as usize) {
                    counter.inc();
                }
            }
            if let Some(last) = last_tile {
                self.last_tile_written.store(last.encoded, Ordering::Relaxed);
            }
        }
        tile_writer.print_stats();
        tile_writer.close()?;
        if let Some(t) = time {
            tracing::info!("Finished z{current_z} in {}", t.stop());
        }
        Ok(())
    }
}

fn print_tile_stats(tile_stats: &TilesetSummaryStatistics, features_processed: &Counter, config: &PlanetilerConfig) {
    tile_stats.print_stats(&config.debug_url_pattern);
    tracing::debug!(" # features: {}", format::integer(features_processed.get()));
}
