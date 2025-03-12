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

pub mod v2;
pub mod v3;

use std::{os::fd::RawFd, sync::Arc};

use anyhow::anyhow;

use self::{v2::ProtocolInitializerV2, v3::ProtocolInitializerV3};
use super::{block_io::block_write_full, event::check_event_valid, header::Header, protocol_trace};
use crate::{
    buffer::manager::BufferManager,
    consts::{HEADER_SIZE, MAX_SUPPORT_PROTO_VERSION, MEMFD_COUNT},
    protocol::{
        block_io::{block_read_full, block_read_out_of_bound_for_fd, send_fd},
        event::EventType,
    },
    queue::QueueManager,
};

pub enum ProtocolInitializer {
    V2(ProtocolInitializerV2),
    V3(ProtocolInitializerV3),
}

impl ProtocolInitializer {
    pub fn init(&self) -> Result<Option<(Arc<BufferManager>, QueueManager)>, anyhow::Error> {
        match self {
            ProtocolInitializer::V2(v2) => v2.init(),
            ProtocolInitializer::V3(v3) => v3.init(),
        }
    }

    pub fn version(&self) -> u8 {
        match self {
            ProtocolInitializer::V2(_) => ProtocolInitializerV2::version(),
            ProtocolInitializer::V3(_) => ProtocolInitializerV3::version(),
        }
    }
}

pub fn handle_exchange_version(conn_fd: RawFd) -> Result<(), anyhow::Error> {
    let mut resp_header = Header([0; HEADER_SIZE].as_mut_ptr());
    resp_header.encode(
        HEADER_SIZE as u32,
        MAX_SUPPORT_PROTO_VERSION,
        EventType::TYPE_EXCHANGE_PROTO_VERSION,
    );
    protocol_trace(&resp_header, &[], true);
    block_write_full(conn_fd, unsafe {
        std::slice::from_raw_parts(resp_header.0, HEADER_SIZE)
    })
}

pub fn handle_share_memory_by_memfd(
    conn_fd: RawFd,
    h: &Header,
    version: u8,
) -> Result<Option<(Arc<BufferManager>, QueueManager)>, anyhow::Error> {
    tracing::info!("recv memfd, header:{}", h);
    // 1.recv shm metadata
    let mut body = vec![0u8; h.length() as usize - HEADER_SIZE];
    block_read_full(conn_fd, &mut body)
        .map_err(|err| anyhow!("read shm metadata failed, reason:{}", err))?;
    let (buffer_path, queue_path) = extract_shm_metadata(&body);

    // 2.send AckReadyRecvFD
    let mut ack = Header([0; HEADER_SIZE].as_mut_ptr());
    ack.encode(
        HEADER_SIZE as u32,
        version,
        EventType::TYPE_ACK_READY_RECV_FD,
    );
    tracing::info!("response typeAckReadyRecvFD");
    block_write_full(conn_fd, unsafe {
        std::slice::from_raw_parts(ack.0, HEADER_SIZE)
    })
    .map_err(|err| anyhow!("send ack TypeAckReadyRecvFD failed reason:{}", err))?;
    tracing::info!("TypeAckReadyRecvFD send finished");

    // 3. recv fd
    tracing::info!("send ack finished");
    let fds = block_read_out_of_bound_for_fd(conn_fd)?;
    if fds.len() < MEMFD_COUNT {
        tracing::warn!("ParseUnixRights len fds:{}", fds.len());
        return Err(anyhow!("the number of memfd received is wrong"));
    }

    let (buffer_fd, queue_fd) = (fds[0], fds[1]);
    tracing::info!(
        "recv memfd, buffer_path:{} queue_path:{} buffer_fd:{} queue_fd:{}",
        buffer_path,
        queue_path,
        buffer_fd,
        queue_fd
    );

    // 4. mapping share memory
    let qm = QueueManager::mapping_with_memfd(queue_path, queue_fd)?;
    let bm = BufferManager::get_with_memfd(buffer_path, buffer_fd, 0, false, &mut []).inspect_err(
        |_| {
            qm.unmap();
        },
    )?;
    tracing::info!("handle_share_memory_by_memfd done");
    Ok(Some((bm, qm)))
}

pub fn send_memfd_to_peer(
    conn_fd: RawFd,
    buffer_path: &str,
    buffer_fd: RawFd,
    queue_path: &str,
    queue_fd: RawFd,
    version: u8,
) -> Result<Option<(Arc<BufferManager>, QueueManager)>, anyhow::Error> {
    let mut event = generate_shm_metadata(
        EventType::TYPE_SHARE_MEMORY_BY_MEMFD,
        buffer_path,
        queue_path,
        version,
    );
    let h = Header(event.as_mut_ptr());
    tracing::info!(
        "send_memfd_to_peer buffer fd:{} queue fd:{} header:{}",
        buffer_fd,
        queue_fd,
        h
    );
    protocol_trace(&h, &event[HEADER_SIZE..], true);

    block_write_full(conn_fd, &event)?;
    let mut buf = [0u8; HEADER_SIZE];
    wait_event_header(conn_fd, EventType::TYPE_ACK_READY_RECV_FD, &mut buf)?;
    send_fd(conn_fd, &[buffer_fd, queue_fd])?;
    Ok(None)
}

pub fn wait_event_header(
    conn_fd: RawFd,
    expect_event_type: EventType,
    buf: &mut [u8],
) -> Result<Header, anyhow::Error> {
    let h = block_read_event_header(conn_fd, buf)?;
    if h.msg_type() != expect_event_type {
        return Err(anyhow!(
            "expect event_type:{} {}, but:{}",
            expect_event_type.inner(),
            expect_event_type,
            h.msg_type(),
        ));
    }

    Ok(h)
}

pub fn block_read_event_header(conn_fd: RawFd, buf: &mut [u8]) -> Result<Header, anyhow::Error> {
    block_read_full(conn_fd, buf)?;
    let h = Header(buf.as_mut_ptr());
    check_event_valid(&h)?;
    protocol_trace(&h, &[], false);
    Ok(h)
}

pub fn handle_share_memory_by_file_path(
    conn_fd: RawFd,
    hdr: &Header,
) -> Result<Option<(Arc<BufferManager>, QueueManager)>, anyhow::Error> {
    tracing::info!("handle_share_memory_by_file_path head:{:?}", hdr);
    let mut body = vec![0u8; hdr.length() as usize - HEADER_SIZE];
    if let Err(err) = block_read_full(conn_fd, body.as_mut()) {
        if !err.to_string().eq("EOF")
            && !err.to_string().contains("closed")
            && !err.to_string().contains("reset by peer")
        {
            tracing::error!("shmipc: failed to read pathlen: {}", err);
        }
        return Err(err);
    }
    let (buffer_path, queue_path) = extract_shm_metadata(&body);
    let qm = QueueManager::mapping_with_file(queue_path).map_err(|err| {
        anyhow!(
            "handle_share_memory_by_file_path mappingQueueManager failed,queuePathLen:{} path:{} \
             err={}",
            queue_path.len(),
            queue_path,
            err
        )
    })?;

    let bm = BufferManager::get_with_file(buffer_path, 0, false, &mut []).map_err(|err| {
        qm.unmap();
        anyhow!(
            "handle_share_memory_by_file_path mapping_buffer_manager failed, buffer_path_len:{} \
             path:{} err={}",
            buffer_path.len(),
            buffer_path,
            err
        )
    })?;

    Ok(Some((bm, qm)))
}

fn extract_shm_metadata(body: &[u8]) -> (&str, &str) {
    let mut offset = 0;
    let queue_path_len = u16::from_be_bytes([body[offset], body[offset + 1]]);
    offset += 2;
    let queue_path =
        unsafe { std::str::from_utf8_unchecked(&body[offset..offset + queue_path_len as usize]) };
    offset += queue_path_len as usize;

    let buffer_path_len = u16::from_be_bytes([body[offset], body[offset + 1]]);
    offset += 2;
    let buffer_path =
        unsafe { std::str::from_utf8_unchecked(&body[offset..offset + buffer_path_len as usize]) };
    (buffer_path, queue_path)
}

pub fn send_share_memory_by_file_path(
    conn_fd: RawFd,
    buffer_path: &str,
    queue_path: &str,
    version: u8,
) -> Result<Option<(Arc<BufferManager>, QueueManager)>, anyhow::Error> {
    let mut data = generate_shm_metadata(
        EventType::TYPE_SHARE_MEMORY_BY_FILE_PATH,
        buffer_path,
        queue_path,
        version,
    );
    let h = Header(data.as_mut_ptr());
    protocol_trace(&h, &data[HEADER_SIZE..], true);
    block_write_full(conn_fd, &data)?;
    Ok(None)
}

fn generate_shm_metadata(
    event_type: EventType,
    buffer_path: &str,
    queue_path: &str,
    version: u8,
) -> Vec<u8> {
    let mut data = vec![0u8; HEADER_SIZE + 2 + buffer_path.len() + 2 + queue_path.len()];
    let mut h = Header(data.as_mut_ptr());
    h.encode(data.len() as u32, version, event_type);
    let mut offset = HEADER_SIZE;
    // queue share memory path
    data[offset..offset + 2].copy_from_slice(&(queue_path.len() as u16).to_be_bytes());
    offset += 2;
    data[offset..offset + queue_path.len()].copy_from_slice(queue_path.as_bytes());
    offset += queue_path.len();
    // buffer share memory path
    data[offset..offset + 2].copy_from_slice(&(buffer_path.len() as u16).to_be_bytes());
    offset += 2;
    data[offset..offset + buffer_path.len()].copy_from_slice(buffer_path.as_bytes());
    data
}
