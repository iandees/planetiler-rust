//! Port of `util/TopOsmTiles.java`: computes tile weights from historic openstreetmap.org tile traffic logs.
//!
//! [`TopOsmTiles::run`] returns the top tiles and their load counts as [`TileWeights`], and [`main`] (the
//! `top-osm-tiles` CLI task) writes them to a file. Precomputed top-1m tile stats can be fetched with
//! [`download_precomputed`].

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{Datelike, NaiveDate};
use parking_lot::Mutex;

use crate::config::arguments::Arguments;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::geo::tile_coord::TileCoord;
use crate::stats::progress_loggers::ProgressLoggers;
use crate::stats::stats::StatsRef;
use crate::util::downloader::Downloader;
use crate::util::tile_weights::TileWeights;
use crate::worker::worker_pipeline::WorkerPipeline;

const DOWNLOAD_URL: &str =
    "https://raw.githubusercontent.com/onthegomap/planetiler/main/layerstats/top_osm_tiles.tsv.gz";

/// Opens the tile log for a day. Return an `std::io::Error` with kind `NotFound` if there is no data for that day.
pub type LogFetcher = Arc<dyn Fn(NaiveDate) -> anyhow::Result<Box<dyn Read + Send>> + Send + Sync>;

/// Computes the most-requested tiles from openstreetmap.org tile logs.
pub struct TopOsmTiles {
    config: PlanetilerConfig,
    stats: StatsRef,
    fetcher: LogFetcher,
}

const TILE_LOGS_URL: &str = "https://planet.openstreetmap.org/tile_logs";

/// URL of the openstreetmap.org tile log for `date`.
pub fn tile_log_url(date: NaiveDate) -> String {
    tile_log_url_from(TILE_LOGS_URL, date)
}

fn tile_log_url_from(base_url: &str, date: NaiveDate) -> String {
    format!("{base_url}/tiles-{:04}-{:02}-{:02}.txt.xz", date.year(), date.month(), date.day())
}

/// Returns a fetcher that downloads `.xz` compressed tile logs from `base_url` (a 404 is reported as `NotFound`).
pub fn http_fetcher(downloader: &Downloader, base_url: &str) -> LogFetcher {
    let (downloader, base_url) = (downloader.clone(), base_url.to_string());
    Arc::new(move |date| {
        let input = downloader.open_stream(&tile_log_url_from(&base_url, date))?;
        Ok(Box::new(xz_reader(input)) as Box<dyn Read + Send>)
    })
}

impl TopOsmTiles {
    /// Creates an instance that downloads `.xz` compressed logs from openstreetmap.org with `downloader`.
    pub fn new(downloader: Downloader, stats: StatsRef) -> TopOsmTiles {
        let fetcher = http_fetcher(&downloader, TILE_LOGS_URL);
        Self::with_fetcher(downloader.config().clone(), stats, fetcher)
    }

    /// Creates an instance that reads logs using `fetcher` (for tests).
    pub fn with_fetcher(config: PlanetilerConfig, stats: StatsRef, fetcher: LogFetcher) -> TopOsmTiles {
        TopOsmTiles { config, stats, fetcher }
    }

    /// Downloads logs for each day in `to_download` using `threads` threads, and returns the `top_n` tiles up to
    /// `max_zoom` (in 512px vector tile zoom levels) by number of loads.
    pub fn run(
        &self,
        threads: usize,
        top_n: usize,
        max_zoom: u32,
        to_download: Vec<NaiveDate>,
    ) -> anyhow::Result<TileWeights> {
        let timer = self.stats.start_stage("top-osm-tiles");
        let downloaded = Arc::new(AtomicI64::new(0));
        let result: Arc<Mutex<Option<TileWeights>>> = Arc::new(Mutex::new(None));
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
                let mut weights = TileWeights::new();
                for (tile, loads) in sorted.into_iter().take(top_n) {
                    weights.put(TileCoord::decode(tile), loads);
                }
                *r.lock() = Some(weights);
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
}

/// Decompresses an `.xz` stream (with `lzma-rust2`, a port of the XZ for Java library that Java uses).
pub fn xz_reader(input: impl Read) -> impl Read {
    lzma_rust2::XzReader::new(BufReader::new(input), true)
}

/// The `top-osm-tiles` CLI task (Java `TopOsmTiles.main`): downloads the last `--days` days of openstreetmap.org
/// tile logs and writes the `--top` most-requested tiles up to `--maxzoom` to `--output`.
pub fn main<S: AsRef<str>>(args: &[S]) -> anyhow::Result<()> {
    let arguments = Arguments::from_args_or_config_file(args)?.or_else(Arguments::of_pairs([("http-retries", "3")]));
    let today = chrono::Utc::now().date_naive();
    run_main(&arguments, today, TopOsmTiles::new)
}

/// [`main`] with a fixed `today` and a way to create the [`TopOsmTiles`] instance (for tests).
fn run_main(
    arguments: &Arguments,
    today: NaiveDate,
    create: impl FnOnce(Downloader, StatsRef) -> TopOsmTiles,
) -> anyhow::Result<()> {
    let config = PlanetilerConfig::from(arguments)?;
    let downloader = Downloader::create(config);
    let stats = arguments.get_stats()?;
    let days = arguments.get_integer("days", "number of days into the past to look", 90)?;
    let max_zoom = arguments.get_integer("maxzoom", "max zoom", 16)?;
    let top_n = arguments.get_integer("top", "top n", 1_000_000)?;
    let output: PathBuf = arguments.file("output", "output", "top_osm_tiles.tsv.gz");
    let default_threads = arguments.threads()?.min(10) as i32;
    let threads = arguments.get_integer(
        "download-threads",
        "number of threads to use for downloading/parsing",
        default_threads,
    )?;

    let to_download = (0..days.max(0) as u64).map(|i| today - chrono::Days::new(i)).collect();
    create(downloader, stats)
        .run(threads.max(1) as usize, top_n.max(0) as usize, max_zoom.max(0) as u32, to_download)?
        .write_to_file(&output)?;
    Ok(())
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
                let not_found =
                    e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                        || matches!(
                            crate::util::downloader::download_error(&e),
                            Some(crate::util::downloader::DownloadError::BadResponse { status: 404, .. })
                        );
                if not_found {
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
pub fn download_precomputed(downloader: &Downloader) {
    let dest = &downloader.config().tile_weights;
    if dest.exists() {
        return;
    }
    let mut tmp_name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    tmp_name.push(".tmp");
    let tmp = dest.with_file_name(tmp_name);
    let result = (|| -> anyhow::Result<()> {
        let mut input = downloader.open_stream(DOWNLOAD_URL)?;
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
    use std::io::Write;

    use super::*;
    use crate::stats::stats::InMemoryStats;
    use crate::testing::stub_downloader::StubDownloader;

    fn reader(s: &str) -> anyhow::Result<Box<dyn Read + Send>> {
        Ok(Box::new(std::io::Cursor::new(s.as_bytes().to_vec())))
    }

    fn day(n: i64) -> NaiveDate {
        NaiveDate::from_ymd_opt(1970, 1, 1).unwrap() + chrono::Days::new(n as u64)
    }

    fn weights(tiles: &[(u32, u32, u32, i64)]) -> TileWeights {
        let mut result = TileWeights::new();
        for &(x, y, z, loads) in tiles {
            result.put(TileCoord::of_xyz(x, y, z), loads);
        }
        result
    }

    const DAY0: &str = "0/0/0 1\n1/0/0 2\n1/1/0 3\n2/0/0 4\n";
    const DAY1: &str = "0/0/0 2\n1/0/0 4\n1/1/0 6\n2/0/0 8\n3/0/0 1\n";

    #[test]
    fn fetch_top_osm_tiles() {
        let fetcher: LogFetcher = Arc::new(|date| {
            if date == day(0) {
                reader(DAY0)
            } else if date == day(1) {
                reader(DAY1)
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing").into())
            }
        });
        let top = TopOsmTiles::with_fetcher(PlanetilerConfig::defaults(), InMemoryStats::arc(), fetcher);
        let result = top.run(2, 2, 4, vec![day(0), day(1), day(2)]).unwrap();
        assert_eq!(weights(&[(0, 0, 0, 15), (0, 0, 1, 12)]), result);
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
        assert_eq!(weights(&[(0, 0, 0, 2)]), result);
    }

    #[test]
    fn exhaust_retries() {
        let top = TopOsmTiles::with_fetcher(retry_config(), InMemoryStats::arc(), flaky(4));
        assert_eq!(TileWeights::new(), top.run(2, 2, 4, vec![day(0)]).unwrap());
    }

    #[test]
    fn log_url() {
        assert_eq!("https://planet.openstreetmap.org/tile_logs/tiles-1970-01-02.txt.xz", tile_log_url(day(1)));
    }

    fn xz(data: &str) -> Vec<u8> {
        let mut writer = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(6)).unwrap();
        writer.write_all(data.as_bytes()).unwrap();
        writer.finish().unwrap()
    }

    #[test]
    fn decompress_xz() {
        let mut result = String::new();
        xz_reader(&xz(DAY1)[..]).read_to_string(&mut result).unwrap();
        assert_eq!(DAY1, result);
        // concatenated streams
        let mut both = xz(DAY0);
        both.extend(xz(DAY1));
        result.clear();
        xz_reader(&both[..]).read_to_string(&mut result).unwrap();
        assert_eq!(format!("{DAY0}{DAY1}"), result);
    }

    /// A stub server with `.xz` tile logs for day 0 and day 1 under [`TILE_LOGS_URL`] (404 for anything else).
    fn stub_tile_logs(config: PlanetilerConfig) -> StubDownloader {
        let stub = StubDownloader::new(config);
        stub.put(&tile_log_url(day(0)), &xz(DAY0));
        stub.put(&tile_log_url(day(1)), &xz(DAY1));
        stub
    }

    #[test]
    fn main_downloads_and_writes_tile_weights() {
        let stub = stub_tile_logs(PlanetilerConfig::defaults());
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("top.tsv.gz");
        let arguments = Arguments::of_pairs([
            ("days", "3"),
            ("top", "2"),
            ("maxzoom", "4"),
            ("download-threads", "1"),
            ("output", output.to_str().unwrap()),
        ]);
        run_main(&arguments, day(2), |_, stats| TopOsmTiles::new(stub.downloader(), stats)).unwrap();
        let mut urls: Vec<String> = stub.requests().into_iter().map(|r| r.url).collect();
        urls.sort();
        assert_eq!(
            vec![
                "https://planet.openstreetmap.org/tile_logs/tiles-1970-01-01.txt.xz",
                "https://planet.openstreetmap.org/tile_logs/tiles-1970-01-02.txt.xz",
                "https://planet.openstreetmap.org/tile_logs/tiles-1970-01-03.txt.xz"
            ],
            urls
        );
        assert_eq!(weights(&[(0, 0, 0, 15), (0, 0, 1, 12)]), TileWeights::read_from_file(&output));
    }

    #[test]
    fn download_precomputed_uses_downloader() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("weights.tsv.gz");
        let mut config = PlanetilerConfig::defaults();
        config.tile_weights = dest.clone();
        let stub = StubDownloader::new(config);
        stub.put(DOWNLOAD_URL, b"contents");
        download_precomputed(&stub.downloader());
        assert_eq!(b"contents".to_vec(), std::fs::read(&dest).unwrap());
        // does not download again once the file exists
        stub.clear_requests();
        download_precomputed(&stub.downloader());
        assert!(stub.requests().is_empty());
    }
}
