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
    ProtocolInitialized, handle_share_memory_by_file_path, send_share_memory_by_file_path,
};
use crate::protocol::{event::EventType, header::Header};

pub enum ProtocolInitializerV2 {
    Client(Client),
    Server(Server),
}

impl ProtocolInitializerV2 {
    pub fn init(&self) -> Result<ProtocolInitialized, anyhow::Error> {
        match self {
            ProtocolInitializerV2::Client(client) => client.init(),
            ProtocolInitializerV2::Server(server) => server.init(),
        }
    }
}

pub struct Client {
    pub(crate) conn_fd: RawFd,
    pub(crate) buffer_path: String,
    pub(crate) queue_path: String,
}

impl Client {
    pub fn init(&self) -> Result<ProtocolInitialized, anyhow::Error> {
        send_share_memory_by_file_path(self.conn_fd, &self.buffer_path, &self.queue_path, 2)?;
        Ok(ProtocolInitialized::legacy(None, 2))
    }
}

pub struct Server {
    pub(crate) conn_fd: RawFd,
    pub(crate) first_event: Header,
}

impl Server {
    pub fn init(&self) -> Result<ProtocolInitialized, anyhow::Error> {
        if self.first_event.msg_type() != EventType::TYPE_SHARE_MEMORY_BY_FILE_PATH {
            return Err(anyhow!(
                "ProtocolInitializerV2 expect first event is:{}({}),but:{}",
                EventType::TYPE_SHARE_MEMORY_BY_FILE_PATH.inner(),
                EventType::TYPE_SHARE_MEMORY_BY_FILE_PATH,
                self.first_event.msg_type().inner()
            ));
        }
        Ok(ProtocolInitialized::legacy(
            handle_share_memory_by_file_path(self.conn_fd, &self.first_event, 2)?,
            2,
        ))
    }
}
