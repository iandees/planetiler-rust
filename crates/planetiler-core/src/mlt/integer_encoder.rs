//! Port of `org.maplibre.mlt.converter.encodings.IntegerEncoder` (only the `IntegerEncodingOption.AUTO` mode, which
//! is the only one Planetiler uses).

use super::encoding::{
    LogicalLevelTechnique, LogicalStreamType, PhysicalLevelTechnique, PhysicalStreamType, StreamExtra, StreamMetadata,
    encode_rle, put_varint, put_varlong, varint_size, varlong_size, zigzag, zigzag_long,
};
use super::fastpfor;

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

/// Physical encoding of `values` (Java `encodeFastPfor` / `EncodingUtils.encodeVarints`), zigzag-encoding them first
/// if `signed`.
pub fn encode_physical(values: &[i32], fast_pfor: bool, signed: bool) -> Vec<u8> {
    if fast_pfor {
        let transformed: Vec<u32> = if signed {
            values.iter().map(|&v| zigzag(v)).collect()
        } else {
            values.iter().map(|&v| v as u32).collect()
        };
        fastpfor::encode(&transformed)
    } else {
        let mut out = Vec::with_capacity(values.len() + 4);
        if signed {
            for &v in values {
                put_varint(&mut out, zigzag(v));
            }
        } else {
            for &v in values {
                put_varint(&mut out, v as u32);
            }
        }
        out
    }
}

/// Size in bytes of [`encode_physical`] without materializing varint output.
fn physical_size(values: &[i32], fast_pfor: bool, signed: bool) -> usize {
    if fast_pfor {
        encode_physical(values, true, signed).len()
    } else if signed {
        values.iter().map(|&v| varint_size(zigzag(v))).sum()
    } else {
        values.iter().map(|&v| varint_size(v as u32)).sum()
    }
}

/// One candidate encoding of [`encode_int`]: lazily materialized for varints.
enum Candidate {
    Bytes(Vec<u8>),
    /// `(values, signed)` to varint-encode, with the precomputed size.
    Varint(Vec<i32>, bool, usize),
}

impl Candidate {
    fn new(values: Vec<i32>, fast_pfor: bool, signed: bool) -> Candidate {
        if fast_pfor {
            Candidate::Bytes(encode_physical(&values, true, signed))
        } else {
            let size = physical_size(&values, false, signed);
            Candidate::Varint(values, signed, size)
        }
    }

    fn len(&self) -> usize {
        match self {
            Candidate::Bytes(b) => b.len(),
            Candidate::Varint(_, _, size) => *size,
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        match self {
            Candidate::Bytes(b) => b,
            Candidate::Varint(values, signed, _) => encode_physical(&values, false, signed),
        }
    }
}

fn concat(a: Vec<i32>, b: impl IntoIterator<Item = i32>) -> Vec<i32> {
    let mut a = a;
    a.extend(b);
    a
}

/// `IntegerEncoder.encodeInt` with `IntegerEncodingOption.AUTO`: picks the smallest of plain, delta, RLE and
/// delta-RLE (forcing RLE for constant streams).
pub fn encode_int(values: &[i32], fast_pfor: bool, signed: bool) -> IntegerEncodingResult {
    let (mut result, chosen) = encode_int_impl(values, fast_pfor, signed);
    result.encoded_values = chosen.into_bytes();
    result
}

/// Size of `encode_int(values, fast_pfor, signed).encoded_values` (avoids materializing varint output).
pub fn encoded_int_size(values: &[i32], fast_pfor: bool, signed: bool) -> usize {
    encode_int_impl(values, fast_pfor, signed).1.len()
}

/// Returns the result (without its `encoded_values`) and the chosen candidate encoding.
fn encode_int_impl(values: &[i32], fast_pfor: bool, signed: bool) -> (IntegerEncodingResult, Candidate) {
    let mut previous_value = 0i32;
    let mut previous_delta = 0i32;
    let mut runs = 1usize;
    let mut delta_runs = 1usize;
    let mut delta_values = Vec::with_capacity(values.len());
    for (i, &value) in values.iter().enumerate() {
        let delta = value.wrapping_sub(previous_value);
        delta_values.push(delta);
        if value != previous_value && i != 0 {
            runs += 1;
        }
        if delta != previous_delta && i != 0 {
            delta_runs += 1;
        }
        previous_value = value;
        previous_delta = delta;
    }

    let plain = Candidate::new(values.to_vec(), fast_pfor, signed);
    let delta = Candidate::new(delta_values.clone(), fast_pfor, true);
    let mut rle: Option<Candidate> = None;
    let mut delta_rle: Option<Candidate> = None;
    let mut rle_physical_len = 0usize;
    let mut delta_rle_physical_len = 0usize;

    // Since we want to force that all const streams use RLE encoding we use this current workaround
    let mut is_const_stream = false;
    if values.len() / runs >= 2 {
        let (rle_runs, rle_values) = encode_rle(values);
        rle_physical_len = rle_runs.len() + rle_values.len();
        is_const_stream = rle_runs.len() == 1;
        let combined = if signed {
            concat(rle_runs, rle_values.iter().map(|&v| zigzag(v) as i32))
        } else {
            concat(rle_runs, rle_values)
        };
        rle = Some(Candidate::new(combined, fast_pfor, false));
    }

    if delta_values.len() / delta_runs >= 2 {
        let (rle_runs, rle_values) = encode_rle(&delta_values);
        delta_rle_physical_len = rle_runs.len() + rle_values.len();
        let combined = concat(rle_runs, rle_values.iter().map(|&v| zigzag(v) as i32));
        delta_rle = Some(Candidate::new(combined, fast_pfor, false));
    }

    let sizes = [
        plain.len(),
        delta.len(),
        rle.as_ref().map_or(usize::MAX, Candidate::len),
        delta_rle.as_ref().map_or(usize::MAX, Candidate::len),
    ];
    let index = if is_const_stream {
        2
    } else {
        let min = *sizes.iter().min().expect("non-empty");
        sizes.iter().position(|&s| s == min).expect("min exists")
    };
    let chosen = match index {
        0 => plain,
        1 => delta,
        2 => rle.expect("rle candidate"),
        _ => delta_rle.expect("delta rle candidate"),
    };
    let mut result = IntegerEncodingResult {
        logical_level_technique1: LogicalLevelTechnique::None,
        logical_level_technique2: LogicalLevelTechnique::None,
        encoded_values: Vec::new(),
        num_runs: 0,
        physical_level_encoded_values_length: values.len() as u32,
    };
    match index {
        2 => {
            result.num_runs = runs as u32;
            result.physical_level_encoded_values_length = rle_physical_len as u32;
            result.logical_level_technique1 = LogicalLevelTechnique::Rle;
        }
        3 => {
            result.num_runs = delta_runs as u32;
            result.physical_level_encoded_values_length = delta_rle_physical_len as u32;
            result.logical_level_technique1 = LogicalLevelTechnique::Delta;
            result.logical_level_technique2 = LogicalLevelTechnique::Rle;
        }
        1 => result.logical_level_technique1 = LogicalLevelTechnique::Delta,
        _ => {}
    }
    (result, chosen)
}

/// `IntegerEncoder.encodeLong` with `IntegerEncodingOption.AUTO` (always varint: FastPFOR only supports 32 bits).
pub fn encode_long(values: &[i64], signed: bool) -> IntegerEncodingResult {
    let mut previous_value = 0i64;
    let mut previous_delta = 0i64;
    let mut runs = 1usize;
    let mut delta_runs = 1usize;
    let mut delta_values = Vec::with_capacity(values.len());
    for (i, &value) in values.iter().enumerate() {
        let delta = value.wrapping_sub(previous_value);
        delta_values.push(delta);
        if value != previous_value && i != 0 {
            runs += 1;
        }
        if delta != previous_delta && i != 0 {
            delta_runs += 1;
        }
        previous_value = value;
        previous_delta = delta;
    }
    let varints = |vals: &[i64], signed: bool| -> Vec<u8> {
        let mut out = Vec::with_capacity(vals.iter().map(|&v| varlong_size(v as u64)).sum());
        for &v in vals {
            put_varlong(&mut out, if signed { zigzag_long(v) } else { v as u64 });
        }
        out
    };

    let plain = varints(values, signed);
    let delta = varints(&delta_values, true);
    let mut rle: Option<Vec<u8>> = None;
    let mut delta_rle: Option<Vec<u8>> = None;
    let mut rle_physical_len = 0usize;
    let mut delta_rle_physical_len = 0usize;
    let mut is_const_stream = false;
    if values.len() / runs >= 2 {
        let (rle_runs, rle_values) = encode_rle(values);
        rle_physical_len = rle_runs.len() + rle_values.len();
        is_const_stream = rle_runs.len() == 1;
        let mut combined = rle_runs;
        if signed {
            combined.extend(rle_values.iter().map(|&v| zigzag_long(v) as i64));
        } else {
            combined.extend(rle_values);
        }
        rle = Some(varints(&combined, false));
    }
    if delta_values.len() / delta_runs >= 2 {
        let (rle_runs, rle_values) = encode_rle(&delta_values);
        delta_rle_physical_len = rle_runs.len() + rle_values.len();
        let mut combined = rle_runs;
        combined.extend(rle_values.iter().map(|&v| zigzag_long(v) as i64));
        delta_rle = Some(varints(&combined, false));
    }
    let sizes = [
        plain.len(),
        delta.len(),
        rle.as_ref().map_or(usize::MAX, Vec::len),
        delta_rle.as_ref().map_or(usize::MAX, Vec::len),
    ];
    let index = if is_const_stream {
        2
    } else {
        let min = *sizes.iter().min().expect("non-empty");
        sizes.iter().position(|&s| s == min).expect("min exists")
    };
    let mut result = IntegerEncodingResult {
        logical_level_technique1: LogicalLevelTechnique::None,
        logical_level_technique2: LogicalLevelTechnique::None,
        encoded_values: Vec::new(),
        num_runs: 0,
        physical_level_encoded_values_length: values.len() as u32,
    };
    match index {
        0 => result.encoded_values = plain,
        1 => {
            result.encoded_values = delta;
            result.logical_level_technique1 = LogicalLevelTechnique::Delta;
        }
        2 => {
            result.encoded_values = rle.expect("rle");
            result.num_runs = runs as u32;
            result.physical_level_encoded_values_length = rle_physical_len as u32;
            result.logical_level_technique1 = LogicalLevelTechnique::Rle;
        }
        _ => {
            result.encoded_values = delta_rle.expect("delta rle");
            result.num_runs = delta_runs as u32;
            result.physical_level_encoded_values_length = delta_rle_physical_len as u32;
            result.logical_level_technique1 = LogicalLevelTechnique::Delta;
            result.logical_level_technique2 = LogicalLevelTechnique::Rle;
        }
    }
    result
}

fn physical(fast_pfor: bool) -> PhysicalLevelTechnique {
    if fast_pfor { PhysicalLevelTechnique::FastPfor } else { PhysicalLevelTechnique::Varint }
}

fn write_stream(
    out: &mut Vec<u8>,
    encoded: IntegerEncodingResult,
    physical_level_technique: PhysicalLevelTechnique,
    stream_type: PhysicalStreamType,
    logical_stream_type: Option<LogicalStreamType>,
    num_values: usize,
) {
    let mut metadata = StreamMetadata::new(
        stream_type,
        logical_stream_type,
        encoded.logical_level_technique1,
        encoded.logical_level_technique2,
        physical_level_technique,
        encoded.physical_level_encoded_values_length,
        encoded.encoded_values.len() as u32,
    );
    if encoded.logical_level_technique1 == LogicalLevelTechnique::Rle
        || encoded.logical_level_technique2 == LogicalLevelTechnique::Rle
    {
        metadata.extra = StreamExtra::Rle(encoded.num_runs, num_values as u32);
    }
    metadata.encode_into(out);
    out.extend_from_slice(&encoded.encoded_values);
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
    let encoded = encode_int(values, fast_pfor, signed);
    write_stream(out, encoded, physical(fast_pfor), stream_type, logical_stream_type, values.len());
}

/// `IntegerEncoder.encodeLongStream`.
pub fn encode_long_stream(
    out: &mut Vec<u8>,
    values: &[i64],
    signed: bool,
    stream_type: PhysicalStreamType,
    logical_stream_type: Option<LogicalStreamType>,
) {
    let encoded = encode_long(values, signed);
    write_stream(out, encoded, PhysicalLevelTechnique::Varint, stream_type, logical_stream_type, values.len());
}

/// `IntegerEncoder.encodeMortonCodes`: delta (not zigzag) encoded morton codes.
pub fn encode_morton_codes(values: &[i32], fast_pfor: bool) -> IntegerEncodingResult {
    let mut previous = 0i32;
    let deltas: Vec<i32> = values
        .iter()
        .map(|&v| {
            let d = v.wrapping_sub(previous);
            previous = v;
            d
        })
        .collect();
    IntegerEncodingResult {
        logical_level_technique1: LogicalLevelTechnique::Morton,
        logical_level_technique2: LogicalLevelTechnique::Delta,
        encoded_values: encode_physical(&deltas, fast_pfor, false),
        num_runs: 0,
        physical_level_encoded_values_length: values.len() as u32,
    }
}

/// Size of [`encode_morton_codes`] output.
pub fn encoded_morton_codes_size(values: &[i32], fast_pfor: bool) -> usize {
    if fast_pfor {
        return encode_morton_codes(values, true).encoded_values.len();
    }
    let mut previous = 0i32;
    values
        .iter()
        .map(|&v| {
            let d = v.wrapping_sub(previous);
            previous = v;
            varint_size(d as u32)
        })
        .sum()
}

/// `IntegerEncoder.encodeMortonStream`.
pub fn encode_morton_stream(out: &mut Vec<u8>, values: &[i32], num_bits: u32, coordinate_shift: u32, fast_pfor: bool) {
    let encoded = encode_morton_codes(values, fast_pfor);
    StreamMetadata::new(
        PhysicalStreamType::Data,
        Some(LogicalStreamType::Dictionary(super::encoding::DictionaryType::Morton)),
        encoded.logical_level_technique1,
        encoded.logical_level_technique2,
        physical(fast_pfor),
        encoded.physical_level_encoded_values_length,
        encoded.encoded_values.len() as u32,
    )
    .with_extra(StreamExtra::Morton(num_bits, coordinate_shift))
    .encode_into(out);
    out.extend_from_slice(&encoded.encoded_values);
}

#[cfg(test)]
mod tests {
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
        let r = encode_long(&[1 << 40, 1 << 40], false);
        assert_eq!(LogicalLevelTechnique::Rle, r.logical_level_technique1);
    }
}
