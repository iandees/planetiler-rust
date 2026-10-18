//! Multithreaded decompression of a single raw deflate stream (no Java equivalent).
//!
//! A deflate stream is a chain of blocks whose boundaries are only known by decoding everything before them, and
//! whose back-references reach up to 32 KB into earlier output, so it is normally decoded by one thread. This uses the
//! approach of [rapidgzip](https://github.com/mxmlnkn/rapidgzip) / pugz to decode it in parallel:
//!
//! 1. The compressed bytes are split into fixed-size chunks. A worker for chunk `j > 0` searches its chunk for the
//!    first bit offset that holds a plausible dynamic-Huffman block header (valid code lengths, complete codes) whose
//!    whole block then decodes without errors.
//! 2. It decodes from there until the first block that starts at or after the next chunk's start offset, without
//!    knowing the 32 KB of output that precedes it: bytes copied from that unknown window are recorded as 16-bit
//!    markers that name the window position. Once the last 32 KB of output contain no markers (usually within a few
//!    blocks), decoding continues into plain bytes.
//! 3. A consumer walks the chunks in order and checks that each one starts exactly where the previous one ended. If
//!    so it replaces the markers with bytes from the real window; if not (the header search found a false positive,
//!    or a chunk contains no block start), it decodes that chunk's range itself, sequentially, with the real window.
//!
//! So a wrong guess only costs time, never correctness. [`ParallelInflater`] implements [`Read`] and keeps a bounded
//! number of chunks in flight. Callers should still verify a checksum of the output (zip entries have a CRC-32).
//!
//! The input is untrusted, so memory per worker and the block-start search are bounded (see
//! `SPECULATIVE_BLOCK_SLACK`), and decoding pauses inside huge blocks instead of materializing them. Differential
//! fuzzing against zlib-rs: `planetiler-benchmarks/src/bin/fuzz_parallel_inflate.rs` (stable) and `fuzz/` (cargo-fuzz).
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::ops::{Deref, Range};
use std::sync::{Arc, OnceLock};

use crossbeam_channel::{Receiver, Sender};

/// Back-references reach at most this far back.
const WINDOW: usize = 1 << 15;
/// Marker values are `MARKER + window index`; literals are `< 256`.
const MARKER: u16 = 256;

const LEN_BASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
/// Order code length code lengths are stored in.
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// Any error while decoding; in speculative mode it just means "not a valid block here".
#[derive(Debug)]
struct Invalid;

type Res<T> = Result<T, Invalid>;

/// LSB-first bit reader over a byte slice that reads zeros past the end (callers check [`Bits::overrun`]).
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], bit: usize) -> Bits<'a> {
        let mut bits = Bits { data, pos: bit / 8, buf: 0, cnt: 0 };
        bits.refill();
        bits.consume((bit % 8) as u32);
        bits
    }

    /// Ensures at least 56 bits are buffered.
    #[inline(always)]
    fn refill(&mut self) {
        if let Some(word) = self.data.get(self.pos..self.pos + 8) {
            let word = u64::from_le_bytes(word.try_into().unwrap());
            self.buf |= word << self.cnt;
            self.pos += ((63 - self.cnt) >> 3) as usize;
            self.cnt |= 56;
        } else {
            while self.cnt <= 56 {
                let byte = self.data.get(self.pos).copied().unwrap_or(0);
                self.buf |= (byte as u64) << self.cnt;
                self.pos += 1;
                self.cnt += 8;
            }
        }
    }

    /// Absolute bit position of the next unread bit.
    #[inline(always)]
    fn bit_pos(&self) -> usize {
        self.pos * 8 - self.cnt as usize
    }

    fn overrun(&self) -> bool {
        self.bit_pos() > self.data.len() * 8
    }

    #[inline(always)]
    fn consume(&mut self, n: u32) {
        self.buf >>= n;
        self.cnt -= n;
    }

    /// Reads `n <= 32` bits; at least `n` must be buffered.
    #[inline(always)]
    fn take(&mut self, n: u32) -> u32 {
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.consume(n);
        v
    }

    fn align_to_byte(&mut self) {
        let n = self.cnt % 8;
        self.consume(n);
    }
}

const SUB: u32 = 1 << 31;

/// Canonical Huffman decoding table: a `2^lut_bits` primary table, plus subtables for longer codes. Entries are
/// `symbol | total length << 16`, or `SUB | subtable offset | subtable bits << 16`; length 0 means invalid code.
struct Huffman {
    table: Vec<u32>,
    lut_bits: u32,
}

impl Huffman {
    fn new(lut_bits: u32) -> Huffman {
        Huffman { table: Vec::new(), lut_bits }
    }

    /// Builds the table for `lens` (code length per symbol, 0 = unused), rejecting over-subscribed and incomplete codes
    /// like zlib (an incomplete code is allowed only if it is a single code of length 1, and an empty one is allowed
    /// but fails when used).
    fn build(&mut self, lens: &[u8]) -> Res<()> {
        let mut count = [0u16; 16];
        for &l in lens {
            count[l as usize] += 1;
        }
        count[0] = 0;
        let mut left = 1i32;
        for &c in &count[1..] {
            left = (left << 1) - c as i32;
            if left < 0 {
                return Err(Invalid);
            }
        }
        let max_len = (1..16).rev().find(|&l| count[l] > 0).unwrap_or(0) as u32;
        if left > 0 && max_len > 1 {
            return Err(Invalid);
        }
        let lut = self.lut_bits;
        self.table.clear();
        self.table.resize(1 << lut, 0);
        let mut next = [0u32; 16];
        let mut code = 0u32;
        for len in 1..16 {
            code = (code + count[len - 1] as u32) << 1;
            next[len] = code;
        }
        let sub_bits = max_len.saturating_sub(lut);
        for (sym, &len) in lens.iter().enumerate() {
            if len == 0 {
                continue;
            }
            let len = len as u32;
            let c = next[len as usize];
            next[len as usize] += 1;
            let rev = c.reverse_bits() >> (32 - len);
            let entry = sym as u32 | len << 16;
            if len <= lut {
                let mut k = rev as usize;
                while k < 1 << lut {
                    self.table[k] = entry;
                    k += 1 << len;
                }
            } else {
                let prefix = (rev & ((1 << lut) - 1)) as usize;
                if self.table[prefix] & SUB == 0 {
                    let offset = self.table.len() as u32;
                    self.table.resize(self.table.len() + (1 << sub_bits), 0);
                    self.table[prefix] = SUB | offset | sub_bits << 16;
                }
                let offset = (self.table[prefix] & 0xffff) as usize;
                let mut k = (rev >> lut) as usize;
                while k < 1 << sub_bits {
                    self.table[offset + k] = entry;
                    k += 1 << (len - lut);
                }
            }
        }
        Ok(())
    }

    /// Decodes one symbol; needs at least 15 buffered bits.
    #[inline(always)]
    fn decode(&self, bits: &mut Bits) -> Res<u32> {
        let mut e = self.table[(bits.buf & ((1 << self.lut_bits) - 1)) as usize];
        if e & SUB != 0 {
            let sub_bits = (e >> 16) & 0xff;
            let index = (e & 0xffff) as usize + ((bits.buf >> self.lut_bits) & ((1 << sub_bits) - 1)) as usize;
            e = self.table[index];
        }
        let len = (e >> 16) & 0xff;
        if len == 0 {
            return Err(Invalid);
        }
        bits.consume(len);
        Ok(e & 0xffff)
    }
}

fn fixed_tables() -> &'static (Huffman, Huffman) {
    static FIXED: OnceLock<(Huffman, Huffman)> = OnceLock::new();
    FIXED.get_or_init(|| {
        let mut lens = [0u8; 288];
        lens[..144].fill(8);
        lens[144..256].fill(9);
        lens[256..280].fill(7);
        lens[280..].fill(8);
        let mut litlen = Huffman::new(10);
        litlen.build(&lens).unwrap();
        let mut dist = Huffman::new(8);
        dist.build(&[5; 32]).unwrap(); // 30 and 31 are invalid symbols
        (litlen, dist)
    })
}

/// Reads a dynamic block's code tables (after the 3 header bits).
fn read_dynamic_tables(bits: &mut Bits, litlen: &mut Huffman, dist: &mut Huffman, scratch: &mut Huffman) -> Res<()> {
    bits.refill();
    let nlen = bits.take(5) as usize + 257;
    let ndist = bits.take(5) as usize + 1;
    let ncode = bits.take(4) as usize + 4;
    if nlen > 286 || ndist > 30 {
        return Err(Invalid);
    }
    let mut cl_lens = [0u8; 19];
    for (k, &i) in CL_ORDER[..ncode].iter().enumerate() {
        if k % 16 == 0 {
            bits.refill(); // up to 57 bits in total
        }
        cl_lens[i] = bits.take(3) as u8;
    }
    // code length codes must be complete
    let mut count = [0u16; 8];
    for &l in &cl_lens {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut left = 1i32;
    for &c in &count[1..] {
        left = (left << 1) - c as i32;
    }
    if left != 0 {
        return Err(Invalid);
    }
    scratch.build(&cl_lens)?;
    let mut lens = [0u8; 286 + 30];
    let total = nlen + ndist;
    let mut i = 0;
    while i < total {
        bits.refill();
        let sym = scratch.decode(bits)?;
        match sym {
            0..=15 => {
                lens[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(Invalid);
                }
                let prev = lens[i - 1];
                let n = 3 + bits.take(2) as usize;
                if i + n > total {
                    return Err(Invalid);
                }
                lens[i..i + n].fill(prev);
                i += n;
            }
            17 | 18 => {
                let n = if sym == 17 { 3 + bits.take(3) as usize } else { 11 + bits.take(7) as usize };
                if i + n > total {
                    return Err(Invalid);
                }
                i += n; // already zero
            }
            _ => return Err(Invalid),
        }
    }
    if lens[256] == 0 {
        return Err(Invalid); // no end-of-block code
    }
    litlen.build(&lens[..nlen])?;
    dist.build(&lens[nlen..total])?;
    if bits.overrun() { Err(Invalid) } else { Ok(()) }
}

/// Where decoded symbols go.
///
/// [`decode_huffman_block`] calls [`Sink::reserve_symbols`] before every step, and each step then makes at most two
/// [`Sink::literal`] calls, or one `literal` and one [`Sink::copy`] call, or one `copy` call. Nothing else calls
/// `literal` or `copy`. [`ByteSink`] uses this to grow its buffer (and to pause) only in `reserve_symbols`, so the
/// pushes in a step never reallocate.
trait Sink {
    /// Called before each step of at most 2 literals or 1 literal + 1 match (up to 259 bytes). Returns `true` to
    /// pause decoding here (at a symbol boundary) because the sink is full.
    fn reserve_symbols(&mut self) -> bool;
    fn literal(&mut self, b: u8);
    fn copy(&mut self, dist: usize, len: usize) -> Res<()>;
    fn stored(&mut self, bytes: &[u8]);
}

/// Plain byte output (after a known window prefix, if any).
struct ByteSink {
    out: Vec<u8>,
    /// Pause once `out` holds at least this many bytes and needs to grow.
    limit: usize,
}

/// Spare capacity [`ByteSink`] keeps so a literal plus a match (up to 258 bytes, copied in 16-byte steps: up to 272
/// bytes pushed before the overshoot is truncated) fit without reallocating. This code used to write into the spare
/// capacity with `unsafe` pointer copies; the safe pushes below cost ~2% more inflate CPU time (see PORTING.md).
const SLACK: usize = 258 + 32;

impl Sink for ByteSink {
    #[inline(always)]
    fn reserve_symbols(&mut self) -> bool {
        if self.out.capacity() - self.out.len() < SLACK {
            if self.out.len() >= self.limit {
                return true;
            }
            self.out.reserve(SLACK.max(self.out.len() / 4));
        }
        false
    }

    #[inline(always)]
    fn literal(&mut self, b: u8) {
        self.out.push(b);
    }

    #[inline(always)]
    fn copy(&mut self, dist: usize, len: usize) -> Res<()> {
        let n = self.out.len();
        if dist > n || dist == 0 {
            return Err(Invalid);
        }
        if dist >= 16 {
            // 16 bytes at a time, each step reading only bytes that are already there; the overshoot is cut off
            let mut i = 0;
            while i < len {
                let s = n - dist + i;
                let step: [u8; 16] = self.out[s..s + 16].try_into().unwrap();
                self.out.extend_from_slice(&step);
                i += 16;
            }
            self.out.truncate(n + len);
        } else if dist == 1 {
            let b = self.out[n - 1];
            self.out.resize(n + len, b);
        } else {
            for i in 0..len {
                let b = self.out[n - dist + i];
                self.out.push(b);
            }
        }
        Ok(())
    }

    fn stored(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }
}

/// Output where references before the start of the chunk become window markers.
struct MarkerSink {
    out: Vec<u16>,
    /// Index after the last marker written (0 = none).
    last_marker_end: usize,
    /// Pause once `out` holds at least this many values.
    limit: usize,
}

impl Sink for MarkerSink {
    #[inline(always)]
    fn reserve_symbols(&mut self) -> bool {
        self.out.len() >= self.limit
    }

    #[inline(always)]
    fn literal(&mut self, b: u8) {
        self.out.push(b as u16);
    }

    fn copy(&mut self, dist: usize, len: usize) -> Res<()> {
        let n = self.out.len();
        if dist > WINDOW {
            return Err(Invalid);
        }
        self.out.reserve(len);
        for i in 0..len {
            let src = (n + i) as isize - dist as isize;
            let v = if src < 0 { MARKER + (WINDOW as isize + src) as u16 } else { self.out[src as usize] };
            if v >= MARKER {
                self.last_marker_end = n + i + 1;
            }
            self.out.push(v);
        }
        Ok(())
    }

    fn stored(&mut self, bytes: &[u8]) {
        self.out.extend(bytes.iter().map(|&b| b as u16));
    }
}

/// Decodes the symbols of one Huffman-coded block into `sink`. Returns `true` at the end of the block, or `false` if
/// the sink asked to pause (decoding can resume from `bits` with the same tables).
#[inline(always)]
fn decode_huffman_block<S: Sink>(bits: &mut Bits, litlen: &Huffman, dist: &Huffman, sink: &mut S) -> Res<bool> {
    loop {
        bits.refill();
        if bits.pos > bits.data.len() + 8 {
            return Err(Invalid); // ran past the end of the input
        }
        if sink.reserve_symbols() {
            return Ok(false);
        }
        let sym = litlen.decode(bits)?;
        if sym < 256 {
            sink.literal(sym as u8);
            // a second literal fits in the remaining buffered bits (>= 41)
            let sym = litlen.decode(bits)?;
            if sym < 256 {
                sink.literal(sym as u8);
                continue;
            }
            if sym == 256 {
                return Ok(true);
            }
            bits.refill();
            copy_match(bits, sym, dist, sink)?;
            continue;
        }
        if sym == 256 {
            return Ok(true);
        }
        copy_match(bits, sym, dist, sink)?;
    }
}

/// Decodes the rest of a length/distance pair (needs at least 41 buffered bits) and copies it.
#[inline(always)]
fn copy_match<S: Sink>(bits: &mut Bits, sym: u32, dist: &Huffman, sink: &mut S) -> Res<()> {
    let li = (sym - 257) as usize;
    if li >= 29 {
        return Err(Invalid);
    }
    let len = LEN_BASE[li] as usize + bits.take(LEN_EXTRA[li] as u32) as usize;
    let dsym = dist.decode(bits)? as usize;
    if dsym >= 30 {
        return Err(Invalid);
    }
    let d = DIST_BASE[dsym] as usize + bits.take(DIST_EXTRA[dsym] as u32) as usize;
    sink.copy(d, len)
}

/// Output buffers handed back by the consumer for reuse, so that the allocator does not have to recycle big buffers
/// freed on a different thread than the one that allocated them.
type Pool = Arc<parking_lot::Mutex<Buffers>>;

#[derive(Default)]
struct Buffers {
    bytes: Vec<Vec<u8>>,
    values: Vec<Vec<u16>>,
}

/// A Huffman block that [`Decoder::block`] stopped in the middle of.
#[derive(Clone, Copy, Debug)]
struct Paused {
    /// Fixed tables (otherwise the decoder's dynamic tables, which stay untouched until it resumes).
    fixed: bool,
    /// BFINAL of the block.
    last: bool,
}

/// Reusable decoding state.
struct Decoder {
    litlen: Huffman,
    dist: Huffman,
    scratch: Huffman,
    pool: Pool,
    /// Set when the last [`Decoder::block`] call paused: the next call continues that block.
    paused: Option<Paused>,
}

impl Decoder {
    fn new(pool: Pool) -> Decoder {
        Decoder { litlen: Huffman::new(10), dist: Huffman::new(8), scratch: Huffman::new(7), pool, paused: None }
    }

    fn buffer(&self) -> Vec<u8> {
        let mut buf = self.pool.lock().bytes.pop().unwrap_or_default();
        buf.clear();
        buf
    }

    fn value_buffer(&self) -> Vec<u16> {
        let mut buf = self.pool.lock().values.pop().unwrap_or_default();
        buf.clear();
        buf
    }

    /// Decodes one block (header included, unless resuming a paused block) into `sink`. Returns `Some(BFINAL)` at
    /// the end of the block, or `None` if the sink asked to pause (the next call continues from `bits`).
    fn block<S: Sink>(&mut self, bits: &mut Bits, sink: &mut S) -> Res<Option<bool>> {
        let Paused { fixed, last } = match self.paused.take() {
            Some(paused) => paused,
            None => {
                bits.refill();
                let last = bits.take(1) == 1;
                match bits.take(2) {
                    0 => {
                        bits.align_to_byte();
                        bits.refill();
                        let len = bits.take(16) as usize;
                        let nlen = bits.take(16) as usize;
                        if len != !nlen & 0xffff {
                            return Err(Invalid);
                        }
                        // the reader buffers whole bytes ahead: after aligning, the next byte is at bit_pos / 8
                        let start = bits.bit_pos() / 8;
                        let bytes = bits.data.get(start..start + len).ok_or(Invalid)?;
                        sink.stored(bytes);
                        *bits = Bits::new(bits.data, (start + len) * 8);
                        return Ok(Some(last));
                    }
                    1 => Paused { fixed: true, last },
                    2 => {
                        read_dynamic_tables(bits, &mut self.litlen, &mut self.dist, &mut self.scratch)?;
                        Paused { fixed: false, last }
                    }
                    _ => return Err(Invalid),
                }
            }
        };
        let (litlen, dist) = if fixed {
            let (litlen, dist) = fixed_tables();
            (litlen, dist)
        } else {
            (&self.litlen, &self.dist)
        };
        let ended = decode_huffman_block(bits, litlen, dist, sink)?;
        if bits.overrun() {
            return Err(Invalid);
        }
        if ended {
            Ok(Some(last))
        } else {
            self.paused = Some(Paused { fixed, last });
            Ok(None)
        }
    }
}

/// Output of decoding a range of blocks.
struct Chunk {
    /// Bit offset of the first block.
    start: usize,
    /// Bit offset after the last block.
    end: usize,
    /// True if the last block was the final block of the stream.
    last: bool,
    /// True if decoding paused in the middle of a block at `end` (the decoder can resume it).
    paused: bool,
    /// Leading output that may contain window markers.
    markers: Vec<u16>,
    /// Remaining output, after `skip` bytes of window copied from the end of `markers`.
    bytes: Vec<u8>,
    skip: usize,
}

/// Decodes blocks starting at bit `start` until reaching one that starts at or after bit `stop`, the final block, or
/// (to bound memory on highly compressible data) the first block boundary after producing `max_out` bytes. A single
/// block can expand ~1000x, so decoding also pauses in the middle of a block once it has produced about `pause_at`
/// bytes (`Chunk::paused`; the next call with the same `decoder` continues that block). With `window = None`,
/// references before `start` become markers; otherwise they read from `window`.
fn decode_range(
    decoder: &mut Decoder,
    data: &[u8],
    start: usize,
    stop: usize,
    max_out: usize,
    pause_at: usize,
    window: Option<&[u8]>,
) -> Res<Chunk> {
    let expected_out = (4 * (stop.min(data.len() * 8).saturating_sub(start) / 8)).min(max_out) + (1 << 16);
    let mut bits = Bits::new(data, start);
    let mut chunk = Chunk { start, end: start, last: false, paused: false, markers: vec![], bytes: vec![], skip: 0 };
    let mut markers =
        window.is_none().then(|| MarkerSink { out: decoder.value_buffer(), last_marker_end: 0, limit: usize::MAX });
    let mut bytes = ByteSink { out: decoder.buffer(), limit: usize::MAX };
    if let Some(window) = window {
        bytes.out.reserve(window.len() + expected_out);
        bytes.out.extend_from_slice(window);
        chunk.skip = window.len();
    }
    loop {
        let produced = markers.as_ref().map_or(0, |m| m.out.len()) + chunk.markers.len() + bytes.out.len() - chunk.skip;
        if bits.bit_pos() > start && (bits.bit_pos() >= stop || produced >= max_out) {
            break;
        }
        let room = pause_at.saturating_sub(produced);
        let last = match &mut markers {
            Some(sink) => {
                sink.limit = sink.out.len().saturating_add(room);
                let last = decoder.block(&mut bits, sink)?;
                // switch to plain bytes once no marker can be referenced any more
                if sink.out.len() >= WINDOW && sink.out.len() - WINDOW >= sink.last_marker_end {
                    let sink = markers.take().unwrap();
                    bytes.out.reserve(WINDOW + expected_out);
                    bytes.out.extend(sink.out[sink.out.len() - WINDOW..].iter().map(|&v| v as u8));
                    chunk.skip = WINDOW;
                    chunk.markers = sink.out;
                }
                last
            }
            None => {
                bytes.limit = bytes.out.len().saturating_add(room);
                decoder.block(&mut bits, &mut bytes)?
            }
        };
        match last {
            Some(true) => {
                chunk.last = true;
                break;
            }
            Some(false) => {}
            None => {
                chunk.paused = true;
                break;
            }
        }
    }
    if let Some(sink) = markers {
        chunk.markers = sink.out;
    }
    chunk.bytes = bytes.out;
    chunk.end = bits.bit_pos();
    Ok(chunk)
}

/// Finds the first bit offset in `[from, to)` where a non-final dynamic block starts that decodes cleanly. Gives up
/// once the trial decodes have produced `budget` values in total, so that crafted input (many plausible headers whose
/// blocks decode for a long time before failing) cannot make the search quadratic.
fn find_block_start(decoder: &mut Decoder, data: &[u8], from: usize, to: usize, budget: usize) -> Option<usize> {
    let to = to.min(data.len() * 8);
    let mut budget = budget;
    let mut sink = MarkerSink { out: Vec::with_capacity(1 << 16), last_marker_end: 0, limit: budget };
    for bit in from..to {
        // quick checks: BFINAL = 0, BTYPE = 2, HLIT <= 29, HDIST <= 29
        let byte = bit / 8;
        let b = data.get(byte..byte + 3)?;
        let word = (b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16) >> (bit % 8);
        if word & 0b111 != 0b100 || (word >> 3) & 0x1f > 29 || (word >> 8) & 0x1f > 29 {
            continue;
        }
        let mut bits = Bits::new(data, bit + 3);
        if read_dynamic_tables(&mut bits, &mut decoder.litlen, &mut decoder.dist, &mut decoder.scratch).is_err() {
            continue;
        }
        sink.out.clear();
        sink.limit = budget;
        match decode_huffman_block(&mut bits, &decoder.litlen, &decoder.dist, &mut sink) {
            Ok(true) if !bits.overrun() => return Some(bit),
            Ok(false) => return None, // out of budget
            _ => budget = budget.saturating_sub(sink.out.len()),
        }
    }
    None
}

/// The last up-to-32 KB of real output.
struct Window {
    buf: Vec<u8>,
}

impl Window {
    fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= WINDOW {
            self.buf.clear();
            self.buf.extend_from_slice(&bytes[bytes.len() - WINDOW..]);
        } else {
            self.buf.extend_from_slice(bytes);
            if self.buf.len() > WINDOW {
                self.buf.drain(..self.buf.len() - WINDOW);
            }
        }
    }

    /// Appends `values` to `out`, replacing markers with window bytes.
    fn resolve(&self, values: &[u16], out: &mut Vec<u8>) -> Res<()> {
        let missing = WINDOW - self.buf.len();
        out.reserve(values.len());
        for &v in values {
            out.push(if v < MARKER {
                v as u8
            } else {
                let index = (v - MARKER) as usize;
                *index.checked_sub(missing).and_then(|i| self.buf.get(i)).ok_or(Invalid)?
            });
        }
        Ok(())
    }
}

/// Output handed to the reader without copying: `bytes[skip..]`.
struct Piece {
    bytes: Vec<u8>,
    skip: usize,
}

/// Compressed input for [`ParallelInflater`]. Workers only look at a window of it at a time ([`InflateInput::view`]),
/// so a memory-mapped input can map just that part and drop it afterwards instead of keeping the whole file resident.
pub trait InflateInput: Send + Sync + Sized + 'static {
    type View: Deref<Target = [u8]>;

    /// Length of the input in bytes.
    fn byte_len(&self) -> usize;

    /// Returns bytes `range` of the input (`range.end <= byte_len()`).
    fn view(self: &Arc<Self>, range: Range<usize>) -> io::Result<Self::View>;
}

/// A range of an in-memory input.
pub struct SliceView<T: Deref<Target = [u8]>> {
    data: Arc<T>,
    range: Range<usize>,
}

impl<T: Deref<Target = [u8]>> Deref for SliceView<T> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.data[self.range.clone()]
    }
}

impl InflateInput for Vec<u8> {
    type View = SliceView<Vec<u8>>;

    fn byte_len(&self) -> usize {
        self.len()
    }

    fn view(self: &Arc<Self>, range: Range<usize>) -> io::Result<Self::View> {
        Ok(SliceView { data: self.clone(), range })
    }
}

/// Speculative chunk decodes may read this many bytes past the end of their chunk to finish their last block.
const MARGIN: usize = 1 << 20;

/// A speculative chunk decode gives up (leaving the chunk to the consumer) once it has produced this much more than
/// its output limit, and so does the block-start search. zlib's blocks hold at most 16-32k symbols (< 8.5 MB), but a
/// crafted block can expand ~1000x, which would otherwise cost each worker GBs of memory.
const SPECULATIVE_BLOCK_SLACK: usize = 16 << 20;

/// Decodes chunk `j` speculatively (absolute bit offsets in the result), or `None` if it did not work out.
fn decode_chunk<D: InflateInput>(
    decoder: &mut Decoder,
    data: &Arc<D>,
    j: usize,
    chunk_bytes: usize,
    max_out: usize,
) -> Option<Chunk> {
    decoder.paused = None;
    let base = j * chunk_bytes;
    let view = data.view(base..(base + chunk_bytes + MARGIN).min(data.byte_len())).ok()?;
    let stop = chunk_bytes * 8;
    let pause_at = max_out + SPECULATIVE_BLOCK_SLACK;
    let mut chunk = if j == 0 {
        decode_range(decoder, &view, 0, stop, max_out, pause_at, Some(&[])).ok()?
    } else {
        let start = find_block_start(decoder, &view, 0, stop, pause_at)?;
        decode_range(decoder, &view, start, stop, max_out, pause_at, None).ok()?
    };
    if chunk.paused {
        // a huge block: leave it to the consumer, which can pause and resume
        decoder.paused = None;
        return None;
    }
    chunk.start += base * 8;
    chunk.end += base * 8;
    Some(chunk)
}

/// Decompresses a raw deflate stream from `data` with a pool of worker threads, returning output in order.
pub struct ParallelInflater<D: InflateInput> {
    data: Arc<D>,
    chunk_bits: usize,
    num_chunks: usize,
    jobs: Option<Sender<usize>>,
    results: Receiver<(usize, Option<Chunk>)>,
    pending: BTreeMap<usize, Option<Chunk>>,
    next_job: usize,
    next_chunk: usize,
    /// Bit offset where the next chunk must start.
    pos: usize,
    done: bool,
    window: Window,
    decoder: Decoder,
    queue: std::collections::VecDeque<Piece>,
    offset: usize,
    fallbacks: usize,
    marker_values: usize,
    pool: Pool,
    max_pooled: usize,
    /// Most output a chunk decode produces before stopping early at a block boundary.
    max_out: usize,
    /// Set while the consumer decodes the rest of a chunk's range (up to this bit offset) itself.
    partial_stop: Option<usize>,
    /// The first error, returned again by every later `read` (the state after an error is not resumable).
    failed: Option<(io::ErrorKind, String)>,
}

impl<D: InflateInput> ParallelInflater<D> {
    /// Starts decoding `data` (a raw deflate stream) in chunks of `chunk_size` compressed bytes on `threads` threads.
    pub fn new(data: Arc<D>, threads: usize, chunk_size: usize) -> ParallelInflater<D> {
        Self::with_any_chunk_size(data, threads, chunk_size.max(1 << 16))
    }

    /// Like [`ParallelInflater::new`] but without the 64 KB minimum chunk size, so that fuzzing and tests can make
    /// small inputs cross many chunk boundaries (slow: chunks smaller than the 32 KB window mostly fall back).
    #[doc(hidden)]
    pub fn with_any_chunk_size(data: Arc<D>, threads: usize, chunk_size: usize) -> ParallelInflater<D> {
        let chunk_bits = chunk_size.max(1) * 8;
        let num_chunks = (data.byte_len() * 8).div_ceil(chunk_bits).max(1);
        let threads = threads.max(1);
        let (jobs, job_rx) = crossbeam_channel::unbounded::<usize>();
        let (result_tx, results) = crossbeam_channel::unbounded();
        let pool = Pool::default();
        let max_out = 2 * chunk_bits / 8;
        for _ in 0..threads {
            let (job_rx, result_tx, data, pool) = (job_rx.clone(), result_tx.clone(), data.clone(), pool.clone());
            std::thread::Builder::new()
                .name("inflate".to_string())
                .spawn(move || {
                    let mut decoder = Decoder::new(pool);
                    for j in job_rx {
                        // a panic would lose the result and hang the consumer: report it as a failed guess instead
                        let chunk = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            decode_chunk(&mut decoder, &data, j, chunk_bits / 8, max_out)
                        }))
                        .unwrap_or(None);
                        if result_tx.send((j, chunk)).is_err() {
                            return;
                        }
                    }
                })
                .expect("spawn inflate thread");
        }
        let mut inflater = ParallelInflater {
            data,
            chunk_bits,
            num_chunks,
            jobs: Some(jobs),
            results,
            pending: BTreeMap::new(),
            next_job: 0,
            next_chunk: 0,
            pos: 0,
            done: false,
            window: Window { buf: Vec::with_capacity(2 * WINDOW) },
            decoder: Decoder::new(pool.clone()),
            queue: Default::default(),
            offset: 0,
            fallbacks: 0,
            marker_values: 0,
            pool,
            max_pooled: threads + 4,
            max_out,
            partial_stop: None,
            failed: None,
        };
        // keep a bounded number of chunks in flight
        for _ in 0..threads + 2 {
            inflater.submit_next();
        }
        inflater
    }

    /// Number of chunks so far that had to be decoded sequentially because the speculative decode did not line up.
    pub fn fallbacks(&self) -> usize {
        self.fallbacks
    }

    /// Number of output bytes so far that were decoded as 16-bit values because they might depend on the unknown
    /// window before their chunk.
    pub fn marker_values(&self) -> usize {
        self.marker_values
    }

    /// Number of chunks the input is split into.
    pub fn num_chunks(&self) -> usize {
        self.num_chunks
    }

    fn submit_next(&mut self) {
        if self.next_job < self.num_chunks
            && let Some(jobs) = &self.jobs
        {
            let _ = jobs.send(self.next_job);
            self.next_job += 1;
        }
    }

    /// Produces the next piece of output into `self.queue`.
    fn advance(&mut self) -> io::Result<()> {
        let invalid = |_| io::Error::new(io::ErrorKind::InvalidData, "invalid deflate stream");
        if let Some(stop) = self.partial_stop {
            // decode the rest of this chunk's range with the real window, a bounded piece at a time
            let window = std::mem::take(&mut self.window.buf);
            let base = self.pos / 8;
            let view = self.data.view(base..self.data.byte_len())?;
            let start = self.pos - base * 8;
            // pauses mid-block at max_out too, so one huge block is handed out in bounded pieces
            let (stop_bit, max_out) = (stop.saturating_sub(base * 8), self.max_out); // < start when resuming past stop
            let result = decode_range(&mut self.decoder, &view, start, stop_bit, max_out, max_out, Some(&window));
            self.window.buf = window;
            let mut chunk = result.map_err(invalid)?;
            chunk.start += base * 8;
            chunk.end += base * 8;
            self.window.push(&chunk.bytes[chunk.skip..]);
            self.finish(chunk, stop);
            return Ok(());
        }
        let j = self.next_chunk;
        if j >= self.num_chunks {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "deflate stream has no final block"));
        }
        let result = loop {
            if let Some(result) = self.pending.remove(&j) {
                break result;
            }
            match self.results.recv() {
                Ok((i, chunk)) => {
                    self.pending.insert(i, chunk);
                }
                Err(_) => return Err(io::Error::other("inflate threads stopped")),
            }
        };
        self.next_chunk += 1;
        self.submit_next();
        let stop = (j + 1) * self.chunk_bits;
        if self.pos >= stop {
            return Ok(()); // the previous chunk's last block covered this whole chunk
        }
        match result {
            Some(mut chunk) if chunk.start == self.pos => {
                self.marker_values += chunk.markers.len();
                let mut resolved = self.decoder.buffer();
                self.window.resolve(&chunk.markers, &mut resolved).map_err(invalid)?;
                self.window.push(&resolved);
                self.window.push(&chunk.bytes[chunk.skip..]);
                self.queue.push_back(Piece { bytes: resolved, skip: 0 });
                let markers = std::mem::take(&mut chunk.markers);
                let mut pool = self.pool.lock();
                if pool.values.len() < self.max_pooled && markers.capacity() > 0 {
                    pool.values.push(markers);
                }
                drop(pool);
                self.finish(chunk, stop);
            }
            _ => {
                // wrong guess or no block start found: decode this range with the real window
                self.fallbacks += 1;
                self.partial_stop = Some(stop);
            }
        }
        Ok(())
    }

    /// Queues the output of `chunk`, which decoded part of the range that ends at bit `stop`.
    fn finish(&mut self, chunk: Chunk, stop: usize) {
        self.pos = chunk.end;
        self.done = chunk.last;
        // a paused chunk ends inside a block: the consumer continues it (`self.decoder` holds the block's state), so
        // a mid-block position is never compared with a worker's guess
        self.partial_stop = (!chunk.last && (chunk.end < stop || chunk.paused)).then_some(stop);
        self.queue.push_back(Piece { bytes: chunk.bytes, skip: chunk.skip });
        if self.done {
            self.jobs = None;
        }
    }
}

impl<D: InflateInput> Read for ParallelInflater<D> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Some(piece) = self.queue.front() {
                let rest = &piece.bytes[piece.skip + self.offset..];
                if !rest.is_empty() {
                    let n = rest.len().min(buf.len());
                    buf[..n].copy_from_slice(&rest[..n]);
                    self.offset += n;
                    return Ok(n);
                }
                if let Some(piece) = self.queue.pop_front() {
                    let mut pool = self.pool.lock();
                    if pool.bytes.len() < 2 * self.max_pooled && piece.bytes.capacity() > 0 {
                        pool.bytes.push(piece.bytes);
                    }
                }
                self.offset = 0;
                continue;
            }
            if self.done {
                return Ok(0);
            }
            if let Some((kind, message)) = &self.failed {
                return Err(io::Error::new(*kind, message.clone()));
            }
            if let Err(e) = self.advance() {
                self.failed = Some((e.kind(), e.to_string()));
                return Err(e);
            }
        }
    }
}

impl<D: InflateInput> Drop for ParallelInflater<D> {
    fn drop(&mut self) {
        // workers exit once the job channel is closed and their current chunk is done
        self.jobs = None;
    }
}

/// Sequentially decodes a raw deflate stream with this module's decoder (for tests).
#[cfg(test)]
fn inflate_sequential(data: &[u8]) -> Res<Vec<u8>> {
    let max = usize::MAX;
    let chunk = decode_range(&mut Decoder::new(Pool::default()), data, 0, max, max, max, Some(&[]))?;
    if !chunk.last {
        return Err(Invalid);
    }
    Ok(chunk.bytes)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn deflate(data: &[u8], level: u32) -> Vec<u8> {
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// Semi-compressible test data: little-endian coordinates on a random walk, plus some text and runs.
    fn test_data(n: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        let mut state = seed;
        let mut x = 0f64;
        while out.len() < n {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            match state >> 60 {
                0 => out.extend_from_slice(b"the quick brown fox jumps over the lazy dog "),
                1 => out.extend(std::iter::repeat_n((state >> 8) as u8, (state >> 20) as usize % 300)),
                _ => {
                    x += ((state >> 11) as f64 / (1u64 << 53) as f64) - 0.5;
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
        }
        out.truncate(n);
        out
    }

    fn parallel(compressed: Vec<u8>, threads: usize, chunk: usize) -> io::Result<Vec<u8>> {
        let mut out = vec![];
        ParallelInflater::new(Arc::new(compressed), threads, chunk).read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn sequential_matches_flate2() {
        for level in [0, 1, 6, 9] {
            for (n, seed) in [(0, 1), (1, 2), (1000, 3), (300_000, 4)] {
                let data = test_data(n, seed);
                assert_eq!(data, inflate_sequential(&deflate(&data, level)).unwrap(), "level {level} n {n}");
            }
        }
        assert!(inflate_sequential(&[0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn long_codes() {
        // geometrically distributed bytes give 15-bit literal codes
        let mut state = 1u64;
        let data: Vec<u8> = (0..2_000_000)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((state >> 11) | 1).trailing_zeros().min(40) as u8 * 6 + (state >> 60) as u8
            })
            .collect();
        let compressed = deflate(&data, 6);
        assert_eq!(data, inflate_sequential(&compressed).unwrap());
        assert_eq!(data, parallel(compressed, 4, 1 << 16).unwrap());
    }

    #[test]
    fn parallel_matches_input() {
        for level in [1, 6, 9] {
            let data = test_data(3_000_000, level as u64);
            let compressed = deflate(&data, level);
            for (threads, chunk) in [(1, 1 << 16), (4, 1 << 16), (8, 100_000), (3, 1 << 30)] {
                assert_eq!(data, parallel(compressed.clone(), threads, chunk).unwrap(), "{level} {threads} {chunk}");
            }
        }
    }

    #[test]
    fn parallel_bounds_output_per_piece_on_highly_compressible_data() {
        // long zero runs (like free pages in a sqlite file) expand ~1000x
        let mut data = vec![0u8; 60_000_000];
        for (i, b) in test_data(400_000, 9).into_iter().enumerate() {
            let len = data.len();
            data[i * 149 % len] = b;
        }
        data[20_000_000..20_400_000].copy_from_slice(&test_data(400_000, 10));
        let compressed = deflate(&data, 6);
        let mut inflater = ParallelInflater::new(Arc::new(compressed), 4, 1 << 16);
        let mut out = vec![];
        let mut buf = vec![0u8; 1 << 16];
        let mut max_queued = 0;
        loop {
            let n = inflater.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            max_queued = max_queued.max(inflater.queue.iter().map(|p| p.bytes.len()).sum::<usize>());
        }
        assert!(out == data);
        // a chunk's pieces stop at a block boundary after max_out bytes (blocks of zeros are up to 16k * 258 bytes)
        assert!(max_queued < 2 * (inflater.max_out + (5 << 20)), "{max_queued}");
    }

    #[test]
    fn parallel_handles_stored_and_tiny_streams() {
        let data = test_data(1_000_000, 7);
        assert_eq!(data, parallel(deflate(&data, 0), 4, 1 << 16).unwrap());
        assert_eq!(b"".to_vec(), parallel(deflate(b"", 6), 4, 1 << 16).unwrap());
        assert_eq!(b"abc".to_vec(), parallel(deflate(b"abc", 6), 4, 1 << 16).unwrap());
    }

    #[test]
    fn parallel_rejects_corrupt_streams() {
        let data = test_data(1_000_000, 8);
        let mut compressed = deflate(&data, 6);
        let len = compressed.len();
        compressed.truncate(len / 2);
        assert!(parallel(compressed, 4, 1 << 16).is_err());
    }

    // Hand-made edge cases (from the differential fuzzer in `planetiler-benchmarks/src/bin/fuzz_parallel_inflate.rs`).
    // Each one is checked against zlib-rs, sequentially and in parallel with chunks down to 1 byte.

    /// LSB-first bit writer for hand-made streams.
    #[derive(Default)]
    struct BitWriter {
        out: Vec<u8>,
        buf: u64,
        n: u32,
    }

    impl BitWriter {
        fn put(&mut self, v: u32, bits: u32) -> &mut Self {
            self.buf |= ((v as u64) & ((1 << bits) - 1)) << self.n;
            self.n += bits;
            while self.n >= 8 {
                self.out.push(self.buf as u8);
                self.buf >>= 8;
                self.n -= 8;
            }
            self
        }

        /// A Huffman code (most significant bit first).
        fn code(&mut self, code: u32, len: u32) -> &mut Self {
            self.put(code.reverse_bits() >> (32 - len), len)
        }

        /// A fixed-Huffman literal/length symbol.
        fn fixed(&mut self, sym: u32) -> &mut Self {
            match sym {
                0..=143 => self.code(0x30 + sym, 8),
                144..=255 => self.code(0x190 + sym - 144, 9),
                256..=279 => self.code(sym - 256, 7),
                _ => self.code(0xc0 + sym - 280, 8),
            }
        }

        fn stored(&mut self, last: bool, bytes: &[u8]) -> &mut Self {
            self.put(last as u32, 1).put(0, 2);
            if self.n > 0 {
                self.put(0, 8 - self.n);
            }
            self.put(bytes.len() as u32, 16).put(!bytes.len() as u32, 16);
            for &b in bytes {
                self.put(b as u32, 8);
            }
            self
        }

        fn finish(&mut self) -> Vec<u8> {
            if self.n > 0 {
                self.put(0, 8 - self.n);
            }
            std::mem::take(&mut self.out)
        }
    }

    /// zlib-rs's output, or `None` if it rejects the stream (including a missing final block).
    fn zlib(data: &[u8]) -> Option<Vec<u8>> {
        let mut d = flate2::Decompress::new(false);
        let mut out = Vec::with_capacity(1 << 20);
        loop {
            out.reserve(1 << 20);
            let (i, o) = (d.total_in(), d.total_out());
            match d.decompress_vec(&data[i as usize..], &mut out, flate2::FlushDecompress::None) {
                Ok(flate2::Status::StreamEnd) => return Some(out),
                Ok(_) if d.total_in() == i && d.total_out() == o => return None,
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }

    /// Decodes `data` like zlib-rs does, sequentially and in parallel (also with tiny chunks), and returns that.
    fn check_against_zlib(data: &[u8]) -> Option<Vec<u8>> {
        let expected = zlib(data);
        assert_eq!(expected, inflate_sequential(data).ok(), "sequential");
        for (threads, chunk) in [(1, 1 << 16), (4, 1), (3, 7), (4, 300)] {
            let mut inflater = ParallelInflater::with_any_chunk_size(Arc::new(data.to_vec()), threads, chunk);
            let mut out = vec![];
            let result = inflater.read_to_end(&mut out).map(|_| out);
            assert_eq!(expected, result.ok(), "{threads} threads, {chunk} byte chunks");
        }
        expected
    }

    #[test]
    fn edge_case_huffman_codes() {
        // empty fixed block
        assert_eq!(Some(vec![]), check_against_zlib(&BitWriter::default().put(1, 1).put(1, 2).fixed(256).finish()));

        // dynamic block whose literal/length code is a single 1-bit code (end of block) and whose distance code is
        // empty: both allowed. Code length code: 18 -> 0, 0 -> 10, 1 -> 11.
        let mut w = BitWriter::default();
        w.put(1, 1).put(2, 2).put(0, 5).put(0, 5).put(18 - 4, 4);
        for &sym in &CL_ORDER[..18] {
            w.put(
                match sym {
                    18 => 1,
                    0 | 1 => 2,
                    _ => 0,
                },
                3,
            );
        }
        // 256 zeros (138 + 118), a 1 for end-of-block, a 0 for the only distance code
        w.code(0, 1).put(127, 7).code(0, 1).put(107, 7).code(0b11, 2).code(0b10, 2);
        let single_code = w.code(0, 1).finish();
        assert_eq!(Some(vec![]), check_against_zlib(&single_code));

        // an incomplete code with more than one symbol is not allowed: same, but end-of-block gets a 2-bit code
        let mut w = BitWriter::default();
        w.put(1, 1).put(2, 2).put(0, 5).put(0, 5).put(18 - 4, 4);
        for &sym in &CL_ORDER[..18] {
            w.put(
                match sym {
                    18 => 1,
                    0 | 2 => 2,
                    _ => 0,
                },
                3,
            );
        }
        w.code(0, 1).put(127, 7).code(0, 1).put(107, 7).code(0b11, 2).code(0b10, 2);
        assert_eq!(None, check_against_zlib(&w.code(0, 2).finish()));

        // a code length repeat (16) that runs from the literal/length lengths into the distance lengths is allowed.
        // Lengths: 97 -> 1, 256 -> 2, 257 -> 2, distances 0-3 -> 2 (HLIT = 1, HDIST = 3). Code length code: all
        // 2 bits (1 -> 00, 2 -> 01, 16 -> 10, 18 -> 11).
        let mut w = BitWriter::default();
        w.put(1, 1).put(2, 2).put(1, 5).put(3, 5).put(18 - 4, 4);
        for &sym in &CL_ORDER[..18] {
            w.put(if matches!(sym, 1 | 2 | 16 | 18) { 2 } else { 0 }, 3);
        }
        w.code(0b11, 2).put(97 - 11, 7); // 0-96: 0
        w.code(0b00, 2); // 97: 1
        w.code(0b11, 2).put(138 - 11, 7).code(0b11, 2).put(20 - 11, 7); // 98-255: 0
        w.code(0b01, 2); // 256: 2
        w.code(0b10, 2).put(5 - 3, 2); // 257 and distances 0-3: 2
        // literal 'a' (0), length 3 (257 -> 11) at distance 1 (00), end of block (10)
        w.code(0, 1).code(0b11, 2).code(0b00, 2).code(0b10, 2);
        assert_eq!(Some(b"aaaa".to_vec()), check_against_zlib(&w.finish()));

        // a repeat (16) as the first code length has nothing to repeat
        let mut w = BitWriter::default();
        w.put(1, 1).put(2, 2).put(1, 5).put(3, 5).put(18 - 4, 4);
        for &sym in &CL_ORDER[..18] {
            w.put(if matches!(sym, 1 | 2 | 16 | 18) { 2 } else { 0 }, 3);
        }
        w.code(0b10, 2).put(0, 2);
        assert_eq!(None, check_against_zlib(&w.finish()));

        // HLIT = 30 (287 literal/length codes) is too many
        let mut w = BitWriter::default();
        w.put(1, 1).put(2, 2).put(30, 5).put(0, 5).put(0, 4).put(0, 32);
        assert_eq!(None, check_against_zlib(&w.finish()));
    }

    #[test]
    fn edge_case_matches_and_blocks() {
        // "ab" + (length 3, distance 2): fine; distance 3 reaches before the start of the stream
        let stream = |dist_sym: u32| {
            let mut w = BitWriter::default();
            w.put(1, 1).put(1, 2).fixed(b'a' as u32).fixed(b'b' as u32).fixed(257).code(dist_sym, 5);
            w.fixed(256).finish()
        };
        assert_eq!(Some(b"ababa".to_vec()), check_against_zlib(&stream(1)));
        assert_eq!(None, check_against_zlib(&stream(2)));

        // length 258 written as symbol 284 + 31 (zlib accepts it), distance 1
        let mut w = BitWriter::default();
        w.put(1, 1).put(1, 2).fixed(b'x' as u32).fixed(284).put(31, 5).code(0, 5).fixed(256);
        assert_eq!(Some(vec![b'x'; 259]), check_against_zlib(&w.finish()));

        // distance 32768 after exactly 32768 bytes of history (stored block); one byte less is too far
        let history: Vec<u8> = (0..32768u32).map(|i| (i * 7 % 251) as u8).collect();
        for n in [32768, 32767] {
            let mut w = BitWriter::default();
            w.stored(false, &history[..n]);
            w.put(1, 1).put(1, 2).fixed(285).code(29, 5).put(32768 - 24577, 13).fixed(256);
            let mut expected = history[..n].to_vec();
            expected.extend_from_slice(&history[..258]);
            assert_eq!((n == 32768).then_some(expected), check_against_zlib(&w.finish()), "{n}");
        }

        // reserved symbols in fixed blocks: literal/length 286 and 287, distance 30 and 31
        for sym in [286, 287] {
            assert_eq!(None, check_against_zlib(&BitWriter::default().put(1, 1).put(1, 2).fixed(sym).finish()));
        }
        for dist_sym in [30, 31] {
            let mut w = BitWriter::default();
            w.put(1, 1).put(1, 2).fixed(b'a' as u32).fixed(257).code(dist_sym, 5).fixed(256);
            assert_eq!(None, check_against_zlib(&w.finish()));
        }

        // stored block with LEN != ~NLEN, block type 3, no final block, empty input
        let mut bad_len = BitWriter::default().stored(true, b"abc").finish();
        bad_len[3] ^= 1;
        assert_eq!(None, check_against_zlib(&bad_len));
        assert_eq!(None, check_against_zlib(&BitWriter::default().put(1, 1).put(3, 2).put(0, 16).finish()));
        assert_eq!(None, check_against_zlib(&BitWriter::default().stored(false, b"abc").finish()));
        assert_eq!(None, check_against_zlib(&[]));

        // many empty stored blocks (5 bytes each, no output) around some data, crossing many chunks
        let mut w = BitWriter::default();
        for i in 0..2000 {
            w.stored(false, if i == 1000 { b"hello" } else { b"" });
        }
        assert_eq!(Some(b"hello world".to_vec()), check_against_zlib(&w.stored(true, b" world").finish()));
    }

    #[test]
    fn errors_are_sticky() {
        let data = test_data(500_000, 11);
        let mut compressed = deflate(&data, 6);
        let len = compressed.len();
        compressed[len / 2] ^= 0x10;
        compressed.truncate(len * 3 / 4);
        let mut inflater = ParallelInflater::with_any_chunk_size(Arc::new(compressed), 4, 10_000);
        let mut out = vec![];
        assert!(inflater.read_to_end(&mut out).is_err());
        let mut buf = [0u8; 100];
        assert!(inflater.read(&mut buf).is_err(), "a read after an error must fail again, not report the end");
    }

    #[test]
    fn huge_single_block_is_handed_out_in_bounded_pieces() {
        // one fixed block of 258-byte matches at distance 1 (13 bits each): 24 MB of output from ~150 KB. A worker
        // used to decode the whole block into one piece (a crafted block can expand ~1000x, so GBs per worker)
        let mut w = BitWriter::default();
        w.put(1, 1).put(1, 2).fixed(b'z' as u32);
        for _ in 0..(24 << 20) / 258 {
            w.fixed(285).code(0, 5);
        }
        let compressed = w.fixed(256).finish();
        let expected_len = 1 + (24 << 20) / 258 * 258;
        let mut inflater = ParallelInflater::new(Arc::new(compressed), 4, 1 << 16);
        let mut buf = vec![0u8; 1 << 16];
        let (mut total, mut max_piece) = (0, 0);
        loop {
            let n = inflater.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            assert!(buf[..n].iter().all(|&b| b == b'z'));
            total += n;
            max_piece = max_piece.max(inflater.queue.iter().map(|p| p.bytes.len()).max().unwrap_or(0));
        }
        assert_eq!(expected_len, total);
        assert!(max_piece < 4 * inflater.max_out, "{max_piece}");
    }

    #[test]
    fn block_start_search_is_bounded_on_crafted_input() {
        // copies of a dynamic block header whose literal code decodes zeros (and every other bit pattern in it) as
        // literals and has no reachable end-of-block code, so every copy is a plausible block start whose block runs
        // to the end of the input and fails there. Without a budget the search decodes ~n^2 / 2 symbols.
        // Lengths: 0-254 -> 8, 255 and 256 -> 9 (end of block is 9 one bits), distance 0 -> 0. Code length code:
        // 8 -> 0, 0 -> 10, 9 -> 11.
        let mut w = BitWriter::default();
        w.put(0, 1).put(2, 2).put(0, 5).put(0, 5).put(7 - 4, 4);
        for &sym in &CL_ORDER[..7] {
            w.put(
                match sym {
                    8 => 1,
                    0 | 9 => 2,
                    _ => 0,
                },
                3,
            );
        }
        for _ in 0..255 {
            w.code(0, 1);
        }
        w.code(0b11, 2).code(0b11, 2).code(0b10, 2);
        let header = w.finish();
        assert!(header.len() <= 40);
        let mut data = vec![];
        while data.len() < 256 << 10 {
            data.extend_from_slice(&header);
            data.resize(data.len().next_multiple_of(64), 0);
        }
        let mut decoder = Decoder::new(Pool::default());
        assert_eq!(None, find_block_start(&mut decoder, &data, 0, data.len() * 8, 1 << 20));
        // the whole stream is invalid too
        assert_eq!(None, check_against_zlib(&data[..20_000]));
    }
}
