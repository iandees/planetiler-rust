//! Port of `BenchmarkPmtiles.java`: round-trips 60M PMTiles directory entries through
//! `directory_to_bytes`/`directory_from_bytes` three times and prints throughput.

use std::time::Instant;

use planetiler_core::pmtiles::pmtiles::{self, Entry};

/// Port of `java.util.Random` so the workload matches Java exactly.
struct JavaRandom {
    seed: u64,
}

impl JavaRandom {
    const MULTIPLIER: u64 = 0x5DEECE66D;
    const MASK: u64 = (1 << 48) - 1;

    fn new(seed: u64) -> JavaRandom {
        JavaRandom { seed: (seed ^ Self::MULTIPLIER) & Self::MASK }
    }

    fn next(&mut self, bits: u32) -> i32 {
        self.seed = (self.seed.wrapping_mul(Self::MULTIPLIER).wrapping_add(0xB)) & Self::MASK;
        (self.seed >> (48 - bits)) as i32
    }

    fn next_int_bound(&mut self, bound: i32) -> i32 {
        let mut r = self.next(31);
        let m = bound - 1;
        if bound & m == 0 {
            return ((bound as i64 * r as i64) >> 31) as i32;
        }
        let mut u = r;
        loop {
            r = u % bound;
            if u.wrapping_sub(r).wrapping_add(m) >= 0 {
                return r;
            }
            u = self.next(31);
        }
    }
}

/// Port of `Format.numeric`: "123", "1.2k", "2.5B", etc.
fn numeric(value: f64) -> String {
    let v = value as i64;
    if v < 0 {
        return "-".into();
    } else if value > 0.0 && value < 1.0 {
        return "<1".into();
    } else if v < 1000 {
        return v.to_string();
    }
    let suffixes = [(1_000_000_000_000i64, "T"), (1_000_000_000, "B"), (1_000_000, "M"), (1_000, "k")];
    let (divide_by, suffix) = suffixes.iter().find(|(d, _)| v >= *d).copied().unwrap_or((1_000, "k"));
    let truncated = v / (divide_by / 10);
    if truncated < 100 && truncated % 10 != 0 {
        format!("{}{suffix}", truncated as f64 / 10.0)
    } else {
        format!("{}{suffix}", truncated / 10)
    }
}

fn main() {
    let num: u64 = 60_000_000;
    let mut random = JavaRandom::new(0);
    for _ in 0..3 {
        let mut entries = Vec::with_capacity(num as usize);
        let mut offset = 0u64;
        for j in 0..num {
            let len = 200 + random.next_int_bound(64000) as u32;
            entries.push(Entry::new(j, offset, len, 1));
            offset += len as u64;
        }
        let timer = Instant::now();
        let result = pmtiles::directory_from_bytes(&pmtiles::directory_to_bytes(&entries)).expect("valid directory");
        assert_eq!(result.len(), entries.len());
        let elapsed = timer.elapsed();
        eprintln!(
            "{num} entries took {} ({}/s)",
            planetiler_core::util::format::duration(elapsed),
            numeric(num as f64 / elapsed.as_secs_f64())
        );
    }
}
