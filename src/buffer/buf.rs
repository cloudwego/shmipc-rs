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

use std::{borrow::Borrow, cmp, fmt, hash, ops::Deref, slice, sync::Arc};

use bytes::Bytes;

use super::{buf_v5::V5Pin, linked::SlicePinState};

/// Keeps the storage behind a zero-copy read alive until the last reader drops: a share of
/// whatever keeps the memory a [`ShmBuf`] points at both mapped and unrecycled, which is a
/// slice's pin state on V4 and a block's pin on V5.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum PinLease {
    V4(Arc<SlicePinState>),
    V5(Arc<V5Pin>),
}

/// A run of shared memory that stays valid independently of the buffer it was read from.
pub struct ShmBuf {
    buf: &'static [u8],
    _lease: PinLease,
}

impl fmt::Debug for ShmBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ShmBuf").field(&self.buf).finish()
    }
}

impl ShmBuf {
    /// `ptr` must point to a region of `len` bytes that is valid to read for the
    /// entire lifetime of the returned `ShmBuf`. The region must not be unmapped,
    /// recycled, or otherwise invalidated while the returned `ShmBuf` (or any
    /// `Bytes` created from it) is alive.
    ///
    /// `lease` must own or otherwise keep alive whatever guarantees those
    /// requirements.
    pub(crate) const unsafe fn new(ptr: *const u8, len: usize, lease: PinLease) -> Self {
        // SAFETY: the caller guarantees the region is readable and that `lease`, which is moved
        // into the returned value, holds it for at least as long as that value lives.
        let buf = unsafe { slice::from_raw_parts(ptr, len) };
        Self { buf, _lease: lease }
    }

    fn into_bytes(self) -> Bytes {
        Bytes::from_owner(self)
    }
}

impl Deref for ShmBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.buf
    }
}

impl AsRef<[u8]> for ShmBuf {
    fn as_ref(&self) -> &[u8] {
        self.buf
    }
}

#[derive(Debug)]
pub enum Buf {
    Shm(ShmBuf),
    Exm(Bytes),
}

impl Buf {
    pub const fn len(&self) -> usize {
        match self {
            Self::Shm(s) => s.buf.len(),
            Self::Exm(b) => b.len(),
        }
    }

    pub const fn is_empty(&self) -> bool {
        match self {
            Self::Shm(s) => s.buf.is_empty(),
            Self::Exm(b) => b.is_empty(),
        }
    }

    /// Convert this buffer into owned [`Bytes`] without copying.
    ///
    /// Shared-memory slices remain pinned until the returned value and all of its clones are
    /// dropped.
    pub fn into_bytes(self) -> Bytes {
        match self {
            Buf::Shm(buf) => buf.into_bytes(),
            Buf::Exm(buf) => buf,
        }
    }
}

impl Deref for Buf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Buf::Shm(buf) => buf.as_ref(),
            Buf::Exm(buf) => buf,
        }
    }
}

impl AsRef<[u8]> for Buf {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        match self {
            Buf::Shm(buf) => buf.as_ref(),
            Buf::Exm(buf) => buf,
        }
    }
}

impl hash::Hash for Buf {
    fn hash<H>(&self, state: &mut H)
    where
        H: hash::Hasher,
    {
        match self {
            Buf::Shm(buf) => buf.as_ref().hash(state),
            Buf::Exm(buf) => buf.hash(state),
        }
    }
}

impl Borrow<[u8]> for Buf {
    fn borrow(&self) -> &[u8] {
        match self {
            Buf::Shm(buf) => buf.as_ref(),
            Buf::Exm(buf) => buf,
        }
    }
}

impl PartialEq for Buf {
    fn eq(&self, other: &Buf) -> bool {
        self.as_ref().eq(other.as_ref())
    }
}

impl PartialOrd for Buf {
    fn partial_cmp(&self, other: &Buf) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Buf {
    fn cmp(&self, other: &Buf) -> cmp::Ordering {
        self.as_ref().cmp(other.as_ref())
    }
}

impl Eq for Buf {}

impl PartialEq<[u8]> for Buf {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_ref() == other
    }
}

impl PartialOrd<[u8]> for Buf {
    fn partial_cmp(&self, other: &[u8]) -> Option<cmp::Ordering> {
        self.as_ref().partial_cmp(other)
    }
}

impl PartialEq<Buf> for [u8] {
    fn eq(&self, other: &Buf) -> bool {
        *other == *self
    }
}

impl PartialOrd<Buf> for [u8] {
    fn partial_cmp(&self, other: &Buf) -> Option<cmp::Ordering> {
        <[u8] as PartialOrd<[u8]>>::partial_cmp(self, other)
    }
}

impl PartialEq<str> for Buf {
    fn eq(&self, other: &str) -> bool {
        self.as_ref() == other.as_bytes()
    }
}

impl PartialOrd<str> for Buf {
    fn partial_cmp(&self, other: &str) -> Option<cmp::Ordering> {
        self.as_ref().partial_cmp(other.as_bytes())
    }
}

impl PartialEq<Buf> for str {
    fn eq(&self, other: &Buf) -> bool {
        *other == *self
    }
}

impl PartialOrd<Buf> for str {
    fn partial_cmp(&self, other: &Buf) -> Option<cmp::Ordering> {
        <[u8] as PartialOrd<[u8]>>::partial_cmp(self.as_bytes(), other)
    }
}

impl PartialEq<Vec<u8>> for Buf {
    fn eq(&self, other: &Vec<u8>) -> bool {
        *self == other[..]
    }
}

impl PartialOrd<Vec<u8>> for Buf {
    fn partial_cmp(&self, other: &Vec<u8>) -> Option<cmp::Ordering> {
        self.as_ref().partial_cmp(&other[..])
    }
}

impl PartialEq<Buf> for Vec<u8> {
    fn eq(&self, other: &Buf) -> bool {
        *other == *self
    }
}

impl PartialOrd<Buf> for Vec<u8> {
    fn partial_cmp(&self, other: &Buf) -> Option<cmp::Ordering> {
        <[u8] as PartialOrd<[u8]>>::partial_cmp(self, other)
    }
}

impl PartialEq<String> for Buf {
    fn eq(&self, other: &String) -> bool {
        *self == other[..]
    }
}

impl PartialOrd<String> for Buf {
    fn partial_cmp(&self, other: &String) -> Option<cmp::Ordering> {
        self.as_ref().partial_cmp(other.as_bytes())
    }
}

impl PartialEq<Buf> for String {
    fn eq(&self, other: &Buf) -> bool {
        *other == *self
    }
}

impl PartialOrd<Buf> for String {
    fn partial_cmp(&self, other: &Buf) -> Option<cmp::Ordering> {
        <[u8] as PartialOrd<[u8]>>::partial_cmp(self.as_bytes(), other)
    }
}

impl PartialEq<Buf> for &[u8] {
    fn eq(&self, other: &Buf) -> bool {
        other.as_ref() == *self
    }
}

impl PartialOrd<Buf> for &[u8] {
    fn partial_cmp(&self, other: &Buf) -> Option<cmp::Ordering> {
        <[u8] as PartialOrd<[u8]>>::partial_cmp(self, other)
    }
}

impl PartialEq<Buf> for &str {
    fn eq(&self, other: &Buf) -> bool {
        *other.as_ref() == *self.as_bytes()
    }
}

impl PartialOrd<Buf> for &str {
    fn partial_cmp(&self, other: &Buf) -> Option<cmp::Ordering> {
        <[u8] as PartialOrd<[u8]>>::partial_cmp(self.as_bytes(), other)
    }
}

impl bytes::Buf for Buf {
    fn remaining(&self) -> usize {
        self.len()
    }

    fn chunk(&self) -> &[u8] {
        self.as_ref()
    }

    fn advance(&mut self, cnt: usize) {
        // no need to care cnt and len, `bytes::Buf` for `&[u8]` and `Bytes` will process it
        match self {
            Self::Shm(s) => bytes::Buf::advance(&mut s.buf, cnt),
            Self::Exm(b) => bytes::Buf::advance(b, cnt),
        }
    }
}
