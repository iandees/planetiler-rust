//! Port of `reader/osm/PbfDecoder.java` (adapted from Osmosis): converts PBF block data into decoded entities.
//!
//! [`decode`] decompresses an `OSMData` blob and returns an iterator ([`PbfElements`]) that lazily builds
//! [`OsmElement`]s so that callers control parallelism (each worker thread decodes its own blocks).
//!
//! Supports raw, zlib, lz4 (like Java) and zstd compressed blobs.
//!
//! Unlike Java (and the prost-generated structs in [`crate::proto::osmpbf`]), blocks are parsed with a hand-written,
//! low-allocation protobuf reader: the decompressed block is kept in a per-thread reusable buffer, the string table is
//! a list of byte ranges into it (each string is only turned into an `Arc<str>` the first time a value/role/user
//! refers to it), packed arrays such as dense node ids/coordinates/tags are decoded on the fly while iterating, and
//! the only allocations per element are the ones needed for the returned [`OsmElement`]. The structure of the whole
//! block is validated up front by [`decode`] so iteration never encounters malformed messages.

use std::cell::RefCell;
use std::sync::{Arc, LazyLock};

use anyhow::{Context, bail};
use chrono::DateTime;
use prost::Message;

use crate::geo::envelope::Envelope;
use crate::proto::osmpbf;
use crate::reader::osm::osm_element::{Info, Member, Node, OsmElement, OsmElementType, Relation, Way};
use crate::reader::osm::osm_header::OsmHeader;
use crate::reader::osm::pbf_field_decoder::PbfFieldDecoder;
use crate::value::{Tags, Value};

// ---------------------------------------------------------------------------------------------------------------------
// protobuf wire format

/// Reads a varint from `buf` at `pos`, advancing `pos`. Returns `None` if the varint is truncated or too long.
#[inline(always)]
fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let p = *pos;
    let b0 = *buf.get(p)?;
    if b0 < 0x80 {
        *pos = p + 1;
        return Some(b0 as u64);
    }
    if let Some(bytes) = buf.get(p..p + 8) {
        // decode up to 8 bytes at once: find the first byte without a continuation bit, then squeeze out the
        // continuation bits
        let x = u64::from_le_bytes(bytes.try_into().expect("8 bytes"));
        let stops = !x & 0x8080_8080_8080_8080;
        if stops != 0 {
            let bits = stops.trailing_zeros() + 1; // = 8 * length
            let x = if bits >= 64 { x } else { x & ((1u64 << bits) - 1) };
            let x = (x & 0x007f_007f_007f_007f) | ((x & 0x7f00_7f00_7f00_7f00) >> 1);
            let x = (x & 0x0000_3fff_0000_3fff) | ((x & 0x3fff_0000_3fff_0000) >> 2);
            let x = (x & 0x0000_0000_0fff_ffff) | ((x & 0x0fff_ffff_0000_0000) >> 4);
            *pos = p + (bits as usize) / 8;
            return Some(x);
        }
    }
    read_varint_slow(buf, pos)
}

#[inline(never)]
fn read_varint_slow(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result = 0u64;
    let mut p = *pos;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(p)?;
        p += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b < 0x80 {
            *pos = p;
            return Some(result);
        }
    }
    None
}

#[inline(always)]
fn zigzag64(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

#[inline(always)]
fn zigzag32(v: u64) -> i32 {
    let v = v as u32;
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

/// The value of a protobuf field. Length-delimited ranges are relative to the message being parsed.
#[derive(Clone, Copy, Debug)]
enum Wire {
    Varint(u64),
    Bytes(usize, usize),
    Fixed,
}

/// Reads the next field of the message in `buf` at `pos`, or `None` at the end of the message.
#[inline(always)]
fn try_next_field(buf: &[u8], pos: &mut usize) -> Result<Option<(u32, Wire)>, &'static str> {
    if *pos >= buf.len() {
        return Ok(None);
    }
    let key = read_varint(buf, pos).ok_or("truncated field key")?;
    let field = (key >> 3) as u32;
    if field == 0 {
        return Err("invalid field number 0");
    }
    let wire = match key & 7 {
        0 => Wire::Varint(read_varint(buf, pos).ok_or("truncated varint")?),
        1 => {
            *pos += 8;
            Wire::Fixed
        }
        2 => {
            let len = read_varint(buf, pos).ok_or("truncated length")? as usize;
            let start = *pos;
            *pos = start.checked_add(len).ok_or("length overflow")?;
            Wire::Bytes(start, *pos)
        }
        5 => {
            *pos += 4;
            Wire::Fixed
        }
        _ => return Err("unsupported wire type"),
    };
    if *pos > buf.len() {
        return Err("field extends past the end of the message");
    }
    Ok(Some((field, wire)))
}

/// [`try_next_field`] with an `anyhow` error.
#[inline]
fn next_field(buf: &[u8], pos: &mut usize) -> anyhow::Result<Option<(u32, Wire)>> {
    try_next_field(buf, pos).map_err(|e| anyhow::anyhow!("{e} at byte {pos} of {}", buf.len()))
}

/// Calls `f` for each field of the message in `buf`.
#[inline]
fn for_each_field(buf: &[u8], mut f: impl FnMut(u32, Wire) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let mut pos = 0;
    while let Some((field, wire)) = next_field(buf, &mut pos)? {
        f(field, wire)?;
    }
    Ok(())
}

/// Same as [`for_each_field`] for messages already checked by [`decode`]'s validation pass (so cannot fail).
#[inline]
fn for_each_valid_field(buf: &[u8], mut f: impl FnMut(u32, Wire)) {
    let mut pos = 0;
    while let Ok(Some((field, wire))) = try_next_field(buf, &mut pos) {
        f(field, wire);
    }
}

/// Calls `f` for each varint of a repeated scalar field, which may be packed (`Bytes`) or not (`Varint`).
#[inline]
fn for_each_varint(buf: &[u8], wire: Wire, mut f: impl FnMut(u64)) {
    match wire {
        Wire::Varint(v) => f(v),
        Wire::Bytes(start, end) => {
            let packed = &buf[start..end];
            let mut pos = 0;
            while let Some(v) = read_varint(packed, &mut pos) {
                f(v);
            }
        }
        Wire::Fixed => {}
    }
}

/// Number of varints in a packed array.
#[inline]
fn count_varints(packed: &[u8]) -> usize {
    packed.iter().filter(|&&b| b < 0x80).count()
}

/// A cursor over a packed array of varints in some buffer.
#[derive(Clone, Copy, Default)]
struct Packed {
    pos: usize,
    end: usize,
}

impl Packed {
    fn of(offset: usize, wire: Wire) -> Packed {
        match wire {
            Wire::Bytes(start, end) => Packed { pos: offset + start, end: offset + end },
            _ => Packed::default(),
        }
    }

    #[inline(always)]
    fn next(&mut self, src: &[u8]) -> Option<u64> {
        if self.pos >= self.end {
            return None;
        }
        read_varint(&src[..self.end], &mut self.pos)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// blobs

enum BlobData<'a> {
    Raw(&'a [u8]),
    Zlib(&'a [u8]),
    Lz4(&'a [u8]),
    Zstd(&'a [u8]),
    Unsupported,
}

/// Parses a `Blob` message without copying its data.
fn parse_blob(raw_blob: &[u8]) -> anyhow::Result<(usize, BlobData<'_>)> {
    let mut raw_size = 0usize;
    let mut data = None;
    for_each_field(raw_blob, |field, wire| {
        match (field, wire) {
            (2, Wire::Varint(v)) => raw_size = (v as i32).max(0) as usize,
            (1 | 3 | 6 | 7, Wire::Bytes(start, end)) => {
                let bytes = &raw_blob[start..end];
                data = Some(match field {
                    1 => BlobData::Raw(bytes),
                    3 => BlobData::Zlib(bytes),
                    6 => BlobData::Lz4(bytes),
                    _ => BlobData::Zstd(bytes),
                });
            }
            (4 | 5, _) => data = Some(BlobData::Unsupported),
            (1..=7, _) => bail!("invalid wire type for blob field {field}"),
            _ => {}
        }
        Ok(())
    })
    .context("Unable to parse PBF blob")?;
    Ok((raw_size, data.unwrap_or(BlobData::Unsupported)))
}

thread_local! {
    static INFLATER: RefCell<Option<flate2::Decompress>> = const { RefCell::new(None) };
}

thread_local! {
    static LIBDEFLATE: RefCell<Option<libdeflater::Decompressor>> = const { RefCell::new(None) };
}

/// Sets the length of `out` to `len` for it to be completely overwritten. Avoids re-zeroing the part of the (reused)
/// buffer that is already initialized.
fn resize_for_overwrite(out: &mut Vec<u8>, len: usize) {
    if out.len() < len {
        out.resize(len, 0);
    } else {
        out.truncate(len);
    }
}

fn inflate_zlib(data: &[u8], raw_size: usize, out: &mut Vec<u8>) -> anyhow::Result<()> {
    if raw_size > 0 {
        // libdeflate decompresses a whole buffer at once when the output size is known (it always is for PBF blobs),
        // which is ~1.5x faster than zlib-rs through flate2's streaming API
        let result = LIBDEFLATE.with_borrow_mut(|d| {
            let d = d.get_or_insert_with(libdeflater::Decompressor::new);
            resize_for_overwrite(out, raw_size);
            d.zlib_decompress(data, out)
        });
        if let Ok(n) = result
            && n == raw_size
        {
            return Ok(());
        }
        // otherwise fall through to the streaming decoder for a precise error or when raw_size is wrong
    }
    inflate_zlib_streaming(data, raw_size, out)
}

fn inflate_zlib_streaming(data: &[u8], raw_size: usize, out: &mut Vec<u8>) -> anyhow::Result<()> {
    out.clear();
    INFLATER.with_borrow_mut(|inflater| {
        let inflater = match inflater {
            Some(inflater) => {
                inflater.reset(true);
                inflater
            }
            None => inflater.insert(flate2::Decompress::new(true)),
        };
        out.reserve(raw_size.max(data.len() * 2));
        loop {
            let consumed = inflater.total_in() as usize;
            let status = inflater
                .decompress_vec(&data[consumed..], out, flate2::FlushDecompress::Finish)
                .context("Unable to decompress PBF blob.")?;
            match status {
                flate2::Status::StreamEnd => return Ok(()),
                _ if out.len() == out.capacity() => out.reserve(out.capacity()),
                _ => bail!("PBF blob contains incomplete compressed data."),
            }
        }
    })
}

/// Decompresses the content of a raw `Blob` message into `out`, replacing its contents.
fn decompress_blob(raw_blob: &[u8], out: &mut Vec<u8>) -> anyhow::Result<()> {
    let (raw_size, data) = parse_blob(raw_blob)?;
    match data {
        BlobData::Raw(data) => {
            out.clear();
            out.extend_from_slice(data);
        }
        BlobData::Zlib(data) => inflate_zlib(data, raw_size, out)?,
        BlobData::Lz4(data) => {
            resize_for_overwrite(out, raw_size);
            let read = lz4_flex::block::decompress_into(data, out).context("Unable to decompress PBF blob.")?;
            if read != raw_size {
                bail!("Unable to decompress PBF blob. got {read} bytes but expected {raw_size}");
            }
        }
        BlobData::Zstd(data) => {
            out.clear();
            out.reserve(raw_size);
            zstd::stream::copy_decode(data, &mut *out).context("Unable to decompress PBF blob.")?;
        }
        BlobData::Unsupported => {
            bail!("PBF blob uses unsupported compression, only lz4, zstd, zlib, or raw may be used.")
        }
    }
    Ok(())
}

/// Reusable per-thread decoding buffers.
#[derive(Default)]
struct Buffers {
    /// The decompressed `PrimitiveBlock`.
    data: Vec<u8>,
    /// String table entries as `(start, end)` ranges of `data`; all are valid UTF-8 unless already in `cache`.
    strings: Vec<(u32, u32)>,
    /// Lazily-created shared strings for each string table entry.
    cache: Vec<Option<Arc<str>>>,
    /// `(start, end)` ranges of `data` for each `PrimitiveGroup`.
    groups: Vec<(usize, usize)>,
    /// Re-encoded dense nodes for groups that need the slow path (see [`PbfElements::start_dense`]).
    dense_owned: Vec<u8>,
    keys: Vec<u32>,
    vals: Vec<u32>,
    roles: Vec<i32>,
    types: Vec<i32>,
    memids: Vec<i64>,
}

thread_local! {
    static BUFFER_POOL: RefCell<Vec<Buffers>> = const { RefCell::new(Vec::new()) };
}

fn take_buffers() -> Buffers {
    BUFFER_POOL.with_borrow_mut(|pool| pool.pop()).unwrap_or_default()
}

fn return_buffers(mut buffers: Buffers) {
    buffers.cache.clear();
    // don't hold on to huge buffers from unusual blocks forever
    if buffers.data.capacity() <= 64 << 20 {
        let _ = BUFFER_POOL.try_with(|pool| {
            let mut pool = pool.borrow_mut();
            if pool.len() < 4 {
                pool.push(buffers);
            }
        });
    }
}

/// Decompresses the content of a raw `Blob` message and passes the uncompressed bytes to `f`.
pub fn with_blob_content<R>(raw_blob: &[u8], f: impl FnOnce(&[u8]) -> anyhow::Result<R>) -> anyhow::Result<R> {
    let mut buffers = take_buffers();
    let result = decompress_blob(raw_blob, &mut buffers.data).and_then(|()| f(&buffers.data));
    return_buffers(buffers);
    result
}

// ---------------------------------------------------------------------------------------------------------------------
// blocks

/// Decompresses and parses a block of primitive OSM elements.
pub fn decode(raw_blob: &[u8]) -> anyhow::Result<PbfElements> {
    let mut buffers = take_buffers();
    let params = decompress_blob(raw_blob, &mut buffers.data).and_then(|()| parse_block(&mut buffers));
    let params = match params {
        Ok(params) => params,
        Err(e) => {
            return_buffers(buffers);
            return Err(e);
        }
    };
    let (granularity, lat_offset, lon_offset, date_granularity) = params;
    let decoder = PbfFieldDecoder::with_params(granularity, lat_offset, lon_offset, date_granularity);
    Ok(PbfElements {
        default_granularity: decoder.is_default_granularity(),
        decoder,
        buffers,
        group: 0,
        phase: Phase::Dense,
        cursor: 0,
        dense: None,
    })
}

/// Parses the top level of the `PrimitiveBlock` in `buffers.data`, filling in the string table and group ranges, and
/// validates the structure of every group. Returns `(granularity, lat_offset, lon_offset, date_granularity)`.
fn parse_block(buffers: &mut Buffers) -> anyhow::Result<(i64, i64, i64, i64)> {
    let Buffers { data, strings, cache, groups, .. } = buffers;
    let data: &[u8] = data;
    strings.clear();
    groups.clear();
    cache.clear();
    if data.len() > u32::MAX as usize {
        bail!("PBF block too large: {} bytes", data.len());
    }
    let (mut granularity, mut lat_offset, mut lon_offset, mut date_granularity) = (100i64, 0i64, 0i64, 1000i64);
    for_each_field(data, |field, wire| {
        match (field, wire) {
            (1, Wire::Bytes(start, end)) => for_each_field(&data[start..end], |field, wire| {
                if let (1, Wire::Bytes(s, e)) = (field, wire) {
                    strings.push(((start + s) as u32, (start + e) as u32));
                }
                Ok(())
            })?,
            (2, Wire::Bytes(start, end)) => {
                validate_group(&data[start..end]).context("Unable to parse primitive group")?;
                groups.push((start, end));
            }
            (17, Wire::Varint(v)) => granularity = v as i32 as i64,
            (18, Wire::Varint(v)) => date_granularity = v as i32 as i64,
            (19, Wire::Varint(v)) => lat_offset = v as i64,
            (20, Wire::Varint(v)) => lon_offset = v as i64,
            (1 | 2 | 17..=20, _) => bail!("invalid wire type for primitive block field {field}"),
            _ => {}
        }
        Ok(())
    })
    .context("Unable to parse primitive block")?;
    cache.resize(strings.len(), None);
    for (i, &(start, end)) in strings.iter().enumerate() {
        let bytes = &data[start as usize..end as usize];
        if std::str::from_utf8(bytes).is_err() {
            cache[i] = Some(Arc::from(String::from_utf8_lossy(bytes)));
        }
    }
    Ok((granularity, lat_offset, lon_offset, date_granularity))
}

/// Checks that every message in a `PrimitiveGroup` can be parsed so iteration never hits malformed data.
fn validate_group(group: &[u8]) -> anyhow::Result<()> {
    fn validate_with_info(msg: &[u8], info_field: u32) -> anyhow::Result<()> {
        for_each_field(msg, |field, wire| {
            if field == info_field
                && let Wire::Bytes(start, end) = wire
            {
                for_each_field(&msg[start..end], |_, _| Ok(()))?;
            }
            Ok(())
        })
    }
    for_each_field(group, |field, wire| {
        match (field, wire) {
            (1 | 3 | 4, Wire::Bytes(start, end)) => validate_with_info(&group[start..end], 4)?,
            (2, Wire::Bytes(start, end)) => validate_with_info(&group[start..end], 5)?,
            (1..=4, _) => bail!("invalid wire type for primitive group field {field}"),
            _ => {}
        }
        Ok(())
    })
}

/// Decompresses and parses a header block of an OSM input file.
pub fn decode_header(raw_blob: &[u8]) -> anyhow::Result<OsmHeader> {
    let header =
        with_blob_content(raw_blob, |data| osmpbf::HeaderBlock::decode(data).context("Unable to decode PBF header"))?;
    let bbox = header.bbox.unwrap_or_default();
    Ok(OsmHeader {
        bounds: Envelope::new(
            bbox.left as f64 / 1e9,
            bbox.right as f64 / 1e9,
            bbox.bottom as f64 / 1e9,
            bbox.top as f64 / 1e9,
        ),
        writingprogram: header.writingprogram().to_string(),
        source: header.source().to_string(),
        instant: DateTime::from_timestamp(header.osmosis_replication_timestamp(), 0).unwrap_or_default(),
        osmosis_replication_sequence_number: header.osmosis_replication_sequence_number(),
        osmosis_replication_base_url: header.osmosis_replication_base_url().to_string(),
        required_features: header.required_features,
        optional_features: header.optional_features,
    })
}

// ---------------------------------------------------------------------------------------------------------------------
// elements

static EMPTY: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from(""));

/// Access to the string table of a block.
struct Strings<'a> {
    data: &'a [u8],
    ranges: &'a [(u32, u32)],
    cache: &'a mut [Option<Arc<str>>],
}

impl Strings<'_> {
    /// The shared string at `index` (empty string when out of range).
    #[inline]
    fn get(&mut self, index: usize) -> Arc<str> {
        if let Some(Some(s)) = self.cache.get(index) {
            return s.clone();
        }
        if index >= self.ranges.len() {
            return EMPTY.clone();
        }
        let s: Arc<str> = Arc::from(self.str(index));
        self.cache[index] = Some(s.clone());
        s
    }

    /// The string at `index` as a `&str` (empty string when out of range).
    #[inline]
    fn str(&self, index: usize) -> &str {
        match (self.cache.get(index), self.ranges.get(index)) {
            (Some(Some(s)), _) => s,
            (_, Some(&(start, end))) => {
                let bytes = &self.data[start as usize..end as usize];
                // SAFETY: parse_block validated every string table entry that isn't in `cache` as UTF-8
                unsafe { std::str::from_utf8_unchecked(bytes) }
            }
            _ => "",
        }
    }

    #[inline]
    fn insert_tag(&mut self, tags: &mut Tags, key: usize, value: usize) {
        tags.insert(String::from(self.str(key)), Value::String(self.get(value)));
    }

    fn tags(&mut self, keys: &[u32], vals: &[u32]) -> Tags {
        let mut tags = crate::value::tags_with_capacity(keys.len().min(vals.len()));
        for (&k, &v) in keys.iter().zip(vals) {
            self.insert_tag(&mut tags, k as usize, v as usize);
        }
        tags
    }

    /// Parses an `Info` message (Java's protobuf `getInfo()` returns a default instance when missing, so info is
    /// always present).
    fn info(&mut self, msg: Option<&[u8]>) -> Option<Info> {
        let (mut version, mut timestamp, mut changeset, mut uid, mut user_sid) = (-1i32, 0i64, 0i64, 0i32, 0u32);
        if let Some(msg) = msg {
            for_each_valid_field(msg, |field, wire| {
                if let Wire::Varint(v) = wire {
                    match field {
                        1 => version = v as i32,
                        2 => timestamp = v as i64,
                        3 => changeset = v as i64,
                        4 => uid = v as i32,
                        5 => user_sid = v as u32,
                        _ => {}
                    }
                }
            });
        }
        Some(Info { changeset, timestamp, user_id: uid, version, user: self.get(user_sid as usize) })
    }
}

/// Iteration state for the dense nodes of a group: cursors into each packed array plus the delta-decoding state.
struct DenseState {
    /// True to read from `Buffers::dense_owned` instead of `Buffers::data`.
    owned: bool,
    ids: Packed,
    lats: Packed,
    lons: Packed,
    keys_vals: Packed,
    versions: Packed,
    timestamps: Packed,
    changesets: Packed,
    uids: Packed,
    user_sids: Packed,
    node_id: i64,
    latitude: i64,
    longitude: i64,
    timestamp: i64,
    changeset: i64,
    uid: i32,
    user_sid: i32,
}

impl DenseState {
    /// Parses a `DenseNodes` message at `offset..offset + msg.len()` of its source buffer. Returns `None` if it uses
    /// an encoding the fast path does not handle (repeated or unpacked occurrences of a packed field).
    fn parse(msg: &[u8], offset: usize, owned: bool) -> Option<DenseState> {
        let mut state = DenseState {
            owned,
            ids: Packed::default(),
            lats: Packed::default(),
            lons: Packed::default(),
            keys_vals: Packed::default(),
            versions: Packed::default(),
            timestamps: Packed::default(),
            changesets: Packed::default(),
            uids: Packed::default(),
            user_sids: Packed::default(),
            node_id: 0,
            latitude: 0,
            longitude: 0,
            timestamp: 0,
            changeset: 0,
            uid: 0,
            user_sid: 0,
        };
        let mut seen = 0u32;
        let mut regular = true;
        fn set(target: &mut Packed, offset: usize, wire: Wire, bit: u32, seen: &mut u32, regular: &mut bool) {
            if *seen & bit != 0 || !matches!(wire, Wire::Bytes(..)) {
                *regular = false;
            }
            *seen |= bit;
            *target = Packed::of(offset, wire);
        }
        for_each_valid_field(msg, |field, wire| match field {
            1 => set(&mut state.ids, offset, wire, 1 << 1, &mut seen, &mut regular),
            8 => set(&mut state.lats, offset, wire, 1 << 8, &mut seen, &mut regular),
            9 => set(&mut state.lons, offset, wire, 1 << 9, &mut seen, &mut regular),
            10 => set(&mut state.keys_vals, offset, wire, 1 << 10, &mut seen, &mut regular),
            5 => {
                if seen & (1 << 5) != 0 {
                    regular = false;
                }
                seen |= 1 << 5;
                if let Wire::Bytes(start, end) = wire {
                    let info_offset = offset + start;
                    let mut info_seen = 0u32;
                    for_each_valid_field(&msg[start..end], |field, wire| {
                        let target = match field {
                            1 => &mut state.versions,
                            2 => &mut state.timestamps,
                            3 => &mut state.changesets,
                            4 => &mut state.uids,
                            5 => &mut state.user_sids,
                            _ => return,
                        };
                        set(target, info_offset, wire, 1 << field, &mut info_seen, &mut regular);
                    });
                }
            }
            _ => {}
        });
        regular.then_some(state)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Dense,
    Nodes,
    Ways,
    Relations,
}

impl Phase {
    fn field(self) -> u32 {
        match self {
            Phase::Nodes => 1,
            Phase::Dense => 2,
            Phase::Ways => 3,
            Phase::Relations => 4,
        }
    }
}

/// Iterator over the elements of a decoded block: dense nodes, nodes, ways, then relations of each primitive group.
pub struct PbfElements {
    decoder: PbfFieldDecoder,
    default_granularity: bool,
    buffers: Buffers,
    /// Index of the current group in `buffers.groups`.
    group: usize,
    phase: Phase,
    /// Position of the next field to read in the current group for the current phase (relative to the group).
    cursor: usize,
    dense: Option<DenseState>,
}

impl Drop for PbfElements {
    fn drop(&mut self) {
        return_buffers(std::mem::take(&mut self.buffers));
    }
}

impl PbfElements {
    /// Sets up dense node iteration for the current group. Uses cursors directly into the block when the
    /// `DenseNodes` message is encoded the usual way (one packed occurrence of each field), otherwise lets prost merge
    /// all occurrences and re-encodes them into `dense_owned`.
    fn start_dense(&mut self, group: &[u8], start: usize, end: usize, occurrences: usize) -> Option<DenseState> {
        if occurrences == 1
            && let Some(state) = DenseState::parse(&group[start..end], start + self.buffers.groups[self.group].0, false)
        {
            return Some(state);
        }
        let dense = osmpbf::PrimitiveGroup::decode(group).ok()?.dense?;
        self.buffers.dense_owned = dense.encode_to_vec();
        DenseState::parse(&self.buffers.dense_owned, 0, true)
    }

    fn next_dense(&mut self) -> Option<OsmElement> {
        let state = self.dense.as_mut()?;
        let buffers = &mut self.buffers;
        let src: &[u8] = if state.owned { &buffers.dense_owned } else { &buffers.data };
        let mut strings = Strings { data: &buffers.data, ranges: &buffers.strings, cache: &mut buffers.cache };

        state.node_id += zigzag64(state.ids.next(src)?);
        state.latitude += state.lats.next(src).map_or(0, zigzag64);
        state.longitude += state.lons.next(src).map_or(0, zigzag64);
        let version = state.versions.next(src).map_or(0, |v| v as i32);
        state.timestamp += state.timestamps.next(src).map_or(0, zigzag64);
        state.changeset += state.changesets.next(src).map_or(0, zigzag64);
        state.uid = state.uid.wrapping_add(state.uids.next(src).map_or(0, zigzag32));
        state.user_sid = state.user_sid.wrapping_add(state.user_sids.next(src).map_or(0, zigzag32));

        // The key and value string indexes are sequential in the same PBF array. Each set of tags is delimited by an
        // index with a value of 0.
        let mut tags = Tags::default();
        if state.keys_vals.pos < state.keys_vals.end && src[state.keys_vals.pos] == 0 {
            // most nodes have no tags
            state.keys_vals.pos += 1;
        } else if state.keys_vals.pos < state.keys_vals.end {
            // count the tags first to size the map exactly
            let mut lookahead = state.keys_vals;
            let mut count = 0;
            while lookahead.next(src).is_some_and(|k| k as i32 != 0) {
                lookahead.next(src);
                count += 1;
            }
            tags.reserve(count);
            while let Some(key) = state.keys_vals.next(src) {
                let key_index = key as i32 as usize;
                if key_index == 0 {
                    break;
                }
                let value_index = state.keys_vals.next(src).map_or(0, |v| v as i32 as usize);
                strings.insert_tag(&mut tags, key_index, value_index);
            }
        }

        let (lat, lon) = if self.default_granularity {
            (state.latitude as f64 / 10000000.0, state.longitude as f64 / 10000000.0)
        } else {
            (self.decoder.decode_latitude(state.latitude), self.decoder.decode_longitude(state.longitude))
        };
        Some(OsmElement::Node(Node {
            id: state.node_id,
            tags,
            lat,
            lon,
            info: Some(Info {
                changeset: state.changeset,
                timestamp: state.timestamp,
                user_id: state.uid,
                version,
                user: strings.get(state.user_sid.max(0) as usize),
            }),
        }))
    }

    fn parse_node(&mut self, msg_start: usize, msg_end: usize) -> OsmElement {
        let decoder = &self.decoder;
        let Buffers { data, strings, cache, keys, vals, .. } = &mut self.buffers;
        let msg = &data[msg_start..msg_end];
        keys.clear();
        vals.clear();
        let (mut id, mut lat, mut lon, mut info) = (0i64, 0i64, 0i64, None);
        for_each_valid_field(msg, |field, wire| match (field, wire) {
            (1, Wire::Varint(v)) => id = zigzag64(v),
            (2, _) => for_each_varint(msg, wire, |v| keys.push(v as u32)),
            (3, _) => for_each_varint(msg, wire, |v| vals.push(v as u32)),
            (4, Wire::Bytes(s, e)) => info = Some(&msg[s..e]),
            (8, Wire::Varint(v)) => lat = zigzag64(v),
            (9, Wire::Varint(v)) => lon = zigzag64(v),
            _ => {}
        });
        let mut strings = Strings { data, ranges: strings, cache };
        OsmElement::Node(Node {
            id,
            tags: strings.tags(keys, vals),
            lat: decoder.decode_latitude(lat),
            lon: decoder.decode_longitude(lon),
            info: strings.info(info),
        })
    }

    fn parse_way(&mut self, msg_start: usize, msg_end: usize) -> OsmElement {
        let Buffers { data, strings, cache, keys, vals, .. } = &mut self.buffers;
        let msg = &data[msg_start..msg_end];
        keys.clear();
        vals.clear();
        let (mut id, mut info) = (0i64, None);
        let mut nodes: Vec<i64> = Vec::new();
        // node ids are delta encoded
        let mut node_id = 0i64;
        for_each_valid_field(msg, |field, wire| match (field, wire) {
            (1, Wire::Varint(v)) => id = v as i64,
            (2, _) => for_each_varint(msg, wire, |v| keys.push(v as u32)),
            (3, _) => for_each_varint(msg, wire, |v| vals.push(v as u32)),
            (4, Wire::Bytes(s, e)) => info = Some(&msg[s..e]),
            (8, _) => {
                if let Wire::Bytes(s, e) = wire {
                    nodes.reserve_exact(count_varints(&msg[s..e]));
                }
                for_each_varint(msg, wire, |v| {
                    node_id = node_id.wrapping_add(zigzag64(v));
                    nodes.push(node_id);
                });
            }
            _ => {}
        });
        let mut strings = Strings { data, ranges: strings, cache };
        OsmElement::Way(Way { id, tags: strings.tags(keys, vals), nodes, info: strings.info(info) })
    }

    fn parse_relation(&mut self, msg_start: usize, msg_end: usize) -> OsmElement {
        let Buffers { data, strings, cache, keys, vals, roles, types, memids, .. } = &mut self.buffers;
        let msg = &data[msg_start..msg_end];
        keys.clear();
        vals.clear();
        roles.clear();
        types.clear();
        memids.clear();
        let (mut id, mut info) = (0i64, None);
        let mut member_id = 0i64;
        for_each_valid_field(msg, |field, wire| match (field, wire) {
            (1, Wire::Varint(v)) => id = v as i64,
            (2, _) => for_each_varint(msg, wire, |v| keys.push(v as u32)),
            (3, _) => for_each_varint(msg, wire, |v| vals.push(v as u32)),
            (4, Wire::Bytes(s, e)) => info = Some(&msg[s..e]),
            (8, _) => for_each_varint(msg, wire, |v| roles.push(v as i32)),
            (9, _) => for_each_varint(msg, wire, |v| {
                member_id = member_id.wrapping_add(zigzag64(v));
                memids.push(member_id);
            }),
            (10, _) => for_each_varint(msg, wire, |v| types.push(v as i32)),
            _ => {}
        });
        let mut strings = Strings { data, ranges: strings, cache };
        let members = memids
            .iter()
            .enumerate()
            .map(|(j, &ref_id)| {
                let member_type = match types.get(j) {
                    Some(1) => OsmElementType::Way,
                    Some(2) => OsmElementType::Relation,
                    _ => OsmElementType::Node,
                };
                let role = roles.get(j).copied().unwrap_or(0).max(0) as usize;
                Member { member_type, ref_id, role: strings.get(role) }
            })
            .collect();
        OsmElement::Relation(Relation { id, tags: strings.tags(keys, vals), members, info: strings.info(info) })
    }
}

impl Iterator for PbfElements {
    type Item = OsmElement;

    fn next(&mut self) -> Option<OsmElement> {
        loop {
            if self.dense.is_some() {
                if let Some(node) = self.next_dense() {
                    return Some(node);
                }
                self.dense = None;
            }
            let &(group_start, group_end) = self.buffers.groups.get(self.group)?;
            let want = self.phase.field();
            let mut found = None;
            {
                let group = &self.buffers.data[group_start..group_end];
                let mut pos = self.cursor;
                while let Ok(Some((field, wire))) = try_next_field(group, &mut pos) {
                    if field == want
                        && let Wire::Bytes(start, end) = wire
                    {
                        found = Some((start, end));
                        break;
                    }
                }
                self.cursor = pos;
            }
            let Some((start, end)) = found else {
                self.cursor = 0;
                self.phase = match self.phase {
                    Phase::Dense => Phase::Nodes,
                    Phase::Nodes => Phase::Ways,
                    Phase::Ways => Phase::Relations,
                    Phase::Relations => {
                        self.group += 1;
                        Phase::Dense
                    }
                };
                continue;
            };
            let (msg_start, msg_end) = (group_start + start, group_start + end);
            match self.phase {
                Phase::Dense => {
                    // `dense` is an optional message field: protobuf merges repeated occurrences
                    let data = std::mem::take(&mut self.buffers.data);
                    let group = &data[group_start..group_end];
                    let mut occurrences = 0;
                    for_each_valid_field(group, |field, _| occurrences += (field == 2) as usize);
                    self.dense = self.start_dense(group, start, end, occurrences);
                    self.buffers.data = data;
                    self.phase = Phase::Nodes;
                    self.cursor = 0;
                }
                Phase::Nodes => return Some(self.parse_node(msg_start, msg_end)),
                Phase::Ways => return Some(self.parse_way(msg_start, msg_end)),
                Phase::Relations => return Some(self.parse_relation(msg_start, msg_end)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::osmpbf::{blob, relation::MemberType};
    use crate::tags;

    fn encode_blob(data: Vec<u8>, compression: &str) -> Vec<u8> {
        let raw_size = Some(data.len() as i32);
        let data = match compression {
            "raw" => blob::Data::Raw(data),
            "zlib" => {
                use std::io::Write;
                let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                enc.write_all(&data).unwrap();
                blob::Data::ZlibData(enc.finish().unwrap())
            }
            "lz4" => blob::Data::Lz4Data(lz4_flex::block::compress(&data)),
            "zstd" => blob::Data::ZstdData(zstd::bulk::compress(&data, 3).unwrap()),
            _ => unreachable!(),
        };
        osmpbf::Blob { raw_size, data: Some(data) }.encode_to_vec()
    }

    fn strings(s: &[&str]) -> osmpbf::StringTable {
        osmpbf::StringTable { s: s.iter().map(|s| s.as_bytes().to_vec()).collect() }
    }

    fn test_block() -> Vec<u8> {
        let group = osmpbf::PrimitiveGroup {
            dense: Some(osmpbf::DenseNodes {
                id: vec![1, 1],
                lat: vec![100_000_000, 10_000_000],
                lon: vec![200_000_000, -10_000_000],
                keys_vals: vec![1, 2, 0, 0],
                denseinfo: None,
            }),
            ways: vec![osmpbf::Way { id: 3, keys: vec![1], vals: vec![3], refs: vec![1, 1], ..Default::default() }],
            relations: vec![osmpbf::Relation {
                id: 4,
                keys: vec![1],
                vals: vec![2],
                memids: vec![3, -2],
                types: vec![MemberType::Way as i32, MemberType::Node as i32],
                roles_sid: vec![4, 0],
                ..Default::default()
            }],
            ..Default::default()
        };
        osmpbf::PrimitiveBlock {
            stringtable: strings(&["", "key", "value", "value2", "outer"]),
            primitivegroup: vec![group.encode_to_vec()],
            ..Default::default()
        }
        .encode_to_vec()
    }

    #[test]
    fn decode_all_compressions() {
        for compression in ["raw", "zlib", "lz4", "zstd"] {
            let elements: Vec<_> = decode(&encode_blob(test_block(), compression)).expect(compression).collect();
            assert_eq!(4, elements.len(), "{compression}");
            match &elements[0] {
                OsmElement::Node(n) => {
                    assert_eq!(1, n.id);
                    assert_eq!(10.0, n.lat);
                    assert_eq!(20.0, n.lon);
                    assert_eq!(tags! {"key" => "value"}, n.tags);
                }
                other => panic!("{other:?}"),
            }
            match &elements[1] {
                OsmElement::Node(n) => {
                    assert_eq!(2, n.id);
                    assert_eq!(11.0, n.lat);
                    assert_eq!(19.0, n.lon);
                    assert!(n.tags.is_empty());
                }
                other => panic!("{other:?}"),
            }
            match &elements[2] {
                OsmElement::Way(w) => {
                    assert_eq!(3, w.id);
                    assert_eq!(vec![1, 2], w.nodes);
                    assert_eq!(tags! {"key" => "value2"}, w.tags);
                }
                other => panic!("{other:?}"),
            }
            match &elements[3] {
                OsmElement::Relation(r) => {
                    assert_eq!(4, r.id);
                    assert_eq!(
                        vec![Member::new(OsmElementType::Way, 3, "outer"), Member::new(OsmElementType::Node, 1, "")],
                        r.members
                    );
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn decode_header_block() {
        let header = osmpbf::HeaderBlock {
            bbox: Some(osmpbf::HeaderBBox {
                left: 1_000_000_000,
                right: 2_000_000_000,
                top: 4_000_000_000,
                bottom: 3_000_000_000,
            }),
            required_features: vec!["OsmSchema-V0.6".into()],
            writingprogram: Some("test".into()),
            osmosis_replication_timestamp: Some(10),
            ..Default::default()
        };
        let parsed = decode_header(&encode_blob(header.encode_to_vec(), "zlib")).unwrap();
        assert_eq!(Envelope::new(1.0, 2.0, 3.0, 4.0), parsed.bounds);
        assert_eq!("test", parsed.writingprogram);
        assert_eq!(vec!["OsmSchema-V0.6".to_string()], parsed.required_features);
        assert_eq!(10, parsed.instant.timestamp());
    }

    #[test]
    fn invalid_blob() {
        assert!(decode(&[1, 2, 3, 4]).is_err());
        let blob = osmpbf::Blob { raw_size: Some(10), data: Some(blob::Data::LzmaData(vec![1])) }.encode_to_vec();
        assert!(decode(&blob).is_err());
        // truncated primitive group
        let mut block = test_block();
        block.truncate(block.len() - 3);
        assert!(decode(&encode_blob(block, "raw")).is_err());
    }

    #[test]
    fn test_read_varint() {
        let mut values = vec![0u64, 1, 127, 128, 255, 300, 16383, 16384, u32::MAX as u64, i64::MAX as u64, u64::MAX];
        for shift in 0..64 {
            values.push(1 << shift);
            values.push((1 << shift) - 1);
        }
        let mut buf = Vec::new();
        for &v in &values {
            prost::encoding::encode_varint(v, &mut buf);
        }
        // read with and without padding after the last value to cover the wide and the byte-at-a-time paths
        for padding in [0, 8] {
            let mut padded = buf.clone();
            padded.extend(std::iter::repeat_n(0xffu8, padding));
            let mut pos = 0;
            for &v in &values {
                assert_eq!(Some(v), read_varint(&padded, &mut pos));
            }
            assert_eq!(buf.len(), pos);
        }
        // truncated / too long
        assert_eq!(None, read_varint(&[0x80], &mut 0));
        assert_eq!(None, read_varint(&[0xff; 11], &mut 0));
        let mut buf = Vec::new();
        prost::encoding::encode_varint(u64::MAX, &mut buf);
        assert_eq!(None, read_varint(&buf[..9], &mut 0));
    }

    fn key(field: u32, wire_type: prost::encoding::WireType, buf: &mut Vec<u8>) {
        prost::encoding::encode_key(field, wire_type, buf);
    }

    fn len_field(field: u32, data: &[u8], buf: &mut Vec<u8>) {
        key(field, prost::encoding::WireType::LengthDelimited, buf);
        prost::encoding::encode_varint(data.len() as u64, buf);
        buf.extend_from_slice(data);
    }

    fn block_with_groups(groups: Vec<Vec<u8>>, granularity: Option<i32>) -> Vec<u8> {
        osmpbf::PrimitiveBlock {
            stringtable: osmpbf::StringTable {
                s: vec![b"".to_vec(), b"key".to_vec(), b"value".to_vec(), b"bad\xffutf8".to_vec(), b"user".to_vec()],
            },
            primitivegroup: groups,
            granularity,
            ..Default::default()
        }
        .encode_to_vec()
    }

    #[test]
    fn test_split_and_unpacked_encodings() {
        let dense1 = osmpbf::DenseNodes {
            id: vec![1, 1],
            lat: vec![10, 10],
            lon: vec![20, 20],
            keys_vals: vec![1, 2, 0, 0],
            denseinfo: Some(osmpbf::DenseInfo { version: vec![1, 2], user_sid: vec![4, -4], ..Default::default() }),
        };
        let dense2 = osmpbf::DenseNodes {
            id: vec![1],
            lat: vec![10],
            lon: vec![20],
            keys_vals: vec![1, 3, 0],
            denseinfo: Some(osmpbf::DenseInfo { version: vec![3], user_sid: vec![4], ..Default::default() }),
        };
        // dense appears twice in the group (protobuf merges them)
        let mut group = Vec::new();
        len_field(2, &dense1.encode_to_vec(), &mut group);
        len_field(2, &dense2.encode_to_vec(), &mut group);
        // a way with unpacked keys/vals and refs split over packed + unpacked occurrences
        let mut way = Vec::new();
        key(1, prost::encoding::WireType::Varint, &mut way);
        prost::encoding::encode_varint(7, &mut way);
        for (field, value) in [(2u32, 1u64), (3, 3)] {
            key(field, prost::encoding::WireType::Varint, &mut way);
            prost::encoding::encode_varint(value, &mut way);
        }
        let mut refs = Vec::new();
        prost::encoding::encode_varint(10, &mut refs); // zigzag(5)
        prost::encoding::encode_varint(2, &mut refs); // zigzag(1)
        len_field(8, &refs, &mut way);
        key(8, prost::encoding::WireType::Varint, &mut way);
        prost::encoding::encode_varint(3, &mut way); // zigzag(-2)
        len_field(3, &way, &mut group);

        let block = block_with_groups(vec![group.clone()], Some(1000));
        let elements: Vec<_> = decode(&encode_blob(block.clone(), "raw")).unwrap().collect();

        // compare against prost's interpretation of the same bytes
        let parsed = osmpbf::PrimitiveGroup::decode(group.as_slice()).unwrap();
        let dense = parsed.dense.unwrap();
        assert_eq!(vec![1, 1, 1], dense.id);
        assert_eq!(vec![5, 1, -2], parsed.ways[0].refs);

        assert_eq!(4, elements.len());
        let OsmElement::Node(n) = &elements[2] else { panic!("{:?}", elements[2]) };
        assert_eq!(3, n.id);
        assert_eq!(3e-5, n.lat);
        assert_eq!(6e-5, n.lon);
        assert_eq!(tags! {"key" => "bad\u{FFFD}utf8"}, n.tags);
        assert_eq!(Some(Info::new(0, 0, 0, 3, "user")), n.info);
        let OsmElement::Node(n) = &elements[1] else { panic!() };
        assert_eq!(Some(Info::new(0, 0, 0, 2, "")), n.info);
        let OsmElement::Way(w) = &elements[3] else { panic!("{:?}", elements[3]) };
        assert_eq!(7, w.id);
        assert_eq!(vec![5, 6, 4], w.nodes);
        assert_eq!(tags! {"key" => "bad\u{FFFD}utf8"}, w.tags);
        assert_eq!(Some(Info { version: -1, ..Info::new(0, 0, 0, 0, "") }), w.info);
    }

    #[test]
    fn test_with_blob_content() {
        let data = test_block();
        for compression in ["raw", "zlib", "lz4", "zstd"] {
            assert_eq!(data, with_blob_content(&encode_blob(data.clone(), compression), |d| Ok(d.to_vec())).unwrap());
        }
        // zlib with an incorrect raw_size still decodes through the streaming fallback
        let mut blob = osmpbf::Blob::decode(encode_blob(data.clone(), "zlib").as_slice()).unwrap();
        blob.raw_size = Some(3);
        assert_eq!(data, with_blob_content(&blob.encode_to_vec(), |d| Ok(d.to_vec())).unwrap());
        blob.raw_size = None;
        assert_eq!(data, with_blob_content(&blob.encode_to_vec(), |d| Ok(d.to_vec())).unwrap());
    }

    #[test]
    fn test_nested_iterators_on_one_thread() {
        let blob = encode_blob(test_block(), "zlib");
        let a = decode(&blob).unwrap();
        let b = decode(&blob).unwrap();
        let (a, b): (Vec<_>, Vec<_>) = (a.collect(), b.collect());
        assert_eq!(a, b);
    }
}
