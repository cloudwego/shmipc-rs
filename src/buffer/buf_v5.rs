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
    collections::VecDeque,
    mem,
    ops::Deref,
    ptr::NonNull,
    slice,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use ahash::AHashSet;
use bytes::{Bytes, BytesMut};

use crate::{
    buffer::{
        BufferReader, BufferWriter,
        buf::{Buf, PinLease, ShmBuf},
    },
    consts::{DEFAULT_SINGLE_BUFFER_SIZE, MAX_DYN_SHM_TRANSFER_SIZE},
    error::Error,
    stats::Stats,
    stream::StreamShared,
    v5::dynshm::{DynShmBufferHeader, LocalManager, MAX_CHAIN_NODES, ManagerError, RemoteManager},
};

/// One block this process allocated and still owns.
#[derive(Debug)]
struct ShmBlock {
    hdr: NonNull<DynShmBufferHeader>,
    /// Cached so free and chain-linking never re-read shared memory, which the peer could
    /// have scribbled on.
    offset: i64,
}

/// V5 write buffer, backed by the local DynShm segment.
///
/// Allocates from DynShm for normal writes and falls back to the heap when allocation fails,
/// when the stream has already fallen back, or when the payload is past
/// [`MAX_DYN_SHM_TRANSFER_SIZE`].
#[derive(Debug)]
pub struct V5WriteBuffer {
    mgr: Arc<LocalManager>,
    shared: Arc<StreamShared>,
    stats: Arc<Stats>,
    pending: Vec<ShmBlock>,
    fallback: Vec<Vec<u8>>,
    len: usize,
}

impl BufferWriter for V5WriteBuffer {
    /// Reserve `size` bytes to write into.
    ///
    /// The returned slice borrows the buffer, so it has to be written before anything else
    /// touches it, including the next reserve.
    fn reserve(&mut self, size: usize) -> Result<&mut [u8], Error> {
        if size == 0 {
            return Ok(&mut []);
        }

        if size > usize::MAX - self.len {
            return Err(Error::SizeTooLarge);
        }
        if !self.shared.is_open() {
            return Err(Error::StreamClosed);
        }
        if !self.fallback.is_empty() || self.shared.in_fallback() {
            tracing::warn!(
                stream = self.shared.id,
                size,
                buffered = self.len,
                has_fallback_data = !self.fallback.is_empty(),
                in_fallback = self.shared.in_fallback(),
                "v5: stream is already in fallback mode, using heap"
            );
            return Ok(self.reserve_fallback(size));
        }
        if size > MAX_DYN_SHM_TRANSFER_SIZE || self.len > MAX_DYN_SHM_TRANSFER_SIZE - size {
            tracing::warn!(
                stream = self.shared.id,
                size,
                buffered = self.len,
                MAX_DYN_SHM_TRANSFER_SIZE,
                "v5: message exceeds the dynshm transfer limit, falling back to heap"
            );
            return Ok(self.reserve_fallback(size));
        }

        // Room left in the block we allocated last.
        // SAFETY: `last` is a block this buffer allocated and has not yet handed to the peer, so
        // it is live and nothing else writes to it.
        if let Some(last) = self.pending.last()
            && let Some(buf) = unsafe { DynShmBufferHeader::reserve(last.hdr.as_ptr(), size) }
        {
            self.len += size;
            return Ok(buf);
        }

        // Allocate new block.
        let hdr = match self.mgr.alloc(size) {
            Ok(hdr) => hdr,
            Err(ManagerError::Closed) => return Err(Error::SessionShutdown),
            Err(err) => {
                tracing::warn!(
                    stream = self.shared.id,
                    size,
                    %err,
                    "v5: dynshm alloc failed, falling back to heap"
                );
                self.stats
                    .v5_alloc_dynshm_error_count
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(self.reserve_fallback(size));
            }
        };

        let (offset, buf) = unsafe {
            let offset = (*hdr).offset;
            let buf = DynShmBufferHeader::data_mut(hdr);
            (offset, buf)
        };

        // Link it behind the previous block so the peer can walk one chain per message.
        if let Some(prev) = self.pending.last() {
            unsafe { (*prev.hdr.as_ptr()).next = offset };
        }
        self.pending.push(ShmBlock {
            hdr: NonNull::new(hdr).expect("alloc returns a non-null header"),
            offset,
        });
        self.len += size;
        Ok(buf)
    }

    fn write_bytes(&mut self, data: &[u8]) -> Result<usize, Error> {
        if data.is_empty() {
            return Ok(0);
        }
        self.reserve(data.len())?.copy_from_slice(data);
        Ok(data.len())
    }
}

impl V5WriteBuffer {
    pub fn new(mgr: Arc<LocalManager>, shared: Arc<StreamShared>, stats: Arc<Stats>) -> Self {
        Self {
            mgr,
            shared,
            stats,
            pending: Vec::with_capacity(8),
            fallback: Vec::new(),
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// True once any part of this message has landed on the heap, which forces the whole
    /// message onto the unix socket at flush.
    pub fn is_fallback(&self) -> bool {
        !self.fallback.is_empty()
    }

    /// Reserves `size` bytes on this side's own heap, for bytes the segment cannot carry.
    fn reserve_fallback(&mut self, size: usize) -> &mut [u8] {
        self.len += size;

        // Over-allocating the first chunk of a fallback message is what lets the rest of it
        // land in the same one rather than one `Vec` per reserve.
        let reusable = self
            .fallback
            .last()
            .is_some_and(|last| last.capacity() - last.len() >= size);
        if !reusable {
            self.fallback.push(Vec::with_capacity(
                size.max(DEFAULT_SINGLE_BUFFER_SIZE as usize),
            ));
        }

        let last = self.fallback.last_mut().expect("pushed just above");
        let start = last.len();
        // Within the capacity checked above, so this cannot reallocate.
        last.resize(start + size, 0);
        &mut last[start..]
    }

    pub fn write_str(&mut self, s: &str) -> Result<usize, Error> {
        self.write_bytes(s.as_bytes())
    }

    /// Take the message's blocks out, for the hand-over to the peer.
    ///
    /// `Ok(None)` means the message held no shared-memory blocks, which is an empty flush and
    /// not a failure. An error means the call itself was refused, and nothing was taken.
    pub fn take_shm(&mut self) -> Result<Option<V5Outgoing>, Error> {
        if !self.fallback.is_empty() {
            return Err(Error::BrokenBuffer);
        }
        if self.pending.is_empty() {
            return Ok(None);
        }
        self.len = 0;
        Ok(Some(V5Outgoing {
            mgr: Arc::clone(&self.mgr),
            allocs: mem::take(&mut self.pending),
        }))
    }

    /// Take back the vector a hand-over used, so the next message does not allocate one.
    ///
    /// The blocks went to the peer, so they are forgotten here rather than freed.
    pub fn recycle(&mut self, mut outgoing: V5Outgoing) {
        let mut allocs = mem::take(&mut outgoing.allocs);
        allocs.clear();
        if allocs.capacity() > self.pending.capacity() {
            self.pending = allocs;
        }
    }

    /// Take everything as chunks to write to the unix socket.
    pub fn take_fallback(&mut self) -> Vec<OutChunk> {
        let mut out = Vec::with_capacity(self.pending.len() + self.fallback.len());

        for block in self.pending.drain(..) {
            out.push(OutChunk::Shm(DynShmOwned {
                mgr: self.mgr.clone(),
                block,
            }));
        }
        out.extend(self.fallback.drain(..).map(OutChunk::Heap));
        self.len = 0;
        out
    }

    /// Free everything without sending, for a stream that is closing.
    pub fn discard(&mut self) {
        for alloc in self.pending.drain(..) {
            self.mgr.free_at(alloc.offset);
        }
        self.fallback.clear();
        self.len = 0;
    }
}

impl Drop for V5WriteBuffer {
    fn drop(&mut self) {
        self.discard();
    }
}

unsafe impl Send for V5WriteBuffer {}

/// The blocks of one message, on their way to the peer.
///
/// They leave the write buffer before the queue put, so a flush cancelled part-way through can
/// neither hand them over twice nor let the next message write into them. Blocks still here
/// when this drops were never handed over and go back to the manager, which is what a flush
/// dropped while it waits on a full queue relies on.
#[derive(Debug)]
pub struct V5Outgoing {
    mgr: Arc<LocalManager>,
    /// Never empty before the hand-over: an empty message is `None` from
    /// [`V5WriteBuffer::take_shm`] instead.
    allocs: Vec<ShmBlock>,
}

impl V5Outgoing {
    /// The offset of the chain's root, which is what the peer is handed.
    pub fn root(&self) -> i64 {
        self.allocs[0].offset
    }
}

impl Drop for V5Outgoing {
    // Give every block back, for a hand-over that did not happen.
    fn drop(&mut self) {
        for alloc in self.allocs.drain(..) {
            self.mgr.free_at(alloc.offset);
        }
    }
}

unsafe impl Send for V5Outgoing {}
unsafe impl Sync for V5Outgoing {}

/// A block of an outgoing message on the fallback path. It can contain either
/// shared-memory or heap bytes.
#[derive(Debug)]
pub enum OutChunk {
    Shm(DynShmOwned),
    Heap(Vec<u8>),
}

impl Deref for OutChunk {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Shm(owned) => owned.bytes(),
            Self::Heap(chunk) => chunk,
        }
    }
}

impl AsRef<[u8]> for OutChunk {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

/// A shared-memory block being sent over the unix socket: the same block the write buffer held,
/// now with the manager to return it to once the last frame has been built.
#[derive(Debug)]
pub struct DynShmOwned {
    mgr: Arc<LocalManager>,
    block: ShmBlock,
}

unsafe impl Send for DynShmOwned {}

impl DynShmOwned {
    fn bytes(&self) -> &[u8] {
        // SAFETY: this owner keeps the block live and unfreed until it drops, and the peer only
        // reads a block once it has been handed over.
        unsafe { DynShmBufferHeader::data(self.block.hdr.as_ptr()) }
    }
}

impl Drop for DynShmOwned {
    fn drop(&mut self) {
        self.mgr.free_at(self.block.offset);
    }
}

/// Something the event loop has delivered but the reader has not looked at yet.
#[derive(Debug)]
enum PendingInput {
    /// The root offset of a chain in the peer's segment.
    Shm(i64),
    /// A message that arrived over the unix socket.
    Heap(Bytes),
}

/// The event loop's half of the read side, and the only part shared between threads.
#[derive(Debug)]
pub struct V5Inbox {
    mgr: Arc<RemoteManager>,
    /// The stream this inbox belongs to, so a delivery can check it is still open.
    shared: Arc<StreamShared>,
    /// Bumped every time the stream is handed back to the pool. A batch that was collected
    /// against an older generation belongs to the stream's previous life and is refused.
    generation: AtomicU64,
    /// Delivered by the event loop, not yet converted into readable slices. The reader takes
    /// these in [`V5ReadBuffer::drain_pending`] and turns them into `V5ReadBuffer::ready`.
    pending: Mutex<Vec<PendingInput>>,
}

impl V5Inbox {
    pub fn new(mgr: Arc<RemoteManager>, shared: Arc<StreamShared>) -> Arc<Self> {
        Arc::new(Self {
            mgr,
            shared,
            generation: AtomicU64::new(0),
            pending: Mutex::new(Vec::with_capacity(8)),
        })
    }

    pub fn add_pending(&self, offset: i64) {
        self.pending.lock().unwrap().push(PendingInput::Shm(offset));
    }

    pub fn add_fallback(&self, data: Bytes) {
        self.pending.lock().unwrap().push(PendingInput::Heap(data));
    }

    /// Deliver a polled batch under one lock, so the event loop pays for the mutex once.
    pub fn add_pending_batch(&self, offsets: &[i64]) {
        if offsets.is_empty() {
            return;
        }
        let mut pending = self.pending.lock().unwrap();
        pending.extend(offsets.iter().map(|&offset| PendingInput::Shm(offset)));
    }

    /// The generation a batch should be collected against.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Deliver a batch only if the stream is still the one it was collected for.
    ///
    /// Returns `false` when the stream has been reused or closed underneath the event loop, in
    /// which case the caller still owns the blocks and has to release them.
    pub fn add_pending_batch_if_generation(&self, offsets: &[i64], generation: u64) -> bool {
        if offsets.is_empty() {
            return true;
        }
        // Taken before the checks so a concurrent `advance_generation_if_idle` cannot slip
        // between them and the add.
        let mut pending = self.pending.lock().unwrap();
        if self.generation() != generation || !self.shared.is_open() {
            return false;
        }
        pending.extend(offsets.iter().map(|&offset| PendingInput::Shm(offset)));
        true
    }

    /// Retire the current generation, so batches already in flight are refused.
    ///
    /// Refuses to advance while undelivered input is queued, because those blocks belong to
    /// the stream's current life and dropping them on the floor would leak them.
    pub fn advance_generation_if_idle(&self) -> bool {
        let pending = self.pending.lock().unwrap();
        if !pending.is_empty() {
            return false;
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// Deliver socket bytes only if the stream is still the one they were read for.
    ///
    /// The same guard as
    /// [`add_pending_batch_if_generation`](Self::add_pending_batch_if_generation), which applies
    /// to the segment's chains. Heap bytes need it just as much: they carry no block to release,
    /// but landing them in a reused stream would splice one request's bytes into the next one's.
    ///
    /// Returns `false` when the stream has been reused or closed, in which case the bytes are
    /// dropped — which is all they need, having never left this side's heap.
    pub fn add_fallback_if_generation(&self, data: Bytes, generation: u64) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if self.generation() != generation || !self.shared.is_open() {
            return false;
        }
        pending.push(PendingInput::Heap(data));
        true
    }

    pub fn is_empty(&self) -> bool {
        self.pending.lock().unwrap().is_empty()
    }

    /// How many deliveries are waiting to be converted into readable bytes.
    pub fn pending_len(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Exchange the pending list for the caller's empty buffer.
    ///
    /// The caller keeps whichever allocation it is not using and hands it back on the next
    /// drain, so a steady stream of deliveries cycles two buffers instead of allocating a
    /// fresh one every time the list is emptied.
    fn swap_pending(&self, spare: &mut Vec<PendingInput>) {
        debug_assert!(spare.is_empty(), "the spare buffer is handed back empty");
        std::mem::swap(&mut *self.pending.lock().unwrap(), spare);
    }
}

impl Drop for V5Inbox {
    /// Anything still queued was never read, so it goes back to the peer rather than leaking
    /// in its segment.
    fn drop(&mut self) {
        for input in self.pending.get_mut().unwrap().drain(..) {
            if let PendingInput::Shm(offset) = input {
                let _ = self.mgr.free_chain(offset);
            }
        }
    }
}

/// Keeps one block of the peer's memory alive, and releases it when the last holder drops.
#[derive(Debug)]
pub(crate) struct V5Pin {
    mgr: Arc<RemoteManager>,
    offset: i64,
}

impl Drop for V5Pin {
    fn drop(&mut self) {
        // A full release queue is absorbed into the manager's pending list and retried, and a
        // closed session has nothing to return to, so neither is an error here.
        let _ = self.mgr.free(self.offset);
    }
}

#[derive(Debug)]
enum ChunkSrc {
    Shm {
        /// The block's offset in the peer's segment, which is what gives it back.
        offset: i64,
        ptr: *const u8,
        len: usize,
        /// Created the first time a zero-copy read borrows the block.
        pin: Option<Arc<V5Pin>>,
    },
    Heap(Bytes),
}

#[derive(Debug)]
struct ReadChunk {
    src: ChunkSrc,
    /// How much of this slice the reader has consumed.
    pos: usize,
}

impl ReadChunk {
    fn len(&self) -> usize {
        match &self.src {
            ChunkSrc::Shm { len, .. } => *len,
            ChunkSrc::Heap(bytes) => bytes.len(),
        }
    }

    fn remaining(&self) -> usize {
        self.len() - self.pos
    }

    fn bytes(&self) -> &[u8] {
        match &self.src {
            // SAFETY: the pin keeps both the block and the mapping alive for as long as this
            // slice exists, and the peer does not write a block it has already handed over.
            ChunkSrc::Shm { ptr, len, .. } => unsafe { slice::from_raw_parts(*ptr, *len) },
            ChunkSrc::Heap(bytes) => bytes,
        }
    }

    /// The block's start and a share of its pin, for a zero-copy read; `None` for heap bytes.
    fn shm_lease(&mut self, mgr: &Arc<RemoteManager>) -> Option<(*const u8, Arc<V5Pin>)> {
        match &mut self.src {
            ChunkSrc::Shm {
                offset, ptr, pin, ..
            } => {
                let pin = pin
                    .get_or_insert_with(|| {
                        Arc::new(V5Pin {
                            mgr: Arc::clone(mgr),
                            offset: *offset,
                        })
                    })
                    .clone();
                Some((*ptr, pin))
            }
            ChunkSrc::Heap(_) => None,
        }
    }

    /// Give the block back, unless a lease holds it, in which case its pin will.
    fn release(self, mgr: &RemoteManager) {
        if let ChunkSrc::Shm {
            offset, pin: None, ..
        } = self.src
        {
            // A full release queue is absorbed into the manager's pending list and retried,
            // and a closed session has nothing to return to, so neither is an error here.
            let _ = mgr.free(offset);
        }
    }
}

/// V5 read buffer, over blocks in the peer's DynShm segment.
#[derive(Debug)]
pub struct V5ReadBuffer {
    inbox: Arc<V5Inbox>,
    mgr: Arc<RemoteManager>,
    stats: Arc<Stats>,
    /// Converted and readable, waiting on a reader. The counterpart to the inbox's `pending`,
    /// which holds what has arrived but not yet been walked.
    ready: VecDeque<ReadChunk>,
    /// The empty half of the inbox's double buffer, swapped in on each drain so neither side
    /// has to allocate.
    spare: Vec<PendingInput>,
    len: usize,
}

impl BufferReader for V5ReadBuffer {
    /// The whole of the first block still holding data, which is the cheapest possible read.
    fn read_chunk(&mut self) -> Result<Buf, Error> {
        let size = self.ready.front().map(ReadChunk::remaining).unwrap_or(0);
        if size == 0 {
            return Err(Error::NotEnoughData);
        }
        self.read_exact_bytes(size)
    }

    /// Take `size` bytes, without copying when one block holds them all.
    fn read_exact_bytes(&mut self, size: usize) -> Result<Buf, Error> {
        if size == 0 {
            return Ok(Buf::Exm(Bytes::new()));
        }
        if self.len < size {
            return Err(Error::NotEnoughData);
        }

        if self.ready.front().is_some_and(|s| s.remaining() >= size) {
            let front = self.ready.front_mut().expect("just checked");
            let pos = front.pos;
            // Owned either way, so the borrow of `front` ends here and the slice can be
            // advanced — and popped — underneath a result that no longer refers to it.
            let buf = if let ChunkSrc::Heap(bytes) = &front.src {
                Buf::Exm(bytes.slice(pos..pos + size))
            } else {
                let (ptr, pin) = front
                    .shm_lease(&self.mgr)
                    .expect("a slice is either heap bytes or a block");
                // SAFETY: `pos + size` is within the block, and the lease holds the pin, so
                // the block outlives the returned value even once the slice is popped below.
                Buf::Shm(unsafe { ShmBuf::new(ptr.add(pos), size, PinLease::V5(pin)) })
            };
            front.pos += size;
            self.len -= size;
            self.pop_if_spent();
            return Ok(buf);
        }

        // Spans blocks, so there is nothing to do but copy.
        self.stats
            .v5_read_copy_count
            .fetch_add(1, Ordering::Relaxed);
        let mut out = BytesMut::with_capacity(size);
        while out.len() < size {
            let Some(front) = self.ready.front_mut() else {
                break;
            };
            let take = front.remaining().min(size - out.len());
            let pos = front.pos;
            out.extend_from_slice(&front.bytes()[pos..pos + take]);
            front.pos += take;
            self.len -= take;
            self.pop_if_spent();
        }
        Ok(Buf::Exm(out.freeze()))
    }

    /// Look at `size` bytes without consuming them.
    fn peek(&mut self, size: usize) -> Result<Buf, Error> {
        if size == 0 {
            return Ok(Buf::Exm(Bytes::new()));
        }
        if self.len < size {
            return Err(Error::NotEnoughData);
        }

        if let Some(front) = self.ready.front_mut()
            && front.remaining() >= size
        {
            let pos = front.pos;
            if let ChunkSrc::Heap(bytes) = &front.src {
                return Ok(Buf::Exm(bytes.slice(pos..pos + size)));
            }
            let (ptr, pin) = front
                .shm_lease(&self.mgr)
                .expect("a slice is either heap bytes or a block");
            // SAFETY: as in `read_exact_bytes`; the lease keeps the block alive, and peeking
            // leaves the slice in place regardless.
            return Ok(Buf::Shm(unsafe {
                ShmBuf::new(ptr.add(pos), size, PinLease::V5(pin))
            }));
        }

        self.stats
            .v5_read_copy_count
            .fetch_add(1, Ordering::Relaxed);
        let mut out = BytesMut::with_capacity(size);
        for slice in &self.ready {
            if out.len() >= size {
                break;
            }
            let take = slice.remaining().min(size - out.len());
            out.extend_from_slice(&slice.bytes()[slice.pos..slice.pos + take]);
        }
        Ok(Buf::Exm(out.freeze()))
    }

    fn discard(&mut self, size: usize) -> Result<usize, Error> {
        if self.len < size {
            return Err(Error::NotEnoughData);
        }
        let mut dropped = 0;
        while dropped < size {
            let Some(front) = self.ready.front_mut() else {
                break;
            };
            let take = front.remaining().min(size - dropped);
            front.pos += take;
            self.len -= take;
            dropped += take;
            self.pop_if_spent();
        }
        Ok(dropped)
    }

    /// Kept for parity with V4, where it is what returns pinned slices.
    ///
    /// Here a block is released the moment nothing refers to it, so the only useful thing left
    /// to do is retry releases the peer's queue was too full to accept earlier.
    fn release_previous_read(&mut self) {
        let _ = self.mgr.retry_pending_releases();
    }
}

impl V5ReadBuffer {
    pub fn new(inbox: Arc<V5Inbox>, stats: Arc<Stats>) -> Self {
        Self {
            mgr: inbox.mgr.clone(),
            inbox,
            stats,
            ready: VecDeque::with_capacity(8),
            spare: Vec::with_capacity(8),
            len: 0,
        }
    }

    pub fn inbox(&self) -> &Arc<V5Inbox> {
        &self.inbox
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// True if the event loop has delivered something this buffer has not converted yet.
    pub fn has_pending(&self) -> bool {
        !self.inbox.is_empty()
    }

    /// Convert everything the event loop delivered into readable slices, and report how many
    /// bytes that added.
    pub fn drain_pending(&mut self) -> Result<usize, Error> {
        let mut inputs = mem::take(&mut self.spare);
        self.inbox.swap_pending(&mut inputs);
        if inputs.is_empty() {
            self.spare = inputs;
            return Ok(0);
        }

        let mut total = 0;
        let mut failed = None;
        {
            let mut rest = inputs.drain(..);
            for input in rest.by_ref() {
                match input {
                    PendingInput::Heap(bytes) => {
                        total += bytes.len();
                        self.len += bytes.len();
                        self.ready.push_back(ReadChunk {
                            src: ChunkSrc::Heap(bytes),
                            pos: 0,
                        });
                    }
                    PendingInput::Shm(root) => match self.walk_chain(root) {
                        Ok(added) => total += added,
                        Err(err) => {
                            failed = Some(err);
                            break;
                        }
                    },
                }
            }
            // A bad chain condemns the whole segment, so nothing after it is worth walking.
            // The blocks still go back, because the caller is about to close the session and
            // no one else holds these offsets.
            for input in rest {
                if let PendingInput::Shm(offset) = input {
                    let _ = self.mgr.free_chain(offset);
                }
            }
        }
        self.spare = inputs;

        match failed {
            Some(err) => Err(err),
            None => Ok(total),
        }
    }

    /// Follow one message's chain, bounded three ways: a visited set against cycles, a node
    /// count, and a total size. The peer writes these offsets, so none of it can be trusted.
    fn walk_chain(&mut self, root: i64) -> Result<usize, Error> {
        // Built only once the chain reaches a second block. Most messages are one block, and a
        // one-block chain cannot loop: `read` refuses a block that names itself as next.
        let mut visited: Option<AHashSet<i64>> = None;
        let mut nodes: usize = 0;
        let mut chain_total: usize = 0;
        let mut offset = root;

        while offset > 0 {
            if nodes >= MAX_CHAIN_NODES {
                return Err(Error::BrokenBuffer);
            }
            if nodes > 0
                && !visited
                    .get_or_insert_with(|| AHashSet::from([root]))
                    .insert(offset)
            {
                return Err(Error::BrokenBuffer);
            }
            nodes += 1;

            let (ptr, len, next) = match self.mgr.read(offset) {
                Some((data, next)) => (data.as_ptr(), data.len(), next),
                None => return Err(Error::BrokenBuffer),
            };
            if chain_total + len > MAX_DYN_SHM_TRANSFER_SIZE {
                return Err(Error::SizeTooLarge);
            }

            self.ready.push_back(ReadChunk {
                src: ChunkSrc::Shm {
                    offset,
                    ptr,
                    len,
                    pin: None,
                },
                pos: 0,
            });
            self.len += len;
            chain_total += len;
            offset = next;
        }
        Ok(chain_total)
    }

    /// Drop the front slice if it is spent, releasing its block unless a [`Buf`] still holds it.
    fn pop_if_spent(&mut self) {
        if self.ready.front().is_some_and(|s| s.remaining() == 0)
            && let Some(slice) = self.ready.pop_front()
        {
            slice.release(&self.mgr);
        }
    }

    /// Copy into `dst`, returning how much was available.
    pub fn read(&mut self, dst: &mut [u8]) -> usize {
        let mut n = 0;
        while n < dst.len() {
            let Some(front) = self.ready.front_mut() else {
                break;
            };
            let take = front.remaining().min(dst.len() - n);
            let pos = front.pos;
            dst[n..n + take].copy_from_slice(&front.bytes()[pos..pos + take]);
            front.pos += take;
            self.len -= take;
            n += take;
            self.pop_if_spent();
        }
        n
    }
}

unsafe impl Send for V5ReadBuffer {}

impl Drop for V5ReadBuffer {
    /// Whatever was made readable and never read goes back to the peer.
    fn drop(&mut self) {
        while let Some(slice) = self.ready.pop_front() {
            slice.release(&self.mgr);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::fd::OwnedFd;

    use super::*;
    use crate::{
        consts::{DYNSHM_BUFFER_HEADER_SIZE, MAX_BLOCK_SIZE, MIN_BLOCK_SIZE, SHARED_REGION_SIZE},
        stream::STREAM_CLOSED,
        v5::release_queue::RQ_SIZE,
    };

    const TEST_SIZE: usize = 128 << 20;
    const SMALL_SIZE: usize = RQ_SIZE + SHARED_REGION_SIZE;

    fn pair_of(
        total: usize,
        pinned: usize,
        shared_cap: usize,
    ) -> (Arc<LocalManager>, Arc<RemoteManager>) {
        let local =
            LocalManager::new_with_size(total, pinned, shared_cap, MIN_BLOCK_SIZE, MAX_BLOCK_SIZE)
                .unwrap();
        let fd: OwnedFd = nix::unistd::dup(local.fd()).unwrap();
        let remote = RemoteManager::map(fd, total as u32).unwrap();
        (local, remote)
    }

    fn pair() -> (Arc<LocalManager>, Arc<RemoteManager>) {
        pair_of(TEST_SIZE, TEST_SIZE / 100, TEST_SIZE / 4)
    }

    fn small_pair() -> (Arc<LocalManager>, Arc<RemoteManager>) {
        pair_of(SMALL_SIZE, 0, SHARED_REGION_SIZE)
    }

    fn writer(local: Arc<LocalManager>) -> V5WriteBuffer {
        V5WriteBuffer::new(local, StreamShared::new(1), Arc::new(Stats::default()))
    }

    fn reader(remote: Arc<RemoteManager>) -> V5ReadBuffer {
        V5ReadBuffer::new(
            V5Inbox::new(remote, StreamShared::new(1)),
            Arc::new(Stats::default()),
        )
    }

    /// Takes one message out of the writer the way a flush does, and returns its root.
    fn take_root(w: &mut V5WriteBuffer) -> i64 {
        let outgoing = w
            .take_shm()
            .unwrap()
            .expect("the write went to shared memory");
        let root = outgoing.root();
        w.recycle(outgoing);
        root
    }

    /// Writes one message, hands its chain to the reader and drains it into readable slices.
    fn deliver(w: &mut V5WriteBuffer, r: &mut V5ReadBuffer, payload: &[u8]) {
        w.write_bytes(payload).unwrap();
        r.inbox().add_pending(take_root(w));
        r.drain_pending().unwrap();
    }

    /// Where a zero-copy result points, measured from the base of the peer's mapping, or
    /// `None` for one that was gathered into this side's own memory.
    fn offset_in(buf: &Buf, remote: &RemoteManager) -> Option<usize> {
        match buf {
            Buf::Shm(shm) => Some(shm.as_ref().as_ptr() as usize - remote.data().as_ptr() as usize),
            Buf::Exm(_) => None,
        }
    }

    /// Takes every block the segment will give, so the next allocation has to fail.
    ///
    /// The payload is one byte: with the header added that still rounds up to a whole minimum
    /// block, so this leaves nothing behind that a larger request could have used.
    fn exhaust(local: &LocalManager) -> Vec<*mut DynShmBufferHeader> {
        let mut hogs = Vec::new();
        while let Ok(hdr) = local.alloc(1) {
            hogs.push(hdr);
            assert!(
                hogs.len() < 10_000,
                "the segment should have run out by now"
            );
        }
        hogs
    }

    #[test]
    fn a_reserve_that_does_not_fit_opens_the_next_block() {
        let (local, _remote) = pair();
        let mut buffer = writer(local);

        buffer.write_bytes(b"head").unwrap();
        assert_eq!(1, buffer.pending.len());

        // Large enough that the block holding "head" cannot take it.
        buffer.reserve(MAX_BLOCK_SIZE).unwrap().fill(0);
        assert_eq!(2, buffer.pending.len());
        assert_eq!(4 + MAX_BLOCK_SIZE, buffer.len());

        // The root of the chain is still the block "head" went into.
        let root = buffer.pending[0].offset;
        assert_eq!(root, take_root(&mut buffer));
    }

    #[test]
    fn two_small_writes_share_one_block() {
        let (local, _remote) = pair();
        let mut buffer = writer(local);

        buffer.write_bytes(b"hello ").unwrap();
        buffer.write_bytes(b"world").unwrap();

        assert_eq!(
            1,
            buffer.pending.len(),
            "the second write must reuse what the first block had left"
        );
        assert_eq!(11, buffer.len());
    }

    #[test]
    fn write_str_writes_its_bytes() {
        let (local, _remote) = pair();
        let mut buffer = writer(local);

        assert_eq!(5, buffer.write_str("hello").unwrap());
        assert_eq!(5, buffer.len());
        assert!(!buffer.is_fallback());
    }

    #[test]
    fn a_write_after_the_stream_closes_is_refused() {
        let (local, _remote) = pair();
        let shared = StreamShared::new(1);
        let mut buffer = V5WriteBuffer::new(local, shared.clone(), Arc::new(Stats::default()));

        shared.state.store(STREAM_CLOSED, Ordering::Release);

        assert!(matches!(buffer.reserve(4), Err(Error::StreamClosed)));
        assert!(buffer.is_empty(), "a refused write reserves nothing");
    }

    #[test]
    fn a_write_after_the_manager_closes_is_refused() {
        let (local, _remote) = pair();
        let mut buffer = writer(local.clone());

        local.close_all();

        assert!(matches!(buffer.reserve(4), Err(Error::SessionShutdown)));
        assert!(buffer.is_empty());
    }

    #[test]
    fn an_exhausted_segment_falls_back_to_the_heap() {
        let (local, _remote) = small_pair();
        let stats = Arc::new(Stats::default());
        let mut buffer =
            V5WriteBuffer::new(local.clone(), StreamShared::new(1), Arc::clone(&stats));

        let _hogs = exhaust(&local);

        assert_eq!(4, buffer.write_bytes(b"heap").unwrap());
        assert!(
            buffer.is_fallback(),
            "a full segment forces the socket path"
        );
        assert_eq!(1, stats.v5_alloc_dynshm_error_count.load(Ordering::Relaxed));
        assert_eq!(4, buffer.len());
    }

    #[test]
    fn a_message_past_the_transfer_limit_goes_to_the_heap() {
        let (local, _remote) = pair();
        let mut buffer = writer(local);

        buffer.write_bytes(b"in shm").unwrap();
        assert!(!buffer.is_fallback());

        buffer.len = MAX_DYN_SHM_TRANSFER_SIZE - 3;
        buffer.write_bytes(b"over").unwrap();

        assert!(
            buffer.is_fallback(),
            "four more bytes do not fit under the limit"
        );
    }

    #[test]
    fn a_stream_already_in_fallback_never_touches_shared_memory() {
        let (local, _remote) = pair();
        let shared = StreamShared::new(1);
        let mut buffer = V5WriteBuffer::new(local, shared.clone(), Arc::new(Stats::default()));

        shared.set_fallback();
        buffer.write_bytes(b"data").unwrap();

        assert!(buffer.is_fallback());
        assert_eq!(4, buffer.len());
    }

    #[test]
    fn heap_chunks_are_reused_across_reserves() {
        let (local, _remote) = pair();
        let mut buffer = writer(local);
        buffer.shared.set_fallback();

        buffer.write_bytes(b"first").unwrap();
        buffer.write_bytes(b"second").unwrap();
        assert_eq!(1, buffer.fallback.len(), "small writes share one chunk");
        assert_eq!(b"firstsecond"[..], buffer.fallback[0][..]);

        // Past what the chunk had left, which opens the next one.
        buffer
            .write_bytes(&vec![b'x'; 2 * DEFAULT_SINGLE_BUFFER_SIZE as usize])
            .unwrap();
        assert_eq!(2, buffer.fallback.len());
    }

    #[test]
    fn fallback_chunks_carry_both_kinds_of_block() {
        let (local, _remote) = pair();
        let mut buffer = writer(local);

        buffer.write_bytes(b"in shm").unwrap();
        buffer.shared.set_fallback();
        buffer.write_bytes(b" on heap").unwrap();

        let chunks = buffer.take_fallback();
        assert_eq!(2, chunks.len());
        assert_eq!(b"in shm"[..], chunks[0][..]);
        assert_eq!(b" on heap"[..], chunks[1][..]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn fallback_chunks_are_the_blocks_themselves_not_copies() {
        let (local, _remote) = pair();
        let mut buffer = writer(local);

        buffer.write_bytes(b"in shm").unwrap();
        let hdr = buffer.pending[0].hdr;
        buffer.shared.set_fallback();
        buffer.write_bytes(b" on heap").unwrap();

        let chunks = buffer.take_fallback();
        // The accessor takes a raw pointer, so it is on the caller to know the block is live:
        // the chunk owns it until it is dropped at the end of this test.
        let block = unsafe { DynShmBufferHeader::data_mut(hdr.as_ptr()) };
        block[0] = b'I';

        assert_eq!(
            b"In shm"[..],
            chunks[0][..],
            "the chunk must see the segment, not a snapshot of it"
        );
    }

    #[test]
    fn fallback_chunks_free_their_blocks_when_dropped() {
        let (local, _remote) = pair();
        let stats = Arc::new(Stats::default());
        let mut buffer =
            V5WriteBuffer::new(local.clone(), StreamShared::new(1), Arc::clone(&stats));

        buffer.write_bytes(b"in shm").unwrap();
        buffer.shared.set_fallback();
        buffer.write_bytes(b" on heap").unwrap();
        assert_eq!(1, local.stats().shared_active);

        let chunks = buffer.take_fallback();
        assert_eq!(
            0,
            stats.fallback_write_count.load(Ordering::Relaxed),
            "the fallback write is counted once by the stream that performs it, not here"
        );
        assert_eq!(
            1,
            local.stats().shared_active,
            "the block is still ours while the frames are being written"
        );

        drop(chunks);
        assert_eq!(
            0,
            local.stats().shared_active,
            "and goes back once the last frame has been built"
        );
    }

    #[test]
    fn take_shm_refuses_a_message_that_is_half_on_the_heap() {
        let (local, _remote) = pair();
        let mut buffer = writer(local);

        buffer.write_bytes(b"in shm").unwrap();
        buffer.shared.set_fallback();
        buffer.write_bytes(b" on heap").unwrap();

        assert!(matches!(buffer.take_shm(), Err(Error::BrokenBuffer)));
    }

    #[test]
    fn discard_frees_every_block() {
        let (local, _remote) = pair();
        let mut buffer = writer(local.clone());

        buffer.write_bytes(b"unsent").unwrap();
        assert_eq!(1, local.stats().shared_active);

        buffer.discard();

        assert!(buffer.is_empty());
        assert_eq!(
            0,
            local.stats().shared_active,
            "a discarded block goes back"
        );
    }

    /// `Stream::flush_v5` takes the message out of the buffer before its queue put, then awaits
    /// the retry loop. Dropping that future on a full queue (a timeout, or a `select!` that gave
    /// up) drops the `V5Outgoing`, which has to give the blocks back by itself.
    #[test]
    fn a_flush_cancelled_on_a_full_queue_frees_its_blocks() {
        let (local, _remote) = pair();
        let mut buffer = writer(local.clone());

        buffer.write_bytes(b"x").unwrap();
        assert_eq!(1, local.stats().shared_active);

        // What `flush_v5` does before its first await.
        let outgoing = buffer
            .take_shm()
            .unwrap()
            .expect("the write went to shared memory");
        assert!(buffer.is_empty());

        // The put failed with `QueueFull`, and the future was dropped while it slept.
        drop(outgoing);
        // The stream went away, which discards what the buffer still holds: nothing.
        drop(buffer);

        assert_eq!(
            0,
            local.stats().shared_active,
            "the blocks of a cancelled flush must go back"
        );
    }

    #[test]
    fn dropping_the_write_buffer_frees_every_block() {
        let (local, _remote) = pair();
        {
            let mut buffer = writer(local.clone());
            buffer.write_bytes(b"unsent").unwrap();
            assert_eq!(1, local.stats().shared_active);
        }
        assert_eq!(0, local.stats().shared_active);
    }

    #[test]
    fn a_message_crosses_the_segment_and_reads_back_whole() {
        let (local, remote) = pair();
        let mut buffer = writer(local);
        let mut read = reader(remote);

        buffer.write_bytes(b"hello ").unwrap();
        buffer.write_bytes(b"world").unwrap();
        let root = take_root(&mut buffer);

        read.inbox().add_pending(root);
        assert_eq!(11, read.drain_pending().unwrap());
        assert_eq!(b"hello world"[..], read.read_exact_bytes(11).unwrap()[..]);
        assert!(read.is_empty());
    }

    #[test]
    fn a_read_hands_back_the_peer_s_own_memory() {
        let (local, remote) = pair();
        let mut buffer = writer(local);
        let mut read = reader(Arc::clone(&remote));

        buffer.write_bytes(b"borrowed").unwrap();
        let root = take_root(&mut buffer);
        read.inbox().add_pending(root);
        read.drain_pending().unwrap();

        let buf = read.read_exact_bytes(8).unwrap();
        assert!(
            matches!(buf, Buf::Shm(_)),
            "a read inside one block must not copy"
        );
        assert_eq!(
            Some(root as usize + DYNSHM_BUFFER_HEADER_SIZE),
            offset_in(&buf, &remote),
            "the bytes must lie at the block's own payload offset"
        );
        assert_eq!(b"borrowed"[..], buf[..]);
    }

    #[test]
    fn a_chained_message_reads_back_in_order() {
        let (local, remote) = pair();
        let mut buffer = writer(local);
        let mut read = reader(remote);

        let body = MAX_BLOCK_SIZE - 64;
        buffer.write_bytes(&vec![b'a'; body]).unwrap();
        buffer.write_bytes(&vec![b'b'; body]).unwrap();
        let root = take_root(&mut buffer);

        read.inbox().add_pending(root);
        assert_eq!(2 * body, read.drain_pending().unwrap());

        // Spans two blocks, so this takes the copying path.
        let all = read.read_exact_bytes(2 * body).unwrap();
        assert!(
            matches!(all, Buf::Exm(_)),
            "a read across blocks has to be gathered, and losing that is silent"
        );
        assert!(all[..body].iter().all(|&b| b == b'a'));
        assert!(all[body..].iter().all(|&b| b == b'b'));
    }

    #[test]
    fn read_chunk_stops_at_the_end_of_a_block() {
        let (local, remote) = pair();
        let mut buffer = writer(local);
        let mut read = reader(remote);

        let body = MAX_BLOCK_SIZE - 64;
        buffer.write_bytes(&vec![b'a'; body]).unwrap();
        buffer.write_bytes(&vec![b'b'; body]).unwrap();
        read.inbox().add_pending(take_root(&mut buffer));
        read.drain_pending().unwrap();

        let first = read.read_chunk().unwrap();
        assert!(matches!(first, Buf::Shm(_)));
        assert_eq!(body, first.len(), "a chunk stops at the block boundary");
        assert!(first.iter().all(|&b| b == b'a'));

        let second = read.read_chunk().unwrap();
        assert_eq!(body, second.len());
        assert!(second.iter().all(|&b| b == b'b'));
        assert!(read.is_empty());
    }

    #[test]
    fn read_copies_out_what_is_there() {
        let (local, remote) = pair();
        let mut buffer = writer(local);
        let mut read = reader(remote);

        deliver(&mut buffer, &mut read, b"copied");

        let mut sink = [0u8; 16];
        assert_eq!(6, read.read(&mut sink), "a short read takes what there is");
        assert_eq!(b"copied"[..], sink[..6]);
        assert!(read.is_empty());
    }

    #[test]
    fn peek_does_not_consume() {
        let (local, remote) = pair();
        let mut buffer = writer(local);
        let mut read = reader(remote);

        deliver(&mut buffer, &mut read, b"peekable");

        let first = read.peek(4).unwrap();
        assert!(
            matches!(first, Buf::Shm(_)),
            "a peek inside one block borrows too"
        );
        assert_eq!(b"peek"[..], first[..]);
        assert_eq!(8, read.len(), "a peek moves no cursor");
        drop(first);

        assert_eq!(b"peek"[..], read.peek(4).unwrap()[..]);
        assert_eq!(b"peekable"[..], read.read_exact_bytes(8).unwrap()[..]);
    }

    #[test]
    fn peek_across_blocks_is_gathered() {
        let (local, remote) = pair();
        let mut buffer = writer(local);
        let mut read = reader(remote);

        let body = MAX_BLOCK_SIZE - 64;
        buffer.write_bytes(&vec![b'a'; body]).unwrap();
        buffer.write_bytes(&vec![b'b'; body]).unwrap();
        read.inbox().add_pending(take_root(&mut buffer));
        read.drain_pending().unwrap();

        let across = read.peek(body + 8).unwrap();
        assert!(matches!(across, Buf::Exm(_)));
        assert_eq!(body + 8, across.len());
        assert!(across[body..].iter().all(|&b| b == b'b'));
        assert_eq!(2 * body, read.len(), "still consumed nothing");
    }

    #[test]
    fn a_short_read_is_refused_rather_than_truncated() {
        let (local, remote) = pair();
        let mut buffer = writer(local);
        let mut read = reader(remote);

        deliver(&mut buffer, &mut read, b"eight!!!");

        assert!(matches!(
            read.read_exact_bytes(9),
            Err(Error::NotEnoughData)
        ));
        assert!(matches!(read.peek(9), Err(Error::NotEnoughData)));
        assert!(matches!(read.discard(9), Err(Error::NotEnoughData)));
        assert_eq!(8, read.len(), "a refused read consumes nothing");
    }

    #[test]
    fn an_empty_buffer_reads_nothing() {
        let (_local, remote) = pair();
        let mut read = reader(remote);

        assert!(matches!(read.read_chunk(), Err(Error::NotEnoughData)));
        assert!(matches!(
            read.read_exact_bytes(1),
            Err(Error::NotEnoughData)
        ));
        assert_eq!(0, read.read(&mut [0u8; 8]));

        // A zero-length read is not worth a lease on a block no byte comes out of.
        assert!(read.read_exact_bytes(0).unwrap().is_empty());
        assert!(read.peek(0).unwrap().is_empty());
    }

    #[test]
    fn a_block_comes_back_only_when_the_last_reader_drops_it() {
        let (local, remote) = pair();
        let mut buffer = writer(Arc::clone(&local));
        let mut read = reader(remote);

        deliver(&mut buffer, &mut read, b"held");

        let held = read.read_exact_bytes(4).unwrap().into_bytes();
        assert!(read.is_empty(), "every byte has been read");
        assert_eq!(
            0,
            local.drain_rq(),
            "a block a reader still holds must stay put"
        );

        let clone = held.clone();
        drop(held);
        assert_eq!(0, local.drain_rq(), "a clone counts as a reader");

        drop(clone);
        assert_eq!(1, local.drain_rq(), "the last holder is what releases it");
        assert_eq!(0, local.drain_rq(), "and it is released exactly once");
    }

    #[test]
    fn a_block_comes_back_as_soon_as_nothing_borrows_it() {
        let (local, remote) = pair();
        let mut buffer = writer(Arc::clone(&local));
        let mut read = reader(remote);

        deliver(&mut buffer, &mut read, b"copy");

        let mut sink = [0u8; 4];
        assert_eq!(4, read.read(&mut sink));
        assert_eq!(
            1,
            local.drain_rq(),
            "a copying read leaves nothing borrowing the block"
        );
    }

    #[test]
    fn dropping_the_read_buffer_gives_back_what_it_drained() {
        let (local, remote) = pair();
        let mut buffer = writer(Arc::clone(&local));
        let mut read = reader(remote);

        deliver(&mut buffer, &mut read, b"never read");
        assert_eq!(0, local.drain_rq());

        drop(read);
        assert_eq!(1, local.drain_rq(), "a block nobody read still goes back");
    }

    #[test]
    fn a_zero_copy_read_can_outlive_the_reader() {
        let (local, remote) = pair();
        let mut buffer = writer(local);
        let mut read = reader(remote);

        buffer.write_bytes(b"payload").unwrap();
        read.inbox().add_pending(take_root(&mut buffer));
        read.drain_pending().unwrap();

        let owned = read.read_exact_bytes(7).unwrap().into_bytes();
        drop(read);
        assert_eq!(b"payload"[..], owned[..]);
    }

    #[test]
    fn the_owner_cannot_reuse_a_block_a_reader_is_still_holding() {
        let (local, remote) = small_pair();
        let mut buffer = writer(Arc::clone(&local));
        let mut read = reader(remote);

        deliver(&mut buffer, &mut read, b"still being read");
        let held = read.read_exact_bytes(16).unwrap().into_bytes();

        // The stream closes under the reader while a borrow is outstanding.
        drop(read);
        assert_eq!(
            0,
            local.drain_rq(),
            "a borrowed block is not the owner's to reclaim"
        );

        // The owner now takes everything the segment will give and scribbles over all of it.
        // If the block had gone back, it would be in this set.
        let hogs = exhaust(&local);
        assert!(!hogs.is_empty(), "the owner must have got something");
        for hdr in &hogs {
            // SAFETY: this side allocated them a moment ago and has not freed them.
            unsafe { DynShmBufferHeader::data_mut(*hdr).fill(0xAA) };
        }

        assert_eq!(
            b"still being read"[..],
            held[..],
            "the borrow must still read what the peer wrote"
        );
    }

    #[test]
    fn a_borrow_outlives_a_discard_of_the_rest_of_its_block() {
        let (local, remote) = pair();
        let mut buffer = writer(Arc::clone(&local));
        let mut read = reader(remote);

        deliver(&mut buffer, &mut read, &[b'x'; 64]);

        let head = read.peek(8).unwrap().into_bytes();
        assert_eq!(64, read.discard(64).unwrap());
        assert!(read.is_empty());
        assert_eq!(0, local.drain_rq(), "the borrow still holds the block");

        assert_eq!([b'x'; 8][..], head[..]);
        drop(head);
        assert_eq!(1, local.drain_rq());
    }

    #[test]
    fn discard_releases_the_blocks_it_drains() {
        let (local, remote) = pair();
        let mut buffer = writer(Arc::clone(&local));
        let mut read = reader(remote);

        let body = MAX_BLOCK_SIZE - 64;
        buffer.write_bytes(&vec![b'a'; body]).unwrap();
        buffer.write_bytes(&vec![b'b'; body]).unwrap();
        read.inbox().add_pending(take_root(&mut buffer));
        read.drain_pending().unwrap();

        assert_eq!(body, read.discard(body).unwrap());
        assert_eq!(1, local.drain_rq(), "the drained block goes back at once");
        assert_eq!(body, read.len());
    }

    #[test]
    fn a_long_run_of_messages_keeps_reusing_the_same_memory() {
        let (local, remote) = small_pair();
        let mut buffer = writer(Arc::clone(&local));
        let mut read = reader(remote);

        let payload = vec![b'z'; 200 << 10];
        for round in 0..200 {
            deliver(&mut buffer, &mut read, &payload);
            let mut sink = vec![0u8; payload.len()];
            assert_eq!(
                payload.len(),
                read.read(&mut sink),
                "round {round} came up short"
            );
            assert_eq!(payload, sink, "round {round} read back the wrong bytes");
            assert!(read.is_empty());
        }

        local.drain_rq();
        assert_eq!(0, local.stats().shared_active, "every block came back");
    }

    #[test]
    fn a_long_run_of_borrowed_reads_keeps_reusing_the_same_memory() {
        let (local, remote) = small_pair();
        let mut buffer = writer(Arc::clone(&local));
        let mut read = reader(remote);

        let payload = vec![b'z'; 200 << 10];
        for round in 0..200 {
            deliver(&mut buffer, &mut read, &payload);
            let borrowed = read.read_exact_bytes(payload.len()).unwrap();
            assert!(
                matches!(borrowed, Buf::Shm(_)),
                "round {round} should not have copied"
            );
            assert_eq!(payload[..], borrowed[..]);
            drop(borrowed);
        }

        local.drain_rq();
        assert_eq!(0, local.stats().shared_active, "every block came back");
    }

    #[test]
    fn release_previous_read_is_harmless_when_nothing_was_held_back() {
        let (local, remote) = pair();
        let mut buffer = writer(Arc::clone(&local));
        let mut read = reader(remote);

        deliver(&mut buffer, &mut read, b"released");
        let mut sink = [0u8; 8];
        read.read(&mut sink);

        read.release_previous_read();
        read.release_previous_read();

        assert_eq!(
            1,
            local.drain_rq(),
            "the block still goes back exactly once"
        );
    }

    #[test]
    fn a_fallback_message_reads_back() {
        let (_local, remote) = pair();
        let stats = Arc::new(Stats::default());
        let mut read = V5ReadBuffer::new(
            V5Inbox::new(remote, StreamShared::new(1)),
            Arc::clone(&stats),
        );

        read.inbox()
            .add_fallback(Bytes::from_static(b"over the socket"));
        assert!(read.has_pending());

        assert_eq!(15, read.drain_pending().unwrap());
        assert!(!read.has_pending());

        let buf = read.read_exact_bytes(15).unwrap();
        assert!(
            matches!(buf, Buf::Exm(_)),
            "socket bytes are this side's own"
        );
        assert_eq!(b"over the socket"[..], buf[..]);
        assert!(read.is_empty());
    }

    #[test]
    fn a_fallback_read_borrows_the_delivered_bytes() {
        let (_local, remote) = pair();
        let mut read = reader(remote);

        let payload = Bytes::from(b"delivered over the socket".to_vec());
        let base = payload.as_ptr();
        read.inbox().add_fallback(payload.clone());
        read.drain_pending().unwrap();

        let head = read.read_exact_bytes(9).unwrap();
        assert_eq!(b"delivered"[..], head[..]);
        assert_eq!(base, head.as_ref().as_ptr(), "Bytes::slice must not copy");
    }

    #[test]
    fn shm_and_fallback_inputs_read_back_in_arrival_order() {
        let (local, remote) = pair();
        let mut buffer = writer(local);
        let mut read = reader(remote);

        buffer.write_bytes(b"first").unwrap();
        read.inbox().add_pending(take_root(&mut buffer));
        read.inbox().add_fallback(Bytes::from_static(b"second"));
        buffer.write_bytes(b"third").unwrap();
        read.inbox().add_pending(take_root(&mut buffer));

        assert_eq!(16, read.drain_pending().unwrap());

        let mut all = [0u8; 16];
        assert_eq!(16, read.read(&mut all));
        assert_eq!(b"firstsecondthird"[..], all[..]);
    }

    #[test]
    fn push_batch_delivers_every_offset() {
        let (local, remote) = pair();
        let mut buffer = writer(local);
        let mut read = reader(remote);

        let mut roots = Vec::new();
        for i in 0..4u8 {
            buffer.write_bytes(&[b'a' + i; 4]).unwrap();
            roots.push(take_root(&mut buffer));
        }

        read.inbox().add_pending_batch(&roots);
        assert!(read.has_pending());
        assert_eq!(16, read.drain_pending().unwrap());

        let mut all = [0u8; 16];
        assert_eq!(16, read.read(&mut all));
        assert_eq!(b"aaaabbbbccccdddd"[..], all[..]);

        read.inbox().add_pending_batch(&[]);
        assert!(!read.has_pending(), "an empty batch is nothing to deliver");
        assert_eq!(0, read.drain_pending().unwrap());
    }

    #[test]
    fn a_cyclic_chain_is_reported_rather_than_followed() {
        let (local, remote) = pair();
        let mut read = reader(remote);

        let first = local.alloc(64).unwrap();
        let second = local.alloc(64).unwrap();
        let root = unsafe {
            (*first).next = (*second).offset;
            (*second).next = (*first).offset;
            (*first).offset
        };

        read.inbox().add_pending(root);
        assert!(matches!(read.drain_pending(), Err(Error::BrokenBuffer)));

        // The blocks walked before the cycle was spotted are still this buffer's, and go back
        // with it rather than leaking in the peer's segment.
        drop(read);
        assert_eq!(2, local.drain_rq());
    }

    #[test]
    fn a_chain_naming_an_unallocated_block_is_refused() {
        let (local, remote) = pair();
        let mut read = reader(remote);

        let hdr = local.alloc(32).unwrap();
        let offset = unsafe { (*hdr).offset };
        // Freed before the reader ever looks at it, so the offset is stale.
        local.free_at(offset);

        read.inbox().add_pending(offset);
        assert!(matches!(read.drain_pending(), Err(Error::BrokenBuffer)));
        assert!(read.is_empty(), "nothing was made readable");
    }

    #[test]
    fn a_chain_that_breaks_halfway_still_gives_back_what_it_pinned() {
        let (local, remote) = pair();
        let mut read = reader(remote);

        let first = local.alloc(32).unwrap();
        let second = local.alloc(32).unwrap();
        let (root, tail) = unsafe {
            (*first).next = (*second).offset;
            ((*first).offset, (*second).offset)
        };
        // The chain now names a block the allocator no longer holds.
        local.free_at(tail);

        read.inbox().add_pending(root);
        assert!(matches!(read.drain_pending(), Err(Error::BrokenBuffer)));

        drop(read);
        assert_eq!(
            1,
            local.drain_rq(),
            "the block taken before the break comes back"
        );
    }

    #[test]
    fn an_orphaned_inbox_returns_what_it_was_given() {
        let (local, remote) = pair();
        let mut buffer = writer(Arc::clone(&local));
        let read = reader(remote);

        buffer.write_bytes(b"undelivered").unwrap();
        let root = take_root(&mut buffer);
        let inbox = Arc::clone(read.inbox());

        drop(read);
        inbox.add_pending(root);
        drop(inbox);

        assert_eq!(
            1,
            local.drain_rq(),
            "the undelivered block goes back, exactly once"
        );
        assert_eq!(0, local.stats().shared_active);
    }

    #[test]
    fn a_hand_over_that_fails_gives_the_blocks_back() {
        let (local, _remote) = pair();
        let mut buffer = writer(Arc::clone(&local));

        buffer.write_bytes(b"unsent").unwrap();
        let outgoing = buffer
            .take_shm()
            .unwrap()
            .expect("the write went to shared memory");
        assert!(buffer.is_empty(), "taken means no longer buffered");
        assert_eq!(
            1,
            local.stats().shared_active,
            "still this side's until the put succeeds"
        );

        drop(outgoing);
        assert_eq!(0, local.stats().shared_active);
    }

    #[test]
    fn recycling_a_hand_over_keeps_its_vector() {
        let (local, _remote) = pair();
        let mut buffer = writer(local);

        buffer.write_bytes(b"sent").unwrap();
        let capacity = buffer.pending.capacity();
        let outgoing = buffer.take_shm().unwrap().unwrap();
        assert_eq!(
            0,
            buffer.pending.capacity(),
            "the vector left with the message"
        );

        buffer.recycle(outgoing);
        assert_eq!(capacity, buffer.pending.capacity());
        assert!(buffer.pending.is_empty());
    }

    #[test]
    fn a_pin_exists_only_once_a_read_borrows_the_block() {
        let (local, remote) = pair();
        let mut buffer = writer(local);
        let mut read = reader(remote);

        let body = MAX_BLOCK_SIZE - 64;
        buffer.write_bytes(&vec![b'a'; body]).unwrap();
        buffer.write_bytes(&vec![b'b'; body]).unwrap();
        read.inbox().add_pending(take_root(&mut buffer));
        read.drain_pending().unwrap();

        let pinned = |read: &V5ReadBuffer| {
            read.ready
                .iter()
                .filter(|slice| matches!(slice.src, ChunkSrc::Shm { pin: Some(_), .. }))
                .count()
        };
        assert_eq!(0, pinned(&read), "delivery alone pins nothing");

        let head = read.peek(8).unwrap();
        assert_eq!(1, pinned(&read), "a borrow pins its own block and no other");
        drop(head);

        let mut sink = vec![0u8; body];
        assert_eq!(body, read.read(&mut sink));
        assert_eq!(
            0,
            pinned(&read),
            "a copying read leaves the next block unpinned"
        );
    }
}
