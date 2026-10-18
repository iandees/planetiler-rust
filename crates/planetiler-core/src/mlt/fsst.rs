//! Port of the pure-Java FSST encoder used by MLT 0.0.10 (`FsstJava` -> `SymbolTableBuilder`, itself a port of
//! [cwida/fsst](https://github.com/cwida/fsst), MIT license).
//!
//! The symbol table that Java builds depends on the iteration order of a `java.util.HashMap<Symbol, Long>` (which
//! feeds a `java.util.PriorityQueue` that orders only by gain, so ties are broken by heap position). To produce the
//! same symbol tables as Java this module emulates both: [`JavaHashMap`] reproduces `HashMap.merge` (head insertion,
//! resize-before-insert, power-of-two tables split in order) and [`JavaPriorityQueue`] reproduces the binary heap's
//! `siftUp` / `siftDown`. Java also hashes symbols with a quirk (the hash of a truncated concatenation includes the
//! truncated bytes), which [`Symbol`] keeps since it decides which candidates are merged.

const MAX_SYMBOL_LENGTH: usize = 8;
const NUM_ITERS: usize = 6;
const DEFAULT_SAMPLE_SIZE: usize = 30_000;

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
    fn of(b: u8) -> Symbol {
        let mut bytes = [0; MAX_SYMBOL_LENGTH];
        bytes[0] = b;
        Symbol { bytes, len: 1, hash: 31i32.wrapping_add(b as i8 as i32) }
    }

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

    /// Java `Symbol.equals` + `hashCode` agreement as used by `HashMap`: same hash and same bytes.
    #[inline]
    fn same_key(&self, other: &Symbol) -> bool {
        self.hash == other.hash && self.as_slice() == other.as_slice()
    }

    /// `Symbol.compareTo`: lexicographic (unsigned) except longer symbols sort before their prefixes.
    fn compare(&self, other: &Symbol) -> std::cmp::Ordering {
        let a = self.as_slice();
        let b = other.as_slice();
        for (x, y) in a.iter().zip(b) {
            if x != y {
                return x.cmp(y);
            }
        }
        b.len().cmp(&a.len())
    }

    /// `Symbol.match`: true if this symbol appears at `offset` in `text`, skipping the first `ignore` bytes.
    #[inline]
    fn matches(&self, text: &[u8], offset: usize, ignore: usize) -> bool {
        let len = self.len();
        if text.len() < offset + len {
            return false;
        }
        text[offset + ignore..offset + len] == self.bytes[ignore..len]
    }
}

/// Emulates the iteration order of a `java.util.HashMap<Symbol, Long>` populated only with `merge(k, v, Long::sum)`.
struct JavaHashMap {
    table: Vec<Vec<u32>>,
    entries: Vec<(Symbol, i64)>,
    threshold: usize,
}

impl JavaHashMap {
    fn new() -> JavaHashMap {
        JavaHashMap { table: Vec::new(), entries: Vec::new(), threshold: 0 }
    }

    #[inline]
    fn spread(h: i32) -> u32 {
        let h = h as u32;
        h ^ (h >> 16)
    }

    fn resize(&mut self) {
        let old_cap = self.table.len();
        let (new_cap, new_thr) = if old_cap == 0 { (16, 12) } else { (old_cap * 2, self.threshold * 2) };
        self.threshold = new_thr;
        let mut new_table: Vec<Vec<u32>> = vec![Vec::new(); new_cap];
        for bucket in std::mem::take(&mut self.table) {
            // preserve order when splitting into the lo / hi buckets
            for idx in bucket {
                let h = Self::spread(self.entries[idx as usize].0.hash) as usize;
                new_table[h & (new_cap - 1)].push(idx);
            }
        }
        self.table = new_table;
    }

    fn merge(&mut self, key: Symbol, value: i64) {
        if self.entries.len() > self.threshold || self.table.is_empty() {
            self.resize();
        }
        let n = self.table.len();
        let i = Self::spread(key.hash) as usize & (n - 1);
        for &idx in &self.table[i] {
            let entry = &mut self.entries[idx as usize];
            if entry.0.same_key(&key) {
                entry.1 += value;
                return;
            }
        }
        let bin_count = self.table[i].len();
        let idx = self.entries.len() as u32;
        self.entries.push((key, value));
        // new nodes go at the head of the bin
        self.table[i].insert(0, idx);
        if bin_count >= 7 {
            // TREEIFY_THRESHOLD - 1: Java resizes small tables instead of converting the bin to a tree.
            // Treeified bins (tables of 64+ entries with 8+ colliding keys) iterate in a different order that is not
            // emulated; the symbol table is still valid, it may just differ from Java's in rare cases.
            if n < 64 {
                self.resize();
            } else {
                TREEIFIED_BINS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// Entries in Java iteration order.
    fn into_iter_java_order(self) -> impl Iterator<Item = (Symbol, i64)> {
        let entries = self.entries;
        self.table.into_iter().flatten().map(move |idx| entries[idx as usize]).collect::<Vec<_>>().into_iter()
    }
}

/// `QSymbol` in a `java.util.PriorityQueue` ordered by gain descending.
struct JavaPriorityQueue {
    heap: Vec<(i64, Symbol)>,
}

impl JavaPriorityQueue {
    /// `QSymbol.compareTo`: `Long.compare(o.gain, gain)`.
    #[inline]
    fn cmp(a: &(i64, Symbol), b: &(i64, Symbol)) -> std::cmp::Ordering {
        b.0.cmp(&a.0)
    }

    fn add(&mut self, x: (i64, Symbol)) {
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

    fn poll(&mut self) -> Option<(i64, Symbol)> {
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

struct Counters {
    count1: Vec<i32>,
    count2: Vec<i32>,
}

impl Counters {
    fn new() -> Counters {
        Counters { count1: vec![0; 512], count2: vec![0; 512 * 512] }
    }
}

struct SymbolTableBuilder {
    sample_size: usize,
    symbols: Vec<Symbol>,
    /// Index single-byte symbol in symbols array by their only byte.
    s_index: Vec<usize>,
    /// Index multi-byte symbols in symbols array by their first 2 bytes.
    s_index_by_first2: Vec<usize>,
    s_index_by_length: Vec<usize>,
    s_index_by_length_reverse: Vec<usize>,
    n_symbols: usize,
}

impl SymbolTableBuilder {
    fn new(sample_size: usize) -> SymbolTableBuilder {
        let mut symbols = Vec::with_capacity(512);
        for code in 0..256 {
            symbols.push(Symbol::of(code as u8));
        }
        symbols.resize(512, Symbol::of(0));
        SymbolTableBuilder {
            sample_size,
            symbols,
            s_index: vec![0; 256],
            s_index_by_first2: vec![0; (1 << 16) + 1],
            s_index_by_length: Vec::new(),
            s_index_by_length_reverse: Vec::new(),
            n_symbols: 0,
        }
    }

    fn build_symbol_table(data: &[u8], sample_size: usize) -> SymbolTableBuilder {
        let mut st = SymbolTableBuilder::new(sample_size);
        let mut best_table: Option<SymbolTableBuilder> = None;
        let mut best_weight = i64::MAX;
        let mut best_counters: Option<Counters> = None;
        let sampled = sample_size < data.len();
        for i in 1..=NUM_ITERS {
            let mut counters = Counters::new();
            let weight = st.compress_count(&mut counters, data, i < NUM_ITERS);
            let next = if i < NUM_ITERS { Some(st.make_table(&counters, false, sampled)) } else { None };
            if weight <= best_weight {
                best_weight = weight;
                best_counters = Some(counters);
                best_table = Some(st);
            }
            match next {
                Some(n) => st = n,
                None => break,
            }
        }
        let best_table = best_table.expect("at least one iteration");
        let result = best_table.make_table(&best_counters.expect("counters"), true, sampled);
        result.sort_symbols_by_length()
    }

    fn sort_symbols_by_length(mut self) -> SymbolTableBuilder {
        self.s_index_by_length = vec![0; self.n_symbols];
        self.s_index_by_length_reverse = vec![0; self.n_symbols];
        let mut idx = 0;
        // 2, 3, 4, 5, 6, 7, 8, 1
        for b in 2..=MAX_SYMBOL_LENGTH + 1 {
            let len = if b > MAX_SYMBOL_LENGTH { 1 } else { b };
            for i in 0..self.n_symbols {
                if self.symbols[256 + i].len() == len {
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
            let symbol = &self.symbols[256 + self.s_index_by_length_reverse[i]];
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
        self.symbols[256 + self.n_symbols] = symbol;
        self.n_symbols += 1;
    }

    #[inline]
    fn find_longest_symbol(&self, text: &[u8], offset: usize) -> usize {
        // first look for a multi-byte symbol starting with the next 2 bytes
        if text.len() - offset >= 2 {
            let a = ((text[offset] as usize) << 8) | text[offset + 1] as usize;
            let start = self.s_index_by_first2[a];
            if start > 0 {
                let end = self.s_index_by_first2[a + 1];
                for code in start..end {
                    if self.symbols[code].matches(text, offset, 2) {
                        return code;
                    }
                }
            }
        }
        // if not found, then look for a single-byte symbol
        let letter = text[offset] as usize;
        let code = self.s_index[letter];
        if code >= 256 {
            return code;
        }
        // otherwise just return the "escape code" for this symbol since it's not in the table
        letter
    }

    fn ranges(&self, size: usize) -> Vec<(usize, usize)> {
        if size < self.sample_size {
            vec![(0, size)]
        } else {
            let chunk_size = 1000;
            let samples = self.sample_size / chunk_size;
            let offset = size / samples;
            (0..samples).map(|i| (i * offset, size.min(i * offset + chunk_size))).collect()
        }
    }

    fn compress_count(&self, counters: &mut Counters, text: &[u8], second_pass: bool) -> i64 {
        if text.is_empty() {
            return 0;
        }
        let mut weight: i64 = 0;
        for (range_start, end) in self.ranges(text.len()) {
            let mut start = range_start;
            let mut code1 = self.find_longest_symbol(text, start);
            let mut symbol = self.symbols[code1];
            let mut cur = start + symbol.len();
            start = cur;
            weight += if code1 < 256 { 2 } else { 1 };
            while cur < end {
                // count single symbol (i.e. an option is not extending it)
                counters.count1[code1] += 1;
                // as an alternative, consider just using the next byte..
                if symbol.len() > 1 {
                    // .. but do not count single byte symbols doubly
                    counters.count1[text[start] as usize] += 1;
                }
                // now match a new symbol
                start = cur;
                let code2 = self.find_longest_symbol(text, cur);
                let symbol2 = self.symbols[code2];
                cur += symbol2.len();
                weight += if code2 < 256 { 2 } else { 1 };
                if second_pass {
                    // consider the symbol that is the concatenation of the two last symbols
                    counters.count2[(code1 << 9) | code2] += 1;
                    // as an alternative, consider just extending with the next byte..
                    if symbol2.len() > 1 {
                        // ..but do not count single byte extensions doubly
                        counters.count2[(code1 << 9) | text[start] as usize] += 1;
                    }
                }
                code1 = code2;
                symbol = self.symbols[code1];
            }
        }
        // account for the encoded symbol table size
        for i in 0..self.n_symbols {
            weight += self.symbols[256 + i].len() as i64 + 1;
        }
        weight
    }

    fn make_table(&self, counters: &Counters, last_pass: bool, sampled: bool) -> SymbolTableBuilder {
        let min_count = 5;
        let mut cands = JavaHashMap::new();
        let add_or_inc = |cands: &mut JavaHashMap, s: Symbol, count: i64, min: i64| {
            if count >= min {
                cands.merge(s, count * s.len() as i64);
            }
        };
        let max = 256 + self.n_symbols;
        for pos1 in 0..max {
            let cnt1 = counters.count1[pos1];
            if cnt1 <= 0 {
                continue;
            }
            let s1 = self.symbols[pos1];
            // heuristic: promoting single-byte symbols (*8) helps reduce exception rates and increases
            // [de]compression speed
            let multiplier: i64 = if s1.len() == 1 { 8 } else { 1 };
            add_or_inc(&mut cands, s1, multiplier * cnt1 as i64, if last_pass && !sampled { 1 } else { min_count });
            // don't need pair-wise counts for last pass to just encode the data
            if last_pass || s1.len() == MAX_SYMBOL_LENGTH {
                continue;
            }
            let row = &counters.count2[pos1 << 9..(pos1 << 9) + max];
            for (pos2, &cnt2) in row.iter().enumerate() {
                if cnt2 < min_count as i32 {
                    continue;
                }
                add_or_inc(&mut cands, Symbol::concat(&s1, &self.symbols[pos2]), cnt2 as i64, min_count);
            }
        }

        let mut pq = JavaPriorityQueue { heap: Vec::with_capacity(cands.entries.len()) };
        for (symbol, gain) in cands.into_iter_java_order() {
            pq.add((gain, symbol));
        }
        let mut st = SymbolTableBuilder::new(self.sample_size);
        while st.n_symbols < 255 {
            let Some((gain, symbol)) = pq.poll() else { break };
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
        st.finish()
    }

    fn finish(mut self) -> SymbolTableBuilder {
        let mut tmp: Vec<Symbol> = self.symbols[256..256 + self.n_symbols].to_vec();
        // sorts prefix symbols after the longer symbols
        tmp.sort_by(Symbol::compare);
        for i in (0..self.n_symbols).rev() {
            let letter = tmp[i].first();
            if tmp[i].len() >= 2 {
                // index multi-byte by their first 2 bytes
                let bytes = tmp[i].as_slice();
                let val = ((bytes[0] as usize) << 8) | bytes[1] as usize;
                self.s_index_by_first2[val] = 256 + i;
                // there might be symbols with this prefix, so store end of the range by setting val+1
                if self.s_index_by_first2[val + 1] == 0 {
                    self.s_index_by_first2[val + 1] = 256 + i + 1;
                }
            } else {
                // index single-byte symbols by their only byte
                self.s_index[letter] = 256 + i;
            }
            self.symbols[256 + i] = tmp[i];
        }
        self
    }
}

/// `FsstEncoder.encode` (Java implementation): builds a symbol table with up to a 30kb sample and compresses `data`
/// with it.
pub fn encode(data: &[u8]) -> SymbolTable {
    SymbolTableBuilder::build_symbol_table(data, DEFAULT_SAMPLE_SIZE).encode(data)
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

    #[test]
    fn utf8() {
        roundtrip("Zürich Genève Ελλάδα 東京 東京 東京 東京 東京 東京 東京".as_bytes());
    }
}
