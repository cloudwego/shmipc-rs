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

pub mod buf;
pub mod linked;
pub mod list;
pub mod manager;
pub mod slice;

use buf::Buf;

use crate::error::Error;

pub trait BufferReader {
    /// Read `size` bytes from share memory
    ///
    /// if `release_previous_read()` is called , the results of previous `read_bytes()` will be
    /// invalid.
    fn read_bytes(&mut self, size: usize) -> Result<Buf<'_>, Error>;

    /// Peek `size` byte from share memory.
    ///
    /// The difference between `peek()` and `read_bytes()` is that
    /// `peek()` don't influence the return value of length, but the `read_bytes()` will decrease
    /// the unread size.
    ///
    /// Results of previous `peek()` call is valid until `release_previous_read()` is called.
    fn peek(&mut self, size: usize) -> Result<Buf<'_>, Error>;

    /// Drop data of given size.
    fn discard(&mut self, size: usize) -> Result<usize, Error>;

    /// Call `release_previous_read()` when it is safe to drop all previous result of `read_bytes()`
    /// and `peek()`, otherwise shm memory will leak.
    fn release_previous_read(&mut self);
}

pub trait BufferWriter {
    /// Reserve `size` bytes share memory space, use it to implement zero copy write.
    fn reserve(&mut self, size: usize) -> Result<&mut [u8], Error>;

    /// Copy data to share memory, return the copy size if success.
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<usize, Error>;
}
