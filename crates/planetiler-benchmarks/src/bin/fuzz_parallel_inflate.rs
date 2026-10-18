//! Randomized differential test of `util::parallel_inflate::ParallelInflater` (and `ZipEntryReader`) against zlib-rs
//! (through flate2) and libdeflate. Runs on stable Rust; build it with debug assertions and overflow checks on:
//!
//! ```sh
//! cargo run --profile release-checked -p planetiler-benchmarks --bin fuzz_parallel_inflate -- --seconds=600
//! ```
//!
//! Every case is generated from `(seed, case index)`, so a failure prints the index to rerun with `--case=N`. Cases:
//! - `own`: valid streams from a randomized deflate encoder in this file (stored/fixed/dynamic blocks, random complete
//!   and single-code Huffman codes up to 15 bits, code-length repeats across the literal/distance boundary, matches
//!   of every length and distance 1..=32768, empty blocks, many tiny blocks, rarely one huge 20-80 MB block)
//! - `flate2`: zlib-rs levels 0-9 on generated data, flushed (sync/partial/full) at random points
//! - `libdeflate`: levels 0-12
//! - `invalid`: `own` streams with one deliberately invalid element (bad code lengths, too-far distance, ...)
//! - `mutated`: valid streams with bit flips, overwritten/inserted/deleted bytes, truncation, splices
//! - `random`: random bytes
//! - rarely, 50-200 MB of data compressed by flate2/libdeflate
//!
//! Each stream is decoded with random thread counts, chunk sizes (256 B - 2 MB; tiny chunks force many chunk
//! boundaries, fallbacks and mid-block pauses) and read sizes, and some cases go through a zip entry (both
//! `ZipEntryReader` paths, with the CRC-32 and size of the intended output). The checks:
//! - if zlib-rs decodes the stream, `ParallelInflater` returns exactly the same bytes (and libdeflate agrees);
//! - if zlib-rs rejects it, `ParallelInflater` returns an error, everything it returned before is a prefix of zlib's
//!   partial output, and the error is sticky;
//! - through a zip entry, the reader returns the intended bytes or an error, never different bytes;
//! - no panics (a panic hook aborts, including panics inside worker threads that the inflater would otherwise turn
//!   into fallbacks) and no hangs (a watchdog aborts cases that run too long).
//!
//! Arguments (`--key=value`): `seconds` (default 60), `seed` (default 1), `start` (first case index, default 0),
//! `case` (run only this case, verbose), `runners` (cases in parallel, default 4), `max_large` (number of 50-200 MB
//! cases, default unlimited), `tmp` (directory for zip files, default a temporary directory).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use planetiler_core::util::parallel_inflate::ParallelInflater;
use planetiler_core::util::zip_entry_reader::ZipEntryReader;

// ---------------------------------------------------------------------------------------------------------------
// PRNG

#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        let mut rng = Rng(seed ^ 0x9e37_79b9_7f4a_7c15);
        rng.next();
        rng
    }

    /// SplitMix64.
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    fn below(&mut self, n: usize) -> usize {
        ((self.next() as u128 * n as u128) >> 64) as usize
    }

    /// Uniform in `lo..=hi`.
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }

    /// Roughly log-uniform in `lo..=hi`.
    fn log_range(&mut self, lo: usize, hi: usize) -> usize {
        let (a, b) = (((lo.max(1)) as f64).ln(), (hi.max(1) as f64).ln());
        let x = a + (b - a) * ((self.next() >> 11) as f64 / (1u64 << 53) as f64);
        (x.exp() as usize).clamp(lo, hi)
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// test data

/// Data for real compressors: a mix of segments with different statistics.
fn gen_data(rng: &mut Rng, n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    let style = rng.below(8);
    let mut x = 0f64;
    while out.len() < n {
        let left = n - out.len();
        let seg = rng.log_range(1, left.min(1 << 20)).min(left);
        let kind = if style < 6 { style } else { rng.below(6) };
        match kind {
            // random bytes (incompressible)
            0 => out.extend((0..seg).map(|_| rng.next() as u8)),
            // small alphabet
            1 => {
                let k = rng.range(1, 16) as u64;
                let base = rng.next() as u8;
                out.extend((0..seg).map(|_| base.wrapping_add((rng.next() % k) as u8)));
            }
            // runs
            2 => {
                let mut left = seg;
                while left > 0 {
                    let run = rng.log_range(1, 5000).min(left);
                    out.extend(std::iter::repeat_n(rng.next() as u8, run));
                    left -= run;
                }
            }
            // periodic with a random period (distances up to and past 32 KB)
            3 => {
                let period = rng.log_range(1, 40_000);
                let pattern: Vec<u8> = (0..period).map(|_| rng.next() as u8).collect();
                for i in 0..seg {
                    let b = pattern[i % period];
                    out.push(if rng.chance(0.001) { b ^ 1 } else { b });
                }
            }
            // text
            4 => {
                const WORDS: [&str; 12] =
                    ["the ", "quick ", "brown ", "fox ", "jumps ", "over ", "lazy ", "dog ", "\n", "water ", "{", "}"];
                let start = out.len();
                while out.len() - start < seg {
                    out.extend_from_slice(WORDS[rng.below(WORDS.len())].as_bytes());
                }
                out.truncate(start + seg);
            }
            // coordinates on a random walk (like a shapefile)
            _ => {
                let start = out.len();
                while out.len() - start < seg {
                    x += ((rng.next() >> 11) as f64 / (1u64 << 53) as f64) - 0.5;
                    out.extend_from_slice(&x.to_le_bytes());
                }
                out.truncate(start + seg);
            }
        }
    }
    out.truncate(n);
    out
}

// ---------------------------------------------------------------------------------------------------------------
// a randomized deflate encoder

const LEN_BASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    buf: u64,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, v: u32, bits: u32) {
        debug_assert!(bits <= 32);
        self.buf |= ((v as u64) & ((1u64 << bits) - 1)) << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.out.push(self.buf as u8);
            self.buf >>= 8;
            self.n -= 8;
        }
    }

    /// Writes a Huffman code (most significant bit first).
    fn code(&mut self, code: (u32, u32)) {
        let (c, len) = code;
        self.put(c.reverse_bits() >> (32 - len), len);
    }

    fn align(&mut self) {
        if self.n > 0 {
            self.put(0, 8 - self.n);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}

/// Canonical codes for `lens` (`(code, len)`, len 0 = unused).
fn canonical(lens: &[u8]) -> Vec<(u32, u32)> {
    let mut count = [0u32; 16];
    for &l in lens {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut next = [0u32; 16];
    let mut code = 0;
    for len in 1..16 {
        code = (code + count[len - 1]) << 1;
        next[len] = code;
    }
    lens.iter()
        .map(|&l| {
            if l == 0 {
                (0, 0)
            } else {
                let c = next[l as usize];
                next[l as usize] += 1;
                (c, l as u32)
            }
        })
        .collect()
}

/// Random complete prefix code lengths for `k` symbols (a single symbol gets length 1), at most `max_len` bits.
fn random_lengths(rng: &mut Rng, k: usize, max_len: u8) -> Vec<u8> {
    match k {
        0 => return vec![],
        1 => return vec![1],
        _ => {}
    }
    assert!(k <= 1 << max_len);
    let mut leaves = vec![0u8];
    // balanced-ish (split a random leaf), skewed (split the deepest) or mixed trees
    let skew = rng.below(3);
    while leaves.len() < k {
        let candidates: Vec<usize> = (0..leaves.len()).filter(|&i| leaves[i] < max_len).collect();
        let i = match skew {
            0 => candidates[rng.below(candidates.len())],
            1 => *candidates.iter().max_by_key(|&&i| leaves[i]).unwrap(),
            _ if rng.chance(0.5) => *candidates.iter().max_by_key(|&&i| leaves[i]).unwrap(),
            _ => candidates[rng.below(candidates.len())],
        };
        let d = leaves[i] + 1;
        leaves[i] = d;
        leaves.push(d);
    }
    rng.shuffle(&mut leaves);
    leaves
}

/// Lengths for the symbols in `used` (indices into `0..n`); others get 0.
fn assign_lengths(rng: &mut Rng, used: &[usize], n: usize, max_len: u8) -> Vec<u8> {
    let lens = random_lengths(rng, used.len(), max_len);
    let mut out = vec![0u8; n];
    for (&sym, &l) in used.iter().zip(&lens) {
        out[sym] = l;
    }
    out
}

#[derive(Clone, Copy)]
enum Op {
    Lit(u8),
    /// `(length, distance, encode 258 as symbol 284 + 31)`
    Match(u16, u16, bool),
}

fn len_code(len: u16) -> (usize, u32) {
    let i = LEN_BASE.iter().rposition(|&b| b <= len).unwrap();
    (257 + i, (len - LEN_BASE[i]) as u32)
}

fn dist_code(dist: u16) -> (usize, u32) {
    let i = DIST_BASE.iter().rposition(|&b| b <= dist).unwrap();
    (i, (dist - DIST_BASE[i]) as u32)
}

/// Things [`Encoder`] can get deliberately wrong (for `invalid` cases).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Defect {
    None,
    /// A distance one past the available history.
    TooFar,
    /// Over-subscribed literal/length code.
    OverSubscribed,
    /// Incomplete literal/length code with more than one symbol.
    Incomplete,
    /// HLIT = 30 or 31.
    TooManyLengths,
    /// HDIST = 30 or 31.
    TooManyDistances,
    /// Code length repeat (16) as the first code length.
    RepeatFirst,
    /// Code length run past HLIT + HDIST.
    RepeatPastEnd,
    /// Stored block with LEN != ~NLEN.
    StoredLen,
    /// Block type 3.
    BadType,
    /// No end-of-block code in the literal/length code.
    NoEob,
    /// Fixed block using literal/length symbol 286/287 or distance symbol 30/31.
    FixedReserved,
    /// The stream ends without a final block.
    NoFinal,
    /// Incomplete code length code.
    IncompleteCl,
}

const DEFECTS: [Defect; 13] = [
    Defect::TooFar,
    Defect::OverSubscribed,
    Defect::Incomplete,
    Defect::TooManyLengths,
    Defect::TooManyDistances,
    Defect::RepeatFirst,
    Defect::RepeatPastEnd,
    Defect::StoredLen,
    Defect::BadType,
    Defect::NoEob,
    Defect::FixedReserved,
    Defect::NoFinal,
    Defect::IncompleteCl,
];

struct Encoder<'a> {
    rng: &'a mut Rng,
    w: BitWriter,
    /// Everything the stream decodes to so far.
    plain: Vec<u8>,
    /// Literal alphabet for this stream.
    alphabet: Vec<u8>,
    /// Probability that a symbol is a match.
    match_p: f64,
    defect: Defect,
    /// Whether the defect has been written yet.
    defect_done: bool,
}

impl Encoder<'_> {
    fn literal(&mut self) -> u8 {
        self.alphabet[self.rng.below(self.alphabet.len())]
    }

    /// Random symbols producing about `target` bytes.
    fn ops(&mut self, target: usize) -> Vec<Op> {
        let mut ops = vec![];
        let mut produced = 0;
        let mut history = self.plain.len();
        while produced < target {
            if history > 0 && self.rng.chance(self.match_p) {
                let hist = history.min(32768);
                let dist = match self.rng.below(7) {
                    0 => self.rng.range(1, hist.min(16)),
                    1 => self.rng.range(1, hist.min(300)),
                    2 => hist,
                    3 => self.rng.log_range(1, hist),
                    4 => 1,
                    _ => self.rng.range(1, hist),
                };
                let len = match self.rng.below(5) {
                    0 => 258,
                    1 => self.rng.range(3, 10),
                    _ => self.rng.range(3, 258),
                };
                let dist = if self.defect == Defect::TooFar && !self.defect_done && self.rng.chance(0.05) {
                    self.defect_done = true;
                    hist + 1
                } else {
                    dist
                };
                let alt = len == 258 && self.rng.chance(0.2);
                ops.push(Op::Match(len as u16, dist.min(32768) as u16, alt));
                produced += len;
                history += len;
            } else {
                ops.push(Op::Lit(self.literal()));
                produced += 1;
                history += 1;
            }
        }
        ops
    }

    fn apply(&mut self, ops: &[Op]) {
        for &op in ops {
            match op {
                Op::Lit(b) => self.plain.push(b),
                Op::Match(len, dist, _) => {
                    let start = self.plain.len().saturating_sub(dist as usize);
                    for i in 0..len as usize {
                        let b = self.plain.get(start + i).copied().unwrap_or(0);
                        self.plain.push(b);
                    }
                }
            }
        }
    }

    fn header(&mut self, last: bool, btype: u32) {
        let last = last && !(self.defect == Defect::NoFinal);
        self.w.put(last as u32, 1);
        self.w.put(btype, 2);
    }

    fn take_defect(&mut self, d: Defect) -> bool {
        if self.defect == d && !self.defect_done && self.rng.chance(0.3) {
            self.defect_done = true;
            true
        } else {
            false
        }
    }

    fn stored(&mut self, last: bool, len: usize) {
        if self.take_defect(Defect::BadType) {
            self.header(last, 3);
            return;
        }
        self.header(last, 0);
        self.w.align();
        let mut nlen = !len as u32 & 0xffff;
        if self.take_defect(Defect::StoredLen) {
            nlen ^= 1 << self.rng.below(16);
        }
        self.w.put(len as u32, 16);
        self.w.put(nlen, 16);
        let random = self.rng.chance(0.5);
        for _ in 0..len {
            let b = if random { self.rng.next() as u8 } else { self.literal() };
            self.w.put(b as u32, 8);
            self.plain.push(b);
        }
    }

    fn symbols(&mut self, ops: &[Op], litlen: &[(u32, u32)], dist: &[(u32, u32)]) {
        for &op in ops {
            match op {
                Op::Lit(b) => self.w.code(litlen[b as usize]),
                Op::Match(len, d, alt) => {
                    let (sym, extra) = if alt { (284, 31) } else { len_code(len) };
                    self.w.code(litlen[sym]);
                    self.w.put(extra, LEN_EXTRA[sym - 257] as u32);
                    let (dsym, dextra) = dist_code(d);
                    self.w.code(dist[dsym]);
                    self.w.put(dextra, DIST_EXTRA[dsym] as u32);
                }
            }
        }
        self.w.code(litlen[256]);
    }

    fn fixed(&mut self, last: bool, target: usize) {
        self.header(last, 1);
        let mut lens = [0u8; 288];
        lens[..144].fill(8);
        lens[144..256].fill(9);
        lens[256..280].fill(7);
        lens[280..].fill(8);
        let litlen = canonical(&lens);
        let dist = canonical(&[5; 32]);
        let ops = self.ops(target);
        if self.take_defect(Defect::FixedReserved) {
            if self.rng.chance(0.5) {
                self.w.code(litlen[286 + self.rng.below(2)]);
            } else {
                self.w.code(litlen[257]);
                self.w.code(dist[30 + self.rng.below(2)]);
            }
        }
        self.symbols(&ops, &litlen, &dist);
        self.apply(&ops);
    }

    fn dynamic(&mut self, last: bool, target: usize) {
        if self.take_defect(Defect::BadType) {
            self.header(last, 3);
            return;
        }
        self.header(last, 2);
        let ops = self.ops(target);
        let mut lit_used = [false; 286];
        let mut dist_used = [false; 30];
        lit_used[256] = true;
        for &op in &ops {
            match op {
                Op::Lit(b) => lit_used[b as usize] = true,
                Op::Match(len, d, alt) => {
                    lit_used[if alt { 284 } else { len_code(len).0 }] = true;
                    dist_used[dist_code(d).0] = true;
                }
            }
        }
        // sometimes give unused symbols codes too
        if self.rng.chance(0.3) {
            for _ in 0..self.rng.below(40) {
                lit_used[self.rng.below(286)] = true;
            }
            for _ in 0..self.rng.below(5) {
                dist_used[self.rng.below(30)] = true;
            }
        }
        let lit_syms: Vec<usize> = (0..286).filter(|&s| lit_used[s]).collect();
        let dist_syms: Vec<usize> = (0..30).filter(|&s| dist_used[s]).collect();
        let mut lit_lens = assign_lengths(self.rng, &lit_syms, 286, 15);
        let mut dist_lens = assign_lengths(self.rng, &dist_syms, 30, 15);
        if dist_syms.is_empty() && self.rng.chance(0.3) {
            dist_lens[self.rng.below(30)] = 1; // a single unused distance code
        }
        if self.take_defect(Defect::OverSubscribed) {
            let s = lit_syms[self.rng.below(lit_syms.len())];
            lit_lens[s] = lit_lens[s].saturating_sub(1).max(1);
            if lit_syms.len() == 1 {
                lit_lens[(s + 1) % 286] = 1;
                lit_lens[(s + 2) % 286] = 1;
            }
        } else if self.take_defect(Defect::Incomplete) {
            let s = lit_syms[self.rng.below(lit_syms.len())];
            if lit_lens[s] < 15 {
                lit_lens[s] += 1;
            }
            if lit_syms.len() == 1 {
                lit_lens[(s + 1) % 286] = 2;
            }
        } else if self.take_defect(Defect::NoEob) {
            lit_lens[256] = 0;
        }
        let mut nlen = (0..286).rposition(|s| lit_lens[s] > 0).unwrap_or(0).max(256) + 1;
        if self.rng.chance(0.3) {
            nlen = self.rng.range(nlen, 286);
        }
        let mut ndist = (0..30).rposition(|s| dist_lens[s] > 0).map_or(1, |i| i + 1);
        if self.rng.chance(0.3) {
            ndist = self.rng.range(ndist, 30);
        }
        let mut all: Vec<u8> = lit_lens[..nlen].to_vec();
        all.extend_from_slice(&dist_lens[..ndist]);
        if self.take_defect(Defect::TooManyLengths) {
            all.splice(nlen..nlen, std::iter::repeat_n(0, 286 - nlen + 1 + self.rng.below(2)));
            nlen = all.len() - ndist;
        } else if self.take_defect(Defect::TooManyDistances) {
            all.extend(std::iter::repeat_n(0, 30 - ndist + 1 + self.rng.below(2)));
            ndist = all.len() - nlen;
        }
        // run-length encode the code lengths: (symbol, extra bits value, extra bits)
        let use_repeats = self.rng.below(3);
        let mut cl_syms: Vec<(usize, u32, u32)> = vec![];
        let mut i = 0;
        if self.take_defect(Defect::RepeatFirst) {
            cl_syms.push((16, 0, 2));
        }
        while i < all.len() {
            let v = all[i];
            let mut run = 1;
            while i + run < all.len() && all[i + run] == v {
                run += 1;
            }
            let repeats = use_repeats == 1 || (use_repeats == 2 && self.rng.chance(0.5));
            if repeats && v == 0 && run >= 3 {
                let n = run.min(138);
                let n = self.rng.range(3, n);
                if n <= 10 {
                    cl_syms.push((17, (n - 3) as u32, 3));
                } else {
                    cl_syms.push((18, (n - 11) as u32, 7));
                }
                i += n;
            } else if repeats && v != 0 && run >= 4 {
                cl_syms.push((v as usize, 0, 0));
                let n = self.rng.range(3, (run - 1).min(6));
                cl_syms.push((16, (n - 3) as u32, 2));
                i += 1 + n;
            } else {
                cl_syms.push((v as usize, 0, 0));
                i += 1;
            }
        }
        if self.take_defect(Defect::RepeatPastEnd) {
            cl_syms.push((18, 127, 7));
        }
        let mut cl_used = [false; 19];
        for &(s, _, _) in &cl_syms {
            cl_used[s] = true;
        }
        let used: Vec<usize> = (0..19).filter(|&s| cl_used[s]).collect();
        let mut cl_lens = assign_lengths(self.rng, &used, 19, 7);
        if used.len() == 1 && !self.take_defect(Defect::IncompleteCl) {
            // a code length code must be complete: give one more symbol a 1-bit code
            let other = (0..19).find(|&s| !cl_used[s]).unwrap();
            cl_lens[other] = 1;
        } else if used.len() > 1 && self.take_defect(Defect::IncompleteCl) {
            let s = used[0];
            if cl_lens[s] < 7 {
                cl_lens[s] += 1;
            }
        }
        let mut hclen = CL_ORDER.iter().rposition(|&s| cl_lens[s] > 0).unwrap_or(0) + 1;
        hclen = hclen.max(4);
        if self.rng.chance(0.3) {
            hclen = self.rng.range(hclen, 19);
        }
        self.w.put((nlen - 257) as u32, 5);
        self.w.put((ndist - 1) as u32, 5);
        self.w.put((hclen - 4) as u32, 4);
        for &s in &CL_ORDER[..hclen] {
            self.w.put(cl_lens[s] as u32, 3);
        }
        let cl_codes = canonical(&cl_lens);
        for &(s, extra, bits) in &cl_syms {
            self.w.code(cl_codes[s]);
            self.w.put(extra, bits);
        }
        let litlen = canonical(&lit_lens[..nlen.min(286)]);
        let dist = canonical(&dist_lens);
        // the defects above can leave symbols without codes: write something anyway
        let litlen: Vec<(u32, u32)> =
            (0..286).map(|s| litlen.get(s).copied().filter(|c| c.1 > 0).unwrap_or((0, 1))).collect();
        let dist: Vec<(u32, u32)> =
            (0..30).map(|s| dist.get(s).copied().filter(|c| c.1 > 0).unwrap_or((0, 1))).collect();
        self.symbols(&ops, &litlen, &dist);
        self.apply(&ops);
    }
}

/// A random stream from [`Encoder`] and what it decodes to (meaningless if `defect != None`).
fn own_stream(rng: &mut Rng, target: usize, defect: Defect) -> (Vec<u8>, Vec<u8>) {
    let alphabet: Vec<u8> = {
        let k = rng.log_range(1, 256);
        let mut all: Vec<u8> = (0..=255).collect();
        rng.shuffle(&mut all);
        all.truncate(k);
        all
    };
    let match_p = [0.0, 0.1, 0.5, 0.9, 0.99][rng.below(5)];
    let mut enc =
        Encoder { rng, w: BitWriter::default(), plain: vec![], alphabet, match_p, defect, defect_done: false };
    // block size profile: tiny blocks, small, mixed, or (rarely) one huge block
    let profile = enc.rng.below(4);
    let mut produced = 0;
    loop {
        let size = match profile {
            0 => enc.rng.range(0, 40),
            1 => enc.rng.log_range(1, 5000),
            2 => enc.rng.log_range(1, 300_000),
            _ => enc.rng.log_range(1, target.max(1)),
        };
        let size = size.min(target.saturating_sub(produced));
        let last = produced + size >= target;
        let before = enc.plain.len();
        match enc.rng.below(10) {
            0 | 1 => enc.stored(last, size.min(65535)),
            2 | 3 => enc.fixed(last, size),
            _ => enc.dynamic(last, size),
        }
        produced += (enc.plain.len() - before).max(size.min(65535)).max(1);
        if last {
            break;
        }
    }
    let plain = std::mem::take(&mut enc.plain);
    let mut stream = std::mem::take(&mut enc.w).finish();
    if enc.rng.chance(0.05) {
        // trailing bytes after the final block are ignored by every decoder
        stream.extend((0..enc.rng.range(1, 100)).map(|_| enc.rng.next() as u8));
    }
    (stream, plain)
}

/// One dynamic block with ~`n` bytes of long matches (expands ~1000x).
fn huge_block(rng: &mut Rng, n: usize) -> (Vec<u8>, Vec<u8>) {
    let mut w = BitWriter::default();
    let mut plain = vec![];
    // 2 literals, then 258-byte matches at distance 1 or 2
    w.put(1, 1);
    w.put(1, 2); // fixed Huffman: short and simple
    let mut lens = [0u8; 288];
    lens[..144].fill(8);
    lens[144..256].fill(9);
    lens[256..280].fill(7);
    lens[280..].fill(8);
    let litlen = canonical(&lens);
    let dist = canonical(&[5; 32]);
    let (a, b) = (rng.next() as u8, rng.next() as u8);
    for byte in [a, b] {
        w.code(litlen[byte as usize]);
        plain.push(byte);
    }
    let d = rng.range(1, 2);
    while plain.len() < n {
        w.code(litlen[285]);
        w.code(dist[d - 1]);
        let start = plain.len() - d;
        for i in 0..258 {
            let b = plain[start + i];
            plain.push(b);
        }
    }
    w.code(litlen[256]);
    (w.finish(), plain)
}

// ---------------------------------------------------------------------------------------------------------------
// compressors and reference decoders

fn compress_flate2(rng: &mut Rng, data: &[u8], level: u32) -> Vec<u8> {
    use flate2::{Compress, Compression, FlushCompress, Status};
    let mut c = Compress::new(Compression::new(level), false);
    let mut out = Vec::with_capacity(data.len() / 2 + 1024);
    let flushes = rng.chance(0.3);
    let mut pos = 0;
    loop {
        let piece = if flushes { rng.log_range(1, 200_000) } else { data.len() };
        let end = (pos + piece).min(data.len());
        let flush = if end == data.len() {
            FlushCompress::Finish
        } else if flushes {
            [FlushCompress::None, FlushCompress::Sync, FlushCompress::Partial, FlushCompress::Full][rng.below(4)]
        } else {
            FlushCompress::None
        };
        loop {
            out.reserve((end - pos) + 4096);
            let before = c.total_in() as usize;
            let status = c.compress_vec(&data[pos..end], &mut out, flush).unwrap();
            pos += c.total_in() as usize - before;
            if status == Status::StreamEnd {
                return out;
            }
            if pos == end && out.capacity() > out.len() && !matches!(flush, FlushCompress::Finish) {
                break;
            }
        }
    }
}

fn compress_libdeflate(data: &[u8], level: i32) -> Vec<u8> {
    let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(level).unwrap());
    let mut out = vec![0u8; c.deflate_compress_bound(data.len())];
    let n = c.deflate_compress(data, &mut out).unwrap();
    out.truncate(n);
    out
}

/// zlib-rs: `Ok(output)` if the stream ends, else `Err(partial output)`.
fn reference(data: &[u8], size_hint: usize) -> Result<Vec<u8>, Vec<u8>> {
    use flate2::{Decompress, FlushDecompress, Status};
    let mut d = Decompress::new(false);
    let mut out = Vec::with_capacity(size_hint.min(1 << 30) + 1024);
    loop {
        if out.capacity() - out.len() < 1 << 16 {
            out.reserve(out.len().max(1 << 16));
        }
        let (in_before, out_before) = (d.total_in(), d.total_out());
        match d.decompress_vec(&data[in_before as usize..], &mut out, FlushDecompress::None) {
            Ok(Status::StreamEnd) => return Ok(out),
            Ok(_) if d.total_in() == in_before && d.total_out() == out_before => return Err(out),
            Ok(_) => {}
            Err(_) => return Err(out),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// the decoders under test

struct Ours {
    out: Vec<u8>,
    error: Option<std::io::Error>,
    fallbacks: usize,
    markers: usize,
}

fn read_all(reader: &mut dyn Read, rng: &mut Rng, size_hint: usize, stop_after: Option<usize>) -> Ours {
    let mut out = Vec::with_capacity(size_hint.min(1 << 30));
    let read_size = if rng.chance(0.2) { rng.log_range(1, 1 << 16) } else { 1 << 20 };
    let mut buf = vec![0u8; read_size];
    loop {
        if stop_after.is_some_and(|s| out.len() >= s) {
            return Ours { out, error: None, fallbacks: 0, markers: 0 };
        }
        let n = rng.range(1, read_size);
        match reader.read(&mut buf[..n]) {
            Ok(0) => return Ours { out, error: None, fallbacks: 0, markers: 0 },
            Ok(k) => out.extend_from_slice(&buf[..k]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                // errors must be sticky
                let again = reader.read(&mut buf);
                assert!(again.is_err(), "read after error returned {again:?}");
                return Ours { out, error: Some(e), fallbacks: 0, markers: 0 };
            }
        }
    }
}

fn run_parallel(stream: &Arc<Vec<u8>>, rng: &mut Rng, size_hint: usize, stop_after: Option<usize>) -> Ours {
    let threads = if rng.chance(0.2) { 1 } else { rng.range(2, 12) };
    // tiny chunks are slow (each one falls back), so keep the chunk count bounded
    let min_chunk = (stream.len() / 4000).max(256);
    let chunk = if rng.chance(0.15) { 1 << 20 } else { rng.log_range(min_chunk, (2 << 20).max(min_chunk)) };
    let mut inflater = if rng.chance(0.5) {
        ParallelInflater::with_any_chunk_size(stream.clone(), threads, chunk)
    } else {
        ParallelInflater::new(stream.clone(), threads, chunk)
    };
    let mut result = read_all(&mut inflater, rng, size_hint, stop_after);
    result.fallbacks = inflater.fallbacks();
    result.markers = inflater.marker_values();
    result
}

/// Writes a zip file with one entry: `stream` as deflated data, with the CRC-32 and size of `plain`.
fn write_zip(path: &std::path::Path, name: &str, stream: &[u8], plain: &[u8]) {
    let crc = crc32fast::hash(plain);
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let u16le = |v: u16| v.to_le_bytes();
    let u32le = |v: u32| v.to_le_bytes();
    let mut local = vec![];
    local.extend(u32le(0x0403_4b50));
    local.extend(u16le(20));
    local.extend(u16le(0)); // flags
    local.extend(u16le(8)); // deflate
    local.extend(u16le(0)); // time
    local.extend(u16le(0x21)); // date 1980-01-01
    local.extend(u32le(crc));
    local.extend(u32le(stream.len() as u32));
    local.extend(u32le(plain.len() as u32));
    local.extend(u16le(name.len() as u16));
    local.extend(u16le(0));
    local.extend(name.as_bytes());
    f.write_all(&local).unwrap();
    f.write_all(stream).unwrap();
    let cd_offset = local.len() + stream.len();
    let mut cd = vec![];
    cd.extend(u32le(0x0201_4b50));
    cd.extend(u16le(20));
    cd.extend(u16le(20));
    cd.extend(u16le(0));
    cd.extend(u16le(8));
    cd.extend(u16le(0));
    cd.extend(u16le(0x21));
    cd.extend(u32le(crc));
    cd.extend(u32le(stream.len() as u32));
    cd.extend(u32le(plain.len() as u32));
    cd.extend(u16le(name.len() as u16));
    cd.extend(u16le(0)); // extra
    cd.extend(u16le(0)); // comment
    cd.extend(u16le(0)); // disk
    cd.extend(u16le(0)); // internal attributes
    cd.extend(u32le(0)); // external attributes
    cd.extend(u32le(0)); // local header offset
    cd.extend(name.as_bytes());
    f.write_all(&cd).unwrap();
    let mut eocd = vec![];
    eocd.extend(u32le(0x0605_4b50));
    eocd.extend(u16le(0));
    eocd.extend(u16le(0));
    eocd.extend(u16le(1));
    eocd.extend(u16le(1));
    eocd.extend(u32le(cd.len() as u32));
    eocd.extend(u32le(cd_offset as u32));
    eocd.extend(u16le(0));
    f.write_all(&eocd).unwrap();
    f.flush().unwrap();
}

// ---------------------------------------------------------------------------------------------------------------
// cases

#[derive(Default)]
struct Stats {
    cases: AtomicU64,
    valid: AtomicU64,
    rejected: AtomicU64,
    zip: AtomicU64,
    large: AtomicU64,
    input_bytes: AtomicU64,
    output_bytes: AtomicU64,
    fallbacks: AtomicU64,
    markers: AtomicU64,
    kinds: Mutex<HashMap<&'static str, u64>>,
}

struct Case {
    kind: &'static str,
    stream: Vec<u8>,
    /// The intended output (for zip entries' CRC and size), if known.
    plain: Option<Vec<u8>>,
}

fn mutate(rng: &mut Rng, stream: &mut Vec<u8>) -> &'static str {
    if stream.is_empty() {
        stream.push(rng.next() as u8);
        return "mutated-empty";
    }
    let len = stream.len();
    // corrupt near the start (block headers) more often
    let mut pos = || if rng.chance(0.3) { rng.below(len.min(64)) } else { rng.below(len) };
    let p = pos();
    match rng.below(9) {
        0 => {
            for _ in 0..rng.range(1, 8) {
                let p = rng.below(stream.len());
                stream[p] ^= 1 << rng.below(8);
            }
            "mutated-bitflips"
        }
        1 => {
            stream[p] ^= 1 << rng.below(8);
            "mutated-bitflip"
        }
        2 => {
            stream[p] = rng.next() as u8;
            "mutated-byte"
        }
        3 => {
            stream.truncate(p);
            "mutated-truncate"
        }
        4 => {
            let n = rng.range(1, 64).min(stream.len() - p);
            stream.drain(p..p + n);
            "mutated-delete"
        }
        5 => {
            let bytes: Vec<u8> = (0..rng.range(1, 64)).map(|_| rng.next() as u8).collect();
            stream.splice(p..p, bytes);
            "mutated-insert"
        }
        6 => {
            let src = rng.below(len);
            let n = rng.range(1, 4096).min(len - src);
            let piece = stream[src..src + n].to_vec();
            let end = (p + n).min(stream.len());
            stream.splice(p..end, piece);
            "mutated-splice"
        }
        7 => {
            let n = rng.range(1, 256).min(stream.len() - p);
            let v = if rng.chance(0.5) { 0 } else { 0xff };
            stream[p..p + n].fill(v);
            "mutated-fill"
        }
        _ => {
            let n = rng.range(1, 3);
            for _ in 0..n {
                let p = rng.below(stream.len());
                stream[p] ^= 1 << rng.below(8);
            }
            stream.truncate(rng.range(stream.len() / 2, stream.len()));
            "mutated-flip-truncate"
        }
    }
}

fn valid_case(rng: &mut Rng, max: usize) -> Case {
    let n = match rng.below(10) {
        0 => rng.range(0, 64),
        1..=5 => rng.log_range(1, 1 << 20),
        _ => rng.log_range(1 << 16, max),
    };
    match rng.below(10) {
        0..=3 => {
            let (stream, plain) = own_stream(rng, n, Defect::None);
            Case { kind: "own", stream, plain: Some(plain) }
        }
        4..=6 => {
            let data = gen_data(rng, n);
            let level = rng.range(0, 9) as u32;
            Case { kind: "flate2", stream: compress_flate2(rng, &data, level), plain: Some(data) }
        }
        _ => {
            let data = gen_data(rng, n);
            let level = rng.range(0, 12) as i32;
            Case { kind: "libdeflate", stream: compress_libdeflate(&data, level), plain: Some(data) }
        }
    }
}

fn make_case(rng: &mut Rng, allow_large: bool) -> Case {
    let r = rng.below(1000);
    if r < 3 && allow_large {
        let n = rng.range(50 << 20, 200 << 20);
        let data = gen_data(rng, n);
        let (kind, stream) = if rng.chance(0.5) {
            let level = rng.range(1, 6) as u32;
            ("large-flate2", compress_flate2(rng, &data, level))
        } else {
            ("large-libdeflate", compress_libdeflate(&data, rng.range(1, 9) as i32))
        };
        return Case { kind, stream, plain: Some(data) };
    }
    if r < 8 {
        let n = rng.range(20 << 20, 80 << 20);
        let (stream, plain) = huge_block(rng, n);
        return Case { kind: "huge-block", stream, plain: Some(plain) };
    }
    match r % 20 {
        0..=9 => valid_case(rng, 8 << 20),
        10..=11 => {
            let defect = DEFECTS[rng.below(DEFECTS.len())];
            let n = rng.log_range(1, 1 << 20);
            let (stream, plain) = own_stream(rng, n, defect);
            Case { kind: "invalid", stream, plain: Some(plain) }
        }
        12..=18 => {
            let mut case = valid_case(rng, 2 << 20);
            case.kind = mutate(rng, &mut case.stream);
            if rng.chance(0.2) {
                mutate(rng, &mut case.stream);
            }
            case
        }
        _ => {
            let n = rng.log_range(1, 1 << 20);
            Case { kind: "random", stream: (0..n).map(|_| rng.next() as u8).collect(), plain: None }
        }
    }
}

fn case_seed(seed: u64, index: u64) -> u64 {
    Rng::new(seed.wrapping_mul(0x2545_f491_4f6c_dd1d) ^ index).next()
}

fn run_case(seed: u64, index: u64, tmp: &std::path::Path, stats: &Stats, allow_large: bool, verbose: bool) {
    let mut rng = Rng::new(case_seed(seed, index));
    let case = make_case(&mut rng, allow_large);
    let size_hint = case.plain.as_ref().map_or(case.stream.len() * 4, |p| p.len());
    let large = case.kind.starts_with("large") || case.kind == "huge-block";
    let fail = |what: String, stream: &[u8]| -> ! {
        let path = tmp.join(format!("fuzz_inflate_failure_{seed}_{index}.deflate"));
        let _ = std::fs::write(&path, stream);
        eprintln!(
            "FAILURE in case {index} (seed {seed}, kind {}): {what}; stream saved to {}",
            case.kind,
            path.display()
        );
        std::process::abort();
    };
    let expected = reference(&case.stream, size_hint);
    if verbose {
        eprintln!(
            "case {index}: kind {}, {} compressed bytes, reference {}",
            case.kind,
            case.stream.len(),
            match &expected {
                Ok(out) => format!("ok, {} bytes", out.len()),
                Err(partial) => format!("error after {} bytes", partial.len()),
            }
        );
    }
    // the generated valid streams must decode to what the generator intended
    let generated_valid = matches!(case.kind, "own" | "flate2" | "libdeflate" | "huge-block") || large;
    if generated_valid && let Some(plain) = &case.plain {
        match &expected {
            Ok(out) if out == plain => {}
            Ok(_) => fail("zlib-rs output differs from the generated data (encoder bug?)".into(), &case.stream),
            Err(_) => fail("zlib-rs rejects a generated valid stream (encoder bug?)".into(), &case.stream),
        }
    }
    // libdeflate agrees on valid streams
    if let Ok(out) = &expected
        && out.len() < 1 << 28
    {
        let mut d = libdeflater::Decompressor::new();
        let mut buf = vec![0u8; out.len()];
        match d.deflate_decompress(&case.stream, &mut buf) {
            Ok(n) if n == out.len() && buf == *out => {}
            other => {
                fail(format!("libdeflate disagrees with zlib-rs: {:?}", other.map(|_| ())), &case.stream);
            }
        }
    }

    let stream = Arc::new(case.stream);
    let runs = if large { 1 } else { rng.range(1, 3) };
    for _ in 0..runs {
        let stop_after = if rng.chance(0.05) { Some(rng.below(size_hint + 1)) } else { None };
        let ours = run_parallel(&stream, &mut rng, size_hint, stop_after);
        stats.fallbacks.fetch_add(ours.fallbacks as u64, Ordering::Relaxed);
        stats.markers.fetch_add(ours.markers as u64, Ordering::Relaxed);
        if stop_after.is_some() && ours.error.is_none() {
            // dropped early: whatever was read must be a prefix of the reference
            let reference = match &expected {
                Ok(out) | Err(out) => out,
            };
            if !reference.starts_with(&ours.out) {
                fail("early-dropped output is not a prefix of the reference".into(), &stream);
            }
            continue;
        }
        match (&expected, &ours.error) {
            (Ok(out), None) => {
                if *out != ours.out {
                    fail(format!("output differs: {} vs reference {} bytes", ours.out.len(), out.len()), &stream);
                }
            }
            (Ok(_), Some(e)) => fail(format!("rejected a valid stream: {e}"), &stream),
            (Err(partial), None) => fail(
                format!(
                    "accepted an invalid stream ({} bytes; zlib-rs failed after {})",
                    ours.out.len(),
                    partial.len()
                ),
                &stream,
            ),
            (Err(partial), Some(_)) => {
                if !partial.starts_with(&ours.out) {
                    fail(
                        format!(
                            "output before the error ({} bytes) is not a prefix of zlib-rs's ({} bytes)",
                            ours.out.len(),
                            partial.len()
                        ),
                        &stream,
                    );
                }
            }
        }
        stats.output_bytes.fetch_add(ours.out.len() as u64, Ordering::Relaxed);
    }

    // through a zip entry: the reader returns the intended bytes, or fails
    if let Some(plain) = &case.plain
        && (rng.chance(0.03) || (large && rng.chance(0.3)))
    {
        stats.zip.fetch_add(1, Ordering::Relaxed);
        let path = tmp.join(format!("fuzz_{index}.zip"));
        write_zip(&path, "dir/entry.bin", &stream, plain);
        for threads in [1, rng.range(2, 12)] {
            let mut reader = ZipEntryReader::open_with(&path, "dir/entry.bin", threads, 0).unwrap();
            let ours = read_all(&mut reader, &mut rng, plain.len(), None);
            let matches_plain = expected.as_ref().is_ok_and(|out| out == plain);
            match &ours.error {
                None if ours.out == *plain => {
                    if !matches_plain {
                        fail(format!("zip reader ({threads} threads) accepted a stream zlib-rs rejects"), &stream);
                    }
                }
                None => fail(format!("zip reader ({threads} threads) returned wrong bytes without an error"), &stream),
                Some(e) if matches_plain => {
                    fail(format!("zip reader ({threads} threads) rejected a valid entry: {e}"), &stream)
                }
                Some(_) => {}
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    stats.cases.fetch_add(1, Ordering::Relaxed);
    stats.input_bytes.fetch_add(stream.len() as u64, Ordering::Relaxed);
    if expected.is_ok() { &stats.valid } else { &stats.rejected }.fetch_add(1, Ordering::Relaxed);
    if large {
        stats.large.fetch_add(1, Ordering::Relaxed);
    }
    *stats.kinds.lock().unwrap().entry(case.kind).or_default() += 1;
}

fn parse_args() -> HashMap<String, String> {
    let mut result = HashMap::new();
    for arg in std::env::args().skip(1) {
        let arg = arg.trim_start_matches('-');
        match arg.split_once('=') {
            Some((k, v)) => result.insert(k.replace('-', "_"), v.to_string()),
            None => result.insert(arg.replace('-', "_"), "true".to_string()),
        };
    }
    result
}

fn main() {
    let args = parse_args();
    let get = |k: &str, default: u64| args.get(k).map_or(default, |v| v.parse().unwrap());
    let seconds = get("seconds", 60);
    let seed = get("seed", 1);
    let start = get("start", 0);
    let runners = get("runners", 4) as usize;
    let max_large = get("max_large", u64::MAX);
    let tmp_holder = tempfile::tempdir().unwrap();
    let tmp = args.get("tmp").map_or_else(|| tmp_holder.path().to_path_buf(), std::path::PathBuf::from);

    std::panic::set_hook(Box::new(|_| {}));
    let overflow_checks = std::panic::catch_unwind(|| std::hint::black_box(255u8) + std::hint::black_box(1u8)).is_err();
    eprintln!("debug assertions: {}, overflow checks: {overflow_checks}", cfg!(debug_assertions));
    // any panic, including one in an inflate worker that the inflater would turn into a fallback, is a failure
    std::panic::set_hook(Box::new(|info| {
        eprintln!("PANIC: {info}");
        std::process::abort();
    }));

    let stats = Arc::new(Stats::default());
    if let Some(case) = args.get("case") {
        run_case(seed, case.parse().unwrap(), &tmp, &stats, true, true);
        eprintln!("case {case} passed");
        return;
    }

    let next = Arc::new(AtomicU64::new(start));
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let stop = Arc::new(AtomicBool::new(false));
    let large_lock = Arc::new(Mutex::new(()));
    let large_left = Arc::new(AtomicUsize::new(max_large.min(usize::MAX as u64) as usize));
    // per runner: (case index + 1, start time in ms since `t0`), 0 = idle
    let t0 = Instant::now();
    let slots: Arc<Vec<(AtomicU64, AtomicU64)>> =
        Arc::new((0..runners).map(|_| (AtomicU64::new(0), AtomicU64::new(0))).collect());
    {
        let (slots, stop) = (slots.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                let now = t0.elapsed().as_millis() as u64;
                for (case, since) in slots.iter() {
                    let c = case.load(Ordering::Relaxed);
                    if c != 0 && now.saturating_sub(since.load(Ordering::Relaxed)) > 300_000 {
                        eprintln!("HANG: case {} (seed {seed}) has run for over 5 minutes", c - 1);
                        std::process::abort();
                    }
                }
            }
        });
    }
    let handles: Vec<_> = (0..runners)
        .map(|r| {
            let (next, stats, stop, slots, tmp) =
                (next.clone(), stats.clone(), stop.clone(), slots.clone(), tmp.clone());
            let (large_lock, large_left) = (large_lock.clone(), large_left.clone());
            std::thread::spawn(move || {
                while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    // only one runner at a time may take a large case (memory)
                    let guard = large_lock.try_lock().ok();
                    let allow_large = guard.is_some() && large_left.load(Ordering::Relaxed) > 0;
                    slots[r].1.store(t0.elapsed().as_millis() as u64, Ordering::Relaxed);
                    slots[r].0.store(index + 1, Ordering::Relaxed);
                    let before = stats.large.load(Ordering::Relaxed);
                    run_case(seed, index, &tmp, &stats, allow_large, false);
                    if allow_large && stats.large.load(Ordering::Relaxed) > before {
                        large_left.fetch_sub(1, Ordering::Relaxed);
                    }
                    slots[r].0.store(0, Ordering::Relaxed);
                    drop(guard);
                }
            })
        })
        .collect();
    let mut last_report = Instant::now();
    while handles.iter().any(|h| !h.is_finished()) {
        std::thread::sleep(Duration::from_millis(200));
        if last_report.elapsed() > Duration::from_secs(30) {
            last_report = Instant::now();
            report(&stats, t0);
        }
    }
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }
    report(&stats, t0);
    eprintln!("next case index: {}", next.load(Ordering::Relaxed));
}

fn report(stats: &Stats, t0: Instant) {
    let mut kinds: Vec<_> = stats.kinds.lock().unwrap().iter().map(|(k, v)| (*k, *v)).collect();
    kinds.sort();
    eprintln!(
        "[{:.0}s] {} cases ({} valid, {} rejected, {} through zip, {} large), {:.1} MB in, {:.1} MB out, {} fallbacks, \
         {} marker values; {kinds:?}",
        t0.elapsed().as_secs_f64(),
        stats.cases.load(Ordering::Relaxed),
        stats.valid.load(Ordering::Relaxed),
        stats.rejected.load(Ordering::Relaxed),
        stats.zip.load(Ordering::Relaxed),
        stats.large.load(Ordering::Relaxed),
        stats.input_bytes.load(Ordering::Relaxed) as f64 / 1e6,
        stats.output_bytes.load(Ordering::Relaxed) as f64 / 1e6,
        stats.fallbacks.load(Ordering::Relaxed),
        stats.markers.load(Ordering::Relaxed),
    );
}
