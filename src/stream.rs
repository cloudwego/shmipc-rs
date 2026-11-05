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
    pin::pin,
    ptr::copy_nonoverlapping,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use tokio::{io::ReadBuf, sync::Notify};

use crate::{
    buffer::{
        Buf, BufferReader, BufferWriter, OutChunk, V5Inbox, V5ReadBuffer, V5WriteBuffer,
        linked::LinkedBuffer, slice::BufferSlice,
    },
    consts::{HEADER_SIZE, MAGIC_NUMBER, MAX_CONTROL_FRAME_SIZE},
    error::Error,
    protocol::event::{EventType, FallbackDataEvent},
    queue::QueueElement,
    session::Session,
    stats::DynShmStats,
    v5::{
        mpsc::PushError,
        queue::{Element, OP_CLOSE_STREAM, OP_STREAM_DATA},
    },
};

pub const STREAM_OPENED: u32 = 0;
pub const STREAM_CLOSED: u32 = 1;
pub const STREAM_HALF_CLOSED: u32 = 2;

const QUEUE_FULL_RETRY_COUNT: usize = 10;
const QUEUE_FULL_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const V5_QUEUE_FULL_RETRY_INTERVAL: Duration = Duration::from_micros(100);

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

/// The read half, which is a chain of shared-memory slices under V2 to V4 and a chain of
/// DynShm blocks under V5.
#[derive(Debug)]
enum RecvSide {
    V4 {
        buf: UnsafeCell<LinkedBuffer>,
        pending: Mutex<Vec<BufferSliceWrapper>>,
    },
    V5 {
        buf: UnsafeCell<V5ReadBuffer>,
        inbox: Arc<V5Inbox>,
    },
}

/// Whichever read half a stream has, so a reader operation runs on the concrete buffer
/// type instead of through a trait object.
enum Reader<'a> {
    V4(&'a mut LinkedBuffer),
    V5(&'a mut V5ReadBuffer),
}

impl BufferReader for Reader<'_> {
    fn read_chunk(&mut self) -> Result<Buf, Error> {
        match self {
            Self::V4(reader) => reader.read_chunk(),
            Self::V5(reader) => reader.read_chunk(),
        }
    }

    fn read_exact_bytes(&mut self, size: usize) -> Result<Buf, Error> {
        match self {
            Self::V4(reader) => reader.read_exact_bytes(size),
            Self::V5(reader) => reader.read_exact_bytes(size),
        }
    }

    fn peek(&mut self, size: usize) -> Result<Buf, Error> {
        match self {
            Self::V4(reader) => reader.peek(size),
            Self::V5(reader) => reader.peek(size),
        }
    }

    fn discard(&mut self, size: usize) -> Result<usize, Error> {
        match self {
            Self::V4(reader) => reader.discard(size),
            Self::V5(reader) => reader.discard(size),
        }
    }

    fn release_previous_read(&mut self) {
        match self {
            Self::V4(reader) => reader.release_previous_read(),
            Self::V5(reader) => reader.release_previous_read(),
        }
    }
}

/// The write half, matching whichever protocol the session negotiated.
#[derive(Debug)]
enum SendSide {
    V4(UnsafeCell<LinkedBuffer>),
    V5(UnsafeCell<V5WriteBuffer>),
}

/// The slice of a stream that its buffers are allowed to see.
#[derive(Debug)]
pub struct StreamShared {
    pub id: u32,
    pub state: AtomicU32,
    /// Sticky: once a flush has fallen back to the unix socket, later writes on this stream
    /// must not go back to shared memory, or the peer would receive them out of order.
    pub in_fallback: AtomicBool,
}

impl StreamShared {
    /// A stream starts open and on the shared-memory path.
    pub fn new(id: u32) -> Arc<Self> {
        Arc::new(Self {
            id,
            state: AtomicU32::new(STREAM_OPENED),
            in_fallback: AtomicBool::new(false),
        })
    }

    /// Whether the stream is still accepting writes, which the buffers check before reserving.
    pub fn is_open(&self) -> bool {
        self.state.load(Ordering::Acquire) == STREAM_OPENED
    }

    /// Whether writes on this stream have to keep going over the unix socket.
    pub fn in_fallback(&self) -> bool {
        self.in_fallback.load(Ordering::Acquire)
    }

    /// Pin this stream's writes to the unix socket. One-way, and never undone short of a reset.
    pub fn set_fallback(&self) {
        self.in_fallback.store(true, Ordering::Release);
    }
}

#[derive(Debug)]
pub struct StreamInner {
    recv: RecvSide,
    send: SendSide,
    /// The stream state the V5 buffers read directly.
    shared: Arc<StreamShared>,
    close_notify: Notify,
    recv_notify: Notify,
    handle_count: AtomicUsize,
}

unsafe impl Sync for StreamInner {}

impl Stream {
    /// Construct a new stream within a given session for an ID
    pub(crate) fn new(id: u32, session_id: usize, session: Session) -> Self {
        let recv_notify = Notify::new();
        let close_notify = Notify::new();
        let shared = StreamShared::new(id);

        let (recv, send) = match &session.shared.v5 {
            Some(v5) => {
                let inbox = V5Inbox::new(v5.peer.dynshm.clone(), Arc::clone(&shared));
                let recv = V5ReadBuffer::new(Arc::clone(&inbox), session.shared.stats.clone());
                let send = V5WriteBuffer::new(
                    v5.own.dynshm.clone(),
                    Arc::clone(&shared),
                    session.shared.stats.clone(),
                );
                (
                    RecvSide::V5 {
                        buf: UnsafeCell::new(recv),
                        inbox,
                    },
                    SendSide::V5(UnsafeCell::new(send)),
                )
            }
            None => (
                RecvSide::V4 {
                    buf: UnsafeCell::new(LinkedBuffer::new(session.shared.buffers().clone())),
                    pending: Mutex::new(Vec::new()),
                },
                SendSide::V4(UnsafeCell::new(LinkedBuffer::new(
                    session.shared.buffers().clone(),
                ))),
            ),
        };

        Self {
            id,
            session_id,
            inner: Arc::new(StreamInner {
                recv,
                send,
                shared,
                close_notify,
                recv_notify,
                handle_count: AtomicUsize::new(1),
            }),
            session,
        }
    }

    /// Whether this stream belongs to a V5 session, which decides which half of every pair of
    /// buffers below is live.
    #[inline]
    fn is_v5(&self) -> bool {
        matches!(self.inner.recv, RecvSide::V5 { .. })
    }

    /// The V5 write half, or `None` on a V2 to V4 session.
    #[allow(clippy::mut_from_ref)]
    #[inline]
    fn v5_send_buf(&self) -> Option<&mut V5WriteBuffer> {
        match &self.inner.send {
            SendSide::V5(buf) => Some(unsafe { &mut *buf.get() }),
            SendSide::V4(_) => None,
        }
    }

    /// The inbox the session's event loop delivers V5 offsets into.
    pub(crate) fn v5_inbox(&self) -> Option<&Arc<V5Inbox>> {
        match &self.inner.recv {
            RecvSide::V5 { inbox, .. } => Some(inbox),
            RecvSide::V4 { .. } => None,
        }
    }

    /// The V2 to V4 read buffer.
    ///
    /// Panics on a V5 session, where the read half is a [`V5ReadBuffer`].
    #[allow(clippy::mut_from_ref)]
    #[inline]
    pub fn recv_buf(&self) -> &mut LinkedBuffer {
        match &self.inner.recv {
            RecvSide::V4 { buf, .. } => unsafe { &mut *buf.get() },
            RecvSide::V5 { .. } => panic!("recv_buf is not available on a V5 stream"),
        }
    }

    /// The V2 to V4 write buffer.
    ///
    /// Panics on a V5 session, where the write half is a [`V5WriteBuffer`].
    #[allow(clippy::mut_from_ref)]
    #[inline]
    pub fn send_buf(&self) -> &mut LinkedBuffer {
        match &self.inner.send {
            SendSide::V4(buf) => unsafe { &mut *buf.get() },
            SendSide::V5(_) => panic!("send_buf is not available on a V5 stream"),
        }
    }

    /// The number of unread bytes, whichever protocol backs the read half.
    #[inline]
    fn recv_len(&self) -> usize {
        match &self.inner.recv {
            RecvSide::V4 { buf, .. } => unsafe { &*buf.get() }.len(),
            RecvSide::V5 { buf, .. } => unsafe { &*buf.get() }.len(),
        }
    }

    /// Run a [`BufferReader`] operation against whichever read half this stream has.
    #[inline]
    fn with_reader<R>(&self, op: impl FnOnce(&mut Reader<'_>) -> R) -> R {
        match &self.inner.recv {
            RecvSide::V4 { buf, .. } => op(&mut Reader::V4(unsafe { &mut *buf.get() })),
            RecvSide::V5 { buf, .. } => op(&mut Reader::V5(unsafe { &mut *buf.get() })),
        }
    }

    pub const fn stream_id(&self) -> u32 {
        self.id
    }

    pub fn v5_dynshm_stats(&self) -> Option<DynShmStats> {
        self.session.v5_dynshm_stats()
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
    /// Whether a read of `min_size` can be served now.
    ///
    /// A closed stream is an error, and a half-closed one that will never reach `min_size` is
    /// [`Error::EndOfStream`] rather than a wait that could not end.
    fn check_read_ready(&self, min_size: usize) -> Result<bool, Error> {
        let state = self.inner.shared.state.load(Ordering::SeqCst);
        if state == STREAM_CLOSED {
            return Err(Error::StreamClosed);
        }
        if self.recv_len() >= min_size {
            return Ok(true);
        }
        if state == STREAM_HALF_CLOSED {
            return Err(Error::EndOfStream);
        }
        Ok(false)
    }

    /// Convert whatever the event loop has delivered into readable bytes.
    async fn take_delivered(&self) -> Result<(), Error> {
        match &self.inner.recv {
            RecvSide::V4 { buf, pending } => {
                self.move_pending_data(pending, unsafe { &mut *buf.get() });
                Ok(())
            }
            RecvSide::V5 { buf, .. } => {
                // A malformed chain surfaces here rather than in the event loop, because the
                // loop only ever handled offsets and never walked them.
                let added = match unsafe { &mut *buf.get() }.drain_pending() {
                    Ok(added) => added,
                    Err(err) => {
                        self.session.close_corrupt(self.id, &err).await;
                        return Err(err);
                    }
                };
                if added > 0 {
                    self.session
                        .shared
                        .stats
                        .in_flow_bytes
                        .fetch_add(added as u64, Ordering::SeqCst);
                }
                Ok(())
            }
        }
    }

    /// Wait until the underlying read buffer contains at least `min_size` bytes.
    async fn read_more(&self, min_size: usize) -> Result<(), Error> {
        loop {
            self.take_delivered().await?;
            if self.check_read_ready(min_size)? {
                return Ok(());
            }

            let mut recv_notified = std::pin::pin!(self.inner.recv_notify.notified());
            let mut close_notified = std::pin::pin!(self.inner.close_notify.notified());
            recv_notified.as_mut().enable();
            close_notified.as_mut().enable();

            // Register both notifications before checking state and pending data again. This
            // prevents `notify_waiters` from being lost between the check and the await.
            self.take_delivered().await?;
            if self.check_read_ready(min_size)? {
                return Ok(());
            }

            _ = futures::future::select(recv_notified, close_notified).await;
        }
    }

    fn move_pending_data(&self, pending: &Mutex<Vec<BufferSliceWrapper>>, buf: &mut LinkedBuffer) {
        let mut pending_data = pending.lock().unwrap();
        if pending_data.is_empty() {
            return;
        }
        let pre_len = buf.len();
        for mut data in pending_data.drain(0..) {
            if let Some(fallback_slice) = data.fallback_slice.take() {
                buf.append_buffer_slice(fallback_slice);
                self.inner.shared.in_fallback.store(true, Ordering::SeqCst);
                continue;
            }
            let mut offset = data.offset;
            loop {
                let slice = match self.session.shared.buffers().read_buffer_slice(offset) {
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
                    self.session.shared.buffers().recycle_buffer(slice);
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
        if buf_len > MAX_CONTROL_FRAME_SIZE - 16 {
            send_buf.recycle();
            return Err(Error::SizeTooLarge);
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
            .on_stream_close(self.id, self.inner.shared.state.load(Ordering::SeqCst));
        // Defer V5 buffer cleanup until the last handle is dropped as they may still be in use.
        if self.is_v5() && self.session.is_closed() {
            return;
        }
        self.clean_local_buffers();
    }

    fn clean_local_buffers(&self) {
        self.clean_pending_data();
        match &self.inner.recv {
            RecvSide::V4 { buf, .. } => unsafe { &mut *buf.get() }.recycle(),
            RecvSide::V5 { buf, .. } => {
                let buf = unsafe { &mut *buf.get() };
                let _ = buf.drain_pending();
                let _ = buf.discard(buf.len());
            }
        }
        match &self.inner.send {
            SendSide::V4(_) => self.send_buf().recycle(),
            SendSide::V5(_) => self.v5_send_buf().expect("v5 send side").discard(),
        }
    }

    /// Release anything the event loop delivered and no reader took.
    ///
    /// V5 has nothing to do here: its inbox frees the chains it still holds when it drops.
    fn clean_pending_data(&self) {
        let RecvSide::V4 { pending, .. } = &self.inner.recv else {
            return;
        };
        let mut pending_data = pending.lock().unwrap();
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
            match self.session.shared.buffers().read_buffer_slice(data.offset) {
                Ok(slice) => {
                    self.session.shared.buffers().recycle_buffers(slice);
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
        if self.inner.shared.state.load(Ordering::SeqCst) != STREAM_OPENED {
            return Err(Error::StreamClosed);
        }
        // return error if has any unread data
        let unread_size = self.recv_len();
        if unread_size > 0 {
            return Err(Error::StreamHasUnreadData(unread_size));
        }

        // Anything the event loop delivered but no reader took belongs to this use of the
        // stream, so it is not ready to be handed back yet.
        match &self.inner.recv {
            RecvSide::V4 { pending, .. } => {
                let pending_len = pending.lock().unwrap().len();
                if pending_len > 0 {
                    return Err(Error::StreamHasPendingData(pending_len));
                }
            }
            // Retiring the generation also rejects batches the event loop is still carrying
            // for this stream's previous life.
            RecvSide::V5 { inbox, .. } => {
                if !inbox.advance_generation_if_idle() {
                    return Err(Error::StreamHasPendingData(inbox.pending_len()));
                }
            }
        }

        self.inner.shared.in_fallback.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// release the data previous read and reuse the last share memory slice for next write.
    pub fn release_read_and_reuse(&self) {
        // V5 releases each block as soon as nothing refers to it, so there is no consumed
        // storage to hand back and no slice worth swapping onto the write side.
        if self.is_v5() {
            self.with_reader(|r| r.release_previous_read());
            return;
        }
        let recv_buf = self.recv_buf();
        let send_buf = self.send_buf();
        recv_buf.release_previous_read_and_reserve();
        if recv_buf.is_empty() && recv_buf.slice_list().size() == 1 {
            std::mem::swap(recv_buf, send_buf);
        }
    }

    /// Deliver a V5 message that arrived over the unix socket instead of the segment.
    ///
    /// The peer's fallback frames carry no shared-memory block, so they go into the inbox as
    /// heap bytes and are read back in arrival order alongside the segment's own chains.
    pub(crate) fn fill_fallback_to_read_buffer(&self, data: Bytes) -> Result<(), Error> {
        let Some(inbox) = self.v5_inbox() else {
            return Err(Error::BrokenBuffer);
        };
        if data.is_empty() {
            return Ok(());
        }
        if !inbox.add_fallback_if_generation(data, inbox.generation()) {
            return Ok(());
        }
        self.inner.recv_notify.notify_one();
        Ok(())
    }

    /// Pin this stream's writes to the unix socket, for a peer that has already fallen back.
    pub(crate) fn set_fallback(&self) {
        self.inner.shared.set_fallback();
    }

    /// Hand a data frame to the stream, for a reader to pick up.
    ///
    /// V2 to V4 only. A V5 stream is delivered into through its inbox, by
    /// `Stream::fill_fallback_to_read_buffer` or by the event loop's own batches.
    pub fn fill_data_to_read_buffer(&self, data: BufferSliceWrapper) -> Result<(), Error> {
        let RecvSide::V4 { buf, pending } = &self.inner.recv else {
            return Err(Error::BrokenBuffer);
        };
        pending.lock().unwrap().push(data);
        // stream had closed, which maybe closed by user due to timeout.
        if self.inner.shared.state.load(Ordering::SeqCst) == STREAM_CLOSED {
            self.clean_pending_data();
            unsafe { &mut *buf.get() }.recycle();
            return Ok(());
        }
        // Unblock any readers
        self.inner.recv_notify.notify_one();

        Ok(())
    }

    pub fn is_open(&self) -> bool {
        self.inner.shared.state.load(Ordering::SeqCst) == STREAM_OPENED
    }

    pub fn safe_close_notify(&self) {
        self.inner.close_notify.notify_waiters();
    }

    /// Wake a reader after the event loop has delivered into this stream's V5 inbox.
    pub(crate) fn notify_readable(&self) {
        self.inner.recv_notify.notify_one();
    }

    pub fn half_close(&self) {
        if self
            .inner
            .shared
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
        self.inner.shared.in_fallback.load(Ordering::SeqCst)
    }

    pub async fn reuse(&self) {
        self.session.put_or_close_stream(self.clone()).await;
    }

    pub async fn close(&mut self) -> Result<(), Error> {
        let old_state = self
            .inner
            .shared
            .state
            .swap(STREAM_CLOSED, Ordering::Release);
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
        if !self.inner.shared.in_fallback.load(Ordering::SeqCst) {
            let is_v5 = self.session.shared.v5.is_some();
            let queued = match &self.session.shared.v5 {
                Some(v5) => v5
                    .own
                    .send_queue
                    .put(Element::new(self.id, OP_CLOSE_STREAM))
                    .is_ok(),
                None => self
                    .session
                    .shared
                    .queues()
                    .send_queue
                    .put(QueueElement {
                        seq_id: self.id,
                        offset_in_shm_buf: 0,
                        status: STREAM_CLOSED,
                    })
                    .is_ok(),
            };
            if queued {
                if is_v5 {
                    self.session
                        .shared
                        .stats
                        .v5_send_queue_count
                        .fetch_add(1, Ordering::SeqCst);
                }
                return self.session.wake_up_peer().await;
            }
            self.session
                .shared
                .stats
                .queue_full_error_count
                .fetch_add(1, Ordering::SeqCst);
            if is_v5 {
                self.session
                    .shared
                    .stats
                    .v5_send_queue_full_count
                    .fetch_add(1, Ordering::SeqCst);
            }
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
    pub async fn read_chunk(&mut self) -> Result<Buf, Error> {
        if self.inner.shared.state.load(Ordering::SeqCst) == STREAM_CLOSED {
            return Err(Error::StreamClosed);
        }
        if self.recv_len() == 0 {
            tracing::debug!("read_chunk seqID:{}", self.id);
            self.read_more(1).await?;
        }
        self.with_reader(|r| r.read_chunk())
    }

    pub(crate) async fn wait_readable(&mut self) -> Result<(), Error> {
        if self.inner.shared.state.load(Ordering::SeqCst) == STREAM_CLOSED {
            return Err(Error::StreamClosed);
        }
        if self.recv_len() == 0 {
            tracing::debug!("wait_readable seqID:{}", self.id);
            self.read_more(1).await?;
        }
        Ok(())
    }

    pub(crate) fn read_available_into(
        &mut self,
        dst: &mut ReadBuf,
        max_slices: usize,
    ) -> Result<usize, Error> {
        if !self.check_read_ready(1)? {
            return Err(Error::NotEnoughData);
        }
        match &self.inner.recv {
            RecvSide::V4 { buf, .. } => {
                unsafe { &mut *buf.get() }.read_available_into(dst, max_slices)
            }
            RecvSide::V5 { buf, .. } => {
                let n = unsafe { &mut *buf.get() }.read(dst.initialize_unfilled());
                dst.advance(n);
                Ok(n)
            }
        }
    }

    /// Read exactly `size` bytes as one contiguous buffer.
    ///
    /// A single-slice result is zero-copy. A result spanning slices is coalesced into owned memory.
    /// If the peer reaches EOF before `size` bytes are available, buffered data is left unconsumed.
    ///
    /// To read the next available contiguous chunk, refer to [`Stream::read_chunk`].
    pub async fn read_exact_bytes(&mut self, size: usize) -> Result<Buf, Error> {
        if size == 0 {
            return self.with_reader(|r| r.read_exact_bytes(0));
        }
        if self.inner.shared.state.load(Ordering::SeqCst) == STREAM_CLOSED {
            return Err(Error::StreamClosed);
        }
        if self.recv_len() < size {
            tracing::debug!(
                "read_exact_bytes seqID:{} len:{} size:{}",
                self.id,
                self.recv_len(),
                size
            );
            self.read_more(size).await?;
        }
        self.with_reader(|r| r.read_exact_bytes(size))
    }

    pub async fn peek(&mut self, size: usize) -> Result<Buf, Error> {
        if self.recv_len() < size {
            self.read_more(size).await?;
        }
        self.with_reader(|r| r.peek(size))
    }

    pub async fn discard(&mut self, size: usize) -> Result<usize, Error> {
        if self.recv_len() < size {
            self.read_more(size).await?;
        }
        self.with_reader(|r| r.discard(size))
    }

    pub fn reserve(&mut self, size: usize) -> Result<&mut [u8], Error> {
        match &self.inner.send {
            SendSide::V4(_) => self.send_buf().reserve(size),
            SendSide::V5(_) => self.v5_send_buf().expect("v5 send side").reserve(size),
        }
    }

    pub fn write_bytes(&mut self, data: &[u8]) -> Result<usize, Error> {
        match &self.inner.send {
            SendSide::V4(_) => self.send_buf().write_bytes(data),
            SendSide::V5(_) => self.v5_send_buf().expect("v5 send side").write_bytes(data),
        }
    }

    pub async fn flush(&mut self, end_stream: bool) -> Result<(), Error> {
        if self.is_v5() {
            return self.flush_v5(end_stream).await;
        }
        self.flush_v4(end_stream).await
    }

    /// Put one element on the V5 send queue, mapping the queue's own errors onto stream errors.
    fn v5_put(&self, element: Element) -> Result<(), Error> {
        match self
            .session
            .shared
            .v5
            .as_ref()
            .expect("v5 session")
            .own
            .send_queue
            .put(element)
        {
            Ok(()) => Ok(()),
            Err(PushError::Full) => Err(Error::QueueFull),
            Err(PushError::Closed) => Err(Error::SessionShutdown),
        }
    }

    /// Put one data element on the V5 send queue, retrying on a full queue.
    async fn retry_queue_put_v5(&self, element: Element) -> Result<(), Error> {
        for attempt in 0..=QUEUE_FULL_RETRY_COUNT {
            match self.v5_put(element) {
                Ok(()) => return Ok(()),
                Err(Error::QueueFull) => {}
                Err(err) => return Err(err),
            }

            if attempt == 0 {
                self.session
                    .shared
                    .stats
                    .queue_full_error_count
                    .fetch_add(1, Ordering::SeqCst);
                self.session
                    .shared
                    .stats
                    .v5_send_queue_full_count
                    .fetch_add(1, Ordering::SeqCst);
            }

            let _ = self.session.wake_up_peer().await;

            if attempt < QUEUE_FULL_RETRY_COUNT {
                let mut close_notified = pin!(self.inner.close_notify.notified());
                close_notified.as_mut().enable();
                let mut shutdown_notified = pin!(self.session.shared.shutdown_notify.notified());
                shutdown_notified.as_mut().enable();

                if !self.inner.shared.is_open() {
                    return Err(Error::StreamClosed);
                }
                if self.session.is_closed() {
                    return Err(Error::SessionShutdown);
                }

                tokio::select! {
                    _ = tokio::time::sleep(V5_QUEUE_FULL_RETRY_INTERVAL) => {}
                    _ = close_notified => return Err(Error::StreamClosed),
                    _ = shutdown_notified => return Err(Error::SessionShutdown),
                }
            }
        }

        Err(Error::QueueFull)
    }

    async fn flush_v5(&mut self, _end_stream: bool) -> Result<(), Error> {
        let state = self.inner.shared.state.load(Ordering::SeqCst);
        let buf = self.v5_send_buf().expect("v5 send side");
        if state != STREAM_OPENED {
            buf.discard();
            return Err(Error::StreamClosed);
        }
        if buf.is_empty() {
            return Ok(());
        }
        let total = buf.len();
        self.session
            .shared
            .stats
            .out_flow_bytes
            .fetch_add(total as u64, Ordering::SeqCst);

        if buf.is_fallback() {
            self.inner.shared.set_fallback();
            let frames = Self::frame_fallback(buf.take_fallback(), total);
            return self.write_fallback_v5(state, frames, total).await;
        }

        let outgoing = match buf.take_shm() {
            Ok(Some(outgoing)) => outgoing,
            Ok(None) => return Ok(()),
            Err(err) => {
                buf.discard();
                return Err(err);
            }
        };
        let element = Element::with_offset(self.id, OP_STREAM_DATA, outgoing.root());

        // `outgoing` frees its blocks when it drops: on a failed put here, and when this future
        // is dropped while the retry waits on a full queue.
        if let Err(err) = self.retry_queue_put_v5(element).await {
            drop(outgoing);
            return Err(err);
        }

        self.v5_send_buf().expect("v5 send side").recycle(outgoing);
        self.session
            .shared
            .stats
            .v5_send_queue_count
            .fetch_add(1, Ordering::SeqCst);
        self.session.wake_up_peer().await
    }

    /// Copy a fallback message into socket frames, each with room for its event header left
    /// at the front, and none longer than the ceiling a peer reads within.
    fn frame_fallback(chunks: Vec<OutChunk>, total: usize) -> Vec<Vec<u8>> {
        const EVENT_HEADER: usize = HEADER_SIZE + 8;
        let max_frame = MAX_CONTROL_FRAME_SIZE;

        let new_frame = |cap: usize| {
            let mut frame = Vec::with_capacity(EVENT_HEADER + cap);
            frame.resize(EVENT_HEADER, 0);
            frame
        };

        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut current = new_frame((max_frame - EVENT_HEADER).min(total));
        for chunk in &chunks {
            let mut rest: &[u8] = chunk;
            while !rest.is_empty() {
                let take = (max_frame - current.len()).min(rest.len());
                current.extend_from_slice(&rest[..take]);
                rest = &rest[take..];
                if current.len() == max_frame {
                    frames.push(std::mem::replace(
                        &mut current,
                        new_frame(max_frame - EVENT_HEADER),
                    ));
                }
            }
        }
        if current.len() > EVENT_HEADER {
            frames.push(current);
        }
        frames
    }

    /// Send a V5 message over the unix socket, split into frames the peer's reader accepts.
    async fn write_fallback_v5(
        &self,
        stream_status: u32,
        frames: Vec<Vec<u8>>,
        total: usize,
    ) -> Result<(), Error> {
        tracing::warn!(
            "session {} v5 stream fallback seqID:{} len:{}",
            self.session.shared.name,
            self.id,
            total
        );
        self.session
            .shared
            .stats
            .fallback_write_count
            .fetch_add(1, Ordering::SeqCst);

        let last = frames.len().saturating_sub(1);
        for (index, mut frame) in frames.into_iter().enumerate() {
            let status = if index == last {
                stream_status
            } else {
                STREAM_OPENED
            };
            let len = frame.len() as u32;
            let mut event = FallbackDataEvent(frame.as_mut_ptr());
            event.encode(len, self.session.shared.msg_version, self.id, status);
            self.session.wait_for_send(None, frame).await?;
        }
        Ok(())
    }

    /// Hand the buffered message to the peer over the shared queue, or over the unix socket when
    /// this stream has fallen back.
    async fn flush_v4(&mut self, end_stream: bool) -> Result<(), Error> {
        let send_buf = self.send_buf();
        if send_buf.is_empty() {
            return Ok(());
        }
        self.session
            .shared
            .stats
            .out_flow_bytes
            .fetch_add(send_buf.len() as u64, Ordering::SeqCst);
        let state = self.inner.shared.state.load(Ordering::SeqCst);
        if state != STREAM_OPENED {
            send_buf.recycle();
            return Err(Error::StreamClosed);
        }
        send_buf.done(end_stream);
        // Once we send data using uds, for this stream we will always use uds later to avoid
        // unordering
        if !send_buf.is_from_share_memory() {
            self.inner.shared.in_fallback.store(true, Ordering::SeqCst);
        }
        if self.inner.shared.in_fallback.load(Ordering::SeqCst) {
            let ret = self
                .write_fallback(state, Error::NoMoreBuffer, send_buf)
                .await;
            send_buf.clean();
            return ret;
        }

        match self.session.shared.queues().send_queue.put(QueueElement {
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
            if self.session.is_closed() {
                return Err(Error::StreamClosed);
            }
            self.session.shared.queues().send_queue.put(QueueElement {
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
