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
    cell::UnsafeCell,
    future::Future,
    ptr::copy_nonoverlapping,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::anyhow;
use tokio::sync::Notify;
use volo::net::Address;

use crate::{
    buffer::{BufferReader, BufferWriter, buf::Buf, linked::LinkedBuffer, slice::BufferSlice},
    consts::MAGIC_NUMBER,
    error::Error,
    protocol::event::{EventType, FallbackDataEvent},
    queue::QueueElement,
    session::Session,
};

pub const STREAM_OPENED: u32 = 0;
pub const STREAM_CLOSED: u32 = 1;
pub const STREAM_HALF_CLOSED: u32 = 2;

/// Stream is used to represent a logical stream within a session
#[derive(Debug, Clone)]
pub struct Stream {
    inner: Arc<StreamInner>,
    id: u32,
    session: Session,
    session_id: usize,
}

#[derive(Debug)]
pub struct StreamInner {
    recv_buf: UnsafeCell<LinkedBuffer>,
    send_buf: UnsafeCell<LinkedBuffer>,
    pending_data: Mutex<Vec<BufferSliceWrapper>>,
    state: AtomicU32,
    close_notify: Notify,
    recv_notify: Notify,
    // if in_fallback_state is set to true, sending should use uds
    in_fallback_state: AtomicBool,
}

unsafe impl Sync for StreamInner {}

impl Stream {
    /// Construct a new stream within a given session for an ID
    pub fn new(id: u32, session_id: usize, session: Session) -> Self {
        let recv_notify = Notify::new();
        let close_notify = Notify::new();
        Self {
            id,
            session_id,
            inner: Arc::new(StreamInner {
                recv_buf: UnsafeCell::new(LinkedBuffer::new(session.shared.buffer_manager.clone())),
                send_buf: UnsafeCell::new(LinkedBuffer::new(session.shared.buffer_manager.clone())),
                pending_data: Mutex::new(Vec::new()),
                state: AtomicU32::new(STREAM_OPENED),
                close_notify,
                recv_notify,
                in_fallback_state: AtomicBool::new(false),
            }),
            session,
        }
    }

    #[allow(clippy::mut_from_ref)]
    #[inline]
    pub fn recv_buf(&self) -> &mut LinkedBuffer {
        unsafe { &mut *self.inner.recv_buf.get() }
    }

    #[allow(clippy::mut_from_ref)]
    #[inline]
    pub fn send_buf(&self) -> &mut LinkedBuffer {
        unsafe { &mut *self.inner.send_buf.get() }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn stream_id(&self) -> u32 {
        self.id
    }

    /// return underlying read buffer, whose'size >= minSize.
    ///
    /// if current's size is not enough, which will block until
    /// the read buffer's size greater than minSize.
    async fn read_more(&self, min_size: usize, buf: &mut LinkedBuffer) -> Result<(), Error> {
        self.move_pending_data(buf);
        let recv_len = buf.len();
        if recv_len >= min_size {
            return Ok(());
        }

        if recv_len == 0 && self.inner.state.load(Ordering::SeqCst) != STREAM_OPENED {
            return Err(Error::EndOfStream);
        }
        loop {
            tokio::select! {
                _ = self.inner.recv_notify.notified() => {
                    self.move_pending_data(buf);
                    if buf.len() >= min_size {
                        return Ok(());
                    }
                }
                _ = self.inner.close_notify.notified() => {
                    self.move_pending_data(buf);
                    if buf.len() >= min_size {
                        return Ok(());
                    }

                    if self.inner.state.load(Ordering::SeqCst) == STREAM_HALF_CLOSED {
                        return Err(Error::EndOfStream);
                    }

                    return Err(Error::StreamClosed);
                }
            }
        }
    }

    fn move_pending_data(&self, buf: &mut LinkedBuffer) {
        let mut pending_data = self.inner.pending_data.lock().unwrap();
        if pending_data.is_empty() {
            return;
        }
        let pre_len = buf.len();
        for data in pending_data.drain(0..) {
            if let Some(fallback_slice) = data.fallback_slice {
                buf.append_buffer_slice(fallback_slice);
                self.inner.in_fallback_state.store(true, Ordering::SeqCst);
                continue;
            }
            let mut offset = data.offset;
            loop {
                match self.session.shared.buffer_manager.read_buffer_slice(offset) {
                    Ok(slice) => {
                        if !slice
                            .buffer_header
                            .as_ref()
                            .map(|h| h.has_next())
                            .unwrap_or(false)
                        {
                            buf.append_buffer_slice(slice);
                            break;
                        }
                        offset = slice.buffer_header.as_ref().unwrap().next_buffer_offset();
                        buf.append_buffer_slice(slice);
                    }
                    Err(err) => {
                        // it means that something bug about protocol occurred, underlying
                        // connection will be closed.
                        tracing::error!("read_buffer_slice error {}", err);
                        break;
                    }
                }
            }
        }
        self.session
            .shared
            .stats
            .in_flow_bytes
            .fetch_add((buf.len() - pre_len) as u64, Ordering::SeqCst);
    }

    pub async fn write_fallback(
        &self,
        stream_status: u32,
        err: Error,
        send_buf: &mut LinkedBuffer,
    ) -> Result<(), Error> {
        tracing::warn!(
            "session {} stream fallback seqID:{} len:{} reason:{}, send_buf.is_from_share_memory: \
             {}",
            self.session.shared.name,
            self.id,
            send_buf.len(),
            err,
            send_buf.is_from_share_memory()
        );
        let mut event = FallbackDataEvent([0u8; 16].as_mut_ptr());
        event.encode(
            send_buf.len() as u32 + 16,
            self.session.shared.communication_version,
            self.id,
            stream_status,
        );
        let mut data = Vec::with_capacity(send_buf.len() + 16);
        data.extend_from_slice(event.as_slice());
        let mut slice = send_buf.slice_list().front();
        while let Some(s) = slice {
            data.extend_from_slice(unsafe {
                std::slice::from_raw_parts(s.data, s.write_index - s.read_index)
            });
            if send_buf
                .slice_list()
                .write()
                .map(|ws| ws == s)
                .unwrap_or(false)
            {
                break;
            }
            slice = s.next();
        }
        send_buf.recycle();
        self.session.open_circuit_breaker().await;
        self.session
            .shared
            .stats
            .fallback_write_count
            .fetch_add(1, Ordering::SeqCst);
        self.session.wait_for_send(None, data).await
    }

    fn clean(&self) {
        self.session
            .on_stream_close(self.id, self.inner.state.load(Ordering::SeqCst));
        self.clean_pending_data();
        self.recv_buf().recycle();
        self.send_buf().recycle();
    }

    fn clean_pending_data(&self) {
        let mut pending_data = self.inner.pending_data.lock().unwrap();
        for data in pending_data.drain(0..) {
            if let Some(fallback_slice) = data.fallback_slice {
                if !fallback_slice.is_from_shm {
                    unsafe {
                        _ = Vec::from_raw_parts(
                            fallback_slice.data,
                            fallback_slice.cap as usize,
                            fallback_slice.cap as usize,
                        )
                    }
                } else {
                    tracing::warn!(
                        "fallback slice is from shm, offset:{}",
                        fallback_slice.offset_in_shm
                    );
                }
                continue;
            }
            match self
                .session
                .shared
                .buffer_manager
                .read_buffer_slice(data.offset)
            {
                Ok(slice) => {
                    self.session.shared.buffer_manager.recycle_buffers(slice);
                }
                Err(err) => {
                    tracing::error!("read_buffer_slice error {}", err);
                    break;
                }
            }
        }
    }

    /// clean the stream's all status for reusing
    pub fn reset(&self) -> Result<(), Error> {
        if self.inner.state.load(Ordering::SeqCst) != STREAM_OPENED {
            return Err(Error::StreamClosed);
        }
        // return error if has any unread data
        let unread_size = self.recv_buf().len();
        if unread_size > 0 {
            return Err(anyhow!("stream had unread data, size:{} ", unread_size).into());
        }

        {
            let pending_data = self.inner.pending_data.lock().unwrap();
            if !pending_data.is_empty() {
                return Err(anyhow!(
                    "stream had unread pending data, unread slice len:{} ",
                    pending_data.len()
                )
                .into());
            }
        }

        self.inner.in_fallback_state.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// release the data previous read and reuse the last share memory slice for next write.
    pub fn release_read_and_reuse(&self) {
        let recv_buf = self.recv_buf();
        let send_buf = self.send_buf();
        recv_buf.release_previous_read_and_reserve();
        if recv_buf.is_empty() && recv_buf.slice_list().size() == 1 {
            std::mem::swap(recv_buf, send_buf);
        }
    }

    // fill_data_to_read_buffer is used to handle a data frame
    pub fn fill_data_to_read_buffer(&self, buf: BufferSliceWrapper) -> Result<(), Error> {
        self.inner.pending_data.lock().unwrap().push(buf);
        // stream had closed, which maybe closed by user due to timeout.
        if self.inner.state.load(Ordering::SeqCst) == STREAM_CLOSED {
            self.clean_pending_data();
            self.recv_buf().recycle();
            return Ok(());
        }
        // Unblock any readers
        self.inner.recv_notify.notify_one();

        Ok(())
    }

    pub fn is_open(&self) -> bool {
        self.inner.state.load(Ordering::SeqCst) == STREAM_OPENED
    }

    pub fn safe_close_notify(&self) {
        self.inner.close_notify.notify_waiters();
    }

    pub fn half_close(&self) {
        if self
            .inner
            .state
            .compare_exchange(
                STREAM_OPENED,
                STREAM_HALF_CLOSED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            self.safe_close_notify();
        }
    }

    pub fn session_id(&self) -> usize {
        self.session_id
    }

    pub fn fallback_state(&self) -> bool {
        self.inner.in_fallback_state.load(Ordering::SeqCst)
    }

    pub async fn reuse(&self) {
        self.session.put_or_close_stream(self.clone()).await;
    }

    pub async fn close(&mut self) -> Result<(), Error> {
        let old_state = self.inner.state.load(Ordering::SeqCst);
        if old_state == STREAM_CLOSED {
            return Ok(());
        }

        if self
            .inner
            .state
            .compare_exchange(old_state, STREAM_CLOSED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.clean();
            if old_state == STREAM_OPENED {
                self.safe_close_notify();

                if self.session.shared.shutdown.load(Ordering::SeqCst) == 1 {
                    return Ok(());
                }
                // notify peer
                match self
                    .session
                    .shared
                    .queue_manager
                    .send_queue
                    .put(QueueElement {
                        seq_id: self.id,
                        offset_in_shm_buf: 0,
                        status: STREAM_CLOSED,
                    }) {
                    Ok(_) => return self.session.wake_up_peer().await,
                    Err(_) => {
                        self.session
                            .shared
                            .stats
                            .queue_full_error_count
                            .fetch_add(1, Ordering::SeqCst);
                        // notify close
                        let mut event = vec![0u8; 12];
                        unsafe {
                            let ptr = event.as_mut_ptr();
                            copy_nonoverlapping(12_u32.to_be_bytes().as_ptr(), ptr, 4);
                            copy_nonoverlapping(
                                MAGIC_NUMBER.to_be_bytes().as_ptr(),
                                ptr.offset(4),
                                2,
                            );
                            *ptr.offset(6) = self.session.shared.communication_version;
                            *ptr.offset(7) = EventType::TYPE_STREAM_CLOSE.inner();
                            copy_nonoverlapping(self.id.to_be_bytes().as_ptr(), ptr.offset(8), 4);
                        }
                        return self.session.wait_for_send(None, event).await;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn peer_addr(&self) -> Option<Address> {
        self.session.shared.peer_addr.clone()
    }
}

pub trait AsyncReadShm {
    fn read_bytes(&mut self, size: usize) -> impl Future<Output = Result<Buf<'_>, Error>> + Send;
    fn peek(&mut self, size: usize) -> impl Future<Output = Result<Buf<'_>, Error>> + Send;
    fn discard(&mut self, size: usize) -> impl Future<Output = Result<usize, Error>> + Send;
    fn release_previous_read(&self);
}

impl<T: AsyncReadShm + Send> AsyncReadShm for &mut T {
    async fn read_bytes(&mut self, size: usize) -> Result<Buf<'_>, Error> {
        (**self).read_bytes(size).await
    }

    async fn peek(&mut self, size: usize) -> Result<Buf<'_>, Error> {
        (**self).peek(size).await
    }

    async fn discard(&mut self, size: usize) -> Result<usize, Error> {
        (**self).discard(size).await
    }

    fn release_previous_read(&self) {
        (**self).release_previous_read();
    }
}

impl AsyncReadShm for Stream {
    async fn read_bytes(&mut self, size: usize) -> Result<Buf<'_>, Error> {
        let buf = self.recv_buf();
        if buf.len() < size {
            tracing::debug!(
                "read_bytes seqID:{} len:{} size:{}",
                self.id,
                buf.len(),
                size
            );
            self.read_more(size, buf).await?;
        }
        buf.read_bytes(size)
    }

    async fn peek(&mut self, size: usize) -> Result<Buf<'_>, Error> {
        let buf = self.recv_buf();
        if buf.len() < size {
            self.read_more(size, buf).await?;
        }
        buf.peek(size)
    }

    async fn discard(&mut self, size: usize) -> Result<usize, Error> {
        let buf = self.recv_buf();
        if buf.len() < size {
            self.read_more(size, buf).await?;
        }
        buf.discard(size)
    }

    fn release_previous_read(&self) {
        self.recv_buf().release_previous_read();
    }
}

pub trait AsyncWriteShm {
    fn reserve(&mut self, size: usize) -> Result<&mut [u8], Error>;
    fn write_bytes(&mut self, data: &[u8]) -> Result<usize, Error>;
    fn flush(&mut self, end_stream: bool) -> impl Future<Output = Result<(), Error>> + Send;
    /// close the stream. after close stream, any operation will return ErrStreamClosed.
    /// unread data will be drained and released.
    fn close(&mut self) -> impl Future<Output = Result<(), Error>> + Send;
}

impl<T: AsyncWriteShm> AsyncWriteShm for &mut T {
    fn reserve(&mut self, size: usize) -> Result<&mut [u8], Error> {
        (**self).reserve(size)
    }

    fn write_bytes(&mut self, data: &[u8]) -> Result<usize, Error> {
        (**self).write_bytes(data)
    }

    fn flush(&mut self, end_stream: bool) -> impl Future<Output = Result<(), Error>> + Send {
        (**self).flush(end_stream)
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Error>> + Send {
        (**self).close()
    }
}

impl AsyncWriteShm for Stream {
    fn reserve(&mut self, size: usize) -> Result<&mut [u8], Error> {
        self.send_buf().reserve(size)
    }

    fn write_bytes(&mut self, data: &[u8]) -> Result<usize, Error> {
        self.send_buf().write_bytes(data)
    }

    async fn flush(&mut self, end_stream: bool) -> Result<(), Error> {
        let send_buf = self.send_buf();
        if send_buf.is_empty() {
            return Ok(());
        }
        self.session
            .shared
            .stats
            .out_flow_bytes
            .fetch_add(send_buf.len() as u64, Ordering::SeqCst);
        let state = self.inner.state.load(Ordering::SeqCst);
        if state != STREAM_OPENED {
            send_buf.recycle();
            return Err(Error::StreamClosed);
        }
        send_buf.done(end_stream);
        // Once we send data using uds, for this stream we will always use uds later to avoid
        // unordering
        if !send_buf.is_from_share_memory() {
            self.inner.in_fallback_state.store(true, Ordering::SeqCst);
        }
        if self.inner.in_fallback_state.load(Ordering::SeqCst) {
            let ret = self
                .write_fallback(state, Error::NoMoreBuffer, send_buf)
                .await;
            send_buf.clean();
            return ret;
        }

        match self
            .session
            .shared
            .queue_manager
            .send_queue
            .put(QueueElement {
                seq_id: self.id,
                offset_in_shm_buf: send_buf.root_buf_offset(),
                status: state,
            }) {
            Ok(_) => {
                let ret = self.session.wake_up_peer().await;
                send_buf.clean();
                return ret;
            }
            Err(err) => match err {
                Error::QueueFull => {
                    self.session
                        .shared
                        .stats
                        .queue_full_error_count
                        .fetch_add(1, Ordering::SeqCst);
                    for _ in 0..10 {
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                                match self.session.shared.queue_manager.send_queue.put(QueueElement {
                                    seq_id: self.id,
                                    offset_in_shm_buf: send_buf.root_buf_offset(),
                                    status: state,
                                }) {
                                    Ok(_) => {
                                        let ret = self.session.wake_up_peer().await;
                                        send_buf.clean();
                                        return ret;
                                    },
                                    Err(err) => {
                                        match err {
                                            Error::QueueFull => continue,
                                            _ => {
                                                send_buf.recycle();
                                                return Err(err);
                                            }
                                        }
                                    }
                                }
                            }
                            _ = self.inner.close_notify.notified() => {
                                send_buf.recycle();
                                return Err(Error::StreamClosed);
                            }
                        }
                    }
                }
                e => {
                    send_buf.recycle();
                    return Err(e);
                }
            },
        }
        Ok(())
    }

    async fn close(&mut self) -> Result<(), Error> {
        self.close().await
    }
}

#[derive(Debug)]
pub struct BufferSliceWrapper {
    pub(crate) fallback_slice: Option<BufferSlice>,
    pub(crate) offset: u32,
}
