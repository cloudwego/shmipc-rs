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
    fs::{self, File, OpenOptions, Permissions},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        unix::prelude::PermissionsExt,
    },
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
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

impl QueueManager {
    pub fn create_with_memfd(queue_path_name: &str, queue_cap: u32) -> Result<Self, anyhow::Error> {
        let memfd = nix::sys::memfd::memfd_create(
            CString::new(format!("shmipc{}", queue_path_name))
                .expect("CString::new failed")
                .as_c_str(),
            nix::sys::memfd::MemFdCreateFlag::empty(),
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
            send_queue: Queue::create_from_bytes(mem.as_mut_ptr(), queue_cap),
            recv_queue: Queue::create_from_bytes(
                unsafe { mem.as_mut_ptr().add(mem_size / 2) },
                queue_cap,
            ),
            mem,
            mmap_map_type: MemMapType::MemMapTypeMemFd,
            memfd: memfd.into_raw_fd(),
        })
    }

    pub fn create_with_file(shm_path: &str, queue_cap: u32) -> Result<Self, anyhow::Error> {
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
            send_queue: Queue::create_from_bytes(mem.as_mut_ptr(), queue_cap),
            recv_queue: Queue::create_from_bytes(
                unsafe { mem.as_mut_ptr().add(mem_size / 2) },
                queue_cap,
            ),
            mem,
            mmap_map_type: MemMapType::MemMapTypeDevShmFile,
            memfd: 0,
        })
    }

    pub fn mapping_with_memfd(queue_path_name: &str, memfd: RawFd) -> Result<Self, anyhow::Error> {
        let file = unsafe { File::from_raw_fd(memfd) };
        let fi = file.metadata()?;

        let mapping_size = fi.len();
        #[cfg(target_arch = "aarch64")]
        // a queueManager have two queue, a queue's head and tail should align to 8 byte boundary
        if mapping_size % 16 != 0 {
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
            send_queue: Queue::mapping_from_bytes(unsafe {
                mem.as_mut_ptr().offset((mapping_size / 2) as isize)
            }),
            recv_queue: Queue::mapping_from_bytes(mem.as_mut_ptr()),
            mem,
            mmap_map_type: MemMapType::MemMapTypeMemFd,
            memfd,
        })
    }

    pub fn mapping_with_file(shm_path: &str) -> Result<Self, anyhow::Error> {
        let file = OpenOptions::new().read(true).write(true).open(shm_path)?;
        file.set_permissions(Permissions::from_mode(0o777))?;
        let fi = file.metadata()?;

        let mapping_size = fi.len();
        #[cfg(target_arch = "aarch64")]
        // a queueManager have two queue, a queue's head and tail should align to 8 byte boundary
        if mapping_size % 16 != 0 {
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
            send_queue: Queue::mapping_from_bytes(unsafe {
                mem.as_mut_ptr().offset((mapping_size / 2) as isize)
            }),
            recv_queue: Queue::mapping_from_bytes(mem.as_mut_ptr()),
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
}

unsafe impl Send for Queue {}
unsafe impl Sync for Queue {}

pub struct QueueElement {
    pub(crate) seq_id: u32,
    pub(crate) offset_in_shm_buf: u32,
    pub(crate) status: u32,
}

impl Queue {
    pub fn create_from_bytes(data: *mut u8, cap: u32) -> Self {
        unsafe { *(data as *mut u32) = cap };
        let q = Self::mapping_from_bytes(data);
        unsafe {
            // Due to the previous shmipc specification, the head and tail of the queue is not align
            // to 8 byte boundary
            q.head.write_unaligned(0);
            q.tail.write_unaligned(0);
            (*q.working_flag).store(0, Ordering::SeqCst);
        }
        q
    }

    pub fn mapping_from_bytes(data: *mut u8) -> Self {
        let cap = unsafe { *(data as *mut u32) };
        let queue_start_offset = QUEUE_HEADER_LENGTH;
        let queue_end_offset = QUEUE_HEADER_LENGTH + QUEUE_ELEMENT_LEN * cap as usize;
        #[cfg(target_arch = "aarch64")]
        unsafe {
            Queue {
                cap: cap as i64,
                working_flag: data.offset(4) as *const AtomicU32,
                head: data.offset(8) as *mut i64,
                tail: data.offset(16) as *mut i64,
                queue_bytes_on_memory: data.offset(queue_start_offset as isize) as *const u8,
                len: queue_end_offset - queue_start_offset,
                lock: Mutex::new(()),
            }
        }
        // TODO: unaligned head and tail
        #[cfg(not(target_arch = "aarch64"))]
        unsafe {
            Self {
                cap: cap as i64,
                working_flag: data.offset(20) as *const AtomicU32,
                head: data.offset(4) as *mut i64,
                tail: data.offset(12) as *mut i64,
                queue_bytes_on_memory: data.add(queue_start_offset) as *const u8,
                len: queue_end_offset - queue_start_offset,
                lock: Mutex::new(()),
            }
        }
    }

    pub fn put(&self, element: QueueElement) -> Result<(), Error> {
        let _tail_lock = self.lock.lock().unwrap();
        unsafe {
            if self.tail.read_unaligned() - self.head.read_unaligned() >= self.cap {
                return Err(Error::QueueFull);
            }
            let queue_offset =
                (self.tail.read_unaligned() % self.cap) as isize * QUEUE_ELEMENT_LEN as isize;
            *(self.queue_bytes_on_memory.offset(queue_offset) as *mut u32) = element.seq_id;
            *(self.queue_bytes_on_memory.offset(queue_offset + 4) as *mut u32) =
                element.offset_in_shm_buf;
            *(self.queue_bytes_on_memory.offset(queue_offset + 8) as *mut u32) = element.status;
            self.tail.write_unaligned(self.tail.read_unaligned() + 1);
        };
        Ok(())
    }

    pub fn pop(&self) -> Result<QueueElement, Error> {
        unsafe {
            if self.head.read_unaligned() >= self.tail.read_unaligned() {
                return Err(Error::QueueEmpty);
            }
            let queue_offset =
                (self.head.read_unaligned() % self.cap) as isize * QUEUE_ELEMENT_LEN as isize;
            let element = QueueElement {
                seq_id: *(self.queue_bytes_on_memory.offset(queue_offset) as *const u32),
                offset_in_shm_buf: *(self.queue_bytes_on_memory.offset(queue_offset + 4)
                    as *const u32),
                status: *(self.queue_bytes_on_memory.offset(queue_offset + 8) as *const u32),
            };

            self.head.write_unaligned(self.head.read_unaligned() + 1);
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
        unsafe { self.tail.read_unaligned() - self.head.read_unaligned() }
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
}

fn count_queue_mem_size(queue_cap: u32) -> usize {
    QUEUE_HEADER_LENGTH + QUEUE_ELEMENT_LEN * queue_cap as usize
}

#[cfg(test)]
mod test {
    use std::{sync::Arc, time::Duration};

    use super::{QUEUE_HEADER_LENGTH, Queue};
    use crate::{
        consts::QUEUE_ELEMENT_LEN,
        queue::{QueueElement, QueueManager},
    };

    #[test]
    fn test_queue_manager_create_mapping() {
        let path = "/tmp/ipc1.queue";

        let qm1 = QueueManager::create_with_file(path, 8192).unwrap();
        let qm2 = QueueManager::mapping_with_file(path).unwrap();

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
        let mem_size = QUEUE_HEADER_LENGTH + QUEUE_ELEMENT_LEN * cap as usize;
        let mut mem: Vec<u8> = vec![0u8; mem_size];
        let queue = Queue::create_from_bytes(mem.as_mut_ptr(), cap);
        std::mem::forget(mem);
        queue
    }
}
