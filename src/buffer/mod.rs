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

mod buf;
pub mod linked;
pub mod list;
pub mod manager;
pub mod slice;

pub use buf::{Buf, ShmBuf};
pub use linked::LinkedBuffer;
pub use slice::BufferSlice;

use crate::error::Error;

pub trait BufferReader {
    /// Read `size` bytes from shared memory.
    ///
    /// A zero-copy result pins its underlying slice until the returned [`Buf`] (or the
    /// [`bytes::Bytes`] created by [`Buf::into_bytes`]) is dropped.
    fn read_bytes(&mut self, size: usize) -> Result<Buf<'_>, Error>;

    /// Peek `size` byte from share memory.
    ///
    /// The difference between `peek()` and `read_bytes()` is that
    /// `peek()` don't influence the return value of length, but the `read_bytes()` will decrease
    /// the unread size.
    ///
    /// A zero-copy result remains valid for its own lifetime, including across
    /// [`BufferReader::release_previous_read`].
    fn peek(&mut self, size: usize) -> Result<Buf<'_>, Error>;

    /// Drop data of given size.
    fn discard(&mut self, size: usize) -> Result<usize, Error>;

    /// Eagerly release consumed storage that is not held by an outstanding zero-copy result.
    /// Pinned slices are reclaimed automatically when their final lease is dropped.
    fn release_previous_read(&mut self);
}

pub trait BufferWriter {
    /// Reserve `size` bytes share memory space, use it to implement zero copy write.
    fn reserve(&mut self, size: usize) -> Result<&mut [u8], Error>;

    /// Copy data to share memory, return the copy size if success.
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<usize, Error>;
}
