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
    marker::PhantomData,
    mem::{align_of, size_of},
    ptr,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::consts::{MPSC_HEADER_SIZE, PAGE_SIZE};

/// The strongest alignment a payload may require, and therefore the alignment the mapping base
/// must have.
const MAX_PAYLOAD_ALIGN: usize = 8;
/// Bytes the turn counter occupies at the front of every slot; the payload follows it.
const SLOT_HEADER_SIZE: usize = 8;

/// The ring header, as both peers lay it out.
///
/// `head` and `tail` are monotonically increasing logical positions, not slot indices: a slot index
/// is `position & mask`. Each is padded out to 64 bytes so that over a page-aligned mapping the
/// single consumer advancing `head` shares no cache line with the producers claiming `tail`.
#[repr(C)]
pub struct MpscHeader {
    /// Next position the consumer will read. Only the consumer advances it.
    pub head: AtomicU64,
    _head_padding: [u8; 56],
    /// Next position a producer will claim. Producers advance it with a compare-exchange.
    pub tail: AtomicU64,
    _tail_padding: [u8; 56],
}

/// Why a region of memory cannot back a ring.
///
/// Both peers derive their geometry from the mapping alone, so a mapping that fails these checks
/// would give the two sides different views of the same bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("a ring mapping of {0} bytes is not a whole number of 4096 byte pages")]
    Unaligned(usize),
    #[error("a ring mapping at {0:#x} is not eight byte aligned")]
    Misaligned(usize),
    #[error("a ring mapping of {0} bytes holds no slots")]
    TooSmall(usize),
    #[error("a payload aligned to {0} bytes does not fit a slot")]
    PayloadOverAligned(usize),
}

/// Why a push did not take effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PushError {
    #[error("queue is full")]
    Full,
    #[error("queue is closed")]
    Closed,
}

/// Bytes one slot occupies: a turn counter plus the payload, rounded up to a power of
/// two so a position maps to an offset with a shift.
pub const fn slot_size<T>() -> usize {
    (SLOT_HEADER_SIZE + size_of::<T>()).next_power_of_two()
}

/// Bytes a ring holding at least `cap` payloads occupies, page aligned.
pub const fn mpsc_mem_size<T>(cap: usize) -> usize {
    let size = MPSC_HEADER_SIZE + cap.next_power_of_two() * slot_size::<T>();
    (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// Payloads a mapping of `mapping_len` bytes holds, which is what both peers derive
/// their geometry from. Zero if no slot fits.
///
/// The slot count is rounded *down* to a power of two so a position masks to a slot index, which
/// means a mapping can end with bytes no slot covers.
pub const fn mpsc_cap<T>(mapping_len: usize) -> usize {
    let slots = mapping_len.saturating_sub(MPSC_HEADER_SIZE) / slot_size::<T>();
    if slots == 0 { 0 } else { 1 << slots.ilog2() }
}

/// A bounded multi-producer, single-consumer ring living in a shared mapping.
///
/// The ring owns no memory: `base` points into a mapping both peers hold, and the geometry is
/// recomputed from the mapping length on each side rather than exchanged, so the two views always
/// agree. Apart from a producer-side copy of `head`, every field is derived state; all state the
/// peers share lives in the shared bytes.
///
/// Each slot carries a turn counter that doubles as a handoff: `generation << 1` means the slot is
/// free for the producer of that lap, `(generation << 1) | 1` means the payload is published and
/// the consumer may read it.
#[derive(Debug)]
pub struct Mpsc<T> {
    /// Start of the mapping, where the [`MpscHeader`] sits.
    base: *mut u8,
    /// Length of the mapping in bytes. Only reported through [`Debug`].
    #[allow(dead_code)]
    len: usize,
    /// `log2(capacity)`, used to turn a position into its lap number.
    cap_shift: u8,
    /// `log2(slot_size::<T>())`, used to turn a slot index into a byte offset.
    slot_shift: u8,
    /// `capacity - 1`, used to turn a position into a slot index.
    mask: u64,
    /// The newest `head` a push on this mapping has seen; see [`try_push`](Self::try_push).
    head_cache: AtomicU64,
    _payload: PhantomData<T>,
}

unsafe impl<T: Send> Send for Mpsc<T> {}
unsafe impl<T: Send> Sync for Mpsc<T> {}

/// The outcome of a non-blocking pop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopResult<T> {
    /// The ring holds no claimed positions.
    Empty,
    /// A producer has claimed the head position but has not published its payload yet. The ring is
    /// not empty; retrying later will yield the value.
    HeadNotReady,
    /// The head payload, now removed from the ring.
    Ready(T),
}

impl<T: Copy> Mpsc<T> {
    /// Initializes a ring in memory, zeroing it.
    ///
    /// Zeroing is what puts the ring in its empty state: head and tail start at zero, and every
    /// slot turn counter starts at zero, which is lap zero's "free to write" value. Only the peer
    /// that creates the mapping may call this; the other side uses [`map`](Self::map).
    pub fn new(memory: &mut [u8]) -> Result<Self, LayoutError> {
        let ring = Self::from_raw_parts(memory.as_mut_ptr(), memory.len())?;

        unsafe {
            ptr::write_bytes(memory.as_mut_ptr(), 0, memory.len());
        }

        Ok(ring)
    }

    /// Maps an existing ring from shared memory without modifying it.
    ///
    /// The geometry is rederived from `len`, so it matches the creating peer as long as both map
    /// the same number of bytes.
    ///
    /// The caller guarantees `base` stays valid and mapped for at least `len` bytes for as long as
    /// the returned ring lives, and that the peer initialized the region with [`new`](Self::new)
    /// before publishing it.
    pub fn map(base: *mut u8, len: usize) -> Result<Self, LayoutError> {
        Self::from_raw_parts(base, len)
    }

    /// Validates a region and derives the addressing geometry from it, touching no memory.
    fn from_raw_parts(base: *mut u8, len: usize) -> Result<Self, LayoutError> {
        if align_of::<T>() > MAX_PAYLOAD_ALIGN {
            return Err(LayoutError::PayloadOverAligned(align_of::<T>()));
        }

        if !len.is_multiple_of(PAGE_SIZE) {
            return Err(LayoutError::Unaligned(len));
        }

        if len < MPSC_HEADER_SIZE {
            return Err(LayoutError::TooSmall(len));
        }

        if !(base as usize).is_multiple_of(MAX_PAYLOAD_ALIGN) {
            return Err(LayoutError::Misaligned(base as usize));
        }

        let slot_shift = slot_size::<T>().ilog2() as u8;
        let raw_cap = (len - MPSC_HEADER_SIZE) >> slot_shift;
        if raw_cap == 0 {
            return Err(LayoutError::TooSmall(len));
        }
        let cap_shift = raw_cap.ilog2() as u8;
        let capacity = 1usize << cap_shift;

        Ok(Self {
            base,
            len,
            cap_shift,
            slot_shift,
            mask: (capacity - 1) as u64,
            head_cache: AtomicU64::new(0),
            _payload: PhantomData,
        })
    }

    /// Borrows the header at the start of the mapping.
    fn header(&self) -> &MpscHeader {
        // SAFETY:
        // - `self.base` points to a valid MpscHeader.
        // - `self.base` is properly aligned for MpscHeader.
        // - The mapping remains valid for the lifetime of `self`.
        unsafe { &*self.base.cast::<MpscHeader>() }
    }

    /// The header, for a reader that holds no reference to the ring.
    ///
    /// The header lives in the shared mapping and is only ever touched through its atomics, so
    /// reading `head` and `tail` through this pointer is sound.
    pub(crate) fn header_ptr(&self) -> *const MpscHeader {
        self.base.cast::<MpscHeader>()
    }

    /// Resolves a logical position to its slot: the start of the slot and its turn counter.
    ///
    /// Positions beyond the capacity wrap onto the same slots, one lap later.
    fn slot_ptr(&self, position: u64) -> (*mut u8, &AtomicU64) {
        let slot_index = (position & self.mask) as usize;
        let offset = MPSC_HEADER_SIZE + (slot_index << self.slot_shift);

        // SAFETY:
        // - `base` points to the start of the MPSC mapping.
        // - `MPSC_HEADER_SIZE` skips the ring header.
        // - `position & mask` is always less than the ring capacity.
        // - `slot_size::<T>()` is the size of one complete slot.
        // - The mapping was validated to contain all slots.
        // - The slot starts at an address aligned for AtomicU64.
        unsafe {
            let slot = self.base.add(offset);
            let turn = &*slot.cast::<AtomicU64>();
            (slot, turn)
        }
    }

    /// Returns the turn/generation for a logical queue position.
    /// A slot is reused once per capacity, so this is `position / capacity`.
    fn turn(&self, position: u64) -> u64 {
        position >> self.cap_shift
    }

    /// Pushes a value, spinning until the ring has room.
    ///
    /// Never returns while the ring stays full, so only call it where a consumer is guaranteed to
    /// be draining. Prefer [`try_push`](Self::try_push) when the caller can back off.
    #[allow(unused)]
    pub fn push(&self, value: T) {
        while !self.try_push(value) {
            std::thread::yield_now();
        }
    }

    /// Pushes a value, returning `false` if the ring is full.
    ///
    /// Only the initial claim is non-blocking: once a position is claimed the producer must see it
    /// through, and it spins waiting for the previous lap's consumer to release the slot before
    /// writing. A claimed position is already counted by [`len`](Self::len) and already blocks the
    /// consumer at that position, which is what [`PopResult::HeadNotReady`] reports.
    pub fn try_push(&self, value: T) -> bool {
        let header = self.header();

        let mut tail = header.tail.load(Ordering::Acquire);
        let position = loop {
            // The consumer's `head` is re-read only when the ring looks full against the copy
            // this side last saw. Head only grows, so the copy never overstates it: a ring with
            // room against the copy has room against the truth. Every push that skips the read
            // leaves the consumer's cache line alone.
            if tail.wrapping_sub(self.head_cache.load(Ordering::Relaxed)) > self.mask {
                let head = header.head.load(Ordering::Acquire);
                self.head_cache.store(head, Ordering::Relaxed);

                // Queue is full; don't claim a position.
                if tail.wrapping_sub(head) > self.mask {
                    return false;
                }
            }

            match header.tail.compare_exchange_weak(
                tail,
                tail + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break tail,
                Err(current) => tail = current,
            }
        };

        let (slot, turn) = self.slot_ptr(position);
        let generation = self.turn(position);

        // Wait for this slot to become writable.
        while turn.load(Ordering::Acquire) != generation << 1 {
            std::hint::spin_loop();
        }

        // Write the payload.
        //
        // SAFETY: this producer owns the slot after successfully claiming `position`.
        unsafe {
            ptr::write(slot.add(SLOT_HEADER_SIZE).cast::<T>(), value);
        }

        // Publish the value.
        turn.store((generation << 1) | 1, Ordering::Release);

        true
    }

    /// Pops the head value, returning `None` only when the ring is empty.
    ///
    /// Spins while a producer holds the head position without having published it, so a stalled
    /// producer stalls this call. Use [`try_pop`](Self::try_pop) to distinguish the two states.
    ///
    /// Single consumer only: concurrent calls race on the head.
    #[allow(unused)]
    pub fn pop(&mut self) -> Option<T> {
        loop {
            match self.try_pop() {
                PopResult::Empty => return None,
                PopResult::HeadNotReady => std::thread::yield_now(),
                PopResult::Ready(value) => return Some(value),
            }
        }
    }

    /// Pops the head value if one is published, otherwise reports why it could not.
    ///
    /// Single consumer only: concurrent calls race on the head.
    pub fn try_pop(&mut self) -> PopResult<T> {
        let header = self.header();
        let head = header.head.load(Ordering::Acquire);
        let (slot, slot_turn) = self.slot_ptr(head);
        let turn = self.turn(head);

        // Acquire pairs with the producer's Release. An odd turn means the
        // payload has been fully published for this lap. It is checked before
        // `tail`, which the producers write, so a pop that finds its slot
        // ready never pulls their cache line.
        if slot_turn.load(Ordering::Acquire) != ((turn << 1) | 1) {
            // Only `tail` tells an unclaimed head from one a producer is
            // still filling.
            return if head == header.tail.load(Ordering::Acquire) {
                PopResult::Empty
            } else {
                PopResult::HeadNotReady
            };
        }

        // SAFETY: the producer has published this slot by setting its turn to
        // the expected odd value. No producer can reuse the slot until we
        // advance the turn below.
        let value = unsafe { ptr::read(slot.add(SLOT_HEADER_SIZE).cast::<T>()) };

        // Release the slot for the next producer lap before publishing the
        // new head.
        slot_turn.store((turn + 1) << 1, Ordering::Release);
        header.head.store(head + 1, Ordering::Release);

        PopResult::Ready(value)
    }

    /// Whether the head slot holds a published element right now.
    pub fn head_ready(&self) -> bool {
        let header = self.header();
        let head = header.head.load(Ordering::Acquire);

        // A published turn implies a claim, so `tail` need not be read.
        let (_, turn) = self.slot_ptr(head);
        turn.load(Ordering::Acquire) == ((self.turn(head) << 1) | 1)
    }

    /// Number of positions claimed but not yet popped.
    ///
    /// This counts claimed positions, not readable ones, so it includes any position a producer has
    /// reserved but not yet published.
    pub fn len(&self) -> usize {
        let header = self.header();
        let head = header.head.load(Ordering::Acquire);
        let tail = header.tail.load(Ordering::Acquire);

        (tail - head) as usize
    }

    /// Payloads the ring holds when full. Always a power of two.
    pub fn cap(&self) -> usize {
        (self.mask + 1) as usize
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        ptr,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use super::*;

    const TEST_SIZE: usize = 128 * PAGE_SIZE;

    fn drain<T: Copy>(q: &mut Mpsc<T>) -> Vec<T> {
        let mut out = Vec::new();

        while let Some(value) = q.pop() {
            out.push(value);
        }

        out
    }

    #[test]
    fn basic() {
        let mut data = vec![0u8; TEST_SIZE];
        let mut q = Mpsc::<i64>::new(&mut data).unwrap();

        q.push(12345);
        assert_eq!(q.len(), 1);

        assert_eq!(q.pop(), Some(12345));
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn pop_empty() {
        let mut data = vec![0u8; TEST_SIZE];
        let mut q = Mpsc::<i64>::new(&mut data).unwrap();

        assert_eq!(q.pop(), None);
    }

    #[test]
    fn try_pop_head_not_ready() {
        let mut data = vec![0u8; TEST_SIZE];
        let mut q = Mpsc::<i64>::new(&mut data).unwrap();

        // Simulate a producer claiming position 0 without publishing it.
        q.header().tail.store(1, Ordering::Release);

        assert_eq!(q.try_pop(), PopResult::HeadNotReady);
        assert!(!q.head_ready());

        let (slot, turn) = q.slot_ptr(0);

        // Simulate the producer writing the payload.
        unsafe {
            ptr::write(slot.add(SLOT_HEADER_SIZE).cast::<i64>(), 12345);
        }

        // Publish slot 0.
        turn.store(1, Ordering::Release);

        assert!(q.head_ready());

        assert_eq!(q.try_pop(), PopResult::Ready(12345));
        assert_eq!(q.try_pop(), PopResult::Empty);
    }

    #[test]
    fn multiple_push_pop() {
        let mut data = vec![0u8; TEST_SIZE];
        let mut q = Mpsc::<i64>::new(&mut data).unwrap();

        let values = [100, 200, 300, 400, 500];

        for value in values {
            q.push(value);
        }

        assert_eq!(q.len(), 5);
        assert_eq!(drain(&mut q), values);
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn map_existing() {
        let mut data = vec![0u8; TEST_SIZE];

        let mut q1 = Mpsc::<i64>::new(&mut data).unwrap();
        q1.push(999);

        let q2 = Mpsc::<i64>::map(data.as_mut_ptr(), data.len()).unwrap();

        q2.push(888);

        assert_eq!(q1.len(), 2);
        assert_eq!(drain(&mut q1), vec![999, 888]);
    }

    #[test]
    fn concurrent_push() {
        let mut data = vec![0u8; TEST_SIZE];
        let mut q = Arc::new(Mpsc::<i64>::new(&mut data).unwrap());

        let num_producers = 8;
        let pushes_per_producer = 1000;
        let total_expected = num_producers * pushes_per_producer;

        thread::scope(|scope| {
            for producer in 0..num_producers {
                let q = Arc::clone(&q);

                scope.spawn(move || {
                    let base = producer * pushes_per_producer;

                    for i in 0..pushes_per_producer {
                        q.push((base + i) as i64);
                    }
                });
            }
        });

        assert_eq!(q.len(), total_expected);

        let q = Arc::get_mut(&mut q).unwrap();

        let mut seen = HashSet::new();

        while let Some(value) = q.pop() {
            seen.insert(value);
        }

        assert_eq!(seen.len(), total_expected);
    }

    #[test]
    fn concurrent_push_pop() {
        let mut data = vec![0u8; TEST_SIZE];
        let q = Arc::new(Mpsc::<i64>::new(&mut data).unwrap());

        let num_producers = 4;
        let pushes_per_producer = 10_000;
        let total_expected = num_producers * pushes_per_producer;

        let popped = Arc::new(AtomicUsize::new(0));

        let mut consumer_q = Mpsc::<i64>::map(data.as_mut_ptr(), data.len()).unwrap();
        let consumer_popped = Arc::clone(&popped);

        let consumer = thread::spawn(move || {
            loop {
                if consumer_popped.load(Ordering::Relaxed) >= total_expected {
                    break;
                }

                match consumer_q.try_pop() {
                    PopResult::Ready(_) => {
                        consumer_popped.fetch_add(1, Ordering::Relaxed);
                    }
                    PopResult::Empty | PopResult::HeadNotReady => {
                        thread::yield_now();
                    }
                }
            }
        });

        thread::scope(|scope| {
            for producer in 0..num_producers {
                let q = Arc::clone(&q);

                scope.spawn(move || {
                    let base = producer * pushes_per_producer;

                    for i in 0..pushes_per_producer {
                        q.push((base + i) as i64);
                    }
                });
            }
        });

        consumer.join().unwrap();

        assert_eq!(popped.load(Ordering::Relaxed), total_expected);
    }

    #[test]
    fn wraparound() {
        let mut data = vec![0u8; TEST_SIZE];
        let mut q = Mpsc::<i64>::new(&mut data).unwrap();

        for round in 0..5 {
            let batch = q.cap() / 2;
            let start = round * batch;

            for i in 0..batch {
                q.push((start + i) as i64);
            }

            let got = drain(&mut q);

            let expected: Vec<i64> = (start..start + batch).map(|x| x as i64).collect();

            assert_eq!(got, expected);
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestEntry {
        offset: i64,
        size: u32,
        flags: u32,
    }

    #[test]
    fn struct_type() {
        let mut data = vec![0u8; TEST_SIZE];
        let mut q = Mpsc::<TestEntry>::new(&mut data).unwrap();

        let entries = [
            TestEntry {
                offset: 100,
                size: 1024,
                flags: 1,
            },
            TestEntry {
                offset: 200,
                size: 2048,
                flags: 2,
            },
            TestEntry {
                offset: 300,
                size: 4096,
                flags: 3,
            },
        ];

        for entry in entries {
            q.push(entry);
        }

        assert_eq!(drain(&mut q), entries);
    }

    #[test]
    fn test_slot_size() {
        // i64: 8 + 8 = 16.
        assert_eq!(slot_size::<i64>(), 16);

        // TestEntry: 8 + 16 = 24, next power of 2 = 32.
        assert_eq!(slot_size::<TestEntry>(), 32);

        // u8: 8 + 1 = 9, next power of 2 = 16.
        assert_eq!(slot_size::<u8>(), 16);
    }

    #[test]
    fn queue_size() {
        for cap in [0, 1, 7, 8, 100, 1000] {
            let size = mpsc_mem_size::<i64>(cap);

            assert_eq!(size % PAGE_SIZE, 0, "cap={cap}");
        }

        for want in [1, 8, 64, 256] {
            let size = mpsc_mem_size::<i64>(want);
            let mut data = vec![0u8; size];

            let q = Mpsc::<i64>::new(&mut data).unwrap();

            assert!(q.cap() >= want, "want cap={want}, actual cap={}", q.cap());
        }

        for (input, expected) in [(200, 256), (500, 512), (1000, 1024), (2000, 2048)] {
            let size = mpsc_mem_size::<i64>(input);
            let mut data = vec![0u8; size];

            let q = Mpsc::<i64>::new(&mut data).unwrap();

            assert_eq!(q.cap(), expected, "input={input}");
        }
    }
}
