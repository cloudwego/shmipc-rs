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

use std::{fmt::Debug, time::Duration};

use anyhow::anyhow;

use crate::{
    buffer::manager::BUFFER_HEADER_SIZE,
    consts::{DEFAULT_QUEUE_CAP, DEFAULT_SHARE_MEMORY_CAP, MemMapType, SESSION_REBUILD_INTERVAL},
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SizePercentPair {
    pub size: u32,
    pub percent: u32,
}

#[derive(Clone, Debug)]
/// Config is used to tune the shmipc session
pub struct Config {
    /// connection_write_timeout is meant to be a "safety value" timeout after
    /// which we will suspect a problem with the underlying connection and
    /// close it. This is only applied to writes, where there's generally
    /// an expectation that things will move along quickly.
    pub connection_write_timeout: Duration,

    pub connection_read_timeout: Option<Duration>,

    pub connection_timeout: Option<Duration>,

    /// initialize_timeout is meant timeout during server and client exchange config phase
    pub initialize_timeout: Duration,

    /// the max number of pending request
    pub queue_cap: u32,

    /// share memory path of the underlying queue
    pub queue_path: String,

    /// the capacity of buffer in share memory
    pub share_memory_buffer_cap: u32,

    /// the share memory path prefix of buffer
    pub share_memory_path_prefix: String,

    /// guess request or response's size for improving performance, and the default value is 4096
    pub buffer_slice_sizes: Vec<SizePercentPair>,

    /// mmap map type, MemMapTypeDevShmFile or MemMapTypeMemFd (server set)
    pub mem_map_type: MemMapType,

    /// client rebuild session interval
    pub rebuild_interval: Duration,

    pub max_stream_num: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    pub fn new() -> Self {
        Self {
            connection_write_timeout: Duration::from_secs(10),
            connection_read_timeout: None,
            connection_timeout: None,
            initialize_timeout: Duration::from_millis(1000),
            queue_cap: DEFAULT_QUEUE_CAP,
            queue_path: "/dev/shm/shmipc_queue".to_owned(),
            share_memory_buffer_cap: DEFAULT_SHARE_MEMORY_CAP,
            share_memory_path_prefix: "/dev/shm/shmipc".to_owned(),
            buffer_slice_sizes: vec![
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
            ],
            mem_map_type: MemMapType::MemMapTypeMemFd,
            rebuild_interval: SESSION_REBUILD_INTERVAL,
            max_stream_num: 4096,
        }
    }

    pub fn verify(&self) -> Result<(), anyhow::Error> {
        if self.share_memory_buffer_cap < (1 << 20) {
            return Err(anyhow!(
                "share memory size is too small:{}, must greater than {}",
                self.share_memory_buffer_cap,
                1 << 20
            ));
        }
        if self.buffer_slice_sizes.is_empty() {
            return Err(anyhow!("buffer_slice_sizes could not be nil"));
        }

        let mut sum = 0;
        for pair in self.buffer_slice_sizes.iter() {
            sum += pair.percent;
            if pair.size > self.share_memory_buffer_cap {
                return Err(anyhow!(
                    "buffer_slice_sizes's size:{} couldn't greater than share_memory_buffer_cap:{}",
                    pair.size,
                    self.share_memory_buffer_cap
                ));
            }

            #[cfg(any(target_arch = "arm", target_arch = "arm64ec"))]
            if pair.size % 4 != 0 {
                return Err(anyhow!(
                    "the size_percent_pair.size must be a multiple of 4"
                ));
            }
        }

        if sum != 100 {
            return Err(anyhow!(
                "the sum of buffer_slice_sizes's percent should be 100",
            ));
        }

        #[cfg(any(target_arch = "arm", target_arch = "arm64ec"))]
        if self.queue_cap % 8 != 0 {
            return Err(anyhow!("the queue_cap must be a multiple of 8"));
        }

        if self.share_memory_path_prefix.is_empty() || self.queue_path.is_empty() {
            return Err(anyhow!("buffer path or queue path could not be nil"));
        }

        #[cfg(not(target_os = "linux"))]
        {
            return Err(anyhow!("shmipc just support linux OS now"));
        }

        #[cfg(not(any(target_arch = "x86_64", target_arch = "arm64ec")))]
        {
            return Err(anyhow!("shmipc just support amd64 or arm64 arch"));
        }

        Ok(())
    }
}
