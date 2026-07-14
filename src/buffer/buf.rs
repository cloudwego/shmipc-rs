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

use std::{borrow::Borrow, cmp, fmt, hash, ops::Deref};

use bytes::Bytes;

use super::linked::PinLease;

pub struct ShmBuf<'shm> {
    buf: &'shm [u8],
    _lease: PinLease,
}

impl fmt::Debug for ShmBuf<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ShmBuf").field(&self.buf).finish()
    }
}

impl<'shm> ShmBuf<'shm> {
    pub(crate) const fn new(buf: &'shm [u8], lease: PinLease) -> Self {
        Self { buf, _lease: lease }
    }

    fn into_bytes(self) -> Bytes {
        // The lease keeps both the mapping and the retired BufferSlice alive until the final
        // Bytes clone is dropped, so erasing the borrow lifetime here is sound.
        let owner: ShmBuf<'static> = unsafe { std::mem::transmute(self) };
        Bytes::from_owner(owner)
    }
}

impl Deref for ShmBuf<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.buf
    }
}

impl AsRef<[u8]> for ShmBuf<'_> {
    fn as_ref(&self) -> &[u8] {
        self.buf
    }
}

#[derive(Debug)]
pub enum Buf<'shm> {
    Shm(ShmBuf<'shm>),
    Exm(Bytes),
}

impl Buf<'_> {
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

impl Deref for Buf<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Buf::Shm(buf) => buf.as_ref(),
            Buf::Exm(buf) => buf,
        }
    }
}

impl AsRef<[u8]> for Buf<'_> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        match self {
            Buf::Shm(buf) => buf.as_ref(),
            Buf::Exm(buf) => buf,
        }
    }
}

impl hash::Hash for Buf<'_> {
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

impl Borrow<[u8]> for Buf<'_> {
    fn borrow(&self) -> &[u8] {
        match self {
            Buf::Shm(buf) => buf.as_ref(),
            Buf::Exm(buf) => buf,
        }
    }
}

impl PartialEq for Buf<'_> {
    fn eq(&self, other: &Buf<'_>) -> bool {
        self.as_ref().eq(other.as_ref())
    }
}

impl PartialOrd for Buf<'_> {
    fn partial_cmp(&self, other: &Buf<'_>) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Buf<'_> {
    fn cmp(&self, other: &Buf<'_>) -> cmp::Ordering {
        self.as_ref().cmp(other.as_ref())
    }
}

impl Eq for Buf<'_> {}

impl PartialEq<[u8]> for Buf<'_> {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_ref() == other
    }
}

impl PartialOrd<[u8]> for Buf<'_> {
    fn partial_cmp(&self, other: &[u8]) -> Option<cmp::Ordering> {
        self.as_ref().partial_cmp(other)
    }
}

impl PartialEq<Buf<'_>> for [u8] {
    fn eq(&self, other: &Buf<'_>) -> bool {
        *other == *self
    }
}

impl PartialOrd<Buf<'_>> for [u8] {
    fn partial_cmp(&self, other: &Buf<'_>) -> Option<cmp::Ordering> {
        <[u8] as PartialOrd<[u8]>>::partial_cmp(self, other)
    }
}

impl PartialEq<str> for Buf<'_> {
    fn eq(&self, other: &str) -> bool {
        self.as_ref() == other.as_bytes()
    }
}

impl PartialOrd<str> for Buf<'_> {
    fn partial_cmp(&self, other: &str) -> Option<cmp::Ordering> {
        self.as_ref().partial_cmp(other.as_bytes())
    }
}

impl PartialEq<Buf<'_>> for str {
    fn eq(&self, other: &Buf<'_>) -> bool {
        *other == *self
    }
}

impl PartialOrd<Buf<'_>> for str {
    fn partial_cmp(&self, other: &Buf<'_>) -> Option<cmp::Ordering> {
        <[u8] as PartialOrd<[u8]>>::partial_cmp(self.as_bytes(), other)
    }
}

impl PartialEq<Vec<u8>> for Buf<'_> {
    fn eq(&self, other: &Vec<u8>) -> bool {
        *self == other[..]
    }
}

impl PartialOrd<Vec<u8>> for Buf<'_> {
    fn partial_cmp(&self, other: &Vec<u8>) -> Option<cmp::Ordering> {
        self.as_ref().partial_cmp(&other[..])
    }
}

impl PartialEq<Buf<'_>> for Vec<u8> {
    fn eq(&self, other: &Buf<'_>) -> bool {
        *other == *self
    }
}

impl PartialOrd<Buf<'_>> for Vec<u8> {
    fn partial_cmp(&self, other: &Buf<'_>) -> Option<cmp::Ordering> {
        <[u8] as PartialOrd<[u8]>>::partial_cmp(self, other)
    }
}

impl PartialEq<String> for Buf<'_> {
    fn eq(&self, other: &String) -> bool {
        *self == other[..]
    }
}

impl PartialOrd<String> for Buf<'_> {
    fn partial_cmp(&self, other: &String) -> Option<cmp::Ordering> {
        self.as_ref().partial_cmp(other.as_bytes())
    }
}

impl PartialEq<Buf<'_>> for String {
    fn eq(&self, other: &Buf<'_>) -> bool {
        *other == *self
    }
}

impl PartialOrd<Buf<'_>> for String {
    fn partial_cmp(&self, other: &Buf<'_>) -> Option<cmp::Ordering> {
        <[u8] as PartialOrd<[u8]>>::partial_cmp(self.as_bytes(), other)
    }
}

impl PartialEq<Buf<'_>> for &[u8] {
    fn eq(&self, other: &Buf<'_>) -> bool {
        other.as_ref() == *self
    }
}

impl PartialOrd<Buf<'_>> for &[u8] {
    fn partial_cmp(&self, other: &Buf<'_>) -> Option<cmp::Ordering> {
        <[u8] as PartialOrd<[u8]>>::partial_cmp(self, other)
    }
}

impl PartialEq<Buf<'_>> for &str {
    fn eq(&self, other: &Buf<'_>) -> bool {
        *other.as_ref() == *self.as_bytes()
    }
}

impl PartialOrd<Buf<'_>> for &str {
    fn partial_cmp(&self, other: &Buf<'_>) -> Option<cmp::Ordering> {
        <[u8] as PartialOrd<[u8]>>::partial_cmp(self.as_bytes(), other)
    }
}

impl bytes::Buf for Buf<'_> {
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
