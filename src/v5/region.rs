// Copyright 2025 CloudWeGo Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    slice,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    config::SizePercentPair,
    consts::{DYNSHM_BUFFER_HEADER_SIZE, MIN_BLOCK_SIZE},
    v5::{buddy::BuddyAllocator, dynshm::DynShmBufferHeader, fixed::FixedAllocator},
};

/// Marks an allocator's block header as live.
pub const MAGIC: u32 = 0x0BAD_F00D;
/// Size of the magic number
pub const MAGIC_SIZE: usize = 4;
/// `[MAGIC:4][order/len:4]`
pub const ALLOCATOR_HEADER_SIZE: usize = 8;

/// Why a region or the allocator behind it refused a request.
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    // Configuration, raised while an allocator is being built.
    #[error("arena size does not match this allocator's block layout")]
    InvalidArenaSize,
    #[error("block size is not a power of two")]
    BlockSizeNotPowerOfTwo,
    #[error("block sizes are not in ascending order")]
    InvalidBlockSizeOrder,
    #[error("block size leaves no room for the block header")]
    BlockTooSmallForHeader,
    #[error("size class configuration is invalid")]
    InvalidSizeClassConfiguration,

    // Release, raised while a block is being handed back.
    #[error("offset does not name a block boundary in this arena")]
    InvalidOffset,
    #[error("block is not allocated: double free or foreign offset")]
    NotAllocated,
    #[error("block header contradicts the block it describes")]
    CorruptMetadata,
}

/// Carves a region's arena into blocks.
pub trait Allocator {
    /// Hands out a block with room for at least `size` bytes, or `None` when the arena has
    /// nothing left and when `size` is larger than any block this allocator carves.
    fn alloc(&mut self, arena: &mut [u8], size: usize) -> Option<Allocation>;
    /// The largest `size` [`alloc`](Self::alloc) could ever serve.
    fn max_allocation(&self) -> usize;
    /// Releases the block described by an [`Allocation`] this allocator handed out. Regions
    /// release by offset, so only tests call this.
    #[cfg_attr(not(test), allow(dead_code))]
    fn free(&mut self, arena: &mut [u8], allocation: Allocation) -> Result<(), Error>;
    /// Releases the block whose data starts at `offset`, which is how a peer names one.
    fn free_at(&mut self, arena: &mut [u8], offset: usize) -> Result<(), Error>;
    /// Whether `offset` could be the data offset of a block from this arena.
    fn is_valid_offset(&self, offset: usize) -> bool;
    /// Payload bytes currently free.
    fn available(&self) -> usize;
    /// Hands the whole arena back by rebuilding the free lists; the arena itself is not written.
    /// Every [`Allocation`] still outstanding is invalid afterwards and must not be freed.
    fn reset(&mut self, arena: &mut [u8]) -> Result<(), Error>;
}

/// One block handed out by an [`Allocator`], described in offsets into the region's arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    /// Offset of the allocator's eight-byte block header in the arena.
    pub block_offset: usize,
    /// Offset at which caller data starts.
    pub data_offset: usize,
    /// Requested data length.
    pub len: usize,
    /// Usable capacity of the allocated block.
    pub cap: usize,
}

/// The allocator a shared region runs, chosen at start-up from `SHMIPC_MEM_ALLOCATOR`.
enum RegionAllocator {
    Buddy(BuddyAllocator),
    Fixed(FixedAllocator),
}

impl Allocator for RegionAllocator {
    fn alloc(&mut self, arena: &mut [u8], size: usize) -> Option<Allocation> {
        match self {
            Self::Buddy(allocator) => allocator.alloc(arena, size),
            Self::Fixed(allocator) => allocator.alloc(arena, size),
        }
    }

    fn max_allocation(&self) -> usize {
        match self {
            Self::Buddy(allocator) => allocator.max_allocation(),
            Self::Fixed(allocator) => allocator.max_allocation(),
        }
    }

    fn free(&mut self, arena: &mut [u8], allocation: Allocation) -> Result<(), Error> {
        match self {
            Self::Buddy(allocator) => allocator.free(arena, allocation),
            Self::Fixed(allocator) => allocator.free(arena, allocation),
        }
    }

    fn free_at(&mut self, arena: &mut [u8], offset: usize) -> Result<(), Error> {
        match self {
            Self::Buddy(allocator) => allocator.free_at(arena, offset),
            Self::Fixed(allocator) => allocator.free_at(arena, offset),
        }
    }

    fn is_valid_offset(&self, offset: usize) -> bool {
        match self {
            Self::Buddy(allocator) => allocator.is_valid_offset(offset),
            Self::Fixed(allocator) => allocator.is_valid_offset(offset),
        }
    }

    fn available(&self) -> usize {
        match self {
            Self::Buddy(allocator) => allocator.available(),
            Self::Fixed(allocator) => allocator.available(),
        }
    }

    fn reset(&mut self, arena: &mut [u8]) -> Result<(), Error> {
        match self {
            Self::Buddy(allocator) => allocator.reset(arena),
            Self::Fixed(allocator) => allocator.reset(arena),
        }
    }
}

/// A region serving many allocations at once, which is where every payload up to
/// [`max_payload`](Self::max_payload) comes from. Anything larger goes to a
/// dedicated region instead.x
pub struct LocalSharedRegion {
    inner: Mutex<LocalSharedRegionInner>,
}

struct LocalSharedRegionInner {
    /// Where this region's range starts inside the manager's mapping, which is what turns
    /// a block's arena offset into the global offset the peer is handed.
    offset: usize,
    /// Length of that range.
    size: usize,
    /// Raw pointer to the start of the range.
    data: *mut u8,
    /// Type of allocator (buddy / fixed), decided at runtime.
    allocator: RegionAllocator,
    /// Blocks handed out and not yet released.
    allocs: usize,
    /// When the allocator was last built over this range, in unix nanoseconds. Zero before
    /// the first allocation and after a punch, which is the signal to rebuild it.
    activated_at: i64,
    /// Whether a punch has retired the free lists, so they are rebuilt before the next
    /// allocation.
    stale: bool,
}

impl LocalSharedRegion {
    /// Builds a region over `[data, data + size)` of the manager's mapping, which begins
    /// at `offset` bytes into that mapping.
    ///
    /// Uses BuddyAllocator by default. Set `SHMIPC_MEM_ALLOCATOR=fixed` for FixedAllocator.
    pub fn new(
        data: *mut u8,
        size: usize,
        offset: usize,
        min_block: usize,
        max_block: usize,
    ) -> Result<Self, anyhow::Error> {
        // SAFETY: the caller promises `size` writable bytes at `data`, and nothing else
        // borrows them yet.
        let arena = unsafe { slice::from_raw_parts_mut(data, size) };

        let allocator = if std::env::var("SHMIPC_MEM_ALLOCATOR").ok().as_deref() == Some("fixed") {
            let mut mid = max_block / 4;

            if mid <= min_block {
                mid = (min_block + max_block) / 2;
            }

            RegionAllocator::Fixed(FixedAllocator::new_from_config(
                arena,
                vec![
                    SizePercentPair {
                        size: min_block as u32,
                        percent: 50,
                    },
                    SizePercentPair {
                        size: mid as u32,
                        percent: 30,
                    },
                    SizePercentPair {
                        size: max_block as u32,
                        percent: 20,
                    },
                ],
            )?)
        } else {
            RegionAllocator::Buddy(BuddyAllocator::new_with_block_size(
                size, min_block, max_block,
            )?)
        };

        Ok(Self {
            inner: Mutex::new(LocalSharedRegionInner {
                offset,
                size,
                data,
                allocator,
                allocs: 0,
                activated_at: 0,
                stale: false,
            }),
        })
    }

    /// The largest payload this region can hand out, excluding the buffer header.
    pub fn max_payload(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .allocator
            .max_allocation()
            .saturating_sub(DYNSHM_BUFFER_HEADER_SIZE)
    }

    /// Allocates a block with room for `size` payload bytes, stamps its
    /// [`DynShmBufferHeader`] and hands back a pointer to that header. `None` when no block
    /// in the region is large enough.
    pub fn alloc(&self, size: usize) -> Option<*mut DynShmBufferHeader> {
        let mut inner = self.inner.lock().unwrap();

        if inner.activated_at == 0 {
            if inner.stale {
                // SAFETY: the mutex is held, so no other borrow of the range is live.
                let arena = unsafe { slice::from_raw_parts_mut(inner.data, inner.size) };
                inner.allocator.reset(arena).ok()?;
                inner.stale = false;
            }
            inner.allocs = 0;
            inner.activated_at = unix_nanos();
        }

        // SAFETY: as above.
        let arena = unsafe { slice::from_raw_parts_mut(inner.data, inner.size) };
        let block = inner
            .allocator
            .alloc(arena, DYNSHM_BUFFER_HEADER_SIZE + size)?;

        let ptr = unsafe { inner.data.add(block.data_offset) as *mut DynShmBufferHeader };

        unsafe {
            (*ptr).offset = inner.offset as i64 + block.data_offset as i64;
            (*ptr).len = size as u32;
            (*ptr).cap = (block.cap - DYNSHM_BUFFER_HEADER_SIZE) as u32;
            (*ptr).next = 0;
        }

        inner.allocs += 1;

        Some(ptr)
    }

    /// Releases the buffer at a global offset, which is the form the peer reports over the
    /// release queue.
    pub fn free_at(&self, global_offset: i64) -> Result<(), Error> {
        let mut inner = self.inner.lock().unwrap();
        let local_offset = usize::try_from(global_offset - inner.offset as i64)
            .map_err(|_| Error::InvalidOffset)?;
        if !inner.allocator.is_valid_offset(local_offset) {
            return Err(Error::InvalidOffset);
        }

        let buf = unsafe { inner.data.add(local_offset) as *mut DynShmBufferHeader };
        unsafe {
            if (*buf).offset == 0 {
                return Err(Error::NotAllocated);
            }
            (*buf).offset = 0;
        }

        // SAFETY: the mutex is held, so no other borrow of the range is live.
        let arena = unsafe { slice::from_raw_parts_mut(inner.data, inner.size) };
        inner.allocator.free_at(arena, local_offset)?;
        inner.allocs = inner.allocs.saturating_sub(1);

        Ok(())
    }

    /// Whether the region is too full to be worth trying.
    pub fn is_full(&self) -> bool {
        self.available() < DYNSHM_BUFFER_HEADER_SIZE + MIN_BLOCK_SIZE
    }

    /// Whether nothing allocated from this region is still outstanding.
    pub fn is_empty(&self) -> bool {
        self.alloc_count() == 0
    }

    /// Blocks handed out and not yet released.
    pub fn alloc_count(&self) -> usize {
        self.inner.lock().unwrap().allocs
    }

    /// Payload bytes the allocator still has free.
    pub fn available(&self) -> usize {
        self.inner.lock().unwrap().allocator.available()
    }

    /// Where this region starts inside the manager's mapping.
    pub fn offset(&self) -> usize {
        self.inner.lock().unwrap().offset
    }

    /// Length of the region's range.
    pub fn size(&self) -> usize {
        self.inner.lock().unwrap().size
    }

    /// Hands the region's range to `punch` and marks it inactive, but only if it has been
    /// sitting empty for at least `max_age` nanoseconds. Returns whether it was punched.
    pub fn punch_if_idle(&self, now: i64, max_age: i64, punch: impl FnOnce(usize, usize)) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.allocs != 0 || inner.activated_at == 0 || now - inner.activated_at < max_age {
            return false;
        }
        punch(inner.offset, inner.size);
        inner.activated_at = 0;
        inner.stale = true;
        true
    }

    /// Overrides the activation stamp. Setting it to zero forces the next
    /// [`alloc`](Self::alloc) to rebuild the allocator.
    pub fn set_activated_at(&self, timestamp: i64) {
        let mut inner = self.inner.lock().unwrap();
        inner.activated_at = timestamp;
        if timestamp == 0 {
            inner.stale = true;
        }
    }
}

/// A region backing one allocation larger than the shared allocator's biggest block. The
/// whole region is the allocation, so there is no allocator and no per-block header beyond
/// the buffer header at its start; `offset` in that header doubles as the busy flag.
pub struct LocalDedicatedRegion {
    /// Start of this region's range inside the manager's mapping.
    data: *mut u8,
    /// How far into that mapping the range begins.
    offset: usize,
    /// Length of the range, the payload's header included.
    size: usize,
    /// When the region was last allocated or freed, in unix nanoseconds. Zero until it is
    /// first used, and -1 once the manager has handed its pages back, which is what marks
    /// it as trimmable.
    updated: i64,
}

impl LocalDedicatedRegion {
    /// Builds a region over `[data, data + size)` of the manager's mapping, which begins at
    /// `offset` bytes into that mapping.
    pub fn new(data: *mut u8, size: usize, offset: usize) -> Self {
        // Explicitly initialize the header because this address range may contain
        // stale contents from a previous use.
        unsafe {
            *(data as *mut DynShmBufferHeader) = DynShmBufferHeader::default();
        }

        Self {
            data,
            offset,
            size,
            updated: 0,
        }
    }

    /// Hand the whole region out for a single use, or `None` if it is still busy or too
    /// small for the request.
    pub fn alloc(&mut self, size: usize) -> Option<*mut DynShmBufferHeader> {
        if !self.is_freed() || size + DYNSHM_BUFFER_HEADER_SIZE > self.size {
            return None;
        }
        self.updated = unix_nanos();
        let buf = self.data as *mut DynShmBufferHeader;

        unsafe {
            (*buf).offset = self.offset as i64;
            (*buf).len = size as u32;
            (*buf).cap = (self.size - DYNSHM_BUFFER_HEADER_SIZE) as u32;
            (*buf).next = 0;
        }

        Some(buf)
    }

    /// Whether the region is available to allocate from.
    pub fn is_freed(&self) -> bool {
        unsafe { (*(self.data as *const DynShmBufferHeader)).offset == 0 }
    }

    /// Release the region, which is what draining the release queue does.
    pub fn mark_freed(&mut self) -> Result<(), Error> {
        let buf = self.data as *mut DynShmBufferHeader;
        unsafe {
            if (*buf).offset == 0 {
                return Err(Error::NotAllocated);
            }
            (*buf).offset = 0;
        }
        self.updated = unix_nanos();

        Ok(())
    }

    /// Where this region starts inside the manager's mapping, which is the offset its one
    /// buffer is known by.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Length of the region's range.
    pub fn size(&self) -> usize {
        self.size
    }

    /// When the region was last allocated or freed, in unix nanoseconds. Zero before first
    /// use, and -1 once punched.
    pub fn updated(&self) -> i64 {
        self.updated
    }

    /// Overrides the timestamp. The manager uses it to mark a region punched, and tests to
    /// age one past the idle threshold without waiting.
    pub fn set_updated(&mut self, timestamp: i64) {
        self.updated = timestamp;
    }
}

/// Reads the `(magic, order)` pair at `offset`, or `None` when a whole header does not
/// lie inside the arena.
pub fn read_header(arena: &[u8], offset: usize) -> Option<(u32, u32)> {
    let header = arena.get(offset..offset.checked_add(ALLOCATOR_HEADER_SIZE)?)?;
    Some((
        u32::from_ne_bytes(header[0..MAGIC_SIZE].try_into().ok()?),
        u32::from_ne_bytes(header[MAGIC_SIZE..8].try_into().ok()?),
    ))
}

/// Stamps the magic and the metadata into the header at `offset`, marking the
/// block live.
pub fn write_header(arena: &mut [u8], offset: usize, value: usize) {
    arena[offset..offset + MAGIC_SIZE].copy_from_slice(&MAGIC.to_ne_bytes());
    arena[offset + MAGIC_SIZE..offset + ALLOCATOR_HEADER_SIZE]
        .copy_from_slice(&(value as u32).to_ne_bytes());
}

/// Zeroes the magic, which is what marks a block as no longer live.
pub fn clear_magic(arena: &mut [u8], offset: usize) {
    arena[offset..offset + MAGIC_SIZE].fill(0);
}

/// Wall-clock nanoseconds since the unix epoch, which is the stamp both kinds of region
/// age themselves against.
pub fn unix_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::{MAX_BLOCK_SIZE, MIN_BLOCK_SIZE};

    struct Arena {
        words: Vec<u64>,
        size: usize,
    }

    impl Arena {
        fn new(size: usize) -> Self {
            assert!(size.is_multiple_of(8));
            Self {
                words: vec![0; size / 8],
                size,
            }
        }

        fn ptr(&mut self) -> *mut u8 {
            self.words.as_mut_ptr() as *mut u8
        }

        fn header_at(&self, local_offset: usize) -> DynShmBufferHeader {
            unsafe {
                *((self.words.as_ptr() as *const u8).add(local_offset) as *const DynShmBufferHeader)
            }
        }
    }

    fn shared(arena: &mut Arena, offset: usize) -> LocalSharedRegion {
        let size = arena.size;
        LocalSharedRegion::new(arena.ptr(), size, offset, MIN_BLOCK_SIZE, MAX_BLOCK_SIZE).unwrap()
    }

    #[test]
    fn shared_alloc_stamps_the_global_offset_into_the_block() {
        const REGION_OFFSET: usize = 1 << 20;

        let mut arena = Arena::new(4 * MAX_BLOCK_SIZE);
        let region = shared(&mut arena, REGION_OFFSET);

        let buf = region.alloc(100).unwrap();
        let header = unsafe { *buf };

        // The offset the peer is handed is global, so the region's own start is included.
        assert!(header.offset > REGION_OFFSET as i64);
        assert_eq!(100, header.len);
        assert_eq!(0, header.next);
        assert!(header.cap >= 100);

        // And the same offset is readable straight out of the arena, which is how the peer
        // will find it.
        let local = header.offset as usize - REGION_OFFSET;
        assert_eq!(header, arena.header_at(local));
        assert_eq!(1, region.alloc_count());
    }

    #[test]
    fn shared_free_at_releases_only_the_named_block() {
        const REGION_OFFSET: usize = 0;

        let mut arena = Arena::new(4 * MAX_BLOCK_SIZE);
        let region = shared(&mut arena, REGION_OFFSET);

        let first = unsafe { *region.alloc(100).unwrap() }.offset;
        let second = unsafe { *region.alloc(100).unwrap() }.offset;
        let third = unsafe { *region.alloc(100).unwrap() }.offset;
        assert_eq!(3, region.alloc_count());

        region.free_at(first).unwrap();
        region.free_at(third).unwrap();
        assert_eq!(1, region.alloc_count());

        // The surviving block still claims its offset.
        assert_eq!(second, arena.header_at(second as usize).offset);
    }

    #[test]
    fn shared_free_at_rejects_a_double_free() {
        let mut arena = Arena::new(4 * MAX_BLOCK_SIZE);
        let region = shared(&mut arena, 0);

        // Read the offset out before the free, which zeroes it.
        let offset = unsafe { *region.alloc(100).unwrap() }.offset;

        assert_eq!(Ok(()), region.free_at(offset));
        assert_eq!(0, region.alloc_count());
        assert_eq!(Err(Error::NotAllocated), region.free_at(offset));
        assert_eq!(0, region.alloc_count());
    }

    #[test]
    fn shared_free_at_rejects_an_offset_from_another_region() {
        let mut arena = Arena::new(4 * MAX_BLOCK_SIZE);
        let region = shared(&mut arena, 1 << 20);

        let offset = unsafe { *region.alloc(100).unwrap() }.offset;

        assert_eq!(
            Err(Error::InvalidOffset),
            region.free_at(offset - (1 << 20))
        );
        assert_eq!(Err(Error::InvalidOffset), region.free_at(0));
        assert_eq!(1, region.alloc_count());
    }

    #[test]
    fn shared_alloc_reports_the_region_running_out() {
        let mut arena = Arena::new(MAX_BLOCK_SIZE);
        let region = shared(&mut arena, 0);

        assert!(!region.is_full());
        assert!(region.is_empty());

        let largest = MAX_BLOCK_SIZE - DYNSHM_BUFFER_HEADER_SIZE;
        assert!(region.alloc(largest).is_none());

        while region.alloc(MIN_BLOCK_SIZE / 2).is_some() {}

        assert!(region.is_full());
        assert!(!region.is_empty());
        assert!(region.available() < DYNSHM_BUFFER_HEADER_SIZE + MIN_BLOCK_SIZE);
    }

    #[test]
    fn only_a_punched_region_is_rebuilt() {
        let mut arena = Arena::new(4 * MAX_BLOCK_SIZE);
        let region = shared(&mut arena, 0);

        let a = unsafe { *region.alloc(100).unwrap() }.offset;
        let b = unsafe { *region.alloc(100).unwrap() }.offset;
        region.free_at(a).unwrap();
        region.free_at(b).unwrap();

        // Still active: the allocator carries on from its free lists, most recent first.
        assert_eq!(b, unsafe { *region.alloc(100).unwrap() }.offset);
        region.free_at(b).unwrap();

        // Punched: the next allocation starts over from a rebuilt allocator.
        assert!(region.punch_if_idle(unix_nanos(), 0, |_, _| {}));
        assert_eq!(a, unsafe { *region.alloc(100).unwrap() }.offset);
        assert_eq!(1, region.alloc_count());
    }

    #[test]
    fn dedicated_alloc_takes_the_whole_region() {
        const REGION_OFFSET: usize = 3 << 20;
        const REGION_SIZE: usize = 4 << 20;

        let mut arena = Arena::new(REGION_SIZE);
        let mut region = LocalDedicatedRegion::new(arena.ptr(), REGION_SIZE, REGION_OFFSET);

        assert!(region.is_freed());
        assert_eq!(REGION_OFFSET, region.offset());
        assert_eq!(REGION_SIZE, region.size());

        let header = unsafe { *region.alloc(3 << 20).unwrap() };

        // The header sits at the very start of the region, so its offset is the region's.
        assert_eq!(REGION_OFFSET as i64, header.offset);
        assert_eq!(3 << 20, header.len);
        assert_eq!((REGION_SIZE - DYNSHM_BUFFER_HEADER_SIZE) as u32, header.cap);
        assert_eq!(0, header.next);
        assert_eq!(header, arena.header_at(0));

        assert!(!region.is_freed());
        assert!(region.updated() > 0);
    }

    #[test]
    fn dedicated_alloc_refuses_a_busy_or_oversized_region() {
        const REGION_SIZE: usize = 1 << 20;

        let mut arena = Arena::new(REGION_SIZE);
        let mut region = LocalDedicatedRegion::new(arena.ptr(), REGION_SIZE, REGION_SIZE);

        // The buffer header has to fit alongside the payload.
        assert!(region.alloc(REGION_SIZE).is_none());
        assert!(
            region
                .alloc(REGION_SIZE - DYNSHM_BUFFER_HEADER_SIZE + 1)
                .is_none()
        );
        assert!(region.is_freed());

        assert!(
            region
                .alloc(REGION_SIZE - DYNSHM_BUFFER_HEADER_SIZE)
                .is_some()
        );
        assert!(region.alloc(1).is_none());
    }

    #[test]
    fn dedicated_mark_freed_rejects_a_double_free() {
        const REGION_SIZE: usize = 1 << 20;

        let mut arena = Arena::new(REGION_SIZE);
        let mut region = LocalDedicatedRegion::new(arena.ptr(), REGION_SIZE, REGION_SIZE);

        region.alloc(1024).unwrap();
        assert!(!region.is_freed());

        assert_eq!(Ok(()), region.mark_freed());
        assert!(region.is_freed());
        assert_eq!(Err(Error::NotAllocated), region.mark_freed());

        // And the region takes a new tenant afterwards.
        assert!(region.alloc(2048).is_some());
    }

    #[test]
    fn dedicated_new_clears_metadata_a_punched_range_left_behind() {
        const REGION_SIZE: usize = 1 << 20;

        let mut arena = Arena::new(REGION_SIZE);

        // Write stale metadata into the header.
        unsafe {
            (*(arena.ptr() as *mut DynShmBufferHeader)).offset = 0xDEAD;
        }

        let region = LocalDedicatedRegion::new(arena.ptr(), REGION_SIZE, REGION_SIZE);

        assert!(region.is_freed());
    }
}
