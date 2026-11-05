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

use std::{
    ffi::CString,
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
};

use anyhow::anyhow;
use memmap2::{MmapMut, MmapOptions};

/// An anonymous shared memory object together with its mapping.
pub struct MemFile {
    fd: OwnedFd,
    mmap: MmapMut,
}

impl MemFile {
    /// Creates a zero-filled object of exactly `size` bytes and maps it.
    ///
    /// `name` is only a debugging aid; it does not identify the object to the peer, which receives
    /// the descriptor itself.
    #[cfg(target_os = "linux")]
    pub fn new(name: &str, size: usize) -> Result<Self, anyhow::Error> {
        if size == 0 {
            return Err(anyhow!("memfile {} cannot be empty", name));
        }

        let shmipc_name = CString::new(format!("shmipc_{name}"))
            .map_err(|err| anyhow!("invalid memfile name: {}", err))?;

        let fd = nix::sys::memfd::memfd_create(
            shmipc_name.as_c_str(),
            nix::sys::memfd::MFdFlags::empty(),
        )
        .map_err(|err| anyhow!("memfd_create failed: {}", err))?;

        let len = i64::try_from(size)
            .map_err(|_| anyhow!("memfile {} size {} does not fit an off_t", name, size))?;

        nix::unistd::ftruncate(&fd, len)
            .map_err(|err| anyhow!("memfile {} ftruncate to {} failed: {}", name, size, err))?;

        // SAFETY: the object was just created, is only reachable through `fd`, and is sized to
        // `size`, so no other mapping can shrink it while this one is alive.
        let mmap = unsafe { MmapOptions::new().len(size).map_mut(&fd) }
            .map_err(|err| anyhow!("memfile {} mmap of {} bytes failed: {}", name, size, err))?;

        Ok(Self { fd, mmap })
    }

    /// Always fails: anonymous shared memory objects are created with `memfd_create`, which only
    /// exists on Linux.
    #[cfg(not(target_os = "linux"))]
    pub fn new(_name: &str, _size: usize) -> Result<Self, anyhow::Error> {
        Err(anyhow!("memfd_create is only supported on linux"))
    }

    /// Maps an object received from a peer, using the size the descriptor reports.
    ///
    /// Takes ownership of `fd`, so the mapping keeps the object alive for as long as the returned
    /// `MemFile` lives.
    #[cfg(target_os = "linux")]
    pub fn map(fd: OwnedFd) -> Result<Self, anyhow::Error> {
        let size = fd_size(&fd)?;
        if size == 0 {
            return Err(anyhow!("received memfile is empty"));
        }
        // SAFETY: the descriptor is owned here and the mapping is sized to what it reports.
        let mmap = unsafe { MmapOptions::new().len(size).map_mut(&fd) }
            .map_err(|err| anyhow!("mmap of received memfile failed: {}", err))?;
        Ok(Self { fd, mmap })
    }

    /// Always fails: mapping a peer's descriptor needs `fstat` on a memfd, which is only supported
    /// on Linux.
    #[cfg(not(target_os = "linux"))]
    pub fn map(_fd: OwnedFd) -> Result<Self, anyhow::Error> {
        Err(anyhow!("opening fd only supported on linux"))
    }

    /// Returns the length of the mapping in bytes, which is the size the object was created with.
    pub fn size(&self) -> usize {
        self.mmap.len()
    }

    /// Borrows the mapped memory.
    pub fn data(&self) -> &[u8] {
        &self.mmap
    }

    /// Borrows the mapped memory mutably.
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.mmap
    }

    /// Drops this process' resident pages for `[offset, offset + len)`, lowering RSS without
    /// disturbing the virtual mapping.
    ///
    /// The object is shared, so the bytes themselves survive in the page cache: a later access
    /// re-faults the range and reads back what was written. This only sheds the mapping's private
    /// cost, it does not deallocate or zero the range.
    ///
    /// An empty `len` is a no-op; a range that overflows or runs past the end of the mapping is an
    /// error.
    pub fn punch_hole(&self, offset: usize, len: usize) -> Result<(), anyhow::Error> {
        if len == 0 {
            return Ok(());
        }

        let end = offset.checked_add(len).ok_or_else(|| {
            anyhow!(
                "punch hole at {} for {} bytes overflows an offset",
                offset,
                len
            )
        })?;

        if end > self.mmap.len() {
            return Err(anyhow!(
                "punch hole {}..{} lies outside the {} byte memfile",
                offset,
                end,
                self.mmap.len()
            ));
        }

        self.punch_hole_range(offset, len)
    }

    /// Always fails: the reclaim relies on `madvise(MADV_DONTNEED)`, whose page-dropping semantics
    /// are Linux specific.
    #[cfg(not(target_os = "linux"))]
    fn punch_hole_range(&self, _offset: usize, _len: usize) -> Result<(), anyhow::Error> {
        Err(anyhow!("MADV_DONTNEED is only supported on linux"))
    }

    /// Performs the reclaim for an already bounds-checked range.
    #[cfg(target_os = "linux")]
    fn punch_hole_range(&self, offset: usize, len: usize) -> Result<(), anyhow::Error> {
        // SAFETY: the range was checked to lie inside the mapping.
        // MADV_DONTNEED can change the observable contents of the mapped pages,
        // so we use the unchecked API and rely on the caller keeping any Rust
        // borrow of the mapping off this range.
        unsafe {
            self.mmap
                .unchecked_advise_range(memmap2::UncheckedAdvice::DontNeed, offset, len)
        }
        .map_err(|err| {
            anyhow!(
                "madvise(MADV_DONTNEED) on {}..{} failed: {}",
                offset,
                offset + len,
                err
            )
        })
    }
}

impl AsRawFd for MemFile {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl AsFd for MemFile {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl std::fmt::Debug for MemFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemFile")
            .field("fd", &self.fd.as_raw_fd())
            .field("size", &self.mmap.len())
            .finish()
    }
}

/// Returns the size the descriptor reports through `fstat`.
///
/// Used to size the mapping for a memfd received from a peer, which arrives without any
/// out-of-band length.
#[cfg(target_os = "linux")]
pub fn fd_size(fd: &OwnedFd) -> Result<usize, anyhow::Error> {
    let stat = nix::sys::stat::fstat(fd).map_err(|err| anyhow!("fstat failed: {}", err))?;
    usize::try_from(stat.st_size)
        .map_err(|_| anyhow!("file size {} cannot fit in usize", stat.st_size))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::fd::AsRawFd;

    use super::{MemFile, fd_size};

    #[test]
    fn create_maps_a_zero_filled_object_of_the_requested_size() {
        let file = MemFile::new("create", 8192).unwrap();

        assert_eq!(8192, file.size());
        assert!(file.data().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn open_maps_the_same_object() {
        let mut owner = MemFile::new("shared", 4096).unwrap();
        owner.data_mut()[..5].copy_from_slice(b"hello");

        // SAFETY: the descriptor is duplicated so both mappings own their own descriptor.
        let duplicated = nix::unistd::dup(&owner).unwrap();
        let mapped = MemFile::map(duplicated).unwrap();

        assert_eq!(4096, mapped.size());
        assert_eq!(b"hello", &mapped.data()[..5]);

        // Writes stay visible across the two mappings.
        owner.data_mut()[..5].copy_from_slice(b"world");
        assert_eq!(b"world", &mapped.data()[..5]);
    }

    #[test]
    fn fd_size_reports_the_created_size() {
        let file = MemFile::new("size", 12288).unwrap();
        let duplicated = nix::unistd::dup(&file).unwrap();

        assert_eq!(12288, fd_size(&duplicated).unwrap());
        assert!(file.as_raw_fd() >= 0);
    }

    #[test]
    fn punching_a_hole_reclaims_each_chunk_in_turn() {
        const TOTAL: usize = 16 << 20;
        const CHUNK: usize = 1 << 20;

        let mut file = MemFile::new("punch", TOTAL).unwrap();

        for offset in (0..TOTAL).step_by(CHUNK) {
            file.data_mut()[offset..offset + CHUNK].fill(0xCD);
            file.punch_hole(offset, CHUNK).unwrap();
        }

        assert!(file.data().iter().all(|byte| *byte == 0xCD));
    }

    #[test]
    fn punching_a_hole_leaves_the_rest_of_the_mapping_writable() {
        const SIZE: usize = 3 << 20;

        let mut file = MemFile::new("punch_partial", SIZE).unwrap();
        file.data_mut().fill(0xAB);

        file.punch_hole(1 << 20, 1 << 20).unwrap();

        assert!(file.data()[..1 << 20].iter().all(|byte| *byte == 0xAB));
        assert!(file.data()[2 << 20..].iter().all(|byte| *byte == 0xAB));

        // The address space survived, so the reclaimed range takes writes again.
        file.data_mut()[1 << 20..2 << 20].fill(0x77);
        assert!(
            file.data()[1 << 20..2 << 20]
                .iter()
                .all(|byte| *byte == 0x77)
        );
    }

    #[test]
    fn punch_hole_ignores_an_empty_range() {
        let file = MemFile::new("punch_empty", 8192).unwrap();

        assert!(file.punch_hole(0, 0).is_ok());
        assert!(file.punch_hole(8192, 0).is_ok());
    }

    #[test]
    fn punch_hole_rejects_a_range_running_past_the_mapping() {
        let file = MemFile::new("punch_past_end", 8192).unwrap();

        assert!(
            file.punch_hole(4096, 8192)
                .unwrap_err()
                .to_string()
                .contains("lies outside")
        );
        assert!(
            file.punch_hole(4096, usize::MAX)
                .unwrap_err()
                .to_string()
                .contains("overflows an offset")
        );
    }

    #[test]
    fn create_rejects_an_empty_object() {
        assert!(
            MemFile::new("empty", 0)
                .unwrap_err()
                .to_string()
                .contains("cannot be empty")
        );
    }
}
