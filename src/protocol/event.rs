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

use std::{fmt::Display, ptr::copy_nonoverlapping, sync::LazyLock};

use super::header::Header;
use crate::{
    consts::{HEADER_SIZE, MAGIC_NUMBER, MAX_SUPPORT_PROTO_VERSION},
    error::Error,
};

pub const MIN_EVENT_TYPE: EventType = EventType::TYPE_SHARE_MEMORY_BY_FILE_PATH;
pub const MAX_EVENT_TYPE: EventType = EventType::TYPE_HOT_RESTART_ACK;

pub static POLLING_EVENT_WITH_VERSION: LazyLock<Vec<Vec<u8>>> = LazyLock::new(|| {
    let mut events = Vec::with_capacity(MAX_SUPPORT_PROTO_VERSION as usize + 1);
    for i in 0..MAX_SUPPORT_PROTO_VERSION + 1 {
        let mut v = vec![0u8; HEADER_SIZE];
        let mut event = Header(v.as_mut_ptr());
        event.encode(HEADER_SIZE as u32, i, EventType::TYPE_POLLING);
        events.push(v);
    }
    events
});

/// EventType for internal implements
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EventType(u8);

impl EventType {
    pub const TYPE_SHARE_MEMORY_BY_FILE_PATH: Self = Self(0);

    /// notify peer start consume
    pub const TYPE_POLLING: Self = Self(1);

    /// stream level event notify peer stream close
    pub const TYPE_STREAM_CLOSE: Self = Self(2);

    /// typePing
    pub const TYPE_FALLBACK_DATA: Self = Self(3);

    /// exchange proto version
    pub const TYPE_EXCHANGE_PROTO_VERSION: Self = Self(4);

    /// query the mem map type supported by the server
    pub const TYPE_SHARE_MEMORY_BY_MEMFD: Self = Self(5);

    /// when server mapping share memory success, give the ack to client.
    pub const TYPE_ACK_SHARE_MEMORY: Self = Self(6);

    pub const TYPE_ACK_READY_RECV_FD: Self = Self(7);

    pub const TYPE_HOT_RESTART: Self = Self(8);

    pub const TYPE_HOT_RESTART_ACK: Self = Self(9);

    pub fn inner(&self) -> u8 {
        self.0
    }
}

impl From<u8> for EventType {
    fn from(v: u8) -> Self {
        match v {
            0 => EventType::TYPE_SHARE_MEMORY_BY_FILE_PATH,
            1 => EventType::TYPE_POLLING,
            2 => EventType::TYPE_STREAM_CLOSE,
            3 => EventType::TYPE_FALLBACK_DATA,
            4 => EventType::TYPE_EXCHANGE_PROTO_VERSION,
            5 => EventType::TYPE_SHARE_MEMORY_BY_MEMFD,
            6 => EventType::TYPE_ACK_SHARE_MEMORY,
            7 => EventType::TYPE_ACK_READY_RECV_FD,
            8 => EventType::TYPE_HOT_RESTART,
            9 => EventType::TYPE_HOT_RESTART_ACK,
            i => Self(i),
        }
    }
}

impl Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut unset = "UNSET".to_owned();
        let s = match self.0 {
            0 => "ShareMemoryByFilePath",
            1 => "Polling",
            2 => "StreamClose",
            3 => "FallbackData",
            4 => "ExchangeProtoVersion",
            5 => "ShareMemoryByMemfd",
            6 => "AckShareMemory",
            7 => "AckReadyRecvFD",
            8 => "HotRestart",
            9 => "HotRestartAck",
            i => {
                unset.push_str(&format!("{i}"));
                &unset
            }
        };
        write!(f, "{}", s)
    }
}

/// header | seq_id | status
#[derive(Clone, Debug)]
pub struct FallbackDataEvent(pub *mut u8);

unsafe impl Sync for FallbackDataEvent {}
unsafe impl Send for FallbackDataEvent {}

impl FallbackDataEvent {
    pub fn encode(&mut self, length: u32, version: u8, seq_id: u32, status: u32) {
        unsafe {
            copy_nonoverlapping(length.to_be_bytes().as_ptr(), self.0, 4);
            copy_nonoverlapping(MAGIC_NUMBER.to_be_bytes().as_ptr(), self.0.offset(4), 2);

            *self.0.offset(6) = version;
            *self.0.offset(7) = EventType::TYPE_FALLBACK_DATA.0;

            copy_nonoverlapping(seq_id.to_be_bytes().as_ptr(), self.0.offset(8), 4);
            copy_nonoverlapping(status.to_be_bytes().as_ptr(), self.0.offset(12), 4);
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.0, HEADER_SIZE + 8) }
    }
}

pub fn check_event_valid(hdr: &Header) -> Result<(), Error> {
    // Verify the magic & version
    if hdr.magic() != MAGIC_NUMBER || hdr.version() == 0 {
        tracing::error!(
            "shmipc: Invalid magic or version {} {}",
            hdr.magic(),
            hdr.version()
        );
        return Err(Error::InvalidVersion);
    }
    let mt = hdr.msg_type();
    if mt < MIN_EVENT_TYPE || mt > MAX_EVENT_TYPE {
        tracing::error!("shmipc, invalid protocol header: {}", hdr);
        return Err(Error::InvalidMsgType);
    }
    Ok(())
}
