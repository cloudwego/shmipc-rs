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
    io::{IoSlice, IoSliceMut},
    os::fd::{BorrowedFd, RawFd},
};

use anyhow::anyhow;
use libc::EINVAL;
use nix::{
    cmsg_space,
    sys::socket::{
        ControlMessage, ControlMessageOwned, MsgFlags, getsockopt, recvmsg, sendmsg,
        sockopt::SockType,
    },
    unistd::{read, write},
};

use crate::consts::MEMFD_COUNT_EVENTFD;

pub(crate) fn block_read_full(conn_fd: RawFd, data: &mut [u8]) -> Result<(), anyhow::Error> {
    let mut read_size = 0;
    while read_size < data.len() {
        let n = read(
            unsafe { BorrowedFd::borrow_raw(conn_fd) },
            &mut data[read_size..],
        )
        .map_err(|e| {
            anyhow!(
                "read_full failed, had read_size:{read_size}, reason:{}",
                e.desc()
            )
        })?;
        read_size += n;
        if n == 0 {
            return Err(anyhow!("EOF"));
        }
    }
    Ok(())
}

pub(crate) fn block_write_full(conn_fd: RawFd, data: &[u8]) -> Result<(), anyhow::Error> {
    let mut written = 0;
    while written < data.len() {
        let n = write(unsafe { BorrowedFd::borrow_raw(conn_fd) }, &data[written..])?;
        written += n;
    }
    Ok(())
}

pub(crate) fn send_fd(conn_fd: RawFd, fds: &[RawFd]) -> Result<(), anyhow::Error> {
    let mut iov = [IoSlice::new(&[0u8; 0])];
    let mut cmsgs = Vec::with_capacity(1);
    if !fds.is_empty() {
        let borrowed_fd = unsafe { BorrowedFd::borrow_raw(conn_fd) };
        let sock_type = getsockopt(&borrowed_fd, SockType)?;
        if sock_type != nix::sys::socket::SockType::Datagram {
            iov[0] = IoSlice::new(&[0u8; 1]);
        }
        cmsgs.push(ControlMessage::ScmRights(fds))
    }
    Ok(sendmsg::<()>(
        conn_fd,
        iov.as_slice(),
        cmsgs.as_slice(),
        MsgFlags::empty(),
        None,
    )
    .map(|_| ())?)
}

pub(crate) fn block_read_out_of_bound_for_fd(
    conn_fd: RawFd,
    expected_fd_count: usize,
) -> Result<Vec<RawFd>, anyhow::Error> {
    let mut iov = [IoSliceMut::new(&mut [0u8; 0])];

    let borrowed_fd = unsafe { BorrowedFd::borrow_raw(conn_fd) };
    let sock_type = getsockopt(&borrowed_fd, SockType)?;
    let mut buf = [0u8; 1];
    if sock_type != nix::sys::socket::SockType::Datagram {
        iov[0] = IoSliceMut::new(&mut buf);
    }
    let mut cmsg_buffer = cmsg_space!([RawFd; MEMFD_COUNT_EVENTFD]);

    let recv_msg = recvmsg::<()>(
        conn_fd,
        iov.as_mut_slice(),
        Some(cmsg_buffer.as_mut()),
        MsgFlags::empty(),
    )
    .map_err(|err| anyhow!("try recv fd from peer failed, reason:{}", err))?;
    tracing::info!("recvmsg finished");

    if let Some(msgs) = recv_msg.cmsgs()?.next() {
        if let ControlMessageOwned::ScmRights(fds) = msgs {
            if fds.len() < expected_fd_count {
                let got = fds.len();
                for fd in fds {
                    let _ = nix::unistd::close(fd);
                }
                return Err(anyhow!(
                    "the number of fd received is wrong, expected at least {}, got {}",
                    expected_fd_count,
                    got
                ));
            }
            Ok(fds)
        } else {
            Err(anyhow!(
                "parse fd from unix domain failed, reason errno:{}",
                EINVAL
            ))
        }
    } else {
        Err(anyhow!("parse socket control message ret is nil"))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        os::{fd::AsRawFd, unix::net::UnixStream},
    };

    use super::{block_read_out_of_bound_for_fd, send_fd};
    use crate::consts::MEMFD_COUNT_EVENTFD;

    #[test]
    fn recv_fd_rejects_insufficient_fd_count() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let file_a = File::open("/dev/null").unwrap();
        let file_b = File::open("/dev/null").unwrap();

        send_fd(
            sender.as_raw_fd(),
            &[file_a.as_raw_fd(), file_b.as_raw_fd()],
        )
        .unwrap();
        let err =
            block_read_out_of_bound_for_fd(receiver.as_raw_fd(), MEMFD_COUNT_EVENTFD).unwrap_err();

        assert!(err.to_string().contains("expected at least 4, got 2"));
    }
}
