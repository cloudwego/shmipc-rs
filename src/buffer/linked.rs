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
use tokio::io::ReadBuf;

use super::{
    BufferReader, BufferWriter,
    buf::{Buf, ShmBuf},
};
use crate::{
    buffer::{
        manager::BufferManager,
        slice::{BufferSlice, SliceList},
    },
    consts::DEFAULT_SINGLE_BUFFER_SIZE,
    error::Error,
};

#[derive(Debug)]
struct PinRegistry {
    buffer_manager: Arc<BufferManager>,
    /// Accessed only while the owning `LinkedBuffer` holds its `recycle_mux`.
    slots: Vec<PinSlot>,
}

#[derive(Debug)]
struct PinSlot {
    key: usize,
    state: Arc<SlicePinState>,
}

impl PinRegistry {
    fn new(buffer_manager: Arc<BufferManager>) -> Self {
        Self {
            buffer_manager,
            slots: Vec::new(),
        }
    }

    fn acquire(&mut self, slice: &BufferSlice) -> PinLease {
        let key = slice.data as usize;
        let state = if let Some(slot) = self.slots.iter().find(|slot| slot.key == key) {
            slot.state.clone()
        } else {
            let state = Arc::new(SlicePinState::new(self.buffer_manager.clone()));
            self.slots.push(PinSlot {
                key,
                state: state.clone(),
            });
            state
        };
        PinLease { _state: state }
    }

    fn is_pinned(&self, slice: &BufferSlice) -> bool {
        self.slots
            .iter()
            .find(|slot| slot.key == slice.data as usize)
            .is_some_and(|slot| Arc::strong_count(&slot.state) != 1)
    }

    fn retire(&mut self, slice: BufferSlice) {
        let key = slice.data as usize;
        let state = self
            .slots
            .iter()
            .position(|slot| slot.key == key)
            .map(|idx| self.slots.swap_remove(idx).state);
        if let Some(state) = state {
            state.retire(slice);
        } else {
            reclaim_slice(&self.buffer_manager, slice);
        }
    }

    fn abandon(&mut self, slice: &BufferSlice) {
        let key = slice.data as usize;
        let state = self
            .slots
            .iter()
            .position(|slot| slot.key == key)
            .map(|idx| self.slots.swap_remove(idx).state);
        if let Some(state) = state {
            debug_assert_eq!(Arc::strong_count(&state), 1);
            debug_assert!(state.retired_slice.lock().unwrap().is_none());
        }
    }
}

#[derive(Debug)]
struct SlicePinState {
    buffer_manager: Arc<BufferManager>,
    retired_slice: Mutex<Option<BufferSlice>>,
}

impl SlicePinState {
    fn new(buffer_manager: Arc<BufferManager>) -> Self {
        Self {
            buffer_manager,
            retired_slice: Mutex::new(None),
        }
    }

    fn retire(self: Arc<Self>, slice: BufferSlice) {
        if Arc::strong_count(&self) == 1 {
            reclaim_slice(&self.buffer_manager, slice);
            return;
        }
        let previous = self.retired_slice.lock().unwrap().replace(slice);
        debug_assert!(previous.is_none());
    }
}

impl Drop for SlicePinState {
    fn drop(&mut self) {
        if let Some(slice) = self.retired_slice.get_mut().unwrap().take() {
            reclaim_slice(&self.buffer_manager, slice);
        }
    }
}

fn reclaim_slice(buffer_manager: &BufferManager, slice: BufferSlice) {
    if slice.is_from_shm {
        buffer_manager.recycle_buffer(slice);
    } else {
        unsafe {
            _ = Vec::from_raw_parts(slice.data, slice.cap as usize, slice.cap as usize);
        }
    }
}

pub(crate) struct PinLease {
    _state: Arc<SlicePinState>,
}

#[derive(Debug)]
pub struct LinkedBuffer {
    slice_list: SliceList,
    /// Serializes zero-copy lease acquisition and slice retirement with a concurrent stream close.
    recycle_mux: Arc<Mutex<()>>,
    buffer_manager: Arc<BufferManager>,
    /// Tracks zero-copy readers and owns retired slices until the final reader is dropped.
    pin_registry: PinRegistry,
    end_stream: bool,
    is_from_shm: bool,
    len: usize,
}

unsafe impl Send for LinkedBuffer {}
unsafe impl Sync for LinkedBuffer {}

impl LinkedBuffer {
    pub fn new(buffer_manager: Arc<BufferManager>) -> Self {
        let pin_registry = PinRegistry::new(buffer_manager.clone());
        Self {
            slice_list: SliceList::new(),
            recycle_mux: Arc::new(Mutex::new(())),
            buffer_manager,
            pin_registry,
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
            let unused_head = if self.slice_list.write().is_some_and(|s| s.next().is_some()) {
                self.slice_list.split_from_write()
            } else {
                None
            };

            let mut slice = self.slice_list.front();
            while let Some(s) = slice {
                s.update();
                if self.slice_list.write().map(|v| v == s).unwrap_or(false) {
                    break;
                }
                slice = s.next();
            }

            // Recycle slices that were allocated speculatively but never written.
            let mut slice = unused_head;
            while let Some(s) = slice {
                let next = unsafe { s.next_slice.map(|s| *Box::from_raw(s.as_ptr())) };
                self.buffer_manager.recycle_buffer(s);
                slice = next;
            }
        }
    }

    pub fn append_buffer_slice(&mut self, slice: BufferSlice) {
        let recycle_mux = self.recycle_mux.clone();
        let _unused = recycle_mux.lock().unwrap();
        if !slice.is_from_shm {
            self.is_from_shm = false;
        }
        self.len += slice.size();
        self.slice_list.push_back(slice);
        self.slice_list.write_slice = self.slice_list.back_slice;
    }

    pub fn release_previous_read_and_reserve(&mut self) {
        let recycle_mux = self.recycle_mux.clone();
        let _unused = recycle_mux.lock().unwrap();
        // try reserve space in long-stream mode for improving performance.
        // we could use read buffer as next write buffer to avoiding share memory allocate and
        // recycle.
        if self.len == 0 && self.slice_list.size() == 1 {
            if self
                .slice_list
                .front()
                .is_some_and(|slice| self.pin_registry.is_pinned(slice))
            {
                let slice = self.slice_list.pop_front().unwrap();
                self.slice_list.write_slice = None;
                self.pin_registry.retire(slice);
                return;
            }
            if self.slice_list.front().unwrap().is_from_shm {
                self.slice_list.front_mut().unwrap().reset_for_reuse();
            } else {
                let slice = self.slice_list.pop_front().unwrap();
                self.pin_registry.retire(slice);
            }
        }
    }

    pub fn recycle(&mut self) {
        let recycle_mux = self.recycle_mux.clone();
        let _unused = recycle_mux.lock().unwrap();
        while let Some(slice) = self.slice_list.pop_front() {
            self.pin_registry.retire(slice);
        }
        self.slice_list.write_slice = None;
        self.is_from_shm = true;
        self.end_stream = false;
        self.len = 0;
    }

    pub fn clean(&mut self) {
        let recycle_mux = self.recycle_mux.clone();
        let _unused = recycle_mux.lock().unwrap();
        while let Some(slice) = self.slice_list.pop_front() {
            if self.pin_registry.is_pinned(&slice) {
                self.pin_registry.retire(slice);
            } else {
                self.pin_registry.abandon(&slice);
                if !slice.is_from_shm {
                    unsafe {
                        _ = Vec::from_raw_parts(slice.data, slice.cap as usize, slice.cap as usize);
                    }
                }
            }
        }
        self.slice_list.write_slice = None;
        self.is_from_shm = true;
        self.end_stream = false;
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
    pub const fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub const fn len_mut(&mut self) -> &mut usize {
        &mut self.len
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub const fn is_from_share_memory(&self) -> bool {
        self.is_from_shm
    }

    #[inline]
    pub const fn slice_list(&self) -> &SliceList {
        &self.slice_list
    }

    #[inline]
    pub const fn slice_list_mut(&mut self) -> &mut SliceList {
        &mut self.slice_list
    }

    fn read_next_slice(&mut self) {
        if let Some(slice) = self.slice_list.pop_front() {
            self.pin_registry.retire(slice);
        }
    }

    pub(crate) fn read_available_into(
        &mut self,
        dst: &mut ReadBuf<'_>,
        max_slices: usize,
    ) -> Result<usize, Error> {
        if dst.remaining() == 0 {
            return Ok(0);
        }
        assert!(max_slices > 0, "max_slices must be greater than zero");

        let recycle_mux = self.recycle_mux.clone();
        let _unused = recycle_mux.lock().unwrap();
        if self.len == 0 {
            return Err(Error::NotEnoughData);
        }

        while self.len > 0
            && self
                .slice_list
                .front()
                .is_some_and(|slice| slice.size() == 0)
        {
            self.read_next_slice();
        }

        let mut copied = 0;
        let mut slices_read = 0;
        while self.len > 0 && dst.remaining() > 0 && slices_read < max_slices {
            let available = self
                .slice_list
                .front()
                .map(BufferSlice::size)
                .filter(|size| *size > 0)
                .ok_or(Error::NotEnoughData)?;
            let read_size = available.min(dst.remaining());
            let data = self.slice_list.front_mut().unwrap().read(read_size);
            dst.put_slice(data);
            self.len -= read_size;
            copied += read_size;

            if read_size == available {
                slices_read += 1;
                if self.len > 0 {
                    self.read_next_slice();
                }
            } else {
                break;
            }
        }

        debug_assert!(copied > 0);
        Ok(copied)
    }
}

impl BufferReader for LinkedBuffer {
    fn read_chunk(&mut self) -> Result<Buf<'_>, Error> {
        let recycle_mux = self.recycle_mux.clone();
        let _unused = recycle_mux.lock().unwrap();

        while self
            .slice_list
            .front()
            .is_some_and(|slice| slice.size() == 0)
        {
            self.read_next_slice();
        }

        let size = self
            .slice_list
            .front()
            .map(BufferSlice::size)
            .filter(|size| *size > 0)
            .ok_or(Error::NotEnoughData)?;
        if self.len < size {
            return Err(Error::NotEnoughData);
        }
        let lease = self.pin_registry.acquire(self.slice_list.front().unwrap());
        self.len -= size;
        let bytes = self.slice_list.front_mut().unwrap().read(size);
        Ok(Buf::Shm(ShmBuf::new(bytes, lease)))
    }

    fn read_exact_bytes(&mut self, mut size: usize) -> Result<Buf<'_>, Error> {
        if size == 0 {
            return Ok(Buf::Exm(Bytes::new()));
        }
        let recycle_mux = self.recycle_mux.clone();
        let _unused = recycle_mux.lock().unwrap();
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

        if let Some(slice) = self.slice_list.front_mut()
            && slice.size() >= size
        {
            let lease = self.pin_registry.acquire(slice);
            self.len -= size;
            // A workaround to avoid https://github.com/rust-lang/rust/issues/54663
            let bytes = self.slice_list.front_mut().unwrap().read(size);
            return Ok(Buf::Shm(ShmBuf::new(bytes, lease)));
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
        let recycle_mux = self.recycle_mux.clone();
        let _unused = recycle_mux.lock().unwrap();
        if self.len < size {
            return Err(Error::NotEnoughData);
        }

        if let Some(slice) = self.slice_list.front_mut() {
            let lease = self.pin_registry.acquire(slice);
            let read_bytes = slice.peek(size);
            if read_bytes.len() == size {
                return Ok(Buf::Shm(ShmBuf::new(read_bytes, lease)));
            }
            drop(lease);
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
        let recycle_mux = self.recycle_mux.clone();
        let _unused = recycle_mux.lock().unwrap();
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
        let recycle_mux = self.recycle_mux.clone();
        let _unused = recycle_mux.lock().unwrap();

        if self.slice_list.size() == 0 {
            return;
        }

        if self.slice_list.front().unwrap().size() == 0
            && self.slice_list.front_slice == self.slice_list.write_slice
        {
            let slice = self.slice_list.pop_front().unwrap();
            self.pin_registry.retire(slice);
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
    use std::{ptr::NonNull, sync::Arc};

    use memmap2::MmapOptions;
    use rand::Rng;
    use tokio::io::ReadBuf;

    use super::{BufferReader, LinkedBuffer};
    use crate::{
        buffer::{BufferWriter, manager::BufferManager, slice::BufferSlice},
        config::SizePercentPair,
        consts::DEFAULT_SINGLE_BUFFER_SIZE,
        error::Error,
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

    fn fallback_slice(data: &[u8]) -> BufferSlice {
        let mut owned = Vec::with_capacity(data.len());
        owned.extend_from_slice(data);
        let slice = BufferSlice {
            buffer_header: None,
            data: owned.as_mut_ptr(),
            cap: owned.capacity() as u32,
            start: 0,
            offset_in_shm: 0,
            read_index: 0,
            write_index: owned.len(),
            is_from_shm: false,
            next_slice: None::<NonNull<BufferSlice>>,
        };
        std::mem::forget(owned);
        slice
    }

    fn append_fallback(buffer: &mut LinkedBuffer, data: &[u8]) {
        buffer.append_buffer_slice(fallback_slice(data));
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
            let r = buf.read_exact_bytes(4096).unwrap();
            assert_eq!(4096, r.len());
        }
        {
            let slots = &buf.pin_registry.slots;
            let state = &slots.first().unwrap().state;
            assert_eq!(Arc::strong_count(state), 1);
            assert!(state.retired_slice.lock().unwrap().is_none());
        }
        _ = buf.discard(buf.len());

        buf.release_previous_read_and_reserve();
        assert_eq!(0, buf.len());
        // the last slice shouldn't release
        assert_eq!(1, buf.slice_list.size());
        assert!(buf.slice_list.write_slice.is_some());
        assert!(
            buf.slice_list
                .front()
                .unwrap()
                .buffer_header
                .as_ref()
                .unwrap()
                .is_in_used()
        );

        buf.release_previous_read();
        assert_eq!(0, buf.slice_list.size());
        assert!(buf.slice_list.write_slice.is_none());
    }

    #[test]
    fn recycle_waits_for_last_zero_copy_lease() {
        let manager = Arc::new(init_shm());
        let slice = manager.alloc_shm_buffer(1024).unwrap();
        let mut buffer = new_linked_buffer_with_slice(manager.clone(), slice);
        let data = vec![7u8; 1024];
        buffer.write_bytes(&data).unwrap();
        buffer.done(false);

        let leased = buffer.read_exact_bytes(data.len()).unwrap().into_bytes();
        let leased_clone = leased.clone();
        buffer.recycle();

        assert!(!manager.check_buffer_returned());
        assert_eq!(&leased[..], data.as_slice());
        drop(leased);
        assert!(!manager.check_buffer_returned());
        assert_eq!(&leased_clone[..], data.as_slice());

        drop(leased_clone);
        assert!(manager.check_buffer_returned());
    }

    #[test]
    fn active_lease_does_not_delay_unrelated_slices() {
        let manager = Arc::new(init_shm());
        let first = manager.alloc_shm_buffer(1024).unwrap();
        let first_capacity = first.capacity();
        let mut buffer = new_linked_buffer_with_slice(manager.clone(), first);
        buffer
            .write_bytes(&vec![9u8; first_capacity + 1024])
            .unwrap();
        buffer.done(false);
        let remaining_after_write = manager.remain_size();

        let leased = buffer
            .read_exact_bytes(first_capacity)
            .unwrap()
            .into_bytes();
        buffer.discard(buffer.len()).unwrap();
        buffer.release_previous_read();

        assert!(buffer.pin_registry.slots.is_empty());
        assert!(manager.remain_size() > remaining_after_write);
        assert!(!manager.check_buffer_returned());

        drop(leased);
        assert!(manager.check_buffer_returned());
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

        let expected = mock_data_array.concat();
        let mut actual = Vec::with_capacity(all);
        while !writer.is_empty() {
            let chunk = writer.read_chunk().unwrap();
            assert!(!chunk.is_empty());
            actual.extend_from_slice(&chunk);
        }
        assert_eq!(actual, expected);
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
        let get_bytes = buffer.read_exact_bytes(mock_data_size).unwrap();
        assert_eq!(mock_data, get_bytes);
    }

    #[test]
    fn test_linked_buffer_done_clears_recycled_tail_link() {
        let bm = Arc::new(init_shm());

        let mut buffer = new_linked_buffer(bm, (64 + 64 + 64) * 1024);
        let mock_data = vec![0u8; 128 * 1024];

        buffer.write_bytes(&mock_data).unwrap();
        buffer.done(true);

        assert_eq!(2, buffer.slice_list.size());
        let last_valid = buffer.slice_list.write().unwrap();
        assert!(last_valid.next().is_none());
        assert!(!last_valid.buffer_header.as_ref().unwrap().has_next());
    }

    fn new_linked_buffer(manager: Arc<BufferManager>, size: u32) -> LinkedBuffer {
        let mut l = LinkedBuffer::new(manager);
        l.alloc(size);
        l.slice_list.write_slice = l.slice_list.front_slice;
        l
    }

    #[test]
    fn read_chunk_returns_one_non_empty_slice() {
        let manager = Arc::new(init_shm());
        let first = manager.alloc_shm_buffer(1024).unwrap();
        let first_capacity = first.capacity();
        let mut buffer = new_linked_buffer_with_slice(manager, first);

        let first_data = vec![1u8; first_capacity];
        let second_data = vec![2u8; 128];
        buffer.write_bytes(&first_data).unwrap();
        buffer.write_bytes(&second_data).unwrap();
        buffer.done(false);

        let first_chunk = buffer.read_chunk().unwrap();
        assert_eq!(&first_chunk[..], first_data);
        drop(first_chunk);

        // The exhausted first slice remains at the front until the next read. `read_chunk` must
        // retire it and return the next non-empty slice instead of reporting an empty read.
        let second_chunk = buffer.read_chunk().unwrap();
        assert_eq!(&second_chunk[..], second_data);
        drop(second_chunk);

        assert!(buffer.is_empty());
        assert!(matches!(buffer.read_chunk(), Err(Error::NotEnoughData)));
        assert_eq!(buffer.slice_list.size(), 0);
        assert!(buffer.slice_list.write_slice.is_none());

        let rewritten = [3u8; 32];
        buffer.write_bytes(&rewritten).unwrap();
        let chunk = buffer.read_chunk().unwrap();
        assert_eq!(&chunk[..], rewritten);
    }

    #[test]
    fn read_available_into_copies_across_slices_without_leases() {
        let manager = Arc::new(init_shm());
        let mut buffer = LinkedBuffer::new(manager);
        append_fallback(&mut buffer, b"ab");
        append_fallback(&mut buffer, b"cde");
        append_fallback(&mut buffer, b"fghi");

        let mut first = [0; 4];
        let mut dst = ReadBuf::new(&mut first);
        assert_eq!(buffer.read_available_into(&mut dst, 64).unwrap(), 4);
        assert_eq!(dst.filled(), b"abcd");
        assert_eq!(buffer.len(), 5);
        assert_eq!(buffer.slice_list.size(), 2);
        assert!(buffer.pin_registry.slots.is_empty());

        let mut second = [0; 8];
        let mut dst = ReadBuf::new(&mut second);
        assert_eq!(buffer.read_available_into(&mut dst, 64).unwrap(), 5);
        assert_eq!(dst.filled(), b"efghi");
        assert!(buffer.is_empty());
        assert_eq!(buffer.slice_list.size(), 1);
        assert_eq!(buffer.slice_list.front().unwrap().size(), 0);
        assert!(buffer.pin_registry.slots.is_empty());

        buffer.clean();
    }

    #[test]
    fn read_available_into_limits_slices_per_call() {
        let manager = Arc::new(init_shm());
        let mut buffer = LinkedBuffer::new(manager);
        for value in 0..65u8 {
            append_fallback(&mut buffer, &[value]);
        }

        let mut output = [0; 65];
        let mut dst = ReadBuf::new(&mut output);
        assert_eq!(buffer.read_available_into(&mut dst, 64).unwrap(), 64);
        assert_eq!(dst.filled(), &(0..64u8).collect::<Vec<_>>());
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.slice_list.size(), 1);

        assert_eq!(buffer.read_available_into(&mut dst, 64).unwrap(), 1);
        assert_eq!(dst.filled(), &(0..65u8).collect::<Vec<_>>());
        assert!(buffer.is_empty());
        assert_eq!(buffer.slice_list.size(), 1);

        buffer.clean();
    }

    #[test]
    fn read_available_into_handles_empty_destination_and_buffer() {
        let manager = Arc::new(init_shm());
        let mut buffer = LinkedBuffer::new(manager);

        let mut empty = [];
        let mut empty_dst = ReadBuf::new(&mut empty);
        assert_eq!(buffer.read_available_into(&mut empty_dst, 64).unwrap(), 0);

        let mut output = [0; 1];
        let mut dst = ReadBuf::new(&mut output);
        assert!(matches!(
            buffer.read_available_into(&mut dst, 64),
            Err(Error::NotEnoughData)
        ));
    }

    #[test]
    fn read_available_into_preserves_last_shm_slice_for_reuse() {
        let manager = Arc::new(init_shm());
        let slice = manager.alloc_shm_buffer(1024).unwrap();
        let mut buffer = new_linked_buffer_with_slice(manager, slice);
        let data = vec![7u8; 256];
        buffer.write_bytes(&data).unwrap();
        buffer.done(false);

        let mut output = vec![0; data.len()];
        let mut dst = ReadBuf::new(&mut output);
        assert_eq!(
            buffer.read_available_into(&mut dst, 64).unwrap(),
            data.len()
        );
        assert_eq!(dst.filled(), data);
        assert!(buffer.is_empty());
        assert_eq!(buffer.slice_list.size(), 1);
        assert!(buffer.pin_registry.slots.is_empty());

        buffer.release_previous_read_and_reserve();
        assert_eq!(buffer.slice_list.size(), 1);
        assert!(buffer.slice_list.write_slice.is_some());
        assert_eq!(buffer.slice_list.front().unwrap().read_index, 0);
        assert_eq!(buffer.slice_list.front().unwrap().write_index, 0);

        buffer.clean();
    }

    #[test]
    fn read_exact_bytes_is_atomic_when_data_is_insufficient() {
        let manager = Arc::new(init_shm());
        let first = manager.alloc_shm_buffer(1024).unwrap();
        let mut buffer = new_linked_buffer_with_slice(manager, first);
        let data = vec![3u8; 256];
        buffer.write_bytes(&data).unwrap();
        buffer.done(false);

        assert!(matches!(
            buffer.read_exact_bytes(data.len() + 1),
            Err(Error::NotEnoughData)
        ));
        assert_eq!(buffer.len(), data.len());

        let exact = buffer.read_exact_bytes(data.len()).unwrap();
        assert_eq!(&exact[..], data);
    }

    #[test]
    fn test_linked_buffer_read_exact_bytes() {
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

                let read_data = buf.read_exact_bytes(one_read_size).unwrap();
                if read_data.is_empty() {
                    assert_eq!(one_read_size, 0);
                } else {
                    assert_eq!(&data[read..read + one_read_size], &read_data[..]);
                }
                read += one_read_size;
            }
            assert_eq!(1 << 21, read);
            buf.read_exact_bytes(0).unwrap();
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

        let get_str = writer.read_exact_bytes(str.len()).unwrap();
        assert_eq!(str, std::str::from_utf8(&get_str).unwrap());

        let get_bytes = writer.read_exact_bytes(str.len()).unwrap();
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
            let get_data = writer.read_exact_bytes(1024).unwrap();
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
                let r = writer.read_exact_bytes(read_size).unwrap();
                for j in 0..r.len() {
                    assert_eq!(count as u8, r[j]);
                    count += 1;
                }
            } else if remain_len > 0 {
                let r = writer.read_exact_bytes(writer.len()).unwrap();
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
