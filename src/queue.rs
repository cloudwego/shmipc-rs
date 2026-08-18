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
    ffi::CString,
    fs::{self, OpenOptions, Permissions},
    os::{
        fd::{AsRawFd, BorrowedFd, IntoRawFd, RawFd},
        unix::prelude::PermissionsExt,
    },
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicI64, AtomicU32, Ordering},
    },
};

use anyhow::anyhow;
use memmap2::{MmapMut, MmapOptions};

use crate::{
    consts::{MemMapType, QUEUE_COUNT, QUEUE_ELEMENT_LEN},
    error::Error,
    util::can_create_on_dev_shm,
};

const QUEUE_HEADER_LENGTH: usize = 24;

#[derive(Debug)]
pub struct QueueManager {
    pub(crate) path: String,
    pub(crate) send_queue: Queue,
    pub(crate) recv_queue: Queue,
    pub(crate) memfd: RawFd,
    #[allow(dead_code)]
    mem: MmapMut,
    mmap_map_type: MemMapType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueLayout {
    Legacy,
    Aligned,
}

impl QueueLayout {
    pub(crate) const fn for_protocol_version(version: u8) -> Self {
        if version >= 4 || cfg!(any(target_arch = "aarch64", target_arch = "riscv64")) {
            Self::Aligned
        } else {
            Self::Legacy
        }
    }

    const fn working_flag_offset(self) -> isize {
        match self {
            Self::Legacy => 20,
            Self::Aligned => 4,
        }
    }

    const fn head_offset(self) -> isize {
        match self {
            Self::Legacy => 4,
            Self::Aligned => 8,
        }
    }

    const fn tail_offset(self) -> isize {
        match self {
            Self::Legacy => 12,
            Self::Aligned => 16,
        }
    }
}

impl QueueManager {
    pub fn create_with_memfd(
        queue_path_name: &str,
        queue_cap: u32,
        proto_version: u8,
    ) -> Result<Self, anyhow::Error> {
        #[cfg(target_os = "linux")]
        {
            let layout = QueueLayout::for_protocol_version(proto_version);
            let memfd = nix::sys::memfd::memfd_create(
                CString::new(format!("shmipc{}", queue_path_name))
                    .expect("CString::new failed")
                    .as_c_str(),
                nix::sys::memfd::MFdFlags::empty(),
            )?;

            let mem_size = count_queue_mem_size(queue_cap) * QUEUE_COUNT;
            nix::unistd::ftruncate(&memfd, mem_size as i64).map_err(|err| {
                anyhow!(
                    "create_queue_manager_with_memfd truncate share memory failed: {}",
                    err
                )
            })?;

            let mut mem = unsafe { MmapOptions::new().len(mem_size).map_mut(&memfd)? };
            mem.fill(0);

            Ok(Self {
                path: queue_path_name.to_owned(),
                send_queue: Queue::create_from_bytes(mem.as_mut_ptr(), queue_cap, layout),
                recv_queue: Queue::create_from_bytes(
                    unsafe { mem.as_mut_ptr().add(mem_size / 2) },
                    queue_cap,
                    layout,
                ),
                mem,
                mmap_map_type: MemMapType::MemMapTypeMemFd,
                memfd: memfd.into_raw_fd(),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(anyhow!("memfd_create is only supported on Linux"))
        }
    }

    pub fn create_with_file(
        shm_path: &str,
        queue_cap: u32,
        proto_version: u8,
    ) -> Result<Self, anyhow::Error> {
        let layout = QueueLayout::for_protocol_version(proto_version);
        // ignore mkdir error
        let path = Path::new(shm_path);
        _ = fs::create_dir_all(path.parent().unwrap_or(Path::new("/")));
        _ = fs::set_permissions(shm_path, Permissions::from_mode(0o777));
        if path.exists() {
            return Err(anyhow!("queue was existed, path:{}", shm_path));
        }
        let mem_size = count_queue_mem_size(queue_cap) * QUEUE_COUNT;
        if !can_create_on_dev_shm(mem_size as u64, shm_path) {
            return Err(anyhow!(
                "err: share memory had not left space, path:{}, size:{}",
                shm_path,
                mem_size
            ));
        }

        let shm_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(shm_path)?;
        shm_file.set_permissions(Permissions::from_mode(0o777))?;
        shm_file.set_len(mem_size as u64)?;

        let mut mem = unsafe {
            MmapOptions::new()
                .len(mem_size)
                .map_mut(shm_file.as_raw_fd())?
        };
        mem.fill(0);

        Ok(Self {
            path: shm_path.to_owned(),
            send_queue: Queue::create_from_bytes(mem.as_mut_ptr(), queue_cap, layout),
            recv_queue: Queue::create_from_bytes(
                unsafe { mem.as_mut_ptr().add(mem_size / 2) },
                queue_cap,
                layout,
            ),
            mem,
            mmap_map_type: MemMapType::MemMapTypeDevShmFile,
            memfd: 0,
        })
    }

    pub fn mapping_with_memfd(
        queue_path_name: &str,
        memfd: RawFd,
        proto_version: u8,
    ) -> Result<Self, anyhow::Error> {
        let layout = QueueLayout::for_protocol_version(proto_version);
        let file_stat = nix::sys::stat::fstat(unsafe { BorrowedFd::borrow_raw(memfd) })?;

        if file_stat.st_size < 0 {
            return Err(anyhow!(
                "invalid queue share memory size: {}",
                file_stat.st_size
            ));
        }
        let mapping_size = file_stat.st_size as u64;
        if layout == QueueLayout::Aligned && !mapping_size.is_multiple_of(16) {
            return Err(anyhow!(
                "the memory size of queue should be a multiple of 16"
            ));
        }

        let mut mem = unsafe {
            MmapOptions::new()
                .len(mapping_size as usize)
                .map_mut(memfd)?
        };
        Ok(Self {
            path: queue_path_name.to_owned(),
            send_queue: Queue::mapping_from_bytes(
                unsafe { mem.as_mut_ptr().offset((mapping_size / 2) as isize) },
                layout,
            ),
            recv_queue: Queue::mapping_from_bytes(mem.as_mut_ptr(), layout),
            mem,
            mmap_map_type: MemMapType::MemMapTypeMemFd,
            memfd,
        })
    }

    pub fn mapping_with_file(shm_path: &str, proto_version: u8) -> Result<Self, anyhow::Error> {
        let layout = QueueLayout::for_protocol_version(proto_version);
        let file = OpenOptions::new().read(true).write(true).open(shm_path)?;
        file.set_permissions(Permissions::from_mode(0o777))?;
        let fi = file.metadata()?;

        let mapping_size = fi.len();
        if layout == QueueLayout::Aligned && !mapping_size.is_multiple_of(16) {
            return Err(anyhow!(
                "the memory size of queue should be a multiple of 16"
            ));
        }

        let mut mem = unsafe {
            MmapOptions::new()
                .len(mapping_size as usize)
                .map_mut(file.as_raw_fd())?
        };
        Ok(Self {
            path: shm_path.to_owned(),
            send_queue: Queue::mapping_from_bytes(
                unsafe { mem.as_mut_ptr().offset((mapping_size / 2) as isize) },
                layout,
            ),
            recv_queue: Queue::mapping_from_bytes(mem.as_mut_ptr(), layout),
            mem,
            mmap_map_type: MemMapType::MemMapTypeDevShmFile,
            memfd: 0,
        })
    }

    pub fn unmap(&self) {
        if let MemMapType::MemMapTypeDevShmFile = self.mmap_map_type {
            if let Err(e) = std::fs::remove_file(&self.path) {
                tracing::warn!("queueManager remove file:{} failed, error={}", self.path, e);
            } else {
                tracing::info!("queueManager remove file:{}", self.path);
            }
        } else if let Err(err) = nix::unistd::close(self.memfd) {
            tracing::warn!("queueManager close fd:{} failed, error={}", self.memfd, err);
        } else {
            tracing::info!("queueManager close fd:{}", self.memfd);
        }
    }
}

#[derive(Debug)]
pub struct Queue {
    // consumer write, producer read
    pub(crate) head: *mut i64,
    // producer write, consumer read
    pub(crate) tail: *mut i64,
    /// when peer is consuming the queue, the working_flag is 1, otherwise 0
    working_flag: *const AtomicU32,
    // it could be from share memory or process memory.
    queue_bytes_on_memory: *const u8,
    cap: i64,
    #[allow(dead_code)]
    len: usize,
    lock: Mutex<()>,
    layout: QueueLayout,
}

unsafe impl Send for Queue {}
unsafe impl Sync for Queue {}

pub struct QueueElement {
    pub(crate) seq_id: u32,
    pub(crate) offset_in_shm_buf: u32,
    pub(crate) status: u32,
}

impl Queue {
    pub fn create_from_bytes(data: *mut u8, cap: u32, layout: QueueLayout) -> Self {
        unsafe { *(data as *mut u32) = cap };
        let q = Self::mapping_from_bytes(data, layout);
        q.store_head(0);
        q.store_tail(0);
        unsafe { (*q.working_flag).store(0, Ordering::SeqCst) };
        q
    }

    pub fn mapping_from_bytes(data: *mut u8, layout: QueueLayout) -> Self {
        let cap = unsafe { *(data as *mut u32) };
        let queue_start_offset = QUEUE_HEADER_LENGTH;
        let queue_end_offset = QUEUE_HEADER_LENGTH + QUEUE_ELEMENT_LEN * cap as usize;
        unsafe {
            Self {
                cap: cap as i64,
                working_flag: data.offset(layout.working_flag_offset()) as *const AtomicU32,
                head: data.offset(layout.head_offset()) as *mut i64,
                tail: data.offset(layout.tail_offset()) as *mut i64,
                queue_bytes_on_memory: data.add(queue_start_offset) as *const u8,
                len: queue_end_offset - queue_start_offset,
                lock: Mutex::new(()),
                layout,
            }
        }
    }

    pub fn put(&self, element: QueueElement) -> Result<(), Error> {
        let _tail_lock = self.lock.lock().unwrap();
        unsafe {
            let tail = self.load_tail();
            if tail - self.load_head() >= self.cap {
                return Err(Error::QueueFull);
            }
            let queue_offset = (tail % self.cap) as isize * QUEUE_ELEMENT_LEN as isize;
            *(self.queue_bytes_on_memory.offset(queue_offset) as *mut u32) = element.seq_id;
            *(self.queue_bytes_on_memory.offset(queue_offset + 4) as *mut u32) =
                element.offset_in_shm_buf;
            *(self.queue_bytes_on_memory.offset(queue_offset + 8) as *mut u32) = element.status;
            self.store_tail(tail + 1);
        };
        Ok(())
    }

    pub fn pop(&self) -> Result<QueueElement, Error> {
        unsafe {
            let head = self.load_head();
            if head >= self.load_tail() {
                return Err(Error::QueueEmpty);
            }
            let queue_offset = (head % self.cap) as isize * QUEUE_ELEMENT_LEN as isize;
            let element = QueueElement {
                seq_id: *(self.queue_bytes_on_memory.offset(queue_offset) as *const u32),
                offset_in_shm_buf: *(self.queue_bytes_on_memory.offset(queue_offset + 4)
                    as *const u32),
                status: *(self.queue_bytes_on_memory.offset(queue_offset + 8) as *const u32),
            };

            self.store_head(head + 1);
            Ok(element)
        }
    }

    #[allow(unused)]
    pub fn is_full(&self) -> bool {
        self.size() == self.cap
    }

    #[allow(unused)]
    pub fn is_empty(&self) -> bool {
        self.size() == 0
    }

    pub fn size(&self) -> i64 {
        self.load_tail() - self.load_head()
    }

    #[allow(unused)]
    pub fn consumer_is_working(&self) -> bool {
        unsafe { (*self.working_flag).load(Ordering::SeqCst) > 0 }
    }

    pub fn mark_working(&self) -> bool {
        unsafe { (*self.working_flag).compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst) }
            .is_ok()
    }

    pub fn mark_not_working(&self) -> bool {
        unsafe {
            (*self.working_flag).store(0, Ordering::SeqCst);
        }
        if self.size() == 0 {
            return true;
        }
        unsafe {
            (*self.working_flag).store(1, Ordering::SeqCst);
        }
        false
    }

    fn load_head(&self) -> i64 {
        self.load_position(self.head)
    }

    fn load_tail(&self) -> i64 {
        self.load_position(self.tail)
    }

    fn store_head(&self, value: i64) {
        self.store_position(self.head, value);
    }

    fn store_tail(&self, value: i64) {
        self.store_position(self.tail, value);
    }

    fn load_position(&self, ptr: *mut i64) -> i64 {
        if self.layout == QueueLayout::Aligned {
            // SAFETY: QueueLayout::Aligned places head/tail at 8-byte aligned offsets.
            // Acquire pairs with the peer's Release store that publishes queue data/capacity.
            unsafe { (&*(ptr as *const AtomicI64)).load(Ordering::Acquire) }
        } else {
            unsafe { ptr.read_unaligned() }
        }
    }

    fn store_position(&self, ptr: *mut i64, value: i64) {
        if self.layout == QueueLayout::Aligned {
            // SAFETY: QueueLayout::Aligned places head/tail at 8-byte aligned offsets.
            unsafe { (&*(ptr as *const AtomicI64)).store(value, Ordering::Release) };
        } else {
            unsafe { ptr.write_unaligned(value) };
        }
    }
}

const fn count_queue_mem_size(queue_cap: u32) -> usize {
    QUEUE_HEADER_LENGTH + QUEUE_ELEMENT_LEN * queue_cap as usize
}

#[cfg(test)]
mod test {
    use std::{
        hint::black_box,
        sync::Arc,
        time::{Duration, Instant},
    };

    use super::{QUEUE_HEADER_LENGTH, Queue, QueueLayout};
    use crate::{
        consts::QUEUE_ELEMENT_LEN,
        queue::{QueueElement, QueueManager},
    };

    #[test]
    fn test_queue_manager_create_mapping() {
        let path = "/tmp/ipc1.queue";

        let qm1 = QueueManager::create_with_file(path, 8192, 3).unwrap();
        let qm2 = QueueManager::mapping_with_file(path, 3).unwrap();

        assert!(
            qm1.send_queue
                .put(QueueElement {
                    seq_id: 0,
                    offset_in_shm_buf: 0,
                    status: 0
                })
                .is_ok()
        );
        assert!(qm2.recv_queue.pop().is_ok());

        assert!(
            qm2.send_queue
                .put(QueueElement {
                    seq_id: 0,
                    offset_in_shm_buf: 0,
                    status: 0
                })
                .is_ok()
        );
        assert!(qm1.recv_queue.pop().is_ok());
        qm1.unmap();
    }

    #[test]
    fn test_queue_operate() {
        let q = create_queue(8192);

        assert!(q.is_empty());
        assert!(!q.is_full());
        assert_eq!(0, q.size());

        let mut put_count = 0;
        for i in 0..8192 {
            assert!(
                q.put(QueueElement {
                    seq_id: i,
                    offset_in_shm_buf: i,
                    status: i
                })
                .is_ok()
            );
            put_count += 1;
        }
        let r = q.put(QueueElement {
            seq_id: 1,
            offset_in_shm_buf: 1,
            status: 1,
        });
        assert!(r.is_err());
        assert!(q.is_full());
        assert!(!q.is_empty());
        assert_eq!(put_count, q.size());

        for i in 0..8192 {
            let e = q.pop().unwrap();
            assert_eq!(i, e.seq_id);
            assert_eq!(i, e.offset_in_shm_buf);
            assert_eq!(i, e.status);
        }

        let r = q.pop();
        assert!(r.is_err());
        assert!(q.is_empty());
        assert!(!q.is_full());
        assert_eq!(0, q.size());

        assert!(!q.consumer_is_working());
        q.mark_working();
        assert!(q.consumer_is_working());
        q.mark_not_working();
        assert!(!q.consumer_is_working());

        _ = q.put(QueueElement {
            seq_id: 1,
            offset_in_shm_buf: 1,
            status: 1,
        });
        q.mark_not_working();
        assert!(q.consumer_is_working());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_queue_multi_producer_and_single_consumer() {
        let q = Arc::new(create_queue(100000));
        let mut pop_count = 0;
        for _ in 0..100 {
            let q = q.clone();
            tokio::spawn(async move {
                for _ in 0..1000 {
                    q.put(QueueElement {
                        seq_id: 1,
                        offset_in_shm_buf: 1,
                        status: 1,
                    })
                    .unwrap();
                }
            });
        }

        while pop_count != 100000 {
            match q.pop() {
                Ok(_) => pop_count += 1,
                Err(_) => {
                    tokio::time::sleep(Duration::from_micros(1)).await;
                }
            }
        }
    }

    fn create_queue(cap: u32) -> Queue {
        create_queue_with_layout(cap, QueueLayout::for_protocol_version(3))
    }

    fn create_queue_with_layout(cap: u32, layout: QueueLayout) -> Queue {
        let mem_size = QUEUE_HEADER_LENGTH + QUEUE_ELEMENT_LEN * cap as usize;
        let mut mem: Vec<u8> = vec![0u8; mem_size];
        let queue = Queue::create_from_bytes(mem.as_mut_ptr(), cap, layout);
        std::mem::forget(mem);
        queue
    }

    #[test]
    #[ignore]
    fn bench_queue_layout_put_pop() {
        const ITERS: usize = 10_000_000;
        for layout in [QueueLayout::Legacy, QueueLayout::Aligned] {
            let q = create_queue_with_layout(1024, layout);
            let start = Instant::now();
            let mut checksum = 0u32;
            for i in 0..ITERS {
                q.put(QueueElement {
                    seq_id: black_box(i as u32),
                    offset_in_shm_buf: black_box(i as u32),
                    status: black_box(0),
                })
                .unwrap();
                let element = q.pop().unwrap();
                checksum = checksum.wrapping_add(element.seq_id);
            }
            let elapsed = start.elapsed();
            let ns_per_round = elapsed.as_nanos() as f64 / ITERS as f64;
            let rounds_per_sec = ITERS as f64 / elapsed.as_secs_f64();
            println!(
                "layout={layout:?} iters={ITERS} elapsed_ms={:.2} ns_per_put_pop={:.2} \
                 rounds_per_sec={rounds_per_sec:.0} checksum={checksum}",
                elapsed.as_secs_f64() * 1000.0,
                ns_per_round,
            );
        }
    }

    #[test]
    fn queue_layout_is_version_aware() {
        assert_eq!(QueueLayout::Aligned, QueueLayout::for_protocol_version(4));

        let legacy = QueueLayout::Legacy;
        assert_eq!(4, legacy.head_offset());
        assert_eq!(12, legacy.tail_offset());
        assert_eq!(20, legacy.working_flag_offset());

        let aligned = QueueLayout::Aligned;
        assert_eq!(4, aligned.working_flag_offset());
        assert_eq!(8, aligned.head_offset());
        assert_eq!(16, aligned.tail_offset());
    }
}
