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

use crate::{
    consts::PAGE_SIZE,
    v5::mpsc::{LayoutError, Mpsc},
};

/// Bytes the release queue region occupies.
pub const RQ_SIZE: usize = 128 * PAGE_SIZE;

/// The release queue is just an MPSC queue of buffer offsets.
///
/// A peer that is done with a buffer pushes its offset here; the owner pops the offsets and returns
/// the buffers to its allocator.
pub type ReleaseQueue = Mpsc<i64>;

/// Initializes a release queue at the front of `memory`, zeroing the region it claims.
///
/// Only the peer that owns the shared memory calls this; the other side uses [`map_rq`]. Anything
/// past [`RQ_SIZE`] is left alone, so this can be handed the whole shared region.
pub fn new_rq(memory: &mut [u8]) -> Result<ReleaseQueue, LayoutError> {
    if memory.len() < RQ_SIZE {
        return Err(LayoutError::TooSmall(memory.len()));
    }

    Mpsc::<i64>::new(&mut memory[..RQ_SIZE])
}

/// Maps an existing release queue at the front of `data` without modifying it.
///
/// The peer must have initialized the region with [`new_rq`] first.
pub fn map_rq(data: &mut [u8]) -> Result<ReleaseQueue, LayoutError> {
    if data.len() < RQ_SIZE {
        return Err(LayoutError::TooSmall(data.len()));
    }

    Mpsc::map(data.as_mut_ptr(), RQ_SIZE)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use super::{RQ_SIZE, map_rq, new_rq};
    use crate::{
        consts::MPSC_HEADER_SIZE,
        v5::mpsc::{LayoutError, Mpsc, mpsc_cap, slot_size},
    };

    /// Offsets the queue holds when full, derived from [`RQ_SIZE`] the same way on both peers.
    pub const RQ_CAPACITY: usize = mpsc_cap::<i64>(RQ_SIZE);
    /// Bytes of [`RQ_SIZE`] the header and slots actually cover.
    pub const RQ_USED_SIZE: usize = MPSC_HEADER_SIZE + RQ_CAPACITY * slot_size::<i64>();

    fn drain_rq_count(rq: &mut Mpsc<i64>) -> usize {
        let mut n = 0;

        while rq.pop().is_some() {
            n += 1;
        }

        n
    }

    #[test]
    fn basic() {
        let mut data = vec![0u8; RQ_SIZE];
        let mut rq = new_rq(&mut data).unwrap();

        assert_eq!(0, rq.len());

        assert!(rq.try_push(12_345));
        assert_eq!(1, rq.len());

        assert_eq!(Some(12_345), rq.pop());
        assert_eq!(0, rq.len());
    }

    #[test]
    fn pop_empty() {
        let mut data = vec![0u8; RQ_SIZE];
        let mut rq = new_rq(&mut data).unwrap();

        assert_eq!(None, rq.pop());
        assert_eq!(rq.len(), 0);
    }

    #[test]
    fn multiple_push_pop() {
        let mut data = vec![0u8; RQ_SIZE];
        let mut rq = new_rq(&mut data).unwrap();

        let offsets = [100i64, 200, 300, 400, 500];

        for offset in offsets {
            assert!(rq.try_push(offset));
        }

        assert_eq!(offsets.len(), rq.len());

        let mut got = Vec::new();

        while let Some(offset) = rq.pop() {
            got.push(offset);
        }

        assert_eq!(offsets, got.as_slice());
        assert_eq!(0, rq.len());
    }

    #[test]
    fn map_existing() {
        let mut data = vec![0u8; RQ_SIZE];

        let mut rq1 = new_rq(&mut data).unwrap();

        // Push from the first queue.
        assert!(rq1.try_push(999));

        // Map the same memory as a second queue, standing in for the peer mapping the same
        // shared-memory region. The second Mpsc must see the first queue's state.
        let rq2 = map_rq(&mut data).unwrap();

        // Push from the second queue.
        assert!(rq2.try_push(888));

        // The first queue must see both entries.
        assert_eq!(2, rq1.len());

        assert_eq!(Some(999), rq1.pop());
        assert_eq!(Some(888), rq1.pop());
        assert_eq!(None, rq1.pop());
    }

    #[test]
    fn concurrent_push() {
        let mut data = vec![0u8; RQ_SIZE];
        let rq = new_rq(&mut data).unwrap();

        let mut rq = Arc::new(rq);

        let num_producers = 8usize;
        let pushes_per_producer = 1_000usize;
        let total = num_producers * pushes_per_producer;

        let mut handles = Vec::with_capacity(num_producers);

        for producer in 0..num_producers {
            let rq = Arc::clone(&rq);

            handles.push(thread::spawn(move || {
                let base = producer * pushes_per_producer;

                for i in 0..pushes_per_producer {
                    let value = (base + i) as i64;

                    while !rq.try_push(value) {
                        thread::yield_now();
                    }
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(total, rq.len());

        // Every producer has joined, so this is the only handle left: taking the consumer
        // role is just taking `&mut`.
        let rq = Arc::get_mut(&mut rq).unwrap();

        let mut seen = std::collections::HashSet::with_capacity(total);

        while let Some(value) = rq.pop() {
            seen.insert(value);
        }

        assert_eq!(total, seen.len());
    }

    #[test]
    fn concurrent_push_pop() {
        let mut data = vec![0u8; RQ_SIZE];
        let rq = new_rq(&mut data).unwrap();

        let rq = Arc::new(rq);

        let num_producers = 4usize;
        let pushes_per_producer = 10_000usize;
        let total = num_producers * pushes_per_producer;

        let popped = Arc::new(AtomicUsize::new(0));

        // The consumer maps its own handle over the same bytes rather than sharing the
        // producers', which is how the two peers of a real queue hold their roles.
        let mut consumer_rq = map_rq(&mut data).unwrap();
        let consumer_popped = Arc::clone(&popped);

        let consumer = thread::spawn(move || {
            loop {
                let count = consumer_popped.load(Ordering::Acquire);

                if count >= total {
                    break;
                }

                if consumer_rq.pop().is_some() {
                    consumer_popped.fetch_add(1, Ordering::Release);
                } else {
                    thread::yield_now();
                }
            }

            // Drain anything published immediately before the final counter update.
            while consumer_rq.pop().is_some() {
                consumer_popped.fetch_add(1, Ordering::Release);
            }
        });

        let mut producers = Vec::with_capacity(num_producers);

        for producer in 0..num_producers {
            let rq = Arc::clone(&rq);

            producers.push(thread::spawn(move || {
                let base = producer * pushes_per_producer;

                for i in 0..pushes_per_producer {
                    let value = (base + i) as i64;

                    while !rq.try_push(value) {
                        thread::yield_now();
                    }
                }
            }));
        }

        for producer in producers {
            producer.join().unwrap();
        }

        consumer.join().unwrap();

        assert_eq!(total, popped.load(Ordering::Acquire));
        assert_eq!(rq.len(), 0);
    }

    #[test]
    fn wraparound() {
        let mut data = vec![0u8; RQ_SIZE];
        let mut rq = new_rq(&mut data).unwrap();

        let batch = rq.cap() / 2;

        for round in 0..5usize {
            for i in 0..batch {
                let value = (round * batch + i) as i64;

                while !rq.try_push(value) {
                    thread::yield_now();
                }
            }

            let mut got = Vec::with_capacity(batch);

            while let Some(value) = rq.pop() {
                got.push(value);
            }

            assert_eq!(batch, got.len());

            for (i, &value) in got.iter().enumerate() {
                assert_eq!((round * batch + i) as i64, value);
            }
        }

        assert_eq!(rq.len(), 0);
    }

    #[test]
    fn capacity_is_derived_from_rq_size() {
        let mut data = vec![0u8; RQ_SIZE];
        let rq = new_rq(&mut data).unwrap();

        assert_eq!(RQ_CAPACITY, rq.cap());
    }

    #[test]
    fn new_queue_is_empty() {
        let mut data = vec![0xCDu8; RQ_SIZE];
        let rq = new_rq(&mut data).unwrap();

        assert_eq!(0, rq.len());
    }

    #[test]
    fn try_push_refuses_when_full() {
        let mut data = vec![0u8; RQ_SIZE];
        let mut rq = new_rq(&mut data).unwrap();

        let capacity = rq.cap();

        for i in 0..capacity {
            assert!(rq.try_push(i as i64), "push {} unexpectedly failed", i);
        }

        assert_eq!(capacity, rq.len());

        // A full queue must make try_push return false rather than wait or overwrite a live
        // entry.
        assert!(!rq.try_push(-1));
        assert_eq!(capacity, rq.len());

        assert_eq!(Some(0), rq.pop());

        // Freeing one slot permits another push.
        assert!(rq.try_push(-1));
        assert_eq!(capacity, rq.len());
    }

    #[test]
    fn push_pop_preserves_fifo_order_across_wraparound() {
        let mut data = vec![0u8; RQ_SIZE];
        let mut rq = new_rq(&mut data).unwrap();

        let capacity = rq.cap();

        // Enough cycles to carry the turn counters through several slot generations.
        for round in 0..8usize {
            let count = capacity.min(1024);

            for i in 0..count {
                let value = (round * count + i) as i64;

                while !rq.try_push(value) {
                    thread::yield_now();
                }
            }

            for i in 0..count {
                let expected = (round * count + i) as i64;
                assert_eq!(Some(expected), rq.pop());
            }

            assert_eq!(rq.len(), 0);
        }
    }

    #[test]
    fn drain_helper_matches_go_drain_rq_count() {
        let mut data = vec![0u8; RQ_SIZE];
        let mut rq = new_rq(&mut data).unwrap();

        for i in 0..100 {
            assert!(rq.try_push(i));
        }

        assert_eq!(100, drain_rq_count(&mut rq));
        assert_eq!(0, rq.len());
        assert_eq!(0, drain_rq_count(&mut rq));
    }

    #[test]
    fn map_rq_maps_only_the_release_queue_region() {
        let mut data = vec![0u8; RQ_SIZE + 64 * 4096];
        let mut rq1 = new_rq(&mut data).unwrap();
        let rq2 = map_rq(&mut data).unwrap();

        assert_eq!(RQ_CAPACITY, rq1.cap());
        assert_eq!(rq1.cap(), rq2.cap());

        assert!(rq2.try_push(777));
        assert_eq!(Some(777), rq1.pop());
        assert!(data[RQ_USED_SIZE..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn map_rq_rejects_a_region_smaller_than_the_queue() {
        let mut data = vec![0u8; RQ_SIZE - 4096];
        let len = data.len();

        assert_eq!(LayoutError::TooSmall(len), map_rq(&mut data).unwrap_err());
    }
}
