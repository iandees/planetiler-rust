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
    freqs: [i32; 33],
}

impl FastPfor {
    fn new() -> FastPfor {
        // Java preallocates 32 zeroed arrays of 8192 ints per call; growing them on demand instead gives the same
        // output (the contents read by the bit packing are the same zeros / values from earlier pages) without
        // allocating 1MB per encoded stream.
        FastPfor { data_to_be_packed: vec![Vec::new(); 33], data_pointers: [0; 33], freqs: [0; 33] }
    }

    /// `getBestBFromData`: returns `(bestb, cexcept, maxb)`.
    fn get_best_b(&mut self, block: &[u32]) -> (usize, usize, usize) {
        self.freqs = [0; 33];
        for &v in &block[..BLOCK_SIZE] {
            self.freqs[bits(v)] += 1;
        }
        let mut bestb = 32usize;
        while self.freqs[bestb] == 0 {
            bestb -= 1;
        }
        let maxb = bestb;
        let mut bestcost = bestb as i32 * BLOCK_SIZE as i32;
        let mut cexcept = 0i32;
        let mut best_cexcept = 0i32;
        let mut b = bestb as i32 - 1;
        while b >= 0 {
            cexcept += self.freqs[(b + 1) as usize];
            if cexcept == BLOCK_SIZE as i32 {
                break;
            }
            // the extra 8 is the cost of storing maxbits
            let mut thiscost =
                cexcept * OVERHEAD_OF_EACH_EXCEPT + cexcept * (maxb as i32 - b) + b * BLOCK_SIZE as i32 + 8;
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

    fn encode_page(&mut self, input: &[u32], out: &mut Vec<u32>) {
        let headerpos = out.len();
        out.push(0);
        self.data_pointers = [0; 33];
        let mut byte_container: Vec<u8> = Vec::with_capacity(3 * input.len() / BLOCK_SIZE + 8);
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
        let bytesize = byte_container.len();
        while byte_container.len() & 3 != 0 {
            byte_container.push(0);
        }
        out.push(bytesize as u32);
        for chunk in byte_container.as_chunks::<4>().0 {
            out.push(u32::from_le_bytes(*chunk));
        }
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
    let mut words: Vec<u32> = Vec::with_capacity(values.len() / 2 + 8);
    if !values.is_empty() {
        let consumed = FastPfor::new().compress(values, &mut words);
        if words.is_empty() {
            words.push(0);
        }
        variable_byte_compress(&values[consumed..], &mut words);
    }
    let mut out = Vec::with_capacity(words.len() * 4);
    for w in words {
        out.extend_from_slice(&w.to_be_bytes());
    }
    out
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
