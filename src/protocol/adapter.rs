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

use std::os::fd::RawFd;

use anyhow::anyhow;

use super::{
    block_io::block_write_full,
    event::EventType,
    header::Header,
    initializer::{
        ProtocolInitializer, block_read_event_header,
        v2::{Client as V2Client, ProtocolInitializerV2, Server as V2Server},
        v3::{Client as V3Client, ProtocolInitializerV3, Server as V3Server},
        v4::{Client as V4Client, ProtocolInitializerV4, Server as V4Server},
        v5::{Client as V5Client, ProtocolInitializerV5, Server as V5Server},
        wait_event_header,
    },
    protocol_trace,
};
use crate::{
    config::{Config, ProtocolMode},
    consts::{HEADER_SIZE, MAX_LEGACY_PROTO_VERSION, MAX_SUPPORT_PROTO_VERSION, MemMapType},
    protocol::negotiation::{
        Negotiated, PROTO_VERSION_V4, PROTO_VERSION_V5, negotiate_client, negotiate_server,
    },
};

/// The paths and descriptors of the buffer and queue managers this client publishes to its peer.
///
/// Only the versions that read and write through one shared pool have these. V5 gives each
/// endpoint its own segment, so a V5 client never creates them and never sends them.
pub struct SharedPool {
    pub buffer_path: String,
    pub queue_path: String,
    pub buffer_fd: RawFd,
    pub queue_fd: RawFd,
}

/// The version this client and its peer settled on, and whatever settling on it carried.
pub struct ClientNegotiation {
    selected_protocol: u8,
    /// Carried by the V4 and V5 negotiation only: the older paths have no features to report.
    negotiated: Option<Negotiated>,
}

impl ClientNegotiation {
    /// The version both sides settled on.
    pub const fn selected_protocol(&self) -> u8 {
        self.selected_protocol
    }

    /// Whether the selected version reads and writes through a shared buffer pool the client has
    /// to create and publish. V5 gives each endpoint its own segment and needs none.
    pub const fn needs_shared_pool(&self) -> bool {
        self.selected_protocol != PROTO_VERSION_V5
    }
}

/// Builds the client-side initializer for whatever version negotiation settled on.
pub struct ClientProtocolAdapter {
    conn_fd: RawFd,
    config: Config,
    /// `None` when the negotiated version does not use a shared pool.
    shared_pool: Option<SharedPool>,
}

impl ClientProtocolAdapter {
    /// `shared_pool` is what this side publishes, and is `None` exactly when
    /// [`ClientNegotiation::needs_shared_pool`] said none was needed.
    pub const fn new(conn_fd: RawFd, config: Config, shared_pool: Option<SharedPool>) -> Self {
        Self {
            conn_fd,
            config,
            shared_pool,
        }
    }

    /// Builds the initializer for the already-negotiated version.
    pub fn get_initializer(
        self,
        negotiation: ClientNegotiation,
    ) -> Result<ProtocolInitializer, anyhow::Error> {
        let ClientNegotiation {
            selected_protocol,
            negotiated,
        } = negotiation;

        let pool = || {
            self.shared_pool.ok_or_else(|| {
                anyhow!(
                    "protocol version {} needs a shared buffer pool, but none was created",
                    selected_protocol
                )
            })
        };
        let feature = || {
            negotiated.ok_or_else(|| {
                anyhow!(
                    "protocol version {} needs a negotiation result, but none was carried",
                    selected_protocol
                )
            })
        };

        match selected_protocol {
            PROTO_VERSION_V5 => Ok(ProtocolInitializer::V5(ProtocolInitializerV5::Client(
                Box::new(V5Client {
                    conn_fd: self.conn_fd,
                    config: self.config,
                    negotiated: feature()?,
                }),
            ))),
            PROTO_VERSION_V4 => {
                let negotiated = feature()?;
                let pool = pool()?;
                Ok(ProtocolInitializer::V4(ProtocolInitializerV4::Client(
                    Box::new(V4Client {
                        conn_fd: self.conn_fd,
                        config: self.config,
                        buffer_path: pool.buffer_path,
                        queue_path: pool.queue_path,
                        buffer_fd: pool.buffer_fd,
                        queue_fd: pool.queue_fd,
                        negotiated,
                    }),
                )))
            }
            3 => {
                let mem_map_type = self.config.mem_map_type;
                let pool = pool()?;
                Ok(ProtocolInitializer::V3(ProtocolInitializerV3::Client(
                    V3Client {
                        conn_fd: self.conn_fd,
                        mem_map_type,
                        buffer_fd: pool.buffer_fd,
                        queue_fd: pool.queue_fd,
                        buffer_path: pool.buffer_path,
                        queue_path: pool.queue_path,
                    },
                )))
            }
            2 => {
                let pool = pool()?;
                Ok(ProtocolInitializer::V2(ProtocolInitializerV2::Client(
                    V2Client {
                        conn_fd: self.conn_fd,
                        buffer_path: pool.buffer_path,
                        queue_path: pool.queue_path,
                    },
                )))
            }
            version => Err(anyhow!(
                "negotiation selected the unsupported protocol version:{}",
                version
            )),
        }
    }

    /// Settles the protocol version with the peer, before any shared memory exists.
    ///
    /// Split out from [`Self::get_initializer`] so the caller can see the selected version before
    /// deciding what to allocate.
    pub fn negotiate(conn_fd: RawFd, config: &Config) -> Result<ClientNegotiation, anyhow::Error> {
        if matches!(
            config.protocol.mode,
            ProtocolMode::V4 { .. } | ProtocolMode::V5 { .. }
        ) {
            let (response, feature) = negotiate_client(conn_fd, config)?;
            return Ok(ClientNegotiation {
                selected_protocol: response.selected_protocol,
                negotiated: Some(Negotiated::new(feature)),
            });
        }

        // temporarily ensure version compatibility.
        if let MemMapType::MemMapTypeDevShmFile = config.mem_map_type {
            return Ok(ClientNegotiation {
                selected_protocol: 2,
                negotiated: None,
            });
        }
        // send version to peer
        let mut header_buf = [0u8; HEADER_SIZE];
        let mut h = Header(header_buf.as_mut_ptr());
        let client_version = MAX_LEGACY_PROTO_VERSION;
        h.encode(
            HEADER_SIZE as u32,
            client_version,
            EventType::TYPE_EXCHANGE_PROTO_VERSION,
        );
        protocol_trace(&h, &[], true);
        block_write_full(conn_fd, &header_buf)?;
        // recv peer's version
        let mut buf = [0u8; HEADER_SIZE];
        let recv_header =
            wait_event_header(conn_fd, EventType::TYPE_EXCHANGE_PROTO_VERSION, &mut buf).map_err(
                |err| {
                    anyhow!(
                        "protrocol_initializer_v3 client_init failed, reason:{}",
                        err
                    )
                },
            )?;
        let server_version = recv_header.version();
        let selected_protocol = std::cmp::min(client_version, server_version);
        if !matches!(selected_protocol, 2 | 3) {
            return Err(anyhow!(
                "not support the protocol version:{}, max_support_version is {}",
                selected_protocol,
                MAX_SUPPORT_PROTO_VERSION
            ));
        }
        Ok(ClientNegotiation {
            selected_protocol,
            negotiated: None,
        })
    }
}

/// Builds the server-side initializer from the first event a client sends, which is what names
/// the protocol family in use.
pub struct ServerProtocolAdapter {
    conn_fd: RawFd,
    config: Config,
}

impl ServerProtocolAdapter {
    pub const fn new(conn_fd: RawFd, config: Config) -> Self {
        Self { conn_fd, config }
    }

    /// Reads the client's first event and dispatches on it.
    ///
    /// V2 and V3 announce themselves by the event type alone; anything negotiated arrives as
    /// [`EventType::TYPE_NEGOTIATION`] and the version comes out of the exchange itself.
    pub fn get_initializer(self) -> Result<ProtocolInitializer, anyhow::Error> {
        // ensure version compatibility
        let mut buf = vec![0u8; HEADER_SIZE];
        let h = block_read_event_header(self.conn_fd, &mut buf)?;
        std::mem::forget(buf);
        match h.msg_type() {
            EventType::TYPE_SHARE_MEMORY_BY_FILE_PATH => Ok(ProtocolInitializer::V2(
                ProtocolInitializerV2::Server(V2Server {
                    conn_fd: self.conn_fd,
                    first_event: h,
                }),
            )),
            EventType::TYPE_EXCHANGE_PROTO_VERSION => Ok(ProtocolInitializer::V3(
                ProtocolInitializerV3::Server(V3Server {
                    conn_fd: self.conn_fd,
                    first_event: h,
                }),
            )),
            EventType::TYPE_NEGOTIATION => self.negotiated_initializer(&h),
            msg_type => Err(anyhow!(
                "not support the first protocol message type:{}, max_support_version is {}",
                msg_type,
                MAX_SUPPORT_PROTO_VERSION
            )),
        }
    }

    /// Completes the JSON negotiation and builds the initializer for the version it selected.
    fn negotiated_initializer(
        self,
        first_event: &Header,
    ) -> Result<ProtocolInitializer, anyhow::Error> {
        let (response, feature) = negotiate_server(self.conn_fd, first_event)?;
        let negotiated = Negotiated::new(feature);

        match response.selected_protocol {
            PROTO_VERSION_V5 => Ok(ProtocolInitializer::V5(ProtocolInitializerV5::Server(
                Box::new(V5Server {
                    conn_fd: self.conn_fd,
                    config: self.config,
                    negotiated,
                }),
            ))),
            PROTO_VERSION_V4 => Ok(ProtocolInitializer::V4(ProtocolInitializerV4::Server(
                V4Server {
                    conn_fd: self.conn_fd,
                    negotiated,
                },
            ))),
            version => Err(anyhow!(
                "negotiation selected the unsupported protocol version:{}",
                version
            )),
        }
    }
}
