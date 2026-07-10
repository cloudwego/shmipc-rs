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

pub mod adapter;
pub mod block_io;
pub mod event;
pub mod event_fd;
pub mod header;
pub mod initializer;
pub mod negotiation;

use std::{
    os::fd::RawFd,
    sync::{Arc, LazyLock},
    time::Duration,
};

use adapter::{ClientProtocolAdapter, ServerProtocolAdapter};
use anyhow::anyhow;
use header::Header;
use initializer::ProtocolInitialized;

use crate::{
    buffer::manager::BufferManager,
    config::{Config, ProtocolMode},
    consts::{BUFFER_PATH_SUFFIX, MemMapType},
    queue::QueueManager,
};

pub static PROTOCOL_TRACE_MODE: LazyLock<bool> =
    LazyLock::new(|| match std::env::var("SHMIPC_PROTOCOL_TRACE") {
        Ok(val) => !val.is_empty(),
        Err(_) => false,
    });

pub fn protocol_trace(h: &Header, body: &[u8], send: bool) {
    if !*PROTOCOL_TRACE_MODE {
        return;
    }
    if send {
        tracing::warn!("send, header:{} body:{}", h, String::from_utf8_lossy(body));
    } else {
        tracing::warn!("recv, header:{} body:{}", h, String::from_utf8_lossy(body));
    }
}

pub fn init_manager(
    config: &mut Config,
    proto_version: u8,
) -> Result<(Arc<BufferManager>, QueueManager), anyhow::Error> {
    Ok(match config.mem_map_type {
        MemMapType::MemMapTypeDevShmFile => {
            let bm = match BufferManager::get_with_file(
                &format!("{}{}", config.share_memory_path_prefix, BUFFER_PATH_SUFFIX),
                config.share_memory_buffer_cap,
                true,
                &mut config.buffer_slice_sizes,
            ) {
                Ok(manager) => manager,
                Err(err) => {
                    _ = std::fs::remove_dir(format!(
                        "{}{}",
                        config.share_memory_path_prefix, BUFFER_PATH_SUFFIX
                    ));
                    return Err(anyhow!(
                        "create share memory buffer manager failed, error={}",
                        err
                    ));
                }
            };
            let qm = match QueueManager::create_with_file(
                &config.queue_path,
                config.queue_cap,
                proto_version,
            ) {
                Ok(manager) => manager,
                Err(err) => {
                    _ = std::fs::remove_dir(&config.queue_path);
                    return Err(anyhow!(
                        "create share memory queue manager failed, error={}",
                        err
                    ));
                }
            };
            (bm, qm)
        }
        MemMapType::MemMapTypeMemFd => {
            let bm = match BufferManager::get_with_memfd(
                &format!("{}{}", config.share_memory_path_prefix, BUFFER_PATH_SUFFIX),
                0,
                config.share_memory_buffer_cap,
                true,
                &mut config.buffer_slice_sizes,
            ) {
                Ok(manager) => manager,
                Err(err) => {
                    return Err(anyhow!(
                        "create share memory buffer manager failed, error={}",
                        err
                    ));
                }
            };
            let qm = match QueueManager::create_with_memfd(
                &config.queue_path,
                config.queue_cap,
                proto_version,
            ) {
                Ok(manager) => manager,
                Err(err) => {
                    return Err(anyhow!(
                        "create share memory queue manager failed, error={}",
                        err
                    ));
                }
            };
            (bm, qm)
        }
    })
}

pub const fn initial_client_proto_version(config: &Config) -> u8 {
    match config.protocol.mode {
        ProtocolMode::Legacy => match config.mem_map_type {
            MemMapType::MemMapTypeDevShmFile => 2,
            MemMapType::MemMapTypeMemFd => 3,
        },
        ProtocolMode::V4 { .. } => 4,
    }
}

pub async fn init_client_protocol(
    buffer_path: String,
    buffer_memfd: RawFd,
    queue_path: String,
    queue_memfd: RawFd,
    conn_fd: RawFd,
    config: Config,
    timeout: Duration,
) -> Result<ProtocolInitialized, anyhow::Error> {
    tracing::info!("starting initializes shmipc client protocol");
    let handler = tokio::task::spawn_blocking(move || {
        let adapter = ClientProtocolAdapter::new(
            conn_fd,
            config,
            buffer_path,
            queue_path,
            buffer_memfd,
            queue_memfd,
        );
        let initializer = adapter.get_initializer()?;
        initializer.init()
    });
    match tokio::time::timeout(timeout, std::pin::pin!(handler)).await {
        Ok(res) => res?,
        Err(_) => Err(anyhow!(
            "ProtocolInitializer init client timeout:{:?} ms",
            timeout
        )),
    }
}

pub async fn init_server_protocol(
    conn_fd: RawFd,
    timeout: Duration,
) -> Result<ProtocolInitialized, anyhow::Error> {
    tracing::info!("starting initializes shmipc server protocol");
    let handler = tokio::task::spawn_blocking(move || {
        let adapter = ServerProtocolAdapter::new(conn_fd);
        let initializer = adapter.get_initializer()?;
        initializer.init()
    });
    match tokio::time::timeout(timeout, std::pin::pin!(handler)).await {
        Ok(res) => res?,
        Err(_) => Err(anyhow!(
            "ProtocolInitializer init server timeout:{:?} ms",
            timeout
        )),
    }
}
