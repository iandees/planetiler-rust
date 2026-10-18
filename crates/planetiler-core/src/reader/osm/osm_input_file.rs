//! Port of `reader/osm/OsmInputFile.java`: an input file in `.osm.pbf` format.
//!
//! See <https://wiki.openstreetmap.org/wiki/PBF_Format>.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use memmap2::Mmap;
use prost::Message;

use crate::config::bounds::BoundsProvider;
use crate::geo::envelope::Envelope;
use crate::proto::osmpbf::BlobHeader;
use crate::reader::osm::osm_block_source::{Block, OsmBlockSource};
use crate::reader::osm::osm_header::OsmHeader;
use crate::reader::osm::pbf_decoder;

const MAX_HEADER_SIZE: usize = 64 * 1024;

/// An OSM `.osm.pbf` input file.
#[derive(Clone, Debug)]
pub struct OsmInputFile {
    path: PathBuf,
    lazy: bool,
}

fn validate_header(data: &[u8]) -> anyhow::Result<()> {
    let header = pbf_decoder::decode_header(data)?;
    let unsupported: Vec<&String> =
        header.required_features.iter().filter(|f| !matches!(f.as_str(), "OsmSchema-V0.6" | "DenseNodes")).collect();
    if !unsupported.is_empty() {
        bail!("PBF file contains unsupported features {unsupported:?}");
    }
    Ok(())
}

/// Reads a 4-byte big-endian length + `BlobHeader` from `reader`, or `None` at end of file.
fn read_blob_header(reader: &mut impl Read, buf: &mut Vec<u8>) -> anyhow::Result<Option<BlobHeader>> {
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let header_size = u32::from_be_bytes(len) as usize;
    if header_size > MAX_HEADER_SIZE {
        bail!("Header longer than 64 KiB");
    }
    buf.resize(header_size, 0);
    reader.read_exact(buf).context("Unable to read blob header")?;
    Ok(Some(BlobHeader::decode(buf.as_slice()).context("Unable to parse blob header")?))
}

impl OsmInputFile {
    /// Creates a new OSM input file reader. If `lazy_reads` is true, defers reading the actual content of each block
    /// from disk until the block is decoded in a worker thread (using a memory-mapped file).
    pub fn new(path: impl Into<PathBuf>, lazy_reads: bool) -> OsmInputFile {
        OsmInputFile { path: path.into(), lazy: lazy_reads }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Size of the file on disk.
    pub fn disk_usage_bytes(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    /// Returns details from the header block of this file.
    pub fn get_header(&self) -> anyhow::Result<OsmHeader> {
        let mut reader = BufReader::new(File::open(&self.path).with_context(|| format!("{:?}", self.path))?);
        let mut buf = Vec::new();
        let header = read_blob_header(&mut reader, &mut buf)?.context("Empty PBF file")?;
        let mut blob = vec![0u8; header.datasize.max(0) as usize];
        reader.read_exact(&mut blob)?;
        pbf_decoder::decode_header(&blob)
    }

    /// Returns a block source that iterates through the blocks of this file.
    pub fn get(&self) -> anyhow::Result<OsmFileBlockSource> {
        let mmap = if self.lazy {
            let file = File::open(&self.path).with_context(|| format!("{:?}", self.path))?;
            // SAFETY: the input file is not expected to be modified while it is being read
            Some(Arc::new(unsafe { Mmap::map(&file)? }))
        } else {
            None
        };
        Ok(OsmFileBlockSource { path: self.path.clone(), mmap })
    }

    /// Shortcut for `OsmInputFile::new(path, false).get()`.
    pub fn read_from(path: impl Into<PathBuf>) -> anyhow::Result<OsmFileBlockSource> {
        OsmInputFile::new(path, false).get()
    }
}

impl BoundsProvider for OsmInputFile {
    /// Returns the bounding box of this file from the header block, or `None` if it can't be read.
    fn get_lat_lon_bounds(&self) -> Option<Envelope> {
        match self.get_header() {
            Ok(header) => Some(header.bounds),
            Err(e) => {
                tracing::warn!("Unable to read bounds from {:?}: {e:#}", self.path);
                None
            }
        }
    }
}

/// Block source over an `.osm.pbf` file.
///
/// In eager mode, the reading thread reads the raw bytes of each block and passes them off to worker threads. In lazy
/// mode the file is memory-mapped and the reading thread just skips over each block, passing the offset/length to
/// workers so they can read the contents from disk in parallel.
#[derive(Debug)]
pub struct OsmFileBlockSource {
    path: PathBuf,
    mmap: Option<Arc<Mmap>>,
}

impl OsmFileBlockSource {
    fn for_each_eager(&self, consumer: &mut dyn FnMut(Block) -> anyhow::Result<()>) -> anyhow::Result<()> {
        let file = File::open(&self.path).with_context(|| format!("{:?}", self.path))?;
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut header_buf = Vec::new();
        while let Some(header) = read_blob_header(&mut reader, &mut header_buf)? {
            let mut bytes = vec![0u8; header.datasize.max(0) as usize];
            reader.read_exact(&mut bytes).context("Unable to read blob")?;
            match header.r#type.as_str() {
                "OSMData" => consumer(Block::from_bytes(bytes))?,
                "OSMHeader" => validate_header(&bytes)?,
                other => tracing::warn!("Unrecognized OSM PBF blob header type: {other}"),
            }
        }
        Ok(())
    }

    fn for_each_lazy(
        &self,
        mmap: &Arc<Mmap>,
        consumer: &mut dyn FnMut(Block) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let data: &[u8] = mmap;
        let mut position = 0usize;
        let mut header_buf = Vec::new();
        while position < data.len() {
            let mut cursor = &data[position..];
            let before = cursor.len();
            let Some(header) = read_blob_header(&mut cursor, &mut header_buf)? else { break };
            let start = position + (before - cursor.len());
            let size = header.datasize.max(0) as usize;
            if start + size > data.len() {
                bail!("Truncated PBF file: blob at {start} of size {size} exceeds file length {}", data.len());
            }
            match header.r#type.as_str() {
                "OSMData" => consumer(Block::from_mmap(mmap.clone(), start, size))?,
                "OSMHeader" => validate_header(&data[start..start + size])?,
                other => tracing::warn!("Unrecognized OSM PBF blob header type: {other}"),
            }
            position = start + size;
        }
        Ok(())
    }
}

impl OsmBlockSource for OsmFileBlockSource {
    fn for_each_block(&self, consumer: &mut dyn FnMut(Block) -> anyhow::Result<()>) -> anyhow::Result<()> {
        match &self.mmap {
            Some(mmap) => self.for_each_lazy(mmap, consumer),
            None => self.for_each_eager(consumer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::osm::osm_element::{Info, Member, Node, OsmElement, OsmElementType, Relation, Way};
    use crate::tags;

    fn resource(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources").join(name)
    }

    fn expected_bounds() -> Envelope {
        Envelope::new(7.409205, 7.448637, 43.72335, 43.75169)
    }

    #[test]
    fn test_get_bounds() {
        assert_eq!(
            Some(expected_bounds()),
            OsmInputFile::new(resource("monaco-latest.osm.pbf"), false).get_lat_lon_bounds()
        );
    }

    #[test]
    fn test_get_header() {
        assert_eq!(
            OsmHeader {
                bounds: expected_bounds(),
                required_features: vec!["OsmSchema-V0.6".into(), "DenseNodes".into()],
                optional_features: vec![],
                writingprogram: "osmium/1.8.0".into(),
                source: "".into(),
                instant: "2021-04-21T20:21:46Z".parse().unwrap(),
                osmosis_replication_sequence_number: 2947,
                osmosis_replication_base_url: "http://download.geofabrik.de/europe/monaco-updates".into(),
            },
            OsmInputFile::new(resource("monaco-latest.osm.pbf"), false).get_header().unwrap()
        );
    }

    #[derive(Default)]
    struct Counts {
        nodes: usize,
        ways: usize,
        rels: usize,
        node: Option<Node>,
        way: Option<Way>,
        rel: Option<Relation>,
    }

    /// Reads all blocks, decoding them on worker threads.
    fn read(name: &str, lazy: bool, node_id: i64, way_id: i64, rel_id: i64) -> Counts {
        let source = OsmInputFile::new(resource(name), lazy).get().unwrap();
        let (tx, rx) = crossbeam_channel::bounded::<Block>(100);
        let results = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..3)
                .map(|_| {
                    let rx = rx.clone();
                    scope.spawn(move || {
                        let mut c = Counts::default();
                        for block in rx {
                            for e in block.decode_elements().unwrap() {
                                match e {
                                    OsmElement::Node(n) => {
                                        c.nodes += 1;
                                        if n.id == node_id {
                                            c.node = Some(n);
                                        }
                                    }
                                    OsmElement::Way(w) => {
                                        c.ways += 1;
                                        if w.id == way_id {
                                            c.way = Some(w);
                                        }
                                    }
                                    OsmElement::Relation(r) => {
                                        c.rels += 1;
                                        if r.id == rel_id {
                                            c.rel = Some(r);
                                        }
                                    }
                                    OsmElement::Other(_) => {}
                                }
                            }
                        }
                        c
                    })
                })
                .collect();
            drop(rx);
            source.for_each_block(&mut |block| Ok(tx.send(block)?)).unwrap();
            drop(tx);
            workers.into_iter().map(|w| w.join().unwrap()).collect::<Vec<_>>()
        });
        let mut total = Counts::default();
        for c in results {
            total.nodes += c.nodes;
            total.ways += c.ways;
            total.rels += c.rels;
            total.node = total.node.or(c.node);
            total.way = total.way.or(c.way);
            total.rel = total.rel.or(c.rel);
        }
        total
    }

    fn expected_node() -> Node {
        Node {
            id: 1737114566,
            tags: tags! {"highway" => "crossing", "crossing" => "zebra"},
            lat: 43.7409723,
            lon: 7.4303278,
            info: Some(Info::new(0, 1600807207, 0, 5, "")),
        }
    }

    fn expected_way() -> Way {
        Way {
            id: 4097656,
            tags: tags! {
              "name" => "Avenue Princesse Alice", "lanes" => "2", "maxspeed" => "30", "highway" => "primary",
              "surface" => "asphalt", "lit" => "yes"
            },
            nodes: vec![
                21912089, 7265761724, 1079750744, 2104793864, 6340961560, 1110560507, 21912093, 6340961559, 21912095,
                7265762803, 2104793866, 6340961561, 5603088200, 6340961562, 21912097, 21912099,
            ],
            info: Some(Info::new(0, 1583398246, 0, 13, "")),
        }
    }

    fn expected_rel() -> Relation {
        Relation {
            id: 7360630,
            tags: tags! {
              "local_ref" => "Saint-Roman", "public_transport:version" => "2", "name" => "Saint-Roman",
              "public_transport" => "stop_area", "type" => "public_transport",
              "operator" => "Compagnie des Autobus de Monaco", "network" => "Autobus de Monaco"
            },
            members: vec![
                Member::new(OsmElementType::Way, 503638817, "platform"),
                Member::new(OsmElementType::Way, 503638816, "platform"),
                Member::new(OsmElementType::Node, 4939122054, "platform"),
                Member::new(OsmElementType::Node, 3465728159, "stop"),
                Member::new(OsmElementType::Node, 4939122068, "platform"),
                Member::new(OsmElementType::Node, 3805333988, "stop"),
            ],
            info: Some(Info::new(0, 1586074405, 0, 2, "")),
        }
    }

    #[test]
    fn test_read_monaco_twice() {
        for lazy in [false, true] {
            for pass in 1..=2 {
                let c = read("monaco-latest.osm.pbf", lazy, 1737114566, 4097656, 7360630);
                assert_eq!(25_423, c.nodes, "nodes pass {pass} lazy={lazy}");
                assert_eq!(4_106, c.ways, "ways pass {pass}");
                assert_eq!(243, c.rels, "rels pass {pass}");
                assert_eq!(Some(expected_node()), c.node);
                assert_eq!(Some(expected_way()), c.way);
                assert_eq!(Some(expected_rel()), c.rel);
            }
        }
    }

    #[test]
    fn test_read_monaco_lz4() {
        for lazy in [false, true] {
            let c = read("monaco-latest.lz4.osm.pbf", lazy, 1737114566, 4097656, 7360630);
            let expected = read("monaco-latest.osm.pbf", lazy, 0, 0, 0);
            assert!(c.nodes > 0);
            assert_eq!((expected.nodes, expected.ways, expected.rels), (c.nodes, c.ways, c.rels));
        }
    }

    #[test]
    fn test_read_monaco_without_changesets_twice() {
        for lazy in [false, true] {
            for pass in 1..=2 {
                let c = read("monaco-latest-without-changesets.osm.pbf", lazy, 1737114566, 0, 0);
                assert_eq!(27_135, c.nodes, "nodes pass {pass}");
                assert_eq!(4_295, c.ways, "ways pass {pass}");
                assert_eq!(255, c.rels, "rels pass {pass}");
                assert_eq!(Some(&Info::new(0, 1647347498, 0, 8, "")), c.node.as_ref().and_then(|n| n.info.as_ref()));
            }
        }
    }

    #[test]
    fn test_missing_file() {
        let file = OsmInputFile::new(resource("does-not-exist.osm.pbf"), false);
        assert!(file.get_header().is_err());
        assert_eq!(None, file.get_lat_lon_bounds());
    }
}
