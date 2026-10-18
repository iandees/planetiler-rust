//! Port of the pure-Java FSST encoder used by MLT 0.0.10 (`FsstJava` -> `SymbolTableBuilder`, itself a port of
//! [cwida/fsst](https://github.com/cwida/fsst), MIT license).
//!
//! The symbol table that Java builds depends on the iteration order of a `java.util.HashMap<Symbol, Long>` (which
//! feeds a `java.util.PriorityQueue` that orders only by gain, so ties are broken by heap position). To produce the
//! same symbol tables as Java this module emulates both: [`Candidates`] reproduces the iteration order of a
//! `HashMap` populated with `merge` (see there) and [`JavaPriorityQueue`] reproduces the binary heap's `siftUp` /
//! `siftDown`. Java also hashes symbols with a quirk (the hash of a truncated concatenation includes the truncated
//! bytes), which [`Symbol`] keeps since it decides which candidates are merged.
//!
//! Unlike Java, the encoder allocates almost nothing per call: the 1 MB pair counter matrix, the 64k-entry
//! first-two-bytes index of each symbol table and the candidate map live in a per-thread [`Scratch`] and are cleared
//! sparsely (only the entries that were written), and only the touched pair counters are visited when building the
//! next table. The results are identical to the straight port (kept as `fsst_reference` for differential tests).

use std::cell::RefCell;

const MAX_SYMBOL_LENGTH: usize = 8;
const NUM_ITERS: usize = 6;
const DEFAULT_SAMPLE_SIZE: usize = 30_000;
/// Codes 0-255 are single bytes, 256-510 the (up to 255) symbols of a table.
const MAX_CODES: usize = 512;
const FIRST2_LEN: usize = (1 << 16) + 1;

/// Number of times a candidate hash map bin would have been converted to a tree by Java (whose iteration order is
/// not emulated), for diagnostics.
pub static TREEIFIED_BINS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Output of FSST-encoding (`SymbolTable` record).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolTable {
    /// Up to 255 symbols concatenated together.
    pub symbols: Vec<u8>,
    /// Lengths of each symbol.
    pub symbol_lengths: Vec<u32>,
    /// Encoded text: a value below 255 refers to a symbol, 255 escapes the next byte.
    pub compressed_data: Vec<u8>,
    pub decompressed_length: usize,
}

/// A candidate symbol (`Symbol`): up to 8 bytes plus Java's precomputed hash code.
#[derive(Clone, Copy, Debug)]
struct Symbol {
    bytes: [u8; MAX_SYMBOL_LENGTH],
    len: u8,
    hash: i32,
}

impl Symbol {
    const EMPTY: Symbol = Symbol { bytes: [0; MAX_SYMBOL_LENGTH], len: 0, hash: 0 };

    fn of(b: u8) -> Symbol {
        let mut bytes = [0; MAX_SYMBOL_LENGTH];
        bytes[0] = b;
        Symbol { bytes, len: 1, hash: 31i32.wrapping_add(b as i8 as i32) }
    }

    #[inline]
    fn concat(a: &Symbol, b: &Symbol) -> Symbol {
        let len = MAX_SYMBOL_LENGTH.min(a.len as usize + b.len as usize);
        let mut bytes = a.bytes;
        let todo = len - a.len as usize;
        bytes[a.len as usize..len].copy_from_slice(&b.bytes[..todo]);
        let mut hash = a.hash;
        for &bb in b.as_slice() {
            hash = hash.wrapping_mul(31).wrapping_add(bb as i8 as i32);
        }
        Symbol { bytes, len: len as u8, hash }
    }

    #[inline]
    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    #[inline]
    fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    fn first(&self) -> usize {
        self.bytes[0] as usize
    }

    /// The bytes as a little-endian word (unused bytes are 0).
    #[inline]
    fn word(&self) -> u64 {
        u64::from_le_bytes(self.bytes)
    }

    /// Java `Symbol.equals` + `hashCode` agreement as used by `HashMap`: same hash and same bytes.
    #[inline]
    fn same_key(&self, other: &Symbol) -> bool {
        self.hash == other.hash && self.len == other.len && self.word() == other.word()
    }

    /// Mask of the low `len` bytes of a little-endian word.
    #[inline]
    fn byte_mask(len: usize) -> u64 {
        if len >= 8 { u64::MAX } else { (1u64 << (8 * len)) - 1 }
    }

    /// `Symbol.compareTo`: lexicographic (unsigned) except longer symbols sort before their prefixes.
    #[inline]
    fn compare(&self, other: &Symbol) -> std::cmp::Ordering {
        // the common length's bytes compared as big-endian words is the lexicographic comparison
        let common = self.len().min(other.len());
        let mask = Self::byte_mask(common);
        let a = (self.word() & mask).swap_bytes();
        let b = (other.word() & mask).swap_bytes();
        a.cmp(&b).then_with(|| other.len().cmp(&self.len()))
    }

    /// `Symbol.match`: true if this symbol appears at `offset` in `text`, skipping the first `ignore` bytes.
    #[inline]
    fn matches(&self, text: &[u8], offset: usize, ignore: usize) -> bool {
        let len = self.len();
        if text.len() < offset + len {
            return false;
        }
        if ignore != 2 || len < 2 {
            return text[offset + ignore..offset + len] == self.bytes[ignore..len];
        }
        // Only called with `ignore` = 2 for symbols whose first two bytes are known to match (see
        // `find_longest_symbol`), so comparing all `len` bytes at once gives the same answer.
        let word = match text.get(offset..offset + 8) {
            Some(bytes) => u64::from_le_bytes(bytes.try_into().expect("8 bytes")),
            None => {
                let mut buf = [0u8; 8];
                buf[..text.len() - offset].copy_from_slice(&text[offset..]);
                u64::from_le_bytes(buf)
            }
        };
        (word ^ self.word()) & Self::byte_mask(len) == 0
    }
}

/// The candidate symbols of [`SymbolTableBuilder::make_table`]: a `java.util.HashMap<Symbol, Long>` populated only
/// with `merge(k, v, Long::sum)`, iterated in Java's order.
///
/// Java's iteration order follows from how `HashMap` works: `merge` inserts a new key at the head of its bin, and a
/// resize splits every bin into two while preserving the relative order of its nodes. Two keys that end up in the
/// same bin of the final table were in the same bin at every smaller size too, so the later one was inserted in
/// front of the earlier one and stays there: the iteration order is by bin of the final table, then by descending
/// insertion order. So only the final table size has to be emulated: resizes happen before an insert when the
/// size exceeds the threshold, and when a bin of a table with fewer than 64 bins gets its 8th node (where Java
/// resizes instead of converting the bin to a tree), so the bin sizes of the current table are tracked. Bins that
/// Java does convert to trees (8+ colliding keys in a table of 64+ bins) iterate in a different order that is not
/// emulated (they are counted in [`TREEIFIED_BINS`]); the symbol table is still valid then, it may just differ from
/// Java's in rare cases.
///
/// Lookups use a separate open-addressing index.
struct Candidates {
    entries: Vec<(Symbol, i64)>,
    /// Size of Java's table (0 before the first insert) and its resize threshold.
    java_table_len: usize,
    threshold: usize,
    /// Number of keys in each bin of Java's table.
    bin_counts: Vec<u32>,
    /// Open-addressing index of `entries` (`u32::MAX` = empty), a power of two longer than twice the entries.
    slots: Vec<u32>,
    /// Slot of each entry (to clear the index).
    entry_slots: Vec<u32>,
}

impl Candidates {
    /// A placeholder that allocates nothing (not usable until replaced).
    fn new_empty() -> Candidates {
        Candidates {
            entries: Vec::new(),
            java_table_len: 0,
            threshold: 0,
            bin_counts: Vec::new(),
            slots: Vec::new(),
            entry_slots: Vec::new(),
        }
    }

    fn new() -> Candidates {
        Candidates {
            entries: Vec::new(),
            java_table_len: 0,
            threshold: 0,
            bin_counts: Vec::new(),
            slots: vec![u32::MAX; 1024],
            entry_slots: Vec::new(),
        }
    }

    /// `HashMap.hash`.
    #[inline]
    fn spread(h: i32) -> u32 {
        let h = h as u32;
        h ^ (h >> 16)
    }

    #[inline]
    fn home_slot(symbol: &Symbol, mask: usize) -> usize {
        let h = (symbol.word() ^ ((symbol.hash as u32 as u64) << 32 | symbol.len as u64))
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (h >> 32) as usize & mask
    }

    /// Empties the map, keeping its allocations.
    fn clear(&mut self) {
        for &slot in &self.entry_slots {
            self.slots[slot as usize] = u32::MAX;
        }
        self.entries.clear();
        self.entry_slots.clear();
        self.java_table_len = 0;
        self.threshold = 0;
    }

    /// Java `HashMap.resize` (only its effect on the table size and bin sizes).
    fn java_resize(&mut self) {
        let (new_len, new_thr) =
            if self.java_table_len == 0 { (16, 12) } else { (self.java_table_len * 2, self.threshold * 2) };
        self.java_table_len = new_len;
        self.threshold = new_thr;
        self.bin_counts.clear();
        self.bin_counts.resize(new_len, 0);
        for (symbol, _) in &self.entries {
            self.bin_counts[Self::spread(symbol.hash) as usize & (new_len - 1)] += 1;
        }
    }

    fn grow_index(&mut self) {
        let new_len = self.slots.len() * 2;
        self.slots.clear();
        self.slots.resize(new_len, u32::MAX);
        let mask = new_len - 1;
        for (idx, (symbol, _)) in self.entries.iter().enumerate() {
            let mut slot = Self::home_slot(symbol, mask);
            while self.slots[slot] != u32::MAX {
                slot = (slot + 1) & mask;
            }
            self.slots[slot] = idx as u32;
            self.entry_slots[idx] = slot as u32;
        }
    }

    /// `HashMap.merge(key, value, Long::sum)`.
    fn merge(&mut self, key: Symbol, value: i64) {
        if self.entries.len() > self.threshold || self.java_table_len == 0 {
            self.java_resize();
        }
        let mask = self.slots.len() - 1;
        let mut slot = Self::home_slot(&key, mask);
        loop {
            let idx = self.slots[slot];
            if idx == u32::MAX {
                break;
            }
            let entry = &mut self.entries[idx as usize];
            if entry.0.same_key(&key) {
                entry.1 += value;
                return;
            }
            slot = (slot + 1) & mask;
        }
        let bin = Self::spread(key.hash) as usize & (self.java_table_len - 1);
        let bin_count = self.bin_counts[bin];
        self.bin_counts[bin] += 1;
        let idx = self.entries.len();
        self.entries.push((key, value));
        self.slots[slot] = idx as u32;
        self.entry_slots.push(slot as u32);
        if self.entries.len() * 2 > self.slots.len() {
            self.grow_index();
        }
        if bin_count >= 7 {
            // TREEIFY_THRESHOLD - 1: Java resizes small tables instead of converting the bin to a tree.
            if self.java_table_len < 64 {
                self.java_resize();
            } else {
                TREEIFIED_BINS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// Writes the entry indexes in Java iteration order to `order`.
    fn java_order(&self, order: &mut Vec<u32>, starts: &mut Vec<u32>) {
        let n = self.java_table_len;
        starts.clear();
        starts.reserve(n + 1);
        let mut sum = 0u32;
        for &c in &self.bin_counts[..n] {
            starts.push(sum);
            sum += c;
        }
        order.clear();
        order.resize(self.entries.len(), 0);
        // bins in table order, the newest entry first within a bin
        for (idx, (symbol, _)) in self.entries.iter().enumerate().rev() {
            let bin = Self::spread(symbol.hash) as usize & (n - 1);
            order[starts[bin] as usize] = idx as u32;
            starts[bin] += 1;
        }
    }
}

/// `QSymbol` (gain and candidate index) in a `java.util.PriorityQueue` ordered by gain descending.
struct JavaPriorityQueue {
    heap: Vec<(i64, u32)>,
}

impl JavaPriorityQueue {
    /// `QSymbol.compareTo`: `Long.compare(o.gain, gain)`.
    #[inline]
    fn cmp(a: &(i64, u32), b: &(i64, u32)) -> std::cmp::Ordering {
        b.0.cmp(&a.0)
    }

    fn add(&mut self, x: (i64, u32)) {
        let mut k = self.heap.len();
        self.heap.push(x);
        while k > 0 {
            let parent = (k - 1) >> 1;
            if Self::cmp(&x, &self.heap[parent]).is_ge() {
                break;
            }
            self.heap[k] = self.heap[parent];
            k = parent;
        }
        self.heap[k] = x;
    }

    fn poll(&mut self) -> Option<(i64, u32)> {
        if self.heap.is_empty() {
            return None;
        }
        let result = self.heap[0];
        let x = self.heap.pop().expect("non-empty");
        let n = self.heap.len();
        if n > 0 {
            let mut k = 0;
            let half = n >> 1;
            while k < half {
                let mut child = (k << 1) + 1;
                let right = child + 1;
                if right < n && Self::cmp(&self.heap[child], &self.heap[right]).is_gt() {
                    child = right;
                }
                if Self::cmp(&x, &self.heap[child]).is_le() {
                    break;
                }
                self.heap[k] = self.heap[child];
                k = child;
            }
            self.heap[k] = x;
        }
        Some(result)
    }
}

/// Single symbol counts (`count1`) and which of them are non-zero.
struct Counts1 {
    counts: [i32; MAX_CODES],
    touched: [u64; MAX_CODES / 64],
}

impl Counts1 {
    fn new() -> Counts1 {
        Counts1 { counts: [0; MAX_CODES], touched: [0; MAX_CODES / 64] }
    }

    #[inline]
    fn inc(&mut self, code: usize) {
        self.counts[code] += 1;
        self.touched[code >> 6] |= 1 << (code & 63);
    }

    /// Calls `f(code, count)` for every non-zero count in increasing code order.
    #[inline]
    fn for_each(&self, mut f: impl FnMut(usize, i32)) {
        for (word_index, &word) in self.touched.iter().enumerate() {
            let mut w = word;
            while w != 0 {
                let code = word_index * 64 + w.trailing_zeros() as usize;
                w &= w - 1;
                f(code, self.counts[code]);
            }
        }
    }

    fn clear(&mut self) {
        for word_index in 0..self.touched.len() {
            let mut w = self.touched[word_index];
            while w != 0 {
                self.counts[word_index * 64 + w.trailing_zeros() as usize] = 0;
                w &= w - 1;
            }
            self.touched[word_index] = 0;
        }
    }

    fn copy_from(&mut self, other: &Counts1) {
        self.clear();
        other.for_each(|code, count| self.counts[code] = count);
        self.touched = other.touched;
    }
}

/// Per-thread buffers reused by every [`encode`] call.
struct Scratch {
    /// Single symbol counts (`count1`).
    count1: Counts1,
    /// `count1` of the best table so far.
    best_count1: Counts1,
    /// Pair counts (`count2`), indexed by `code1 << 9 | code2`; all zero between uses.
    count2: Vec<i32>,
    /// One bit per `count2` entry that was incremented.
    count2_touched: Vec<u64>,
    /// One bit per `count2` row (`code1`) that was incremented.
    count2_rows: [u64; MAX_CODES / 64],
    /// Zeroed buffers for [`SymbolTableBuilder`]s.
    pool: Vec<TableBuffers>,
    candidates: Candidates,
    queue: JavaPriorityQueue,
    order: Vec<u32>,
    starts: Vec<u32>,
}

impl Scratch {
    fn new() -> Scratch {
        Scratch {
            count1: Counts1::new(),
            best_count1: Counts1::new(),
            count2: vec![0; MAX_CODES * MAX_CODES],
            count2_touched: vec![0; MAX_CODES * MAX_CODES / 64],
            count2_rows: [0; MAX_CODES / 64],
            pool: Vec::new(),
            candidates: Candidates::new(),
            queue: JavaPriorityQueue { heap: Vec::new() },
            order: Vec::new(),
            starts: Vec::new(),
        }
    }

    fn take_buffers(&mut self) -> TableBuffers {
        self.pool.pop().unwrap_or_else(|| {
            let mut symbols = vec![Symbol::EMPTY; MAX_CODES];
            for (code, symbol) in symbols.iter_mut().enumerate().take(256) {
                *symbol = Symbol::of(code as u8);
            }
            TableBuffers { symbols, s_index: [0; 256], s_index_by_first2: vec![0; FIRST2_LEN] }
        })
    }

    /// Returns the buffers of `table` to the pool after resetting the entries it set.
    fn release(&mut self, table: SymbolTableBuilder) {
        let mut buffers = table.buffers;
        for symbol in &buffers.symbols[256..256 + table.n_symbols] {
            if symbol.len() >= 2 {
                let val = (symbol.first() << 8) | symbol.bytes[1] as usize;
                buffers.s_index_by_first2[val] = 0;
                buffers.s_index_by_first2[val + 1] = 0;
            } else {
                buffers.s_index[symbol.first()] = 0;
            }
        }
        self.pool.push(buffers);
    }
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::new());
}

/// Storage of a [`SymbolTableBuilder`]. Codes are below 512, so the indexes are `u16`.
struct TableBuffers {
    /// Codes 0-255: the single bytes; 256+: the symbols of the table.
    symbols: Vec<Symbol>,
    /// Index single-byte symbol in symbols array by their only byte.
    s_index: [u16; 256],
    /// Index multi-byte symbols in symbols array by their first 2 bytes.
    s_index_by_first2: Vec<u16>,
}

struct SymbolTableBuilder {
    sample_size: usize,
    buffers: TableBuffers,
    s_index_by_length: Vec<usize>,
    s_index_by_length_reverse: Vec<usize>,
    n_symbols: usize,
}

impl SymbolTableBuilder {
    fn new(sample_size: usize, scratch: &mut Scratch) -> SymbolTableBuilder {
        SymbolTableBuilder {
            sample_size,
            buffers: scratch.take_buffers(),
            s_index_by_length: Vec::new(),
            s_index_by_length_reverse: Vec::new(),
            n_symbols: 0,
        }
    }

    fn build_symbol_table(data: &[u8], sample_size: usize, scratch: &mut Scratch) -> SymbolTableBuilder {
        let mut st = SymbolTableBuilder::new(sample_size, scratch);
        let mut best_table: Option<SymbolTableBuilder> = None;
        let mut best_weight = i64::MAX;
        let sampled = sample_size < data.len();
        for i in 1..=NUM_ITERS {
            scratch.count1.clear();
            let weight = st.compress_count(scratch, data, i < NUM_ITERS);
            let next = if i < NUM_ITERS { Some(st.make_table(scratch, false, false, sampled)) } else { None };
            // Once the next table is the same as this one, every later iteration would repeat this one exactly
            // (same counts, weight and next table), so the best table can't change any more.
            let converged = next.as_ref().is_some_and(|n| n.same_symbols(&st));
            if weight <= best_weight {
                best_weight = weight;
                let Scratch { count1, best_count1, .. } = scratch;
                best_count1.copy_from(count1);
                if let Some(old) = best_table.replace(st) {
                    scratch.release(old);
                }
            } else {
                scratch.release(st);
            }
            match next {
                Some(n) if converged => {
                    scratch.release(n);
                    break;
                }
                Some(n) => st = n,
                None => break,
            }
        }
        let best_table = best_table.expect("at least one iteration");
        // the last pass only uses the single symbol counts
        let result = best_table.make_table(scratch, true, true, sampled);
        scratch.release(best_table);
        result.sort_symbols_by_length()
    }

    /// Whether both tables have the same symbols (bytes and Java hash) in the same order.
    fn same_symbols(&self, other: &SymbolTableBuilder) -> bool {
        self.n_symbols == other.n_symbols
            && self.buffers.symbols[256..256 + self.n_symbols]
                .iter()
                .zip(&other.buffers.symbols[256..256 + other.n_symbols])
                .all(|(a, b)| a.same_key(b))
    }

    fn sort_symbols_by_length(mut self) -> SymbolTableBuilder {
        self.s_index_by_length = vec![0; self.n_symbols];
        self.s_index_by_length_reverse = vec![0; self.n_symbols];
        let mut idx = 0;
        // 2, 3, 4, 5, 6, 7, 8, 1
        for b in 2..=MAX_SYMBOL_LENGTH + 1 {
            let len = if b > MAX_SYMBOL_LENGTH { 1 } else { b };
            for i in 0..self.n_symbols {
                if self.buffers.symbols[256 + i].len() == len {
                    self.s_index_by_length[i] = idx;
                    self.s_index_by_length_reverse[idx] = i;
                    idx += 1;
                }
            }
        }
        self
    }

    fn encode(&self, text: &[u8]) -> SymbolTable {
        let mut encoded_symbols = Vec::new();
        let mut lengths = Vec::with_capacity(self.n_symbols);
        for i in 0..self.n_symbols {
            let symbol = &self.buffers.symbols[256 + self.s_index_by_length_reverse[i]];
            lengths.push(symbol.len() as u32);
            encoded_symbols.extend_from_slice(symbol.as_slice());
        }
        let mut encoded = Vec::with_capacity(text.len());
        let mut i = 0;
        while i < text.len() {
            let code = self.find_longest_symbol(text, i);
            if code < 256 {
                encoded.push(255);
                encoded.push(text[i]);
                i += 1;
            } else {
                let symbol = self.s_index_by_length[code - 256];
                encoded.push(symbol as u8);
                i += lengths[symbol] as usize;
            }
        }
        SymbolTable {
            symbols: encoded_symbols,
            symbol_lengths: lengths,
            compressed_data: encoded,
            decompressed_length: text.len(),
        }
    }

    fn add(&mut self, symbol: Symbol) {
        self.buffers.symbols[256 + self.n_symbols] = symbol;
        self.n_symbols += 1;
    }

    #[inline]
    fn find_longest_symbol(&self, text: &[u8], offset: usize) -> usize {
        let buffers = &self.buffers;
        // first look for a multi-byte symbol starting with the next 2 bytes
        if text.len() - offset >= 2 {
            let a = ((text[offset] as usize) << 8) | text[offset + 1] as usize;
            let start = buffers.s_index_by_first2[a] as usize;
            if start > 0 {
                let end = buffers.s_index_by_first2[a + 1] as usize;
                for code in start..end {
                    if buffers.symbols[code].matches(text, offset, 2) {
                        return code;
                    }
                }
            }
        }
        // if not found, then look for a single-byte symbol
        let letter = text[offset] as usize;
        let code = buffers.s_index[letter] as usize;
        if code >= 256 {
            return code;
        }
        // otherwise just return the "escape code" for this symbol since it's not in the table
        letter
    }

    fn ranges(&self, size: usize) -> impl Iterator<Item = (usize, usize)> {
        let (samples, offset, chunk_size) = if size < self.sample_size {
            (1, 0, size)
        } else {
            let chunk_size = 1000;
            let samples = self.sample_size / chunk_size;
            (samples, size / samples, chunk_size)
        };
        (0..samples).map(move |i| (i * offset, size.min(i * offset + chunk_size)))
    }

    fn compress_count(&self, scratch: &mut Scratch, text: &[u8], second_pass: bool) -> i64 {
        if text.is_empty() {
            return 0;
        }
        let Scratch { count1, count2, count2_touched, count2_rows, .. } = scratch;
        let symbols = &self.buffers.symbols;
        let mut weight: i64 = 0;
        for (range_start, end) in self.ranges(text.len()) {
            let mut start = range_start;
            let mut code1 = self.find_longest_symbol(text, start);
            let mut symbol = symbols[code1];
            let mut cur = start + symbol.len();
            start = cur;
            weight += if code1 < 256 { 2 } else { 1 };
            while cur < end {
                // count single symbol (i.e. an option is not extending it)
                count1.inc(code1);
                // as an alternative, consider just using the next byte..
                if symbol.len() > 1 {
                    // .. but do not count single byte symbols doubly
                    count1.inc(text[start] as usize);
                }
                // now match a new symbol
                start = cur;
                let code2 = self.find_longest_symbol(text, cur);
                let symbol2 = symbols[code2];
                cur += symbol2.len();
                weight += if code2 < 256 { 2 } else { 1 };
                if second_pass {
                    count2_rows[code1 >> 6] |= 1 << (code1 & 63);
                    // consider the symbol that is the concatenation of the two last symbols
                    let idx = (code1 << 9) | code2;
                    count2[idx] += 1;
                    count2_touched[idx >> 6] |= 1 << (idx & 63);
                    // as an alternative, consider just extending with the next byte..
                    if symbol2.len() > 1 {
                        // ..but do not count single byte extensions doubly
                        let idx = (code1 << 9) | text[start] as usize;
                        count2[idx] += 1;
                        count2_touched[idx >> 6] |= 1 << (idx & 63);
                    }
                }
                code1 = code2;
                symbol = symbols[code1];
            }
        }
        // account for the encoded symbol table size
        for i in 0..self.n_symbols {
            weight += symbols[256 + i].len() as i64 + 1;
        }
        weight
    }

    /// Builds the next table from the counts of the last `compress_count` (`count1` from `scratch.best_count1` if
    /// `best_counts`), then clears the pair counts.
    fn make_table(
        &self,
        scratch: &mut Scratch,
        last_pass: bool,
        best_counts: bool,
        sampled: bool,
    ) -> SymbolTableBuilder {
        let min_count = 5;
        let mut cands = std::mem::replace(&mut scratch.candidates, Candidates::new_empty());
        cands.clear();
        let add_or_inc = |cands: &mut Candidates, s: Symbol, count: i64, min: i64| {
            if count >= min {
                cands.merge(s, count * s.len() as i64);
            }
        };
        let symbols = &self.buffers.symbols;
        let max = 256 + self.n_symbols;
        {
            let count1 = if best_counts { &scratch.best_count1 } else { &scratch.count1 };
            let count2 = &scratch.count2;
            let touched = &scratch.count2_touched;
            let count2_rows = &scratch.count2_rows;
            // only non-zero counts matter, visited in code order
            count1.for_each(|pos1, cnt1| {
                if cnt1 <= 0 || pos1 >= max {
                    return;
                }
                let s1 = symbols[pos1];
                // heuristic: promoting single-byte symbols (*8) helps reduce exception rates and increases
                // [de]compression speed
                let multiplier: i64 = if s1.len() == 1 { 8 } else { 1 };
                add_or_inc(&mut cands, s1, multiplier * cnt1 as i64, if last_pass && !sampled { 1 } else { min_count });
                // don't need pair-wise counts for last pass to just encode the data
                if last_pass || s1.len() == MAX_SYMBOL_LENGTH {
                    return;
                }
                // pair counts that were never incremented are 0 (< min_count), so only visit touched ones, in order
                if count2_rows[pos1 >> 6] & (1 << (pos1 & 63)) == 0 {
                    return;
                }
                let row = pos1 << 9;
                for word_index in 0..max.div_ceil(64) {
                    let mut word = touched[(row >> 6) + word_index];
                    while word != 0 {
                        let pos2 = word_index * 64 + word.trailing_zeros() as usize;
                        word &= word - 1;
                        let cnt2 = count2[row | pos2];
                        if cnt2 < min_count as i32 {
                            continue;
                        }
                        add_or_inc(&mut cands, Symbol::concat(&s1, &symbols[pos2]), cnt2 as i64, min_count);
                    }
                }
            });
        }
        if !last_pass {
            let Scratch { count2, count2_touched, count2_rows, .. } = scratch;
            for (rows_index, rows) in count2_rows.iter_mut().enumerate() {
                let mut r = *rows;
                while r != 0 {
                    let row = rows_index * 64 + r.trailing_zeros() as usize;
                    r &= r - 1;
                    for word_index in row * (MAX_CODES / 64)..(row + 1) * (MAX_CODES / 64) {
                        let mut w = count2_touched[word_index];
                        while w != 0 {
                            count2[word_index * 64 + w.trailing_zeros() as usize] = 0;
                            w &= w - 1;
                        }
                        count2_touched[word_index] = 0;
                    }
                }
                *rows = 0;
            }
        }

        let mut order = std::mem::take(&mut scratch.order);
        cands.java_order(&mut order, &mut scratch.starts);

        let mut pq = JavaPriorityQueue { heap: std::mem::take(&mut scratch.queue.heap) };
        pq.heap.clear();
        for &idx in &order {
            pq.add((cands.entries[idx as usize].1, idx));
        }
        let mut st = SymbolTableBuilder::new(self.sample_size, scratch);
        while st.n_symbols < 255 {
            let Some((gain, idx)) = pq.poll() else { break };
            let symbol = cands.entries[idx as usize].0;
            if !last_pass || sampled {
                st.add(symbol);
            } else {
                // adding a symbol costs length + 1, so don't add if it costs more than it saves
                let costs = symbol.len() as i64 + 1;
                let saves = if symbol.len() == 1 { gain / 8 } else { gain };
                if saves > costs {
                    st.add(symbol);
                }
            }
        }
        scratch.order = order;
        scratch.queue.heap = pq.heap;
        scratch.candidates = cands;
        st.finish()
    }

    fn finish(mut self) -> SymbolTableBuilder {
        let n = self.n_symbols;
        let buffers = &mut self.buffers;
        // sorts prefix symbols after the longer symbols
        buffers.symbols[256..256 + n].sort_by(Symbol::compare);
        for i in (0..n).rev() {
            let symbol = buffers.symbols[256 + i];
            let letter = symbol.first();
            if symbol.len() >= 2 {
                // index multi-byte by their first 2 bytes
                let val = (letter << 8) | symbol.bytes[1] as usize;
                buffers.s_index_by_first2[val] = (256 + i) as u16;
                // there might be symbols with this prefix, so store end of the range by setting val+1
                if buffers.s_index_by_first2[val + 1] == 0 {
                    buffers.s_index_by_first2[val + 1] = (256 + i + 1) as u16;
                }
            } else {
                // index single-byte symbols by their only byte
                buffers.s_index[letter] = (256 + i) as u16;
            }
        }
        self
    }
}

/// `FsstEncoder.encode` (Java implementation): builds a symbol table with up to a 30kb sample and compresses `data`
/// with it.
pub fn encode(data: &[u8]) -> SymbolTable {
    SCRATCH.with(|scratch| {
        let scratch = &mut scratch.borrow_mut();
        let table = SymbolTableBuilder::build_symbol_table(data, DEFAULT_SAMPLE_SIZE, scratch);
        let result = table.encode(data);
        scratch.release(table);
        result
    })
}

/// `Fsst.decode`.
pub fn decode(symbols: &[u8], symbol_lengths: &[u32], compressed: &[u8], decompressed_length: usize) -> Vec<u8> {
    let mut offsets = Vec::with_capacity(symbol_lengths.len());
    let mut o = 0usize;
    for &l in symbol_lengths {
        offsets.push(o);
        o += l as usize;
    }
    let mut out = Vec::with_capacity(decompressed_length);
    let mut i = 0;
    while i < compressed.len() {
        let idx = compressed[i] as usize;
        if idx == 255 {
            i += 1;
            if let Some(&b) = compressed.get(i) {
                out.push(b);
            }
        } else if idx < symbol_lengths.len() {
            let start = offsets[idx];
            out.extend_from_slice(&symbols[start..start + symbol_lengths[idx] as usize]);
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(input: &[u8]) -> SymbolTable {
        let table = encode(input);
        let decoded = decode(&table.symbols, &table.symbol_lengths, &table.compressed_data, table.decompressed_length);
        assert_eq!(input, decoded.as_slice());
        table
    }

    #[test]
    fn empty_and_small() {
        let t = roundtrip(b"");
        assert!(t.compressed_data.is_empty());
        roundtrip(b"a");
        roundtrip(b"hello hello hello hello hello hello world");
    }

    #[test]
    fn compresses_repetitive_text() {
        let text: String = (0..2000).map(|i| format!("Street {} ", i % 37)).collect();
        let t = roundtrip(text.as_bytes());
        assert!(t.compressed_data.len() < text.len() / 2, "{} vs {}", t.compressed_data.len(), text.len());
        // sampled path (> 30kb)
        let big: String = (0..10_000).map(|i| format!("Avenue {} ", i % 101)).collect();
        roundtrip(big.as_bytes());
    }

    /// Deterministic pseudo-random strings with lots of shared substrings, like attribute values.
    fn sample_strings(seed: u64, count: usize, alphabet: &[&str]) -> Vec<u8> {
        let mut state = seed;
        let mut next = |bound: usize| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((state >> 33) as usize) % bound
        };
        let mut out = Vec::new();
        for _ in 0..count {
            for _ in 0..1 + next(4) {
                out.extend_from_slice(alphabet[next(alphabet.len())].as_bytes());
            }
            if next(3) == 0 {
                out.extend_from_slice(next(1000).to_string().as_bytes());
            }
        }
        out
    }

    #[test]
    fn same_tables_as_straight_port() {
        let words = [
            "Straße",
            "strasse",
            "Rue ",
            "de la ",
            "Via ",
            "Zürich",
            "Bahnhof",
            "weg",
            "gasse",
            "platz",
            " ",
            "Saint-",
            "x",
            "q",
            "Ελλάδα",
            "東京",
            "a",
            "b",
            "ab",
            "abc",
        ];
        let mut inputs: Vec<Vec<u8>> = vec![b"".to_vec(), b"a".to_vec(), b"aaaaaaaaaaaaaaaaaaaaaaaa".to_vec()];
        for seed in 0..60u64 {
            let count = [1, 3, 10, 40, 200, 1000, 5000][seed as usize % 7];
            let alphabet = &words[..(4 + seed as usize % (words.len() - 3)).min(words.len())];
            inputs.push(sample_strings(seed, count, alphabet));
        }
        // all 256 byte values, and long runs of distinct pairs (many candidates, large hash tables)
        inputs.push((0..=255u8).cycle().take(20_000).collect());
        inputs.push((0..40_000u32).flat_map(|i| [(i % 251) as u8, (i * 7 % 253) as u8]).collect());
        for input in &inputs {
            let expected = super::super::fsst_reference::encode(input);
            assert_eq!(expected, encode(input), "input of {} bytes", input.len());
            // the per-thread buffers are reused: encoding again gives the same result
            assert_eq!(expected, encode(input));
        }
    }

    #[test]
    fn utf8() {
        roundtrip("Zürich Genève Ελλάδα 東京 東京 東京 東京 東京 東京 東京".as_bytes());
    }
}
