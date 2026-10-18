//! Port of `reader/osm/OsmBlockSource.java`: an osm.pbf input that iterates through [`Block`]s of raw bytes that can
//! be decompressed/parsed in worker threads using [`Block::decode_elements`].

use std::sync::Arc;

use memmap2::Mmap;

use crate::reader::osm::osm_element::OsmElement;
use crate::reader::osm::pbf_decoder::{self, PbfElements};

/// A source of OSM blocks, iterated sequentially in a single thread.
pub trait OsmBlockSource: Send + Sync {
    /// Calls `consumer` for each block from the input sequentially in the calling thread. Stops early and returns the
    /// error if `consumer` fails.
    fn for_each_block(&self, consumer: &mut dyn FnMut(Block) -> anyhow::Result<()>) -> anyhow::Result<()>;
}

#[derive(Debug)]
enum BlockData {
    /// Raw `Blob` bytes read from disk.
    Bytes(Vec<u8>),
    /// A range of a memory-mapped file containing a `Blob`, read lazily in the worker thread.
    Mapped { mmap: Arc<Mmap>, offset: usize, len: usize },
    /// Already-decoded elements (useful for tests).
    Elements(Vec<OsmElement>),
}

/// An individual block from an osm.pbf file that can be decompressed/parsed with [`Block::decode_elements`].
///
/// Blocks are `Send` so they can be handed off to worker threads.
#[derive(Debug)]
pub struct Block(BlockData);

impl Block {
    /// A block from raw (compressed) `Blob` bytes.
    pub fn from_bytes(bytes: Vec<u8>) -> Block {
        Block(BlockData::Bytes(bytes))
    }

    /// A block backed by a range of a memory-mapped file.
    pub fn from_mmap(mmap: Arc<Mmap>, offset: usize, len: usize) -> Block {
        Block(BlockData::Mapped { mmap, offset, len })
    }

    /// Create a fake block from existing elements - useful for tests.
    pub fn of(elements: impl IntoIterator<Item = OsmElement>) -> Block {
        Block(BlockData::Elements(elements.into_iter().collect()))
    }

    /// The raw compressed bytes of this block, if it is backed by PBF data.
    pub fn raw_bytes(&self) -> Option<&[u8]> {
        match &self.0 {
            BlockData::Bytes(b) => Some(b),
            BlockData::Mapped { mmap, offset, len } => Some(&mmap[*offset..*offset + *len]),
            BlockData::Elements(_) => None,
        }
    }

    /// Decompress and parse OSM elements from this block.
    pub fn decode_elements(&self) -> anyhow::Result<BlockElements> {
        match &self.0 {
            BlockData::Elements(e) => Ok(BlockElements::Elements(e.clone().into_iter())),
            _ => Ok(BlockElements::Pbf(Box::new(pbf_decoder::decode(self.raw_bytes().unwrap())?))),
        }
    }

    /// Same as [`Block::decode_elements`] but consumes the block (avoids cloning pre-decoded elements).
    pub fn into_elements(self) -> anyhow::Result<BlockElements> {
        match self.0 {
            BlockData::Elements(e) => Ok(BlockElements::Elements(e.into_iter())),
            _ => self.decode_elements(),
        }
    }
}

/// Iterator over the elements of a [`Block`].
pub enum BlockElements {
    Pbf(Box<PbfElements>),
    Elements(std::vec::IntoIter<OsmElement>),
}

impl Iterator for BlockElements {
    type Item = OsmElement;

    #[inline]
    fn next(&mut self) -> Option<OsmElement> {
        match self {
            BlockElements::Pbf(p) => p.next(),
            BlockElements::Elements(e) => e.next(),
        }
    }
}

/// A block source over in-memory lists of elements, one [`Block`] per list (useful for tests).
#[derive(Clone, Debug, Default)]
pub struct ElementsBlockSource(pub Vec<Vec<OsmElement>>);

impl OsmBlockSource for ElementsBlockSource {
    fn for_each_block(&self, consumer: &mut dyn FnMut(Block) -> anyhow::Result<()>) -> anyhow::Result<()> {
        for block in &self.0 {
            consumer(Block::of(block.iter().cloned()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::osm::osm_element::Node;

    #[test]
    fn elements_source() {
        let source =
            ElementsBlockSource(vec![vec![Node::new(1, 2.0, 3.0).into()], vec![Node::new(2, 2.0, 3.0).into()]]);
        let mut ids = vec![];
        source
            .for_each_block(&mut |block| {
                ids.extend(block.decode_elements()?.map(|e| e.id()));
                Ok(())
            })
            .unwrap();
        assert_eq!(vec![1, 2], ids);
    }
}
