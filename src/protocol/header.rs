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

use std::{fmt::Display, ptr::copy_nonoverlapping};

use super::event::EventType;
use crate::consts::{HEADER_SIZE, MAGIC_NUMBER};

#[derive(Clone, Debug)]
pub struct Header(pub *mut u8);

unsafe impl Sync for Header {}
unsafe impl Send for Header {}

impl Header {
    #[inline]
    pub fn length(&self) -> u32 {
        unsafe { u32::from_be_bytes(*(self.0.cast_const() as *const [u8; 4])) }
    }

    #[inline]
    pub fn magic(&self) -> u16 {
        unsafe { u16::from_be_bytes(*(self.0.offset(4).cast_const() as *const [u8; 2])) }
    }

    #[inline]
    pub fn version(&self) -> u8 {
        unsafe { *self.0.offset(6) }
    }

    #[inline]
    pub fn msg_type(&self) -> EventType {
        unsafe { EventType::from(*self.0.offset(7)) }
    }

    pub fn encode(&mut self, length: u32, version: u8, msg_type: EventType) {
        unsafe {
            copy_nonoverlapping(length.to_be_bytes().as_ptr(), self.0, 4);
            copy_nonoverlapping(MAGIC_NUMBER.to_be_bytes().as_ptr(), self.0.offset(4), 2);
            *self.0.offset(6) = version;
            *self.0.offset(7) = msg_type.inner();
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.0, HEADER_SIZE) }
    }
}

impl Display for Header {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg_type = self.msg_type();
        write!(
            f,
            "Header {{ length: {}, magic: {}, version: {}, msg_type: {} }}",
            self.length(),
            self.magic(),
            self.version(),
            msg_type
        )
    }
}
