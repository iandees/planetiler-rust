//! Port of the integer codec MLT uses for `PhysicalLevelTechnique.FAST_PFOR`: JavaFastPFOR 0.3.10
//! `new Composition(new FastPFOR(), new VariableByte())` (block size 256, page size 65536), with the output words
//! written big-endian like `EncodingUtils.encodeFastPfor128`.

const BLOCK_SIZE: usize = 256;
const PAGE_SIZE: usize = 65536;
const OVERHEAD_OF_EACH_EXCEPT: i32 = 8;

/// `BitPacking.fastpack`: packs the low `bit` bits of 32 values into `bit` words appended to `out`.
fn fastpack(input: &[u32], out: &mut Vec<u32>, bit: u32) {
    debug_assert!(input.len() >= 32);
    if bit == 0 {
        return;
    }
    if bit == 32 {
        out.extend_from_slice(&input[..32]);
        return;
    }
    let start = out.len();
    out.resize(start + bit as usize, 0);
    let words = &mut out[start..];
    let mask = (1u32 << bit) - 1;
    for (i, &v) in input[..32].iter().enumerate() {
        let v = v & mask;
        let bitpos = i as u32 * bit;
        let word = (bitpos / 32) as usize;
        let off = bitpos % 32;
        words[word] |= v << off;
        if off + bit > 32 {
            words[word + 1] |= v >> (32 - off);
        }
    }
}

/// `BitPacking.fastunpack`.
fn fastunpack(input: &[u32], out: &mut [u32], bit: u32) {
    if bit == 0 {
        out[..32].fill(0);
        return;
    }
    if bit == 32 {
        out[..32].copy_from_slice(&input[..32]);
        return;
    }
    let mask = (1u32 << bit) - 1;
    for (i, o) in out[..32].iter_mut().enumerate() {
        let bitpos = i as u32 * bit;
        let word = (bitpos / 32) as usize;
        let off = bitpos % 32;
        let mut v = input[word] >> off;
        if off + bit > 32 {
            v |= input[word + 1] << (32 - off);
        }
        *o = v & mask;
    }
}

#[inline]
fn bits(v: u32) -> usize {
    32 - v.leading_zeros() as usize
}

struct FastPfor {
    data_to_be_packed: Vec<Vec<u32>>,
    data_pointers: [usize; 33],
    /// Number of leading entries of each `data_to_be_packed` array written by the current [`FastPfor::compress`]
    /// call, zeroed again when it ends: the packing of a partial group of 32 exceptions reads entries past the
    /// written ones, which are zeros in Java (fresh arrays per call), so they must be zeros here too.
    dirty: [usize; 33],
    byte_container: Vec<u8>,
    freqs: [i32; 33],
}

impl FastPfor {
    fn new() -> FastPfor {
        // Java preallocates 32 zeroed arrays of 8192 ints per call; growing them on demand instead gives the same
        // output (the contents read by the bit packing are the same zeros / values from earlier pages) without
        // allocating 1MB per encoded stream.
        FastPfor {
            data_to_be_packed: vec![Vec::new(); 33],
            data_pointers: [0; 33],
            dirty: [0; 33],
            byte_container: Vec::new(),
            freqs: [0; 33],
        }
    }

    /// `getBestBFromData`: returns `(bestb, cexcept, maxb)`.
    #[inline]
    fn get_best_b(&mut self, block: &[u32; BLOCK_SIZE]) -> (usize, usize, usize) {
        best_b(&mut self.freqs, block)
    }

    fn encode_page(&mut self, input: &[u32], out: &mut Vec<u32>) {
        let headerpos = out.len();
        out.push(0);
        self.data_pointers = [0; 33];
        let mut byte_container = std::mem::take(&mut self.byte_container);
        byte_container.clear();
        for block in input.as_chunks::<BLOCK_SIZE>().0 {
            let (bestb, cexcept, maxb) = self.get_best_b(block);
            byte_container.push(bestb as u8);
            byte_container.push(cexcept as u8);
            if cexcept > 0 {
                byte_container.push(maxb as u8);
                let index = maxb - bestb;
                let packed = &mut self.data_to_be_packed[index];
                if self.data_pointers[index] + cexcept >= packed.len() {
                    let mut newsize = 2 * (self.data_pointers[index] + cexcept);
                    // make sure it is a multiple of 32
                    newsize = (newsize + 31) - (newsize + 31) % 32;
                    packed.resize(newsize, 0);
                }
                for (k, &v) in block.iter().enumerate() {
                    if (v >> bestb) != 0 {
                        // we have an exception
                        byte_container.push(k as u8);
                        packed[self.data_pointers[index]] = v >> bestb;
                        self.data_pointers[index] += 1;
                    }
                }
            }
            for k in (0..BLOCK_SIZE).step_by(32) {
                fastpack(&block[k..], out, bestb as u32);
            }
        }
        out[headerpos] = (out.len() - headerpos) as u32;
        for (dirty, &count) in self.dirty.iter_mut().zip(&self.data_pointers) {
            *dirty = (*dirty).max(count);
        }
        let bytesize = byte_container.len();
        while byte_container.len() & 3 != 0 {
            byte_container.push(0);
        }
        out.push(bytesize as u32);
        for chunk in byte_container.as_chunks::<4>().0 {
            out.push(u32::from_le_bytes(*chunk));
        }
        self.byte_container = byte_container;
        let mut bitmap = 0u32;
        for k in 2..=32 {
            if self.data_pointers[k] != 0 {
                bitmap |= 1 << (k - 1);
            }
        }
        out.push(bitmap);
        for k in 2..=32 {
            let count = self.data_pointers[k];
            if count != 0 {
                out.push(count as u32);
                let mut j = 0;
                while j < count {
                    fastpack(&self.data_to_be_packed[k][j..], out, k as u32);
                    j += 32;
                }
                let overflow = j - count;
                let new_len = out.len() - overflow * k / 32;
                out.truncate(new_len);
            }
        }
    }

    /// `FastPFOR.compress`: compresses the largest multiple of 256 values. Returns the number of values consumed.
    fn compress(&mut self, input: &[u32], out: &mut Vec<u32>) -> usize {
        let inlength = input.len() - input.len() % BLOCK_SIZE;
        if inlength == 0 {
            return 0;
        }
        out.push(inlength as u32);
        let mut pos = 0;
        while pos != inlength {
            let thissize = PAGE_SIZE.min(inlength - pos);
            self.encode_page(&input[pos..pos + thissize], out);
            pos += thissize;
        }
        for (packed, dirty) in self.data_to_be_packed.iter_mut().zip(&mut self.dirty) {
            packed[..*dirty].fill(0);
            *dirty = 0;
        }
        inlength
    }
}

/// `VariableByte.compress` into little-endian words.
fn variable_byte_compress(input: &[u32], out: &mut Vec<u32>) {
    if input.is_empty() {
        return;
    }
    let mut buf: Vec<u8> = Vec::with_capacity(input.len() * 2 + 4);
    for &val in input {
        if val < (1 << 7) {
            buf.push((val | (1 << 7)) as u8);
        } else if val < (1 << 14) {
            buf.push((val & 0x7f) as u8);
            buf.push(((val >> 7) as u8) | 0x80);
        } else if val < (1 << 21) {
            buf.push((val & 0x7f) as u8);
            buf.push(((val >> 7) & 0x7f) as u8);
            buf.push(((val >> 14) as u8) | 0x80);
        } else if val < (1 << 28) {
            buf.push((val & 0x7f) as u8);
            buf.push(((val >> 7) & 0x7f) as u8);
            buf.push(((val >> 14) & 0x7f) as u8);
            buf.push(((val >> 21) as u8) | 0x80);
        } else {
            buf.push((val & 0x7f) as u8);
            buf.push(((val >> 7) & 0x7f) as u8);
            buf.push(((val >> 14) & 0x7f) as u8);
            buf.push(((val >> 21) & 0x7f) as u8);
            buf.push(((val >> 28) as u8) | 0x80);
        }
    }
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
    for chunk in buf.as_chunks::<4>().0 {
        out.push(u32::from_le_bytes(*chunk));
    }
}

/// `EncodingUtils.encodeFastPfor128(values, zigZagEncode=false, deltaEncode=false)` on already-transformed
/// values: `Composition(FastPFOR, VariableByte)`, words written big-endian.
pub fn encode(values: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(values, &mut out);
    out
}

/// Per-thread encoder state and word buffer reused by [`encode_into`].
struct EncodeScratch {
    codec: FastPfor,
    words: Vec<u32>,
}

thread_local! {
    static ENCODE_SCRATCH: std::cell::RefCell<EncodeScratch> =
        std::cell::RefCell::new(EncodeScratch { codec: FastPfor::new(), words: Vec::new() });
}

/// [`encode`], appending to `out`.
pub fn encode_into(values: &[u32], out: &mut Vec<u8>) {
    if values.is_empty() {
        return;
    }
    if values.len() < BLOCK_SIZE {
        // no FastPFOR block: a 0 header word, then the variable bytes
        out.reserve(4 + 4 * variable_byte_words(values));
        out.extend_from_slice(&[0; 4]);
        variable_byte_compress_be(values, out);
        return;
    }
    ENCODE_SCRATCH.with(|scratch| {
        let scratch = &mut *scratch.borrow_mut();
        let words = &mut scratch.words;
        words.clear();
        let consumed = scratch.codec.compress(values, words);
        variable_byte_compress(&values[consumed..], words);
        out.reserve(words.len() * 4);
        for &w in words.iter() {
            out.extend_from_slice(&w.to_be_bytes());
        }
    });
}

/// Number of words [`variable_byte_compress`] writes for `values`.
#[inline]
fn variable_byte_words(values: &[u32]) -> usize {
    values.iter().map(|&v| super::encoding::varint_size(v)).sum::<usize>().div_ceil(4)
}

/// [`variable_byte_compress`] written straight to big-endian bytes (each little-endian word of 4 variable bytes
/// becomes those 4 bytes reversed).
fn variable_byte_compress_be(values: &[u32], out: &mut Vec<u8>) {
    let mut word = [0u8; 4];
    let mut n = 0;
    let mut push = |b: u8, out: &mut Vec<u8>| {
        word[n] = b;
        n += 1;
        if n == 4 {
            out.extend_from_slice(&[word[3], word[2], word[1], word[0]]);
            n = 0;
        }
    };
    for &val in values {
        let mut v = val;
        while v >= 1 << 7 {
            push((v & 0x7f) as u8, out);
            v >>= 7;
        }
        push((v as u8) | 0x80, out);
    }
    if n > 0 {
        word[n..].fill(0);
        out.extend_from_slice(&[word[3], word[2], word[1], word[0]]);
    }
}

/// `encode(values).len()`, computed without packing anything.
pub fn encoded_size(values: &[u32]) -> usize {
    if values.is_empty() {
        return 0;
    }
    let inlength = values.len() - values.len() % BLOCK_SIZE;
    let tail = variable_byte_words(&values[inlength..]);
    if inlength == 0 {
        return 4 * (1 + tail);
    }
    let mut words = 1;
    let mut freqs = [0i32; 33];
    for page in values[..inlength].chunks(PAGE_SIZE) {
        let mut data_pointers = [0usize; 33];
        let mut bytesize = 0;
        // header word, then the packed blocks
        words += 1;
        for block in page.as_chunks::<BLOCK_SIZE>().0 {
            let (bestb, cexcept, maxb) = best_b(&mut freqs, block);
            bytesize += 2;
            if cexcept > 0 {
                bytesize += 1 + cexcept;
                data_pointers[maxb - bestb] += cexcept;
            }
            words += bestb * (BLOCK_SIZE / 32);
        }
        // byte size, the bytes, the bitmap
        words += 1 + bytesize.div_ceil(4) + 1;
        for (k, &count) in data_pointers.iter().enumerate().skip(2) {
            if count != 0 {
                let j = count.div_ceil(32) * 32;
                words += 1 + j / 32 * k - (j - count) * k / 32;
            }
        }
    }
    4 * (words + tail)
}

/// `getBestBFromData` with caller-provided frequency counts.
#[inline]
fn best_b(freqs: &mut [i32; 33], block: &[u32; BLOCK_SIZE]) -> (usize, usize, usize) {
    // four interleaved histograms so consecutive values with the same bit width don't serialize on one counter
    let mut partial = [[0i32; 33]; 4];
    for chunk in block.as_chunks::<4>().0 {
        for (histogram, &v) in partial.iter_mut().zip(chunk) {
            histogram[bits(v)] += 1;
        }
    }
    for (b, freq) in freqs.iter_mut().enumerate() {
        *freq = partial[0][b] + partial[1][b] + partial[2][b] + partial[3][b];
    }
    let mut bestb = 32usize;
    while freqs[bestb] == 0 {
        bestb -= 1;
    }
    let maxb = bestb;
    let mut bestcost = bestb as i32 * BLOCK_SIZE as i32;
    let mut cexcept = 0i32;
    let mut best_cexcept = 0i32;
    let mut b = bestb as i32 - 1;
    while b >= 0 {
        cexcept += freqs[(b + 1) as usize];
        if cexcept == BLOCK_SIZE as i32 {
            break;
        }
        // the extra 8 is the cost of storing maxbits
        let mut thiscost = cexcept * OVERHEAD_OF_EACH_EXCEPT + cexcept * (maxb as i32 - b) + b * BLOCK_SIZE as i32 + 8;
        if maxb as i32 - b == 1 {
            thiscost -= cexcept;
        }
        if thiscost < bestcost {
            bestcost = thiscost;
            bestb = b as usize;
            best_cexcept = cexcept;
        }
        b -= 1;
    }
    (bestb, best_cexcept as usize, maxb)
}

/// Decodes `num_values` values from FastPFOR-encoded `data` (inverse of [`encode`]).
pub fn decode(data: &[u8], num_values: usize) -> anyhow::Result<Vec<u32>> {
    if !data.len().is_multiple_of(4) {
        anyhow::bail!("FastPFOR data is not a multiple of 4 bytes: {}", data.len());
    }
    let words: Vec<u32> = data.as_chunks::<4>().0.iter().map(|c| u32::from_be_bytes(*c)).collect();
    let mut out: Vec<u32> = Vec::with_capacity(num_values + BLOCK_SIZE);
    if words.is_empty() {
        return Ok(out);
    }
    let get = |i: usize| -> anyhow::Result<u32> {
        words.get(i).copied().ok_or_else(|| anyhow::anyhow!("truncated FastPFOR data"))
    };
    // FastPFOR part
    let mut pos = 0usize;
    let outlength = get(pos)? as usize;
    pos += 1;
    if outlength > 0 {
        let mut data_to_be_packed: Vec<Vec<u32>> = vec![Vec::new(); 33];
        let mut decoded = 0usize;
        while decoded < outlength {
            let thissize = PAGE_SIZE.min(outlength - decoded);
            let initpos = pos;
            let wheremeta = get(pos)? as usize;
            pos += 1;
            let mut inexcept = initpos + wheremeta;
            let bytesize = get(inexcept)? as usize;
            inexcept += 1;
            let mut byte_container = Vec::with_capacity(bytesize + 4);
            for i in 0..bytesize.div_ceil(4) {
                byte_container.extend_from_slice(&get(inexcept + i)?.to_le_bytes());
            }
            inexcept += bytesize.div_ceil(4);
            let bitmap = get(inexcept)?;
            inexcept += 1;
            for (k, packed) in data_to_be_packed.iter_mut().enumerate().skip(2) {
                if bitmap & (1 << (k - 1)) != 0 {
                    let size = get(inexcept)? as usize;
                    inexcept += 1;
                    let roundedup = (size + 31) - (size + 31) % 32;
                    packed.clear();
                    packed.resize(roundedup, 0);
                    let mut j = 0;
                    while j < size {
                        let mut chunk = [0u32; 32];
                        for (w, c) in chunk.iter_mut().take(k).enumerate() {
                            *c = words.get(inexcept + w).copied().unwrap_or(0);
                        }
                        fastunpack(&chunk, &mut packed[j..j + 32], k as u32);
                        inexcept += k;
                        j += 32;
                    }
                    let overflow = j - size;
                    inexcept -= overflow * k / 32;
                }
            }
            let mut data_pointers = [0usize; 33];
            let mut bc = 0usize;
            let byte = |bc: &mut usize| -> anyhow::Result<u8> {
                let b = *byte_container.get(*bc).ok_or_else(|| anyhow::anyhow!("truncated FastPFOR metadata"))?;
                *bc += 1;
                Ok(b)
            };
            for _ in 0..thissize / BLOCK_SIZE {
                let b = byte(&mut bc)? as u32;
                let cexcept = byte(&mut bc)? as usize;
                let base = out.len();
                out.resize(base + BLOCK_SIZE, 0);
                for k in (0..BLOCK_SIZE).step_by(32) {
                    let mut chunk = [0u32; 32];
                    for (w, c) in chunk.iter_mut().take(b as usize).enumerate() {
                        *c = get(pos + w)?;
                    }
                    fastunpack(&chunk, &mut out[base + k..base + k + 32], b);
                    pos += b as usize;
                }
                if cexcept > 0 {
                    let maxbits = byte(&mut bc)? as usize;
                    let index = maxbits - b as usize;
                    for _ in 0..cexcept {
                        let p = byte(&mut bc)? as usize;
                        if index == 1 {
                            out[base + p] |= 1 << b;
                        } else {
                            let v = data_to_be_packed[index][data_pointers[index]];
                            data_pointers[index] += 1;
                            out[base + p] |= v << b;
                        }
                    }
                }
            }
            decoded += thissize;
            pos = inexcept;
        }
    }
    // VariableByte part
    let mut bytes = Vec::with_capacity((words.len() - pos) * 4);
    for w in &words[pos..] {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    let mut v: u32 = 0;
    let mut shift = 0;
    for b in bytes {
        if out.len() >= num_values {
            break;
        }
        v |= ((b & 0x7f) as u32) << shift;
        if b & 0x80 != 0 {
            out.push(v);
            v = 0;
            shift = 0;
        } else {
            shift += 7;
        }
    }
    out.truncate(num_values);
    if out.len() != num_values {
        anyhow::bail!("FastPFOR decoded {} values, expected {num_values}", out.len());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(values: &[u32]) {
        let encoded = encode(values);
        assert_eq!(values, decode(&encoded, values.len()).unwrap().as_slice());
    }

    /// The straight port: a new codec per call.
    fn encode_fresh(values: &[u32]) -> Vec<u8> {
        let mut words: Vec<u32> = Vec::new();
        if !values.is_empty() {
            let consumed = FastPfor::new().compress(values, &mut words);
            if words.is_empty() {
                words.push(0);
            }
            variable_byte_compress(&values[consumed..], &mut words);
        }
        words.iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    #[test]
    fn encoded_size_matches_encode() {
        let mut state = 7u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 32) as u32
        };
        for len in
            [0usize, 1, 2, 3, 4, 5, 100, 255, 256, 257, 511, 512, 1000, 4096, 65535, 65536, 65537, 70_000, 140_000]
        {
            for shape in 0..6 {
                let values: Vec<u32> = (0..len)
                    .map(|i| match shape {
                        0 => 0,
                        1 => next() % 17,
                        2 => next() >> (next() % 32),
                        3 => {
                            if next() % 50 == 0 {
                                next()
                            } else {
                                next() % 8
                            }
                        }
                        4 => i as u32,
                        _ => {
                            if next() % 7 == 0 {
                                1 << (next() % 32)
                            } else {
                                next() % 300
                            }
                        }
                    })
                    .collect();
                let encoded = encode(&values);
                // the per-thread codec is reused across calls: same output as a fresh one (like Java's)
                assert_eq!(encode_fresh(&values), encoded, "len {len} shape {shape}");
                assert_eq!(encoded.len(), encoded_size(&values), "len {len} shape {shape}");
                let mut appended = vec![9u8];
                encode_into(&values, &mut appended);
                assert_eq!(&appended[1..], encoded.as_slice());
                assert_eq!(values, decode(&encoded, values.len()).unwrap());
            }
        }
    }

    #[test]
    fn small() {
        assert!(encode(&[]).is_empty());
        // fewer than 256 values: a 0 header word then variable bytes
        assert_eq!(vec![0, 0, 0, 0, 0, 0, 0x82, 0x81], encode(&[1, 2]));
        roundtrip(&[1, 2, 3]);
        roundtrip(&[u32::MAX, 0, 1 << 28, 1 << 21, 1 << 14, 1 << 7]);
    }

    #[test]
    fn blocks_with_exceptions() {
        let mut values: Vec<u32> = (0..1000u32).map(|i| i % 17).collect();
        values[5] = 100_000;
        values[300] = 1 << 31;
        values[301] = 3 << 20;
        values[700] = 64;
        roundtrip(&values);
        let big: Vec<u32> = (0..70_000u32).map(|i| i.wrapping_mul(2_654_435_761) >> (i % 32)).collect();
        roundtrip(&big);
    }
}
