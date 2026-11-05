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
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    ptr, slice,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use ahash::AHashSet;
use anyhow::anyhow;
use tracing::{debug, warn};

use crate::{
    consts::{
        DYNSHM_BUFFER_HEADER_SIZE, DYNSHM_TOTAL_SIZE, MAX_BLOCK_SIZE, MIN_BLOCK_SIZE,
        SHARED_REGION_SIZE,
    },
    v5::{
        memfile::MemFile,
        mpsc::{MpscHeader, PopResult},
        region::{LocalDedicatedRegion, LocalSharedRegion, unix_nanos},
        release_queue::{RQ_SIZE, ReleaseQueue, map_rq, new_rq},
    },
};

/// Setting for the number of shared regions that are pinned.
pub const PINNED_REGION_NUM_ENV: &str = "SHMIPC_PINNED_REGION_NUM";
/// Default configuration when [`PINNED_REGION_NUM_ENV`] is unset or unreadable.
pub const DEFAULT_PINNED_REGION_NUM: usize = 4;

/// How long a region may sit idle before its pages are handed back.
pub const MAX_AGE: Duration = Duration::from_secs(30);
/// How often the cleanup thread drains releases and looks for idle regions.
pub const PRUNE_INTERVAL: Duration = Duration::from_secs(1);
/// Ceiling on a buffer chain, being every block the segment could possibly hold.
pub const MAX_CHAIN_NODES: usize = DYNSHM_TOTAL_SIZE / MIN_BLOCK_SIZE;
/// Stands in for a dedicated region's allocation timestamp once its pages have been handed
/// back, marking it as a candidate for trimming off the end.
const DEDICATED_REGION_PUNCHED: i64 = -1;

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum ManagerError {
    #[error("no space")]
    NoSpace,
    #[error("not allocated")]
    NotAllocated,
    #[error("invalid offset")]
    InvalidOffset,
    #[error("out of bounds")]
    OutOfBounds,
    #[error("release queue full")]
    ReleaseQueueFull,
    #[error("manager closed")]
    Closed,
}

/// Whether `SHMIPC_DEBUG_MODE=1` asked for a log line per allocation released.
fn debug_mode() -> bool {
    std::env::var("SHMIPC_DEBUG_MODE").ok().as_deref() == Some("1")
}

/// Returns the pinned region count.
fn configured_pinned_region_num(max_region_num: usize) -> usize {
    pinned_region_num_from(
        std::env::var(PINNED_REGION_NUM_ENV).ok().as_deref(),
        max_region_num,
    )
}

/// The clamping itself, split out from the environment so it can be called directly.
/// Anything that does not parse as a count falls back to the default rather than failing.
fn pinned_region_num_from(raw: Option<&str>, max_region_num: usize) -> usize {
    raw.and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_PINNED_REGION_NUM)
        .min(max_region_num)
}

/// Rounds a payload plus its buffer header up to whole [`MAX_BLOCK_SIZE`] blocks, which is
/// the unit a dedicated region is carved in. Zero for a size the wire format could not
/// describe, which the caller has to treat as a refusal rather than as an empty region.
pub fn calc_dedicated_region_size(data_size: usize) -> usize {
    if data_size > i32::MAX as usize {
        return 0;
    }
    (data_size + DYNSHM_BUFFER_HEADER_SIZE).div_ceil(MAX_BLOCK_SIZE) * MAX_BLOCK_SIZE
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DynShmBufferHeader {
    /// The block's own global offset, zero once it has been freed.
    pub offset: i64,
    /// Global offset of the next block of the same message, or zero at the end of a chain.
    pub next: i64,
    /// Bytes of `cap` that hold data.
    pub len: u32,
    /// Bytes of room after the header, which is the block the allocator handed out less
    /// the header itself.
    pub cap: u32,
}

impl DynShmBufferHeader {
    /// The bytes written into this block so far, which follow the header.
    ///
    /// `hdr` must point at a live block with block-wide provenance, and the block must not be
    /// written to or freed while the returned slice is alive.
    pub unsafe fn data<'a>(hdr: *const Self) -> &'a [u8] {
        // SAFETY: the caller guarantees a live block with block-wide provenance, so the header
        // is readable and the data follows it within the same allocation.
        unsafe {
            slice::from_raw_parts(
                hdr.cast::<u8>().add(DYNSHM_BUFFER_HEADER_SIZE),
                (*hdr).len as usize,
            )
        }
    }

    /// The bytes written into this block so far, for writing into.
    ///
    /// As [`Self::data`], and the caller must have exclusive access to the block while the
    /// returned slice is alive.
    pub unsafe fn data_mut<'a>(hdr: *mut Self) -> &'a mut [u8] {
        // SAFETY: as `data`, with the caller also promising exclusivity.
        unsafe {
            slice::from_raw_parts_mut(
                hdr.cast::<u8>().add(DYNSHM_BUFFER_HEADER_SIZE),
                (*hdr).len as usize,
            )
        }
    }

    /// Extends the block by `size` bytes and hands back just the new room, or `None` when
    /// the block has no space for it.
    ///
    /// As [`Self::data_mut`]: `hdr` must point at a live block with block-wide provenance that
    /// the caller has exclusive access to while the returned slice is alive.
    pub unsafe fn reserve<'a>(hdr: *mut Self, size: usize) -> Option<&'a mut [u8]> {
        // SAFETY: the caller guarantees a live block, so the header is readable and writable.
        let (start, cap) = unsafe { ((*hdr).len as usize, (*hdr).cap as usize) };
        let end = start.checked_add(size)?;

        if end > cap {
            return None;
        }

        // SAFETY: as above.
        unsafe { (*hdr).len = end as u32 };

        // SAFETY: `end <= cap`, so the room lies inside the block, and it starts after every
        // byte handed out before it.
        unsafe {
            Some(slice::from_raw_parts_mut(
                hdr.cast::<u8>().add(DYNSHM_BUFFER_HEADER_SIZE + start),
                size,
            ))
        }
    }

    /// Copies `payload` in after whatever the block already holds. False if it does not fit.
    ///
    /// # Safety
    ///
    /// As [`Self::reserve`].
    pub unsafe fn append(hdr: *mut Self, payload: &[u8]) -> bool {
        // SAFETY: the caller upholds `reserve`'s contract.
        match unsafe { Self::reserve(hdr, payload.len()) } {
            Some(buf) => {
                buf.copy_from_slice(payload);
                true
            }
            None => false,
        }
    }
}

/// The dedicated area, which grows down from the end of the segment.
struct Dedicated {
    /// Every dedicated region carved so far, freed ones included, in the order they were
    /// carved: descending by offset, so the last is the lowest and the only one whose
    /// address space can be given back.
    regions: Vec<LocalDedicatedRegion>,
    /// Lowest offset any dedicated region starts at, and the segment size when there are
    /// none.
    offset: usize,
}

/// Wakes the cleanup thread out of its tick.
#[derive(Default)]
struct Shutdown {
    closed: Mutex<bool>,
    wake: Condvar,
}

impl Shutdown {
    /// Marks the manager closed and wakes the cleanup thread out of its tick.
    fn signal(&self) {
        *self.closed.lock().unwrap() = true;
        self.wake.notify_all();
    }

    /// Sleeps up to `interval`, returning false once shutdown has been signalled.
    fn wait(&self, interval: Duration) -> bool {
        let closed = self.closed.lock().unwrap();
        if *closed {
            return false;
        }
        let (closed, _) = self.wake.wait_timeout(closed, interval).unwrap();
        !*closed
    }
}

/// The owning side of a dynamic shared-memory segment: it carves the buffers, hands their
/// offsets to the peer, and reclaims them when the peer reports back.
///
/// The segment is laid out as the release queue, then the shared area of equally sized
/// regions, then whatever is left, into which dedicated regions are carved downwards from
/// the end. A payload that fits a shared region's largest block comes from there; anything
/// larger gets a dedicated region of its own.
pub struct LocalManager {
    /// The segment itself, kept for the mapping and for the descriptor the peer is sent.
    file: MemFile,
    /// Start of the mapping.
    data: *mut u8,
    /// Length of the mapping.
    size: usize,

    /// Offsets the peer has finished with, waiting to be reclaimed. The ring has a single
    /// consumer, so the lock is what picks the one thread draining at any moment: a thread
    /// that cannot take it reports nothing freed rather than waiting.
    rq: Mutex<ReleaseQueue>,
    /// The ring's header, read without the lock to see whether a drain is worth taking it.
    rq_header: *const MpscHeader,

    /// The shared region which is fixed once built.
    shared_regions: Vec<LocalSharedRegion>,
    /// Regions `[0, min_shared_count)` are never punched.
    min_shared_count: usize,
    /// Largest payload a shared region can serve.
    max_shared_payload: usize,
    /// End of the shared area, which a dedicated region may not grow past.
    max_shared_offset: usize,
    /// Round-robin cursor over the pinned regions.
    alloc_idx: AtomicU32,
    /// Smallest block a shared region's allocator carves.
    min_block: usize,
    /// Largest block a shared region's allocator carves.
    max_block: usize,

    /// The dedicated area. One lock for the whole area, because carving a region and
    /// trimming one both move its boundary.
    dedicated: Mutex<Dedicated>,

    /// Set by close; every operation checks it first. The mapping outlives the flag, so an
    /// operation that passed the check just before close still touches valid memory.
    closed: AtomicBool,

    /// Shared with the cleanup thread.
    shutdown: Arc<Shutdown>,
    /// The cleanup thread, taken and joined by [`close_all`](LocalManager::close_all).
    cleanup: Mutex<Option<JoinHandle<()>>>,
    /// Whether to log every release, from [`debug_mode`].
    debug: bool,
}

/// Memory usage across a `LocalManager`'s regions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Release notifications waiting to be drained.
    pub rq_pending: usize,
    /// Total shared regions.
    pub shared_regions: usize,
    /// Shared regions holding at least one allocation.
    pub shared_active: usize,
    /// Free bytes across all shared regions.
    pub shared_free_bytes: usize,
    /// Total bytes across all shared regions.
    pub shared_bytes: usize,

    /// Dedicated regions, including freed regions.
    pub dedicated_regions: usize,
    /// Dedicated regions with an active allocation.
    pub dedicated_active: usize,
    /// Total bytes across all dedicated regions.
    pub dedicated_bytes: usize,
    /// Bytes held by dedicated regions that are currently allocated.
    pub dedicated_used: usize,
}

unsafe impl Send for LocalManager {}
unsafe impl Sync for LocalManager {}

impl LocalManager {
    /// Builds the default 4 GiB segment, a quarter of it given to shared regions.
    pub fn new() -> Result<Arc<Self>, anyhow::Error> {
        let total = DYNSHM_TOTAL_SIZE;
        let shared_cap = total / 4;
        let pinned =
            SHARED_REGION_SIZE * configured_pinned_region_num(shared_cap / SHARED_REGION_SIZE);

        Self::new_with_size(total, pinned, shared_cap, MIN_BLOCK_SIZE, MAX_BLOCK_SIZE)
    }

    /// Builds a segment of `total` bytes: `pinned_shared` bytes of shared regions that are
    /// never released, `shared_cap - pinned_shared` more that are released once idle, and
    /// whatever is left for dedicated regions.
    pub fn new_with_size(
        total: usize,
        pinned_shared: usize,
        shared_cap: usize,
        min_block: usize,
        max_block: usize,
    ) -> Result<Arc<Self>, anyhow::Error> {
        if total < RQ_SIZE + SHARED_REGION_SIZE {
            return Err(anyhow!(
                "dynshm segment of {} bytes cannot hold the release queue and one region",
                total
            ));
        }

        if shared_cap < pinned_shared {
            return Err(anyhow!(
                "dynshm shared cap {} is below the pinned {}",
                shared_cap,
                pinned_shared
            ));
        }

        if !min_block.is_power_of_two() || !max_block.is_power_of_two() {
            return Err(anyhow!("dynshm block sizes must be powers of two"));
        }

        if min_block > max_block {
            return Err(anyhow!(
                "dynshm min block {} exceeds max block {}",
                min_block,
                max_block
            ));
        }

        let mut file = MemFile::new("dynshm", total)?;
        let data = file.data_mut().as_mut_ptr();

        // Initialize RQ at offset 0.
        let rq = new_rq(&mut file.data_mut()[..RQ_SIZE])?;
        let rq_header = rq.header_ptr();

        let shared_count = shared_cap / SHARED_REGION_SIZE;
        let mut shared_regions = Vec::with_capacity(shared_count);
        let mut max_shared_offset = RQ_SIZE;

        for _ in 0..shared_count {
            // SAFETY: the range lies inside the mapping, which outlives the region, and no
            // other region is handed the same bytes.
            let region = unsafe {
                LocalSharedRegion::new(
                    data.add(max_shared_offset),
                    SHARED_REGION_SIZE,
                    max_shared_offset,
                    min_block,
                    max_block,
                )
            }?;

            shared_regions.push(region);
            max_shared_offset += SHARED_REGION_SIZE;
        }

        let shutdown = Arc::new(Shutdown::default());

        let max_shared_payload = shared_regions
            .first()
            .map(|region| region.max_payload())
            .unwrap_or(0);

        let manager = Arc::new(Self {
            file,
            data,
            size: total,
            rq: Mutex::new(rq),
            rq_header,
            shared_regions,
            min_shared_count: pinned_shared / SHARED_REGION_SIZE,
            max_shared_offset,
            max_shared_payload,
            alloc_idx: AtomicU32::new(0),
            min_block,
            max_block,
            dedicated: Mutex::new(Dedicated {
                regions: Vec::new(),
                offset: total,
            }),
            closed: AtomicBool::new(false),
            shutdown: Arc::clone(&shutdown),
            cleanup: Mutex::new(None),
            debug: debug_mode(),
        });

        // The worker holds a weak reference, so dropping the last handle still tears the
        // manager down instead of leaving the thread owning it forever.
        let worker = Arc::downgrade(&manager);
        let handle = thread::spawn(move || cleanup_loop(worker, shutdown));

        *manager.cleanup.lock().unwrap() = Some(handle);

        Ok(manager)
    }

    /// Allocates a buffer with room for `size` payload bytes and returns a pointer to its
    /// header, whose `offset` is the global offset the peer is given.
    ///
    /// Anything a shared region's largest block cannot hold goes to a dedicated region.
    pub fn alloc(&self, size: usize) -> Result<*mut DynShmBufferHeader, ManagerError> {
        if self.is_closed() {
            return Err(ManagerError::Closed);
        }

        if size == 0 {
            return Err(ManagerError::NoSpace);
        }

        // Reclaim what the peer has handed back first.
        if self.rq_pending() > 0 {
            self.drain_rq_inner();
        }

        if size > self.max_shared_payload {
            return self.alloc_dedicated(size);
        }

        // Prefer the pinned regions, spread across them so one region's allocator does not
        // become the contended one.
        if self.min_shared_count > 0 {
            let start = self
                .alloc_idx
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1) as usize;
            for i in 0..self.min_shared_count {
                if let Some(buf) =
                    self.shared_regions[start.wrapping_add(i) % self.min_shared_count].alloc(size)
                {
                    return Ok(buf);
                }
            }
        }

        for region in &self.shared_regions[self.min_shared_count..] {
            if let Some(buf) = region.alloc(size) {
                return Ok(buf);
            }
        }

        Err(ManagerError::NoSpace)
    }

    /// Serves a payload too large for any shared block from a region of its own, reusing a
    /// freed region of about the right size before carving a new one off the end.
    fn alloc_dedicated(&self, size: usize) -> Result<*mut DynShmBufferHeader, ManagerError> {
        let region_size = calc_dedicated_region_size(size);

        if region_size == 0 {
            return Err(ManagerError::NoSpace);
        }

        let mut dedicated = self.dedicated.lock().unwrap();

        // Reuse a freed region of roughly the right size. More than a block of slack goes
        // back to carving a new one, so a large region is not tied up by a small payload.
        for region in &mut dedicated.regions {
            if !region.is_freed() {
                continue;
            }
            if region.size() >= region_size
                && region.size() <= region_size + MAX_BLOCK_SIZE
                && let Some(buf) = region.alloc(size)
            {
                return Ok(buf);
            }
        }

        let new_offset = dedicated
            .offset
            .checked_sub(region_size)
            .ok_or(ManagerError::NoSpace)?;
        if new_offset < self.max_shared_offset {
            return Err(ManagerError::NoSpace);
        }

        // SAFETY: the range lies between the shared area and the previous lowest dedicated
        // region, so it belongs to no other region.
        let mut region = unsafe {
            LocalDedicatedRegion::new(self.data.add(new_offset), region_size, new_offset)
        };
        let buf = region.alloc(size).ok_or(ManagerError::NoSpace)?;
        dedicated.regions.push(region);
        dedicated.offset = new_offset;

        Ok(buf)
    }

    /// The shared region an offset falls in, if it names one at all.
    pub fn get_shared_by_offset(&self, offset: i64) -> Option<&LocalSharedRegion> {
        if self.is_closed() {
            return None;
        }
        self.shared_by_offset(offset)
    }

    /// As [`get_shared_by_offset`](Self::get_shared_by_offset), without the closed check, for
    /// callers that have made it. Regions are equally sized and laid out in order, so the
    /// offset indexes straight into them.
    fn shared_by_offset(&self, offset: i64) -> Option<&LocalSharedRegion> {
        if offset < RQ_SIZE as i64 || offset >= self.max_shared_offset as i64 {
            return None;
        }
        self.shared_regions
            .get((offset as usize - RQ_SIZE) / SHARED_REGION_SIZE)
    }

    /// Runs `visit` over the dedicated region an offset falls in, with the dedicated area
    /// locked, and returns `None` when no region covers the offset.
    ///
    /// The region cannot be handed out directly because pruning may drop it, so borrowing
    /// it is only sound while that lock is held.
    pub fn with_dedicated_by_offset<R>(
        &self,
        offset: i64,
        visit: impl FnOnce(&mut LocalDedicatedRegion) -> R,
    ) -> Option<R> {
        if self.is_closed() {
            return None;
        }

        let mut dedicated = self.dedicated.lock().unwrap();
        let region = dedicated.regions.iter_mut().find(|region| {
            (region.offset() as i64) <= offset && offset < (region.offset() + region.size()) as i64
        })?;

        Some(visit(region))
    }

    /// Borrows the descriptor for the duration of an `SCM_RIGHTS` transfer.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// Length of the segment, which is what the peer is told to expect when it maps it.
    pub fn size(&self) -> usize {
        self.size
    }

    /// The whole segment, which is what the peer maps. Empty once closed.
    ///
    /// Closing does not release the descriptor: the mapping and the fd both live until
    /// the manager is dropped, so a slice the peer is still reading through never turns
    /// into a fault.
    pub fn data(&self) -> &[u8] {
        if self.is_closed() {
            return &[];
        }

        self.file.data()
    }

    /// Releases the peer has queued and this side has not reclaimed yet, read straight from
    /// the ring's header so an allocation can skip the drain when there are none.
    fn rq_pending(&self) -> usize {
        // SAFETY: the header sits at the front of the mapping, which lives as long as `self`,
        // and both fields are atomics, so this reads alongside the consumer and the peer.
        let header = unsafe { &*self.rq_header };
        let head = header.head.load(Ordering::Acquire);
        let tail = header.tail.load(Ordering::Acquire);
        tail.wrapping_sub(head) as usize
    }

    /// Reclaims everything the peer has released since the last drain, returning how many
    /// buffers were freed.
    pub fn drain_rq(&self) -> usize {
        if self.is_closed() {
            return 0;
        }
        self.drain_rq_inner()
    }

    /// The drain itself. Only one thread drains at a time: a thread that cannot take the
    /// lock reports nothing freed rather than waiting.
    fn drain_rq_inner(&self) -> usize {
        let Ok(mut rq) = self.rq.try_lock() else {
            return 0;
        };
        let mut freed = 0;
        while let PopResult::Ready(offset) = rq.try_pop() {
            self.free_at_inner(offset);
            freed += 1;
        }
        freed
    }

    /// Releases a buffer this side allocated, for the case where it never reaches the peer.
    pub fn free_at(&self, offset: i64) {
        if self.is_closed() {
            return;
        }
        self.free_at_inner(offset);
    }

    /// Hands one offset back to whichever region owns it. Offsets come from the peer, so a
    /// release that names no live block is logged and dropped rather than propagated.
    fn free_at_inner(&self, offset: i64) {
        if let Some(region) = self.shared_by_offset(offset) {
            match region.free_at(offset) {
                Err(err) => warn!(offset, %err, "dynshm: shared free failed"),
                Ok(()) if self.debug => debug!(offset, "dynshm: freed shared"),
                Ok(()) => {}
            }
            return;
        }

        // A dedicated region holds one allocation, and it starts where the region does, so
        // anything else is not a buffer this side handed out.
        let mut dedicated = self.dedicated.lock().unwrap();
        let Some(region) = dedicated
            .regions
            .iter_mut()
            .find(|region| region.offset() as i64 == offset)
        else {
            drop(dedicated);
            warn!(offset, "dynshm: free offset not found");
            return;
        };

        let size = region.size();
        let result = region.mark_freed();
        drop(dedicated);

        match result {
            Err(err) => warn!(offset, %err, "dynshm: dedicated free failed"),
            Ok(()) if self.debug => debug!(offset, size, "dynshm: freed dedicated"),
            Ok(()) => {}
        }
    }

    /// Hands back the pages of every region idle long enough, and trims freed dedicated
    /// regions off the end to recover address space. Pinned shared regions are left alone.
    pub fn prune_idle_regions(&self) {
        if self.is_closed() {
            return;
        }

        let now = unix_nanos();
        let max_age = MAX_AGE.as_nanos() as i64;

        for region in &self.shared_regions[self.min_shared_count..] {
            region.punch_if_idle(now, max_age, |offset, size| self.punch(offset, size));
        }

        let mut dedicated = self.dedicated.lock().unwrap();
        for region in &mut dedicated.regions {
            if region.updated() <= 0 || !region.is_freed() {
                continue;
            }
            if now - region.updated() >= max_age {
                self.punch(region.offset(), region.size());
                region.set_updated(DEDICATED_REGION_PUNCHED);
            }
        }

        // Only a suffix of punched regions can be dropped: a live region below them still
        // owns its range, so the dedicated area cannot grow back past it.
        while dedicated
            .regions
            .last()
            .is_some_and(|region| region.updated() == DEDICATED_REGION_PUNCHED)
        {
            dedicated.regions.pop();
        }

        dedicated.offset = match dedicated.regions.last() {
            Some(region) => region.offset(),
            None => self.size,
        };
    }

    /// Hands a range's pages back to the kernel.
    fn punch(&self, offset: usize, size: usize) {
        if let Err(err) = self.file.punch_hole(offset, size) {
            warn!(offset, size, %err, "dynshm: releasing region pages failed");
        }
    }

    /// A snapshot of what the regions hold, all zeroes once closed.
    pub fn stats(&self) -> Stats {
        let mut stats = Stats::default();
        if self.is_closed() {
            return stats;
        }

        stats.rq_pending = self.rq.try_lock().map(|rq| rq.len()).unwrap_or(0);
        stats.shared_regions = self.shared_regions.len();

        for region in &self.shared_regions {
            stats.shared_bytes += region.size();
            stats.shared_free_bytes += region.available();
            if region.alloc_count() > 0 {
                stats.shared_active += 1;
            }
        }

        let dedicated = self.dedicated.lock().unwrap();
        stats.dedicated_regions = dedicated.regions.len();

        for region in &dedicated.regions {
            stats.dedicated_bytes += region.size();
            if !region.is_freed() {
                stats.dedicated_active += 1;
                stats.dedicated_used += region.size();
            }
        }

        stats
    }

    /// Whether [`close_all`](Self::close_all) has run.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Stops the cleanup thread and refuses every subsequent operation. The mapping stays
    /// until the manager itself is dropped.
    pub fn close_all(&self) {
        self.shutdown.signal();

        // Joined before the flag flips, because the worker's last pass still has to get past
        // the closed check to do its final drain.
        let handle = self.cleanup.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }

        self.closed.store(true, Ordering::Release);
    }
}

impl Drop for LocalManager {
    fn drop(&mut self) {
        self.shutdown.signal();
    }
}

/// Drains releases and prunes idle regions once per [`PRUNE_INTERVAL`], and once more on
/// the way out so a shutdown does not leave a queue's worth of releases unreclaimed.
fn cleanup_loop(manager: Weak<LocalManager>, shutdown: Arc<Shutdown>) {
    loop {
        let running = shutdown.wait(PRUNE_INTERVAL);
        let Some(manager) = manager.upgrade() else {
            return;
        };
        manager.drain_rq();
        manager.prune_idle_regions();
        if !running {
            return;
        }
    }
}

impl std::fmt::Debug for LocalManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalManager")
            .field("file", &self.file)
            .field("shared_regions", &self.shared_regions.len())
            .field("min_shared_count", &self.min_shared_count)
            .field("min_block", &self.min_block)
            .field("max_block", &self.max_block)
            .finish()
    }
}

/// The peer's segment, mapped here for reading payloads out of.
pub struct RemoteManager {
    /// The peer's segment, mapped from the descriptor it sent.
    file: MemFile,
    /// Start of that mapping.
    data: *mut u8,
    /// Length of the mapping, which every offset is bounds checked against.
    size: usize,
    /// The peer's release queue, and the only part of the segment written from this side.
    rq: ReleaseQueue,
    /// Offsets the owning side has not been told about yet, because the queue was full.
    pending_release: Mutex<Vec<i64>>,
    /// Set by close; every operation checks it first. The mapping outlives the flag, so an
    /// operation that passed the check just before close still touches valid memory.
    closed: AtomicBool,
}

unsafe impl Send for RemoteManager {}
unsafe impl Sync for RemoteManager {}

impl RemoteManager {
    /// Maps a segment received over `SCM_RIGHTS`.
    pub fn map(fd: OwnedFd, advertised: u32) -> Result<Arc<Self>, anyhow::Error> {
        let expected = if advertised == 0 {
            DYNSHM_TOTAL_SIZE
        } else {
            advertised as usize
        };

        let mut file = MemFile::map(fd)?;
        if file.size() != expected || file.size() < RQ_SIZE + DYNSHM_BUFFER_HEADER_SIZE {
            return Err(anyhow!(
                "unexpected dynshm size: got {} want {}",
                file.size(),
                expected
            ));
        }

        let size = file.size();
        let data = file.data_mut().as_mut_ptr();
        let rq = map_rq(&mut file.data_mut()[..RQ_SIZE])?;

        Ok(Arc::new(Self {
            file,
            data,
            size,
            rq,
            pending_release: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        }))
    }

    /// Length of the mapped segment.
    pub fn size(&self) -> usize {
        self.size
    }

    /// The whole mapped segment. Unlike [`LocalManager::data`] this keeps returning the
    /// mapping after a close, because closing this side only stops it writing releases.
    pub fn data(&self) -> &[u8] {
        self.file.data()
    }

    /// The payload at a global offset, with the offset of the next block of the same
    /// message. `None` for anything that does not describe a live buffer.
    pub fn read(&self, global_offset: i64) -> Option<(&[u8], i64)> {
        if self.is_closed() {
            return None;
        }
        self.read_inner(global_offset)
    }

    /// The read itself, without the closed check, so a chain walk pays for it once rather
    /// than per node.
    fn read_inner(&self, global_offset: i64) -> Option<(&[u8], i64)> {
        if global_offset < RQ_SIZE as i64
            || global_offset > self.size as i64 - DYNSHM_BUFFER_HEADER_SIZE as i64
        {
            return None;
        }

        // SAFETY: the offset lies inside the mapping with room for a header behind it.
        let header = unsafe {
            ptr::read_unaligned(
                self.data
                    .add(global_offset as usize)
                    .cast::<DynShmBufferHeader>(),
            )
        };

        // A block that does not name its own offset was never allocated here, or was freed
        // and the sender is working from a stale offset.
        if header.offset != global_offset || header.len > header.cap {
            return None;
        }

        // The peer chose `cap`, so it is checked against the mapping rather than trusted: a
        // corrupt one would otherwise read off the end.
        let remaining = self.size - global_offset as usize - DYNSHM_BUFFER_HEADER_SIZE;
        if header.cap as usize > remaining {
            return None;
        }

        if header.next != 0
            && (header.next < RQ_SIZE as i64
                || header.next > self.size as i64 - DYNSHM_BUFFER_HEADER_SIZE as i64
                || header.next == global_offset)
        {
            return None;
        }

        // SAFETY: the header was validated against the mapping just above, so `len` bytes
        // behind it lie inside it.
        let data = unsafe {
            slice::from_raw_parts(
                self.data
                    .add(global_offset as usize + DYNSHM_BUFFER_HEADER_SIZE),
                header.len as usize,
            )
        };

        Some((data, header.next))
    }

    /// Tells the owning side that a buffer is finished with.
    pub fn free(&self, global_offset: i64) -> Result<(), ManagerError> {
        if self.is_closed() {
            return Err(ManagerError::Closed);
        }

        if global_offset < RQ_SIZE as i64
            || global_offset > self.size as i64 - DYNSHM_BUFFER_HEADER_SIZE as i64
        {
            return Err(ManagerError::OutOfBounds);
        }

        // SAFETY: the offset was just bounds checked against the mapping.
        let header = unsafe {
            ptr::read_unaligned(
                self.data
                    .add(global_offset as usize)
                    .cast::<DynShmBufferHeader>(),
            )
        };
        if header.offset != global_offset {
            return Err(ManagerError::NotAllocated);
        }

        let mut pending = self.pending_release.lock().unwrap();
        pending.push(global_offset);

        Self::flush_releases(&self.rq, &mut pending)
    }

    /// Walks a message's chain and schedules every block in it for release.
    pub fn free_chain(&self, root: i64) -> Result<(), ManagerError> {
        if self.is_closed() {
            return Err(ManagerError::Closed);
        }

        let mut offsets = Vec::new();
        let mut visited = AHashSet::new();
        let mut offset = root;

        while offset > 0 {
            if offsets.len() >= MAX_CHAIN_NODES {
                return Err(ManagerError::OutOfBounds);
            }
            // A repeat means the chain loops back, so walking it would never end.
            if !visited.insert(offset) {
                return Err(ManagerError::InvalidOffset);
            }
            let (_, next) = self.read_inner(offset).ok_or(ManagerError::InvalidOffset)?;
            offsets.push(offset);
            offset = next;
        }

        let mut pending = self.pending_release.lock().unwrap();
        pending.extend_from_slice(&offsets);

        Self::flush_releases(&self.rq, &mut pending)
    }

    /// Pushes as much of `pending` into the release queue as fits, dropping what got through
    /// from the front and keeping the rest for a later retry. Reports
    /// [`ReleaseQueueFull`](ManagerError::ReleaseQueueFull) whenever anything is left over.
    fn flush_releases(rq: &ReleaseQueue, pending: &mut Vec<i64>) -> Result<(), ManagerError> {
        let mut pushed = 0;
        while pushed < pending.len() && rq.try_push(pending[pushed]) {
            pushed += 1;
        }

        pending.drain(..pushed);
        if pending.is_empty() {
            Ok(())
        } else {
            Err(ManagerError::ReleaseQueueFull)
        }
    }

    /// Retries offsets held back when the release queue was full.
    pub fn retry_pending_releases(&self) -> Result<(), ManagerError> {
        if self.is_closed() {
            return Err(ManagerError::Closed);
        }
        let mut pending = self.pending_release.lock().unwrap();
        Self::flush_releases(&self.rq, &mut pending)
    }

    /// Whether [`close_all`](Self::close_all) has run.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Refuses every subsequent operation and drops anything still waiting to be released.
    /// The mapping itself lives until the manager is dropped. A release that made it past
    /// the check as the flag flipped leaves its offset behind, which is harmless: nothing
    /// retries after close.
    pub fn close_all(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.pending_release.lock().unwrap().clear();
    }
}

impl std::fmt::Debug for RemoteManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteManager")
            .field("file", &self.file)
            .field("size", &self.size)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pinned_region_count_reads_a_setting_and_clamps_it() {
        const MAX: usize = 64;

        for (raw, expected) in [
            (None, DEFAULT_PINNED_REGION_NUM),
            (Some(""), DEFAULT_PINNED_REGION_NUM),
            (Some("8"), 8),
            (Some("invalid"), DEFAULT_PINNED_REGION_NUM),
            (Some("0"), 0),
            (Some("-1"), DEFAULT_PINNED_REGION_NUM),
            (Some("100"), MAX),
        ] {
            assert_eq!(expected, pinned_region_num_from(raw, MAX), "for {raw:?}");
        }

        // Clamping is against what the segment can hold, so a small segment wins over a
        // larger request.
        assert_eq!(1, pinned_region_num_from(Some("8"), 1));
        assert_eq!(0, pinned_region_num_from(None, 0));
    }

    #[test]
    fn dedicated_region_size_rounds_up_to_whole_blocks() {
        // A payload plus its header, rounded up to whole allocator blocks.
        for (payload, blocks) in [
            (MAX_BLOCK_SIZE - DYNSHM_BUFFER_HEADER_SIZE, 1),
            (MAX_BLOCK_SIZE - DYNSHM_BUFFER_HEADER_SIZE + 1, 2),
            (1 << 20, 5),
            (2 << 20, 9),
            (3 << 20, 13),
            (5 << 20, 21),
            (10 << 20, 41),
        ] {
            assert_eq!(
                blocks * MAX_BLOCK_SIZE,
                calc_dedicated_region_size(payload),
                "payload {payload}"
            );
        }
    }

    #[test]
    fn dedicated_region_size_refuses_what_the_wire_cannot_describe() {
        assert_eq!(0, calc_dedicated_region_size(1 << 31));
        assert_eq!(0, calc_dedicated_region_size(usize::MAX));
    }

    /// A header with room behind it, standing in for a block inside a region.
    struct Block {
        words: Vec<u64>,
    }

    impl Block {
        fn new(cap: u32) -> Self {
            let mut block = Self {
                words: vec![0; (DYNSHM_BUFFER_HEADER_SIZE + cap as usize).div_ceil(8)],
            };

            // SAFETY: the vector is at least a header long and `u64`-aligned.
            unsafe { (*block.header()).cap = cap };

            block
        }

        fn header(&mut self) -> *mut DynShmBufferHeader {
            self.words.as_mut_ptr().cast()
        }
    }

    #[test]
    fn buffer_header_reserves_and_appends_within_capacity() {
        let mut block = Block::new(32);
        let header = block.header();

        // SAFETY: `header` points at a block-sized allocation, with the whole block's
        // provenance, that outlives every borrow taken out of it below.
        unsafe {
            assert!(DynShmBufferHeader::data(header).is_empty());

            let room = DynShmBufferHeader::reserve(header, 8).unwrap();
            assert_eq!(8, room.len());
            room.copy_from_slice(b"reserved");
            assert_eq!(8, (*header).len);
            assert_eq!(b"reserved", DynShmBufferHeader::data(header));

            // Appending lands after what is already there.
            assert!(DynShmBufferHeader::append(header, b"-more"));
            assert_eq!(b"reserved-more", DynShmBufferHeader::data(header));
            assert_eq!(13, (*header).len);

            // And neither writes past the block.
            assert!(DynShmBufferHeader::reserve(header, 20).is_none());
            assert!(!DynShmBufferHeader::append(header, &[0; 20]));
            assert_eq!(
                13,
                (*header).len,
                "a refused reserve must not move the length"
            );

            assert!(DynShmBufferHeader::append(header, &[0xAB; 19]));
            assert_eq!(32, (*header).len);
            assert!(!DynShmBufferHeader::append(header, b"x"));
        }
    }

    #[test]
    fn pending_releases_are_kept_when_the_queue_fills() {
        let mut memory = vec![0u8; RQ_SIZE];
        let mut rq = new_rq(&mut memory).unwrap();

        let mut pending: Vec<i64> = (1..=4).collect();
        assert_eq!(Ok(()), RemoteManager::flush_releases(&rq, &mut pending));
        assert!(pending.is_empty());
        assert_eq!(4, rq.len());

        // Fill the queue, then offer more than it can take.
        while rq.try_push(0) {}

        let mut pending: Vec<i64> = (100..110).collect();
        assert_eq!(
            Err(ManagerError::ReleaseQueueFull),
            RemoteManager::flush_releases(&rq, &mut pending)
        );
        assert_eq!(10, pending.len(), "nothing fit, so nothing was consumed");

        // Draining lets exactly as many through as there is now room for.
        for _ in 0..3 {
            rq.pop().unwrap();
        }

        assert_eq!(
            Err(ManagerError::ReleaseQueueFull),
            RemoteManager::flush_releases(&rq, &mut pending)
        );
        assert_eq!(
            (103..110).collect::<Vec<_>>(),
            pending,
            "the offsets that fit are the ones dropped from the front"
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod manager_tests {
    use std::{os::fd::OwnedFd, thread};

    use super::*;

    const TEST_SIZE: usize = 128 << 20;

    fn manager() -> Arc<LocalManager> {
        LocalManager::new_with_size(
            TEST_SIZE,
            TEST_SIZE / 100,
            TEST_SIZE / 4,
            MIN_BLOCK_SIZE,
            MAX_BLOCK_SIZE,
        )
        .unwrap()
    }

    fn peer(manager: &LocalManager) -> Arc<RemoteManager> {
        let fd: OwnedFd = nix::unistd::dup(manager.fd()).unwrap();

        RemoteManager::map(fd, TEST_SIZE as u32).unwrap()
    }

    fn header(buf: *mut DynShmBufferHeader) -> DynShmBufferHeader {
        unsafe { *buf }
    }

    #[test]
    fn shared_allocs_come_from_the_region_after_the_release_queue() {
        let manager = manager();

        let buf = manager.alloc(1024).unwrap();
        unsafe { assert_eq!(1024, DynShmBufferHeader::data(buf).len()) };

        let region = manager
            .get_shared_by_offset(header(buf).offset)
            .expect("offset should name a shared region");

        assert_eq!(RQ_SIZE, region.offset());
        assert_eq!(SHARED_REGION_SIZE, region.size());

        unsafe {
            DynShmBufferHeader::data_mut(buf)[..5].copy_from_slice(b"hello");
            assert_eq!(b"hello", &DynShmBufferHeader::data(buf)[..5]);
        }

        // A second allocation of a size the same region can serve stays in that region.
        let second = manager.alloc(2048).unwrap();
        let second_region = manager.get_shared_by_offset(header(second).offset).unwrap();

        assert_eq!(region.offset(), second_region.offset());

        for buf in [buf, second] {
            let offset = header(buf).offset;
            assert!(offset >= region.offset() as i64);
            assert!(offset < (region.offset() + region.size()) as i64);
        }

        manager.close_all();
    }

    #[test]
    fn allocs_larger_than_a_block_get_a_dedicated_region() {
        let manager = manager();

        const SIZE: usize = 3 << 20;
        let buf = manager.alloc(SIZE).unwrap();
        unsafe { assert_eq!(SIZE, DynShmBufferHeader::data(buf).len()) };

        let region_size = manager
            .with_dedicated_by_offset(header(buf).offset, |region| region.size())
            .expect("offset should name a dedicated region");

        assert_eq!(calc_dedicated_region_size(SIZE), region_size);
        assert!(manager.get_shared_by_offset(header(buf).offset).is_none());

        unsafe {
            DynShmBufferHeader::data_mut(buf)[..10].copy_from_slice(b"large data");
            assert_eq!(b"large data", &DynShmBufferHeader::data(buf)[..10]);
        }

        manager.close_all();
    }

    #[test]
    fn a_dedicated_region_clears_metadata_left_in_its_range() {
        let manager = manager();

        const SIZE: usize = 3 << 20;
        let new_offset =
            manager.dedicated.lock().unwrap().offset - calc_dedicated_region_size(SIZE);

        // Stand in for whatever a previous tenant of this range left behind.
        unsafe {
            ptr::write_bytes(
                manager.data.add(new_offset),
                0xFF,
                DYNSHM_BUFFER_HEADER_SIZE,
            );
        }

        let buf = header(manager.alloc(SIZE).unwrap());

        assert_eq!(new_offset as i64, buf.offset);
        assert_eq!(SIZE as u32, buf.len);
        assert_eq!(0, buf.next);

        manager.close_all();
    }

    #[test]
    fn a_freed_dedicated_region_is_reused_before_a_new_one_is_carved() {
        let manager = manager();

        const SIZE: usize = 3 << 20;
        let first = header(manager.alloc(SIZE).unwrap()).offset;

        manager
            .with_dedicated_by_offset(first, |region| region.mark_freed())
            .unwrap()
            .unwrap();

        let second = header(manager.alloc(SIZE).unwrap()).offset;

        assert_eq!(first, second, "the freed region should have been reused");
        assert_eq!(1, manager.stats().dedicated_regions);

        manager.close_all();
    }

    #[test]
    fn offsets_outside_the_shared_area_name_no_region() {
        let manager = manager();

        // The release queue owns everything below RQ_SIZE.
        assert!(manager.get_shared_by_offset(0).is_none());
        assert!(manager.get_shared_by_offset(RQ_SIZE as i64 - 1).is_none());
        assert!(manager.get_shared_by_offset(RQ_SIZE as i64).is_some());

        let last = manager.max_shared_offset as i64;
        assert!(manager.get_shared_by_offset(last - 1).is_some());
        assert!(manager.get_shared_by_offset(last).is_none());
        assert!(manager.get_shared_by_offset(TEST_SIZE as i64).is_none());

        manager.close_all();
    }

    #[test]
    fn the_peer_reads_what_this_side_wrote_and_releases_it_back() {
        let manager = manager();

        let buf = manager.alloc(1024).unwrap();
        // Read the offset out first: freeing zeroes it.
        let offset = header(buf).offset;
        unsafe { DynShmBufferHeader::data_mut(buf)[..9].copy_from_slice(b"test data") };

        let peer = peer(&manager);

        let (data, next) = peer.read(offset).unwrap();
        assert_eq!(b"test data", &data[..9]);
        assert_eq!(0, next);

        peer.free(offset).unwrap();

        assert_eq!(1, manager.drain_rq());
        assert_eq!(
            0,
            manager.get_shared_by_offset(offset).unwrap().alloc_count()
        );

        manager.close_all();
    }

    #[test]
    fn the_peer_refuses_a_header_that_does_not_describe_a_live_block() {
        let manager = manager();

        let buf = manager.alloc(16).unwrap();
        let offset = header(buf).offset;

        let peer = peer(&manager);
        assert!(peer.read(offset).is_some());

        // A capacity running past the end of the segment would read off the mapping.
        unsafe { (*buf).cap = u32::MAX };
        assert!(peer.read(offset).is_none());

        // As would a length past the block's own capacity.
        unsafe {
            (*buf).cap = 16;
            (*buf).len = 32;
        }
        assert!(peer.read(offset).is_none());

        // A block that does not name its own offset was never allocated.
        unsafe {
            (*buf).len = 16;
            (*buf).offset = offset + 8;
        }
        assert!(peer.read(offset).is_none());
        assert_eq!(Err(ManagerError::NotAllocated), peer.free(offset));

        // And neither end of the segment is readable.
        assert!(peer.read(0).is_none());
        assert!(peer.read(RQ_SIZE as i64 - 1).is_none());
        assert!(peer.read(TEST_SIZE as i64).is_none());
        assert_eq!(Err(ManagerError::OutOfBounds), peer.free(0));

        manager.close_all();
    }

    #[test]
    fn the_peer_releases_a_whole_chain_at_once() {
        let manager = manager();

        let first = manager.alloc(32).unwrap();
        let second = manager.alloc(32).unwrap();
        let third = manager.alloc(32).unwrap();

        let root = header(first).offset;

        unsafe {
            (*first).next = header(second).offset;
            (*second).next = header(third).offset;
        }

        let region_allocs = manager.get_shared_by_offset(root).unwrap();
        assert_eq!(3, region_allocs.alloc_count());

        let peer = peer(&manager);
        peer.free_chain(root).unwrap();

        assert_eq!(3, manager.drain_rq());
        assert_eq!(0, region_allocs.alloc_count());

        manager.close_all();
    }

    #[test]
    fn a_looping_chain_frees_nothing() {
        let manager = manager();

        let first = manager.alloc(32).unwrap();
        let second = manager.alloc(32).unwrap();
        let root = header(first).offset;

        unsafe {
            (*first).next = header(second).offset;
            (*second).next = root;
        }

        let peer = peer(&manager);

        assert_eq!(Err(ManagerError::InvalidOffset), peer.free_chain(root));
        assert_eq!(0, manager.drain_rq(), "a rejected chain releases nothing");
        assert_eq!(2, manager.get_shared_by_offset(root).unwrap().alloc_count());

        // A chain whose tail does not check out is refused whole, too.
        unsafe { (*second).next = TEST_SIZE as i64 - 8 };
        assert_eq!(Err(ManagerError::InvalidOffset), peer.free_chain(root));
        assert_eq!(0, manager.drain_rq());

        manager.close_all();
    }

    #[test]
    fn a_full_release_queue_holds_offsets_instead_of_blocking() {
        let manager = manager();

        let buf = manager.alloc(16).unwrap();
        let offset = header(buf).offset;

        let peer = peer(&manager);

        while peer.rq.try_push(0) {}

        assert_eq!(Err(ManagerError::ReleaseQueueFull), peer.free(offset));

        // Draining the queue lets the held offset through on the retry.
        while manager.rq.lock().unwrap().pop().is_some() {}

        assert_eq!(Ok(()), peer.retry_pending_releases());
        assert_eq!(1, manager.drain_rq());
        assert_eq!(
            0,
            manager.get_shared_by_offset(offset).unwrap().alloc_count()
        );

        manager.close_all();
    }

    #[test]
    fn draining_releases_only_the_buffers_the_peer_named() {
        let manager = manager();

        let first = header(manager.alloc(100).unwrap()).offset;
        let second = header(manager.alloc(100).unwrap()).offset;
        let third = header(manager.alloc(100).unwrap()).offset;

        let peer = peer(&manager);
        peer.free(first).unwrap();
        peer.free(third).unwrap();

        assert_eq!(2, manager.drain_rq());
        assert_eq!(
            1,
            manager.get_shared_by_offset(second).unwrap().alloc_count()
        );
        assert_eq!(0, manager.drain_rq(), "a second drain has nothing to do");

        manager.close_all();
    }

    #[test]
    fn draining_releases_dedicated_regions_too() {
        let manager = manager();

        let offset = header(manager.alloc(2 << 20).unwrap()).offset;

        assert_eq!(
            Some(false),
            manager.with_dedicated_by_offset(offset, |region| region.is_freed())
        );

        let peer = peer(&manager);
        peer.free(offset).unwrap();

        assert_eq!(1, manager.drain_rq());
        assert_eq!(
            Some(true),
            manager.with_dedicated_by_offset(offset, |region| region.is_freed())
        );

        manager.close_all();
    }

    #[test]
    fn pruning_reclaims_an_idle_dedicated_region_and_its_address_space() {
        let manager = manager();

        let offset = header(manager.alloc(2 << 20).unwrap()).offset;

        let size = manager
            .with_dedicated_by_offset(offset, |region| {
                region.mark_freed().unwrap();
                // Age it past the idle threshold so this pass picks it up.
                region.set_updated(unix_nanos() - MAX_AGE.as_nanos() as i64 - 1);
                region.size()
            })
            .unwrap();

        let dedicated_offset = manager.dedicated.lock().unwrap().offset;

        manager.prune_idle_regions();

        assert!(manager.with_dedicated_by_offset(offset, |_| ()).is_none());
        assert_eq!(
            dedicated_offset + size,
            manager.dedicated.lock().unwrap().offset,
            "trimming the region should give its address space back"
        );

        manager.close_all();
    }

    #[test]
    fn pruning_leaves_a_region_that_is_still_in_use() {
        let manager = manager();

        let offset = header(manager.alloc(2 << 20).unwrap()).offset;

        // Old, but never freed.
        manager.with_dedicated_by_offset(offset, |region| {
            region.set_updated(unix_nanos() - MAX_AGE.as_nanos() as i64 - 1);
        });

        manager.prune_idle_regions();

        assert_eq!(
            Some(false),
            manager.with_dedicated_by_offset(offset, |region| region.is_freed())
        );

        manager.close_all();
    }

    #[test]
    fn stats_track_what_each_kind_of_region_holds() {
        let manager = manager();

        let initial = manager.stats();
        assert!(initial.shared_regions > 0);
        assert_eq!(0, initial.shared_active);
        assert!(initial.shared_free_bytes > 0);
        assert_eq!(0, initial.dedicated_regions);

        let shared = header(manager.alloc(1024).unwrap()).offset;

        let after_shared = manager.stats();
        assert_eq!(1, after_shared.shared_active);
        assert!(after_shared.shared_free_bytes < initial.shared_free_bytes);

        let dedicated = header(manager.alloc(2 << 20).unwrap()).offset;

        let after_dedicated = manager.stats();
        assert_eq!(1, after_dedicated.dedicated_regions);
        assert_eq!(1, after_dedicated.dedicated_active);
        assert_eq!(
            after_dedicated.dedicated_bytes,
            after_dedicated.dedicated_used
        );

        manager
            .with_dedicated_by_offset(dedicated, |region| region.mark_freed())
            .unwrap()
            .unwrap();

        let freed = manager.stats();
        assert_eq!(1, freed.dedicated_regions, "the region is kept for reuse");
        assert_eq!(0, freed.dedicated_active);
        assert_eq!(0, freed.dedicated_used);

        manager.free_at(shared);

        let end = manager.stats();
        assert_eq!(0, end.shared_active);
        // Coalescing is lazy, so the space need not come back in full straight away.
        assert!(end.shared_free_bytes > initial.shared_free_bytes - 1024);

        manager.close_all();
    }

    #[test]
    fn stats_report_releases_still_waiting_to_be_drained() {
        let manager = manager();

        // Stop the cleanup thread so it cannot empty the queue underneath the assertions.
        manager.shutdown.signal();
        if let Some(cleanup) = manager.cleanup.lock().unwrap().take() {
            cleanup.join().unwrap();
        }

        let offset = header(manager.alloc(1024).unwrap()).offset;

        // Stand in for the peer reporting the buffer back.
        assert!(manager.rq.lock().unwrap().try_push(offset));
        assert_eq!(1, manager.stats().rq_pending);

        // A drain that cannot take the queue reports nothing freed rather than waiting.
        let draining = manager.rq.lock().unwrap();
        assert_eq!(0, manager.drain_rq(), "a drain in flight is not waited on");
        drop(draining);

        assert_eq!(1, manager.drain_rq());
        assert_eq!(0, manager.stats().rq_pending);

        manager.close_all();
    }

    #[test]
    fn a_dedicated_buffer_can_be_extended_into_the_rest_of_its_region() {
        let manager = manager();

        const SIZE: usize = 1 << 20;
        let buf = manager.alloc(SIZE).unwrap();
        let offset = header(buf).offset;

        unsafe { assert_eq!(SIZE, DynShmBufferHeader::data(buf).len()) };
        assert_eq!(
            (calc_dedicated_region_size(SIZE) - DYNSHM_BUFFER_HEADER_SIZE) as u32,
            header(buf).cap
        );

        let extra = (header(buf).cap - header(buf).len) as usize / 2;
        assert_eq!(extra, unsafe {
            DynShmBufferHeader::reserve(buf, extra).unwrap().len()
        });

        // The peer sees the new length, because the length lives in shared memory.
        let peer = peer(&manager);
        let (data, _) = peer.read(offset).unwrap();
        assert_eq!(SIZE + extra, data.len());

        manager.close_all();
    }

    #[test]
    fn closing_refuses_later_work_but_keeps_the_mapping() {
        let manager = manager();

        let buf = manager.alloc(16).unwrap();
        unsafe { DynShmBufferHeader::data_mut(buf)[..12].copy_from_slice(b"before-close") };

        manager.close_all();
        manager.close_all();

        // The mapping outlives the close, so a buffer handed out earlier still reads and
        // writes.
        unsafe {
            assert_eq!(b"before-close", &DynShmBufferHeader::data(buf)[..12]);
            DynShmBufferHeader::data_mut(buf)[0] = b'B';
            assert_eq!(b'B', DynShmBufferHeader::data(buf)[0]);
        }

        assert!(manager.is_closed());
        assert!(manager.data().is_empty());
        assert_eq!(Stats::default(), manager.stats());
        assert_eq!(Err(ManagerError::Closed), manager.alloc(1));
        assert!(manager.get_shared_by_offset(header(buf).offset).is_none());
        assert_eq!(0, manager.drain_rq());
    }

    #[test]
    fn closing_while_another_thread_allocates_ends_that_thread() {
        let manager = manager();

        let allocating = Arc::clone(&manager);
        let (started, wait) = std::sync::mpsc::channel();

        let worker = thread::spawn(move || {
            started.send(()).unwrap();

            loop {
                match allocating.alloc(128) {
                    Ok(buf) => allocating.free_at(header(buf).offset),
                    Err(ManagerError::Closed) => return Ok(()),
                    Err(err) => return Err(err),
                }
            }
        });

        wait.recv().unwrap();
        manager.close_all();

        assert_eq!(Ok(()), worker.join().unwrap());
    }

    #[test]
    fn the_peer_refuses_work_once_closed() {
        let manager = manager();

        let offset = header(manager.alloc(16).unwrap()).offset;
        let peer = peer(&manager);

        peer.close_all();
        peer.close_all();

        assert!(peer.is_closed());
        assert!(peer.read(offset).is_none());
        assert_eq!(Err(ManagerError::Closed), peer.free(offset));
        assert_eq!(Err(ManagerError::Closed), peer.free_chain(offset));
        assert_eq!(Err(ManagerError::Closed), peer.retry_pending_releases());

        manager.close_all();
    }

    #[test]
    fn allocation_moves_to_a_dedicated_region_at_the_block_boundary() {
        let manager = manager();

        let largest_shared = manager.max_shared_payload;

        assert!(largest_shared < MAX_BLOCK_SIZE - DYNSHM_BUFFER_HEADER_SIZE);

        let buf = manager.alloc(largest_shared).expect("a block still fits");
        let offset = header(buf).offset;
        assert!(manager.get_shared_by_offset(offset).is_some());
        assert!(manager.with_dedicated_by_offset(offset, |_| ()).is_none());
        unsafe { assert_eq!(largest_shared, DynShmBufferHeader::data(buf).len()) };

        for size in [
            largest_shared + 1,
            MAX_BLOCK_SIZE - DYNSHM_BUFFER_HEADER_SIZE,
            MAX_BLOCK_SIZE,
            MAX_BLOCK_SIZE + 1,
        ] {
            let buf = manager.alloc(size).expect("a dedicated region is carved");
            let offset = header(buf).offset;
            assert!(
                manager.get_shared_by_offset(offset).is_none(),
                "{size} should not come from a shared region"
            );
            assert!(manager.with_dedicated_by_offset(offset, |_| ()).is_some());
        }

        manager.close_all();
    }

    #[test]
    fn allocation_refuses_sizes_it_cannot_represent() {
        let manager = manager();

        assert_eq!(Err(ManagerError::NoSpace), manager.alloc(0));
        assert_eq!(Err(ManagerError::NoSpace), manager.alloc(usize::MAX));
        assert_eq!(Err(ManagerError::NoSpace), manager.alloc(1 << 31));

        // The refusals leave the allocator usable.
        assert!(manager.alloc(1024).is_ok());

        manager.close_all();
    }

    #[test]
    fn a_full_shared_area_refuses_and_then_recovers() {
        let manager = manager();

        let size = manager.max_shared_payload;
        let mut held = Vec::new();
        loop {
            match manager.alloc(size) {
                Ok(buf) => held.push(header(buf).offset),
                Err(err) => {
                    assert_eq!(ManagerError::NoSpace, err);
                    break;
                }
            }
            assert!(
                held.len() < 4096,
                "the shared area should have run out long before here"
            );
        }
        assert!(!held.is_empty(), "at least one block must have fitted");

        let returned = held.pop().unwrap();
        manager.free_at(returned);

        // Which block comes back is the free list's business; that one does is the point.
        let buf = manager
            .alloc(size)
            .expect("a returned block is available again");
        assert!(manager.get_shared_by_offset(header(buf).offset).is_some());

        manager.close_all();
    }

    #[test]
    fn a_full_dedicated_area_refuses_and_then_recovers() {
        let manager = manager();

        const SIZE: usize = 3 << 20;
        let mut held = Vec::new();
        loop {
            match manager.alloc(SIZE) {
                Ok(buf) => held.push(header(buf).offset),
                Err(err) => {
                    assert_eq!(ManagerError::NoSpace, err);
                    break;
                }
            }
            assert!(held.len() < 1024, "the segment should have run out by here");
        }
        assert!(!held.is_empty());

        let returned = held.pop().unwrap();
        manager.free_at(returned);
        assert_eq!(
            Some(true),
            manager.with_dedicated_by_offset(returned, |region| region.is_freed())
        );

        // A freed region of a compatible size is reused rather than carved anew.
        let before = manager.stats().dedicated_regions;
        let buf = manager.alloc(SIZE).expect("the freed region is reused");
        assert_eq!(returned, header(buf).offset);
        assert_eq!(before, manager.stats().dedicated_regions);

        manager.close_all();
    }

    #[test]
    fn the_peer_refuses_frees_that_name_no_live_block() {
        let manager = manager();
        let peer = peer(&manager);

        let buf = manager.alloc(1024).unwrap();
        let offset = header(buf).offset;

        // Below the segment's data, inside the release queue, and past the end.
        assert_eq!(Err(ManagerError::OutOfBounds), peer.free(0));
        assert_eq!(
            Err(ManagerError::OutOfBounds),
            peer.free(RQ_SIZE as i64 - 1)
        );
        assert_eq!(Err(ManagerError::OutOfBounds), peer.free(-1));
        assert_eq!(Err(ManagerError::OutOfBounds), peer.free(TEST_SIZE as i64));

        // Inside the segment, but not where a block starts.
        assert_eq!(Err(ManagerError::NotAllocated), peer.free(offset + 8));
        assert_eq!(
            Err(ManagerError::NotAllocated),
            peer.free(offset + DYNSHM_BUFFER_HEADER_SIZE as i64)
        );

        // The first release is accepted; the second names a block that no longer exists.
        assert_eq!(Ok(()), peer.free(offset));
        assert_eq!(1, manager.drain_rq());
        assert_eq!(Err(ManagerError::NotAllocated), peer.free(offset));
        assert_eq!(0, manager.drain_rq());

        // A refused free leaves the allocator able to hand the block out again.
        assert!(manager.alloc(1024).is_ok());

        manager.close_all();
    }

    #[test]
    fn offsets_outside_the_dedicated_areas_name_no_dedicated_region() {
        let manager = manager();

        assert!(manager.with_dedicated_by_offset(0, |_| ()).is_none());
        assert!(manager.with_dedicated_by_offset(-1, |_| ()).is_none());
        assert!(
            manager
                .with_dedicated_by_offset(TEST_SIZE as i64, |_| ())
                .is_none()
        );

        // A live shared block is in the segment but names no dedicated region either.
        let buf = manager.alloc(1024).unwrap();
        assert!(
            manager
                .with_dedicated_by_offset(header(buf).offset, |_| ())
                .is_none()
        );

        manager.close_all();
    }

    #[test]
    fn many_threads_allocating_and_freeing_leave_every_region_empty() {
        const THREADS: usize = 8;
        const ROUNDS: usize = 250;

        let manager = manager();

        let workers: Vec<_> = (0..THREADS)
            .map(|id| {
                let manager = manager.clone();
                thread::spawn(move || {
                    let fill = id as u8;
                    for round in 0..ROUNDS {
                        let size = 64 + (id * 97 + round * 13) % 4096;
                        let buf = manager.alloc(size).expect("small blocks always fit");

                        // SAFETY: this thread is the block's only owner until it frees it.
                        let offset = unsafe {
                            DynShmBufferHeader::data_mut(buf).fill(fill);
                            assert!(
                                DynShmBufferHeader::data(buf)
                                    .iter()
                                    .all(|&byte| byte == fill),
                                "another thread wrote into this block"
                            );
                            (*buf).offset
                        };
                        manager.free_at(offset);
                    }
                })
            })
            .collect();

        for worker in workers {
            worker.join().expect("no thread may panic");
        }

        let stats = manager.stats();
        assert_eq!(
            0, stats.shared_active,
            "every shared region should be empty"
        );
        assert_eq!(0, stats.dedicated_active);
        assert_eq!(0, stats.rq_pending);

        // And the allocator still works afterwards, up to the largest block it can serve.
        assert!(manager.alloc(manager.max_shared_payload).is_ok());

        manager.close_all();
    }

    #[test]
    fn a_segment_that_cannot_hold_the_layout_is_refused() {
        assert!(
            LocalManager::new_with_size(RQ_SIZE, 0, 0, MIN_BLOCK_SIZE, MAX_BLOCK_SIZE).is_err()
        );

        // Pinned regions cannot outnumber the shared ones.
        assert!(
            LocalManager::new_with_size(
                TEST_SIZE,
                TEST_SIZE / 2,
                TEST_SIZE / 4,
                MIN_BLOCK_SIZE,
                MAX_BLOCK_SIZE
            )
            .is_err()
        );

        // The allocator only carves powers of two, smallest first.
        assert!(
            LocalManager::new_with_size(TEST_SIZE, 0, TEST_SIZE / 4, 3, MAX_BLOCK_SIZE).is_err()
        );
        assert!(
            LocalManager::new_with_size(
                TEST_SIZE,
                0,
                TEST_SIZE / 4,
                MAX_BLOCK_SIZE,
                MIN_BLOCK_SIZE
            )
            .is_err()
        );
    }
}
