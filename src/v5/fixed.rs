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
    config::SizePercentPair,
    v5::region::{
        ALLOCATOR_HEADER_SIZE, Allocation, Allocator, Error, MAGIC, clear_magic, read_header,
        write_header,
    },
};

/// 8 KiB
const DEFAULT_SMALL_SIZE: usize = 8 * 1024;
/// 64 KiB
const DEFAULT_MEDIUM_SIZE: usize = 64 * 1024;
/// 128 KiB
const DEFAULT_LARGE_SIZE: usize = 128 * 1024;
/// Share of the arena given to small blocks by default.
const SMALL_PERCENT: usize = 50;
/// Share of the arena given to medium blocks by default; large blocks take the rest.
const MEDIUM_PERCENT: usize = 30;

/// A slab allocator over one region's arena, and the alternative to
/// [`BuddyAllocator`](crate::v5::buddy::BuddyAllocator) behind `SHMIPC_MEM_ALLOCATOR=fixed`.
///
/// The arena is split once into three contiguous areas of same-sized blocks, and every
/// block carries an eight-byte `[MAGIC:4][len:4]` header at its start.
pub struct FixedAllocator {
    /// Length every arena passed to this allocator must have.
    arena_size: usize,
    /// Block size of each class, header included, ascending.
    sizes: [usize; 3],
    /// Payload bytes per block of each class: `sizes` less the header.
    usable: [usize; 3],
    /// Free block offsets for each class.
    free_lists: [Vec<usize>; 3],
    /// Arena offset where each size-class region starts.
    starts: [usize; 3],
    /// Number of blocks in each size class.
    counts: [usize; 3],
}

impl Allocator for FixedAllocator {
    /// Hands out a block from the smallest class that holds `size`, stamping its header.
    fn alloc(&mut self, arena: &mut [u8], size: usize) -> Option<Allocation> {
        if arena.len() != self.arena_size || size == 0 {
            return None;
        }

        let class = if size <= self.usable[0] {
            0
        } else if size <= self.usable[1] {
            1
        } else if size <= self.usable[2] {
            2
        } else {
            return None;
        };

        // Take the smallest suitable class, falling back to larger ones.
        let (class, block_offset) =
            (class..3).find_map(|c| self.alloc_from_class(c).map(|offset| (c, offset)))?;
        write_header(arena, block_offset, size);

        Some(Allocation {
            block_offset,
            data_offset: block_offset + ALLOCATOR_HEADER_SIZE,
            len: size,
            cap: self.usable[class],
        })
    }

    /// Largest request the arena can ever serve: one large block less its header.
    fn max_allocation(&self) -> usize {
        self.usable[2]
    }

    /// Returns a block described by the [`Allocation`] the allocator handed out.
    fn free(&mut self, arena: &mut [u8], allocation: Allocation) -> Result<(), Error> {
        self.free_at(arena, allocation.data_offset)
    }

    /// Returns a block named only by the offset of its data, the way a peer reports a
    /// release across shared memory.
    fn free_at(&mut self, arena: &mut [u8], data_offset: usize) -> Result<(), Error> {
        if arena.len() != self.arena_size {
            return Err(Error::InvalidArenaSize);
        }

        let block_offset = data_offset
            .checked_sub(ALLOCATOR_HEADER_SIZE)
            .ok_or(Error::InvalidOffset)?;
        let class = self
            .class_for_offset(block_offset)
            .ok_or(Error::InvalidOffset)?;

        let (magic, _) = read_header(arena, block_offset).ok_or(Error::InvalidOffset)?;
        if magic != MAGIC {
            return Err(Error::NotAllocated);
        }

        clear_magic(arena, block_offset);
        self.free_lists[class].push(block_offset);

        Ok(())
    }

    /// Whether `data_offset` could name the data of a block from this arena.
    fn is_valid_offset(&self, data_offset: usize) -> bool {
        let Some(block_offset) = data_offset.checked_sub(ALLOCATOR_HEADER_SIZE) else {
            return false;
        };

        self.class_for_offset(block_offset).is_some()
    }

    /// Payload bytes the free lists hold, counting each free block at its class capacity.
    fn available(&self) -> usize {
        let mut total = 0;
        for class in 0..3 {
            total += self.free_lists[class].len() * self.usable[class];
        }

        total
    }

    /// Hands the whole arena back to the allocator.
    ///
    /// Only the free lists are rebuilt; the arena is not written. A region is reset after a
    /// punch, and touching every block header would fault those pages straight back in, for
    /// magics the frees that emptied the region have already cleared. Every allocation handed
    /// out before the reset is invalid afterwards and must not be freed, because its header
    /// still reads as live.
    fn reset(&mut self, arena: &mut [u8]) -> Result<(), Error> {
        if arena.len() != self.arena_size {
            return Err(Error::InvalidArenaSize);
        }
        self.initialize_free_lists();

        Ok(())
    }
}

impl FixedAllocator {
    /// Creates a fixed-size allocator with default sizes (8KiB, 64KiB, 128KiB).
    /// The arena is divided: 50% small, 30% medium, 20% large.
    #[allow(unused)]
    pub fn new(arena: &[u8]) -> Result<Self, Error> {
        Self::new_with_sizes(
            arena,
            DEFAULT_SMALL_SIZE,
            DEFAULT_MEDIUM_SIZE,
            DEFAULT_LARGE_SIZE,
        )
    }

    /// Creates an allocator with the given class sizes and the default 50/30/20 split.
    pub fn new_with_sizes(
        arena: &[u8],
        small: usize,
        medium: usize,
        large: usize,
    ) -> Result<Self, Error> {
        Self::build(arena, small, medium, large, SMALL_PERCENT, MEDIUM_PERCENT)
    }

    /// Creates an allocator from a user's size-and-share configuration.
    ///
    /// Exactly three classes are expected, ascending in size, and their percentages have to
    /// sum to 100.
    pub fn new_from_config(arena: &[u8], classes: Vec<SizePercentPair>) -> Result<Self, Error> {
        if classes.len() != 3 {
            return Err(Error::InvalidSizeClassConfiguration);
        }

        let percent: u32 = classes.iter().map(|c| c.percent).sum();
        if percent != 100 {
            return Err(Error::InvalidSizeClassConfiguration);
        }

        Self::build(
            arena,
            classes[0].size as usize,
            classes[1].size as usize,
            classes[2].size as usize,
            classes[0].percent as usize,
            classes[1].percent as usize,
        )
    }

    /// Lays the three class areas out over the arena back to back and fills their free
    /// lists.
    ///
    /// Each area is rounded down to a whole number of its own blocks, so no area runs into
    /// the next, and the large one absorbs the remainder.
    fn build(
        arena: &[u8],
        small: usize,
        medium: usize,
        large: usize,
        small_percent: usize,
        medium_percent: usize,
    ) -> Result<Self, Error> {
        if small <= ALLOCATOR_HEADER_SIZE
            || medium <= ALLOCATOR_HEADER_SIZE
            || large <= ALLOCATOR_HEADER_SIZE
        {
            return Err(Error::BlockTooSmallForHeader);
        }
        if small > medium || medium > large {
            return Err(Error::InvalidBlockSizeOrder);
        }

        let total = arena.len();
        let small_region = (total * small_percent / 100 / small) * small;
        let medium_region = (total * medium_percent / 100 / medium) * medium;
        let remaining = total - small_region - medium_region;
        let large_region = (remaining / large) * large;

        if small_region == 0 || medium_region == 0 || large_region == 0 {
            return Err(Error::InvalidArenaSize);
        }
        let sizes = [small, medium, large];
        let usable = [
            small - ALLOCATOR_HEADER_SIZE,
            medium - ALLOCATOR_HEADER_SIZE,
            large - ALLOCATOR_HEADER_SIZE,
        ];
        let starts = [0, small_region, small_region + medium_region];
        let counts = [
            small_region / small,
            medium_region / medium,
            large_region / large,
        ];

        let mut allocator = Self {
            sizes,
            usable,
            starts,
            counts,
            free_lists: [
                Vec::with_capacity(counts[0]),
                Vec::with_capacity(counts[1]),
                Vec::with_capacity(counts[2]),
            ],
            arena_size: arena.len(),
        };

        allocator.initialize_free_lists();

        Ok(allocator)
    }

    /// Puts every block of every class back on its free list, which is the state a freshly
    /// built or freshly reset allocator is in.
    fn initialize_free_lists(&mut self) {
        for class in 0..3 {
            self.free_lists[class].clear();
            for index in 0..self.counts[class] {
                let offset = self.starts[class] + index * self.sizes[class];
                self.free_lists[class].push(offset);
            }
        }
    }

    /// Takes a block offset off one class's free list, or `None` when that class is drained.
    fn alloc_from_class(&mut self, class: usize) -> Option<usize> {
        self.free_lists[class].pop()
    }

    /// The class a block offset belongs to, or `None` when it lands outside every area or
    /// part-way into a block.
    fn class_for_offset(&self, offset: usize) -> Option<usize> {
        for class in (0..3).rev() {
            let start = self.starts[class];
            let end = start + self.counts[class] * self.sizes[class];

            if offset >= start && offset < end {
                let relative = offset - start;
                if relative.is_multiple_of(self.sizes[class]) {
                    return Some(class);
                }

                return None;
            }
        }

        None
    }

    /// Free blocks left in each class.
    #[allow(unused)]
    pub fn available_blocks(&self) -> [usize; 3] {
        [
            self.free_lists[0].len(),
            self.free_lists[1].len(),
            self.free_lists[2].len(),
        ]
    }

    /// Block size of each class, header included.
    #[allow(unused)]
    pub fn sizes(&self) -> [usize; 3] {
        self.sizes
    }

    /// Payload bytes per block of each class.
    #[allow(unused)]
    pub fn usable_sizes(&self) -> [usize; 3] {
        self.usable
    }

    /// Blocks each class was carved into.
    #[allow(unused)]
    pub fn block_counts(&self) -> [usize; 3] {
        self.counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arena(size: usize) -> Vec<u8> {
        vec![0u8; size]
    }

    fn default_allocator() -> (Vec<u8>, FixedAllocator) {
        let memory = arena(4 * 1024 * 1024);
        let allocator = FixedAllocator::new(&memory).unwrap();
        (memory, allocator)
    }

    fn small_allocator() -> (Vec<u8>, FixedAllocator) {
        let memory = arena(64 * 1024);
        let allocator = FixedAllocator::new_with_sizes(&memory, 1024, 2048, 4096).unwrap();
        (memory, allocator)
    }

    #[test]
    fn each_class_serves_a_request_of_its_own_size() {
        let (mut memory, mut allocator) = default_allocator();

        let small = allocator.alloc(&mut memory, 100).unwrap();
        assert_eq!(100, small.len);
        assert_eq!(DEFAULT_SMALL_SIZE - ALLOCATOR_HEADER_SIZE, small.cap);

        let medium = allocator.alloc(&mut memory, 10 * 1024).unwrap();
        assert_eq!(10 * 1024, medium.len);
        assert_eq!(DEFAULT_MEDIUM_SIZE - ALLOCATOR_HEADER_SIZE, medium.cap);

        let large = allocator.alloc(&mut memory, 100 * 1024).unwrap();
        assert_eq!(100 * 1024, large.len);
        assert_eq!(DEFAULT_LARGE_SIZE - ALLOCATOR_HEADER_SIZE, large.cap);

        allocator.free(&mut memory, small).unwrap();
        allocator.free(&mut memory, medium).unwrap();
        allocator.free(&mut memory, large).unwrap();
    }

    #[test]
    fn a_request_lands_in_the_smallest_class_that_fits() {
        let mut memory = arena(1024 * 1024);
        let mut allocator = FixedAllocator::new_with_sizes(&memory, 1024, 4096, 16384).unwrap();
        let usable = allocator.usable_sizes();

        assert_eq!(
            usable[0],
            allocator.alloc(&mut memory, usable[0]).unwrap().cap
        );
        assert_eq!(
            usable[1],
            allocator.alloc(&mut memory, usable[0] + 1).unwrap().cap
        );
        assert_eq!(
            usable[2],
            allocator.alloc(&mut memory, usable[1] + 1).unwrap().cap
        );
        // Nothing serves a request larger than the biggest class.
        assert!(allocator.alloc(&mut memory, usable[2] + 1).is_none());
        assert!(allocator.alloc(&mut memory, 0).is_none());
    }

    #[test]
    fn a_block_from_a_larger_class_reports_that_class_capacity() {
        let (mut memory, mut allocator) = small_allocator();
        let usable = allocator.usable_sizes();
        let counts = allocator.block_counts();

        // Drain the small class with requests only the small class would pick.
        for _ in 0..counts[0] {
            assert_eq!(usable[0], allocator.alloc(&mut memory, 512).unwrap().cap);
        }

        // The next one falls back to a medium block.
        assert_eq!(usable[1], allocator.alloc(&mut memory, 512).unwrap().cap);
    }

    #[test]
    fn allocation_stops_when_every_class_is_drained_and_resumes_after_a_free() {
        let (mut memory, mut allocator) = small_allocator();
        let counts = allocator.block_counts();
        let total = counts[0] + counts[1] + counts[2];

        let mut blocks = Vec::new();
        while let Some(block) = allocator.alloc(&mut memory, 512) {
            blocks.push(block);
            assert!(blocks.len() <= total, "handed out more blocks than exist");
        }
        assert_eq!(total, blocks.len());

        allocator.free(&mut memory, blocks.remove(0)).unwrap();
        assert!(allocator.alloc(&mut memory, 512).is_some());
    }

    #[test]
    fn freeing_a_block_twice_is_rejected() {
        let (mut memory, mut allocator) = small_allocator();
        let block = allocator.alloc(&mut memory, 100).unwrap();

        allocator.free(&mut memory, block).unwrap();
        assert_eq!(Err(Error::NotAllocated), allocator.free(&mut memory, block));
    }

    #[test]
    fn freeing_an_offset_outside_the_arena_is_rejected() {
        let (mut memory, mut allocator) = small_allocator();

        // Before the first header, past the end, and in the middle of a block.
        assert_eq!(Err(Error::InvalidOffset), allocator.free_at(&mut memory, 0));
        let past_end = memory.len() + 1000;
        assert_eq!(
            Err(Error::InvalidOffset),
            allocator.free_at(&mut memory, past_end)
        );
        assert_eq!(
            Err(Error::InvalidOffset),
            allocator.free_at(&mut memory, ALLOCATOR_HEADER_SIZE + 1)
        );
    }

    #[test]
    fn free_at_releases_the_block_behind_a_data_offset() {
        let (mut memory, mut allocator) = small_allocator();
        let before = allocator.available();
        let block = allocator.alloc(&mut memory, 100).unwrap();

        allocator.free_at(&mut memory, block.data_offset).unwrap();
        assert_eq!(before, allocator.available());

        assert_eq!(
            Err(Error::NotAllocated),
            allocator.free_at(&mut memory, block.data_offset)
        );
    }

    #[test]
    fn an_arena_of_the_wrong_size_is_refused() {
        let (_, mut allocator) = small_allocator();
        let mut wrong = arena(32 * 1024);

        assert!(allocator.alloc(&mut wrong, 100).is_none());
        assert_eq!(
            Err(Error::InvalidArenaSize),
            allocator.free_at(&mut wrong, ALLOCATOR_HEADER_SIZE)
        );
        assert_eq!(Err(Error::InvalidArenaSize), allocator.reset(&mut wrong));
    }

    #[test]
    fn only_offsets_naming_a_block_are_valid() {
        let (mut memory, mut allocator) = small_allocator();
        let block = allocator.alloc(&mut memory, 100).unwrap();

        assert!(allocator.is_valid_offset(block.data_offset));
        // Ahead of the first block's data, past the arena, and misaligned.
        assert!(!allocator.is_valid_offset(0));
        let past_end = memory.len() + 1000;
        assert!(!allocator.is_valid_offset(past_end));
        assert!(!allocator.is_valid_offset(ALLOCATOR_HEADER_SIZE + 1));
    }

    #[test]
    fn available_tracks_what_the_free_lists_hold() {
        let (mut memory, mut allocator) = small_allocator();
        let before = allocator.available();
        assert!(before > 0);

        let block = allocator.alloc(&mut memory, 512).unwrap();
        assert!(allocator.available() < before);

        allocator.free(&mut memory, block).unwrap();
        assert_eq!(before, allocator.available());
    }

    #[test]
    fn reset_returns_every_block_without_touching_the_arena() {
        let (mut memory, mut allocator) = small_allocator();
        let before = allocator.available();
        for _ in 0..10 {
            allocator.alloc(&mut memory, 512).unwrap();
        }
        assert!(allocator.available() < before);
        let bytes = memory.clone();

        allocator.reset(&mut memory).unwrap();
        assert_eq!(before, allocator.available());
        assert_eq!(allocator.block_counts(), allocator.available_blocks());

        // Only the free lists change: the arena, headers included, is left as it was.
        assert_eq!(bytes, memory);
    }

    #[test]
    fn sizes_and_arenas_are_validated() {
        let memory = arena(64 * 1024);

        // Small class no larger than the header it carries.
        assert_eq!(
            Some(Error::BlockTooSmallForHeader),
            FixedAllocator::new_with_sizes(&memory, 4, 1024, 2048).err()
        );
        // Classes out of order.
        assert_eq!(
            Some(Error::InvalidBlockSizeOrder),
            FixedAllocator::new_with_sizes(&memory, 2048, 1024, 4096).err()
        );
        // An arena too small to hold one block of every class.
        assert_eq!(
            Some(Error::InvalidArenaSize),
            FixedAllocator::new_with_sizes(&arena(100), 1024, 2048, 4096).err()
        );
    }

    #[test]
    fn a_config_sets_the_class_sizes_and_the_split_between_them() {
        let mut memory = arena(1024 * 1024);
        let pairs = |percents: [u32; 3]| {
            [1024u32, 4096, 16384]
                .iter()
                .zip(percents)
                .map(|(&size, percent)| SizePercentPair { size, percent })
                .collect::<Vec<_>>()
        };

        let mut allocator = FixedAllocator::new_from_config(&memory, pairs([50, 30, 20])).unwrap();
        assert_eq!([1024, 4096, 16384], allocator.sizes());
        assert!(allocator.alloc(&mut memory, 100).is_some());
        assert!(allocator.alloc(&mut memory, 2000).is_some());
        assert!(allocator.alloc(&mut memory, 5000).is_some());

        // A larger small percentage buys more small blocks.
        let skewed = FixedAllocator::new_from_config(&memory, pairs([70, 20, 10])).unwrap();
        assert!(skewed.block_counts()[0] > allocator.block_counts()[0]);
    }

    #[test]
    fn a_config_needs_three_classes_summing_to_a_hundred_percent() {
        let memory = arena(1024 * 1024);

        assert_eq!(
            Some(Error::InvalidSizeClassConfiguration),
            FixedAllocator::new_from_config(
                &memory,
                vec![SizePercentPair {
                    size: 1024,
                    percent: 100,
                }],
            )
            .err()
        );
        assert_eq!(
            Some(Error::InvalidSizeClassConfiguration),
            FixedAllocator::new_from_config(
                &memory,
                vec![
                    SizePercentPair {
                        size: 1024,
                        percent: 50,
                    },
                    SizePercentPair {
                        size: 4096,
                        percent: 30,
                    },
                    SizePercentPair {
                        size: 16384,
                        percent: 10,
                    },
                ],
            )
            .err()
        );
    }
}
