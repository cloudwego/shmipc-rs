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
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use anyhow::anyhow;

use crate::{
    consts::{PAGE_SIZE, QUEUE_HEADER_SIZE},
    v5::{
        memfile::MemFile,
        mpsc::{Mpsc, PopResult, PushError, mpsc_cap, mpsc_mem_size},
    },
};

/// The peer is opening a new stream.
#[allow(dead_code)]
pub const OP_OPEN_STREAM: u8 = 1;
/// The peer is closing a stream it opened.
pub const OP_CLOSE_STREAM: u8 = 2;
/// The peer has written data for a stream; the element carries the buffer offset.
pub const OP_STREAM_DATA: u8 = 3;

/// The queue header, which sits ahead of the ring in the same mapping.
///
/// It holds the cross-process wakeup state, padded to a full cache line so it does not share one
/// with the ring header behind it.
#[repr(C)]
struct QueueHeader {
    /// Non-zero while the peer consumer is awake and draining, so producers know not to spend
    /// another eventfd wakeup on it.
    working_flag: AtomicU32,
    _padding: [u8; 60],
}

/// One queue entry: a fixed 16 byte command addressed to a stream.
///
/// The layout is shared with the peer, so field order and padding are part of the protocol.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Element {
    /// Stream the command applies to.
    pub stream_id: u32,
    /// One of the `OP_*` constants.
    pub op: u8,
    /// Explicit padding, so the layout is the same on both peers.
    pub unused: [u8; 3],
    /// Operation payload. For [`OP_STREAM_DATA`] this is a buffer offset in native byte order;
    /// see [`offset`](Self::offset).
    pub data: [u8; 8],
}

impl Element {
    /// Builds an element with an empty payload.
    pub const fn new(stream_id: u32, op: u8) -> Self {
        Self {
            stream_id,
            op,
            unused: [0; 3],
            data: [0; 8],
        }
    }

    /// Builds an element carrying a buffer offset as its payload.
    pub const fn with_offset(stream_id: u32, op: u8, offset: i64) -> Self {
        Self {
            stream_id,
            op,
            unused: [0; 3],
            data: offset.to_ne_bytes(),
        }
    }

    /// Reads the payload back as a buffer offset.
    ///
    /// The bytes are interpreted in native order, which is sound because both peers run on the same
    /// host and share the mapping.
    pub const fn offset(self) -> i64 {
        i64::from_ne_bytes(self.data)
    }

    /// Overwrites the payload with a buffer offset.
    #[allow(unused)]
    pub const fn set_offset(&mut self, offset: i64) {
        self.data = offset.to_ne_bytes();
    }
}

/// Bytes required for a queue holding at least `cap` elements.
pub const fn queue_mem_size(cap: usize) -> usize {
    QUEUE_HEADER_SIZE + mpsc_mem_size::<Element>(cap)
}

/// Elements a mapping of `mapping_len` bytes holds, rounded down to a power of two.
///
/// Zero when the mapping cannot hold the header, when the remainder is not a whole number of pages,
/// or when no slot fits.
pub const fn queue_cap(mapping_len: usize) -> usize {
    if mapping_len < QUEUE_HEADER_SIZE {
        return 0;
    }

    let ring_len = mapping_len - QUEUE_HEADER_SIZE;
    if !ring_len.is_multiple_of(PAGE_SIZE) {
        return 0;
    }

    mpsc_cap::<Element>(ring_len)
}

/// Checks that a received mapping is large enough for the header plus at least one page of ring,
/// and that the ring part is a whole number of pages.
fn validate_queue_size(size: usize) -> Result<(), anyhow::Error> {
    if size < QUEUE_HEADER_SIZE + PAGE_SIZE {
        return Err(anyhow!("invalid queue size: {}", size));
    }

    let ring_size = size - QUEUE_HEADER_SIZE;

    if !ring_size.is_multiple_of(PAGE_SIZE) {
        return Err(anyhow!("invalid queue size: {}", size));
    }

    Ok(())
}

/// The shared state behind one direction of the connection: a header, a ring, and the local close
/// flag, all over a single memfd.
///
/// [`SendQueue`] and [`RecvQueue`] are the two roles a peer can take on it.
#[derive(Debug)]
struct DataQueue {
    /// The mapping both peers share. Holds `header` and the ring bytes, and keeps them alive.
    file: MemFile,
    /// Points at the front of `file`'s mapping.
    header: *mut QueueHeader,
    /// The ring, mapped over the bytes after the header.
    ring: Mpsc<Element>,
    /// Local shutdown flag. Private to this process, not shared with the peer.
    closed: AtomicBool,
}

unsafe impl Send for DataQueue {}
unsafe impl Sync for DataQueue {}

impl DataQueue {
    /// Creates the shared queue and initializes an empty header/ring.
    fn create(name: &str, cap: usize) -> Result<Self, anyhow::Error> {
        let size = queue_mem_size(cap);
        let mut file = MemFile::new(name, size)?;

        let (header_bytes, ring_bytes) = file.data_mut().split_at_mut(QUEUE_HEADER_SIZE);

        header_bytes.fill(0);

        let header = header_bytes.as_mut_ptr().cast::<QueueHeader>();
        let ring = Mpsc::<Element>::new(ring_bytes)?;

        Ok(Self {
            file,
            header,
            ring,
            closed: AtomicBool::new(false),
        })
    }

    /// Maps a queue a peer created, validating that its size matches what this side expects.
    fn map(fd: OwnedFd) -> Result<Self, anyhow::Error> {
        let mut file = MemFile::map(fd)?;

        let size = file.size();
        validate_queue_size(size)?;

        if queue_cap(size) == 0 {
            return Err(anyhow!("queue mapping is too small: {}", size));
        }

        let (header_bytes, ring_bytes) = file.data_mut().split_at_mut(QUEUE_HEADER_SIZE);
        let header = header_bytes.as_mut_ptr().cast::<QueueHeader>();
        let ring = Mpsc::<Element>::map(ring_bytes.as_mut_ptr(), ring_bytes.len())?;

        Ok(Self {
            file,
            header,
            ring,
            closed: AtomicBool::new(false),
        })
    }

    /// Borrows the descriptor, which is what gets sent to the peer so it can map the same object.
    fn fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// Appends an element, failing rather than waiting when the ring is full or the queue is
    /// closed.
    fn push(&self, element: Element) -> Result<(), PushError> {
        if self.is_closed() {
            return Err(PushError::Closed);
        }

        if self.ring.try_push(element) {
            Ok(())
        } else {
            Err(PushError::Full)
        }
    }

    /// Borrows the header at the front of the mapping.
    fn header(&self) -> &QueueHeader {
        unsafe { &*self.header }
    }

    /// Pops the head element, spinning while a producer holds it unpublished.
    ///
    /// `Ok(None)` means empty; an error means the queue was closed locally.
    fn pop(&mut self) -> Result<Option<Element>, anyhow::Error> {
        if self.is_closed() {
            return Err(anyhow!("queue is closed"));
        }

        Ok(self.ring.pop())
    }

    /// Pops the head element without spinning, distinguishing an empty ring from one whose head is
    /// still being filled.
    fn try_pop(&mut self) -> Result<PopResult<Element>, anyhow::Error> {
        if self.is_closed() {
            return Err(anyhow!("queue is closed"));
        }

        Ok(self.ring.try_pop())
    }

    /// Returns whether the peer consumer is currently marked working.
    fn consumer_working(&self) -> bool {
        self.header().working_flag.load(Ordering::Acquire) != 0
    }

    /// Moves the working flag from 0 to 1 with a compare-exchange.
    ///
    /// Returns `true` for exactly the one producer that wins the transition, which is the one that
    /// must then issue the eventfd wakeup; every other producer sees `false` and skips the syscall
    /// because the consumer is already awake. Also `false` once the queue is closed locally, since
    /// there is nothing left to wake for.
    fn mark_working(&self) -> bool {
        if self.is_closed() {
            return false;
        }

        self.header()
            .working_flag
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::Acquire)
            .is_ok()
    }

    /// Clears the working flag, then re-checks for work published during the gap.
    ///
    /// Returns `true` when the queue is genuinely drained and the consumer may sleep. If work
    /// appeared between the store and the check, the flag is set again and `false` is returned, so
    /// a producer that saw the flag still set and skipped its wakeup does not leave the consumer
    /// asleep on a non-empty queue. A closed queue reports drained.
    fn mark_not_working(&self) -> bool {
        if self.is_closed() {
            return true;
        }
        let header = self.header();
        header.working_flag.store(0, Ordering::SeqCst);
        if !self.ring.head_ready() {
            return true;
        }
        header.working_flag.store(1, Ordering::SeqCst);
        false
    }

    /// Bytes of the whole mapping, header included.
    fn size(&self) -> usize {
        self.file.size()
    }

    /// Positions claimed but not yet popped, published or not.
    fn len(&self) -> usize {
        self.ring.len()
    }

    /// Elements the ring holds when full.
    fn cap(&self) -> usize {
        self.ring.cap()
    }

    /// Marks this side shut down. Idempotent, and invisible to the peer.
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Whether this side has been closed.
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// The producing end of a queue: the side that creates the shared object and hands its descriptor
/// to the peer.
///
/// The ring accepts multiple producers, so this can be shared across threads (behind an `Arc`) and
/// used concurrently.
#[derive(Debug)]
pub struct SendQueue {
    inner: DataQueue,
}

impl SendQueue {
    /// Creates a new shared queue holding at least `cap` elements, rounded up to a power of two.
    ///
    /// `name` is only a debugging aid; the peer receives the descriptor, not the name.
    pub fn new(name: &str, cap: u32) -> Result<Self, anyhow::Error> {
        Ok(Self {
            inner: DataQueue::create(name, cap as usize)?,
        })
    }

    /// Borrows the descriptor to send to the peer, which maps it as a [`RecvQueue`].
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.inner.fd()
    }

    /// Appends an element, returning [`PushError::Full`] rather than waiting when the ring is full.
    ///
    /// This does not wake the peer consumer; pair it with
    /// [`claim_peer_consumer`](Self::claim_peer_consumer).
    pub fn put(&self, element: Element) -> Result<(), PushError> {
        if self.inner.is_closed() {
            return Err(PushError::Closed);
        }
        self.inner.push(element)
    }

    /// Claims the peer consumer's wakeup state, so exactly one producer pays for the wakeup.
    ///
    /// `true` means this caller won the claim and must write to the eventfd. `false` means the
    /// consumer is already marked working (or this queue is closed) and no wakeup is needed. Call
    /// it after [`put`](Self::put), so the element is visible by the time the consumer runs.
    pub fn claim_peer_consumer(&self) -> bool {
        self.inner.mark_working()
    }

    /// Whether the peer consumer is currently marked working.
    #[allow(unused)]
    pub fn peer_consumer_working(&self) -> bool {
        self.inner.consumer_working()
    }

    /// Returns the number of reserved positions in the queue.
    ///
    /// This may include positions whose payload has not been published yet.
    #[allow(unused)]
    pub fn pending(&self) -> usize {
        self.inner.len()
    }

    /// Elements the queue holds when full.
    #[allow(unused)]
    pub fn cap(&self) -> usize {
        self.inner.cap()
    }

    /// Bytes of the shared mapping, header included.
    pub fn size(&self) -> usize {
        self.inner.size()
    }

    /// Stops this side from producing. Idempotent, and not visible to the peer, which keeps
    /// draining whatever was already published.
    pub fn close(&self) {
        self.inner.close()
    }

    /// Whether [`close`](Self::close) has been called.
    #[allow(unused)]
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}

/// The consuming end of a queue, mapped from a descriptor the peer sent.
///
/// The ring has a single consumer, so all popping must happen from one thread at a time.
#[derive(Debug)]
pub struct RecvQueue {
    inner: DataQueue,
}

impl RecvQueue {
    /// Maps a queue the peer created, taking ownership of its descriptor.
    ///
    /// Fails if the mapping is not the size a queue of this layout would be.
    pub fn map(fd: OwnedFd) -> Result<Self, anyhow::Error> {
        Ok(Self {
            inner: DataQueue::map(fd)?,
        })
    }

    /// Pops the head element without spinning, telling an empty queue apart from one whose head a
    /// producer is still filling. Errors once this side is closed.
    pub fn try_pop(&mut self) -> Result<PopResult<Element>, anyhow::Error> {
        self.inner.try_pop()
    }

    /// Pops the head element, spinning while a producer holds it unpublished.
    ///
    /// `Ok(None)` means the queue is empty. Errors once this side is closed.
    #[allow(unused)]
    pub fn pop(&mut self) -> Result<Option<Element>, anyhow::Error> {
        self.inner.pop()
    }

    /// Marks the consumer as not working and re-checks the queue.
    ///
    /// Returns `true` when the current head is not ready and the consumer may sleep.
    /// Returns `false` when the head is ready and draining must continue.
    pub fn finish_draining(&self) -> bool {
        self.inner.mark_not_working()
    }

    /// Positions the producers have claimed but this side has not popped yet, published or not.
    #[allow(unused)]
    pub fn pending(&self) -> usize {
        self.inner.len()
    }

    /// Bytes of the shared mapping, header included.
    #[allow(unused)]
    pub fn size(&self) -> usize {
        self.inner.size()
    }

    /// Stops this side from consuming. Idempotent, and not visible to the peer, which keeps
    /// producing into a queue nobody drains.
    pub fn close(&self) {
        self.inner.close()
    }

    /// Whether [`close`](Self::close) has been called.
    #[allow(unused)]
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{
        collections::HashSet,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use nix::unistd::dup;

    use super::{Element, OP_STREAM_DATA, RecvQueue, SendQueue};
    use crate::v5::mpsc::PushError;

    const TEST_CAP: u32 = 1024;

    // Tests run in parallel under `cargo test`, so don't reuse the same
    // shared-memory name between tests.
    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn test_name(prefix: &str) -> String {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{}-{id}", std::process::id())
    }

    fn new_queue(prefix: &str) -> SendQueue {
        SendQueue::new(&test_name(prefix), TEST_CAP).unwrap()
    }

    fn map_recv(send: &SendQueue) -> RecvQueue {
        let fd = dup(send.fd()).unwrap();
        RecvQueue::map(fd).unwrap()
    }

    #[test]
    fn put_pop() {
        let send = new_queue("put-pop");
        let mut recv = map_recv(&send);

        let expected = Element {
            stream_id: 1,
            op: 2,
            data: [1, 2, 3, 4, 0, 0, 0, 0],
            ..Element::default()
        };
        send.put(expected).unwrap();

        assert_eq!(recv.pop().unwrap(), Some(expected));
    }

    #[test]
    fn empty() {
        let send = new_queue("empty");
        let mut recv = map_recv(&send);

        assert_eq!(recv.pop().unwrap(), None);
        assert_eq!(recv.pending(), 0);
    }

    #[test]
    fn full() {
        let send = new_queue("full");
        let mut recv = map_recv(&send);
        let capacity = send.cap();

        for i in 0..capacity {
            send.put(Element::new(i as u32, OP_STREAM_DATA)).unwrap();
        }
        assert_eq!(send.pending(), capacity);
        assert_eq!(
            send.put(Element::new(999, OP_STREAM_DATA)),
            Err(PushError::Full)
        );

        for i in 0..capacity {
            assert_eq!(
                recv.pop().unwrap(),
                Some(Element::new(i as u32, OP_STREAM_DATA))
            );
        }
        assert_eq!(recv.pop().unwrap(), None);
        assert_eq!(recv.pending(), 0);

        let expected = Element::new(1000, OP_STREAM_DATA);
        send.put(expected).unwrap();
        assert_eq!(recv.pop().unwrap(), Some(expected));
    }

    #[test]
    fn multiple() {
        let send = new_queue("multiple");
        let mut recv = map_recv(&send);

        for i in 0..100u32 {
            send.put(Element::new(i, (i % 256) as u8)).unwrap();
        }
        assert_eq!(send.pending(), 100);

        let mut got = Vec::with_capacity(100);
        while let Some(element) = recv.pop().unwrap() {
            got.push(element);
        }

        assert_eq!(got.len(), 100);
        for i in 0..100u32 {
            assert_eq!(got[i as usize].stream_id, i);
            assert_eq!(got[i as usize].op, (i % 256) as u8);
        }
        assert_eq!(recv.pending(), 0);
    }

    #[test]
    fn working_flag() {
        let send = new_queue("working");
        let recv = map_recv(&send);

        assert!(!send.peer_consumer_working());

        assert!(send.claim_peer_consumer());
        assert!(send.peer_consumer_working());

        assert!(!send.claim_peer_consumer());
        assert!(send.peer_consumer_working());

        assert!(recv.finish_draining());
        assert!(!send.peer_consumer_working());
    }

    #[test]
    fn working_flag_with_pending() {
        let send = new_queue("working-pending");
        let recv = map_recv(&send);

        send.put(Element::new(1, OP_STREAM_DATA)).unwrap();
        assert!(send.claim_peer_consumer());

        // There is still work pending, so the consumer must not sleep.
        assert!(!recv.finish_draining());
        assert!(send.peer_consumer_working());
    }

    #[test]
    fn map() {
        let send = new_queue("map");
        let expected = Element::new(42, 3);

        send.put(expected).unwrap();

        let mut recv = map_recv(&send);
        assert_eq!(recv.pop().unwrap(), Some(expected));
    }

    #[test]
    fn concurrent() {
        let send = Arc::new(SendQueue::new(&test_name("concurrent"), TEST_CAP).unwrap());
        let mut recv = map_recv(&send);

        let num_producers = 4;
        let per_producer = 100;
        let mut handles = Vec::with_capacity(num_producers);

        for producer in 0..num_producers {
            let send = Arc::clone(&send);

            handles.push(thread::spawn(move || {
                let base = producer * per_producer;

                for i in 0..per_producer {
                    let stream_id = (base + i) as u32;
                    send.put(Element::new(stream_id, OP_STREAM_DATA)).unwrap();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let mut seen = HashSet::with_capacity(num_producers * per_producer);
        while let Some(element) = recv.pop().unwrap() {
            seen.insert(element.stream_id);
        }

        assert_eq!(seen.len(), num_producers * per_producer);
    }

    #[test]
    fn concurrent_near_full_is_bounded() {
        let send = Arc::new(SendQueue::new(&test_name("concurrent-near-full"), TEST_CAP).unwrap());
        let mut recv = map_recv(&send);
        let capacity = send.cap();

        for i in 0..capacity - 1 {
            send.put(Element::new(i as u32, OP_STREAM_DATA)).unwrap();
        }
        assert_eq!(send.pending(), capacity - 1);

        let mut handles = Vec::with_capacity(2);

        for id in 0..2u32 {
            let send = Arc::clone(&send);

            handles.push(thread::spawn(move || {
                send.put(Element::new(10_000 + id, OP_STREAM_DATA))
            }));
        }

        let mut success = 0;
        let mut full = 0;

        for handle in handles {
            match handle.join().unwrap() {
                Ok(()) => success += 1,
                Err(PushError::Full) => full += 1,
                Err(error) => panic!("unexpected push result: {error:?}"),
            }
        }

        assert_eq!(success, 1);
        assert_eq!(full, 1);
        assert_eq!(send.pending(), capacity);

        while recv.pop().unwrap().is_some() {}
        assert_eq!(recv.pending(), 0);
    }

    #[test]
    fn mem_size() {
        let size = super::queue_mem_size(1024);
        assert!(size > 0);
    }

    #[test]
    fn new_queue_cap() {
        // Power of two: exact match.
        let queue = SendQueue::new(&test_name("cap-exact"), 2048).unwrap();
        assert_eq!(queue.cap(), 2048);

        // Non-power of two: rounds up.
        let queue = SendQueue::new(&test_name("cap-round-up"), 1000).unwrap();
        assert_eq!(queue.cap(), 1024);
    }

    #[test]
    fn close_is_idempotent_and_rejects_operations() {
        let send = SendQueue::new(&test_name("close"), 16).unwrap();

        assert!(!send.is_closed());
        send.close();
        assert!(send.is_closed());
        send.close();
        assert!(send.is_closed());

        assert_eq!(send.put(Element::default()), Err(PushError::Closed));

        // Producer-side working-flag operations must stay safe after close.
        assert!(!send.claim_peer_consumer());
    }

    #[test]
    fn close_concurrent_with_operations() {
        let send = Arc::new(SendQueue::new(&test_name("concurrent-close"), 16).unwrap());
        let worker_queue = Arc::clone(&send);

        let started = Arc::new(std::sync::Barrier::new(2));
        let worker_started = Arc::clone(&started);

        let handle = thread::spawn(move || {
            worker_started.wait();

            loop {
                match worker_queue.put(Element::new(1, OP_STREAM_DATA)) {
                    Ok(()) => {}
                    Err(PushError::Full) => thread::yield_now(),
                    Err(PushError::Closed) => return,
                }
            }
        });

        started.wait();
        thread::sleep(Duration::from_millis(1));
        send.close();

        let deadline = Instant::now() + Duration::from_secs(1);

        while !handle.is_finished() {
            assert!(
                Instant::now() < deadline,
                "operation did not observe queue close"
            );
            thread::yield_now();
        }

        handle.join().unwrap();
    }
}
