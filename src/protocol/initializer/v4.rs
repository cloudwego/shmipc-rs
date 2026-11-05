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

use std::os::fd::{AsRawFd, FromRawFd, RawFd};

use anyhow::anyhow;

use super::{
    ProtocolInitialized, SharedMemoryMode::SharedPool, block_read_event_header, block_write_full,
    handle_share_memory_by_file_path, handle_share_memory_by_memfd_with_expected,
    send_memfd_to_peer_with_fds, send_share_memory_by_file_path, wait_event_header,
};
use crate::{
    config::Config,
    consts::{HEADER_SIZE, MEMFD_COUNT, MEMFD_COUNT_EVENTFD, MemMapType},
    protocol::{
        event::EventType,
        event_fd::{EventFdPair, create_eventfd},
        header::Header,
        negotiation::{Negotiated, NegotiatedFeature, PROTO_VERSION_V4},
        protocol_trace,
    },
};

pub enum ProtocolInitializerV4 {
    Client(Box<Client>),
    Server(Server),
}

impl ProtocolInitializerV4 {
    pub fn init(&self) -> Result<ProtocolInitialized, anyhow::Error> {
        match self {
            ProtocolInitializerV4::Client(client) => client.init(),
            ProtocolInitializerV4::Server(server) => server.init(),
        }
    }
}

pub struct Client {
    pub(crate) conn_fd: RawFd,
    pub(crate) config: Config,
    pub(crate) buffer_path: String,
    pub(crate) queue_path: String,
    pub(crate) buffer_fd: RawFd,
    pub(crate) queue_fd: RawFd,
    pub(crate) negotiated: Negotiated,
}

impl Client {
    pub fn init(&self) -> Result<ProtocolInitialized, anyhow::Error> {
        let Negotiated {
            message_version,
            feature,
        } = self.negotiated;

        let eventfd = if feature == NegotiatedFeature::EventFd {
            Some(EventFdPair {
                wakeup_send: create_eventfd()?,
                wakeup_recv: create_eventfd()?,
            })
        } else {
            None
        };

        match self.config.mem_map_type {
            MemMapType::MemMapTypeDevShmFile => {
                send_share_memory_by_file_path(
                    self.conn_fd,
                    &self.buffer_path,
                    &self.queue_path,
                    message_version,
                )?;
            }
            MemMapType::MemMapTypeMemFd => {
                let extra_fds = eventfd
                    .as_ref()
                    .map(|eventfd| {
                        vec![
                            eventfd.wakeup_send.as_raw_fd(),
                            eventfd.wakeup_recv.as_raw_fd(),
                        ]
                    })
                    .unwrap_or_default();
                send_memfd_to_peer_with_fds(
                    self.conn_fd,
                    &self.buffer_path,
                    self.buffer_fd,
                    &self.queue_path,
                    self.queue_fd,
                    &extra_fds,
                    message_version,
                )?;
            }
        }

        let mut buf = [0u8; HEADER_SIZE];
        wait_event_header(self.conn_fd, EventType::TYPE_ACK_SHARE_MEMORY, &mut buf)?;
        Ok(ProtocolInitialized::new(
            SharedPool(None),
            PROTO_VERSION_V4,
            message_version,
            feature,
            eventfd,
        ))
    }
}

pub struct Server {
    pub(crate) conn_fd: RawFd,
    pub(crate) negotiated: Negotiated,
}

impl Server {
    pub fn init(&self) -> Result<ProtocolInitialized, anyhow::Error> {
        let Negotiated {
            message_version,
            feature,
        } = self.negotiated;

        let mut buf = [0u8; HEADER_SIZE];
        let h = block_read_event_header(self.conn_fd, &mut buf).map_err(|err| {
            anyhow!(
                "ProtocolInitializerV4 block_read_event_header failed, reason:{}",
                err
            )
        })?;

        if feature == NegotiatedFeature::EventFd
            && h.msg_type() != EventType::TYPE_SHARE_MEMORY_BY_MEMFD
        {
            return Err(anyhow!("eventfd_wakeup requires memfd shared memory"));
        }

        let (shared_memory, eventfd) = match h.msg_type() {
            EventType::TYPE_SHARE_MEMORY_BY_FILE_PATH => (
                handle_share_memory_by_file_path(self.conn_fd, &h, PROTO_VERSION_V4)?,
                None,
            ),
            EventType::TYPE_SHARE_MEMORY_BY_MEMFD => {
                let expected_fd_count = if feature == NegotiatedFeature::EventFd {
                    MEMFD_COUNT_EVENTFD
                } else {
                    MEMFD_COUNT
                };
                let (bm, qm, fds) = handle_share_memory_by_memfd_with_expected(
                    self.conn_fd,
                    &h,
                    PROTO_VERSION_V4,
                    message_version,
                    expected_fd_count,
                )?;
                let eventfd = if feature == NegotiatedFeature::EventFd {
                    // SAFETY: these fds were just received via SCM_RIGHTS and are uniquely owned.
                    let wakeup_recv = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[2]) };
                    // SAFETY: these fds were just received via SCM_RIGHTS and are uniquely owned.
                    let wakeup_send = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[3]) };
                    Some(EventFdPair {
                        wakeup_send,
                        wakeup_recv,
                    })
                } else {
                    None
                };
                (Some((bm, qm)), eventfd)
            }
            _ => {
                return Err(anyhow!(
                    "expect event type is TypeShareMemoryByFilePath or TypeShareMemoryByMemfd, \
                     but:{}({})",
                    h.msg_type().inner(),
                    h.msg_type()
                ));
            }
        };

        let mut resp_buf = [0u8; HEADER_SIZE];
        let mut resp_header = Header(resp_buf.as_mut_ptr());
        resp_header.encode(
            HEADER_SIZE as u32,
            message_version,
            EventType::TYPE_ACK_SHARE_MEMORY,
        );
        protocol_trace(&resp_header, &[], true);
        block_write_full(self.conn_fd, &resp_buf)?;

        Ok(ProtocolInitialized::new(
            SharedPool(shared_memory),
            PROTO_VERSION_V4,
            message_version,
            feature,
            eventfd,
        ))
    }
}
