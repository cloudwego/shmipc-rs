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

use crate::v5::region::{
    ALLOCATOR_HEADER_SIZE, Allocation, Allocator, Error, MAGIC, clear_magic, read_header,
    write_header,
};

/// 512 KiB
const DEFAULT_MAX_BLOCK_SIZE: usize = 512 * 1024;
/// 8 KiB
const DEFAULT_MIN_BLOCK_SIZE: usize = 8 * 1024;

/// A buddy allocator over one region's arena.
///
/// The arena is carved into power-of-two blocks between `min_block` and `max_block`, and
/// every block carries an eight-byte `[MAGIC:4][order:4]` header at its start. That header
/// lets a block be released by offset and makes a double free detectable.
///
/// Coalescing is lazy: a freed block goes straight back onto its own order's list, and
/// buddies are merged only when an allocation cannot otherwise be served.
#[derive(Debug)]
pub struct BuddyAllocator {
    /// Length every arena passed to this allocator must have.
    arena_size: usize,
    /// Smallest block the arena is carved into, header included.
    min_block: usize,
    /// Largest block, and the unit the arena is initially split into.
    max_block: usize,
    /// `log2(min_block)`, so order `n` names a block of `min_block << n` bytes.
    min_shift: u32,
    /// Order of `max_block`, and the highest index into `free`.
    max_order: usize,
    /// Free block offsets per order, `free[n]` holding blocks of `min_block << n`.
    free: Vec<Vec<usize>>,
    /// Set when a block below `max_order` was freed.
    needs_coalesce: bool,
}

impl Allocator for BuddyAllocator {
    /// Carve `size` bytes out of `arena` and stamp the resulting block's header.
    fn alloc(&mut self, arena: &mut [u8], size: usize) -> Option<Allocation> {
        if arena.len() != self.arena_size
            || size == 0
            || size > self.max_block - ALLOCATOR_HEADER_SIZE
        {
            return None;
        }
        let order = self.order_for(size + ALLOCATOR_HEADER_SIZE);
        let mut found = (order..=self.max_order).find(|&o| !self.free[o].is_empty());

        if found.is_none() && self.needs_coalesce {
            self.coalesce_until(order);
            found = (order..=self.max_order).find(|&o| !self.free[o].is_empty());
            if found.is_none() {
                self.needs_coalesce = false;
            }
        }

        let mut found = found?;
        let offset = self.free[found].pop()?;

        while found > order {
            found -= 1;
            self.free[found].push(offset + (self.min_block << found));
        }
        let block_size = self.min_block << order;
        write_header(arena, offset, order);
        Some(Allocation {
            block_offset: offset,
            data_offset: offset + ALLOCATOR_HEADER_SIZE,
            len: size,
            cap: block_size - ALLOCATOR_HEADER_SIZE,
        })
    }

    /// Largest request the arena can ever serve: the maximum block less the header every
    /// block carries.
    fn max_allocation(&self) -> usize {
        self.max_block - ALLOCATOR_HEADER_SIZE
    }

    /// Return a block described by the [`Allocation`] the allocator handed out.
    fn free(&mut self, arena: &mut [u8], allocation: Allocation) -> Result<(), Error> {
        if arena.len() != self.arena_size {
            return Err(Error::InvalidArenaSize);
        }

        if allocation.block_offset >= self.arena_size
            || allocation.data_offset != allocation.block_offset + ALLOCATOR_HEADER_SIZE
            || allocation.data_offset >= self.arena_size
        {
            return Err(Error::InvalidOffset);
        }

        if allocation.len > allocation.cap {
            return Err(Error::CorruptMetadata);
        }

        let claimed = self
            .order_for_capacity(allocation.cap)
            .ok_or(Error::CorruptMetadata)?;
        let order = self.live_order(arena, allocation.block_offset)?;
        if claimed != order {
            return Err(Error::CorruptMetadata);
        }
        self.release(arena, allocation.block_offset, order);
        Ok(())
    }

    /// Return a block named only by the offset of its data, the way a peer reports a
    /// release across shared memory.
    fn free_at(&mut self, arena: &mut [u8], data_offset: usize) -> Result<(), Error> {
        if arena.len() != self.arena_size {
            return Err(Error::InvalidArenaSize);
        }
        if !self.is_valid_offset(data_offset) {
            return Err(Error::InvalidOffset);
        }
        let block_offset = data_offset - ALLOCATOR_HEADER_SIZE;
        let order = self.live_order(arena, block_offset)?;
        self.release(arena, block_offset, order);
        Ok(())
    }

    /// Whether `data_offset` could name the data of a block from this arena: it has to sit
    /// exactly one header past a minimum-block boundary inside the arena.
    fn is_valid_offset(&self, data_offset: usize) -> bool {
        data_offset >= ALLOCATOR_HEADER_SIZE
            && data_offset - ALLOCATOR_HEADER_SIZE < self.arena_size
            && (data_offset - ALLOCATOR_HEADER_SIZE) & (self.min_block - 1) == 0
    }

    /// Payload bytes the free lists hold right now, counting a header out of every free
    /// block.
    fn available(&self) -> usize {
        self.free
            .iter()
            .enumerate()
            .map(|(order, list)| list.len() * ((self.min_block << order) - ALLOCATOR_HEADER_SIZE))
            .sum()
    }

    /// Hand the whole arena back to the allocator.
    ///
    /// Only the free lists are rebuilt; the arena is not written. A region is reset after a
    /// punch, and touching a header in every block would fault those pages straight back in,
    /// for magics the frees that emptied the region have already cleared. Every allocation
    /// handed out before the reset is invalid afterwards and must not be freed, because its
    /// header still reads as live.
    fn reset(&mut self, arena: &mut [u8]) -> Result<(), Error> {
        if arena.len() != self.arena_size {
            return Err(Error::InvalidArenaSize);
        }
        for list in &mut self.free {
            list.clear();
        }
        for offset in (0..self.arena_size).step_by(self.max_block) {
            self.free[self.max_order].push(offset);
        }
        self.needs_coalesce = false;
        Ok(())
    }
}

impl BuddyAllocator {
    /// Builds an allocator over `arena_size` bytes with the default 8 KiB to 512 KiB block
    /// range.
    #[allow(unused)]
    pub fn new(arena_size: usize) -> Result<Self, Error> {
        Self::new_with_block_size(arena_size, DEFAULT_MIN_BLOCK_SIZE, DEFAULT_MAX_BLOCK_SIZE)
    }

    /// Builds an allocator over `arena_size` bytes carved into blocks between `min_block`
    /// and `max_block`.
    ///
    /// Both block sizes must be powers of two, `min_block` has to leave room for the block
    /// header, and the arena has to be a positive whole number of maximum blocks so that
    /// the initial free list covers it exactly.
    pub fn new_with_block_size(
        arena_size: usize,
        min_block: usize,
        max_block: usize,
    ) -> Result<Self, Error> {
        if !min_block.is_power_of_two() || !max_block.is_power_of_two() {
            return Err(Error::BlockSizeNotPowerOfTwo);
        }
        if min_block > max_block {
            return Err(Error::InvalidBlockSizeOrder);
        }
        if min_block <= ALLOCATOR_HEADER_SIZE {
            return Err(Error::BlockTooSmallForHeader);
        }
        if arena_size < max_block || !arena_size.is_multiple_of(max_block) {
            return Err(Error::InvalidArenaSize);
        }

        let min_shift = min_block.trailing_zeros();
        let max_order = (max_block.trailing_zeros() - min_shift) as usize;
        let mut free = (0..=max_order).map(|_| Vec::new()).collect::<Vec<_>>();

        // Put every block into the largest free list
        for offset in (0..arena_size).step_by(max_block) {
            free[max_order].push(offset);
        }
        Ok(Self {
            arena_size,
            min_block,
            max_block,
            min_shift,
            max_order,
            free,
            needs_coalesce: false,
        })
    }

    /// Order of the smallest block that holds `bytes`. Callers pass the size with the
    /// block header already added in.
    fn order_for(&self, bytes: usize) -> usize {
        if bytes <= self.min_block {
            return 0;
        }
        (usize::BITS - (bytes - 1).leading_zeros() - self.min_shift) as usize
    }

    /// Recover the order implied by an [`Allocation`]'s capacity, or `None` when no
    /// block this arena hands out has that capacity.
    fn order_for_capacity(&self, cap: usize) -> Option<usize> {
        let block_size = cap.checked_add(ALLOCATOR_HEADER_SIZE)?;
        if !block_size.is_power_of_two()
            || block_size < self.min_block
            || block_size > self.max_block
        {
            return None;
        }
        Some((block_size.trailing_zeros() - self.min_shift) as usize)
    }

    /// Read the header of a block believed to be live and hand back the order it
    /// claims, having checked that the claim is one this arena could have produced.
    fn live_order(&self, arena: &[u8], block_offset: usize) -> Result<usize, Error> {
        let (magic, order) = read_header(arena, block_offset).ok_or(Error::InvalidOffset)?;
        if magic != MAGIC {
            return Err(Error::NotAllocated);
        }
        let order = order as usize;
        if order > self.max_order {
            return Err(Error::CorruptMetadata);
        }
        // A block of 2^n bytes only ever starts at a multiple of 2^n, so a header
        // saying otherwise is describing a block that was never allocated here.
        if block_offset & ((self.min_block << order) - 1) != 0 {
            return Err(Error::CorruptMetadata);
        }
        Ok(order)
    }

    /// Retire a validated block: clear the magic first, so that even if the caller
    /// keeps its [`Allocation`] the block can no longer be presented as live.
    fn release(&mut self, arena: &mut [u8], block_offset: usize, order: usize) {
        clear_magic(arena, block_offset);
        self.free[order].push(block_offset);
        self.needs_coalesce |= order < self.max_order;
    }

    /// Merges free buddies upwards until order `target`, so a request for that order can
    /// be served when the space for it only exists in smaller pieces.
    fn coalesce_until(&mut self, target: usize) {
        for order in 0..target {
            let block_size = self.min_block << order;
            let (current, next) = self.free.split_at_mut(order + 1);
            let list = &mut current[order];
            let next_list = &mut next[0];

            if list.len() < 2 {
                continue;
            }

            list.sort_unstable();

            let mut kept = 0;
            let mut i = 0;

            while i < list.len() {
                if i + 1 < list.len() {
                    let left = list[i];
                    let right = list[i + 1];
                    if (left ^ block_size) == right {
                        next_list.push(left);
                        i += 2;
                        continue;
                    }
                }

                // `kept <= i`, so compacting unpaired blocks cannot overwrite an offset
                // that the scan has yet to inspect. Keep this order's allocation for reuse.
                list[kept] = list[i];
                kept += 1;
                i += 1;
            }

            list.truncate(kept);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arena(size: usize) -> Vec<u8> {
        vec![0u8; size]
    }

    fn allocation_order(memory: &[u8], allocation: &Allocation) -> usize {
        read_header(memory, allocation.block_offset)
            .expect("allocation should have a valid header")
            .1 as usize
    }

    #[test]
    fn a_freed_block_is_split_reused_and_merged_again() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();
        let before = allocator.available();
        let a = allocator.alloc(&mut memory, 20).unwrap();
        let b = allocator.alloc(&mut memory, 20).unwrap();
        assert_ne!(a.block_offset, b.block_offset);
        allocator.free(&mut memory, a).unwrap();
        allocator.free(&mut memory, b).unwrap();
        assert!(allocator.alloc(&mut memory, 100).is_some());
        assert!(before > 0);
    }

    #[test]
    fn configuration_and_offsets_are_validated() {
        assert_eq!(
            Some(Error::BlockSizeNotPowerOfTwo),
            BuddyAllocator::new_with_block_size(1024, 63, 256).err()
        );
        assert_eq!(
            Some(Error::InvalidBlockSizeOrder),
            BuddyAllocator::new_with_block_size(1024, 256, 64).err()
        );
        // A power of two, but not one large enough to carry the block header.
        assert_eq!(
            Some(Error::BlockTooSmallForHeader),
            BuddyAllocator::new_with_block_size(1024, 8, 256).err()
        );
        assert_eq!(
            Some(Error::InvalidArenaSize),
            BuddyAllocator::new_with_block_size(1000, 64, 256).err()
        );
        let allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();
        assert!(allocator.is_valid_offset(8));
        assert!(!allocator.is_valid_offset(9));
        assert!(!allocator.is_valid_offset(0));
        assert!(!allocator.is_valid_offset(1024 + 8));
    }

    #[test]
    fn an_arena_of_the_wrong_length_is_refused() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();
        let allocation = allocator.alloc(&mut memory, 20).unwrap();

        let mut wrong = arena(2048);
        assert!(allocator.alloc(&mut wrong, 20).is_none());
        assert_eq!(
            Err(Error::InvalidArenaSize),
            allocator.free(&mut wrong, allocation)
        );
        assert_eq!(
            Err(Error::InvalidArenaSize),
            allocator.free_at(&mut wrong, allocation.data_offset)
        );
        allocator.free(&mut memory, allocation).unwrap();
    }

    #[test]
    fn zero_and_oversized_requests_are_refused() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();
        assert!(allocator.alloc(&mut memory, 0).is_none());
        assert!(allocator.alloc(&mut memory, 256).is_none());
        assert!(
            allocator
                .alloc(&mut memory, 256 - ALLOCATOR_HEADER_SIZE)
                .is_some()
        );
    }

    #[test]
    fn every_requested_size_gets_a_block_that_holds_it() {
        let mut memory = arena(4096);
        let mut allocator = BuddyAllocator::new_with_block_size(4096, 64, 1024).unwrap();
        for size in [1, 10, 56, 57, 120, 500, 1016] {
            let allocation = allocator.alloc(&mut memory, size).unwrap();
            assert!(allocation.cap >= size);
            assert_eq!(allocation.len, size);
            assert_eq!(
                allocation.data_offset,
                allocation.block_offset + ALLOCATOR_HEADER_SIZE
            );
            allocator.free(&mut memory, allocation).unwrap();
        }
    }

    #[test]
    fn exhausting_the_arena_then_freeing_it_restores_the_largest_block() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();
        let mut blocks = Vec::new();
        while let Some(allocation) = allocator.alloc(&mut memory, 10) {
            blocks.push(allocation);
        }
        assert_eq!(1024 / 64, blocks.len());
        assert!(allocator.alloc(&mut memory, 1).is_none());
        for allocation in blocks {
            allocator.free(&mut memory, allocation).unwrap();
        }
        let large = allocator
            .alloc(&mut memory, 256 - ALLOCATOR_HEADER_SIZE)
            .unwrap();
        assert_eq!(allocator.max_allocation(), large.cap);
    }

    #[test]
    fn a_block_freed_twice_is_refused_the_second_time() {
        let mut memory = arena(256);
        let mut allocator = BuddyAllocator::new_with_block_size(256, 64, 256).unwrap();
        let arena_capacity = allocator.available();

        let small = (0..4)
            .map(|_| allocator.alloc(&mut memory, 10).unwrap())
            .collect::<Vec<_>>();
        let first = small[0];
        for allocation in &small {
            allocator.free(&mut memory, *allocation).unwrap();
        }

        let large = allocator.alloc(&mut memory, 200).unwrap();
        assert_eq!(first.block_offset, large.block_offset);
        // While the large block is live, the stale allocation's order contradicts the
        // header it would be freeing.
        assert_eq!(
            Err(Error::CorruptMetadata),
            allocator.free(&mut memory, first)
        );
        allocator.free(&mut memory, large).unwrap();

        assert_eq!(Err(Error::NotAllocated), allocator.free(&mut memory, first));
        assert_eq!(
            Err(Error::NotAllocated),
            allocator.free_at(&mut memory, first.data_offset)
        );
        assert_eq!(arena_capacity, allocator.available());
        assert!(allocator.available() <= 256);
    }

    #[test]
    fn a_corrupted_magic_is_refused() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();
        let allocation = allocator.alloc(&mut memory, 20).unwrap();
        memory[allocation.block_offset] ^= 0xff;
        assert_eq!(
            Err(Error::NotAllocated),
            allocator.free(&mut memory, allocation)
        );
        assert_eq!(
            Err(Error::NotAllocated),
            allocator.free_at(&mut memory, allocation.data_offset)
        );
    }

    #[test]
    fn a_corrupted_order_is_refused() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();
        let allocation = allocator.alloc(&mut memory, 20).unwrap();
        let order_at = allocation.block_offset + 4;
        memory[order_at..order_at + 4].copy_from_slice(&9u32.to_ne_bytes());
        assert_eq!(
            Err(Error::CorruptMetadata),
            allocator.free(&mut memory, allocation)
        );
        assert_eq!(
            Err(Error::CorruptMetadata),
            allocator.free_at(&mut memory, allocation.data_offset)
        );

        memory[order_at..order_at + 4].copy_from_slice(&2u32.to_ne_bytes());
        assert_eq!(
            Err(Error::CorruptMetadata),
            allocator.free(&mut memory, allocation)
        );
    }

    #[test]
    fn free_at_takes_the_order_from_the_header() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();
        let before = allocator.available();
        let small = allocator.alloc(&mut memory, 10).unwrap();
        let large = allocator.alloc(&mut memory, 200).unwrap();
        assert_ne!(small.cap, large.cap);
        allocator.free_at(&mut memory, small.data_offset).unwrap();
        allocator.free_at(&mut memory, large.data_offset).unwrap();
        allocator.coalesce_until(allocator.max_order);
        assert_eq!(before, allocator.available());
    }

    #[test]
    fn a_reset_arena_hands_out_the_whole_range_again() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();
        let initial = allocator.available();
        allocator.alloc(&mut memory, 20).unwrap();
        allocator.reset(&mut memory).unwrap();
        assert_eq!(initial, allocator.available());

        // Every root block is on offer again, and nothing beyond them.
        for _ in 0..4 {
            allocator
                .alloc(&mut memory, 256 - ALLOCATOR_HEADER_SIZE)
                .unwrap();
        }
        assert!(allocator.alloc(&mut memory, 1).is_none());
    }

    #[test]
    fn default_configuration_is_valid() {
        let allocator = BuddyAllocator::new(512 * 1024).unwrap();

        assert_eq!(allocator.min_block, 8 * 1024);
        assert_eq!(allocator.max_block, 512 * 1024);
        assert_eq!(allocator.max_order, 6);
        assert_eq!(
            allocator.max_allocation(),
            512 * 1024 - ALLOCATOR_HEADER_SIZE
        );
    }

    #[test]
    fn allocation_boundaries_account_for_header() {
        let mut memory = arena(4096);
        let mut allocator = BuddyAllocator::new_with_block_size(4096, 64, 4096).unwrap();

        // 64-byte block has 56 bytes available after the 8-byte header.
        let a = allocator.alloc(&mut memory, 56).unwrap();
        assert_eq!(allocation_order(&memory, &a), 0);
        assert_eq!(a.cap, 56);
        allocator.free(&mut memory, a).unwrap();

        // One byte more requires the next buddy order.
        let b = allocator.alloc(&mut memory, 57).unwrap();
        assert_eq!(allocation_order(&memory, &b), 1);
        assert_eq!(b.cap, 120);
        allocator.free(&mut memory, b).unwrap();

        // 128-byte block has 120 bytes available.
        let c = allocator.alloc(&mut memory, 120).unwrap();
        assert_eq!(allocation_order(&memory, &c), 1);
        assert_eq!(c.cap, 120);
        allocator.free(&mut memory, c).unwrap();

        // One byte more requires 256 bytes.
        let d = allocator.alloc(&mut memory, 121).unwrap();
        assert_eq!(allocation_order(&memory, &d), 2);
        assert_eq!(d.cap, 248);
        allocator.free(&mut memory, d).unwrap();

        // 256-byte block has 248 bytes available.
        let e = allocator.alloc(&mut memory, 248).unwrap();
        assert_eq!(allocation_order(&memory, &e), 2);
        assert_eq!(e.cap, 248);
        allocator.free(&mut memory, e).unwrap();

        // One byte more requires 512 bytes.
        let f = allocator.alloc(&mut memory, 249).unwrap();
        assert_eq!(allocation_order(&memory, &f), 3);
        assert_eq!(f.cap, 504);
        allocator.free(&mut memory, f).unwrap();
    }

    #[test]
    fn allocation_writes_and_free_clears_header() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();

        let allocation = allocator.alloc(&mut memory, 20).unwrap();

        let magic = u32::from_ne_bytes(
            memory[allocation.block_offset..allocation.block_offset + 4]
                .try_into()
                .unwrap(),
        );
        let order = u32::from_ne_bytes(
            memory[allocation.block_offset + 4..allocation.block_offset + 8]
                .try_into()
                .unwrap(),
        );

        assert_eq!(magic, MAGIC);
        assert_eq!(order as usize, allocation_order(&memory, &allocation));

        allocator.free(&mut memory, allocation).unwrap();

        let cleared_magic = u32::from_ne_bytes(
            memory[allocation.block_offset..allocation.block_offset + 4]
                .try_into()
                .unwrap(),
        );

        assert_eq!(cleared_magic, 0);
    }

    #[test]
    fn coalesce_until_two_buddies() {
        let mut allocator = BuddyAllocator::new_with_block_size(1024 * 1024, 8192, 65536).unwrap();

        allocator.free.iter_mut().for_each(Vec::clear);
        allocator.free[0].extend([0, 8192]);
        allocator.coalesce_until(1);

        assert_eq!(allocator.free[0], Vec::<usize>::new());
        assert_eq!(allocator.free[1], vec![0]);
    }

    #[test]
    fn coalesce_until_four_nodes() {
        let mut allocator = BuddyAllocator::new_with_block_size(1024 * 1024, 8192, 65536).unwrap();

        allocator.free.iter_mut().for_each(Vec::clear);
        allocator.free[0].extend([0, 8192, 16384, 24576]);

        allocator.coalesce_until(2);

        assert!(allocator.free[0].is_empty());
        assert!(allocator.free[1].is_empty());
        assert_eq!(allocator.free[2], vec![0]);
    }

    #[test]
    fn coalesce_until_eight_nodes() {
        let mut allocator = BuddyAllocator::new_with_block_size(1024 * 1024, 8192, 65536).unwrap();

        allocator.free.iter_mut().for_each(Vec::clear);

        allocator.free[0].extend([0, 8192, 16384, 24576, 32768, 40960, 49152, 57344]);

        allocator.coalesce_until(3);

        assert!(allocator.free[0].is_empty());
        assert!(allocator.free[1].is_empty());
        assert!(allocator.free[2].is_empty());
        assert_eq!(allocator.free[3], vec![0]);
    }

    #[test]
    fn coalesce_until_partial_buddies() {
        let mut allocator = BuddyAllocator::new_with_block_size(1024 * 1024, 8192, 65536).unwrap();

        allocator.free.iter_mut().for_each(Vec::clear);
        allocator.free[0].extend([0, 8192, 32768]);

        allocator.coalesce_until(2);

        assert_eq!(allocator.free[0], vec![32768]);
        assert_eq!(allocator.free[1], vec![0]);
        assert!(allocator.free[2].is_empty());
    }

    #[test]
    fn coalescing_preserves_live_blocks_and_all_unpaired_offsets() {
        // Every subset includes the cases where survivors precede, separate, or follow
        // merged buddies, including odd-length free lists and entirely free arenas.
        for released in 0u16..256 {
            let mut memory = arena(512);
            let mut allocator = BuddyAllocator::new_with_block_size(512, 64, 256).unwrap();
            let mut small = Vec::new();
            while let Some(allocation) = allocator.alloc(&mut memory, 56) {
                memory[allocation.data_offset..allocation.data_offset + allocation.len]
                    .fill((allocation.block_offset / 64) as u8);
                small.push(allocation);
            }
            small.sort_unstable_by_key(|allocation| allocation.block_offset);

            // Release out of offset order so the allocator must sort before pairing.
            for index in [6, 1, 4, 3, 0, 7, 2, 5] {
                if released & (1 << index) != 0 {
                    allocator.free(&mut memory, small[index]).unwrap();
                }
            }
            let mut larger = Vec::new();
            for pair in (0..4).rev() {
                if released & (3 << (pair * 2)) == 3 << (pair * 2) {
                    let allocation = allocator.alloc(&mut memory, 120).unwrap();
                    assert_eq!(allocation.block_offset, pair * 128);
                    larger.push(allocation);
                }
            }
            assert!(allocator.alloc(&mut memory, 120).is_none());

            for (index, allocation) in small.into_iter().enumerate() {
                if released & (1 << index) == 0 {
                    assert!(
                        memory[allocation.data_offset..allocation.data_offset + allocation.len]
                            .iter()
                            .all(|&byte| byte == index as u8)
                    );
                    allocator.free(&mut memory, allocation).unwrap();
                }
            }
            for allocation in larger {
                allocator.free(&mut memory, allocation).unwrap();
            }
            let first = allocator.alloc(&mut memory, 248).unwrap();
            let second = allocator.alloc(&mut memory, 248).unwrap();
            assert_ne!(first.block_offset, second.block_offset);
            assert!(allocator.alloc(&mut memory, 1).is_none());
            allocator.free(&mut memory, first).unwrap();
            allocator.free(&mut memory, second).unwrap();
            assert_eq!(allocator.available(), 2 * 248);
        }
    }

    #[test]
    fn coalesce_until_no_buddies() {
        let mut allocator = BuddyAllocator::new_with_block_size(1024 * 1024, 8192, 65536).unwrap();

        allocator.free.iter_mut().for_each(Vec::clear);
        allocator.free[0].extend([0, 16384, 32768, 49152]);

        allocator.coalesce_until(1);

        assert_eq!(allocator.free[0], vec![0, 16384, 32768, 49152]);
        assert!(allocator.free[1].is_empty());
    }

    #[test]
    fn coalesce_until_single_node() {
        let mut allocator = BuddyAllocator::new_with_block_size(1024 * 1024, 8192, 65536).unwrap();

        allocator.free.iter_mut().for_each(Vec::clear);
        allocator.free[0].push(16384);

        allocator.coalesce_until(1);

        assert_eq!(allocator.free[0], vec![16384]);
        assert!(allocator.free[1].is_empty());
    }

    #[test]
    fn coalesce_until_does_not_merge_beyond_target() {
        let mut allocator = BuddyAllocator::new_with_block_size(1024 * 1024, 8192, 65536).unwrap();

        allocator.free.iter_mut().for_each(Vec::clear);
        allocator.free[0].extend([0, 8192, 16384, 24576]);

        allocator.coalesce_until(1);

        assert!(allocator.free[0].is_empty());
        assert_eq!(allocator.free[1], vec![0, 16384]);
        assert!(allocator.free[2].is_empty());
    }

    #[test]
    fn coalesce_until_preserves_already_available_higher_order_block() {
        let mut allocator = BuddyAllocator::new_with_block_size(1024 * 1024, 8192, 65536).unwrap();

        allocator.free.iter_mut().for_each(Vec::clear);
        allocator.free[2].push(0);

        allocator.coalesce_until(2);

        assert!(allocator.free[0].is_empty());
        assert!(allocator.free[1].is_empty());
        assert_eq!(allocator.free[2], vec![0]);
    }

    #[test]
    fn coalesce_until_ignores_higher_order_blocks_when_target_is_lower() {
        let mut allocator = BuddyAllocator::new_with_block_size(1024 * 1024, 8192, 65536).unwrap();

        allocator.free.iter_mut().for_each(Vec::clear);
        allocator.free[3].push(0);

        allocator.coalesce_until(1);

        assert!(allocator.free[0].is_empty());
        assert!(allocator.free[1].is_empty());
        assert!(allocator.free[2].is_empty());
        assert_eq!(allocator.free[3], vec![0]);
    }

    #[test]
    fn available_never_exceeds_arena_after_random_churn() {
        let mut memory = arena(4 * 1024 * 1024);
        let mut allocator =
            BuddyAllocator::new_with_block_size(4 * 1024 * 1024, 8192, 65536).unwrap();

        let initial = allocator.available();
        let mut live = Vec::new();

        let sizes = [100, 512, 1024, 4096, 8192, 16384, 32768, 65528];

        let mut state = 42u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            (state >> 33) as usize
        };

        for _ in 0..100_000 {
            if live.is_empty() || next() % 3 != 0 {
                let size = sizes[next() % sizes.len()];

                if let Some(allocation) = allocator.alloc(&mut memory, size) {
                    live.push(allocation);
                }
            } else {
                let index = next() % live.len();
                let allocation = live.swap_remove(index);
                allocator.free(&mut memory, allocation).unwrap();
            }

            assert!(
                allocator.available() <= 4 * 1024 * 1024,
                "available memory exceeded arena size"
            );
        }

        for allocation in live {
            allocator.free(&mut memory, allocation).unwrap();
        }

        allocator.coalesce_until(allocator.max_order);

        assert_eq!(initial, allocator.available());
    }

    #[test]
    fn free_rejects_malformed_allocation() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();

        let allocation = allocator.alloc(&mut memory, 20).unwrap();

        // A data offset that does not sit one header past its own block.
        let mut malformed = allocation;
        malformed.data_offset += 1;
        assert_eq!(
            Err(Error::InvalidOffset),
            allocator.free(&mut memory, malformed)
        );

        // A capacity no block of this arena ever has.
        let mut malformed = allocation;
        malformed.cap += 1;
        assert_eq!(
            Err(Error::CorruptMetadata),
            allocator.free(&mut memory, malformed)
        );

        // Impossible order in the block header.
        let malformed = allocation;
        let live_order = allocation_order(&memory, &allocation);
        write_header(&mut memory, malformed.block_offset, allocator.max_order + 1);
        assert_eq!(
            Err(Error::CorruptMetadata),
            allocator.free(&mut memory, malformed)
        );
        write_header(&mut memory, malformed.block_offset, live_order);

        // Block offset outside the arena.
        let mut malformed = allocation;
        malformed.block_offset = memory.len();
        assert_eq!(
            Err(Error::InvalidOffset),
            allocator.free(&mut memory, malformed)
        );

        // Keep the original allocation valid.
        allocator.free(&mut memory, allocation).unwrap();
    }

    #[test]
    fn free_at_checks_arena_boundaries() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();

        assert_eq!(
            Err(Error::InvalidOffset),
            allocator.free_at(&mut memory, 1024)
        );

        assert_eq!(
            Err(Error::InvalidOffset),
            allocator.free_at(&mut memory, 1025)
        );

        assert_eq!(
            Err(Error::InvalidOffset),
            allocator.free_at(&mut memory, usize::MAX)
        );

        // Header size itself is a valid data offset, because it refers to
        // block offset zero. It is rejected here because that block is not live.
        assert_eq!(
            Err(Error::NotAllocated),
            allocator.free_at(&mut memory, ALLOCATOR_HEADER_SIZE)
        );
    }

    #[test]
    fn free_at_accepts_last_valid_block_offset() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();

        let mut allocations = Vec::new();

        while let Some(allocation) = allocator.alloc(&mut memory, 10) {
            allocations.push(allocation);
        }

        let last = *allocations.last().unwrap();

        assert!(allocator.is_valid_offset(last.data_offset));

        allocator.free_at(&mut memory, last.data_offset).unwrap();

        // The remaining allocations must still be valid.
        for allocation in allocations
            .into_iter()
            .filter(|allocation| allocation.block_offset != last.block_offset)
        {
            allocator.free(&mut memory, allocation).unwrap();
        }
    }

    #[test]
    fn failed_allocation_clears_needs_coalesce() {
        let mut allocator = BuddyAllocator::new_with_block_size(1024 * 1024, 8192, 65536).unwrap();

        allocator.free.iter_mut().for_each(Vec::clear);

        // Put unrelated blocks into the minimum-order free list. There are
        // no buddies, so a request requiring order 1 cannot be satisfied.
        allocator.free[0].extend([0, 16384, 32768, 49152]);
        allocator.needs_coalesce = true;

        let mut memory = arena(1024 * 1024);

        assert!(allocator.alloc(&mut memory, 16384).is_none());
        assert!(!allocator.needs_coalesce);
    }

    #[test]
    fn failed_allocation_attempts_coalescing_when_needed() {
        let mut allocator = BuddyAllocator::new_with_block_size(1024 * 1024, 8192, 65536).unwrap();

        allocator.free.iter_mut().for_each(Vec::clear);

        // Two order-0 buddies can become one order-1 block.
        allocator.free[0].extend([0, 8192]);
        allocator.needs_coalesce = true;

        let mut memory = arena(1024 * 1024);

        let allocation = allocator
            .alloc(&mut memory, 16384 - ALLOCATOR_HEADER_SIZE)
            .unwrap();

        assert_eq!(allocation.block_offset, 0);
        assert_eq!(allocation_order(&memory, &allocation), 1);

        // Coalescing succeeded, so the lazy flag does not need to be cleared
        // by the failed-allocation path.
        assert!(allocator.free[0].is_empty());
    }

    #[test]
    fn freeing_a_small_block_marks_allocator_for_coalescing() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();

        assert!(!allocator.needs_coalesce);

        let allocation = allocator.alloc(&mut memory, 10).unwrap();
        assert_eq!(allocation_order(&memory, &allocation), 0);

        allocator.free(&mut memory, allocation).unwrap();

        assert!(allocator.needs_coalesce);
    }

    #[test]
    fn freeing_a_max_order_block_does_not_require_coalescing() {
        let mut memory = arena(256);
        let mut allocator = BuddyAllocator::new_with_block_size(256, 64, 256).unwrap();

        assert!(!allocator.needs_coalesce);

        let allocation = allocator
            .alloc(&mut memory, 256 - ALLOCATOR_HEADER_SIZE)
            .unwrap();

        assert_eq!(allocation_order(&memory, &allocation), allocator.max_order);

        allocator.free(&mut memory, allocation).unwrap();

        assert!(!allocator.needs_coalesce);
    }

    #[test]
    fn successful_allocation_after_lazy_coalescing_restores_accounting() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();

        let initial = allocator.available();

        let a = allocator.alloc(&mut memory, 10).unwrap();
        let b = allocator.alloc(&mut memory, 10).unwrap();

        allocator.free(&mut memory, a).unwrap();
        allocator.free(&mut memory, b).unwrap();

        assert!(allocator.needs_coalesce);

        let large = allocator
            .alloc(&mut memory, 128 - ALLOCATOR_HEADER_SIZE)
            .unwrap();

        assert_eq!(allocation_order(&memory, &large), 1);

        allocator.free(&mut memory, large).unwrap();
        allocator.coalesce_until(allocator.max_order);

        assert_eq!(allocator.available(), initial);
    }

    #[test]
    fn allocated_data_can_be_written_without_corrupting_headers() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();

        let a = allocator.alloc(&mut memory, 20).unwrap();
        let b = allocator.alloc(&mut memory, 100).unwrap();

        assert_ne!(a.block_offset, b.block_offset);

        // Fill the entire usable capacity of both allocations.
        for byte in &mut memory[a.data_offset..a.data_offset + a.cap] {
            *byte = 0xAA;
        }

        for byte in &mut memory[b.data_offset..b.data_offset + b.cap] {
            *byte = 0x55;
        }

        // Headers must still contain valid allocation metadata.
        let (a_magic, _) = read_header(&memory, a.block_offset).unwrap();
        let (b_magic, _) = read_header(&memory, b.block_offset).unwrap();

        assert_eq!(a_magic, MAGIC);
        assert_eq!(b_magic, MAGIC);

        // Data written to one allocation must not overwrite the other.
        assert!(
            memory[b.data_offset..b.data_offset + b.cap]
                .iter()
                .all(|&byte| byte == 0x55)
        );
        assert!(
            memory[a.data_offset..a.data_offset + a.cap]
                .iter()
                .all(|&byte| byte == 0xAA)
        );

        allocator.free(&mut memory, a).unwrap();
        allocator.free(&mut memory, b).unwrap();
    }

    #[test]
    fn allocation_data_does_not_overlap_header_or_other_blocks() {
        let mut memory = arena(4096);
        let mut allocator = BuddyAllocator::new_with_block_size(4096, 64, 1024).unwrap();

        let mut allocations = Vec::new();

        for size in [1, 20, 56, 57, 100, 120, 121, 248, 249] {
            if let Some(allocation) = allocator.alloc(&mut memory, size) {
                allocations.push(allocation);
            }
        }

        for allocation in &allocations {
            let (_, order) = read_header(&memory, allocation.block_offset)
                .expect("allocation should have a valid header");
            let block_size = allocator.min_block << order as usize;
            // The returned data starts immediately after its header.
            assert_eq!(
                allocation.data_offset,
                allocation.block_offset + ALLOCATOR_HEADER_SIZE
            );

            // Data stays inside the block.
            assert!(
                allocation.data_offset + allocation.cap <= allocation.block_offset + block_size
            );

            // Header and data cannot overlap.
            assert!(allocation.block_offset + ALLOCATOR_HEADER_SIZE <= allocation.data_offset);
        }

        for i in 0..allocations.len() {
            for j in i + 1..allocations.len() {
                let a = &allocations[i];
                let b = &allocations[j];

                let (_, a_order) = read_header(&memory, a.block_offset)
                    .expect("allocation should have a valid header");
                let (_, b_order) = read_header(&memory, b.block_offset)
                    .expect("allocation should have a valid header");

                let a_start = a.block_offset;
                let a_end = a.block_offset + (allocator.min_block << a_order as usize);

                let b_start = b.block_offset;
                let b_end = b.block_offset + (allocator.min_block << b_order as usize);

                assert!(a_end <= b_start || b_end <= a_start, "{a:?} overlaps {b:?}");
            }
        }

        for allocation in allocations {
            allocator.free(&mut memory, allocation).unwrap();
        }
    }

    #[test]
    fn reset_leaves_the_arena_untouched() {
        // A reset only rebuilds the free lists. Writing to the arena would fault every touched
        // page back in after a punch, for headers the frees have already cleared.
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();
        while allocator.alloc(&mut memory, 10).is_some() {}
        let before = memory.clone();

        allocator.reset(&mut memory).unwrap();

        assert_eq!(before, memory);
        assert_eq!(allocator.available(), 4 * (256 - ALLOCATOR_HEADER_SIZE));
    }

    #[test]
    fn reset_allows_the_same_offsets_to_be_allocated_again() {
        let mut memory = arena(1024);
        let mut allocator = BuddyAllocator::new_with_block_size(1024, 64, 256).unwrap();

        let first = allocator.alloc(&mut memory, 20).unwrap();

        allocator.reset(&mut memory).unwrap();

        let second = allocator.alloc(&mut memory, 20).unwrap();

        assert_eq!(first.block_offset, second.block_offset);
        assert_eq!(first.data_offset, second.data_offset);

        allocator.free(&mut memory, second).unwrap();
        allocator.coalesce_until(allocator.max_order);
    }
}
