//! Port of `reader/osm/PbfDecoder.java` (adapted from Osmosis): converts PBF block data into decoded entities.
//!
//! [`decode`] decompresses and parses an `OSMData` blob, returning an iterator ([`PbfElements`]) that lazily builds
//! [`OsmElement`]s so that callers control parallelism (each worker thread decodes its own blocks).
//!
//! Supports raw, zlib, lz4 (like Java) and zstd compressed blobs. Decompression uses a per-thread reusable buffer, and
//! way node / relation member arrays are delta-decoded in place to avoid extra allocations.
//!
//! TODO(port): decoding still goes through prost structs; a hand-written zero-copy decoder could avoid the
//! intermediate allocations for keys/vals arrays.

use std::cell::RefCell;

use anyhow::{Context, bail};
use chrono::DateTime;
use prost::Message;

use crate::geo::envelope::Envelope;
use crate::proto::osmpbf::{self, blob, relation::MemberType};
use crate::reader::osm::osm_element::{Info, Member, Node, OsmElement, OsmElementType, Relation, Way};
use crate::reader::osm::osm_header::OsmHeader;
use crate::reader::osm::pbf_field_decoder::PbfFieldDecoder;
use crate::value::{Tags, Value};

thread_local! {
    static DECOMPRESS_BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Decompresses the content of a raw `Blob` message and passes the uncompressed bytes to `f`.
pub fn with_blob_content<R>(raw_blob: &[u8], f: impl FnOnce(&[u8]) -> anyhow::Result<R>) -> anyhow::Result<R> {
    let blob = osmpbf::Blob::decode(raw_blob).context("Unable to parse PBF blob")?;
    let raw_size = blob.raw_size.unwrap_or(0).max(0) as usize;
    match blob.data {
        Some(blob::Data::Raw(data)) => f(&data),
        Some(blob::Data::ZlibData(data)) => DECOMPRESS_BUFFER.with_borrow_mut(|buf| {
            buf.clear();
            buf.reserve(raw_size);
            let mut inflater = flate2::Decompress::new(true);
            let status = inflater
                .decompress_vec(&data, buf, flate2::FlushDecompress::Finish)
                .context("Unable to decompress PBF blob.")?;
            if status != flate2::Status::StreamEnd {
                bail!("PBF blob contains incomplete compressed data.");
            }
            f(buf)
        }),
        Some(blob::Data::Lz4Data(data)) => DECOMPRESS_BUFFER.with_borrow_mut(|buf| {
            buf.clear();
            buf.resize(raw_size, 0);
            let read = lz4_flex::block::decompress_into(&data, buf).context("Unable to decompress PBF blob.")?;
            if read != raw_size {
                bail!("Unable to decompress PBF blob. got {read} bytes but expected {raw_size}");
            }
            f(buf)
        }),
        Some(blob::Data::ZstdData(data)) => DECOMPRESS_BUFFER.with_borrow_mut(|buf| {
            buf.clear();
            buf.reserve(raw_size);
            zstd::stream::copy_decode(data.as_slice(), &mut *buf).context("Unable to decompress PBF blob.")?;
            f(buf)
        }),
        _ => bail!("PBF blob uses unsupported compression, only lz4, zstd, zlib, or raw may be used."),
    }
}

/// Decompresses and parses a block of primitive OSM elements.
pub fn decode(raw_blob: &[u8]) -> anyhow::Result<PbfElements> {
    let mut block = with_blob_content(raw_blob, |data| {
        osmpbf::PrimitiveBlock::decode(data).context("Unable to parse primitive block")
    })?;
    let decoder = PbfFieldDecoder::new(&mut block);
    let groups = std::mem::take(&mut block.primitivegroup)
        .into_iter()
        .map(|g| osmpbf::PrimitiveGroup::decode(g.as_slice()).context("Unable to parse primitive group"))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(PbfElements {
        default_granularity: decoder.is_default_granularity(),
        decoder,
        groups: groups.into_iter(),
        current: None,
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

struct DenseState {
    nodes: osmpbf::DenseNodes,
    info: osmpbf::DenseInfo,
    i: usize,
    kv_index: usize,
    node_id: i64,
    latitude: i64,
    longitude: i64,
    timestamp: i64,
    changeset: i64,
    uid: i32,
    user_sid: i32,
}

struct GroupIter {
    dense: Option<DenseState>,
    nodes: std::vec::IntoIter<osmpbf::Node>,
    ways: std::vec::IntoIter<osmpbf::Way>,
    relations: std::vec::IntoIter<osmpbf::Relation>,
}

/// Iterator over the elements of a decoded block: dense nodes, nodes, ways, then relations of each primitive group.
pub struct PbfElements {
    decoder: PbfFieldDecoder,
    default_granularity: bool,
    groups: std::vec::IntoIter<osmpbf::PrimitiveGroup>,
    current: Option<GroupIter>,
}

impl PbfElements {
    fn build_tags(&self, keys: &[u32], vals: &[u32]) -> Tags {
        let mut tags = Tags::with_capacity(keys.len());
        for (k, v) in keys.iter().zip(vals) {
            let key = self.decoder.decode_string(*k as usize);
            let value = self.decoder.decode_string(*v as usize);
            tags.insert(String::from(&**key), Value::String(value.clone()));
        }
        tags
    }

    fn parse_info(&self, info: Option<osmpbf::Info>) -> Option<Info> {
        // Java's protobuf getInfo() returns a default instance when missing, so info is always present
        let info = info.unwrap_or_default();
        Some(Info {
            changeset: info.changeset(),
            timestamp: info.timestamp(),
            user_id: info.uid(),
            version: info.version(),
            user: self.decoder.decode_string(info.user_sid() as usize).clone(),
        })
    }

    fn next_dense(&self, state: &mut DenseState) -> Option<OsmElement> {
        let i = state.i;
        if i >= state.nodes.id.len() {
            return None;
        }
        let nodes = &state.nodes;
        let info = &state.info;
        state.node_id += nodes.id[i];
        state.latitude += nodes.lat.get(i).copied().unwrap_or(0);
        state.longitude += nodes.lon.get(i).copied().unwrap_or(0);
        let version = info.version.get(i).copied().unwrap_or(0);
        state.timestamp += info.timestamp.get(i).copied().unwrap_or(0);
        state.changeset += info.changeset.get(i).copied().unwrap_or(0);
        state.uid += info.uid.get(i).copied().unwrap_or(0);
        state.user_sid += info.user_sid.get(i).copied().unwrap_or(0);
        state.i += 1;

        // The key and value string indexes are sequential in the same PBF array. Each set of tags is delimited by an
        // index with a value of 0.
        let kvs = &nodes.keys_vals;
        let mut tags = Tags::new();
        while state.kv_index < kvs.len() {
            let key_index = kvs[state.kv_index] as usize;
            state.kv_index += 1;
            if key_index == 0 {
                break;
            }
            let value_index = kvs.get(state.kv_index).copied().unwrap_or(0) as usize;
            state.kv_index += 1;
            if tags.capacity() == 0 {
                tags.reserve(3);
            }
            tags.insert(
                String::from(&**self.decoder.decode_string(key_index)),
                Value::String(self.decoder.decode_string(value_index).clone()),
            );
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
                user: self.decoder.decode_string(state.user_sid.max(0) as usize).clone(),
            }),
        }))
    }

    fn next_in_group(&self, group: &mut GroupIter) -> Option<OsmElement> {
        if let Some(dense) = &mut group.dense {
            if let Some(node) = self.next_dense(dense) {
                return Some(node);
            }
            group.dense = None;
        }
        if let Some(node) = group.nodes.next() {
            return Some(OsmElement::Node(Node {
                id: node.id,
                tags: self.build_tags(&node.keys, &node.vals),
                lat: self.decoder.decode_latitude(node.lat),
                lon: self.decoder.decode_longitude(node.lon),
                info: self.parse_info(node.info),
            }));
        }
        if let Some(mut way) = group.ways.next() {
            // node ids are delta encoded, decode in place to reuse the allocation
            let mut node_id = 0i64;
            for r in way.refs.iter_mut() {
                node_id += *r;
                *r = node_id;
            }
            return Some(OsmElement::Way(Way {
                id: way.id,
                tags: self.build_tags(&way.keys, &way.vals),
                nodes: std::mem::take(&mut way.refs),
                info: self.parse_info(way.info),
            }));
        }
        if let Some(relation) = group.relations.next() {
            let mut member_id = 0i64;
            let members = relation
                .memids
                .iter()
                .enumerate()
                .map(|(j, delta)| {
                    member_id += delta;
                    let member_type = match relation.types.get(j).copied().map(MemberType::try_from) {
                        Some(Ok(MemberType::Way)) => OsmElementType::Way,
                        Some(Ok(MemberType::Relation)) => OsmElementType::Relation,
                        _ => OsmElementType::Node,
                    };
                    let role = relation.roles_sid.get(j).copied().unwrap_or(0).max(0) as usize;
                    Member { member_type, ref_id: member_id, role: self.decoder.decode_string(role).clone() }
                })
                .collect();
            return Some(OsmElement::Relation(Relation {
                id: relation.id,
                tags: self.build_tags(&relation.keys, &relation.vals),
                members,
                info: self.parse_info(relation.info),
            }));
        }
        None
    }
}

impl Iterator for PbfElements {
    type Item = OsmElement;

    fn next(&mut self) -> Option<OsmElement> {
        loop {
            if let Some(mut group) = self.current.take()
                && let Some(element) = self.next_in_group(&mut group)
            {
                self.current = Some(group);
                return Some(element);
            }
            let group = self.groups.next()?;
            self.current = Some(GroupIter {
                dense: group.dense.map(|mut nodes| DenseState {
                    info: nodes.denseinfo.take().unwrap_or_default(),
                    nodes,
                    i: 0,
                    kv_index: 0,
                    node_id: 0,
                    latitude: 0,
                    longitude: 0,
                    timestamp: 0,
                    changeset: 0,
                    uid: 0,
                    user_sid: 0,
                }),
                nodes: group.nodes.into_iter(),
                ways: group.ways.into_iter(),
                relations: group.relations.into_iter(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    }
}
