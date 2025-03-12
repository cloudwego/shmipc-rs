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

use std::{os::fd::RawFd, sync::Arc};

use anyhow::anyhow;

use super::{
    block_read_event_header, block_write_full, handle_exchange_version,
    handle_share_memory_by_file_path, handle_share_memory_by_memfd, send_memfd_to_peer,
    send_share_memory_by_file_path, wait_event_header,
};
use crate::{
    buffer::manager::BufferManager,
    consts::{HEADER_SIZE, MemMapType},
    protocol::{event::EventType, header::Header, protocol_trace},
    queue::QueueManager,
};

pub enum ProtocolInitializerV3 {
    Client(Client),
    Server(Server),
}

impl ProtocolInitializerV3 {
    pub fn init(&self) -> Result<Option<(Arc<BufferManager>, QueueManager)>, anyhow::Error> {
        match self {
            ProtocolInitializerV3::Client(client) => client.init(),
            ProtocolInitializerV3::Server(server) => server.init(),
        }
    }

    pub fn version() -> u8 {
        3
    }
}

pub struct Client {
    pub(crate) conn_fd: RawFd,
    pub(crate) mem_map_type: MemMapType,
    pub(crate) buffer_path: String,
    pub(crate) queue_path: String,
    pub(crate) buffer_fd: RawFd,
    pub(crate) queue_fd: RawFd,
}

impl Client {
    pub fn init(&self) -> Result<Option<(Arc<BufferManager>, QueueManager)>, anyhow::Error> {
        match self.mem_map_type {
            MemMapType::MemMapTypeDevShmFile => {
                send_share_memory_by_file_path(self.conn_fd, &self.buffer_path, &self.queue_path, 3)
            }
            MemMapType::MemMapTypeMemFd => send_memfd_to_peer(
                self.conn_fd,
                &self.buffer_path,
                self.buffer_fd,
                &self.queue_path,
                self.queue_fd,
                3,
            ),
        }?;
        let mut buf = [0u8; HEADER_SIZE];
        wait_event_header(self.conn_fd, EventType::TYPE_ACK_SHARE_MEMORY, &mut buf)?;
        Ok(None)
    }
}

pub struct Server {
    pub(crate) conn_fd: RawFd,
    pub(crate) first_event: Header,
}

impl Server {
    pub fn init(&self) -> Result<Option<(Arc<BufferManager>, QueueManager)>, anyhow::Error> {
        if EventType::TYPE_EXCHANGE_PROTO_VERSION != self.first_event.msg_type() {
            return Err(anyhow!(
                "ProtocolInitializerV3 expect first event is:{}({}),but:{}",
                EventType::TYPE_EXCHANGE_PROTO_VERSION.inner(),
                EventType::TYPE_EXCHANGE_PROTO_VERSION,
                self.first_event.msg_type().inner()
            ));
        }

        // 1. exchange version
        handle_exchange_version(self.conn_fd).map_err(|err| {
            anyhow!(
                "ProtocolInitializerV3 exchange_version failed, reason:{}",
                err
            )
        })?;

        // 2. recv and mapping share memory
        let mut buf = [0u8; HEADER_SIZE];
        let h = block_read_event_header(self.conn_fd, &mut buf).map_err(|err| {
            anyhow!(
                "ProtocolInitializerV3 block_read_event_header failed, reason:{}",
                err
            )
        })?;
        let r = match h.msg_type() {
            EventType::TYPE_SHARE_MEMORY_BY_FILE_PATH => {
                handle_share_memory_by_file_path(self.conn_fd, &h)
            }
            EventType::TYPE_SHARE_MEMORY_BY_MEMFD => {
                handle_share_memory_by_memfd(self.conn_fd, &h, 3)
            }
            _ => {
                return Err(anyhow!(
                    "expect event type is TypeShareMemoryByFilePath or TypeShareMemoryByMemfd, \
                     but:{}({})",
                    h.msg_type().inner(),
                    h.msg_type()
                ));
            }
        }?;
        // 3. ack share memory
        let mut resp_header = Header([0u8; HEADER_SIZE].as_mut_ptr());
        resp_header.encode(HEADER_SIZE as u32, 3, EventType::TYPE_ACK_SHARE_MEMORY);
        protocol_trace(&resp_header, &[], true);
        block_write_full(self.conn_fd, unsafe {
            std::slice::from_raw_parts(resp_header.0, HEADER_SIZE)
        })?;
        Ok(r)
    }
}
