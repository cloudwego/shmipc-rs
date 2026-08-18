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
    ptr::copy_nonoverlapping,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{io::ReadBuf, sync::Notify};

use crate::{
    buffer::{Buf, BufferReader, BufferWriter, linked::LinkedBuffer, slice::BufferSlice},
    consts::MAGIC_NUMBER,
    error::Error,
    protocol::event::{EventType, FallbackDataEvent},
    queue::QueueElement,
    session::Session,
};

pub const STREAM_OPENED: u32 = 0;
pub const STREAM_CLOSED: u32 = 1;
pub const STREAM_HALF_CLOSED: u32 = 2;

const QUEUE_FULL_RETRY_COUNT: usize = 10;
const QUEUE_FULL_RETRY_INTERVAL: Duration = Duration::from_millis(10);

async fn retry_queue_put<F>(close_notify: &Notify, mut put: F) -> Result<(), Error>
where
    F: FnMut() -> Result<(), Error>,
{
    for _ in 0..QUEUE_FULL_RETRY_COUNT {
        if tokio::time::timeout(QUEUE_FULL_RETRY_INTERVAL, close_notify.notified())
            .await
            .is_ok()
        {
            return Err(Error::StreamClosed);
        }

        match put() {
            Ok(()) => return Ok(()),
            Err(Error::QueueFull) => continue,
            Err(err) => return Err(err),
        }
    }

    Err(Error::QueueFull)
}

/// Stream is used to represent a logical stream within a session
#[derive(Debug)]
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
    handle_count: AtomicUsize,
}

unsafe impl Sync for StreamInner {}

impl Stream {
    /// Construct a new stream within a given session for an ID
    pub(crate) fn new(id: u32, session_id: usize, session: Session) -> Self {
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
                handle_count: AtomicUsize::new(1),
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

    pub const fn stream_id(&self) -> u32 {
        self.id
    }

    pub(crate) fn shares_inner_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Clone for Stream {
    fn clone(&self) -> Self {
        self.inner.handle_count.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::clone(&self.inner),
            id: self.id,
            session: self.session.clone(),
            session_id: self.session_id,
        }
    }
}

impl Stream {
    fn check_read_ready(&self, min_size: usize, buf: &LinkedBuffer) -> Result<bool, Error> {
        let state = self.inner.state.load(Ordering::SeqCst);
        if state == STREAM_CLOSED {
            return Err(Error::StreamClosed);
        }
        if buf.len() >= min_size {
            return Ok(true);
        }
        if state == STREAM_HALF_CLOSED {
            return Err(Error::EndOfStream);
        }
        Ok(false)
    }

    /// Wait until the underlying read buffer contains at least `min_size` bytes.
    async fn read_more(&self, min_size: usize, buf: &mut LinkedBuffer) -> Result<(), Error> {
        loop {
            self.move_pending_data(buf);
            if self.check_read_ready(min_size, buf)? {
                return Ok(());
            }

            let mut recv_notified = std::pin::pin!(self.inner.recv_notify.notified());
            let mut close_notified = std::pin::pin!(self.inner.close_notify.notified());
            recv_notified.as_mut().enable();
            close_notified.as_mut().enable();

            // Register both notifications before checking state and pending data again. This
            // prevents `notify_waiters` from being lost between the check and the await.
            self.move_pending_data(buf);
            if self.check_read_ready(min_size, buf)? {
                return Ok(());
            }

            _ = futures::future::select(recv_notified, close_notified).await;
        }
    }

    fn move_pending_data(&self, buf: &mut LinkedBuffer) {
        let mut pending_data = self.inner.pending_data.lock().unwrap();
        if pending_data.is_empty() {
            return;
        }
        let pre_len = buf.len();
        for mut data in pending_data.drain(0..) {
            if let Some(fallback_slice) = data.fallback_slice.take() {
                buf.append_buffer_slice(fallback_slice);
                self.inner.in_fallback_state.store(true, Ordering::SeqCst);
                continue;
            }
            let mut offset = data.offset;
            loop {
                let slice = match self.session.shared.buffer_manager.read_buffer_slice(offset) {
                    Ok(slice) => slice,
                    Err(err) => {
                        tracing::error!("read_buffer_slice error {err}");
                        break;
                    }
                };
                let has_next = slice
                    .buffer_header
                    .as_ref()
                    .map(|h| h.has_next())
                    .unwrap_or(false);
                if slice.size() == 0 {
                    let next_offset = slice.buffer_header.as_ref().unwrap().next_buffer_offset();
                    self.session.shared.buffer_manager.recycle_buffer(slice);
                    if has_next {
                        offset = next_offset;
                        continue;
                    } else {
                        if let Some(back) = buf.slice_list().back()
                            && let Some(bh) = &back.buffer_header
                        {
                            bh.clear_flag();
                            bh.set_in_used();
                        }
                        break;
                    }
                }
                if !has_next {
                    buf.append_buffer_slice(slice);
                    break;
                }
                offset = slice.buffer_header.as_ref().unwrap().next_buffer_offset();
                buf.append_buffer_slice(slice);
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
        let buf_len = send_buf.len();
        if buf_len > u32::MAX as usize - 16 {
            send_buf.recycle();
            return Err(Error::NoMoreBuffer);
        }
        tracing::warn!(
            "session {} stream fallback seqID:{} len:{} reason:{}, send_buf.is_from_share_memory: \
             {}",
            self.session.shared.name,
            self.id,
            buf_len,
            err,
            send_buf.is_from_share_memory()
        );
        let mut event = FallbackDataEvent([0u8; 16].as_mut_ptr());
        event.encode(
            buf_len as u32 + 16,
            self.session.shared.msg_version,
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
        self.clean_local_buffers();
    }

    fn clean_local_buffers(&self) {
        self.clean_pending_data();
        self.recv_buf().recycle();
        self.send_buf().recycle();
    }

    fn clean_pending_data(&self) {
        let mut pending_data = self.inner.pending_data.lock().unwrap();
        for mut data in pending_data.drain(0..) {
            if let Some(fallback_slice) = data.fallback_slice.take() {
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
            return Err(Error::StreamHasUnreadData(unread_size));
        }

        let pending_data_len = self.inner.pending_data.lock().unwrap().len();
        if pending_data_len > 0 {
            return Err(Error::StreamHasPendingData(pending_data_len));
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

    pub const fn session_id(&self) -> usize {
        self.session_id
    }

    pub fn fallback_state(&self) -> bool {
        self.inner.in_fallback_state.load(Ordering::SeqCst)
    }

    pub async fn reuse(&self) {
        self.session.put_or_close_stream(self.clone()).await;
    }

    pub async fn close(&mut self) -> Result<(), Error> {
        let old_state = self.inner.state.swap(STREAM_CLOSED, Ordering::Release);
        if old_state == STREAM_CLOSED {
            return Ok(());
        }
        self.clean();
        if old_state != STREAM_OPENED {
            return Ok(());
        }
        self.safe_close_notify();

        if self.session.shared.shutdown.load(Ordering::SeqCst) == 1 {
            return Ok(());
        }
        if !self.inner.in_fallback_state.load(Ordering::SeqCst) {
            if self
                .session
                .shared
                .queue_manager
                .send_queue
                .put(QueueElement {
                    seq_id: self.id,
                    offset_in_shm_buf: 0,
                    status: STREAM_CLOSED,
                })
                .is_ok()
            {
                return self.session.wake_up_peer().await;
            }
            self.session
                .shared
                .stats
                .queue_full_error_count
                .fetch_add(1, Ordering::SeqCst);
        }

        // notify close
        let mut event = vec![0u8; 12];
        unsafe {
            let ptr = event.as_mut_ptr();
            copy_nonoverlapping(12_u32.to_be_bytes().as_ptr(), ptr, 4);
            copy_nonoverlapping(MAGIC_NUMBER.to_be_bytes().as_ptr(), ptr.offset(4), 2);
            *ptr.offset(6) = self.session.shared.msg_version;
            *ptr.offset(7) = EventType::TYPE_STREAM_CLOSE.inner();
            copy_nonoverlapping(self.id.to_be_bytes().as_ptr(), ptr.offset(8), 4);
        }
        self.session.wait_for_send(None, event).await
    }

    /// Read the first non-empty contiguous shm buffer chunk.
    ///
    /// The returned chunk never spans multiple underlying buffer slices.
    ///
    /// To read an exact length, refer to [`Stream::read_exact_bytes`].
    ///
    /// Call [`Stream::release_read_and_reuse`] after processing a response to make consumed
    /// storage available for stream reuse immediately. Any outstanding zero-copy buffer keeps
    /// only its own slice pinned until it is dropped.
    pub async fn read_chunk(&mut self) -> Result<Buf<'_>, Error> {
        if self.inner.state.load(Ordering::SeqCst) == STREAM_CLOSED {
            return Err(Error::StreamClosed);
        }
        let buf = self.recv_buf();
        if buf.is_empty() {
            tracing::debug!("read_chunk seqID:{}", self.id);
            self.read_more(1, buf).await?;
        }
        buf.read_chunk()
    }

    pub(crate) async fn wait_readable(&mut self) -> Result<(), Error> {
        if self.inner.state.load(Ordering::SeqCst) == STREAM_CLOSED {
            return Err(Error::StreamClosed);
        }
        let buf = self.recv_buf();
        if buf.is_empty() {
            tracing::debug!("wait_readable seqID:{}", self.id);
            self.read_more(1, buf).await?;
        }
        Ok(())
    }

    pub(crate) fn read_available_into(
        &mut self,
        dst: &mut ReadBuf<'_>,
        max_slices: usize,
    ) -> Result<usize, Error> {
        let buf = self.recv_buf();
        if !self.check_read_ready(1, buf)? {
            return Err(Error::NotEnoughData);
        }
        buf.read_available_into(dst, max_slices)
    }

    /// Read exactly `size` bytes as one contiguous buffer.
    ///
    /// A single-slice result is zero-copy. A result spanning slices is coalesced into owned memory.
    /// If the peer reaches EOF before `size` bytes are available, buffered data is left unconsumed.
    ///
    /// To read the next available contiguous chunk, refer to [`Stream::read_chunk`].
    pub async fn read_exact_bytes(&mut self, size: usize) -> Result<Buf<'_>, Error> {
        if size == 0 {
            return self.recv_buf().read_exact_bytes(0);
        }
        if self.inner.state.load(Ordering::SeqCst) == STREAM_CLOSED {
            return Err(Error::StreamClosed);
        }
        let buf = self.recv_buf();
        if buf.len() < size {
            tracing::debug!(
                "read_exact_bytes seqID:{} len:{} size:{}",
                self.id,
                buf.len(),
                size
            );
            self.read_more(size, buf).await?;
        }
        buf.read_exact_bytes(size)
    }

    pub async fn peek(&mut self, size: usize) -> Result<Buf<'_>, Error> {
        let buf = self.recv_buf();
        if buf.len() < size {
            self.read_more(size, buf).await?;
        }
        buf.peek(size)
    }

    pub async fn discard(&mut self, size: usize) -> Result<usize, Error> {
        let buf = self.recv_buf();
        if buf.len() < size {
            self.read_more(size, buf).await?;
        }
        buf.discard(size)
    }

    pub fn reserve(&mut self, size: usize) -> Result<&mut [u8], Error> {
        self.send_buf().reserve(size)
    }

    pub fn write_bytes(&mut self, data: &[u8]) -> Result<usize, Error> {
        self.send_buf().write_bytes(data)
    }

    pub async fn flush(&mut self, end_stream: bool) -> Result<(), Error> {
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
            Err(Error::QueueFull) => {}
            Err(err) => {
                send_buf.recycle();
                return Err(err);
            }
        }
        self.session
            .shared
            .stats
            .queue_full_error_count
            .fetch_add(1, Ordering::SeqCst);
        let root_buf_offset = send_buf.root_buf_offset();
        match retry_queue_put(&self.inner.close_notify, || {
            self.session
                .shared
                .queue_manager
                .send_queue
                .put(QueueElement {
                    seq_id: self.id,
                    offset_in_shm_buf: root_buf_offset,
                    status: state,
                })
        })
        .await
        {
            Ok(()) => {
                let ret = self.session.wake_up_peer().await;
                send_buf.clean();
                ret
            }
            // The queue never took ownership, so retain the buffer for a caller retry.
            Err(Error::QueueFull) => Err(Error::QueueFull),
            Err(err) => {
                send_buf.recycle();
                Err(err)
            }
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // `LinkedBuffer` and fallback slices own raw allocations and therefore cannot clean
        // themselves up. A peer close removes the stream from the session map without calling
        // `clean`, so release any unread data when the final stream handle goes away.
        if self.inner.handle_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.clean_local_buffers();
        }
    }
}

#[derive(Debug)]
pub struct BufferSliceWrapper {
    pub(crate) fallback_slice: Option<BufferSlice>,
    pub(crate) offset: u32,
}

impl Drop for BufferSliceWrapper {
    fn drop(&mut self) {
        let Some(fallback_slice) = self.fallback_slice.take() else {
            return;
        };
        if fallback_slice.is_from_shm {
            tracing::warn!(
                "fallback slice is from shm, offset:{}",
                fallback_slice.offset_in_shm
            );
            return;
        }
        unsafe {
            _ = Vec::from_raw_parts(
                fallback_slice.data,
                fallback_slice.cap as usize,
                fallback_slice.cap as usize,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn retry_queue_put_reports_persistent_queue_full() {
        let close_notify = Notify::new();
        let attempts = AtomicUsize::new(0);

        let err = retry_queue_put(&close_notify, || {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err(Error::QueueFull)
        })
        .await
        .unwrap_err();

        assert!(matches!(err, Error::QueueFull));
        assert_eq!(attempts.load(Ordering::Relaxed), QUEUE_FULL_RETRY_COUNT);
    }
}
