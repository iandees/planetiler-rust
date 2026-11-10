//! Port of `util/RequestPlanner.java`: decides which HTTP range requests to make to fetch parts of one remote file, and
//! how many of them to make at once.
//!
//! The same rules serve sparse reads (row groups of a GeoParquet file in the map bounds) and full downloads (the whole
//! file), so the requests ramp from a few small ones toward a full download as more of a file is needed:
//!
//! 1. `threads_per_file` requests run at once for a whole file. Sparse reads get more, in proportion to how much of
//!    the file they skip, up to 4 times as many: they move a fraction of the file's bytes, so more requests at once
//!    still load the server less than a full download, and they finish sooner on links that cap the speed of each
//!    connection.
//! 2. HTTP requests aim for the bytes needed divided by the requests to run at once, between [`MIN_REQUEST_BYTES`] and
//!    `max_request_bytes`, so there are enough of them to run that many at once.
//! 3. Neighboring small requested ranges share an HTTP request up to that size when the gap between them is at most 1%
//!    of the bytes needed, fetching the gap rather than making another request. Requests start and end on needed
//!    ranges, never in a gap.
//! 4. A range larger than the target request size gets split evenly.

use anyhow::bail;

use crate::util::downloader::ByteRange;

/// Requests don't get split smaller than 2MB since below that, the round trip of each request costs more than parallel
/// requests gain. This is the same as the largest request DuckDB 2 makes to read GeoParquet.
pub const MIN_REQUEST_BYTES: u64 = 2 << 20;
const MAX_SPARSE_THREADS_FACTOR: u64 = 4;
const MAX_GAP_DIVISOR: u64 = 100;

/// Plans the HTTP requests to make for one remote file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestPlanner {
    threads_per_file: usize,
    max_request_bytes: u64,
}

/// The requests to make to fetch some ranges of a file, in file order, and how many of them to make at once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub http_requests: Vec<ByteRange>,
    pub max_in_flight: usize,
}

impl Plan {
    /// A plan that makes no requests.
    pub const EMPTY: Plan = Plan { http_requests: Vec::new(), max_in_flight: 1 };

    /// Returns the number of bytes the requests fetch, including the gaps between needed ranges they span.
    pub fn bytes(&self) -> u64 {
        self.http_requests.iter().map(|r| r.length).sum()
    }

    /// Returns the average number of bytes that requests in flight fetch at once, for memory-budgeting purposes.
    pub fn bytes_in_flight(&self) -> u64 {
        if self.http_requests.is_empty() {
            0
        } else {
            self.bytes() / self.http_requests.len() as u64 * self.max_in_flight.min(self.http_requests.len()) as u64
        }
    }
}

impl RequestPlanner {
    /// `threads_per_file` is the number of requests to make at once for a whole file (`download_threads`) and
    /// `max_request_bytes` the largest request to make (`download_chunk_size_mb`).
    ///
    /// # Panics
    ///
    /// If either is 0.
    pub fn new(threads_per_file: usize, max_request_bytes: u64) -> RequestPlanner {
        assert!(
            threads_per_file >= 1 && max_request_bytes >= 1,
            "threads_per_file and max_request_bytes must be positive"
        );
        RequestPlanner { threads_per_file, max_request_bytes }
    }

    pub fn threads_per_file(&self) -> usize {
        self.threads_per_file
    }

    pub fn max_request_bytes(&self) -> u64 {
        self.max_request_bytes
    }

    /// Returns the requests that fetch `needed` byte ranges of a file that is `file_length` bytes long. The ranges
    /// must be in ascending order, must not overlap and must end within the file. Empty ranges get skipped.
    ///
    /// # Errors
    ///
    /// If `needed` is out of order, overlaps or goes past the end of the file.
    pub fn plan(&self, needed: &[ByteRange], file_length: u64) -> anyhow::Result<Plan> {
        let needed_ranges: Vec<ByteRange> = needed.iter().copied().filter(|r| r.length > 0).collect();
        let (Some(first), Some(last)) = (needed_ranges.first(), needed_ranges.last()) else {
            return Ok(Plan::EMPTY);
        };
        if last.end() > file_length {
            bail!("Ranges must end within the file ({file_length} bytes): {needed:?}");
        }
        let threads = self.threads_per_file as u64;
        let needed_bytes: u64 = needed_ranges.iter().map(|r| r.length).sum();
        let max_gap_bytes = needed_bytes / MAX_GAP_DIVISOR;
        let max_in_flight =
            (threads.saturating_mul(file_length) / needed_bytes).clamp(threads, threads * MAX_SPARSE_THREADS_FACTOR);
        let target_request_bytes = needed_bytes
            .div_ceil(max_in_flight)
            .clamp(MIN_REQUEST_BYTES.min(self.max_request_bytes), self.max_request_bytes);
        let mut http_requests = vec![];
        // the HTTP request being built, from request_start to the end of the last needed range added to it
        let mut request_start = first.offset;
        let mut request_end = request_start;
        for needed_range in &needed_ranges {
            if needed_range.offset < request_end {
                bail!("Ranges must be in ascending order and not overlap: {needed:?}");
            }
            let gap_too_big = needed_range.offset - request_end > max_gap_bytes;
            let request_would_be_too_big =
                needed_range.offset > request_start && needed_range.end() - request_start > target_request_bytes;
            if gap_too_big || request_would_be_too_big {
                // close the request and start a new one
                http_requests.push(ByteRange::new(request_start, request_end - request_start));
                request_start = needed_range.offset;
            }
            if needed_range.length > target_request_bytes {
                // range too big, split into ~even pieces no smaller than the target
                let pieces = (needed_range.length / target_request_bytes)
                    .max(needed_range.length.div_ceil(self.max_request_bytes));
                for i in 1..pieces {
                    let piece_end =
                        needed_range.offset + (needed_range.length as u128 * i as u128 / pieces as u128) as u64;
                    http_requests.push(ByteRange::new(request_start, piece_end - request_start));
                    request_start = piece_end;
                }
            }
            request_end = needed_range.end();
        }
        http_requests.push(ByteRange::new(request_start, request_end - request_start));
        Ok(Plan { http_requests, max_in_flight: max_in_flight as usize })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::planetiler_config::PlanetilerConfig;

    const MB: u64 = 1_000_000;
    const ROW_GROUP: u64 = 2_400_000;

    fn defaults() -> RequestPlanner {
        let config = PlanetilerConfig::defaults();
        RequestPlanner::new(config.download_threads, config.download_chunk_size_mb * MB)
    }

    fn range(offset: u64, length: u64) -> ByteRange {
        ByteRange::new(offset, length)
    }

    fn row_groups(offset: u64, size: u64, count: u64) -> Vec<ByteRange> {
        (0..count).map(|i| range(offset + i * size, size)).collect()
    }

    fn concat(a: Vec<ByteRange>, b: Vec<ByteRange>) -> Vec<ByteRange> {
        a.into_iter().chain(b).collect()
    }

    fn lengths(plan: &Plan) -> Vec<u64> {
        plan.http_requests.iter().map(|r| r.length).collect()
    }

    fn is_needed(needed: &[ByteRange], position: u64) -> bool {
        needed.iter().any(|r| position >= r.offset && position < r.end())
    }

    fn plan(planner: &RequestPlanner, needed: &[ByteRange]) -> Plan {
        plan_in(planner, needed, needed.last().unwrap().end())
    }

    /// Plans `needed` and checks the invariants every plan must hold.
    fn plan_in(planner: &RequestPlanner, needed: &[ByteRange], file_length: u64) -> Plan {
        let plan = planner.plan(needed, file_length).unwrap();
        let non_empty: Vec<ByteRange> = needed.iter().copied().filter(|r| r.length > 0).collect();
        let mut end = 0;
        for request in &plan.http_requests {
            assert!(request.offset >= end, "requests out of order or overlapping: {plan:?}");
            assert!(request.length > 0 && request.length <= planner.max_request_bytes, "bad size: {request:?}");
            assert!(is_needed(&non_empty, request.offset), "starts in a gap: {request:?}");
            assert!(is_needed(&non_empty, request.end() - 1), "ends in a gap: {request:?}");
            end = request.end();
        }
        for range in &non_empty {
            let covered: u64 = plan
                .http_requests
                .iter()
                .map(|r| r.end().min(range.end()).saturating_sub(r.offset.max(range.offset)))
                .sum();
            assert_eq!(range.length, covered, "not fetched: {range:?}");
        }
        assert!(plan.max_in_flight >= 1);
        plan
    }

    #[test]
    fn monaco_osm_pbf() {
        let plan = plan(&defaults(), &[range(0, 600_000)]);
        assert_eq!(vec![range(0, 600_000)], plan.http_requests);
    }

    #[test]
    fn massachusetts_osm_pbf() {
        let plan = plan(&defaults(), &[range(0, 310 * MB)]);
        assert_eq!(vec![77_500_000; 4], lengths(&plan));
        assert_eq!(4, plan.max_in_flight);
    }

    #[test]
    fn switzerland_osm_pbf() {
        let plan = plan(&defaults(), &[range(0, 480 * MB)]);
        assert_eq!(vec![96 * MB; 5], lengths(&plan));
        assert_eq!(4, plan.max_in_flight);
    }

    #[test]
    fn planet_osm_pbf() {
        let whole = [range(0, 95_600 * MB)];
        let p = plan(&defaults(), &whole);
        assert_eq!(956, p.http_requests.len());
        assert!(p.http_requests.iter().all(|r| r.length == 100 * MB));
        assert_eq!(4, p.max_in_flight);

        // raising download_threads is how big machines get more requests at once, with the same requests
        let faster = plan(&RequestPlanner::new(64, 100 * MB), &whole);
        assert_eq!(p.http_requests, faster.http_requests);
        assert_eq!(64, faster.max_in_flight);
    }

    #[test]
    fn whole_water_parquet() {
        let p = plan(&defaults(), &[range(0, 1_000 * MB)]);
        assert_eq!(10, p.http_requests.len());
        assert_eq!(4, p.max_in_flight);
    }

    #[test]
    fn water_parquet_in_small_bounds() {
        // 5 clusters of 4 row groups of 512 KB, far apart: one request per cluster, many at once since each is small
        let mut needed = vec![];
        for cluster in 0..5 {
            needed.extend(row_groups(cluster * 100 * MB, 512_000, 4));
        }
        let p = plan(&defaults(), &needed);
        assert_eq!(vec![2_048_000; 5], lengths(&p));
        assert_eq!(16, p.max_in_flight);
    }

    #[test]
    fn few_row_groups_in_small_bounds() {
        // like Monaco buildings: 2 neighbouring row groups of a large file get split into a request each to fetch them
        // at once
        let p = plan_in(&defaults(), &row_groups(50 * MB, ROW_GROUP, 2), 225 * ROW_GROUP);
        assert_eq!(row_groups(50 * MB, ROW_GROUP, 2), p.http_requests);
        assert_eq!(16, p.max_in_flight);
    }

    #[test]
    fn row_groups_in_state_bounds() {
        // like Massachusetts buildings in one file: a few clusters of neighbouring row groups, a fifth of the file,
        // split into requests of about 1/16 of the 108 MB needed to run 16 at once
        let needed = concat(
            concat(row_groups(0, ROW_GROUP, 10), row_groups(200 * MB, ROW_GROUP, 20)),
            row_groups(400 * MB, ROW_GROUP, 15),
        );
        let p = plan_in(&defaults(), &needed, 225 * ROW_GROUP);
        assert_eq!(23, p.http_requests.len());
        assert!(p.http_requests.iter().all(|r| r.length <= 2 * ROW_GROUP), "{p:?}");
        assert_eq!(16, p.max_in_flight);
    }

    #[test]
    fn whole_file_of_row_groups_is_like_full_download() {
        // planet bounds need every row group of every file, which gets fetched like a full download of the file
        let file_bytes = 225 * ROW_GROUP;
        let cached = plan(&defaults(), &row_groups(0, ROW_GROUP, 225));
        let full = plan(&defaults(), &[range(0, file_bytes)]);
        assert_eq!(6, cached.http_requests.len());
        assert_eq!(full.http_requests.len(), cached.http_requests.len());
        assert_eq!(full.max_in_flight, cached.max_in_flight);
        // split between row groups, not inside them like a full download
        assert!(cached.http_requests.iter().all(|r| r.length % ROW_GROUP == 0), "{cached:?}");
    }

    #[test]
    fn half_a_file_of_row_groups() {
        let p = plan_in(&defaults(), &row_groups(0, ROW_GROUP, 112), 225 * ROW_GROUP);
        assert_eq!(vec![14 * ROW_GROUP; 8], lengths(&p));
        assert_eq!(8, p.max_in_flight);
    }

    #[test]
    fn most_of_a_file_of_row_groups() {
        // skipping a few row groups leaves requests as large as a full download's, so no more of them at once
        let needed: Vec<ByteRange> =
            row_groups(0, ROW_GROUP, 225).into_iter().filter(|r| r.offset % (10 * ROW_GROUP) != 0).collect();
        let p = plan(&defaults(), &needed);
        assert_eq!(6, p.http_requests.len());
        assert_eq!(4, p.max_in_flight);
    }

    #[test]
    fn merges_gaps_up_to_one_percent_of_bytes_needed() {
        // 2 MB needed: gaps up to 20 KB get fetched instead of making another request
        let merged = plan(&defaults(), &[range(0, MB), range(MB + 20_000, MB)]);
        assert_eq!(vec![range(0, 2 * MB + 20_000)], merged.http_requests);
        assert_eq!(2 * MB + 20_000, merged.bytes());

        let separate = plan(&defaults(), &[range(0, MB), range(MB + 20_001, MB)]);
        assert_eq!(vec![range(0, MB), range(MB + 20_001, MB)], separate.http_requests);
    }

    #[test]
    fn splits_between_ranges_instead_of_fetching_gaps() {
        // the gap merges the ranges into one span, but splitting it into 4 requests cuts at the gap instead of
        // fetching it
        let p = plan(&defaults(), &[range(0, 50 * MB), range(50 * MB + 500_000, 50 * MB)]);
        assert_eq!(100 * MB, p.bytes());
    }

    #[test]
    fn splits_ranges_larger_than_a_request() {
        let needed = concat(
            concat(row_groups(0, ROW_GROUP, 3), vec![range(3 * ROW_GROUP, 250 * MB)]),
            row_groups(3 * ROW_GROUP + 250 * MB, ROW_GROUP, 3),
        );
        let p = plan(&defaults(), &needed);
        assert!(p.http_requests.len() >= 3, "{p:?}");
    }

    #[test]
    fn small_max_request_bytes() {
        let p = plan(&RequestPlanner::new(4, MB), &[range(100, 2_500_000)]);
        assert_eq!(vec![range(100, 833_333), range(833_433, 833_333), range(1_666_766, 833_334)], p.http_requests);
    }

    #[test]
    fn threads_scale_sparse_reads() {
        let scattered: Vec<ByteRange> = (0..100).map(|i| range(i * 100 * MB, 512 << 10)).collect();
        assert_eq!(16, plan(&defaults(), &scattered).max_in_flight);
        assert_eq!(4, plan(&RequestPlanner::new(1, 100 * MB), &scattered).max_in_flight);
        assert_eq!(64, plan(&RequestPlanner::new(16, 100 * MB), &scattered).max_in_flight);
        assert_eq!(256, plan(&RequestPlanner::new(64, 100 * MB), &scattered).max_in_flight);
        assert_eq!(100, plan(&defaults(), &scattered).http_requests.len());
    }

    #[test]
    fn nothing_needed() {
        assert_eq!(Plan::EMPTY, defaults().plan(&[], 100).unwrap());
        assert_eq!(Plan::EMPTY, defaults().plan(&[range(10, 0), range(20, 0)], 100).unwrap());
        assert_eq!(0, Plan::EMPTY.bytes());
        assert_eq!(0, Plan::EMPTY.bytes_in_flight());
    }

    #[test]
    fn skips_empty_ranges() {
        let p = plan(&defaults(), &[range(0, 0), range(10, 100), range(500, 0)]);
        assert_eq!(vec![range(10, 100)], p.http_requests);
    }

    #[test]
    fn bytes_in_flight() {
        let p = defaults().plan(&[range(0, 310 * MB)], 310 * MB).unwrap();
        assert_eq!(310 * MB, p.bytes());
        assert_eq!(310 * MB, p.bytes_in_flight());
        let planet = defaults().plan(&[range(0, 95_600 * MB)], 95_600 * MB).unwrap();
        assert_eq!(400 * MB, planet.bytes_in_flight());
    }

    #[test]
    fn rejects_ranges_out_of_order_overlapping_or_past_the_end() {
        assert!(defaults().plan(&[range(100, 10), range(0, 10)], 1_000).is_err());
        assert!(defaults().plan(&[range(0, 10), range(5, 10)], 1_000).is_err());
        assert!(defaults().plan(&[range(0, 10), range(95, 10)], 100).is_err());
    }

    #[test]
    #[should_panic(expected = "must be positive")]
    fn rejects_zero_threads() {
        RequestPlanner::new(0, MB);
    }

    #[test]
    #[should_panic(expected = "must be positive")]
    fn rejects_zero_request_bytes() {
        RequestPlanner::new(4, 0);
    }
}
