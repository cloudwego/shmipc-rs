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
    config::Config,
    consts::{BUFFER_PATH_SUFFIX, MemMapType},
    protocol::adapter::{ClientNegotiation, SharedPool},
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

/// Settles the protocol version with the peer.
///
/// The caller runs this before creating any shared memory, so that a version which does not use
/// the shared buffer pool never pays for one.
pub async fn negotiate_client_protocol(
    conn_fd: RawFd,
    config: Config,
    timeout: Duration,
) -> Result<ClientNegotiation, anyhow::Error> {
    tracing::info!("starting negotiates shmipc client protocol");
    let mut handler =
        tokio::task::spawn_blocking(move || ClientProtocolAdapter::negotiate(conn_fd, &config));
    match tokio::time::timeout(timeout, &mut handler).await {
        Ok(res) => {
            let negotiation = res??;
            tracing::info!(
                "shmipc client negotiated protocol version:{}",
                negotiation.selected_protocol()
            );
            Ok(negotiation)
        }
        Err(_) => {
            // SAFETY: caller guarantees `conn_fd` remains a valid socket
            // for the duration of this function.
            unsafe { libc::shutdown(conn_fd, libc::SHUT_RDWR) };
            let _ = handler.await;
            Err(anyhow!("ProtocolInitializer negotiate client timeout"))
        }
    }
}

/// Exchanges shared memory with the peer over the version [`negotiate_client_protocol`] settled on.
///
/// `shared_pool` is what this side publishes, and is `None` exactly when the negotiated version
/// does not use one.
pub async fn init_client_protocol(
    negotiation: ClientNegotiation,
    shared_pool: Option<SharedPool>,
    conn_fd: RawFd,
    config: Config,
    timeout: Duration,
) -> Result<ProtocolInitialized, anyhow::Error> {
    if timeout.is_zero() {
        return Err(anyhow!(
            "ProtocolInitializer init client has no time left in the {:?} handshake budget",
            config.initialize_timeout
        ));
    }
    tracing::info!("starting initializes shmipc client protocol");
    let mut handler = tokio::task::spawn_blocking(move || {
        let adapter = ClientProtocolAdapter::new(conn_fd, config, shared_pool);
        let initializer = adapter.get_initializer(negotiation)?;
        initializer.init()
    });
    match tokio::time::timeout(timeout, &mut handler).await {
        Ok(res) => {
            let initialized = res??;
            tracing::info!(
                "shmipc client protocol initialized, version:{} feature:{:?}",
                initialized.proto_version,
                initialized.feature
            );
            Ok(initialized)
        }
        Err(_) => {
            unsafe { libc::shutdown(conn_fd, libc::SHUT_RDWR) };
            let _ = handler.await;
            Err(anyhow!("ProtocolInitializer init client timeout"))
        }
    }
}

pub async fn init_server_protocol(
    conn_fd: RawFd,
    config: Config,
    timeout: Duration,
) -> Result<ProtocolInitialized, anyhow::Error> {
    tracing::info!("starting initializes shmipc server protocol");
    let mut handler = tokio::task::spawn_blocking(move || {
        let adapter = ServerProtocolAdapter::new(conn_fd, config);
        let initializer = adapter.get_initializer()?;
        initializer.init()
    });
    match tokio::time::timeout(timeout, &mut handler).await {
        Ok(res) => {
            let initialized = res??;
            tracing::info!(
                "shmipc server protocol initialized, version:{} feature:{:?}",
                initialized.proto_version,
                initialized.feature
            );
            Ok(initialized)
        }
        Err(_) => {
            unsafe { libc::shutdown(conn_fd, libc::SHUT_RDWR) };
            let _ = handler.await;
            Err(anyhow!("ProtocolInitializer init server timeout"))
        }
    }
}
