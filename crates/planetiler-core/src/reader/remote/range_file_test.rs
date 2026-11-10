//! Port of `RangeFileTest`: behavior every [`RangeFile`] shares, plus tests for local, HTTP and cached files.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::cached_range_file::CachedRangeFile;
use super::download_mode::DownloadMode;
use super::http_range_file::HttpRangeFile;
use super::local_range_file::LocalRangeFile;
use super::range_file::{self, RangeFile};
use crate::config::arguments::Arguments;
use crate::config::planetiler_config::PlanetilerConfig;
use crate::testing::stub_downloader::{StubDownloader, StubRequest};
use crate::util::downloader::{ByteRange, DownloadError, download_error};
use crate::util::request_planner::Plan;

const URL: &str = "http://host/file.parquet";
/// Like parquet files, which fetch their footer when they get opened.
const TAIL_BYTES: usize = 1 << 20;

fn contents(length: usize) -> Vec<u8> {
    (0..length).map(|i| (i * 31 + (i >> 8)) as u8).collect()
}

/// Larger than the tail fetched when opening a remote file, so reads at the start need range requests.
fn contents_3mb() -> Vec<u8> {
    contents(3_000_000)
}

fn config() -> PlanetilerConfig {
    PlanetilerConfig::from(&Arguments::of_pairs([("download_chunk_size_mb", "1")])).unwrap()
}

fn range(offset: u64, length: u64) -> ByteRange {
    ByteRange::new(offset, length)
}

fn slice(offset: usize, length: usize) -> Vec<u8> {
    contents_3mb()[offset..offset + length].to_vec()
}

fn assert_reads(file: &dyn RangeFile, offset: u64, length: usize) {
    let expected = slice(offset as usize, length);
    assert_eq!(expected, file.read(offset, length).unwrap(), "read {offset}+{length}");
    let later = file.read_later(&[range(offset, length as u64)]).unwrap().remove(0).get().unwrap();
    assert_eq!(expected, later, "readLater {offset}+{length}");
}

fn ranges(downloader: &StubDownloader) -> Vec<String> {
    downloader.requests().into_iter().map(|r| r.range.unwrap()).collect()
}

fn request(range: &str) -> StubRequest {
    StubRequest { url: URL.into(), range: Some(range.into()), if_match: None, if_unmodified_since: None }
}

fn is<T: 'static>(file: &Arc<dyn RangeFile>) -> bool {
    file.as_any().is::<T>()
}

fn common_tests(file: &dyn RangeFile) {
    // length
    assert_eq!(3_000_000, file.length().unwrap());
    // reads
    assert_reads(file, 0, 10);
    assert_reads(file, 0, 0);
    assert_reads(file, 999_990, 20);
    assert_reads(file, 500_000, 1_500_000);
    assert_reads(file, 3_000_000 - 10, 10);
    assert_reads(file, 0, 3_000_000);
    // concurrent reads
    std::thread::scope(|scope| {
        for i in 0..50u64 {
            scope.spawn(move || assert_reads(file, i * 58_001, 70_000));
        }
    });
}

fn data_error(error: &anyhow::Error) -> Option<&DownloadError> {
    download_error(error)
}

mod local {
    use super::*;

    fn open(tmp: &Path) -> Arc<dyn RangeFile> {
        let path = tmp.join("file.parquet");
        std::fs::write(&path, contents_3mb()).unwrap();
        range_file::open_local(&path).unwrap()
    }

    #[test]
    fn common() {
        let tmp = tempfile::tempdir().unwrap();
        let file = open(tmp.path());
        common_tests(file.as_ref());
        file.close().unwrap();
    }

    #[test]
    fn local_file_wins_in_every_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let file = open(tmp.path());
        file.close().unwrap();
        let downloader = StubDownloader::new(config());
        downloader.put(URL, &contents_3mb());
        for mode in [DownloadMode::None, DownloadMode::Cached, DownloadMode::Streaming, DownloadMode::Full] {
            let opened =
                range_file::open(&tmp.path().join("file.parquet"), URL, TAIL_BYTES, mode, false, &downloader).unwrap();
            assert!(is::<LocalRangeFile>(&opened), "{mode}");
        }
        assert!(downloader.requests().is_empty());
    }
}

mod http {
    use super::*;

    struct Fixture {
        tmp: tempfile::TempDir,
        downloader: StubDownloader,
        file: Arc<dyn RangeFile>,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let downloader = StubDownloader::new(config());
            downloader.put(URL, &contents_3mb());
            let file = range_file::open(
                &tmp.path().join("file.parquet"),
                URL,
                TAIL_BYTES,
                DownloadMode::Streaming,
                false,
                &downloader,
            )
            .unwrap();
            Fixture { tmp, downloader, file }
        }
    }

    #[test]
    fn common() {
        let f = Fixture::new();
        common_tests(f.file.as_ref());
    }

    #[test]
    fn open_fetches_tail() {
        let f = Fixture::new();
        assert_eq!(vec![request(&format!("bytes=-{TAIL_BYTES}"))], f.downloader.requests());
        assert!(!f.tmp.path().join("file.parquet").exists());
    }

    #[test]
    fn reads_from_tail_make_no_requests() {
        let f = Fixture::new();
        f.downloader.clear_requests();
        assert_reads(f.file.as_ref(), (3_000_000 - TAIL_BYTES) as u64, TAIL_BYTES);
        assert!(f.downloader.requests().is_empty());
    }

    #[test]
    fn splits_large_reads() {
        let f = Fixture::new();
        f.downloader.clear_requests();
        f.file.read(100, 2_500_000).unwrap();
        // split evenly into requests no larger than download_chunk_size_mb
        let mut ranges = ranges(&f.downloader);
        ranges.sort();
        assert_eq!(vec!["bytes=100-833432", "bytes=1666766-2500099", "bytes=833433-1666765"], ranges);
    }

    #[test]
    fn fails_when_file_changes() {
        let f = Fixture::new();
        f.downloader.put_with_validators(URL, &contents_3mb(), Some("\"new\""), None);
        let error = f.file.read(0, 10).unwrap_err();
        assert!(matches!(data_error(&error), Some(DownloadError::RemoteFileChanged { .. })), "{error:#}");
        assert!(error.to_string().contains("--download-fail-if-changed=false"), "{error:#}");
    }

    #[test]
    fn ignores_validators_with_check_for_changes_off() {
        let config = PlanetilerConfig::from(&Arguments::of_pairs([("download_fail_if_changed", "false")])).unwrap();
        let lenient = StubDownloader::new(config);
        lenient.put(URL, &contents_3mb());
        let opened = HttpRangeFile::open(&lenient, URL, 0).unwrap();
        lenient.put_with_validators(URL, &contents_3mb(), Some("\"new\""), Some("new date"));
        assert_eq!(contents_3mb()[..10].to_vec(), opened.read(0, 10).unwrap());
        assert_eq!(request("bytes=0-9"), *lenient.requests().last().unwrap());

        // a different length can't be the same file
        lenient.put(URL, &contents(3_000_001));
        let error = opened.read(0, 10).unwrap_err();
        assert!(matches!(data_error(&error), Some(DownloadError::RemoteFileChanged { .. })), "{error:#}");
    }

    #[test]
    fn counts_fetched_bytes_without_retries() {
        let f = Fixture::new();
        assert_eq!(TAIL_BYTES as u64, f.file.bytes_fetched());
        assert_eq!(1, f.file.requests_made());

        f.file.read(100, 2_500_000).unwrap();
        assert_eq!(TAIL_BYTES as u64 + 2_500_000, f.file.bytes_fetched());
        assert_eq!(4, f.file.requests_made());

        f.downloader.fail_next(1, 503);
        f.file.read(0, 10).unwrap();
        assert_eq!(TAIL_BYTES as u64 + 2_500_010, f.file.bytes_fetched());
        assert_eq!(5, f.file.requests_made());
    }

    #[test]
    fn to_fetch_matches_reads() {
        let f = Fixture::new();
        let tail_start = (3_000_000 - TAIL_BYTES) as u64;
        assert!(!f.file.needs_fetch(tail_start, 100));
        assert_eq!(Plan::EMPTY, f.file.plan_fetch(&[range(tail_start, 100)]));
        assert!(f.file.needs_fetch(tail_start - 5, 10), "reads starting before the tail fetch all of it");
        let plan = f.file.plan_fetch(&[range(100, 2_500_000)]);
        assert_eq!(2_500_000, plan.bytes());
        assert_eq!(3, plan.http_requests.len());

        f.file.read(100, 2_500_000).unwrap();
        assert_eq!(TAIL_BYTES as u64 + 2_500_000, f.file.bytes_fetched());
        assert_eq!(1 + 3, f.file.requests_made());
    }

    #[test]
    fn open_without_tail_fetches_first_byte() {
        let f = Fixture::new();
        f.downloader.clear_requests();
        let opened = HttpRangeFile::open(&f.downloader, URL, 0).unwrap();
        assert_eq!(vec![request("bytes=0-0")], f.downloader.requests());
        assert_eq!(3_000_000, opened.length().unwrap());
        assert!(!opened.needs_fetch(0, 1));
        assert!(opened.needs_fetch(0, 10));
        assert_reads(&opened, 0, 1);
        assert_eq!(1, f.downloader.requests().len());
    }

    #[test]
    fn merges_neighbouring_ranges_into_one_request() {
        let f = Fixture::new();
        f.downloader.clear_requests();
        let reads = f.file.read_later(&[range(100, 100), range(200, 100), range(300, 100)]).unwrap();
        for (i, read) in reads.iter().enumerate() {
            assert_eq!(slice(100 + i * 100, 100), read.get().unwrap());
        }
        assert_eq!(vec!["bytes=100-399"], ranges(&f.downloader));
    }

    #[test]
    fn sends_requests_once_their_first_range_starts() {
        let f = Fixture::new();
        f.downloader.clear_requests();
        let reads = f.file.read_later(&[range(0, 10), range(1_000_000, 10)]).unwrap();
        assert!(ranges(&f.downloader).is_empty(), "nothing gets sent before a range starts");
        assert_eq!(slice(0, 10), reads[0].get().unwrap());
        assert_eq!(vec!["bytes=0-9"], ranges(&f.downloader));
        assert_eq!(slice(1_000_000, 10), reads[1].get().unwrap());
        assert_eq!(vec!["bytes=0-9", "bytes=1000000-1000009"], ranges(&f.downloader));
    }

    #[test]
    fn returns_ranges_before_later_ones_in_the_same_request_start() {
        let config = PlanetilerConfig::from(&Arguments::of_pairs([("download_chunk_size_mb", "10")])).unwrap();
        let larger = StubDownloader::new(config);
        larger.put(URL, &contents_3mb());
        let opened = HttpRangeFile::open(&larger, URL, 0).unwrap();
        larger.clear_requests();
        let fetched_before = opened.bytes_fetched();
        let reads =
            opened.read_later(&[range(100, 600_000), range(600_100, 600_000), range(1_200_100, 600_000)]).unwrap();
        assert_eq!(slice(100, 600_000), reads[0].get().unwrap());
        assert_eq!(vec!["bytes=100-1800099"], ranges(&larger));
        // the request reads 1 MB at a time, and stops in its first MB where the second range starts
        assert_eq!(fetched_before + (1 << 20), opened.bytes_fetched());
        assert_eq!(slice(600_100, 600_000), reads[1].get().unwrap());
        assert_eq!(slice(1_200_100, 600_000), reads[2].get().unwrap());
    }

    #[test]
    fn resumes_mid_request_after_dropped_connection() {
        let f = Fixture::new();
        f.downloader.clear_requests();
        f.downloader.truncate_next(1, 150);
        let reads = f.file.read_later(&[range(1000, 100), range(1100, 100), range(1200, 100)]).unwrap();
        for (i, read) in reads.iter().enumerate() {
            assert_eq!(slice(1000 + i * 100, 100), read.get().unwrap());
        }
        assert_eq!(vec!["bytes=1000-1299", "bytes=1150-1299"], ranges(&f.downloader));
    }

    #[test]
    fn close_fails_reads_not_started() {
        let f = Fixture::new();
        let read = f.file.read_later(&[range(0, 10)]).unwrap().remove(0);
        f.file.close().unwrap();
        let error = read.get().unwrap_err();
        assert!(format!("{error:#}").contains("closed"), "{error:#}");
    }

    #[test]
    fn skips_remaining_requests_after_one_fails() {
        let config = PlanetilerConfig::from(&Arguments::of_pairs([
            ("download_threads", "1"),
            ("http_retries", "0"),
            ("download_chunk_size_mb", "10"),
        ]))
        .unwrap();
        let failing = StubDownloader::new(config);
        failing.put(URL, &contents(40_000_000));
        let opened = HttpRangeFile::open(&failing, URL, 0).unwrap();
        failing.clear_requests();
        failing.fail_next(1, 503);
        assert!(opened.read(0, 40_000_000).is_err());
        // 4 requests planned, one at a time for large requests: the 3 after the failed one never get sent
        assert_eq!(1, failing.requests().len());
    }
}

mod cached {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        path: PathBuf,
        downloader: StubDownloader,
        file: Arc<dyn RangeFile>,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("file.parquet");
            let downloader = StubDownloader::new(config());
            downloader.put(URL, &contents_3mb());
            let file = range_file::open(&path, URL, TAIL_BYTES, DownloadMode::Cached, false, &downloader).unwrap();
            Fixture { _tmp: tmp, path, downloader, file }
        }

        fn reopen(&mut self) {
            self.file.close().unwrap();
            self.downloader.clear_requests();
            self.file =
                range_file::open(&self.path, URL, TAIL_BYTES, DownloadMode::Cached, false, &self.downloader).unwrap();
        }
    }

    #[test]
    fn common() {
        let f = Fixture::new();
        common_tests(f.file.as_ref());
    }

    #[test]
    fn reuses_cached_ranges() {
        let mut f = Fixture::new();
        f.file.read(10, 100).unwrap();
        f.reopen();
        // only checks that the remote file didn't change
        assert_eq!(vec![request("bytes=0-0")], f.downloader.requests());
        assert_reads(f.file.as_ref(), 10, 100);
        assert_reads(f.file.as_ref(), 3_000_000 - 100, 100);
        assert_eq!(1, f.downloader.requests().len());

        assert_reads(f.file.as_ref(), 0, 200);
        assert_eq!(2, f.downloader.requests().len());
    }

    #[test]
    fn starts_over_when_remote_file_changed() {
        let mut f = Fixture::new();
        f.file.read(10, 100).unwrap();
        let mut changed = contents_3mb();
        changed[20] = 1;
        f.downloader.put(URL, &changed);
        f.reopen();
        assert_eq!(changed[10..110].to_vec(), f.file.read(10, 100).unwrap());
        assert_eq!(2, f.downloader.requests().len());
    }

    #[test]
    fn becomes_complete_file() {
        let mut f = Fixture::new();
        f.file.read(0, 3_000_000 - TAIL_BYTES).unwrap();
        assert!(!CachedRangeFile::ranges_path(&f.path).exists());
        assert!(CachedRangeFile::meta_path(&f.path).exists());
        assert_eq!(contents_3mb(), std::fs::read(&f.path).unwrap());

        f.reopen();
        assert!(is::<LocalRangeFile>(&f.file));
        assert!(f.downloader.requests().is_empty());
    }

    #[test]
    fn refresh_replaces_changed_complete_file() {
        let mut f = Fixture::new();
        f.file.read(0, 3_000_000).unwrap();
        f.file.close().unwrap();

        f.file = range_file::open(&f.path, URL, TAIL_BYTES, DownloadMode::Cached, true, &f.downloader).unwrap();
        assert!(is::<LocalRangeFile>(&f.file));
        f.file.close().unwrap();

        f.downloader.put_with_validators(URL, &contents_3mb(), Some("\"new\""), None);
        f.file = range_file::open(&f.path, URL, TAIL_BYTES, DownloadMode::Streaming, true, &f.downloader).unwrap();
        assert!(is::<HttpRangeFile>(&f.file));
        assert!(f.path.exists(), "streaming leaves the local copy alone");

        f.file = range_file::open(&f.path, URL, TAIL_BYTES, DownloadMode::Cached, true, &f.downloader).unwrap();
        assert!(is::<CachedRangeFile>(&f.file));
        assert!(CachedRangeFile::is_partial(&f.path));
    }

    #[test]
    fn reads_offline_from_partial_cache() {
        let mut f = Fixture::new();
        f.file.read(10, 100).unwrap();
        f.file.close().unwrap();
        f.file = range_file::open_local(&f.path).unwrap();
        assert_reads(f.file.as_ref(), 10, 100);
        let error = f.file.read(0, 100).unwrap_err();
        assert!(error.to_string().contains("--download"), "{error:#}");
    }

    #[test]
    fn ignores_truncated_ranges_line() {
        let mut f = Fixture::new();
        f.file.read(10, 100).unwrap();
        f.file.close().unwrap();
        // a crash while appending a line can leave just the start of it
        let mut ranges = std::fs::read_to_string(CachedRangeFile::ranges_path(&f.path)).unwrap();
        ranges.push_str("200");
        std::fs::write(CachedRangeFile::ranges_path(&f.path), ranges).unwrap();
        f.reopen();
        assert_reads(f.file.as_ref(), 10, 100);
        assert_eq!(1, f.downloader.requests().len());
    }

    #[test]
    fn counts_only_bytes_fetched_from_remote() {
        let mut f = Fixture::new();
        f.file.read(10, 100).unwrap();
        f.reopen();
        f.file.read(10, 100).unwrap();
        assert_eq!(1, f.file.bytes_fetched(), "only the validation request");
        assert_eq!(1, f.file.requests_made());
        f.file.read(200, 100).unwrap();
        assert_eq!(101, f.file.bytes_fetched());
        assert_eq!(2, f.file.requests_made());
    }

    #[test]
    fn to_fetch_excludes_cached_ranges() {
        let mut f = Fixture::new();
        f.file.read(10, 100).unwrap();
        assert!(!f.file.needs_fetch(20, 50));
        assert_eq!(Plan::EMPTY, f.file.plan_fetch(&[range(20, 50)]));
        assert!(f.file.needs_fetch(0, 210), "a partly cached read fetches all of it");
        assert_eq!(vec![range(0, 210)], f.file.plan_fetch(&[range(0, 210)]).http_requests);
        assert!(!f.file.needs_fetch(3_000_000 - 10, 10), "the tail is cached when the file is created");
        f.file.close().unwrap();
        f.file = range_file::open_local(&f.path).unwrap();
        assert!(!f.file.needs_fetch(0, 210), "nothing gets fetched offline");
    }

    #[test]
    fn downloads_whole_file_without_range_support() {
        for mode in [DownloadMode::Cached, DownloadMode::Streaming] {
            let mut f = Fixture::new();
            f.file.close().unwrap();
            std::fs::remove_file(&f.path).unwrap();
            std::fs::remove_file(CachedRangeFile::ranges_path(&f.path)).unwrap();
            f.downloader.ignore_ranges();
            std::fs::remove_file(CachedRangeFile::meta_path(&f.path)).unwrap();
            f.file = range_file::open(&f.path, URL, TAIL_BYTES, mode, false, &f.downloader).unwrap();
            assert!(is::<LocalRangeFile>(&f.file), "{mode}");
            assert_reads(f.file.as_ref(), 0, 3_000_000);
            f.file.close().unwrap();
        }
    }

    #[test]
    fn caches_gaps_fetched_between_ranges() {
        let mut f = Fixture::new();
        // 200 KB needed: the 500 byte gap between the ranges gets fetched with them, and cached too
        let reads = f.file.read_later(&[range(0, 100_000), range(100_500, 100_000)]).unwrap();
        reads[0].get().unwrap();
        reads[1].get().unwrap();
        f.reopen();
        assert_reads(f.file.as_ref(), 0, 200_500);
        assert_eq!(1, f.downloader.requests().len(), "only the validation request");
    }

    #[test]
    fn reads_cached_and_missing_ranges_together() {
        let f = Fixture::new();
        f.file.read(10, 100).unwrap();
        f.downloader.clear_requests();
        let reads = f.file.read_later(&[range(10, 100), range(500_000, 100)]).unwrap();
        assert_eq!(slice(10, 100), reads[0].get().unwrap());
        assert_eq!(slice(500_000, 100), reads[1].get().unwrap());
        assert_eq!(vec!["bytes=500000-500099"], ranges(&f.downloader));
    }
}
