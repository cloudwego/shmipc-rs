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

pub const PROTO_VERSION: u8 = 2;
pub const MAX_SUPPORT_PROTO_VERSION: u8 = 3;
pub const MAGIC_NUMBER: u16 = 0x7758;

#[repr(u8)]
#[derive(Default, Clone, Copy, Debug)]
pub enum MemMapType {
    #[default]
    MemMapTypeDevShmFile = 0,
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
pub const DEFAULT_SHARE_MEMORY_CAP: u32 = 32 * 1024 * 1024;
pub const DEFAULT_SINGLE_BUFFER_SIZE: i64 = 4096;
pub const QUEUE_ELEMENT_LEN: usize = 12;
pub const QUEUE_COUNT: usize = 2;

pub const SIZE_OF_LENGTH: usize = 4;
pub const SIZE_OF_MAGIC: usize = 2;
pub const SIZE_OF_VERSION: usize = 1;
pub const SIZE_OF_TYPE: usize = 1;

pub const HEADER_SIZE: usize = SIZE_OF_LENGTH + SIZE_OF_MAGIC + SIZE_OF_VERSION + SIZE_OF_TYPE;
