//! Port of `util/TopOsmTiles.java`: computes tile weights from historic openstreetmap.org tile traffic logs.
//!
//! [`TopOsmTiles::run`] returns the top tiles and their load counts; the caller can put them into a
//! [`TileWeights`](crate::util::tile_weights) instance. Precomputed top-1m tile stats can be fetched with
//! [`download_precomputed`].

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{Datelike, NaiveDate};
use parking_lot::Mutex;

use crate::config::planetiler_config::PlanetilerConfig;
use crate::geo::tile_coord::TileCoord;
use crate::stats::progress_loggers::ProgressLoggers;
use crate::stats::stats::StatsRef;
use crate::worker::worker_pipeline::WorkerPipeline;

const DOWNLOAD_URL: &str =
    "https://raw.githubusercontent.com/onthegomap/planetiler/main/layerstats/top_osm_tiles.tsv.gz";

/// Opens the tile log for a day. Return an `std::io::Error` with kind `NotFound` if there is no data for that day.
pub type LogFetcher = Arc<dyn Fn(NaiveDate) -> anyhow::Result<Box<dyn Read + Send>> + Send + Sync>;

/// Tiles and their number of loads, sorted by loads descending.
pub type TopTiles = Vec<(TileCoord, i64)>;

/// Computes the most-requested tiles from openstreetmap.org tile logs.
pub struct TopOsmTiles {
    config: PlanetilerConfig,
    stats: StatsRef,
    fetcher: LogFetcher,
}

/// URL of the openstreetmap.org tile log for `date`.
pub fn tile_log_url(date: NaiveDate) -> String {
    format!(
        "https://planet.openstreetmap.org/tile_logs/tiles-{:04}-{:02}-{:02}.txt.xz",
        date.year(),
        date.month(),
        date.day()
    )
}

impl TopOsmTiles {
    /// Creates an instance that downloads logs from openstreetmap.org.
    ///
    /// TODO(port): the logs are `.xz` compressed; decompressing them needs an xz/lzma dependency which is not in the
    /// workspace yet, so the default fetcher currently returns an error.
    pub fn new(config: PlanetilerConfig, stats: StatsRef) -> TopOsmTiles {
        let fetcher: LogFetcher =
            Arc::new(|date| anyhow::bail!("Decompressing xz tile logs is not supported yet: {}", tile_log_url(date)));
        Self::with_fetcher(config, stats, fetcher)
    }

    /// Creates an instance that reads logs using `fetcher` (for tests).
    pub fn with_fetcher(config: PlanetilerConfig, stats: StatsRef, fetcher: LogFetcher) -> TopOsmTiles {
        TopOsmTiles { config, stats, fetcher }
    }

    /// Downloads logs for each day in `to_download` using `threads` threads, and returns the `top_n` tiles up to
    /// `max_zoom` (in 512px vector tile zoom levels) sorted by number of loads descending.
    pub fn run(
        &self,
        threads: usize,
        top_n: usize,
        max_zoom: u32,
        to_download: Vec<NaiveDate>,
    ) -> anyhow::Result<Vec<(TileCoord, i64)>> {
        let timer = self.stats.start_stage("top-osm-tiles");
        let downloaded = Arc::new(AtomicI64::new(0));
        let result: Arc<Mutex<Option<TopTiles>>> = Arc::new(Mutex::new(None));
        let num_files = to_download.len() as i64;
        let (fetcher, retries, d, r) =
            (self.fetcher.clone(), self.config.http_retries, downloaded.clone(), result.clone());

        let pipeline = WorkerPipeline::start("top-osm-tiles", self.stats.clone())
            .read_from_tiny("urls", to_download)
            .add_worker("download", threads, move |prev, next| {
                for date in prev {
                    for line in read_file(&fetcher, retries, max_zoom, date) {
                        next.accept(line);
                    }
                    d.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            })
            .add_buffer_batched("lines", 100_000, 1_000)
            .sink_to("collect", 1, move |lines| {
                let mut counts: HashMap<u64, i64> = HashMap::new();
                for (tile, loads) in lines {
                    *counts.entry(tile).or_default() += loads;
                }
                tracing::info!("Extracting top {top_n} tiles from {} tiles", counts.len());
                let mut sorted: Vec<(u64, i64)> = counts.into_iter().collect();
                sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                sorted.truncate(top_n);
                *r.lock() = Some(sorted.into_iter().map(|(k, v)| (TileCoord::decode(k), v)).collect());
                Ok(())
            });

        let progress = ProgressLoggers::create()
            .add_percent_counter("files", num_files, move || downloaded.load(Ordering::Relaxed))
            .new_line()
            .add_pipeline_stats(&pipeline)
            .new_line()
            .add_process_stats();
        pipeline.await_and_log(&progress, self.config.log_interval)?;
        timer.stop();
        self.stats.print_summary();
        result.lock().take().ok_or_else(|| anyhow::anyhow!("no result"))
    }

    // TODO(port): Java's `main` (the `top-osm-tiles` CLI command) writes the result with `TileWeights.writeToFile`;
    // wire it up in the CLI once `util::tile_weights` is ported.
}

/// Reads one day's tile log, returning `(encoded tile, loads)` pairs, retrying up to `retries` times.
fn read_file(fetcher: &LogFetcher, retries: u32, max_zoom: u32, date: NaiveDate) -> Vec<(u64, i64)> {
    for i in 0..=retries {
        let attempt = (|| -> anyhow::Result<Vec<(u64, i64)>> {
            let reader = BufReader::new(fetcher(date)?);
            let mut result = vec![];
            for line in reader.lines() {
                let line = line?;
                let parts: Vec<&str> = line.split(['/', ' ']).collect();
                if parts.len() == 4 {
                    // adjust osm tiles (256x256px) to vector (512x512px) by moving up one zoom level
                    let z: i64 = parts[0].parse()?;
                    let z = z - 1;
                    if z >= 0 && z <= max_zoom as i64 {
                        let x = parts[1].parse::<u32>()? >> 1;
                        let y = parts[2].parse::<u32>()? >> 1;
                        let loads: i64 = parts[3].parse()?;
                        result.push((TileCoord::of_xyz(x, y, z as u32).encoded, loads));
                    }
                }
            }
            Ok(result)
        })();
        match attempt {
            Ok(result) => return result,
            Err(e) => {
                if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
                    tracing::info!("No data for {date}");
                    break;
                }
                if i == retries {
                    tracing::warn!("Failed getting {date} {e}");
                }
            }
        }
    }
    vec![]
}

/// Downloads precomputed top-1m tile stats from 90 days of openstreetmap.org tile logs to
/// [`PlanetilerConfig::tile_weights`] if the file doesn't already exist. Failures are logged, not returned.
pub fn download_precomputed(config: &PlanetilerConfig) {
    let dest = &config.tile_weights;
    if dest.exists() {
        return;
    }
    let mut tmp_name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    tmp_name.push(".tmp");
    let tmp = dest.with_file_name(tmp_name);
    let result = (|| -> anyhow::Result<()> {
        let mut input = crate::util::downloader::open_stream(DOWNLOAD_URL, config)?;
        tracing::info!("Downloading pre-computed tile weights from {DOWNLOAD_URL} to {}", dest.display());
        if let Some(parent) = tmp.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&tmp)?;
        std::io::copy(&mut input, &mut out)?;
        drop(out);
        std::fs::rename(&tmp, dest)?;
        Ok(())
    })();
    if let Err(e) = result {
        tracing::warn!("Failed downloading pre-computed tile weights: {e}");
        let _ = std::fs::remove_file(dest);
    }
    let _ = std::fs::remove_file(&tmp);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::arguments::Arguments;
    use crate::stats::stats::InMemoryStats;

    fn reader(s: &str) -> anyhow::Result<Box<dyn Read + Send>> {
        Ok(Box::new(std::io::Cursor::new(s.as_bytes().to_vec())))
    }

    fn day(n: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(1970, 1, 1).unwrap() + chrono::Days::new(n as u64)
    }

    #[test]
    fn fetch_top_osm_tiles() {
        let fetcher: LogFetcher = Arc::new(|date| {
            if date == day(0) {
                reader("0/0/0 1\n1/0/0 2\n1/1/0 3\n2/0/0 4\n")
            } else if date == day(1) {
                reader("0/0/0 2\n1/0/0 4\n1/1/0 6\n2/0/0 8\n3/0/0 1\n")
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing").into())
            }
        });
        let top = TopOsmTiles::with_fetcher(PlanetilerConfig::defaults(), InMemoryStats::arc(), fetcher);
        let result = top.run(2, 2, 4, vec![day(0), day(1), day(2)]).unwrap();
        let result: Vec<(u32, u32, u32, i64)> = result.into_iter().map(|(t, v)| (t.x, t.y, t.z, v)).collect();
        assert_eq!(vec![(0, 0, 0, 15), (0, 0, 1, 12)], result);
    }

    fn retry_config() -> PlanetilerConfig {
        PlanetilerConfig::from(&Arguments::of_pairs([("http-retries", "3")])).unwrap()
    }

    fn flaky(failures: i64) -> LogFetcher {
        let tries = Arc::new(AtomicI64::new(failures));
        Arc::new(move |date| {
            if date == day(0) {
                if tries.fetch_sub(1, Ordering::SeqCst) > 0 {
                    return Err(std::io::Error::other("Injected download failure").into());
                }
                reader("1/0/0 2\n")
            } else {
                Err(std::io::Error::other("other failure").into())
            }
        })
    }

    #[test]
    fn retries() {
        let top = TopOsmTiles::with_fetcher(retry_config(), InMemoryStats::arc(), flaky(3));
        let result = top.run(2, 2, 4, vec![day(0)]).unwrap();
        let result: Vec<(u32, u32, u32, i64)> = result.into_iter().map(|(t, v)| (t.x, t.y, t.z, v)).collect();
        assert_eq!(vec![(0, 0, 0, 2)], result);
    }

    #[test]
    fn exhaust_retries() {
        let top = TopOsmTiles::with_fetcher(retry_config(), InMemoryStats::arc(), flaky(4));
        assert!(top.run(2, 2, 4, vec![day(0)]).unwrap().is_empty());
    }

    #[test]
    fn log_url() {
        assert_eq!("https://planet.openstreetmap.org/tile_logs/tiles-1970-01-02.txt.xz", tile_log_url(day(1)));
    }
}
