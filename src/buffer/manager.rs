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
use std::{
    collections::HashMap,
    ffi::CString,
    fs::{self, OpenOptions, Permissions},
    os::{
        fd::{IntoRawFd, RawFd},
        raw::c_void,
        unix::prelude::PermissionsExt,
    },
    path::Path,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicI32, Ordering},
    },
};

use anyhow::anyhow;
use memmap2::{MmapMut, MmapOptions};
use nix::libc::munmap;

use super::list::BufferList;
use crate::{
    buffer::{
        list::{BUFFER_LIST_HEADER_SIZE, count_buffer_list_mem_size},
        slice::{BufferHeader, BufferSlice, SliceList},
    },
    config::SizePercentPair,
    consts::MemMapType,
    error::Error,
    util::can_create_on_dev_shm,
};

/// cap 4 + size 4 + start 4 + next 4 + flag 4
pub const BUFFER_HEADER_SIZE: u32 = 4 + 4 + 4 + 4 + 4;
pub const BUFFER_CAP_OFFSET: u32 = 0;
pub const BUFFER_SIZE_OFFSET: u32 = BUFFER_CAP_OFFSET + 4;
pub const BUFFER_DATA_START_OFFSET: u32 = BUFFER_SIZE_OFFSET + 4;
pub const NEXT_BUFFER_OFFSET: u32 = BUFFER_DATA_START_OFFSET + 4;
pub const BUFFER_FLAG_OFFSET: u32 = NEXT_BUFFER_OFFSET + 4;

pub const BUFFER_MANAGER_HEADER_SIZE: u32 = 8;
pub const BM_CAP_OFFSET: u32 = 4;

pub const HAS_NEXT_BUFFER_FLAG: u8 = 1 << 0;
pub const SLICE_IN_USED_FLAG: u8 = 1 << 1;

static BUFFER_MANAGERS: LazyLock<Mutex<HashMap<String, Arc<BufferManager>>>> =
    LazyLock::new(|| Mutex::new(HashMap::with_capacity(8)));

#[derive(Debug)]
/// BufferManager's layout in share memory: list_size 2 byte | buffer_list n byte
pub struct BufferManager {
    /// Ascending ordered by BufferList.cap_per_buffer
    pub(crate) lists: Vec<BufferList>,
    mem: MmapMut,
    #[allow(dead_code)]
    min_slice_size: u32,
    max_slice_size: u32,
    ref_count: AtomicI32,
    mem_map_type: MemMapType,
    pub(crate) path: String,
    pub(crate) memfd: RawFd,
}

impl BufferManager {
    pub fn get_with_memfd(
        buffer_path_name: &str,
        mut memfd: RawFd,
        mut capacity: u32,
        create: bool,
        pairs: &mut [SizePercentPair],
    ) -> Result<Arc<Self>, anyhow::Error> {
        let mut bms = BUFFER_MANAGERS.lock().unwrap();
        if let Some(bm) = bms.get(buffer_path_name) {
            bm.ref_count.fetch_add(1, Ordering::SeqCst);
            if memfd > 0 && !create {
                // FIXME: whether need to close memfd?
                _ = nix::unistd::close(memfd);
            }
            return Ok(bm.clone());
        }
        if create {
            let owned_fd = nix::sys::memfd::memfd_create(
                CString::new(format!("shmipc{}", buffer_path_name))
                    .expect("CString::new failed")
                    .as_c_str(),
                nix::sys::memfd::MemFdCreateFlag::empty(),
            )
            .map_err(|err| anyhow!("BufferManager get_with_memfd memfd_create failed: {}", err))?;
            nix::unistd::ftruncate(&owned_fd, capacity as i64).map_err(|err| {
                anyhow!(
                    "BufferManager get_with_memfd truncate share memory failed: {}",
                    err
                )
            })?;
            memfd = owned_fd.into_raw_fd();
        } else {
            let f_info = nix::sys::stat::fstat(memfd)
                .map_err(|err| anyhow!("BufferManager get_with_memfd mapping failed: {}", err))?;
            capacity = f_info.st_size as u32;
        }

        let mem = unsafe {
            MmapOptions::new()
                .len(capacity as usize)
                .map_mut(memfd)
                .map_err(|err| anyhow!("BufferManager get_with_memfd mmap failed: {}", err))?
        };

        let mut bm = if create {
            pairs.sort_by(|a, b| a.size.cmp(&b.size));
            Self::create(pairs, buffer_path_name, mem, 0)
        } else {
            Self::mapping(buffer_path_name, mem, 0)
        }?;

        bm.memfd = memfd;
        bm.mem_map_type = MemMapType::MemMapTypeMemFd;

        let bm = Arc::new(bm);
        bms.insert(buffer_path_name.to_owned(), bm.clone());
        Ok(bm)
    }

    pub fn get_with_file(
        shm_path: &str,
        mut capacity: u32,
        create: bool,
        pairs: &mut [SizePercentPair],
    ) -> Result<Arc<BufferManager>, anyhow::Error> {
        let mut bms = BUFFER_MANAGERS.lock().unwrap();
        if let Some(bm) = bms.get(shm_path) {
            bm.ref_count.fetch_add(1, Ordering::SeqCst);
            return Ok(bm.clone());
        }

        // ignore mkdir error
        _ = fs::create_dir_all(Path::new(shm_path).parent().unwrap_or(Path::new("/")));
        _ = fs::set_permissions(shm_path, Permissions::from_mode(0o777));

        let shm_file = if create {
            if !can_create_on_dev_shm(capacity as u64, shm_path) {
                return Err(anyhow!(
                    "get_global_buffer_manager can not create on dev shm"
                ));
            }

            let shm_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(shm_path)?;
            shm_file.set_permissions(Permissions::from_mode(0o777))?;

            shm_file.set_len(capacity as u64)?;
            shm_file
        } else {
            // file flag don't include os.O_CREATE, because in this case, the share memory should
            // be created by peer.
            let shm_file = OpenOptions::new().read(true).write(true).open(shm_path)?;
            let fi = shm_file.metadata().map_err(|err| {
                anyhow!(
                    "get_global_buffer_manager mapping failed,{}",
                    err.to_string()
                )
            })?;
            capacity = fi.len() as u32;
            shm_file
        };

        let mem = unsafe {
            MmapOptions::new()
                .len(capacity as usize)
                .map_mut(shm_file.into_raw_fd())?
        };

        let bm = if create {
            pairs.sort_by(|a, b| a.size.cmp(&b.size));
            Self::create(pairs, shm_path, mem, 0)
        } else {
            Self::mapping(shm_path, mem, 0)
        }?;

        let bm = Arc::new(bm);
        bms.insert(shm_path.to_owned(), bm.clone());
        Ok(bm)
    }

    /// Create a buffer manager in share memory, used for client side.
    pub fn create(
        list_size_percent: &[SizePercentPair],
        path: &str,
        mem: MmapMut,
        offset: u32,
    ) -> Result<Self, anyhow::Error> {
        if mem.len() <= offset as usize {
            return Err(anyhow!(
                "mem's size is at least: {}, but: {}",
                offset + 1,
                mem.len()
            ));
        }

        // number of list 2 byte | 2 byte reserve | used_length 4 byte
        let buffer_region_cap = (mem.len() as u32
            - offset
            - BUFFER_LIST_HEADER_SIZE * list_size_percent.len() as u32
            - BUFFER_MANAGER_HEADER_SIZE) as u64;
        tracing::info!(
            "create buffer manager path:{} config:{:?} mem_size:{} buffer_region_cap:{} offset:{}",
            path,
            list_size_percent,
            mem.len(),
            buffer_region_cap,
            offset
        );
        unsafe {
            *(mem.as_ptr().offset(offset as isize) as *mut u16) = list_size_percent.len() as u16;
        }

        let mut had_used_offset = BUFFER_MANAGER_HEADER_SIZE + offset;
        let mut free_buffer_lists = Vec::with_capacity(list_size_percent.len());
        let mut sum_percent = 0;
        for pair in list_size_percent.iter() {
            sum_percent += pair.percent;
            if sum_percent > 100 {
                return Err(anyhow!(
                    "the sum of all size_percent_pairs's percent must be equals 100"
                ));
            }
            let buffer_num = (buffer_region_cap * pair.percent as u64 / 100) as u32
                / (pair.size + BUFFER_HEADER_SIZE);
            let need_size = count_buffer_list_mem_size(buffer_num, pair.size);
            let free_list = BufferList::create(buffer_num, pair.size, &mem, had_used_offset)?;
            free_buffer_lists.push(free_list);
            had_used_offset += need_size;
        }
        unsafe {
            *(mem.as_ptr().offset((offset + BM_CAP_OFFSET) as isize) as *mut u32) =
                had_used_offset - BUFFER_MANAGER_HEADER_SIZE;
        }
        Ok(Self {
            lists: free_buffer_lists,
            mem,
            min_slice_size: list_size_percent[0].size,
            max_slice_size: list_size_percent[list_size_percent.len() - 1].size,
            path: path.to_owned(),
            ref_count: AtomicI32::new(1),
            mem_map_type: Default::default(),
            memfd: Default::default(),
        })
    }

    /// Mapping a buffer manager in share memory, used for server side.
    pub fn mapping(
        path: &str,
        mem: MmapMut,
        buffer_region_start_offset: u32,
    ) -> Result<Self, anyhow::Error> {
        if mem.len() <= (buffer_region_start_offset + BM_CAP_OFFSET) as usize
            || mem.len() <= buffer_region_start_offset as usize
        {
            return Err(anyhow!(
                "mem's size is at least:{} but:{} buffer_region_start_offset:{}",
                buffer_region_start_offset + BM_CAP_OFFSET + 1,
                mem.len(),
                buffer_region_start_offset
            ));
        }
        let list_num =
            unsafe { *(mem.as_ptr().offset(buffer_region_start_offset as isize) as *const u16) };
        let mut free_lists = Vec::with_capacity(list_num as usize);
        let length = unsafe {
            *(mem
                .as_ptr()
                .offset((buffer_region_start_offset + BM_CAP_OFFSET) as isize)
                as *const u32)
        };
        if mem.len() < (BUFFER_MANAGER_HEADER_SIZE + length) as usize || list_num == 0 {
            return Err(anyhow!(
                "could not mapping buffer manager ,list_num:{} len(mem) at least:{} but:{}",
                list_num,
                BUFFER_MANAGER_HEADER_SIZE + length,
                mem.len()
            ));
        }
        let mut had_used_offset = BUFFER_MANAGER_HEADER_SIZE;
        tracing::info!(
            "mapping buffer manager, list_num:{} length:{}",
            list_num,
            length
        );

        for _ in 0..list_num {
            let l = BufferList::mapping(&mem, buffer_region_start_offset + had_used_offset)?;
            unsafe {
                tracing::info!(
                    "mapping buffer list offset:{} size:{} head:{} tail:{} capPerBuffer:{} ",
                    buffer_region_start_offset + had_used_offset,
                    (*l.size).load(Ordering::SeqCst),
                    (*l.head).load(Ordering::SeqCst),
                    (*l.tail).load(Ordering::SeqCst),
                    *l.cap_per_buffer
                );
            }
            let size =
                count_buffer_list_mem_size(unsafe { (*l.cap).load(Ordering::SeqCst) }, unsafe {
                    *l.cap_per_buffer
                });
            had_used_offset += size;
            free_lists.push(l);
        }

        // sort by cap_per_buffer
        Ok(Self {
            path: path.to_owned(),
            mem,
            min_slice_size: unsafe { *free_lists[0].cap_per_buffer },
            max_slice_size: unsafe { *free_lists[free_lists.len() - 1].cap_per_buffer },
            lists: free_lists,
            ref_count: AtomicI32::new(1),
            mem_map_type: Default::default(),
            memfd: Default::default(),
        })
    }
}

pub async fn add_global_buffer_manager_ref_count(path: &str, c: i32) {
    let bm = BUFFER_MANAGERS.lock().unwrap().remove(path);
    if let Some(bm) = &bm {
        if bm.ref_count.fetch_add(c, Ordering::SeqCst) + c <= 0 {
            tracing::info!("clean buffer manager:{}", path);
            bm.unmap().await;
            return;
        }
    }
    if let Some(bm) = bm {
        BUFFER_MANAGERS.lock().unwrap().insert(path.to_owned(), bm);
    }
}

impl BufferManager {
    pub fn remain_size(&self) -> u32 {
        let mut result = 0;
        for list in self.lists.iter() {
            let remain = unsafe {
                (*list.size).load(Ordering::SeqCst) as isize * (*list.cap_per_buffer) as isize
            };

            if remain > 0 {
                result += remain as u32;
            }
        }
        result
    }

    // alloc single buffer slice , whose performance better than alloc_shm_buffers.
    pub fn alloc_shm_buffer(&self, size: u32) -> Result<BufferSlice, Error> {
        if size <= self.max_slice_size {
            for list in self.lists.iter() {
                if size <= unsafe { *list.cap_per_buffer } {
                    let buf = list.pop()?;
                    return Ok(buf);
                }
            }
        }
        Err(Error::NoMoreBuffer)
    }

    pub fn alloc_shm_buffers(&self, slices: &mut SliceList, size: u32) -> i64 {
        let mut remain = size as i64;
        let mut alloc_size = 0;
        let mut i = self.lists.len() as isize - 1;
        while i >= 0 && remain > 0 {
            while remain > 0 {
                if let Ok(buf) = self.lists[i as usize].pop() {
                    alloc_size += buf.cap as i64;
                    remain -= buf.cap as i64;
                    slices.push_back(buf);
                } else {
                    break;
                }
            }
            i -= 1;
        }
        alloc_size
    }

    pub fn recycle_buffer(&self, slice: BufferSlice) {
        if slice.is_from_shm {
            for list in self.lists.iter() {
                if slice.cap == unsafe { *list.cap_per_buffer } {
                    list.push(slice);
                    break;
                }
            }
        } else {
            unsafe {
                _ = Vec::from_raw_parts(slice.data, slice.cap as usize, slice.cap as usize);
            }
        }
    }

    pub fn recycle_buffers(&self, mut slice: BufferSlice) {
        if slice.is_from_shm {
            loop {
                // unwrap is safe, because slice is from shm
                if !slice.buffer_header.as_ref().unwrap().has_next() {
                    self.recycle_buffer(slice);
                    return;
                }
                let next_slice_offset = slice.buffer_header.as_ref().unwrap().next_buffer_offset();
                self.recycle_buffer(slice);
                match self.read_buffer_slice(next_slice_offset) {
                    Ok(s) => slice = s,
                    Err(e) => {
                        tracing::error!(
                            "{}",
                            format!(
                                "BufferManager recycle_buffers read_buffer_slice failed, err={}",
                                e
                            )
                        );
                        return;
                    }
                }
            }
        } else {
            unsafe {
                _ = Vec::from_raw_parts(slice.data, slice.cap as usize, slice.cap as usize);
            }
        }
    }

    pub fn slice_size(&self) -> isize {
        let mut size = 0;
        for list in self.lists.iter() {
            size += unsafe { (*list.size).load(Ordering::SeqCst) as isize };
        }
        size
    }

    pub fn read_buffer_slice(&self, offset: u32) -> Result<BufferSlice, anyhow::Error> {
        if (offset + BUFFER_HEADER_SIZE) as usize >= self.mem.len() {
            return Err(anyhow!(
                "broken share memory. readBufferSlice unexpect offset:{} buffers cap:{}",
                offset,
                self.mem.len()
            ));
        }
        let buf_cap = unsafe {
            *(self
                .mem
                .as_ptr()
                .offset((offset + BUFFER_CAP_OFFSET) as isize) as *const u32)
        };
        let buf_end_offset = offset + BUFFER_HEADER_SIZE + buf_cap;
        if buf_end_offset > self.mem.len() as u32 {
            return Err(anyhow!(
                "broken share memory. readBufferSlice unexpect buffer_end_offset:{} \
                 buffer_start_offset:{} buffers cap:{}",
                buf_end_offset,
                offset,
                self.mem.len()
            ));
        }
        unsafe {
            Ok(BufferSlice::new(
                Some(BufferHeader(
                    self.mem.as_ptr().offset(offset as isize) as *mut u8
                )),
                slice::from_raw_parts_mut(
                    self.mem
                        .as_ptr()
                        .offset((offset + BUFFER_HEADER_SIZE) as isize)
                        as *mut u8,
                    (buf_end_offset - (offset + BUFFER_HEADER_SIZE)) as usize,
                ),
                offset,
                true,
            ))
        }
    }

    pub fn check_buffer_returned(&self) -> bool {
        for list in self.lists.iter() {
            unsafe {
                if (*list.size).load(Ordering::SeqCst) as u32 != (*list.cap).load(Ordering::SeqCst)
                {
                    return false;
                }

                if (*list.counter).load(Ordering::SeqCst) != 0 {
                    return false;
                }
            }
        }
        true
    }

    pub async fn unmap(&self) {
        // spin 5s to check if all buffer are returned, if timeout, we still force unmap TODO: ?
        for _ in 0..50 {
            if self.check_buffer_returned() {
                tracing::info!("all buffer returned before unmap");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        unsafe { munmap(self.mem.as_ptr() as *mut c_void, self.mem.len()) };
        if let MemMapType::MemMapTypeDevShmFile = self.mem_map_type {
            if let Err(e) = std::fs::remove_file(&self.path) {
                tracing::warn!(
                    "bufferManager remove file:{} failed, error={}",
                    self.path,
                    e
                );
            } else {
                tracing::info!("bufferManager remove file:{}", self.path);
            }
        } else if let Err(err) = nix::unistd::close(self.memfd) {
            tracing::warn!(
                "bufferManager close fd:{} failed, error={}",
                self.memfd,
                err
            );
        } else {
            tracing::info!("bufferManager close fd:{}", self.memfd);
        }
    }
}

#[cfg(test)]
mod tests {
    use rand::Rng;

    use super::{BufferManager, SizePercentPair};
    use crate::buffer::{linked::LinkedBuffer, slice::SliceList};

    #[test]
    fn test_buffer_manager_create_and_mapping() {
        // create
        let bm1 = BufferManager::get_with_file(
            "/tmp/shm",
            32 << 20,
            true,
            &mut [
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
        )
        .unwrap();

        let allocate_func = |bm: &BufferManager| {
            for _ in 0..10 {
                _ = bm.alloc_shm_buffer(4096).unwrap();
                _ = bm.alloc_shm_buffer(16 * 1024).unwrap();
                _ = bm.alloc_shm_buffer(64 * 1024).unwrap();
            }
        };
        allocate_func(&bm1);

        // mapping
        let bm2 = BufferManager::get_with_file(
            "/tmp/shm",
            32 << 20,
            false,
            &mut [
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
        )
        .unwrap();
        for i in 0..bm1.lists.len() {
            unsafe {
                assert_eq!(*bm1.lists[i].cap_per_buffer, *bm2.lists[i].cap_per_buffer);
                assert_eq!(
                    (*bm1.lists[i].size).load(std::sync::atomic::Ordering::SeqCst),
                    (*bm2.lists[i].size).load(std::sync::atomic::Ordering::SeqCst)
                );
                assert_eq!(bm1.lists[i].offset_in_shm, bm2.lists[i].offset_in_shm);
            }
        }

        allocate_func(&bm2);

        for i in 0..bm1.lists.len() {
            unsafe {
                assert_eq!(*bm1.lists[i].cap_per_buffer, *bm2.lists[i].cap_per_buffer);
                assert_eq!(
                    (*bm1.lists[i].size).load(std::sync::atomic::Ordering::SeqCst),
                    (*bm2.lists[i].size).load(std::sync::atomic::Ordering::SeqCst)
                );
                assert_eq!(bm1.lists[i].offset_in_shm, bm2.lists[i].offset_in_shm);
            }
        }
    }

    #[test]
    fn test_buffer_manager_read_buffer_slice() {
        let bm = BufferManager::get_with_file(
            "/tmp/shm1",
            1 << 20,
            true,
            &mut [SizePercentPair {
                size: 4096,
                percent: 100,
            }],
        )
        .unwrap();

        let mut s = bm.alloc_shm_buffer(4096).unwrap();
        let mut rng = rand::rng();
        let data: Vec<u8> = (0..4096).map(|_| rng.random()).collect();
        assert_eq!(4096, s.append(&data));
        assert_eq!(4096, s.size());
        s.update();

        let mut s2 = bm.read_buffer_slice(s.offset_in_shm).unwrap();
        assert_eq!(s.capacity(), s2.capacity());
        assert_eq!(s.size(), s2.size());

        let get_data = s2.read(4096);
        assert_eq!(data, get_data);

        let s3 = bm.read_buffer_slice(s.offset_in_shm + (1 << 20));
        assert!(s3.is_err());

        let s4 = bm.read_buffer_slice(s.offset_in_shm + 4096);
        assert!(s4.is_err());
    }

    #[test]
    fn test_buffer_manager_alloc_recycle() {
        // alloc buffer
        let bm = BufferManager::get_with_file(
            "/tmp/shm11",
            1 << 20,
            true,
            &mut [
                SizePercentPair {
                    size: 4096,
                    percent: 50,
                },
                SizePercentPair {
                    size: 8192,
                    percent: 50,
                },
            ],
        )
        .unwrap();
        // use first two buffer to record buffer list info (List header)
        assert_eq!((1 << 20) - 4096 - 8192, bm.remain_size());

        let num_of_slice = bm.slice_size();
        let mut buffers = Vec::with_capacity(1024);
        while let Ok(s) = bm.alloc_shm_buffer(4096) {
            buffers.push(s);
        }
        for buffer in buffers {
            bm.recycle_buffer(buffer);
        }

        // alloc_buffers, recycle_buffers
        let mut slices = SliceList::new();
        let size = bm.alloc_shm_buffers(&mut slices, 256 * 1024);
        assert_eq!(256 * 1024, size);
        let mut linked_buffer_slices = LinkedBuffer::new(bm.clone());
        while slices.size() > 0 {
            linked_buffer_slices.append_buffer_slice(slices.pop_front().unwrap());
        }
        linked_buffer_slices.done(false);
        bm.recycle_buffers(linked_buffer_slices.slice_list_mut().pop_front().unwrap());
        assert_eq!(num_of_slice, bm.slice_size());
    }
}
