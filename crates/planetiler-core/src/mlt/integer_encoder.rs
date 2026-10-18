//! Port of `org.maplibre.mlt.converter.encodings.IntegerEncoder` (only the `IntegerEncodingOption.AUTO` mode, which
//! is the only one Planetiler uses).
//!
//! Java materializes all four logical encodings of a stream (plain, delta, RLE, delta-RLE), physically encodes each
//! and keeps the smallest. Here one pass over the values computes the varint size of every candidate (the RLE ones
//! from the run boundaries), which is also the FastPFOR size of candidates shorter than one 256-value block (those
//! are only variable bytes after a header word); longer FastPFOR candidates are materialized into a reused buffer
//! and sized by [`fastpfor::encoded_size`] without packing. Only the chosen candidate is encoded, straight into the
//! output. The bytes are the same (`integer_encoder_reference` keeps the straight port for differential tests).

use std::cell::RefCell;

use super::encoding::{
    LogicalLevelTechnique, LogicalStreamType, PhysicalLevelTechnique, PhysicalStreamType, StreamExtra, StreamMetadata,
    put_varint, put_varlong, varint_size, varlong_size, zigzag, zigzag_long,
};
use super::fastpfor;

/// FastPFOR packs blocks of this many values; shorter inputs are only variable bytes.
const FAST_PFOR_BLOCK: usize = 256;

/// `IntegerEncoder.IntegerEncodingResult`.
#[derive(Debug, Clone)]
pub struct IntegerEncodingResult {
    pub logical_level_technique1: LogicalLevelTechnique,
    pub logical_level_technique2: LogicalLevelTechnique,
    pub encoded_values: Vec<u8>,
    /// If rle or delta-rle encoding is used, otherwise can be ignored
    pub num_runs: u32,
    pub physical_level_encoded_values_length: u32,
}

thread_local! {
    /// Buffer for materialized FastPFOR candidates.
    static VALUES_SCRATCH: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

#[inline]
fn transform(v: i32, signed: bool) -> u32 {
    if signed { zigzag(v) } else { v as u32 }
}

/// FastPFOR size of a candidate of `len` values whose varint sizes sum to `varint_bytes`, if it is shorter than a
/// block (a header word and the variable bytes padded to words).
#[inline]
fn short_fast_pfor_size(len: usize, varint_bytes: usize) -> usize {
    if len == 0 { 0 } else { 4 + 4 * varint_bytes.div_ceil(4) }
}

/// Physical encoding of `values` (Java `encodeFastPfor` / `EncodingUtils.encodeVarints`), zigzag-encoding them first
/// if `signed`.
pub fn encode_physical(values: &[i32], fast_pfor: bool, signed: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() + 4);
    encode_physical_into(&mut out, values, fast_pfor, signed);
    out
}

fn encode_physical_into(out: &mut Vec<u8>, values: &[i32], fast_pfor: bool, signed: bool) {
    if fast_pfor {
        with_scratch(|buf| {
            buf.extend(values.iter().map(|&v| transform(v, signed)));
            fastpfor::encode_into(buf, out);
        });
    } else {
        for &v in values {
            put_varint(out, transform(v, signed));
        }
    }
}

/// Runs `f` with the (cleared) per-thread candidate buffer.
fn with_scratch<R>(f: impl FnOnce(&mut Vec<u32>) -> R) -> R {
    VALUES_SCRATCH.with(|buf| match buf.try_borrow_mut() {
        Ok(mut buf) => {
            buf.clear();
            f(&mut buf)
        }
        // not re-entered in practice, but stay correct if it ever is
        Err(_) => f(&mut Vec::new()),
    })
}

/// The logical encodings [`encode_int`] chooses between (the index Java uses).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Plain = 0,
    Delta = 1,
    Rle = 2,
    DeltaRle = 3,
}

/// The chosen encoding of an int stream.
#[derive(Clone, Copy, Debug)]
struct Choice {
    kind: Kind,
    runs: u32,
    delta_runs: u32,
    /// Encoded size in bytes.
    size: usize,
}

/// Writes the values of candidate `kind` (transformed, before the physical encoding) to `buf`.
fn materialize(kind: Kind, values: &[i32], signed: bool, buf: &mut Vec<u32>) {
    buf.clear();
    match kind {
        Kind::Plain => buf.extend(values.iter().map(|&v| transform(v, signed))),
        Kind::Delta => {
            let mut previous = 0i32;
            buf.extend(values.iter().map(|&v| {
                let d = v.wrapping_sub(previous);
                previous = v;
                zigzag(d)
            }));
        }
        Kind::Rle | Kind::DeltaRle => {
            // `EncodingUtils.encodeRle`: the run lengths, then the run values
            let delta = kind == Kind::DeltaRle;
            let mut run_values: Vec<u32> = Vec::new();
            let mut previous_input = 0i32;
            let mut current = 0i32;
            let mut run = 0u32;
            for (i, &v) in values.iter().enumerate() {
                let x = if delta { v.wrapping_sub(previous_input) } else { v };
                previous_input = v;
                if i != 0 && x != current {
                    buf.push(run);
                    run_values.push(if delta { zigzag(current) } else { transform(current, signed) });
                    run = 0;
                }
                run += 1;
                current = x;
            }
            if !values.is_empty() {
                buf.push(run);
                run_values.push(if delta { zigzag(current) } else { transform(current, signed) });
            }
            buf.extend_from_slice(&run_values);
        }
    }
}

/// `IntegerEncoder.encodeInt` with `IntegerEncodingOption.AUTO`: picks the smallest of plain, delta, RLE and
/// delta-RLE (forcing RLE for constant streams).
fn choose(values: &[i32], fast_pfor: bool, signed: bool) -> Choice {
    choose_below(values, fast_pfor, signed, usize::MAX).expect("no budget")
}

/// [`choose`], but gives up (`None`) as soon as every candidate is known to need at least `budget` bytes.
fn choose_below(values: &[i32], fast_pfor: bool, signed: bool, budget: usize) -> Option<Choice> {
    let n = values.len();
    // The partial varint sums only grow, and they bound the final sizes from below: exactly for varints, and for
    // FastPFOR when every candidate is shorter than a block (`4 + 4 * ceil(sum / 4)` bytes), i.e. fewer than 128
    // values (the RLE candidates have up to `2 * n`).
    let can_give_up = budget != usize::MAX && (!fast_pfor || n < FAST_PFOR_BLOCK / 2);
    let (mut plain, mut delta, mut rle, mut delta_rle) = (0usize, 0usize, 0usize, 0usize);
    let mut runs = 1usize;
    let mut delta_runs = 1usize;
    let mut previous_value = 0i32;
    let mut previous_delta = 0i32;
    let mut run = 0u32;
    let mut delta_run = 0u32;
    for (i, &value) in values.iter().enumerate() {
        let d = value.wrapping_sub(previous_value);
        plain += varint_size(transform(value, signed));
        delta += varint_size(zigzag(d));
        if i != 0 && value != previous_value {
            runs += 1;
            rle += varint_size(run) + varint_size(transform(previous_value, signed));
            run = 0;
        }
        run += 1;
        if i != 0 && d != previous_delta {
            delta_runs += 1;
            delta_rle += varint_size(delta_run) + varint_size(zigzag(previous_delta));
            delta_run = 0;
        }
        delta_run += 1;
        previous_value = value;
        previous_delta = d;
        if can_give_up && i & 31 == 31 && plain.min(delta).min(rle).min(delta_rle) >= budget {
            return None;
        }
    }
    if n > 0 {
        rle += varint_size(run) + varint_size(transform(previous_value, signed));
        delta_rle += varint_size(delta_run) + varint_size(zigzag(previous_delta));
    }
    let has_rle = n / runs >= 2;
    let has_delta_rle = n / delta_runs >= 2;
    let lens = [n, n, 2 * runs, 2 * delta_runs];
    let mut sizes =
        [plain, delta, if has_rle { rle } else { usize::MAX }, if has_delta_rle { delta_rle } else { usize::MAX }];
    if fast_pfor {
        let kinds = [Kind::Plain, Kind::Delta, Kind::Rle, Kind::DeltaRle];
        for k in 0..4 {
            if sizes[k] == usize::MAX {
                continue;
            }
            sizes[k] = if lens[k] < FAST_PFOR_BLOCK {
                short_fast_pfor_size(lens[k], sizes[k])
            } else {
                with_scratch(|buf| {
                    materialize(kinds[k], values, signed, buf);
                    fastpfor::encoded_size(buf)
                })
            };
        }
    }
    // Since we want to force that all const streams use RLE encoding we use this current workaround
    let is_const_stream = has_rle && runs == 1;
    let index = if is_const_stream {
        2
    } else {
        let min = *sizes.iter().min().expect("non-empty");
        sizes.iter().position(|&s| s == min).expect("min exists")
    };
    let kind = [Kind::Plain, Kind::Delta, Kind::Rle, Kind::DeltaRle][index];
    Some(Choice { kind, runs: runs as u32, delta_runs: delta_runs as u32, size: sizes[index] })
}

/// Appends the chosen encoding of `values`.
fn write_choice(out: &mut Vec<u8>, choice: &Choice, values: &[i32], fast_pfor: bool, signed: bool) {
    let start = out.len();
    if fast_pfor {
        with_scratch(|buf| {
            materialize(choice.kind, values, signed, buf);
            fastpfor::encode_into(buf, out);
        });
    } else {
        match choice.kind {
            Kind::Plain => encode_physical_into(out, values, false, signed),
            _ => with_scratch(|buf| {
                materialize(choice.kind, values, signed, buf);
                for &v in buf.iter() {
                    put_varint(out, v);
                }
            }),
        }
    }
    debug_assert_eq!(choice.size, out.len() - start);
}

impl Choice {
    fn techniques(&self) -> (LogicalLevelTechnique, LogicalLevelTechnique) {
        match self.kind {
            Kind::Plain => (LogicalLevelTechnique::None, LogicalLevelTechnique::None),
            Kind::Delta => (LogicalLevelTechnique::Delta, LogicalLevelTechnique::None),
            Kind::Rle => (LogicalLevelTechnique::Rle, LogicalLevelTechnique::None),
            Kind::DeltaRle => (LogicalLevelTechnique::Delta, LogicalLevelTechnique::Rle),
        }
    }

    /// `(num_runs, physical_level_encoded_values_length)`.
    fn lengths(&self, num_values: usize) -> (u32, u32) {
        match self.kind {
            Kind::Plain | Kind::Delta => (0, num_values as u32),
            Kind::Rle => (self.runs, 2 * self.runs),
            Kind::DeltaRle => (self.delta_runs, 2 * self.delta_runs),
        }
    }
}

/// `IntegerEncoder.encodeInt` with `IntegerEncodingOption.AUTO`: picks the smallest of plain, delta, RLE and
/// delta-RLE (forcing RLE for constant streams).
pub fn encode_int(values: &[i32], fast_pfor: bool, signed: bool) -> IntegerEncodingResult {
    let choice = choose(values, fast_pfor, signed);
    let mut encoded_values = Vec::with_capacity(choice.size);
    write_choice(&mut encoded_values, &choice, values, fast_pfor, signed);
    let (logical_level_technique1, logical_level_technique2) = choice.techniques();
    let (num_runs, physical_level_encoded_values_length) = choice.lengths(values.len());
    IntegerEncodingResult {
        logical_level_technique1,
        logical_level_technique2,
        encoded_values,
        num_runs,
        physical_level_encoded_values_length,
    }
}

/// Size of `encode_int(values, fast_pfor, signed).encoded_values` (without encoding anything).
pub fn encoded_int_size(values: &[i32], fast_pfor: bool, signed: bool) -> usize {
    choose(values, fast_pfor, signed).size
}

/// [`encoded_int_size`] if it is below `budget`, otherwise `None` (often without looking at all values).
pub fn encoded_int_size_below(values: &[i32], fast_pfor: bool, signed: bool, budget: usize) -> Option<usize> {
    choose_below(values, fast_pfor, signed, budget).map(|c| c.size).filter(|&size| size < budget)
}

/// `IntegerEncoder.encodeLong` with `IntegerEncodingOption.AUTO` (always varint: FastPFOR only supports 32 bits),
/// appending the stream header and data.
fn encode_long_into(
    out: &mut Vec<u8>,
    values: &[i64],
    signed: bool,
    stream_type: PhysicalStreamType,
    logical_stream_type: Option<LogicalStreamType>,
) {
    let t = |v: i64| if signed { zigzag_long(v) } else { v as u64 };
    let n = values.len();
    let (mut plain, mut delta, mut rle, mut delta_rle) = (0usize, 0usize, 0usize, 0usize);
    let mut runs = 1usize;
    let mut delta_runs = 1usize;
    let mut previous_value = 0i64;
    let mut previous_delta = 0i64;
    let mut run = 0u64;
    let mut delta_run = 0u64;
    for (i, &value) in values.iter().enumerate() {
        let d = value.wrapping_sub(previous_value);
        plain += varlong_size(t(value));
        delta += varlong_size(zigzag_long(d));
        if i != 0 && value != previous_value {
            runs += 1;
            rle += varlong_size(run) + varlong_size(t(previous_value));
            run = 0;
        }
        run += 1;
        if i != 0 && d != previous_delta {
            delta_runs += 1;
            delta_rle += varlong_size(delta_run) + varlong_size(zigzag_long(previous_delta));
            delta_run = 0;
        }
        delta_run += 1;
        previous_value = value;
        previous_delta = d;
    }
    if n > 0 {
        rle += varlong_size(run) + varlong_size(t(previous_value));
        delta_rle += varlong_size(delta_run) + varlong_size(zigzag_long(previous_delta));
    }
    let has_rle = n / runs >= 2;
    let has_delta_rle = n / delta_runs >= 2;
    let sizes =
        [plain, delta, if has_rle { rle } else { usize::MAX }, if has_delta_rle { delta_rle } else { usize::MAX }];
    let index = if has_rle && runs == 1 {
        2
    } else {
        let min = *sizes.iter().min().expect("non-empty");
        sizes.iter().position(|&s| s == min).expect("min exists")
    };
    let kind = [Kind::Plain, Kind::Delta, Kind::Rle, Kind::DeltaRle][index];
    let choice = Choice { kind, runs: runs as u32, delta_runs: delta_runs as u32, size: sizes[index] };
    write_header(out, &choice, PhysicalLevelTechnique::Varint, stream_type, logical_stream_type, n);
    let start = out.len();
    match kind {
        Kind::Plain => values.iter().for_each(|&v| put_varlong(out, t(v))),
        Kind::Delta => {
            let mut previous = 0i64;
            for &v in values {
                put_varlong(out, zigzag_long(v.wrapping_sub(previous)));
                previous = v;
            }
        }
        Kind::Rle | Kind::DeltaRle => {
            // the run lengths, then the run values
            let delta = kind == Kind::DeltaRle;
            let mut run_values: Vec<u64> = Vec::new();
            let mut previous_input = 0i64;
            let mut current = 0i64;
            let mut run = 0u64;
            for (i, &v) in values.iter().enumerate() {
                let x = if delta { v.wrapping_sub(previous_input) } else { v };
                previous_input = v;
                if i != 0 && x != current {
                    put_varlong(out, run);
                    run_values.push(if delta { zigzag_long(current) } else { t(current) });
                    run = 0;
                }
                run += 1;
                current = x;
            }
            if n > 0 {
                put_varlong(out, run);
                run_values.push(if delta { zigzag_long(current) } else { t(current) });
            }
            for v in run_values {
                put_varlong(out, v);
            }
        }
    }
    debug_assert_eq!(choice.size, out.len() - start);
}

fn physical(fast_pfor: bool) -> PhysicalLevelTechnique {
    if fast_pfor { PhysicalLevelTechnique::FastPfor } else { PhysicalLevelTechnique::Varint }
}

fn write_header(
    out: &mut Vec<u8>,
    choice: &Choice,
    physical_level_technique: PhysicalLevelTechnique,
    stream_type: PhysicalStreamType,
    logical_stream_type: Option<LogicalStreamType>,
    num_values: usize,
) {
    let (technique1, technique2) = choice.techniques();
    let (num_runs, physical_length) = choice.lengths(num_values);
    let mut metadata = StreamMetadata::new(
        stream_type,
        logical_stream_type,
        technique1,
        technique2,
        physical_level_technique,
        physical_length,
        choice.size as u32,
    );
    if matches!(choice.kind, Kind::Rle | Kind::DeltaRle) {
        metadata.extra = StreamExtra::Rle(num_runs, num_values as u32);
    }
    metadata.encode_into(out);
}

/// `IntegerEncoder.encodeIntStream`: an integer stream with its metadata header.
pub fn encode_int_stream(
    out: &mut Vec<u8>,
    values: &[i32],
    fast_pfor: bool,
    signed: bool,
    stream_type: PhysicalStreamType,
    logical_stream_type: Option<LogicalStreamType>,
) {
    let choice = choose(values, fast_pfor, signed);
    write_header(out, &choice, physical(fast_pfor), stream_type, logical_stream_type, values.len());
    out.reserve(choice.size);
    write_choice(out, &choice, values, fast_pfor, signed);
}

/// `IntegerEncoder.encodeLongStream`.
pub fn encode_long_stream(
    out: &mut Vec<u8>,
    values: &[i64],
    signed: bool,
    stream_type: PhysicalStreamType,
    logical_stream_type: Option<LogicalStreamType>,
) {
    encode_long_into(out, values, signed, stream_type, logical_stream_type);
}

/// Morton code deltas (not zigzag encoded) into `buf`.
fn morton_deltas(values: &[i32], buf: &mut Vec<u32>) {
    let mut previous = 0i32;
    buf.extend(values.iter().map(|&v| {
        let d = v.wrapping_sub(previous);
        previous = v;
        d as u32
    }));
}

/// Size of the `encodedValues` of `IntegerEncoder.encodeMortonCodes` (delta, not zigzag, encoded morton codes).
pub fn encoded_morton_codes_size(values: &[i32], fast_pfor: bool) -> usize {
    if fast_pfor && values.len() >= FAST_PFOR_BLOCK {
        return with_scratch(|buf| {
            morton_deltas(values, buf);
            fastpfor::encoded_size(buf)
        });
    }
    let mut previous = 0i32;
    let varint_bytes = values
        .iter()
        .map(|&v| {
            let d = v.wrapping_sub(previous);
            previous = v;
            varint_size(d as u32)
        })
        .sum();
    if fast_pfor { short_fast_pfor_size(values.len(), varint_bytes) } else { varint_bytes }
}

/// `IntegerEncoder.encodeMortonStream`.
pub fn encode_morton_stream(out: &mut Vec<u8>, values: &[i32], num_bits: u32, coordinate_shift: u32, fast_pfor: bool) {
    let size = encoded_morton_codes_size(values, fast_pfor);
    StreamMetadata::new(
        PhysicalStreamType::Data,
        Some(LogicalStreamType::Dictionary(super::encoding::DictionaryType::Morton)),
        LogicalLevelTechnique::Morton,
        LogicalLevelTechnique::Delta,
        physical(fast_pfor),
        values.len() as u32,
        size as u32,
    )
    .with_extra(StreamExtra::Morton(num_bits, coordinate_shift))
    .encode_into(out);
    let start = out.len();
    with_scratch(|buf| {
        morton_deltas(values, buf);
        if fast_pfor {
            fastpfor::encode_into(buf, out);
        } else {
            for &v in buf.iter() {
                put_varint(out, v);
            }
        }
    });
    debug_assert_eq!(size, out.len() - start);
}

#[cfg(test)]
mod tests {
    use super::super::integer_encoder_reference as reference;
    use super::*;

    #[test]
    fn const_stream_uses_rle() {
        let r = encode_int(&[5, 5, 5, 5], false, false);
        assert_eq!(LogicalLevelTechnique::Rle, r.logical_level_technique1);
        assert_eq!(vec![4, 5], r.encoded_values);
        assert_eq!(2, r.physical_level_encoded_values_length);
        assert_eq!(1, r.num_runs);
    }

    #[test]
    fn sequence_uses_delta_rle() {
        let values: Vec<i32> = (0..100).collect();
        let r = encode_int(&values, false, false);
        assert_eq!(LogicalLevelTechnique::Delta, r.logical_level_technique1);
        assert_eq!(LogicalLevelTechnique::Rle, r.logical_level_technique2);
        // runs [1, 99], values zigzag [0, 1]
        assert_eq!(vec![1, 99, 0, 2], r.encoded_values);
        assert_eq!(encoded_int_size(&values, false, false), r.encoded_values.len());
    }

    #[test]
    fn plain_for_random() {
        let r = encode_int(&[3, 1, 4, 1, 5], false, false);
        assert_eq!(LogicalLevelTechnique::None, r.logical_level_technique1);
        assert_eq!(vec![3, 1, 4, 1, 5], r.encoded_values);
    }

    #[test]
    fn long_stream() {
        let mut out = Vec::new();
        encode_long_stream(&mut out, &[1 << 40, 1 << 40], false, PhysicalStreamType::Data, None);
        let r = reference::encode_long(&[1 << 40, 1 << 40], false);
        assert_eq!(LogicalLevelTechnique::Rle, r.logical_level_technique1);
    }

    /// Deterministic streams of many shapes: constant, sequences, runs, small / large / negative values, around the
    /// 256-value FastPFOR block size.
    fn streams() -> Vec<Vec<i64>> {
        let mut state = 11u64;
        let mut next = move || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 16) as i64
        };
        let mut result = vec![vec![], vec![0], vec![7], vec![-3, -3], vec![i32::MIN as i64, i32::MAX as i64]];
        for len in [1usize, 2, 3, 5, 17, 100, 127, 128, 129, 255, 256, 257, 300, 511, 512, 513, 1000, 5000, 70_000] {
            for shape in 0..9 {
                let mut v = Vec::with_capacity(len);
                let mut x = 0i64;
                for i in 0..len {
                    let r = next();
                    x = match shape {
                        0 => 5,
                        1 => i as i64,
                        2 => i as i64 * 3 + 1000,
                        3 => r % 100,
                        4 => (r % 2_000_000) - 1_000_000,
                        5 => {
                            if r % 10 == 0 {
                                r % 1000
                            } else {
                                x
                            }
                        }
                        6 => x + (r % 5) - 2,
                        7 => r as i32 as i64,
                        _ => (i / 7) as i64 * ((r % 3) - 1),
                    };
                    v.push(x);
                }
                result.push(v);
            }
        }
        result
    }

    #[test]
    fn same_bytes_as_straight_port() {
        for values in streams() {
            let ints: Vec<i32> = values.iter().map(|&v| v as i32).collect();
            for fast_pfor in [false, true] {
                for signed in [false, true] {
                    let mut expected = Vec::new();
                    reference::encode_int_stream(
                        &mut expected,
                        &ints,
                        fast_pfor,
                        signed,
                        PhysicalStreamType::Length,
                        None,
                    );
                    let mut actual = Vec::new();
                    encode_int_stream(&mut actual, &ints, fast_pfor, signed, PhysicalStreamType::Length, None);
                    assert_eq!(expected, actual, "{} values, fast_pfor {fast_pfor} signed {signed}", ints.len());
                    let size = reference::encoded_int_size(&ints, fast_pfor, signed);
                    assert_eq!(size, encoded_int_size(&ints, fast_pfor, signed));
                    for budget in [0, 1, size / 2, size.saturating_sub(1), size, size + 1, usize::MAX] {
                        let expected = if size < budget { Some(size) } else { None };
                        assert_eq!(expected, encoded_int_size_below(&ints, fast_pfor, signed, budget));
                    }
                    assert_eq!(
                        reference::encode_int(&ints, fast_pfor, signed).encoded_values,
                        encode_int(&ints, fast_pfor, signed).encoded_values
                    );
                }
                let mut expected = Vec::new();
                reference::encode_morton_stream(&mut expected, &ints, 13, 7, fast_pfor);
                let mut actual = Vec::new();
                encode_morton_stream(&mut actual, &ints, 13, 7, fast_pfor);
                assert_eq!(expected, actual);
                assert_eq!(
                    reference::encoded_morton_codes_size(&ints, fast_pfor),
                    encoded_morton_codes_size(&ints, fast_pfor)
                );
            }
            for signed in [false, true] {
                let longs: Vec<i64> = values.iter().map(|&v| v.wrapping_mul(1 << 20)).collect();
                for input in [&values, &longs] {
                    let mut expected = Vec::new();
                    reference::encode_long_stream(&mut expected, input, signed, PhysicalStreamType::Data, None);
                    let mut actual = Vec::new();
                    encode_long_stream(&mut actual, input, signed, PhysicalStreamType::Data, None);
                    assert_eq!(expected, actual, "{} longs, signed {signed}", input.len());
                }
            }
        }
    }
}
