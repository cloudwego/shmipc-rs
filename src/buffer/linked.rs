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
    ptr::NonNull,
    sync::{Arc, Mutex},
};

use bytes::{Bytes, BytesMut};

use super::{BufferReader, BufferWriter, buf::Buf};
use crate::{
    buffer::{
        manager::BufferManager,
        slice::{BufferSlice, SliceList},
    },
    consts::DEFAULT_SINGLE_BUFFER_SIZE,
    error::Error,
};

#[derive(Debug)]
pub struct LinkedBuffer {
    slice_list: SliceList,
    /// LinkedBuffer's `recycle()` will hold this lock
    /// in most scenario(99.999..%), no competition on this mutex.
    /// But when `stream.close()` called, at the meantime,
    /// session receive data and find the stream is under status of close,
    /// which will call LinkedBuffer's `recycle()` causing competition.
    recycle_mux: Mutex<()>,
    buffer_manager: Arc<BufferManager>,
    /// Already read slices dropped by `read_bytes()` will be saved here instead of recycled
    /// instantly. Slices inside will be recycled when `release_previous_read()` is called.
    pinned_list: SliceList,
    /// If `SliceList.front()` is pinned (initialized with false, be turned to true when
    /// `read_bytes()` and `peek()`)
    current_pinned: bool,
    end_stream: bool,
    is_from_shm: bool,
    len: usize,
}

unsafe impl Send for LinkedBuffer {}
unsafe impl Sync for LinkedBuffer {}

impl LinkedBuffer {
    pub fn new(buffer_manager: Arc<BufferManager>) -> Self {
        Self {
            slice_list: SliceList::new(),
            recycle_mux: Mutex::new(()),
            buffer_manager,
            pinned_list: SliceList::new(),
            current_pinned: false,
            end_stream: false,
            is_from_shm: true,
            len: 0,
        }
    }

    pub fn alloc(&mut self, size: u32) {
        let mut remain = size as i64;
        if let Ok(buf) = self.buffer_manager.alloc_shm_buffer(size) {
            self.slice_list.push_back(buf);
            return;
        }
        let alloc_size = self
            .buffer_manager
            .alloc_shm_buffers(&mut self.slice_list, size);
        remain -= alloc_size;
        // fallback. alloc memory buffer (not shm)
        if remain > 0 {
            if remain < DEFAULT_SINGLE_BUFFER_SIZE {
                remain = DEFAULT_SINGLE_BUFFER_SIZE;
            }
            let mut v = vec![0u8; remain as usize];
            self.slice_list
                .push_back(BufferSlice::new(None, v.as_mut_slice(), 0, false));
            self.is_from_shm = false;
            std::mem::forget(v);
        }
    }

    pub fn done(&mut self, end_stream: bool) {
        _ = end_stream;

        if self.is_from_shm {
            let mut slice = self.slice_list.front();
            while let Some(s) = slice {
                s.update();
                if self.slice_list.write().map(|v| v == s).unwrap_or(false) {
                    break;
                }
                slice = s.next();
            }
            // recycle unused slice
            if let Some(write_slice) = self.slice_list.write() {
                if write_slice.next().is_some() {
                    let head = self.slice_list.split_from_write();
                    let mut slice = head;
                    while let Some(s) = slice {
                        let next = unsafe { s.next_slice.map(|s| *Box::from_raw(s.as_ptr())) };
                        self.buffer_manager.recycle_buffer(s);
                        slice = next;
                    }
                }
            }
        }
    }

    pub fn append_buffer_slice(&mut self, slice: BufferSlice) {
        if !slice.is_from_shm {
            self.is_from_shm = false;
        }
        self.len += slice.size();
        self.slice_list.push_back(slice);
        self.slice_list.write_slice = self.slice_list.back_slice;
    }

    pub fn release_previous_read_and_reserve(&mut self) {
        self.clean_pinned_list();
        // try reserve space in long-stream mode for improving performance.
        // we could use read buffer as next write buffer to avoiding share memory allocate and
        // recycle.
        if self.len == 0 && self.slice_list.size() == 1 {
            if self.slice_list.front().unwrap().is_from_shm {
                self.slice_list.front_mut().unwrap().reset();
            } else {
                let slice = self.slice_list.pop_front().unwrap();
                unsafe {
                    _ = Vec::from_raw_parts(slice.data, slice.cap as usize, slice.cap as usize);
                }
            }
        }
    }

    pub fn recycle(&mut self) {
        let _unused = self.recycle_mux.lock().unwrap();
        while let Some(slice) = self.slice_list.pop_front() {
            if slice.is_from_shm {
                self.buffer_manager.recycle_buffer(slice);
            } else {
                unsafe {
                    _ = Vec::from_raw_parts(slice.data, slice.cap as usize, slice.cap as usize);
                }
            }
        }
        self.slice_list.write_slice = None;
        self.is_from_shm = true;
        self.end_stream = false;
        self.current_pinned = false;
        self.len = 0;
    }

    pub fn clean(&mut self) {
        while let Some(slice) = self.slice_list.pop_front() {
            if !slice.is_from_shm {
                unsafe {
                    _ = Vec::from_raw_parts(slice.data, slice.cap as usize, slice.cap as usize);
                }
            }
        }
        self.slice_list.write_slice = None;
        self.is_from_shm = true;
        self.end_stream = false;
        self.current_pinned = false;
        self.len = 0;
    }

    pub fn root_buf_offset(&self) -> u32 {
        self.slice_list
            .front()
            .map(|v| v.offset_in_shm)
            .unwrap_or(0)
    }

    pub fn cap(&self) -> usize {
        let mut sum = 0;
        let mut e = self.slice_list.front();
        while let Some(s) = e {
            sum += s.capacity();
            e = s.next();
        }
        sum
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn len_mut(&mut self) -> &mut usize {
        &mut self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn is_from_share_memory(&self) -> bool {
        self.is_from_shm
    }

    #[inline]
    pub fn slice_list(&self) -> &SliceList {
        &self.slice_list
    }

    #[inline]
    pub fn slice_list_mut(&mut self) -> &mut SliceList {
        &mut self.slice_list
    }

    fn read_next_slice(&mut self) {
        if let Some(slice) = self.slice_list.pop_front() {
            if slice.is_from_shm {
                if self.current_pinned {
                    self.pinned_list.push_back(slice);
                } else {
                    self.buffer_manager.recycle_buffer(slice);
                }
            }
        }
        self.current_pinned = false;
    }

    fn clean_pinned_list(&mut self) {
        if self.pinned_list.size() == 0 {
            return;
        }
        self.current_pinned = false;
        while self.pinned_list.size() > 0 {
            if let Some(slice) = self.pinned_list.pop_front() {
                if slice.is_from_shm {
                    self.buffer_manager.recycle_buffer(slice);
                } else {
                    unsafe {
                        _ = Vec::from_raw_parts(slice.data, slice.cap as usize, slice.cap as usize);
                    }
                }
            }
        }
    }
}

impl BufferReader for LinkedBuffer {
    fn read_bytes(&mut self, mut size: usize) -> Result<Buf<'_>, Error> {
        if size == 0 {
            return Ok(Buf::Exm(Bytes::new()));
        }
        if self.len < size {
            return Err(Error::NotEnoughData);
        }

        if self
            .slice_list
            .front()
            .map(|v| v.size() == 0)
            .unwrap_or_default()
        {
            self.read_next_slice();
        }

        if let Some(slice) = self.slice_list.front_mut() {
            if slice.size() >= size {
                self.current_pinned = true;
                self.len -= size;
                // A workaround to avoid https://github.com/rust-lang/rust/issues/54663
                let bytes = self.slice_list.front_mut().unwrap().read(size);
                return Ok(Buf::Shm(bytes));
            }
        }
        // slow path
        self.len -= size;
        let mut result = BytesMut::with_capacity(size);

        while size > 0 {
            if let Some(slice) = self.slice_list.front_mut() {
                let read_data = slice.read(size);
                result.extend_from_slice(read_data);
                let read_size = read_data.len();
                if read_size != size {
                    self.read_next_slice();
                }
                size -= read_size;
            }
        }
        Ok(Buf::Exm(result.freeze()))
    }

    fn peek(&mut self, mut size: usize) -> Result<Buf<'_>, Error> {
        if size == 0 {
            return Ok(Buf::Exm(Bytes::new()));
        }
        if self.len < size {
            return Err(Error::NotEnoughData);
        }

        if let Some(slice) = self.slice_list.front_mut() {
            let read_bytes = slice.peek(size);
            if read_bytes.len() == size {
                self.current_pinned = true;
                return Ok(Buf::Shm(read_bytes));
            }
        }

        // slow path
        let mut result = BytesMut::with_capacity(size);
        let mut e = self.slice_list.front_mut();
        while size > 0 && e.is_some() {
            let bs = unsafe { e.unwrap_unchecked() };
            let read_bytes = bs.peek(size);
            result.extend_from_slice(read_bytes);
            size -= read_bytes.len();
            e = bs.next_mut();
        }

        Ok(Buf::Exm(result.freeze()))
    }

    fn discard(&mut self, mut size: usize) -> Result<usize, Error> {
        if self.len < size {
            return Err(Error::NotEnoughData);
        }
        let mut n = 0;
        loop {
            if let Some(slice) = self.slice_list.front_mut() {
                let skip = slice.skip(size);
                n += skip;
                size -= skip;
                if size == 0 {
                    break;
                }
                self.read_next_slice();
            }
        }
        self.len -= n;
        Ok(n)
    }

    fn release_previous_read(&mut self) {
        self.clean_pinned_list();

        if self.slice_list.size() == 0 {
            return;
        }

        if self.slice_list.front().unwrap().size() == 0
            && self.slice_list.front_slice == self.slice_list.write_slice
        {
            self.buffer_manager
                .recycle_buffer(self.slice_list.pop_front().unwrap());
            self.slice_list.write_slice = None;
        }
    }
}

impl BufferWriter for LinkedBuffer {
    /// 1. if cur slice can contain the size, then reserve and return it.
    /// 2. if the next slice can contain the size, then reserve and return it.
    /// 3. alloc a new slice which can contain the size.
    fn reserve(&mut self, size: usize) -> Result<&mut [u8], Error> {
        // 1. use current slice
        if self.slice_list.write_slice.is_none() {
            self.alloc(size as u32);
            self.slice_list.write_slice = self.slice_list.front_slice;
        }
        let mut write_slice = self.slice_list.write_slice.unwrap();
        if let Ok(ret) = unsafe { write_slice.as_mut().reserve(size) } {
            self.len += size;
            return Ok(ret);
        }

        // 2. use next slice
        if let Some(e) = unsafe { write_slice.as_ref().next_mut() } {
            let ptr = e as *mut BufferSlice;
            if let Ok(ret) = e.reserve(size) {
                unsafe {
                    self.slice_list.write_slice = Some(NonNull::new_unchecked(ptr));
                }
                self.len += size;
                return Ok(ret);
            }
        }

        // 3. alloc a new slice
        if let Ok(buf) = self.buffer_manager.alloc_shm_buffer(size as u32) {
            self.slice_list.push_back(buf);
        } else {
            // fallback
            let mut alloc_size = size;
            if alloc_size < DEFAULT_SINGLE_BUFFER_SIZE as usize {
                alloc_size = DEFAULT_SINGLE_BUFFER_SIZE as usize;
            }
            let mut v = vec![0u8; alloc_size];
            self.slice_list
                .push_back(BufferSlice::new(None, v.as_mut_slice(), 0, false));
            self.is_from_shm = false;
            std::mem::forget(v);
        }
        self.slice_list.write_slice = self.slice_list.back_slice;
        self.len += size;
        self.slice_list.write_mut().unwrap().reserve(size)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.slice_list.write_slice.is_none() {
            self.alloc(bytes.len() as u32);
            self.slice_list.write_slice = self.slice_list.front_slice;
        }
        let mut n = 0;
        loop {
            n += self.slice_list.write_mut().unwrap().append(&bytes[n..]);
            if n < bytes.len() {
                // l.sliceList.write slice must be used out
                if self.slice_list.write().unwrap().next().is_none() {
                    // which means no allocated bufferSlice is left
                    self.alloc((bytes.len() - n) as u32);
                }
                self.slice_list.write_slice = self.slice_list.write().unwrap().next_slice;
            } else {
                // n equals bytes.len()
                break;
            }
        }
        self.len += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use memmap2::MmapOptions;
    use rand::Rng;

    use super::{BufferReader, LinkedBuffer};
    use crate::{
        buffer::{BufferWriter, manager::BufferManager, slice::BufferSlice},
        config::SizePercentPair,
        consts::DEFAULT_SINGLE_BUFFER_SIZE,
    };

    fn init_shm() -> BufferManager {
        let shm_path = "/tmp/ipc.test";
        _ = std::fs::remove_dir("shm_path");
        let shm_size = 10 << 20;
        let mem = MmapOptions::new().len(shm_size).map_anon().unwrap();
        BufferManager::create(
            &[
                SizePercentPair {
                    size: 4096,
                    percent: 70,
                },
                SizePercentPair {
                    size: 16 * 1024,
                    percent: 20,
                },
                SizePercentPair {
                    size: 64 * 1024,
                    percent: 10,
                },
            ],
            shm_path,
            mem,
            0,
        )
        .unwrap()
    }

    fn new_linked_buffer_with_slice(
        manager: Arc<BufferManager>,
        slice: BufferSlice,
    ) -> LinkedBuffer {
        let mut l = LinkedBuffer::new(manager);
        l.slice_list.push_back(slice);
        l.slice_list.write_slice = l.slice_list.back_slice;
        l
    }

    #[test]
    fn test_linked_buffer_release_previous_read() {
        let bm = Arc::new(init_shm());
        let slice = bm.alloc_shm_buffer(1024).unwrap();
        let mut buf = new_linked_buffer_with_slice(bm, slice);
        let slice_num = 100;
        for i in 0..slice_num * 4096 {
            assert_eq!(1, buf.write_bytes(&[i as u8]).unwrap());
        }
        buf.done(true);

        for _ in 0..slice_num / 2 {
            let r = buf.read_bytes(4096).unwrap();
            assert_eq!(4096, r.len());
        }
        assert_eq!(slice_num / 2 - 1, buf.pinned_list.size());
        _ = buf.discard(buf.len());

        buf.release_previous_read_and_reserve();
        assert_eq!(0, buf.pinned_list.size());
        assert_eq!(0, buf.len());
        // the last slice shouldn't release
        assert_eq!(1, buf.slice_list.size());
        assert!(buf.slice_list.write_slice.is_some());

        buf.release_previous_read();
        assert_eq!(0, buf.slice_list.size());
        assert!(buf.slice_list.write_slice.is_none());
    }

    #[test]
    fn test_linked_buffer_fallback_when_write() {
        let mem = MmapOptions::new().len(10 * 1024).map_anon().unwrap();
        let bm = Arc::new(
            BufferManager::create(
                &[SizePercentPair {
                    size: 1024,
                    percent: 100,
                }],
                "",
                mem,
                0,
            )
            .unwrap(),
        );

        let buf = bm.alloc_shm_buffer(1024).unwrap();
        let mut writer = new_linked_buffer_with_slice(bm.clone(), buf);
        let data_size = 1024;
        let mut mock_data_array = vec![vec![0u8; data_size]; 100];
        for (i, array) in mock_data_array.iter_mut().enumerate() {
            rand::rng().fill(&mut array[..]);
            let n = writer.write_bytes(&array[..]).unwrap();
            assert_eq!(data_size, n);
            assert_eq!(data_size * (i + 1), writer.len());
        }
        assert!(!writer.is_from_shm);

        writer.done(false);
        let all = data_size * mock_data_array.len();
        assert_eq!(all, writer.len());

        for (i, array) in mock_data_array.into_iter().enumerate() {
            assert_eq!(all - i * data_size, writer.len());
            let get = writer.read_bytes(data_size).unwrap();
            assert_eq!(array, get);
        }
    }

    #[test]
    fn test_linked_buffer_reserve() {
        let bm = Arc::new(init_shm());

        // alloc 3 buffer slice
        let mut buffer = new_linked_buffer(bm.clone(), (64 + 64 + 64) * 1024);
        assert_eq!(3, buffer.slice_list.size());
        assert!(buffer.is_from_shm);
        assert_eq!(buffer.slice_list.front(), buffer.slice_list.write());

        // reserve a buf in first slice
        let ret = buffer.reserve(60 * 1024).unwrap();
        assert_eq!(60 * 1024, ret.len());
        assert_eq!(3, buffer.slice_list.size());
        assert!(buffer.is_from_shm);
        assert_eq!(buffer.slice_list.front(), buffer.slice_list.write());

        // reserve a buf in the second slice when the first one is not enough
        let ret = buffer.reserve(6 * 1024).unwrap();
        assert_eq!(6 * 1024, ret.len());
        assert_eq!(3, buffer.slice_list.size());
        assert!(buffer.is_from_shm);
        assert_eq!(
            buffer.slice_list.front().unwrap().next(),
            buffer.slice_list.write()
        );

        // reserve a buf in a new allocated slice
        let ret = buffer.reserve(128 * 1024).unwrap();
        assert_eq!(128 * 1024, ret.len());
        assert_eq!(4, buffer.slice_list.size());
        assert!(!buffer.is_from_shm);
        assert_eq!(buffer.slice_list.back(), buffer.slice_list.write());
    }

    #[test]
    fn test_linked_buffer_done() {
        let bm = Arc::new(init_shm());
        let mock_data_size = 128 * 1024;
        let mut mock_data = vec![0u8; mock_data_size];
        rand::rng().fill(&mut mock_data[..]);
        // alloc 3 buffer slice
        let mut buffer = new_linked_buffer(bm.clone(), (64 + 64 + 64) * 1024);
        assert_eq!(3, buffer.slice_list.size());

        // write data to full 2 slice, remove one
        buffer.write_bytes(&mock_data[..]).unwrap();
        buffer.done(true);
        assert_eq!(2, buffer.slice_list.size());
        let get_bytes = buffer.read_bytes(mock_data_size).unwrap();
        assert_eq!(mock_data, get_bytes);
    }

    fn new_linked_buffer(manager: Arc<BufferManager>, size: u32) -> LinkedBuffer {
        let mut l = LinkedBuffer::new(manager);
        l.alloc(size);
        l.slice_list.write_slice = l.slice_list.front_slice;
        l
    }

    #[test]
    fn test_linked_buffer_read_bytes() {
        let manager = Arc::new(init_shm());

        let create_buffer_writer = || {
            let buf = manager.alloc_shm_buffer(1024).unwrap();
            new_linked_buffer_with_slice(manager, buf)
        };

        let write_and_read = |mut buf: LinkedBuffer| {
            let size = 1 << 21;
            let data = vec![0u8; size];
            while buf.len() < size {
                let mut one_write_size = rand::rng().random_range(0..size / 10);
                if buf.len() + one_write_size > size {
                    one_write_size = size - buf.len();
                }
                let n = buf
                    .write_bytes(&data[buf.len()..buf.len() + one_write_size])
                    .unwrap();
                assert_eq!(one_write_size, n);
            }
            buf.done(false);
            let mut read = 0;
            while !buf.is_empty() {
                let mut one_read_size = rand::rng().random_range(0..size / 10000);
                if read + one_read_size > buf.len() {
                    one_read_size = buf.len();
                }
                // do nothing
                _ = buf.peek(one_read_size);

                let read_data = buf.read_bytes(one_read_size).unwrap();
                if read_data.is_empty() {
                    assert_eq!(one_read_size, 0);
                } else {
                    assert_eq!(&data[read..read + one_read_size], &read_data[..]);
                }
                read += one_read_size;
            }
            assert_eq!(1 << 21, read);
            buf.read_bytes(0).unwrap();
            buf.release_previous_read();
        };

        for _ in 0..100 {
            write_and_read((create_buffer_writer.clone())());
        }
    }

    #[test]
    fn test_buffer_discard() {
        let manager = Arc::new(init_shm());

        let create_buffer_writer = || {
            let buf = manager.alloc_shm_buffer(1024).unwrap();
            new_linked_buffer_with_slice(manager, buf)
        };

        let mut writer = (create_buffer_writer.clone())();
        let capacity = writer.cap();
        writer.write_bytes(&vec![0u8; capacity]).unwrap();
        let n = writer.discard(capacity).unwrap();
        assert_eq!(capacity, n);
        assert_eq!(0, writer.len());

        let mut writer = create_buffer_writer();
        let origin_cap = writer.cap();
        writer.write_bytes(&vec![0u8; origin_cap]).unwrap();
        writer.write_bytes(&vec![0u8; 1024]).unwrap();

        let n = writer.discard(origin_cap).unwrap();
        assert_eq!(origin_cap, n);

        let n = writer.discard(1024).unwrap();
        assert_eq!(1024, n);
    }

    #[test]
    fn test_buffer_read_write() {
        let manager = Arc::new(init_shm());

        let create_buffer_writer = || {
            let buf = manager.alloc_shm_buffer(1024).unwrap();
            new_linked_buffer_with_slice(manager, buf)
        };

        let str = "hello";
        let mut writer = (create_buffer_writer.clone())();
        writer.write_bytes(str.as_bytes()).unwrap();
        writer.write_bytes(str.as_bytes()).unwrap();

        writer.done(false);

        let get_str = writer.read_bytes(str.len()).unwrap();
        assert_eq!(str, std::str::from_utf8(&get_str).unwrap());

        let get_bytes = writer.read_bytes(str.len()).unwrap();
        assert_eq!(str.as_bytes(), &get_bytes[..]);

        let mut writer = (create_buffer_writer.clone())();

        const ONE_MSG_SIZE: usize = 1024;
        const MSG_NUM: usize = 10;
        let mut result = vec![0u8; ONE_MSG_SIZE * MSG_NUM];
        for i in 0..MSG_NUM {
            let mut data = [0u8; ONE_MSG_SIZE];
            rand::rng().fill(&mut data[..]);
            result[i * ONE_MSG_SIZE..(i + 1) * ONE_MSG_SIZE].copy_from_slice(&data[..]);
            let n = writer.write_bytes(&data[..]).unwrap();
            assert_eq!(ONE_MSG_SIZE, n);
        }
        assert_eq!(ONE_MSG_SIZE * MSG_NUM, writer.len());

        writer.done(false);
        assert_eq!(ONE_MSG_SIZE * MSG_NUM, writer.len());

        let peek1 = writer.peek(ONE_MSG_SIZE).unwrap();
        assert_eq!(ONE_MSG_SIZE, peek1.len());
        assert_eq!(&result[..ONE_MSG_SIZE], &peek1[..]);
        assert_eq!(ONE_MSG_SIZE * MSG_NUM, writer.len());

        // cross two underlying slice
        let peek2 = writer.peek(5 * ONE_MSG_SIZE).unwrap();
        assert_eq!(5 * ONE_MSG_SIZE, peek2.len());
        assert_eq!(&result[..5 * ONE_MSG_SIZE], &peek2[..]);
        assert_eq!(MSG_NUM * ONE_MSG_SIZE, writer.len());

        let mut remain = writer.len();
        for _ in 0..MSG_NUM {
            remain -= ONE_MSG_SIZE;
            let get_data = writer.read_bytes(1024).unwrap();
            assert_eq!(ONE_MSG_SIZE, get_data.len());
            assert_eq!(remain, writer.len());
        }

        let mut writer = (create_buffer_writer.clone())();
        for i in 0..2 * DEFAULT_SINGLE_BUFFER_SIZE {
            writer.write_bytes(&[i as u8]).unwrap();
        }

        writer.done(false);
        let mut count = 0;
        let read_size = 10;
        loop {
            let remain_len = writer.len();
            if remain_len > read_size {
                let r = writer.read_bytes(read_size).unwrap();
                for j in 0..r.len() {
                    assert_eq!(count as u8, r[j]);
                    count += 1;
                }
            } else if remain_len > 0 {
                let r = writer.read_bytes(writer.len()).unwrap();
                for j in 0..r.len() {
                    assert_eq!(count as u8, r[j]);
                    count += 1;
                }
            } else {
                break;
            }
        }
        assert_eq!(2 * DEFAULT_SINGLE_BUFFER_SIZE, count);
    }
}
