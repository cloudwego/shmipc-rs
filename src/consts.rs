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

use std::time::Duration;

use crate::{buffer::manager::BUFFER_HEADER_SIZE, config::SizePercentPair};

pub const PROTO_VERSION: u8 = 2;
pub const MAX_LEGACY_PROTO_VERSION: u8 = 3;
pub const MAX_SUPPORT_PROTO_VERSION: u8 = 5;
pub const MAGIC_NUMBER: u16 = 0x7758;

#[repr(u8)]
#[derive(Default, Clone, Copy, Debug)]
pub enum MemMapType {
    MemMapTypeDevShmFile = 0,
    #[default]
    MemMapTypeMemFd,
}

#[derive(Default, Clone, Copy, Debug)]
#[repr(u32)]
pub enum StateType {
    #[default]
    DefaultState = 0,
    HotRestartState,
    HotRestartDoneState,
}

pub const MEMFD_CREATE_NAME: &str = "shmipc";
pub const BUFFER_PATH_SUFFIX: &str = "_buffer";

pub const MEMFD_DATA_LEN: usize = 4;
pub const MEMFD_COUNT: usize = 2;
pub const MEMFD_COUNT_EVENTFD: usize = 4;

pub const BUFER_PATH_SUFFIX: &str = "_buffer";
pub const UNIX_NETWORK: &str = "unix";

pub const HOT_RESTART_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
pub const HOT_RESTART_CHECK_INTERVAL: Duration = Duration::from_millis(100);

pub const SESSION_REBUILD_INTERVAL: Duration = Duration::from_secs(60);

pub const EPOCH_ID_LEN: usize = 8;

/// linux file name max length
pub const FILE_NAME_MAX_LEN: usize = 255;
/// The buffer path will concatenate epoch information and end with [_epoch_{epochId uint64}_{randId
/// uint64}], so the maximum length of epoch information is 1+5+1+20+1+20
pub const EPOCH_INFO_MAX_LEN: usize = 7 + 20 + 1 + 20;
/// _queue_{sessionId int}
pub const QUEUE_INFO_MAX_LEN: usize = 7 + 20;

pub const DEFAULT_QUEUE_CAP: u32 = 8192;
pub const DEFAULT_QUEUE_PATH: &str = "/dev/shm/shmipc_queue";
pub const DEFAULT_SHARE_MEMORY_CAP: u32 = 32 * 1024 * 1024;
pub const DEFAULT_SINGLE_BUFFER_SIZE: i64 = 4096;
pub const DEFAULT_BUFFER_SLICE_SIZES: [SizePercentPair; 3] = [
    SizePercentPair {
        size: 8192 - BUFFER_HEADER_SIZE,
        percent: 50,
    },
    SizePercentPair {
        size: 32 * 1024 - BUFFER_HEADER_SIZE,
        percent: 30,
    },
    SizePercentPair {
        size: 128 * 1024 - BUFFER_HEADER_SIZE,
        percent: 20,
    },
];
pub const QUEUE_ELEMENT_LEN: usize = 12;
pub const QUEUE_COUNT: usize = 2;

pub const SIZE_OF_LENGTH: usize = 4;
pub const SIZE_OF_MAGIC: usize = 2;
pub const SIZE_OF_VERSION: usize = 1;
pub const SIZE_OF_TYPE: usize = 1;

pub const HEADER_SIZE: usize = SIZE_OF_LENGTH + SIZE_OF_MAGIC + SIZE_OF_VERSION + SIZE_OF_TYPE;

// V5 Constants
/// How long a V5 consumer waits for a peer that claimed a queue slot to publish it.
pub const DEFAULT_V5_QUEUE_PUBLISH_TIMEOUT: Duration = Duration::from_secs(1);
/// `[offset:8][next:8][len:4][cap:4]`
pub const DYNSHM_BUFFER_HEADER_SIZE: usize = 24;
/// 4 GiB size of the dynamic shared-memory segment.
pub const DYNSHM_TOTAL_SIZE: usize = 4 << 30;
/// 256 KiB max block for the V5 shared-region allocator. A payload a block cannot hold
/// gets a dedicated region instead, carved in multiples of this size.
pub const MAX_BLOCK_SIZE: usize = 256 << 10;
/// Largest frame either side writes to the socket, and the largest a session will read: a
/// fallback payload this big is read into an allocation of its own, and a header claiming
/// more is refused.
pub const MAX_CONTROL_FRAME_SIZE: usize = 256 << 20;
/// Largest message one DynShm chain carries (1 GiB).
pub const MAX_DYN_SHM_TRANSFER_SIZE: usize = 1 << 30;
/// Number of file descriptors sent when eventfd wakeups are enabled.
pub const MEMFD_COUNT_EVENTFD_V5: usize = 3;
/// 16 KiB min block for the V5 shared-region allocator.
pub const MIN_BLOCK_SIZE: usize = 16 << 10;
/// Bytes the V5 ring header occupies ahead of the slot array: the head and tail cache lines.
pub const MPSC_HEADER_SIZE: usize = 128;
/// Granularity every V5 mapping is sized and validated against.
pub const PAGE_SIZE: usize = 4096;
/// Bytes the V5 queue header occupies ahead of the ring: the working flag and its padding.
pub const QUEUE_HEADER_SIZE: usize = 64;
/// 16 MiB per shared region.
pub const SHARED_REGION_SIZE: usize = 16 << 20;
