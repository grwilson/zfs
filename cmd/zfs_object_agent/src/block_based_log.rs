use crate::base_types::*;
use crate::block_access::BlockAccess;
use crate::extent_allocator::ExtentAllocator;
use anyhow::Context;
use async_stream::stream;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use futures_core::Stream;
use log::*;
use more_asserts::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::cmp::max;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::Bound::*;
use std::ops::Sub;
use std::sync::Arc;

// XXX maybe this is wasteful for the smaller logs?
const DEFAULT_EXTENT_SIZE: usize = 128 * 1024 * 1024;
//const READ_IO_SIZE: usize = 1 * 1024 * 1024;
const ENTRIES_PER_CHUNK: usize = 100;

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct BlockBasedLogPhys {
    // XXX on-disk format could just be array of extents; offset can be derived
    // from size of previous extents. We do need the btree in RAM though so that
    // we can do random reads on the Index (unless the ChunkSummary points
    // directly to the on-disk location)
    extents: BTreeMap<LogOffset, Extent>, // offset -> disk_location, size
    next_chunk: ChunkID,
    next_chunk_offset: LogOffset, // logical byte offset of next chunk to write
    num_entries: u64,
}

pub trait BlockBasedLogEntry: 'static + OnDisk + Copy + Clone + Unpin + Send + Sync {}
//pub trait OrderedBlockBasedLogEntry: BlockBasedLogEntry + Ord {}

pub struct BlockBasedLog<T: BlockBasedLogEntry> {
    block_access: Arc<BlockAccess>,
    extent_allocator: Arc<ExtentAllocator>,
    phys: BlockBasedLogPhys,
    // XXX need to load this from the chunkSummary
    // XXX need to detect if this is present or not, and fail operations that rely on it if not present
    chunks: Vec<(LogOffset, T)>, // Stores first entry (and offset) of each chunk.
    pending_entries: Vec<T>,
}

#[derive(Serialize, Deserialize, Debug)]
struct BlockBasedLogChunk<T: BlockBasedLogEntry> {
    id: ChunkID,
    offset: LogOffset,
    #[serde(bound(deserialize = "Vec<T>: DeserializeOwned"))]
    entries: Vec<T>,
}

impl<T: BlockBasedLogEntry> BlockBasedLog<T> {
    pub fn open(
        block_access: Arc<BlockAccess>,
        extent_allocator: Arc<ExtentAllocator>,
        phys: BlockBasedLogPhys,
    ) -> BlockBasedLog<T> {
        for (_offset, extent) in phys.extents.iter() {
            extent_allocator.claim(extent);
        }
        BlockBasedLog {
            block_access,
            extent_allocator,
            phys,
            chunks: Vec::new(),
            pending_entries: Vec::new(),
        }
    }

    pub fn get_phys(&self) -> BlockBasedLogPhys {
        self.phys.clone()
    }

    pub fn append(&mut self, entry: T) {
        self.pending_entries.push(entry);
        // XXX if too many pending, initiate flush?
    }

    pub async fn flush(&mut self) {
        if self.pending_entries.is_empty() {
            return;
        }

        let writes_stream = FuturesUnordered::new();
        for pending_entries_chunk in self.pending_entries.chunks(ENTRIES_PER_CHUNK) {
            let chunk = BlockBasedLogChunk {
                id: self.phys.next_chunk,
                offset: self.phys.next_chunk_offset,
                entries: pending_entries_chunk.to_owned(),
            };

            let first_entry = *chunk.entries.first().unwrap();

            let mut extent = self.next_write_location();
            let raw_chunk = self.block_access.json_chunk_to_raw(&chunk);
            let raw_size = raw_chunk.len();
            if raw_size > extent.size {
                // free the unused tail of this extent
                self.extent_allocator.free(&extent);
                let capacity = match self.phys.extents.iter_mut().next_back() {
                    Some((last_offset, last_extent)) => {
                        last_extent.size -= extent.size;
                        LogOffset(last_offset.0 + last_extent.size as u64)
                    }
                    None => LogOffset(0),
                };

                extent = self
                    .extent_allocator
                    .allocate(raw_size, max(raw_size, DEFAULT_EXTENT_SIZE));
                self.phys.extents.insert(capacity, extent);
                assert_ge!(extent.size, raw_size);
            }
            // XXX add name of this log for debug purposes?
            debug!(
                "flushing BlockBasedLog: writing {:?} ({:?}) with {} entries ({} bytes) to {:?}",
                chunk.id,
                chunk.offset,
                chunk.entries.len(),
                raw_chunk.len(),
                extent.location,
            );
            // XXX would be better to aggregate lots of buffers into one write
            writes_stream.push(self.block_access.write_raw(extent.location, raw_chunk));

            assert_eq!(ChunkID(self.chunks.len() as u64), chunk.id);
            self.chunks.push((chunk.offset, first_entry));

            self.phys.num_entries += chunk.entries.len() as u64;
            self.phys.next_chunk = self.phys.next_chunk.next();
            self.phys.next_chunk_offset.0 += raw_size as u64;
        }
        writes_stream.for_each(|_| async move {}).await;
        self.pending_entries.truncate(0);
    }

    pub fn clear(&mut self) {
        self.pending_entries.clear();
        for extent in self.phys.extents.values() {
            self.extent_allocator.free(extent);
        }
        self.phys = BlockBasedLogPhys::default();
    }

    fn next_write_location(&self) -> Extent {
        match self.phys.extents.iter().next_back() {
            Some((offset, extent)) => {
                // There shouldn't be any extents after the last (partially-full) one.
                assert_ge!(self.phys.next_chunk_offset, offset);
                let offset_within_extent = self.phys.next_chunk_offset.0 - offset.0;
                // The last extent should go at least to the end of the chunks.
                assert_le!(offset_within_extent as usize, extent.size);
                Extent {
                    location: DiskLocation {
                        offset: extent.location.offset + offset_within_extent,
                    },
                    size: extent.size - offset_within_extent as usize,
                }
            }
            None => Extent {
                location: DiskLocation { offset: 0 },
                size: 0,
            },
        }
    }

    /*
    fn capacity(&self) -> LogOffset {
        match self.phys.extents.iter().next_back() {
            Some((offset, extent)) => offset + extent.size,
            None => 0,
        }
    }
    */

    /// Iterates the on-disk state; panics if there are pending changes.
    pub fn iter(&self) -> impl Stream<Item = T> {
        assert!(self.pending_entries.is_empty());
        // XXX is it possible to do this without copying self.phys.extents?  Not
        // a huge deal I guess since it should be small.
        let phys = self.phys.clone();
        let block_access = self.block_access.clone();
        stream! {
            let mut num_entries = 0;
            let mut chunk_id = ChunkID(0);
            for (_offset, extent) in phys.extents.iter() {
                // XXX probably want to do smaller i/os than the entire extent (up to 128MB)
                // XXX also want to issue a few in parallel?
                // XXX if this extent is at the end, we don't need to read the
                // unused part of it (based on next_chunk_offset)
                let extent_bytes = block_access.read_raw(*extent).await;
                // XXX handle checksum error here
                let mut total_consumed = 0;
                while total_consumed < extent_bytes.len() {
                    let chunk_location = DiskLocation {
                        offset: extent.location.offset + total_consumed as u64,
                    };
                    trace!("decoding {:?} from {:?}", chunk_id, chunk_location);
                    let (chunk, consumed): (BlockBasedLogChunk<T>, usize) = block_access
                        .json_chunk_from_raw(&extent_bytes[total_consumed..])
                        .context(format!("{:?} at {:?}", chunk_id, chunk_location,))
                        .unwrap();
                    assert_eq!(chunk.id, chunk_id);
                    for entry in chunk.entries {
                        yield entry;
                        num_entries += 1;
                    }
                    chunk_id = chunk_id.next();
                    total_consumed += consumed;
                    if chunk_id == phys.next_chunk {
                        break;
                    }
                }
            }
            assert_eq!(phys.num_entries, num_entries);
        }
    }

    /// Returns the exact location/size of this chunk (not the whole contiguous extent)
    fn chunk_extent(&self, chunk_id: usize) -> Extent {
        let (chunk_offset, _first_entry) = self.chunks[chunk_id];
        let chunk_size = if chunk_id == self.chunks.len() - 1 {
            self.phys.next_chunk_offset - chunk_offset
        } else {
            self.chunks[chunk_id + 1].0 - chunk_offset
        } as usize;

        let (extent_offset, extent) = self
            .phys
            .extents
            .range((Unbounded, Included(chunk_offset)))
            .next_back()
            .unwrap();
        extent.range((chunk_offset - *extent_offset) as usize, chunk_size)
    }

    /// Entries must have been added in sorted order, according to the provided
    /// key-extraction function.  Similar to Vec::binary_search_by_key().
    pub async fn lookup_by_key<B, F>(&self, key: &B, mut f: F) -> Option<T>
    where
        B: Ord + Debug,
        F: FnMut(&T) -> B,
    {
        assert_eq!(ChunkID(self.chunks.len() as u64), self.phys.next_chunk);
        // XXX would be nice to also store last entry in the log, so that if we
        // look for something after it, we can return None without reading the
        // last chunk.
        let chunk_id = match self
            .chunks
            .binary_search_by_key(key, |(_offset, first_entry)| f(first_entry))
        {
            Ok(index) => index,
            Err(index) if index == 0 => return None, // key is before the first chunk, therefore not present
            Err(index) => index - 1,
        };

        let chunk_extent = self.chunk_extent(chunk_id);
        trace!(
            "reading log chunk {} at {:?} to lookup {:?}",
            chunk_id,
            chunk_extent,
            key
        );
        let chunk_bytes = self.block_access.read_raw(chunk_extent).await;
        let (chunk, _consumed): (BlockBasedLogChunk<T>, usize) =
            self.block_access.json_chunk_from_raw(&chunk_bytes).unwrap();
        assert_eq!(chunk.id, ChunkID(chunk_id as u64));
        match chunk.entries.binary_search_by_key(key, f) {
            Ok(index) => Some(chunk.entries[index]),
            Err(_) => None,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct LogOffset(u64);

impl Sub<LogOffset> for LogOffset {
    type Output = u64;

    fn sub(self, rhs: LogOffset) -> Self::Output {
        self.0 - rhs.0
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct ChunkID(u64);
impl ChunkID {
    pub fn next(&self) -> ChunkID {
        ChunkID(self.0 + 1)
    }
}
