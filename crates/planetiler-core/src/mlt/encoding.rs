//! Port of the low-level MLT encoding helpers: `EncodingUtils`, `ByteRleEncoder`, `BooleanEncoder`, the stream
//! metadata classes (`StreamMetadata`, `RleEncodedStreamMetadata`, `MortonEncodedStreamMetadata`) and the stream type
//! enums from `org.maplibre.mlt.metadata.stream`.

/// `PhysicalStreamType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PhysicalStreamType {
    Present = 0,
    Data = 1,
    Offset = 2,
    Length = 3,
}

/// `DictionaryType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DictionaryType {
    None = 0,
    Single = 1,
    Shared = 2,
    Vertex = 3,
    Morton = 4,
    Fsst = 5,
}

/// `LengthType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LengthType {
    VarBinary = 0,
    Geometries = 1,
    Parts = 2,
    Rings = 3,
    Triangles = 4,
    Symbol = 5,
    Dictionary = 6,
}

/// `OffsetType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OffsetType {
    Vertex = 0,
    Index = 1,
    String = 2,
    Key = 3,
}

/// `LogicalStreamType`: exactly one of the dictionary/offset/length types, or none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalStreamType {
    Dictionary(DictionaryType),
    Offset(OffsetType),
    Length(LengthType),
}

impl LogicalStreamType {
    fn code(self) -> u8 {
        match self {
            LogicalStreamType::Dictionary(d) => d as u8,
            LogicalStreamType::Offset(o) => o as u8,
            LogicalStreamType::Length(l) => l as u8,
        }
    }
}

/// `LogicalLevelTechnique`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LogicalLevelTechnique {
    None = 0,
    Delta = 1,
    ComponentwiseDelta = 2,
    Rle = 3,
    Morton = 4,
    Pde = 5,
}

impl LogicalLevelTechnique {
    fn from_code(code: u8) -> LogicalLevelTechnique {
        match code {
            1 => LogicalLevelTechnique::Delta,
            2 => LogicalLevelTechnique::ComponentwiseDelta,
            3 => LogicalLevelTechnique::Rle,
            4 => LogicalLevelTechnique::Morton,
            5 => LogicalLevelTechnique::Pde,
            _ => LogicalLevelTechnique::None,
        }
    }
}

/// `PhysicalLevelTechnique`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PhysicalLevelTechnique {
    None = 0,
    FastPfor = 1,
    Varint = 2,
    Alp = 3,
}

impl PhysicalLevelTechnique {
    fn from_code(code: u8) -> PhysicalLevelTechnique {
        match code {
            1 => PhysicalLevelTechnique::FastPfor,
            2 => PhysicalLevelTechnique::Varint,
            3 => PhysicalLevelTechnique::Alp,
            _ => PhysicalLevelTechnique::None,
        }
    }
}

/// Extra stream header fields of `RleEncodedStreamMetadata` / `MortonEncodedStreamMetadata`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamExtra {
    None,
    /// `runs`, `numRleValues`.
    Rle(u32, u32),
    /// `numBits`, `coordinateShift`.
    Morton(u32, u32),
}

/// `StreamMetadata` (with the RLE / Morton subclasses folded into [`StreamExtra`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamMetadata {
    pub physical_stream_type: PhysicalStreamType,
    pub logical_stream_type: Option<LogicalStreamType>,
    pub logical_level_technique1: LogicalLevelTechnique,
    pub logical_level_technique2: LogicalLevelTechnique,
    pub physical_level_technique: PhysicalLevelTechnique,
    /// After the logical level technique was applied: when RLE is used it is the length of the runs and values
    /// arrays.
    pub num_values: u32,
    pub byte_length: u32,
    pub extra: StreamExtra,
}

impl StreamMetadata {
    pub fn new(
        physical_stream_type: PhysicalStreamType,
        logical_stream_type: Option<LogicalStreamType>,
        logical_level_technique1: LogicalLevelTechnique,
        logical_level_technique2: LogicalLevelTechnique,
        physical_level_technique: PhysicalLevelTechnique,
        num_values: u32,
        byte_length: u32,
    ) -> StreamMetadata {
        StreamMetadata {
            physical_stream_type,
            logical_stream_type,
            logical_level_technique1,
            logical_level_technique2,
            physical_level_technique,
            num_values,
            byte_length,
            extra: StreamExtra::None,
        }
    }

    pub fn with_extra(mut self, extra: StreamExtra) -> StreamMetadata {
        self.extra = extra;
        self
    }

    /// `StreamMetadata.encode()`: appends the encoded header to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let logical = self.logical_stream_type.map_or(0, LogicalStreamType::code);
        out.push(((self.physical_stream_type as u8) << 4) | logical);
        out.push(
            ((self.logical_level_technique1 as u8) << 5)
                | ((self.logical_level_technique2 as u8) << 2)
                | self.physical_level_technique as u8,
        );
        put_varint(out, self.num_values);
        put_varint(out, self.byte_length);
        match self.extra {
            StreamExtra::None => {}
            StreamExtra::Rle(a, b) | StreamExtra::Morton(a, b) => {
                put_varint(out, a);
                put_varint(out, b);
            }
        }
    }

    /// `StreamMetadataDecoder.decode`: parses a stream header at `offset`, advancing it past the header.
    pub fn decode(tile: &[u8], offset: &mut usize) -> anyhow::Result<StreamMetadata> {
        let stream_type = *tile.get(*offset).ok_or_else(|| anyhow::anyhow!("truncated stream metadata"))?;
        let physical_stream_type = match stream_type >> 4 {
            0 => PhysicalStreamType::Present,
            1 => PhysicalStreamType::Data,
            2 => PhysicalStreamType::Offset,
            3 => PhysicalStreamType::Length,
            other => anyhow::bail!("invalid physical stream type {other}"),
        };
        let low = stream_type & 0xf;
        let logical_stream_type = match physical_stream_type {
            PhysicalStreamType::Data => Some(LogicalStreamType::Dictionary(match low {
                0 => DictionaryType::None,
                1 => DictionaryType::Single,
                2 => DictionaryType::Shared,
                3 => DictionaryType::Vertex,
                4 => DictionaryType::Morton,
                5 => DictionaryType::Fsst,
                other => anyhow::bail!("invalid dictionary type {other}"),
            })),
            PhysicalStreamType::Offset => Some(LogicalStreamType::Offset(match low {
                0 => OffsetType::Vertex,
                1 => OffsetType::Index,
                2 => OffsetType::String,
                3 => OffsetType::Key,
                other => anyhow::bail!("invalid offset type {other}"),
            })),
            PhysicalStreamType::Length => Some(LogicalStreamType::Length(match low {
                0 => LengthType::VarBinary,
                1 => LengthType::Geometries,
                2 => LengthType::Parts,
                3 => LengthType::Rings,
                4 => LengthType::Triangles,
                5 => LengthType::Symbol,
                6 => LengthType::Dictionary,
                other => anyhow::bail!("invalid length type {other}"),
            })),
            PhysicalStreamType::Present => None,
        };
        *offset += 1;
        let header = *tile.get(*offset).ok_or_else(|| anyhow::anyhow!("truncated stream metadata"))?;
        *offset += 1;
        let logical_level_technique1 = LogicalLevelTechnique::from_code(header >> 5);
        let logical_level_technique2 = LogicalLevelTechnique::from_code((header >> 2) & 0x7);
        let physical_level_technique = PhysicalLevelTechnique::from_code(header & 0x3);
        let num_values = decode_varint(tile, offset)?;
        let byte_length = decode_varint(tile, offset)?;
        let mut result = StreamMetadata::new(
            physical_stream_type,
            logical_stream_type,
            logical_level_technique1,
            logical_level_technique2,
            physical_level_technique,
            num_values,
            byte_length,
        );
        // Currently morton can't be combined with RLE only with delta
        if logical_level_technique1 == LogicalLevelTechnique::Morton {
            let a = decode_varint(tile, offset)?;
            let b = decode_varint(tile, offset)?;
            result.extra = StreamExtra::Morton(a, b);
        } else if (logical_level_technique1 == LogicalLevelTechnique::Rle
            || logical_level_technique2 == LogicalLevelTechnique::Rle)
            && physical_level_technique != PhysicalLevelTechnique::None
        {
            // Boolean rle doesn't need additional information
            let a = decode_varint(tile, offset)?;
            let b = decode_varint(tile, offset)?;
            result.extra = StreamExtra::Rle(a, b);
        }
        Ok(result)
    }
}

/// Appends an unsigned (protobuf-style) varint of the 32 bits of `v` (Java `EncodingUtils.putVarInt(int)`, which
/// treats negative ints as unsigned).
#[inline]
pub fn put_varint(out: &mut Vec<u8>, v: u32) {
    let mut v = v;
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Appends an unsigned varint of the 64 bits of `v` (Java `putVarInt(long)`).
#[inline]
pub fn put_varlong(out: &mut Vec<u8>, v: u64) {
    let mut v = v;
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Number of bytes [`put_varint`] writes for `v`.
#[inline]
pub fn varint_size(v: u32) -> usize {
    let bits = 32 - v.leading_zeros() as usize;
    bits.div_ceil(7).max(1)
}

/// Number of bytes [`put_varlong`] writes for `v`.
#[inline]
pub fn varlong_size(v: u64) -> usize {
    let bits = 64 - v.leading_zeros() as usize;
    bits.div_ceil(7).max(1)
}

/// Java `EncodingUtils.encodeVarint(int, false)`.
pub fn encode_varint(v: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(varint_size(v));
    put_varint(&mut out, v);
    out
}

/// Appends a length-prefixed UTF-8 string (Java `EncodingUtils.putString`).
pub fn put_string(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

#[inline]
pub fn zigzag(v: i32) -> u32 {
    ((v >> 31) ^ (v << 1)) as u32
}

#[inline]
pub fn zigzag_long(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
pub fn unzigzag(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

#[inline]
pub fn unzigzag_long(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Reads an unsigned varint of up to 32 bits (Java `DecodingUtils.decodeVarint`).
pub fn decode_varint(data: &[u8], offset: &mut usize) -> anyhow::Result<u32> {
    let mut result: u32 = 0;
    let mut shift = 0;
    loop {
        let b = *data.get(*offset).ok_or_else(|| anyhow::anyhow!("truncated varint"))?;
        *offset += 1;
        if shift < 32 {
            result |= ((b & 0x7f) as u32) << shift;
        }
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift > 35 {
            anyhow::bail!("varint too long");
        }
    }
}

/// Reads an unsigned varint of up to 64 bits.
pub fn decode_varlong(data: &[u8], offset: &mut usize) -> anyhow::Result<u64> {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let b = *data.get(*offset).ok_or_else(|| anyhow::anyhow!("truncated varint"))?;
        *offset += 1;
        if shift < 64 {
            result |= ((b & 0x7f) as u64) << shift;
        }
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift > 70 {
            anyhow::bail!("varint too long");
        }
    }
}

/// `EncodingUtils.encodeRle(int[])`: returns `(runs, values)`. `values` must not be empty.
pub fn encode_rle<T>(values: &[T]) -> (Vec<T>, Vec<T>)
where
    T: Copy + PartialEq + Default + From<u8> + TryFrom<usize>,
    <T as TryFrom<usize>>::Error: std::fmt::Debug,
{
    let mut value_buffer = Vec::new();
    let mut runs_buffer = Vec::new();
    let mut previous = T::default();
    let mut runs = 0usize;
    for (i, &value) in values.iter().enumerate() {
        if previous != value && i != 0 {
            value_buffer.push(previous);
            runs_buffer.push(T::try_from(runs).expect("run length fits"));
            runs = 0;
        }
        runs += 1;
        previous = value;
    }
    value_buffer.push(values[values.len() - 1]);
    runs_buffer.push(T::try_from(runs).expect("run length fits"));
    (runs_buffer, value_buffer)
}

/// Port of `ByteRleEncoder` (the ORC byte run-length encoding).
pub fn encode_byte_rle(values: &[u8]) -> Vec<u8> {
    const MIN_REPEAT_SIZE: usize = 3;
    const MAX_LITERAL_SIZE: usize = 128;
    const MAX_REPEAT_SIZE: usize = 127 + MIN_REPEAT_SIZE;

    struct Encoder {
        output: Vec<u8>,
        literals: [u8; MAX_LITERAL_SIZE],
        num_literals: usize,
        repeat: bool,
        tail_run_length: usize,
    }

    impl Encoder {
        fn write_values(&mut self) {
            if self.num_literals != 0 {
                if self.repeat {
                    self.output.push((self.num_literals - MIN_REPEAT_SIZE) as u8);
                    self.output.push(self.literals[0]);
                } else {
                    self.output.push((-(self.num_literals as i32)) as u8);
                    self.output.extend_from_slice(&self.literals[..self.num_literals]);
                }
                self.repeat = false;
                self.tail_run_length = 0;
                self.num_literals = 0;
            }
        }

        fn write(&mut self, value: u8) {
            if self.num_literals == 0 {
                self.literals[0] = value;
                self.num_literals = 1;
                self.tail_run_length = 1;
            } else if self.repeat {
                if value == self.literals[0] {
                    self.num_literals += 1;
                    if self.num_literals == MAX_REPEAT_SIZE {
                        self.write_values();
                    }
                } else {
                    self.write_values();
                    self.literals[self.num_literals] = value;
                    self.num_literals += 1;
                    self.tail_run_length = 1;
                }
            } else {
                if value == self.literals[self.num_literals - 1] {
                    self.tail_run_length += 1;
                } else {
                    self.tail_run_length = 1;
                }
                if self.tail_run_length == MIN_REPEAT_SIZE {
                    if self.num_literals + 1 == MIN_REPEAT_SIZE {
                        self.repeat = true;
                        self.num_literals += 1;
                    } else {
                        self.num_literals -= MIN_REPEAT_SIZE - 1;
                        self.write_values();
                        self.literals[0] = value;
                        self.repeat = true;
                        self.num_literals = MIN_REPEAT_SIZE;
                    }
                } else {
                    self.literals[self.num_literals] = value;
                    self.num_literals += 1;
                    if self.num_literals == MAX_LITERAL_SIZE {
                        self.write_values();
                    }
                }
            }
        }
    }

    let mut encoder = Encoder {
        output: Vec::with_capacity(values.len() / 4 + 2),
        literals: [0; MAX_LITERAL_SIZE],
        num_literals: 0,
        repeat: false,
        tail_run_length: 0,
    };
    for &v in values {
        encoder.write(v);
    }
    encoder.write_values();
    encoder.output
}

/// Inverse of [`encode_byte_rle`]: decodes `num_bytes` bytes starting at `offset`, consuming `byte_length` input
/// bytes.
pub fn decode_byte_rle(data: &[u8], num_bytes: usize) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(num_bytes);
    let mut pos = 0;
    while out.len() < num_bytes {
        let control = *data.get(pos).ok_or_else(|| anyhow::anyhow!("truncated byte rle"))?;
        pos += 1;
        if control < 0x80 {
            let value = *data.get(pos).ok_or_else(|| anyhow::anyhow!("truncated byte rle"))?;
            pos += 1;
            for _ in 0..(control as usize + 3) {
                out.push(value);
            }
        } else {
            let n = 256 - control as usize;
            let slice = data.get(pos..pos + n).ok_or_else(|| anyhow::anyhow!("truncated byte rle"))?;
            out.extend_from_slice(slice);
            pos += n;
        }
    }
    out.truncate(num_bytes);
    Ok(out)
}

/// `EncodingUtils.encodeBooleanRle`: packs `bits` into `ceil(len/8)` bytes (bit `i` is bit `i % 8` of byte `i / 8`,
/// like `BitSet.toByteArray`) and byte-RLE encodes them.
pub fn encode_boolean_rle(bits: &[bool]) -> Vec<u8> {
    let mut bytes = vec![0u8; bits.len().div_ceil(8)];
    for (i, &b) in bits.iter().enumerate() {
        if b {
            bytes[i / 8] |= 1 << (i % 8);
        }
    }
    encode_byte_rle(&bytes)
}

/// Inverse of [`encode_boolean_rle`].
pub fn decode_boolean_rle(data: &[u8], num_values: usize) -> anyhow::Result<Vec<bool>> {
    let bytes = decode_byte_rle(data, num_values.div_ceil(8))?;
    Ok((0..num_values).map(|i| bytes[i / 8] & (1 << (i % 8)) != 0).collect())
}

/// `BooleanEncoder.encodeBooleanStream`: a boolean RLE stream with its metadata header.
pub fn encode_boolean_stream(out: &mut Vec<u8>, values: &[bool], stream_type: PhysicalStreamType) {
    let encoded = encode_boolean_rle(values);
    StreamMetadata::new(
        stream_type,
        None,
        LogicalLevelTechnique::Rle,
        LogicalLevelTechnique::None,
        PhysicalLevelTechnique::None,
        values.len() as u32,
        encoded.len() as u32,
    )
    .encode_into(out);
    out.extend_from_slice(&encoded);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for v in [0u32, 1, 127, 128, 300, 16384, u32::MAX, (-1i32) as u32] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            assert_eq!(varint_size(v), out.len());
            let mut off = 0;
            assert_eq!(v, decode_varint(&out, &mut off).unwrap());
        }
        let mut out = Vec::new();
        put_varint(&mut out, (-1i32) as u32);
        assert_eq!(5, out.len());
    }

    #[test]
    fn zigzag_matches_java() {
        assert_eq!(0, zigzag(0));
        assert_eq!(1, zigzag(-1));
        assert_eq!(2, zigzag(1));
        assert_eq!(u32::MAX, zigzag(i32::MIN));
        for v in [0, 1, -1, 12345, -12345, i32::MAX, i32::MIN] {
            assert_eq!(v, unzigzag(zigzag(v)));
        }
        for v in [0, 1, -1, i64::MAX, i64::MIN] {
            assert_eq!(v, unzigzag_long(zigzag_long(v)));
        }
    }

    #[test]
    fn rle() {
        let (runs, values) = encode_rle(&[1i32, 1, 1, 2, 3, 3]);
        assert_eq!(vec![3, 1, 2], runs);
        assert_eq!(vec![1, 2, 3], values);
    }

    #[test]
    fn byte_rle_roundtrip() {
        let mut input = Vec::new();
        for i in 0..1000u32 {
            input.push(if i % 7 < 3 { 5 } else { (i * 31 % 251) as u8 });
        }
        input.extend(std::iter::repeat_n(9u8, 300));
        for len in [0, 1, 2, 3, 4, 10, 129, 130, 131, input.len()] {
            let encoded = encode_byte_rle(&input[..len]);
            assert_eq!(&input[..len], decode_byte_rle(&encoded, len).unwrap().as_slice(), "{len}");
        }
        // ORC examples
        assert_eq!(vec![0x61, 0x00], encode_byte_rle(&[0; 100]));
        assert_eq!(vec![0xfe, 0x44, 0x45], encode_byte_rle(&[0x44, 0x45]));
    }

    #[test]
    fn boolean_rle_roundtrip() {
        let bits: Vec<bool> = (0..77).map(|i| i % 3 == 0).collect();
        let encoded = encode_boolean_rle(&bits);
        assert_eq!(bits, decode_boolean_rle(&encoded, bits.len()).unwrap());
    }
}
