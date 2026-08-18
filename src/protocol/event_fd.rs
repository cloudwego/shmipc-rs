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
    io,
    os::fd::{FromRawFd, OwnedFd, RawFd},
};

#[derive(Debug)]
pub(crate) struct EventFdPair {
    pub(crate) wakeup_send: OwnedFd,
    pub(crate) wakeup_recv: OwnedFd,
}

#[cfg(target_os = "linux")]
pub(crate) fn create_eventfd() -> io::Result<OwnedFd> {
    // SAFETY: eventfd is called with valid flags and returns a fresh fd on success.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: fd was just returned by eventfd and is uniquely owned here.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn create_eventfd() -> io::Result<OwnedFd> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "eventfd is only supported on Linux",
    ))
}

#[cfg(target_os = "linux")]
pub(crate) fn write_eventfd(fd: RawFd) -> io::Result<()> {
    loop {
        // SAFETY: fd is an eventfd owned by the session; libc validates the descriptor.
        let ret = unsafe { libc::eventfd_write(fd, 1) };
        if ret == 0 {
            return Ok(());
        }

        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if err.kind() == io::ErrorKind::WouldBlock {
            return Ok(());
        }
        return Err(err);
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn write_eventfd(_fd: RawFd) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "eventfd is only supported on Linux",
    ))
}

/// Drains an eventfd until EAGAIN and returns whether any counter value was consumed.
#[cfg(target_os = "linux")]
pub(crate) fn drain_eventfd(fd: RawFd) -> io::Result<bool> {
    let mut drained = false;
    loop {
        let mut value: libc::eventfd_t = 0;
        // SAFETY: fd is an eventfd owned by the session; libc writes to a valid stack pointer.
        let ret = unsafe { libc::eventfd_read(fd, &mut value) };
        if ret == 0 {
            drained = true;
            continue;
        }

        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if err.kind() == io::ErrorKind::WouldBlock {
            return Ok(drained);
        }
        return Err(err);
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn drain_eventfd(_fd: RawFd) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "eventfd is only supported on Linux",
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{
        io,
        os::fd::{AsRawFd, RawFd},
    };

    use super::{create_eventfd, drain_eventfd, write_eventfd};

    #[test]
    fn eventfd_write_drain_and_drop_close_fd() {
        let fd = create_eventfd().unwrap();
        let raw_fd = fd.as_raw_fd();

        assert!(!drain_eventfd(raw_fd).unwrap());
        write_eventfd(raw_fd).unwrap();
        assert!(drain_eventfd(raw_fd).unwrap());
        assert!(!drain_eventfd(raw_fd).unwrap());

        drop(fd);
        assert_fd_closed(raw_fd);
    }

    #[test]
    fn eventfd_counter_overflow_is_non_fatal() {
        let fd = create_eventfd().unwrap();
        let raw_fd = fd.as_raw_fd();
        let nearly_full = (u64::MAX - 2).to_ne_bytes();

        // SAFETY: raw_fd is a valid eventfd and nearly_full points to exactly one u64 value.
        let ret = unsafe {
            libc::write(
                raw_fd,
                nearly_full.as_ptr().cast::<libc::c_void>(),
                nearly_full.len(),
            )
        };
        assert_eq!(nearly_full.len() as isize, ret);

        write_eventfd(raw_fd).unwrap();
        write_eventfd(raw_fd).unwrap();
        assert!(drain_eventfd(raw_fd).unwrap());
    }

    fn assert_fd_closed(fd: RawFd) {
        // SAFETY: fcntl validates the raw descriptor and does not take ownership.
        let ret = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(-1, ret);
        assert_eq!(Some(libc::EBADF), io::Error::last_os_error().raw_os_error());
    }
}
