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

use core::slice;
use std::{cmp::min, ptr::NonNull};

use crate::{
    buffer::manager::{
        BUFFER_CAP_OFFSET, BUFFER_DATA_START_OFFSET, BUFFER_FLAG_OFFSET, BUFFER_SIZE_OFFSET,
        HAS_NEXT_BUFFER_FLAG, NEXT_BUFFER_OFFSET, SLICE_IN_USED_FLAG,
    },
    error::Error,
};

#[derive(Default, Debug)]
pub struct SliceList {
    pub front_slice: Option<NonNull<BufferSlice>>,
    pub write_slice: Option<NonNull<BufferSlice>>,
    pub back_slice: Option<NonNull<BufferSlice>>,
    len: usize,
}

impl SliceList {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn front(&self) -> Option<&BufferSlice> {
        unsafe { self.front_slice.map(|node| &(*node.as_ptr())) }
    }

    #[inline]
    pub fn front_mut(&self) -> Option<&mut BufferSlice> {
        unsafe { self.front_slice.map(|node| &mut (*node.as_ptr())) }
    }

    #[inline]
    pub fn back(&self) -> Option<&BufferSlice> {
        unsafe { self.back_slice.map(|node| &(*node.as_ptr())) }
    }

    #[inline]
    pub fn back_mut(&self) -> Option<&mut BufferSlice> {
        unsafe { self.back_slice.map(|node| &mut (*node.as_ptr())) }
    }

    #[inline]
    pub fn write(&self) -> Option<&BufferSlice> {
        unsafe { self.write_slice.map(|node| &(*node.as_ptr())) }
    }

    #[inline]
    pub fn write_mut(&self) -> Option<&mut BufferSlice> {
        unsafe { self.write_slice.map(|node| &mut (*node.as_ptr())) }
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.len
    }

    pub fn push_back(&mut self, s: BufferSlice) {
        unsafe {
            let new = NonNull::new_unchecked(Box::into_raw(Box::new(s)));
            if self.len > 0 {
                self.back_slice.unwrap().as_mut().next_slice = Some(new);
            } else {
                self.front_slice = Some(new);
            }
            self.back_slice = Some(new);
            self.len += 1;
        }
    }

    pub fn pop_front(&mut self) -> Option<BufferSlice> {
        unsafe {
            let r = self.front_slice;
            if self.len > 0 {
                self.len -= 1;
                self.front_slice = self.front_slice.unwrap().as_ref().next_slice;
            }
            if self.len == 0 {
                self.front_slice = None;
                self.back_slice = None;
            }
            r.map(|node| *Box::from_raw(node.as_ptr()))
        }
    }

    pub fn split_from_write(&mut self) -> Option<BufferSlice> {
        unsafe {
            self.write_slice.and_then(|slice| {
                let next_list_head = (*slice.as_ptr()).next_slice;
                self.back_slice = Some(slice);
                (*slice.as_ptr()).next_slice = None;
                let mut next_list_size = 0;
                let mut s = next_list_head;
                while s.is_some() {
                    next_list_size += 1;
                    s = (*s.unwrap_unchecked().as_ptr()).next_slice;
                }
                self.len -= next_list_size;
                next_list_head.map(|head| *Box::from_raw(head.as_ptr()))
            })
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct BufferSlice {
    /// BufferHeader layout: cap 4 byte | size 4 byte | start 4 byte | next 4 byte | flag 2 byte |
    /// unused 2 byte
    pub buffer_header: Option<BufferHeader>,
    pub data: *mut u8,
    pub cap: u32,
    /// use for prepend
    pub start: u32,
    pub offset_in_shm: u32,
    pub read_index: usize,
    pub write_index: usize,
    pub is_from_shm: bool,
    pub next_slice: Option<NonNull<BufferSlice>>,
}

unsafe impl Send for BufferSlice {}
unsafe impl Sync for BufferSlice {}

impl BufferSlice {
    pub fn new(
        header: Option<BufferHeader>,
        data: &mut [u8],
        offset_in_shm: u32,
        is_from_shm: bool,
    ) -> Self {
        debug_assert!(!data.is_empty());

        let len = data.len() as u32;
        let mut s = Self {
            buffer_header: None,
            data: data.as_mut_ptr(),
            cap: 0,
            start: 0,
            offset_in_shm,
            read_index: 0,
            write_index: 0,
            is_from_shm,
            next_slice: None,
        };
        if is_from_shm && header.is_some() {
            let buffer_header = header.unwrap();
            s.cap = buffer_header.cap();
            s.start = buffer_header.start();
            s.write_index = (s.start + buffer_header.size()) as usize;
            s.buffer_header = Some(buffer_header);
        } else {
            s.cap = len;
        }
        s
    }

    pub fn update(&self) {
        if let Some(buffer_header) = &self.buffer_header {
            buffer_header.set_size(self.size() as u32);
            buffer_header.set_start(self.start);

            if let Some(next_slice) = &self.next_slice {
                unsafe {
                    buffer_header.link_next((*next_slice.as_ptr()).offset_in_shm);
                }
            }
        }
    }

    pub fn reset(&mut self) {
        if let Some(buffer_header) = &self.buffer_header {
            buffer_header.set_size(0);
            buffer_header.set_start(0);
            buffer_header.clear_flag()
        }
        self.write_index = 0;
        self.read_index = 0;
        self.next_slice = None;
    }

    pub fn size(&self) -> usize {
        self.write_index - self.read_index
    }

    pub fn remain(&self) -> usize {
        self.cap as usize - self.write_index
    }

    pub fn capacity(&self) -> usize {
        self.cap as usize
    }

    pub fn reserve(&mut self, size: usize) -> Result<&mut [u8], Error> {
        let start = self.write_index;
        let remain = self.remain();
        if remain >= size {
            self.write_index += size;
            return Ok(unsafe { slice::from_raw_parts_mut(self.data.add(start), size) });
        }
        Err(Error::NoMoreBuffer)
    }

    pub fn append(&mut self, data: &[u8]) -> usize {
        if data.is_empty() {
            return 0;
        }
        let copy_size = min(data.len(), self.remain());
        unsafe {
            self.data
                .add(self.write_index)
                .copy_from_nonoverlapping(data.as_ptr(), copy_size)
        };
        self.write_index += copy_size;
        copy_size
    }

    #[must_use]
    pub fn read(&mut self, mut size: usize) -> &[u8] {
        size = min(size, self.size());
        let data = unsafe { slice::from_raw_parts(self.data.add(self.read_index), size) };
        self.read_index += size;
        data
    }

    pub fn peek(&mut self, mut size: usize) -> &[u8] {
        size = min(size, self.size());
        unsafe { slice::from_raw_parts(self.data.add(self.read_index), size) }
    }

    pub fn skip(&mut self, size: usize) -> usize {
        let un_read = self.size();
        if un_read > size {
            self.read_index += size;
            return size;
        }
        self.read_index += un_read;
        un_read
    }

    #[inline]
    pub fn next(&self) -> Option<&BufferSlice> {
        unsafe { self.next_slice.map(|node| &(*node.as_ptr())) }
    }

    #[inline]
    pub fn next_mut(&self) -> Option<&mut BufferSlice> {
        unsafe { self.next_slice.map(|node| &mut (*node.as_ptr())) }
    }
}

/// BufferHeader is the header of a buffer slice.
///
/// Layout: cap 4 byte | size 4 byte | start 4 byte | next 4 byte | flag 2 byte | unused 2 byte
///
/// # Safety
///
/// Make sure it is well initialized before use and see ptr.offset method safety requirements.
#[derive(Eq, PartialEq, Debug, Clone)]
pub struct BufferHeader(pub *mut u8);

impl BufferHeader {
    #[inline]
    pub fn next_buffer_offset(&self) -> u32 {
        unsafe { *(self.0.offset(NEXT_BUFFER_OFFSET as isize) as *const u32) }
    }

    #[inline]
    pub fn has_next(&self) -> bool {
        unsafe { *self.0.offset(BUFFER_FLAG_OFFSET as isize) & HAS_NEXT_BUFFER_FLAG > 0 }
    }

    #[inline]
    pub fn clear_flag(&self) {
        unsafe {
            *self.0.offset(BUFFER_FLAG_OFFSET as isize) = 0u8;
        }
    }

    #[inline]
    pub fn set_in_used(&self) {
        unsafe {
            *self.0.offset(BUFFER_FLAG_OFFSET as isize) |= SLICE_IN_USED_FLAG;
        }
    }

    #[inline]
    pub fn is_in_used(&self) -> bool {
        unsafe { *self.0.offset(BUFFER_FLAG_OFFSET as isize) & SLICE_IN_USED_FLAG > 0 }
    }

    #[inline]
    pub fn link_next(&self, next: u32) {
        unsafe {
            *(self.0.offset(NEXT_BUFFER_OFFSET as isize) as *mut u32) = next;
            *self.0.offset(BUFFER_FLAG_OFFSET as isize) |= HAS_NEXT_BUFFER_FLAG;
        }
    }

    #[inline]
    pub fn cap(&self) -> u32 {
        unsafe { *(self.0.offset(BUFFER_CAP_OFFSET as isize) as *const u32) }
    }

    #[inline]
    pub fn size(&self) -> u32 {
        unsafe { *(self.0.offset(BUFFER_SIZE_OFFSET as isize) as *const u32) }
    }

    #[inline]
    pub fn set_size(&self, size: u32) {
        unsafe {
            *(self.0.offset(BUFFER_SIZE_OFFSET as isize) as *mut u32) = size;
        }
    }

    #[inline]
    pub fn start(&self) -> u32 {
        unsafe { *(self.0.offset(BUFFER_DATA_START_OFFSET as isize) as *const u32) }
    }

    #[inline]
    pub fn set_start(&self, start: u32) {
        unsafe {
            *(self.0.offset(BUFFER_DATA_START_OFFSET as isize) as *mut u32) = start;
        }
    }
}

#[cfg(test)]
mod tests {
    use core::slice;

    use memmap2::MmapOptions;
    use rand::Rng;

    use super::{BufferSlice, SliceList};
    use crate::{
        buffer::{
            manager::{BUFFER_CAP_OFFSET, BUFFER_HEADER_SIZE, BufferManager},
            slice::BufferHeader,
        },
        config::SizePercentPair,
    };

    #[test]
    fn test_buffer_slice_read_write() {
        const SIZE: usize = 8192;

        let mut buf = [0u8; SIZE];
        let mut slice = BufferSlice::new(None, &mut buf, 0, false);
        for i in 0..SIZE {
            let n = slice.append(&[i as u8]);
            assert_eq!(n, 1);
        }
        let n = slice.append(&[10]);
        assert_eq!(n, 0);

        let data = slice.read(SIZE * 10);
        assert_eq!(data.len(), SIZE);

        // vertfy read data.
        (0..SIZE).for_each(|i| {
            assert_eq!(data[i], i as u8);
        });
    }

    #[test]
    fn test_buffer_slice_skip() {
        const SIZE: usize = 8192;

        let mut buf = [0u8; SIZE];
        let mut slice = BufferSlice::new(None, &mut buf, 0, false);
        slice.append(&[0u8; SIZE]);
        let mut remain = slice.capacity();

        let n = slice.skip(10);
        remain -= n;
        assert_eq!(remain, slice.size());

        let n = slice.skip(100);
        remain -= n;
        assert_eq!(remain, slice.size());

        _ = slice.skip(10000);
        assert_eq!(0, slice.size());
    }

    #[test]
    fn test_buffer_slice_reserve() {
        const SIZE: usize = 8192;

        let mut buf = [0u8; SIZE];
        let mut slice = BufferSlice::new(None, &mut buf, 0, false);
        let data1 = slice.reserve(100).unwrap();
        assert_eq!(100, data1.len());

        (0..data1.len()).for_each(|i| data1[i] = i as u8);
        let data1 = unsafe { slice::from_raw_parts(data1.as_ptr(), data1.len()) };

        let data2 = slice.reserve(SIZE);
        assert!(data2.is_err());

        let read_data = slice.read(100);
        assert_eq!(100, read_data.len());

        (0..100).for_each(|i| {
            assert_eq!(read_data[i], data1[i]);
        });

        let read_data = slice.read(10000);
        assert_eq!(read_data.len(), 0);
    }

    #[test]
    fn test_buffer_slice_update() {
        const SIZE: usize = 8192;

        let mut buf = [0u8; SIZE];
        let mut header = [0u8; BUFFER_HEADER_SIZE as usize];
        unsafe {
            *(header.as_mut_ptr().offset(BUFFER_CAP_OFFSET as isize) as *mut u32) = 8192u32;
        }
        let mut slice =
            BufferSlice::new(Some(BufferHeader(header.as_mut_ptr())), &mut buf, 0, true);

        let n = slice.append(&[0u8; SIZE]);
        assert_eq!(n, SIZE);
        slice.update();
        assert_eq!(SIZE, slice.buffer_header.as_ref().unwrap().size() as usize);
    }

    #[test]
    fn test_buffer_slice_linked_next() {
        const SIZE: usize = 8192;
        const SLICE_NUM: usize = 100;

        let mut slices = Vec::with_capacity(SLICE_NUM);
        let mem = MmapOptions::new().len(10 << 20).map_anon().unwrap();
        let bm = BufferManager::create(
            &[SizePercentPair {
                size: 8192,
                percent: 100,
            }],
            "",
            mem,
            0,
        )
        .unwrap();

        let mut write_data_array = Vec::with_capacity(100);

        for _ in 0..SLICE_NUM {
            let mut s = bm.alloc_shm_buffer(SIZE as u32).unwrap();
            let mut rng = rand::rng();
            let data: Vec<u8> = (0..SIZE).map(|_| rng.random()).collect();
            assert_eq!(s.append(&data), SIZE);
            s.update();
            slices.push(s);
            write_data_array.push(data);
        }

        for i in 0..slices.len() - 1 {
            slices[i]
                .buffer_header
                .as_ref()
                .unwrap()
                .link_next(slices[i + 1].offset_in_shm);
        }

        let mut next = slices[0].offset_in_shm;
        (0..SLICE_NUM).for_each(|i| {
            let mut s = bm.read_buffer_slice(next).unwrap();
            assert_eq!(s.capacity(), SIZE);
            assert_eq!(s.size(), SIZE);
            let read_data = s.read(SIZE);
            assert_eq!(read_data.len(), SIZE);
            (0..SIZE).for_each(|j| {
                assert_eq!(read_data[j], write_data_array[i][j]);
            });
            let is_last_slice = i == SLICE_NUM - 1;
            assert_eq!(s.buffer_header.as_ref().unwrap().has_next(), !is_last_slice);
            next = s.buffer_header.as_ref().unwrap().next_buffer_offset();
        });
    }

    #[test]
    fn test_slice_list_push_pop() {
        // 1. twice push, twice pop
        let mut l = SliceList::new();
        l.push_back(BufferSlice::new(None, &mut [0; 1024], 0, false));
        assert_eq!(l.front(), l.back());
        assert_eq!(1, l.size());

        l.push_back(BufferSlice::new(None, &mut [0; 1024], 0, false));
        assert_eq!(2, l.size());
        assert_ne!(l.front(), l.back());

        l.pop_front();
        assert_eq!(1, l.size());
        assert_eq!(l.front(), l.back());

        l.pop_front();
        assert_eq!(0, l.size());
        assert!(l.front().is_none());
        assert!(l.back().is_none());

        // multi push and pop
        for i in 0..100 {
            l.push_back(BufferSlice::new(None, &mut [0; 1024], 0, false));
            assert_eq!(i + 1, l.size());
        }
        for i in 0..100 {
            l.pop_front();
            assert_eq!(100 - (i + 1), l.size());
        }
        assert_eq!(0, l.size());
        assert!(l.front().is_none());
        assert!(l.back().is_none());
    }

    #[test]
    fn test_slice_list_split_from_write() {
        // 1. sliceList's size == 1
        let mut l = SliceList::new();
        l.push_back(BufferSlice::new(None, &mut [0; 1024], 0, false));
        l.write_slice = l.front_slice;
        assert!(l.split_from_write().is_none());
        assert_eq!(1, l.size());
        assert_eq!(l.front(), l.back());
        assert_eq!(l.back(), l.write());

        // 2. sliceList's size == 2, writeSlice's index is 0
        let mut l = SliceList::new();
        l.push_back(BufferSlice::new(None, &mut [0; 1024], 0, false));
        l.push_back(BufferSlice::new(None, &mut [0; 1024], 0, false));
        l.write_slice = l.front_slice;
        l.split_from_write();

        assert_eq!(1, l.size());
        assert_eq!(l.front(), l.back());
        assert_eq!(l.back(), l.write());

        // 2. sliceList's size == 2, writeSlice's index is 1
        let mut l = SliceList::new();
        l.push_back(BufferSlice::new(None, &mut [0; 1024], 0, false));
        l.push_back(BufferSlice::new(None, &mut [0; 1024], 0, false));
        l.write_slice = l.back_slice;
        assert!(l.split_from_write().is_none());
        assert_eq!(2, l.size());
        assert_eq!(l.back(), l.write());

        // 3. sliceList's size == 3, writeSlice's index is 50
        let mut l = SliceList::new();
        for i in 0..100 {
            l.push_back(BufferSlice::new(None, &mut [0; 1024], 0, false));
            if i == 50 {
                l.write_slice = l.back_slice;
            }
        }
        l.split_from_write();
        assert_eq!(l.back(), l.write());

        assert_eq!(51, l.size());
    }
}
