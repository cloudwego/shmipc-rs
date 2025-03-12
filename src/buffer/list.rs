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

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use anyhow::anyhow;
use memmap2::MmapMut;

use super::slice::{BufferHeader, BufferSlice};
use crate::{
    Error,
    buffer::manager::{
        BUFFER_DATA_START_OFFSET, BUFFER_FLAG_OFFSET, BUFFER_HEADER_SIZE, BUFFER_SIZE_OFFSET,
        HAS_NEXT_BUFFER_FLAG, NEXT_BUFFER_OFFSET,
    },
};

/// size 4 | cap 4 | head 4 | tail 4 | capPerBuffer 4 | push count 8 | pop count 8
pub const BUFFER_LIST_HEADER_SIZE: u32 = 36;

/// BufferList's layout in share memory: size 4 byte | cap 4 byte | head 4 byte | tail 4 byte |
/// cap_per_buffer 4 byte | buffer_region n byte
///
/// Thread safe, lock free. Support push & pop concurrently even cross different process.
#[derive(Clone, Copy, Debug)]
pub struct BufferList {
    /// the number of free buffer in list
    pub(crate) size: *const AtomicI32,
    /// the capacity of list
    pub(crate) cap: *const AtomicU32,
    /// it points to the first free buffer, whose offset in buffer_region
    pub(crate) head: *const AtomicU32,
    /// it points to the last free buffer, whose offset in buffer_region
    pub(crate) tail: *const AtomicU32,
    /// the capacity of per buffer
    pub(crate) cap_per_buffer: *mut u32,
    pub(crate) counter: *const AtomicI32,
    /// underlying memory
    buffer_region: *mut u8,
    buffer_region_len: u32,
    buffer_region_offset_in_shm: u32,
    #[allow(dead_code)]
    /// the buffer_list's location offset in share memory
    pub(crate) offset_in_shm: u32,
}

unsafe impl Send for BufferList {}
unsafe impl Sync for BufferList {}

impl BufferList {
    /// Create a new BufferList in share memory, used for client side.
    pub fn create(
        buffer_num: u32,
        cap_per_buffer: u32,
        mem: &MmapMut,
        offset_in_mem: u32,
    ) -> Result<Self, anyhow::Error> {
        if buffer_num == 0 || cap_per_buffer == 0 {
            return Err(anyhow!(
                "buffer_num:{} and cap_per_buffer:{} could not be 0",
                buffer_num,
                cap_per_buffer
            ));
        }
        let at_least_size = count_buffer_list_mem_size(buffer_num, cap_per_buffer);
        let mem_len = mem.len() as u32;
        if mem_len < (offset_in_mem + at_least_size)
            || offset_in_mem > mem_len
            || at_least_size > mem_len
        {
            return Err(anyhow!(
                "mem's size is at least:{} but:{} offset_in_mem:{} at_least_size:{}",
                offset_in_mem + at_least_size,
                mem_len,
                offset_in_mem,
                at_least_size
            ));
        }

        let buffer_region_start = offset_in_mem + BUFFER_LIST_HEADER_SIZE;
        let buffer_region_end = offset_in_mem + at_least_size;
        if buffer_region_end <= buffer_region_start {
            return Err(anyhow!(
                "buffer_region_start:{} buffer_region_end:{} slice bounds out of range",
                buffer_region_start,
                buffer_region_end
            ));
        }
        let ptr = mem.as_ptr();

        let offset = offset_in_mem as isize;
        let b = Self {
            size: unsafe { ptr.offset(offset) as *const AtomicI32 },
            cap: unsafe { ptr.offset(offset + 4) as *const AtomicU32 },
            head: unsafe { ptr.offset(offset + 8) as *const AtomicU32 },
            tail: unsafe { ptr.offset(offset + 12) as *const AtomicU32 },
            cap_per_buffer: unsafe { ptr.offset(offset + 16) as *mut u32 },
            counter: unsafe { ptr.offset(offset + 20) as *const AtomicI32 },
            buffer_region: unsafe {
                ptr.offset(offset + BUFFER_LIST_HEADER_SIZE as isize) as *mut u8
            },
            buffer_region_len: at_least_size - BUFFER_LIST_HEADER_SIZE,
            buffer_region_offset_in_shm: offset_in_mem + BUFFER_LIST_HEADER_SIZE,
            offset_in_shm: offset_in_mem,
        };
        unsafe {
            (*b.size).store(buffer_num as i32, Ordering::SeqCst);
            (*b.cap).store(buffer_num, Ordering::SeqCst);
            (*b.head).store(0, Ordering::SeqCst);
            (*b.tail).store(
                (buffer_num - 1) * (cap_per_buffer + BUFFER_HEADER_SIZE),
                Ordering::SeqCst,
            );
            *b.cap_per_buffer = cap_per_buffer;
            (*b.counter).store(0, Ordering::SeqCst);
        }

        tracing::info!(
            "create buffer list: buffer_num:{} cap_per_buffer:{} offset_in_mem:{} need_size:{} \
             buffer_region_len:{}",
            buffer_num,
            cap_per_buffer,
            offset_in_mem,
            at_least_size,
            at_least_size - BUFFER_LIST_HEADER_SIZE,
        );

        let mut current = 0;
        let mut next;
        for i in 0..buffer_num {
            next = current + cap_per_buffer + BUFFER_HEADER_SIZE;
            unsafe {
                *(b.buffer_region.offset(current as isize) as *mut u32) = cap_per_buffer;
                *(b.buffer_region
                    .offset((current + BUFFER_SIZE_OFFSET) as isize)
                    as *mut u32) = 0;
                *(b.buffer_region
                    .offset((current + BUFFER_DATA_START_OFFSET) as isize)
                    as *mut u32) = 0;
                if i < (buffer_num - 1) {
                    *(b.buffer_region
                        .offset((current + NEXT_BUFFER_OFFSET) as isize)
                        as *mut u32) = next;
                    *(b.buffer_region
                        .offset((current + BUFFER_FLAG_OFFSET) as isize)
                        as *mut u32) = 0;
                    *b.buffer_region
                        .offset((current + BUFFER_FLAG_OFFSET) as isize) |= HAS_NEXT_BUFFER_FLAG;
                }
            }
            current = next;
        }
        unsafe {
            let tail = b
                .buffer_region
                .offset((*b.tail).load(Ordering::SeqCst) as isize);
            *(tail.offset(BUFFER_FLAG_OFFSET as isize) as *mut u32) = 0;
        }

        Ok(b)
    }

    /// Mapping a BufferList from share memory, used for server side.
    pub fn mapping(mem: &MmapMut, offset_in_shm: u32) -> Result<Self, anyhow::Error> {
        if mem.len() < (offset_in_shm + BUFFER_LIST_HEADER_SIZE) as usize {
            return Err(anyhow!(
                "mapping buffer list failed, mem's size is at least {}, ",
                offset_in_shm + BUFFER_LIST_HEADER_SIZE,
            ));
        }

        let ptr = mem.as_ptr();
        let offset = offset_in_shm as isize;

        let size = unsafe { ptr.offset(offset) as *const AtomicI32 };
        let cap = unsafe { ptr.offset(offset + 4) as *const AtomicU32 };
        let head = unsafe { ptr.offset(offset + 8) as *const AtomicU32 };
        let tail = unsafe { ptr.offset(offset + 12) as *const AtomicU32 };
        let cap_per_buffer = unsafe { ptr.offset(offset + 16) as *mut u32 };
        let counter = unsafe { ptr.offset(offset + 24) as *const AtomicI32 };

        let need_size =
            count_buffer_list_mem_size(unsafe { (*cap).load(Ordering::SeqCst) }, unsafe {
                *cap_per_buffer
            });
        if offset_in_shm + need_size > mem.len() as u32
            || offset_in_shm + need_size < offset_in_shm + BUFFER_LIST_HEADER_SIZE
        {
            return Err(unsafe {
                anyhow!(
                    "mapping buffer list failed, size:{} cap:{} head:{} tail:{} cap_per_buffer:{} \
                     err: mem's size is at least {} but:{}",
                    (*size).load(Ordering::SeqCst),
                    (*cap).load(Ordering::SeqCst),
                    (*head).load(Ordering::SeqCst),
                    (*tail).load(Ordering::SeqCst),
                    *cap_per_buffer,
                    need_size,
                    mem.len()
                )
            });
        }

        Ok(Self {
            size,
            cap,
            head,
            tail,
            cap_per_buffer,
            counter,
            buffer_region: unsafe {
                ptr.offset(offset + BUFFER_LIST_HEADER_SIZE as isize) as *mut u8
            },
            buffer_region_len: need_size - BUFFER_LIST_HEADER_SIZE,
            buffer_region_offset_in_shm: offset_in_shm + BUFFER_LIST_HEADER_SIZE,
            offset_in_shm,
        })
    }

    /// Push a buffer to the list.
    pub fn push(&self, mut buffer: BufferSlice) {
        buffer.reset();
        loop {
            let old_tail = unsafe { (*self.tail).load(Ordering::SeqCst) };
            let new_tail = buffer.offset_in_shm - self.buffer_region_offset_in_shm;
            unsafe {
                if (*self.tail)
                    .compare_exchange(old_tail, new_tail, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    BufferHeader(self.buffer_region.offset(old_tail as isize)).link_next(new_tail);
                    (*self.size).fetch_add(1, Ordering::SeqCst);
                    (*self.counter).fetch_sub(1, Ordering::SeqCst);

                    return;
                }
            }
        }
    }

    /// Pop a buffer from the list.
    ///
    /// When data races occurred, it will retry 200 times at most. if still failed to pop, return
    /// NoMoreBuffer error.
    pub fn pop(&self) -> Result<BufferSlice, Error> {
        let mut old_head = unsafe { (*self.head).load(Ordering::SeqCst) };
        let remain = unsafe { (*self.size).fetch_sub(1, Ordering::SeqCst) };

        if remain <= 1
            || unsafe {
                old_head + BUFFER_HEADER_SIZE + *self.cap_per_buffer > self.buffer_region_len
            }
        {
            unsafe {
                (*self.size).fetch_add(1, Ordering::SeqCst);
            }
            return Err(Error::NoMoreBuffer);
        }

        // when data races occurred, max retry 200 times.
        for _ in 0..200 {
            unsafe {
                let bh = BufferHeader(self.buffer_region.offset(old_head as isize));

                if bh.has_next() {
                    if (*self.head)
                        .compare_exchange(
                            old_head,
                            bh.next_buffer_offset(),
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        let h = BufferHeader(self.buffer_region.offset(old_head as isize));
                        h.clear_flag();
                        h.set_in_used();
                        (*self.counter).fetch_add(1, Ordering::SeqCst);
                        return Ok(BufferSlice::new(
                            Some(h),
                            std::slice::from_raw_parts_mut(
                                self.buffer_region
                                    .offset((old_head + BUFFER_HEADER_SIZE) as isize),
                                *self.cap_per_buffer as usize,
                            ),
                            old_head + self.buffer_region_offset_in_shm,
                            true,
                        ));
                    }
                } else {
                    // don't alloc the last slice
                    if (*self.size).load(Ordering::SeqCst) <= 1 {
                        (*self.size).fetch_add(1, Ordering::SeqCst);
                        return Err(Error::NoMoreBuffer);
                    }
                }
                old_head = (*self.head).load(Ordering::SeqCst);
            }
        }
        unsafe {
            (*self.size).fetch_add(1, Ordering::SeqCst);
        }
        Err(Error::NoMoreBuffer)
    }

    #[allow(unused)]
    pub fn remain(&self) -> isize {
        // when the size is 1, not allow pop for solving problem about concurrent operating.
        unsafe { (*self.size).load(Ordering::SeqCst) as isize - 1 }
    }
}

pub fn count_buffer_list_mem_size(buffer_num: u32, cap_per_buffer: u32) -> u32 {
    BUFFER_LIST_HEADER_SIZE + buffer_num * (cap_per_buffer + BUFFER_HEADER_SIZE)
}

#[cfg(test)]
mod tests {
    use memmap2::MmapOptions;

    use crate::buffer::list::{BufferList, count_buffer_list_mem_size};

    #[test]
    fn test_buffer_list_put_pop() {
        let cap_per_buffer = 4096;
        let buffer_num = 1000;
        let mem = MmapOptions::new()
            .len(count_buffer_list_mem_size(buffer_num, cap_per_buffer) as usize)
            .map_anon()
            .unwrap();

        let l = BufferList::create(buffer_num, cap_per_buffer, &mem, 0).unwrap();

        let mut buffers = Vec::with_capacity(1024);
        let origin_size = l.remain();
        while l.remain() > 0 {
            let b = l.pop().unwrap();
            assert_eq!(cap_per_buffer, b.capacity() as u32);
            assert_eq!(0, b.size());
            assert!(!b.buffer_header.as_ref().unwrap().has_next());
            buffers.push(b);
        }

        for buffer in buffers {
            l.push(buffer);
        }

        buffers = Vec::with_capacity(1024);
        assert_eq!(origin_size, l.remain());
        while l.remain() > 0 {
            let b = l.pop().unwrap();
            assert_eq!(cap_per_buffer, b.capacity() as u32);
            assert_eq!(0, b.size());
            assert!(!b.buffer_header.as_ref().unwrap().has_next());
            buffers.push(b);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_buffer_list_concurrent_put_pop() {
        let cap_per_buffer = 16;
        let buffer_num = 100;
        let mem = MmapOptions::new()
            .len(count_buffer_list_mem_size(buffer_num, cap_per_buffer) as usize)
            .map_anon()
            .unwrap();
        let l = BufferList::create(buffer_num, cap_per_buffer, &mem, 0).unwrap();

        let concurrency = 10;
        let mut join_handle = Vec::new();
        for _ in 0..concurrency {
            join_handle.push(tokio::spawn({
                async move {
                    for _ in 0..10000 {
                        let mut b = l.pop();
                        while b.is_err() {
                            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                            b = l.pop();
                        }
                        let b = b.unwrap();
                        assert_eq!(cap_per_buffer, b.capacity() as u32);
                        assert_eq!(0, b.size());
                        assert!(
                            !b.buffer_header.as_ref().unwrap().has_next(),
                            "offset:{} next:{}",
                            b.offset_in_shm,
                            b.buffer_header.as_ref().unwrap().next_buffer_offset(),
                        );
                        l.push(b);
                    }
                }
            }));
        }
        for handle in join_handle {
            handle.await.unwrap();
        }
        assert_eq!(
            buffer_num,
            unsafe { (*l.size).load(std::sync::atomic::Ordering::SeqCst) } as u32
        );
    }

    #[test]
    fn test_buffer_list_create_and_mapping_free_buffer_list() {
        let cap_per_buffer = 16;
        let buffer_num = 10;
        let mem = MmapOptions::new()
            .len(count_buffer_list_mem_size(buffer_num, cap_per_buffer) as usize)
            .map_anon()
            .unwrap();
        let l = BufferList::create(0, cap_per_buffer, &mem, 0);
        assert!(l.is_err());

        let l = BufferList::create(buffer_num + 1, cap_per_buffer, &mem, 0);
        assert!(l.is_err());

        let _ = BufferList::create(buffer_num, cap_per_buffer, &mem, 0).unwrap();

        let test_mem = std::sync::Arc::new(MmapOptions::new().len(10).map_anon().unwrap());
        let ml = BufferList::mapping(&test_mem, 0);
        assert!(ml.is_err());

        let ml = BufferList::mapping(&mem, 8);
        assert!(ml.is_err());

        let _ = BufferList::mapping(&mem, 0).unwrap();
    }

    #[test]
    fn test_create_free_buffer_list() {
        assert!(
            BufferList::create(
                4294967295,
                4294967295,
                &MmapOptions::new().len(1).map_anon().unwrap(),
                4294967279
            )
            .is_err()
        )
    }
}
