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
use serde::Serialize;

use super::{
    ProtocolInitialized, block_read_event_header, block_read_full, block_write_full,
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
        negotiation::{
            MSG_VERSION_V4, NegotiatedFeature, NegotiationRequest, NegotiationResponse,
            PROTO_VERSION_V4, select_feature,
        },
        protocol_trace,
    },
};

pub enum ProtocolInitializerV4 {
    Client(Box<Client>),
    Server(Server),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum V4ClientInitError {
    #[error("v4 negotiation failed due to network/protocol error: {source}")]
    NetworkOrProtocol {
        #[source]
        source: anyhow::Error,
    },
    #[error("v4 negotiation rejected: {0}")]
    Rejected(crate::protocol::negotiation::NegotiationError),
    #[error("v4 negotiation returned unsupported protocol {0}")]
    UnsupportedProtocol(u8),
    #[error("v4 negotiation response is invalid: {source}")]
    InvalidResponse {
        #[source]
        source: anyhow::Error,
    },
}

impl V4ClientInitError {
    pub(crate) fn network_or_protocol(err: impl Into<anyhow::Error>) -> anyhow::Error {
        anyhow::Error::new(Self::NetworkOrProtocol { source: err.into() })
    }

    pub(crate) fn rejected(err: crate::protocol::negotiation::NegotiationError) -> anyhow::Error {
        anyhow::Error::new(Self::Rejected(err))
    }

    pub(crate) fn unsupported_protocol(version: u8) -> anyhow::Error {
        anyhow::Error::new(Self::UnsupportedProtocol(version))
    }

    pub(crate) fn invalid_response(err: impl Into<anyhow::Error>) -> anyhow::Error {
        anyhow::Error::new(Self::InvalidResponse { source: err.into() })
    }

    pub(crate) const fn is_network_or_protocol(&self) -> bool {
        matches!(self, Self::NetworkOrProtocol { .. })
    }
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
}

impl Client {
    pub fn init(&self) -> Result<ProtocolInitialized, anyhow::Error> {
        let request = NegotiationRequest::from_config(&self.config)?;
        write_negotiation(self.conn_fd, &request)
            .map_err(V4ClientInitError::network_or_protocol)?;

        let mut buf = [0u8; HEADER_SIZE];
        let response_header =
            wait_event_header(self.conn_fd, EventType::TYPE_NEGOTIATION, &mut buf)
                .map_err(V4ClientInitError::network_or_protocol)?;
        let response_body = read_message_body(self.conn_fd, &response_header)
            .map_err(V4ClientInitError::network_or_protocol)?;
        let response: NegotiationResponse =
            serde_json::from_slice(&response_body).map_err(V4ClientInitError::invalid_response)?;
        if let Some(err) = response.error {
            return Err(V4ClientInitError::rejected(err));
        }
        if response.selected_protocol != PROTO_VERSION_V4 {
            return Err(V4ClientInitError::unsupported_protocol(
                response.selected_protocol,
            ));
        }
        let feature = response
            .selected_feature_for_request(&request)
            .map_err(V4ClientInitError::invalid_response)?;

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
                    MSG_VERSION_V4,
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
                    MSG_VERSION_V4,
                )?;
            }
        }

        let mut buf = [0u8; HEADER_SIZE];
        wait_event_header(self.conn_fd, EventType::TYPE_ACK_SHARE_MEMORY, &mut buf)?;
        Ok(ProtocolInitialized::new(
            None,
            PROTO_VERSION_V4,
            MSG_VERSION_V4,
            feature,
            eventfd,
        ))
    }
}

pub struct Server {
    pub(crate) conn_fd: RawFd,
    pub(crate) first_event: Header,
}

impl Server {
    pub fn init(&self) -> Result<ProtocolInitialized, anyhow::Error> {
        if self.first_event.msg_type() != EventType::TYPE_NEGOTIATION {
            return Err(anyhow!(
                "ProtocolInitializerV4 expect first event is:{}({}),but:{}",
                EventType::TYPE_NEGOTIATION.inner(),
                EventType::TYPE_NEGOTIATION,
                self.first_event.msg_type().inner()
            ));
        }

        let body = read_message_body(self.conn_fd, &self.first_event)?;
        let request: NegotiationRequest = match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(err) => {
                let _ = write_negotiation(
                    self.conn_fd,
                    &NegotiationResponse::json_parse_error(err.to_string()),
                );
                return Err(err.into());
            }
        };
        if !request.supports_protocols.contains(&PROTO_VERSION_V4) {
            write_negotiation(self.conn_fd, &NegotiationResponse::version_not_supported())?;
            return Err(anyhow!("client does not support protocol v4"));
        }

        let feature = select_feature(&request);
        write_negotiation(self.conn_fd, &NegotiationResponse::ok(feature)?)?;

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
                    MSG_VERSION_V4,
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

        let mut resp_header = Header([0u8; HEADER_SIZE].as_mut_ptr());
        resp_header.encode(
            HEADER_SIZE as u32,
            MSG_VERSION_V4,
            EventType::TYPE_ACK_SHARE_MEMORY,
        );
        protocol_trace(&resp_header, &[], true);
        // SAFETY: resp_header points to the stack buffer used to construct the header.
        block_write_full(self.conn_fd, unsafe {
            std::slice::from_raw_parts(resp_header.0, HEADER_SIZE)
        })?;

        Ok(ProtocolInitialized::new(
            shared_memory,
            PROTO_VERSION_V4,
            MSG_VERSION_V4,
            feature,
            eventfd,
        ))
    }
}

fn write_negotiation<T>(conn_fd: RawFd, value: &T) -> Result<(), anyhow::Error>
where
    T: Serialize,
{
    let payload = serde_json::to_vec(value)?;
    let mut data = vec![0u8; HEADER_SIZE + payload.len()];
    let mut header = Header(data.as_mut_ptr());
    header.encode(
        data.len() as u32,
        MSG_VERSION_V4,
        EventType::TYPE_NEGOTIATION,
    );
    data[HEADER_SIZE..].copy_from_slice(&payload);
    protocol_trace(&header, &payload, true);
    block_write_full(conn_fd, &data)
}

fn read_message_body(conn_fd: RawFd, h: &Header) -> Result<Vec<u8>, anyhow::Error> {
    if h.length() < HEADER_SIZE as u32 {
        return Err(anyhow!(
            "invalid v4 message length {}, header size {}",
            h.length(),
            HEADER_SIZE
        ));
    }
    let mut body = vec![0u8; h.length() as usize - HEADER_SIZE];
    if !body.is_empty() {
        block_read_full(conn_fd, &mut body)
            .map_err(|err| anyhow!("read v4 message body failed, reason:{}", err))?;
    }
    protocol_trace(h, &body, false);
    Ok(body)
}
