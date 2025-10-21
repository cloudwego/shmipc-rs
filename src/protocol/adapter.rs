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
        wait_event_header,
    },
    protocol_trace,
};
use crate::consts::{HEADER_SIZE, MAX_SUPPORT_PROTO_VERSION, MemMapType};

pub struct ClientProtocolAdapter {
    conn_fd: RawFd,
    mem_map_type: MemMapType,
    buffer_path: String,
    queue_path: String,
    buffer_fd: RawFd,
    queue_fd: RawFd,
}

impl ClientProtocolAdapter {
    pub const fn new(
        conn_fd: RawFd,
        mem_map_type: MemMapType,
        buffer_path: String,
        queue_path: String,
        buffer_fd: RawFd,
        queue_fd: RawFd,
    ) -> Self {
        Self {
            conn_fd,
            mem_map_type,
            buffer_path,
            queue_path,
            buffer_fd,
            queue_fd,
        }
    }

    pub fn get_initializer(self) -> Result<ProtocolInitializer, anyhow::Error> {
        // temporarily ensure version compatibility.
        if let MemMapType::MemMapTypeDevShmFile = self.mem_map_type {
            return Ok(ProtocolInitializer::V2(ProtocolInitializerV2::Client(
                V2Client {
                    conn_fd: self.conn_fd,
                    buffer_path: self.buffer_path,
                    queue_path: self.queue_path,
                },
            )));
        }
        // send version to peer
        let mut h = Header([0; HEADER_SIZE].as_mut_ptr());
        let client_version = MAX_SUPPORT_PROTO_VERSION;
        h.encode(
            HEADER_SIZE as u32,
            client_version,
            EventType::TYPE_EXCHANGE_PROTO_VERSION,
        );
        protocol_trace(&h, &[], true);
        block_write_full(self.conn_fd, unsafe {
            std::slice::from_raw_parts(h.0, HEADER_SIZE)
        })?;
        // recv peer's version
        let mut buf = [0u8; HEADER_SIZE];
        let recv_header = wait_event_header(
            self.conn_fd,
            EventType::TYPE_EXCHANGE_PROTO_VERSION,
            &mut buf,
        )
        .map_err(|err| {
            anyhow!(
                "protrocol_initializer_v3 client_init failed, reason:{}",
                err
            )
        })?;
        let server_version = recv_header.version();
        match std::cmp::min(client_version, server_version) {
            2 => Ok(ProtocolInitializer::V2(ProtocolInitializerV2::Client(
                V2Client {
                    conn_fd: self.conn_fd,
                    buffer_path: self.buffer_path,
                    queue_path: self.queue_path,
                },
            ))),
            3 => Ok(ProtocolInitializer::V3(ProtocolInitializerV3::Client(
                V3Client {
                    conn_fd: self.conn_fd,
                    mem_map_type: self.mem_map_type,
                    buffer_fd: self.buffer_fd,
                    queue_fd: self.queue_fd,
                    buffer_path: self.buffer_path,
                    queue_path: self.queue_path,
                },
            ))),
            version => Err(anyhow!(
                "not support the protocol version:{}, max_support_version is {}",
                version,
                MAX_SUPPORT_PROTO_VERSION
            )),
        }
    }
}

pub struct ServerProtocolAdapter {
    conn_fd: RawFd,
}

impl ServerProtocolAdapter {
    pub const fn new(conn_fd: RawFd) -> Self {
        Self { conn_fd }
    }

    pub fn get_initializer(self) -> Result<ProtocolInitializer, anyhow::Error> {
        // ensure version compatibility
        let mut buf = vec![0u8; HEADER_SIZE];
        let h = block_read_event_header(self.conn_fd, &mut buf)?;
        std::mem::forget(buf);
        match h.version() {
            2 => Ok(ProtocolInitializer::V2(ProtocolInitializerV2::Server(
                V2Server {
                    conn_fd: self.conn_fd,
                    first_event: h,
                },
            ))),
            3 => Ok(ProtocolInitializer::V3(ProtocolInitializerV3::Server(
                V3Server {
                    conn_fd: self.conn_fd,
                    first_event: h,
                },
            ))),
            version => Err(anyhow!(
                "not support the protocol version:{}, max_support_version is {}",
                version,
                MAX_SUPPORT_PROTO_VERSION
            )),
        }
    }
}
