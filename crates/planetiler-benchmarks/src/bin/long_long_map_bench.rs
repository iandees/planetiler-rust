//! Port of `planetiler-benchmarks/.../LongLongMapBench.java`: performance tests for `LongLongMap` implementations.
//!
//! Adds items to the map then reads from it for 30s using multiple threads and reports memory/disk usage, writes and
//! reads per second.
//!
//! Usage: `long_long_map_bench <type> <storage> <entries> <readers> [madvise=true]`, for example
//! `cargo run --release -p planetiler-benchmarks --bin long_long_map_bench -- sparsearray mmap 500000000 8`.
//!
//! The output lines (`Loaded ...`, `Storage: ...`, `RAM: ...`, `Read ...` and the final tab-separated summary) match
//! the Java benchmark so results can be compared directly. Periodic progress lines are a simplified version of
//! Java's `ProgressLoggers` output.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use planetiler_core::collection::long_long_map::{self, LongLongMap};

/// Java `Format.numeric`/`Format.storage` (unpadded).
fn format_with_suffixes(value: f64, suffixes: &[(u64, &str)]) -> String {
    if value < 0.0 {
        return "-".into();
    } else if value > 0.0 && value < 1.0 {
        return "<1".into();
    }
    let value = value as u64;
    if value < 1000 {
        return value.to_string();
    }
    let (divide_by, suffix) = suffixes.iter().rev().find(|(d, _)| *d <= value).copied().unwrap();
    let truncated = value / (divide_by / 10);
    let has_decimal = truncated < 100 && !truncated.is_multiple_of(10);
    if has_decimal { format!("{:.1}{suffix}", truncated as f64 / 10.0) } else { format!("{}{suffix}", truncated / 10) }
}

fn numeric(value: f64) -> String {
    format_with_suffixes(
        value,
        &[(1_000, "k"), (1_000_000, "M"), (1_000_000_000, "B"), (1_000_000_000_000, "T"), (1_000_000_000_000_000, "Q")],
    )
}

fn storage(value: u64) -> String {
    format_with_suffixes(
        value as f64,
        &[(1_000, "k"), (1_000_000, "M"), (1_000_000_000, "G"), (1_000_000_000_000, "T"), (1_000_000_000_000_000, "P")],
    )
}

fn size(path: &Path) -> u64 {
    planetiler_core::util::file_utils::size(path).unwrap_or(0)
}

fn delete(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Process memory/cpu stats for progress lines.
fn process_stats(start: Instant) -> String {
    let cpu = planetiler_core::stats::process_info::get_process_cpu_time()
        .map(|c| format!("{:.1}", c.as_secs_f64() / start.elapsed().as_secs_f64().max(1e-9)))
        .unwrap_or_else(|| "-".into());
    format!("cpus: {cpu}")
}

/// Per-reader counter padded to its own cache line to avoid false sharing.
#[repr(align(128))]
#[derive(Default)]
struct PaddedCounter(AtomicU64);

/// xorshift64* PRNG (Java uses `java.util.Random`; the key distribution is the same: uniform in `[1, entries]`).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng((seed + 1).wrapping_mul(0x9E3779B97F4A7C15))
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!("usage: long_long_map_bench <type> <storage> <entries> <readers> [madvise=true]");
        std::process::exit(1);
    }
    let path = Path::new("./llmaptest");
    delete(path);
    let madvise = args.len() < 5 || args[4].eq_ignore_ascii_case("true");
    let map: Arc<dyn LongLongMap> = Arc::from(long_long_map::from_config(&args[0], &args[1], path, madvise)?);
    let entries: u64 = args[2].parse()?;
    let readers: usize = args[3].parse()?;
    let process_start = Instant::now();

    // write phase
    let counter = AtomicU64::new(0);
    let done = AtomicBool::new(false);
    let write_rate = std::thread::scope(|s| -> anyhow::Result<String> {
        let writer =
            std::thread::Builder::new().name("writer".into()).spawn_scoped(s, || -> anyhow::Result<String> {
                let mut writer = map.new_writer();
                let start = Instant::now();
                for i in 0..entries as i64 {
                    writer.put(i + 1, i + 2)?;
                    counter.store(i as u64, Ordering::Relaxed);
                }
                let elapsed = start.elapsed();
                drop(writer);
                let rate =
                    format!("{}/s", numeric((entries as u128 * 1_000_000_000 / elapsed.as_nanos().max(1)) as f64));
                eprintln!("Loaded {entries} in {}s ({rate})", elapsed.as_secs());
                done.store(true, Ordering::Release);
                Ok(rate)
            })?;
        let mut last = Instant::now();
        let mut last_count = 0;
        while !done.load(Ordering::Acquire) {
            std::thread::park_timeout(Duration::from_millis(50));
            if last.elapsed() >= Duration::from_secs(10) && !done.load(Ordering::Acquire) {
                let count = counter.load(Ordering::Relaxed);
                let rate = (count - last_count) as f64 / last.elapsed().as_secs_f64();
                eprintln!(
                    "[writer] entries: [ {} {:.0}% {}/s ] {} | {}",
                    numeric(count as f64),
                    100.0 * count as f64 / entries.max(1) as f64,
                    numeric(rate),
                    storage(map.disk_usage_bytes()),
                    process_stats(process_start)
                );
                last = Instant::now();
                last_count = count;
            }
        }
        writer.join().expect("writer thread panicked")
    })?;

    map.get(1);
    eprintln!("Storage: {}", storage(map.disk_usage_bytes()));
    eprintln!("RAM: {}", storage(map.estimate_memory_usage_bytes()));

    // read phase
    let counters: Arc<Vec<PaddedCounter>> = Arc::new((0..readers).map(|_| PaddedCounter::default()).collect());
    let barrier = Arc::new(Barrier::new(readers + 1));
    for rnum in 0..readers {
        let map = map.clone();
        let counters = counters.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            let mut random = Rng::new(rnum as u64);
            let counter = &counters[rnum].0;
            let mut sum: i64 = 0;
            loop {
                counter.fetch_add(1, Ordering::Relaxed);
                let key = 1 + (random.next_u64() >> 1) % entries;
                let value = map.get(key as i64);
                debug_assert_eq!(key as i64 + 1, value, "{key} value was {value}");
                sum = sum.wrapping_add(value);
                std::hint::black_box(sum);
            }
        });
    }
    let total = || counters.iter().map(|c| c.0.load(Ordering::Relaxed)).sum::<u64>();
    barrier.wait();
    let start = Instant::now();
    let mut last = start;
    let mut last_count = 0;
    for _ in 0..3 {
        std::thread::sleep(Duration::from_secs(10));
        let count = total();
        let rate = (count - last_count) as f64 / last.elapsed().as_secs_f64();
        eprintln!(
            "[reader] entries: [ {} {}/s ] | {}",
            numeric(count as f64),
            numeric(rate),
            process_stats(process_start)
        );
        last = Instant::now();
        last_count = count;
    }
    let elapsed = start.elapsed();
    let read = total();
    let read_rate = format!("{}/s", numeric((read as u128 * 1_000_000_000 / elapsed.as_nanos().max(1)) as f64));
    eprintln!("Read {read} in 30s ({read_rate})");
    eprintln!(
        "{}",
        [
            args[0].as_str(),
            args[1].as_str(),
            args[2].as_str(),
            args[3].as_str(),
            &storage(map.estimate_memory_usage_bytes()),
            &storage(map.disk_usage_bytes()),
            &storage(size(path)),
            &write_rate,
            &read_rate,
        ]
        .join("\t")
    );
    delete(path);
    std::thread::sleep(Duration::from_millis(100));
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format() {
        assert_eq!("999", numeric(999.0));
        assert_eq!("1k", numeric(1000.0));
        assert_eq!("1.5k", numeric(1500.0));
        assert_eq!("15k", numeric(15_500.0));
        assert_eq!("2.3B", numeric(2_300_000_000.0));
        assert_eq!("4.5G", storage(4_500_000_000));
    }
}
