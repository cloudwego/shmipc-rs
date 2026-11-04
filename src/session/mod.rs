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

pub mod config;
pub mod manager;
pub mod pool;

use std::{
    collections::HashMap,
    mem,
    os::fd::{AsRawFd, OwnedFd},
    pin::Pin,
    sync::{
        Arc, LazyLock, Mutex, OnceLock, RwLock,
        atomic::{AtomicU32, Ordering},
    },
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use anyhow::anyhow;
use bytes::Bytes;
use futures::future::Either;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt as _, AsyncWrite, AsyncWriteExt},
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
};

use self::pool::StreamPool;
pub use self::{config::SessionManagerConfig, manager::SessionManager};
use crate::{
    buffer::{
        V5Inbox,
        manager::{BufferManager, add_global_buffer_manager_ref_count},
        slice::BufferSlice,
    },
    config::{Config, ProtocolConfig, ProtocolMode},
    consts::{
        EPOCH_INFO_MAX_LEN, FILE_NAME_MAX_LEN, HEADER_SIZE, MAX_CONTROL_FRAME_SIZE, MemMapType,
        QUEUE_INFO_MAX_LEN,
    },
    error::Error,
    protocol::{
        adapter::SharedPool,
        event::{EventType, POLLING_EVENT_WITH_VERSION, check_event_valid},
        event_fd::{drain_eventfd, write_eventfd},
        header::Header,
        init_client_protocol, init_manager, init_server_protocol,
        initializer::{SharedMemoryMode, v5::V5Initialized},
        negotiate_client_protocol,
        negotiation::{NegotiatedFeature, NegotiationInitError},
    },
    queue::QueueManager,
    stats::{DynShmStats, Stats},
    stream::{BufferSliceWrapper, STREAM_CLOSED, STREAM_OPENED, Stream},
    transport::{TransportConnect, TransportStream},
    util::buf_reader::BufReader,
    v5::{
        mpsc::PopResult,
        queue::{Element, OP_CLOSE_STREAM, OP_STREAM_DATA, RecvQueue},
    },
};

/// The unix socket read buffer, which only ever holds event headers and the small payloads of
/// control events. A fallback payload is read past it, into an allocation of its own that the
/// stream then owns, so it never has to fit here.
static BUF_READER_CAPACITY: LazyLock<usize> = LazyLock::new(|| {
    std::env::var("BUF_READER_CAPACITY")
        .map(|s| s.parse().unwrap_or(16 * 1024))
        .unwrap_or(16 * 1024)
});

/// Session is used to wrap a reliable ordered connection and to
/// multiplex it into multiple streams.
#[derive(Clone, Debug)]
pub(crate) struct Session {
    pub(crate) shared: Arc<Shared>,
}

#[derive(Debug)]
pub(crate) struct Shared {
    config: Config,
    /// next_stream_id is the next stream we should send.
    ///
    /// In client mode, nextStreamID is odd number.
    ///
    /// In server mode, nextStreamID is even number.
    next_stream_id: AtomicU32,
    /// The shared buffer pool of a V2 to V4 session. `None` on V5, which gives each side its
    /// own DynShm segment instead and never touches this.
    pub(crate) buffer_manager: Option<Arc<BufferManager>>,
    /// The shared queue pair of a V2 to V4 session. `None` on V5, which has its own queues in
    /// [`Shared::v5`].
    pub(crate) queue_manager: Option<QueueManager>,
    /// The queues and dynamic segments of a V5 session, `None` for V2 to V4.
    pub(crate) v5: Option<Box<V5Initialized>>,
    pub(crate) proto_version: u8,
    pub(crate) msg_version: u8,
    pub(crate) negotiated_feature: NegotiatedFeature,
    pub(crate) eventfd_send: Option<OwnedFd>,
    pub(crate) name: String,
    pool: StreamPool,
    streams: RwLock<HashMap<u32, Stream>>,
    is_client: bool,

    pub(crate) shutdown: AtomicU32,
    unhealthy: AtomicU32,

    shutdown_err: Mutex<Option<Error>>,

    send_tx: mpsc::Sender<SendReady>,
    /// The socket's write half. The write loop holds the lock for the whole of every frame it
    /// writes, and a wakeup that finds it free writes itself without a hop through the loop.
    /// `None` once the write loop has exited and shut the socket down for writing.
    writer: tokio::sync::Mutex<Option<ConnWriter>>,
    accept_tx: Option<mpsc::Sender<Stream>>,
    pub(crate) shutdown_notify: Notify,
    pub(crate) stats: Arc<Stats>,
    // pub(crate) peer_addr: SocketAddr,
    read_loop: OnceLock<JoinHandle<()>>,
    write_loop: OnceLock<JoinHandle<()>>,
    polling_loop: OnceLock<JoinHandle<()>>,
    eventfd_loop: OnceLock<JoinHandle<()>>,
    /// Taken out for the length of a V5 drain and put back after it, so this lock is only ever
    /// held for the swap and never across an await.
    v5_drain: Mutex<V5DrainScratch>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(handle) = self.read_loop.get() {
            handle.abort();
        }
        if let Some(handle) = self.write_loop.get() {
            handle.abort();
        }
        if let Some(handle) = self.polling_loop.get() {
            handle.abort();
        }
        if let Some(handle) = self.eventfd_loop.get() {
            handle.abort();
        }
    }
}

/// What the read loop does after [`Session::handle_events`] has consumed what it could.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Have at least this many bytes buffered before parsing again.
    Fill(usize),
    /// A fallback frame's headers have been consumed. Read its payload of `len` bytes straight
    /// into an allocation of its own, then hand it to
    /// [`Session::handle_fallback_payload`].
    FallbackPayload {
        seq_id: u32,
        status: u32,
        len: usize,
    },
}

/// Offsets the event loop has taken off the V5 queue for one stream and not yet delivered.
#[derive(Debug)]
struct V5NotifyBatch {
    stream: Stream,
    inbox: Arc<V5Inbox>,
    generation: u64,
    offsets: Vec<i64>,
}

/// The lists a drain of the V5 receive queue works with, kept between drains so a steady run of
/// wakeups reuses them rather than allocating a batch list and an offset list per drain.
#[derive(Debug, Default)]
struct V5DrainScratch {
    batches: Vec<V5NotifyBatch>,
    /// Offset lists of batches already handed over, emptied and waiting for the next batch. What
    /// they retain is bounded by the largest drain so far, which the per-drain lists they replace
    /// held just the same, only briefly.
    spare_offsets: Vec<Vec<i64>>,
}

pub struct SendReady {
    pub(crate) hdr: Option<Header>,
    pub(crate) body: Bytes,
    pub(crate) tx: Option<oneshot::Sender<()>>,
}

/// The socket's write half, boxed so the session can hold it without a type parameter.
struct ConnWriter {
    conn: Pin<Box<dyn AsyncWrite + Send>>,
    /// The tail of a wakeup that a direct, non-blocking write could only get part of onto the
    /// socket. The write loop sends it ahead of its next frame, so no frame starts inside it.
    carry: Bytes,
}

impl std::fmt::Debug for ConnWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConnWriter")
    }
}

impl Shared {
    /// The V2 to V4 buffer pool.
    ///
    /// Panics on a V5 session, which has no shared pool.
    #[inline]
    pub(crate) fn buffers(&self) -> &Arc<BufferManager> {
        self.buffer_manager
            .as_ref()
            .expect("a V2 to V4 session has a buffer manager")
    }

    /// The V2 to V4 queue pair.
    ///
    /// Panics on a V5 session, whose queues live in [`Shared::v5`].
    #[inline]
    pub(crate) fn queues(&self) -> &QueueManager {
        self.queue_manager
            .as_ref()
            .expect("a V2 to V4 session has a queue manager")
    }
}

/// Derives one session's config from the manager's, naming the shared memory this session uses.
fn session_config(
    base: &Config,
    session_id: usize,
    epoch_id: u64,
    rand_id: u64,
) -> Result<Config, Error> {
    let mut config = base.clone();
    config
        .share_memory_path_prefix
        .push_str(&format!("_{}", std::process::id()));
    if let MemMapType::MemMapTypeDevShmFile = config.mem_map_type
        && config.share_memory_path_prefix.len() + EPOCH_INFO_MAX_LEN + QUEUE_INFO_MAX_LEN
            > FILE_NAME_MAX_LEN
    {
        return Err(Error::FileNameTooLong);
    }
    if epoch_id > 0 {
        config
            .share_memory_path_prefix
            .push_str(&format!("_epoch_{}_{}", epoch_id, rand_id));
    }
    if !config.share_memory_path_prefix.is_empty() {
        config.queue_path = format!("{}_queue_{}", config.share_memory_path_prefix, session_id);
    }
    Ok(config)
}

impl Session {
    pub(crate) fn stats_snapshot(&self) -> crate::stats::StatsSnapshot {
        self.shared.stats.snapshot()
    }

    pub(crate) fn v5_dynshm_stats(&self) -> Option<DynShmStats> {
        self.shared.v5.as_deref().map(|v5| v5.own.dynshm.stats())
    }

    pub async fn client<C>(
        session_id: usize,
        epoch_id: u64,
        rand_id: u64,
        sm_config: &SessionManagerConfig,
        connect: &C,
        addr: C::Address,
    ) -> Result<Self, Error>
    where
        C: TransportConnect,
        <C::Stream as TransportStream>::ReadHalf: Send + 'static,
        <C::Stream as TransportStream>::WriteHalf: Send + 'static,
        C::Address: Clone,
    {
        let config = session_config(sm_config.config(), session_id, epoch_id, rand_id)?;
        let conn_stream = connect.connect(addr.clone()).await?;
        match Self::new(config.clone(), conn_stream, None).await {
            Ok(session) => Ok(session),
            Err(err)
                if matches!(
                    config.protocol.mode,
                    ProtocolMode::V4 { fallback: true } | ProtocolMode::V5 { fallback: true }
                ) && is_negotiation_fallback_error(&err) =>
            {
                let mut fallback_config = config;
                fallback_config.protocol = ProtocolConfig::default();
                let conn_stream = match connect.connect(addr).await {
                    Ok(conn_stream) => conn_stream,
                    Err(fallback_error) => {
                        return Err(anyhow::Error::new(NegotiationFallbackFailed {
                            error: err,
                            fallback_error: anyhow::Error::new(fallback_error),
                        })
                        .into());
                    }
                };
                match Self::new(fallback_config, conn_stream, None).await {
                    Ok(session) => Ok(session),
                    Err(fallback_error) => Err(anyhow::Error::new(NegotiationFallbackFailed {
                        error: err,
                        fallback_error,
                    })
                    .into()),
                }
            }
            Err(err) => Err(err.into()),
        }
    }

    pub async fn server<S>(
        config: Config,
        conn_stream: S,
        // addr: A,
        accept_tx: mpsc::Sender<Stream>,
    ) -> Result<Self, Error>
    where
        S: TransportStream,
        S::ReadHalf: Send + 'static,
        S::WriteHalf: Send + 'static,
    {
        Ok(Self::new(config, conn_stream, Some(accept_tx)).await?)
    }

    async fn new<S>(
        mut config: Config,
        conn_stream: S,
        // peer_addr: A,
        accept_tx: Option<mpsc::Sender<Stream>>,
    ) -> Result<Self, anyhow::Error>
    where
        S: TransportStream,
        S::ReadHalf: Send + 'static,
        S::WriteHalf: Send + 'static,
    {
        config
            .verify()
            .map_err(|err| err.context("verify config failed"))?;

        let conn_fd = conn_stream.as_raw_fd();
        let is_client = accept_tx.is_none();

        let mut nonblocking = false as libc::c_int;
        unsafe {
            if libc::ioctl(conn_fd, libc::FIONBIO, &mut nonblocking) == -1 {
                return Err(anyhow!(
                    "set conn_fd {} blocking failed, error={}",
                    conn_fd,
                    std::io::Error::last_os_error()
                ));
            }
        };

        // on server mode the backend task will use accept_ch to transfer new stream.
        let next_stream_id = if !is_client { 2 } else { 1 };

        let (bm, qm, mut protocol_initialized) = if is_client {
            let handshake_started = Instant::now();
            let negotiation =
                negotiate_client_protocol(conn_fd, config.clone(), config.initialize_timeout)
                    .await?;

            // Allocated only once the peer has settled on a version that reads through a shared
            // pool. A V5 session gives each endpoint its own segment, so creating one here would
            // map a buffer region the session never touches.
            let (bm, qm) = if negotiation.needs_shared_pool() {
                let (bm, qm) =
                    init_manager(&mut config, negotiation.selected_protocol()).map_err(|err| {
                        anyhow!("create share memory buffer manager failed, error={}", err)
                    })?;
                (Some(bm), Some(qm))
            } else {
                (None, None)
            };
            let shared_pool = bm.as_ref().zip(qm.as_ref()).map(|(bm, qm)| SharedPool {
                buffer_path: bm.path.clone(),
                queue_path: qm.path.clone(),
                buffer_fd: bm.memfd,
                queue_fd: qm.memfd,
            });

            let initialized = match init_client_protocol(
                negotiation,
                shared_pool,
                conn_fd,
                config.clone(),
                config
                    .initialize_timeout
                    .saturating_sub(handshake_started.elapsed()),
            )
            .await
            {
                Ok(initialized) => initialized,
                Err(err) => {
                    if let (Some(bm), Some(qm)) = (&bm, &qm) {
                        let buffer_path = bm.path.clone();
                        qm.unmap();
                        add_global_buffer_manager_ref_count(&buffer_path, -1).await;
                    }
                    return Err(err);
                }
            };
            (bm, qm, initialized)
        } else {
            let mut initialized =
                init_server_protocol(conn_fd, config.clone(), config.initialize_timeout).await?;
            // A V5 handshake negotiates per-endpoint segments rather than a shared pool, so
            // there is nothing to take here and nothing that needs one.
            let (bm, qm) = match initialized.shared_memory.take() {
                Some((bm, qm)) => (Some(bm), Some(qm)),
                None if matches!(
                    initialized.shared_memory,
                    SharedMemoryMode::PerEndpointPool(_)
                ) =>
                {
                    (None, None)
                }
                None => {
                    return Err(anyhow!("server protocol did not initialize shared memory"));
                }
            };
            (bm, qm, initialized)
        };

        let v5 = protocol_initialized.shared_memory.take_v5();

        let (eventfd_send, eventfd_recv) = match protocol_initialized.eventfd {
            Some(eventfd) => (Some(eventfd.wakeup_send), Some(eventfd.wakeup_recv)),
            None => (None, None),
        };
        let negotiated_feature = protocol_initialized.feature;
        let proto_version = protocol_initialized.proto_version;
        let msg_version = protocol_initialized.msg_version;

        let (send_tx, send_rx) = mpsc::channel::<SendReady>(4096);

        nonblocking = true as libc::c_int;
        unsafe {
            if libc::ioctl(conn_fd, libc::FIONBIO, &mut nonblocking) == -1 {
                return Err(anyhow!(
                    "set conn_fd {} blocking failed, error={}",
                    conn_fd,
                    std::io::Error::last_os_error()
                ));
            }
        };
        let (owned_read_half, owned_write_half) = conn_stream.into_split();

        // A V5 session has no queue manager to take its name from.
        let session_name = qm
            .as_ref()
            .map_or_else(|| config.queue_path.clone(), |qm| qm.path.clone());

        let session = Session {
            shared: Arc::new(Shared {
                pool: StreamPool::new(config.max_stream_num),
                config,
                next_stream_id: AtomicU32::new(next_stream_id),
                buffer_manager: bm,
                name: session_name,
                queue_manager: qm,
                v5,
                proto_version,
                msg_version,
                negotiated_feature,
                eventfd_send,
                shutdown: AtomicU32::new(0),
                unhealthy: AtomicU32::new(0),
                send_tx,
                writer: tokio::sync::Mutex::new(Some(ConnWriter {
                    conn: Box::pin(owned_write_half),
                    carry: Bytes::new(),
                })),
                shutdown_err: Mutex::new(None),
                streams: RwLock::new(HashMap::new()),
                is_client,
                accept_tx,
                shutdown_notify: Notify::new(),
                stats: Arc::new(Stats::default()),
                // peer_addr,
                read_loop: OnceLock::new(),
                write_loop: OnceLock::new(),
                polling_loop: OnceLock::new(),
                eventfd_loop: OnceLock::new(),
                v5_drain: Mutex::new(V5DrainScratch::default()),
            }),
        };

        if let NegotiatedFeature::EventQueuePolling { interval } = negotiated_feature {
            session
                .shared
                .polling_loop
                .set(tokio::spawn(session.clone().polling_loop(interval)))
                .unwrap();
        }

        if let Some(eventfd_recv) = eventfd_recv {
            session
                .shared
                .eventfd_loop
                .set(tokio::spawn(session.clone().eventfd_loop(eventfd_recv)))
                .unwrap();
        }

        // uds read
        session
            .shared
            .read_loop
            .set(tokio::spawn(session.clone().read_loop(owned_read_half)))
            .unwrap();

        // uds write
        session
            .shared
            .write_loop
            .set(tokio::spawn(session.clone().write_loop(send_rx)))
            .unwrap();

        Ok(session)
    }

    /// Return whether the session is healthy
    pub fn is_healthy(&self) -> bool {
        self.shared.unhealthy.load(Ordering::SeqCst) == 0
    }

    /// Does a safe check to see if we have shutdown
    pub fn is_closed(&self) -> bool {
        self.shared.shutdown.load(Ordering::SeqCst) == 1
    }

    /// What a shut-down session reports to an operation it interrupted: the error that brought
    /// it down when one was recorded, and a plain shutdown otherwise.
    fn shutdown_error(&self) -> Error {
        match &*self.shared.shutdown_err.lock().unwrap() {
            Some(err) => Error::Others(anyhow!("session shutdown: {err}")),
            None => Error::SessionShutdown,
        }
    }

    pub fn get_or_open_stream(&self, session_id: usize) -> Result<Stream, Error> {
        if !self.is_healthy() {
            return Err(Error::SessionUnhealthy);
        }

        while let Some(stream) = self.shared.pool.pop() {
            // ensure return an open stream
            if stream.is_open() {
                return Ok(stream);
            }
        }

        self.open_stream(session_id)
    }

    pub async fn put_or_close_stream(&self, mut s: Stream) {
        // if the stream is in fallback state, we will not reuse it
        if s.fallback_state() {
            if let Err(err) = s.close().await {
                tracing::error!("{} close stream error: {}", self.shared.name, err);
            }
            return;
        }
        match s.reset() {
            Ok(_) => {
                s.release_read_and_reuse();
                if let Err(err) = self.shared.pool.push(s).await {
                    tracing::error!("put stream to pool error: {}", err);
                }
            }
            Err(err) => {
                tracing::error!("{} put_or_close_stream error: {}", self.shared.name, err);
                if let Err(err) = s.close().await {
                    tracing::error!("{} close stream error: {}", self.shared.name, err);
                }
            }
        }
    }

    /// Used to create a new stream
    pub fn open_stream(&self, session_id: usize) -> Result<Stream, Error> {
        if self.is_closed() {
            if let Some(err) = self.shared.shutdown_err.lock().unwrap().take() {
                return Err(err);
            } else {
                return Err(Error::SessionShutdown);
            }
        }
        if self.shared.unhealthy.load(Ordering::SeqCst) == 1 {
            return Err(Error::SessionUnhealthy);
        }

        // get an id, and check for stream exhaustion
        let id = self.shared.next_stream_id.fetch_add(1, Ordering::SeqCst) + 1;

        if self.shared.streams.read().unwrap().contains_key(&id) {
            return Err(Error::StreamsExhausted);
        }

        let stream = Stream::new(id, session_id, self.clone());
        self.shared
            .streams
            .write()
            .unwrap()
            .insert(id, stream.clone());

        tracing::trace!(
            "{} open stream {} proto {}",
            self.shared.name,
            id,
            self.shared.proto_version
        );

        Ok(stream)
    }

    /// Tear the session down because the peer wrote data that cannot be trusted.
    ///
    /// A DynShm chain that does not parse means this side cannot safely walk the peer's
    /// segment at all, so the session goes down rather than failing the one reader that
    /// happened to touch it and leaving the peer its slot.
    pub(crate) async fn close_corrupt(&self, stream_id: u32, err: &Error) {
        tracing::error!(
            "{} closing session on invalid V5 DynShm chain, stream:{stream_id} reason:{err}",
            self.shared.name
        );
        // Recorded so the readers this shutdown wakes on other streams are told why, and kept
        // only if nothing has recorded a cause yet, so the first failure is the one they see.
        {
            let mut shutdown_err = self.shared.shutdown_err.lock().unwrap();
            if shutdown_err.is_none() {
                *shutdown_err = Some(Error::Others(anyhow!(
                    "invalid V5 DynShm chain on stream {stream_id}: {err}"
                )));
            }
        }
        self.close().await;
    }

    /// Attempts to send a GoAway before closing the connection.
    pub async fn close(&self) {
        if self
            .shared
            .shutdown
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        tracing::info!(
            "close session {} hadShutDown:{}",
            self.shared.name,
            self.shared.shutdown.load(Ordering::SeqCst)
        );
        self.shared.pool.close().await;

        self.shared.shutdown_notify.notify_waiters();

        // close all streams
        let s = {
            let mut streams = self.shared.streams.write().unwrap();
            streams.drain().map(|(_, s)| s).collect::<Vec<_>>()
        };
        for mut stream in s {
            _ = stream.close().await;
        }

        if let Some(bm) = &self.shared.buffer_manager {
            add_global_buffer_manager_ref_count(&bm.path, -1).await;
        }
        if let Some(qm) = &self.shared.queue_manager {
            qm.unmap();
        }

        if let Some(v5) = &self.shared.v5 {
            v5.close();
        }
    }

    pub async fn wait_for_send(&self, hdr: Option<Header>, body: Vec<u8>) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel::<()>();
        let ready = SendReady {
            hdr,
            body: body.into(),
            tx: Some(tx),
        };
        match tokio::time::timeout(
            self.shared.config.connection_write_timeout,
            self.shared.send_tx.send(ready),
        )
        .await
        {
            Ok(_) => {
                if let Err(err) = rx.await {
                    return Err(anyhow!("wait for send failed, error={}", err).into());
                }
            }
            Err(_) => {
                tracing::debug!("write timeout, send channel is full");
                return Err(Error::ConnectionWriteTimeout);
            }
        }

        Ok(())
    }

    /// A non-blocking attempt to put a wakeup on the socket without the write loop.
    /// The loop holds the same lock for every so a wakeup can only land between frames.
    ///
    /// Synchronous on purpose: the poll registers no waker, so a socket with no room hands
    /// the wakeup to the loop rather than parking this task with the lock held, and the lock
    /// never crosses an await.
    fn try_write_wakeup(&self, event: &mut Bytes) -> std::io::Result<bool> {
        let Ok(mut writer) = self.shared.writer.try_lock() else {
            return Ok(false);
        };
        let Some(ConnWriter { conn, carry }) = writer.as_mut() else {
            return Ok(false);
        };
        if !carry.is_empty() {
            return Ok(false);
        }
        let mut cx = Context::from_waker(Waker::noop());
        match conn.as_mut().poll_write(&mut cx, &event[..]) {
            Poll::Ready(Ok(written)) if written == event.len() => Ok(true),
            Poll::Ready(Ok(written)) => {
                *carry = event.split_off(written);
                *event = Bytes::new();
                Ok(false)
            }
            Poll::Ready(Err(err)) => Err(err),
            Poll::Pending => Ok(false),
        }
    }

    pub async fn wake_up_peer(&self) -> Result<(), Error> {
        if matches!(
            self.shared.negotiated_feature,
            NegotiatedFeature::EventQueuePolling { .. }
        ) {
            return Ok(());
        }
        let claimed = match &self.shared.v5 {
            Some(v5) => v5.own.send_queue.claim_peer_consumer(),
            None => self.shared.queues().send_queue.mark_working(),
        };
        if !claimed {
            return Ok(());
        }
        self.shared
            .stats
            .send_polling_event_count
            .fetch_add(1, Ordering::SeqCst);
        if let Some(eventfd_send) = &self.shared.eventfd_send {
            write_eventfd(eventfd_send.as_raw_fd())?;
            return Ok(());
        }
        let mut event = Bytes::from_static(
            POLLING_EVENT_WITH_VERSION[self.shared.msg_version as usize].as_slice(),
        );
        match self.try_write_wakeup(&mut event) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(err) => {
                Box::pin(self.exit_err(err.into())).await;
                return Ok(());
            }
        }
        _ = self
            .shared
            .send_tx
            .send(SendReady {
                hdr: None,
                body: event,
                tx: None,
            })
            .await;
        Ok(())
    }

    pub async fn open_circuit_breaker(&self) {
        static DEBUG_MODE: LazyLock<bool> =
            LazyLock::new(|| std::env::var("SHMIPC_DEBUG_MODE").is_ok());

        if *DEBUG_MODE {
            return;
        }

        if self
            .shared
            .unhealthy
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        tracing::info!(
            "session {} circuit breaker open, set unhealthy status",
            self.shared.name
        );
        tokio::spawn({
            let shared = self.shared.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                shared.unhealthy.store(0, Ordering::SeqCst);
                tracing::info!(
                    "session {} circuit breaker closed, remove unhealthy status",
                    shared.name
                );
            }
        });
    }

    pub fn on_stream_close(&self, id: u32, state: u32) {
        tracing::trace!("stream:{} close state:{}", id, state);
        self.shared.streams.write().unwrap().remove(&id);
    }

    pub async fn recv_loop(
        self,
        mut rx: mpsc::Receiver<Stream>,
        stream_tx: mpsc::UnboundedSender<std::io::Result<Stream>>,
    ) {
        let mut notified = std::pin::pin!(self.shared.shutdown_notify.notified());
        loop {
            match futures::future::select(std::pin::pin!(rx.recv()), &mut notified).await {
                Either::Left((res, _)) => {
                    if let Some(stream) = res {
                        if stream_tx.send(Ok(stream)).is_err() {
                            return;
                        }
                    } else {
                        tracing::warn!("session unhealthy");
                        continue;
                    }
                }
                Either::Right(_) => {
                    tracing::info!("session shutdown");
                    return;
                }
            }
        }
    }

    async fn read_loop<R>(self, reader: R)
    where
        R: tokio::io::AsyncRead,
    {
        tokio::pin!(reader);
        let mut reader = BufReader::with_capacity(*BUF_READER_CAPACITY, reader);
        let mut shutdown_notified = std::pin::pin!(self.shared.shutdown_notify.notified());
        let mut len = HEADER_SIZE;
        loop {
            let buf = match futures::future::select(
                std::pin::pin!(reader.fill_buf_at_least(len)),
                &mut shutdown_notified,
            )
            .await
            {
                Either::Left((buf, _)) => {
                    if self.shared.shutdown.load(Ordering::SeqCst) == 1 {
                        return;
                    }
                    buf
                }
                Either::Right(_) => return,
            };
            let (consumed, next, err) = match buf {
                Ok(buf) => self.handle_events(buf).await,
                Err(err) => {
                    self.exit_err(err.into()).await;
                    return;
                }
            };
            reader.consume(consumed);
            if let Some(err) = err
                && !self.is_closed()
            {
                self.exit_err(err).await;
                return;
            }
            len = match next {
                Next::Fill(len) => len,
                Next::FallbackPayload {
                    seq_id,
                    status,
                    len: payload_len,
                } => {
                    // Read past the buffer, into the allocation the stream will own. The reader
                    // hands over whatever prefix it already holds and takes the rest straight
                    // from the socket.
                    let mut payload = vec![0u8; payload_len];
                    if payload_len > 0 {
                        match futures::future::select(
                            std::pin::pin!(reader.read_exact(&mut payload)),
                            &mut shutdown_notified,
                        )
                        .await
                        {
                            Either::Left((Ok(_), _)) => {}
                            Either::Left((Err(err), _)) => {
                                self.exit_err(err.into()).await;
                                return;
                            }
                            Either::Right(_) => return,
                        }
                        if self.shared.shutdown.load(Ordering::SeqCst) == 1 {
                            return;
                        }
                    }
                    if let Some(err) = self.handle_fallback_payload(seq_id, status, payload).await
                        && !self.is_closed()
                    {
                        self.exit_err(err).await;
                        return;
                    }
                    HEADER_SIZE
                }
            };
        }
    }

    async fn write_loop(self, mut send_rx: mpsc::Receiver<SendReady>) {
        let shutdown_notified = self.shared.shutdown_notify.notified();
        tokio::pin!(shutdown_notified);
        loop {
            let ready = match futures::future::select(
                std::pin::pin!(send_rx.recv()),
                &mut shutdown_notified,
            )
            .await
            {
                Either::Left((Some(ready), _)) => ready,
                _ => break,
            };
            let mut writer = self.shared.writer.lock().await;
            let Some(ConnWriter { conn, carry }) = writer.as_mut() else {
                break;
            };
            // The tail of a wakeup a direct write could only get part of goes first.
            if !carry.is_empty()
                && let Err(err) = conn.write_all(&mem::take(carry)).await
            {
                drop(writer);
                drop(ready.tx);
                self.exit_err(err.into()).await;
                break;
            }
            // send a header if ready
            if let Some(hdr) = ready.hdr
                && let Err(err) = conn.write_all(hdr.as_slice()).await
            {
                drop(writer);
                drop(ready.tx);
                self.exit_err(err.into()).await;
                break;
            }
            // send data from a body if given
            if !ready.body.is_empty()
                && let Err(err) = conn.write_all(&ready.body).await
            {
                drop(writer);
                drop(ready.tx);
                self.exit_err(err.into()).await;
                break;
            }
            drop(writer);

            // no error, successful send
            if let Some(tx) = ready.tx {
                _ = tx.send(());
            }
        }
        self.shared.writer.lock().await.take();
    }

    async fn polling_loop(self, interval: Duration) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let shutdown_notified = self.shared.shutdown_notify.notified();
        tokio::pin!(shutdown_notified);
        loop {
            match futures::future::select(std::pin::pin!(ticker.tick()), &mut shutdown_notified)
                .await
            {
                Either::Left(_) => {
                    if let Some(err) = self.drain_recv_queue().await {
                        self.exit_err(err).await;
                        return;
                    }
                }
                Either::Right(_) => return,
            }
        }
    }

    async fn eventfd_loop(self, eventfd_recv: OwnedFd) {
        let eventfd = match tokio::io::unix::AsyncFd::new(eventfd_recv) {
            Ok(eventfd) => eventfd,
            Err(err) => {
                self.exit_err(err.into()).await;
                return;
            }
        };
        let shutdown_notified = self.shared.shutdown_notify.notified();
        tokio::pin!(shutdown_notified);
        loop {
            match futures::future::select(
                std::pin::pin!(eventfd.readable()),
                &mut shutdown_notified,
            )
            .await
            {
                Either::Left((ready, _)) => {
                    let mut ready = match ready {
                        Ok(ready) => ready,
                        Err(err) => {
                            self.exit_err(err.into()).await;
                            return;
                        }
                    };
                    let mut drained = false;
                    match ready.try_io(|inner| -> std::io::Result<()> {
                        drained = drain_eventfd(inner.as_raw_fd())?;
                        Err(std::io::ErrorKind::WouldBlock.into())
                    }) {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            self.exit_err(err.into()).await;
                            return;
                        }
                        Err(_) => {}
                    }
                    if !drained {
                        continue;
                    }
                    self.shared
                        .stats
                        .recv_polling_event_count
                        .fetch_add(1, Ordering::SeqCst);
                    if let Some(err) = self.drain_recv_queue().await {
                        self.exit_err(err).await;
                        return;
                    }
                }
                Either::Right(_) => return,
            }
        }
    }

    /// Used to handle an error that is causing the session to terminate.
    async fn exit_err(&self, err: Error) {
        tracing::warn!("{} exit with error: {}", self.shared.name, err);
        self.shared
            .stats
            .event_conn_error_count
            .fetch_add(1, Ordering::SeqCst);
        self.shared.shutdown_err.lock().unwrap().replace(err);
        self.close().await;
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "legacy fallback failed after initialization failed; error: {error}; fallback error: \
     {fallback_error}"
)]
struct NegotiationFallbackFailed {
    #[source]
    error: anyhow::Error,
    fallback_error: anyhow::Error,
}

fn is_negotiation_fallback_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NegotiationInitError>()
        .is_some_and(NegotiationInitError::is_network_or_protocol)
}

impl Session {
    /// Handle every complete event in `buf`, returning how much of it was consumed and what
    /// the read loop has to do before calling again.
    pub async fn handle_events(&self, buf: &[u8]) -> (usize, Next, Option<Error>) {
        // fallback data layout: eventHeader | seqID | status | payload
        const FALLBACK_DATA_HEADER: usize = 8;
        let mut consumed = 0;
        while buf[consumed..].len() >= HEADER_SIZE {
            let event_header = Header(buf[consumed..consumed + HEADER_SIZE].as_ptr() as *mut _);
            if let Err(err) = check_event_valid(&event_header) {
                return (consumed + HEADER_SIZE, Next::Fill(HEADER_SIZE), Some(err));
            }
            if event_header.length() as usize > MAX_CONTROL_FRAME_SIZE {
                return (
                    consumed + HEADER_SIZE,
                    Next::Fill(HEADER_SIZE),
                    Some(Error::SizeTooLarge),
                );
            }
            let min_payload_size = match event_header.msg_type() {
                EventType::TYPE_STREAM_CLOSE => 4,
                EventType::TYPE_FALLBACK_DATA => FALLBACK_DATA_HEADER,
                _ => 0,
            };
            if (event_header.length() as usize) < HEADER_SIZE + min_payload_size {
                return (
                    consumed + HEADER_SIZE,
                    Next::Fill(HEADER_SIZE),
                    Some(Error::NotEnoughData),
                );
            }
            let (n, required, stop, err) = match event_header.msg_type() {
                EventType::TYPE_POLLING => {
                    if matches!(
                        self.shared.negotiated_feature,
                        NegotiatedFeature::EventQueuePolling { .. } | NegotiatedFeature::EventFd
                    ) {
                        self.shared
                            .stats
                            .recv_polling_event_count
                            .fetch_add(1, Ordering::SeqCst);
                        (HEADER_SIZE, HEADER_SIZE, false, None)
                    } else {
                        self.handle_polling(&event_header, &buf[consumed + HEADER_SIZE..])
                            .await
                    }
                }
                EventType::TYPE_STREAM_CLOSE => {
                    self.handle_stream_close(&event_header, &buf[consumed + HEADER_SIZE..])
                        .await
                }
                EventType::TYPE_FALLBACK_DATA => {
                    // Only the stream id and status are parsed here. The payload can be far
                    // larger than the buffer, so the read loop reads it into an allocation of
                    // its own that the stream then owns.
                    let head = &buf[consumed + HEADER_SIZE..];
                    if head.len() < FALLBACK_DATA_HEADER {
                        return (
                            consumed,
                            Next::Fill(HEADER_SIZE + FALLBACK_DATA_HEADER),
                            None,
                        );
                    }
                    let seq_id = u32::from_be_bytes(head[..4].try_into().unwrap());
                    // now the first byte of status is streamState, and the other byte of status
                    // is undefined.
                    let status = u32::from_be_bytes(head[4..8].try_into().unwrap()) & 0xff;
                    let len = event_header.length() as usize - HEADER_SIZE - FALLBACK_DATA_HEADER;
                    return (
                        consumed + HEADER_SIZE + FALLBACK_DATA_HEADER,
                        Next::FallbackPayload {
                            seq_id,
                            status,
                            len,
                        },
                        None,
                    );
                }
                _ => {
                    return (
                        consumed + HEADER_SIZE,
                        Next::Fill(HEADER_SIZE),
                        Some(Error::InvalidMsgType),
                    );
                }
            };
            consumed += n;
            if err.is_some() {
                return (consumed, Next::Fill(HEADER_SIZE), err);
            }
            if stop {
                return (consumed, Next::Fill(required), None);
            }
        }
        (consumed, Next::Fill(HEADER_SIZE), None)
    }

    pub async fn handle_polling(
        &self,
        _event_header: &Header,
        _buf: &[u8],
    ) -> (usize, usize, bool, Option<Error>) {
        self.shared
            .stats
            .recv_polling_event_count
            .fetch_add(1, Ordering::SeqCst);
        (
            HEADER_SIZE,
            HEADER_SIZE,
            false,
            self.drain_recv_queue().await,
        )
    }

    /// Take everything the peer has queued and deliver it to the streams it is for.
    async fn drain_recv_queue(&self) -> Option<Error> {
        match self.shared.v5.as_deref() {
            Some(v5) => {
                let mut queue = v5.peer.recv_queue.lock().await;
                self.drain_recv_queue_v5_locked(v5, &mut queue, false).await
            }
            None => self.drain_recv_queue_v4().await,
        }
    }

    /// Drain the V5 receive queue as a barrier, for an event that arrived over the unix socket.
    async fn drain_v5_barrier(&self, v5: &V5Initialized, queue: &mut RecvQueue) -> Option<Error> {
        self.drain_recv_queue_v5_locked(v5, queue, true).await
    }

    /// Drain the V5 receive queue, delivering each stream's offsets to its inbox.
    ///
    /// Offsets are accumulated per stream and handed over in batches, so a burst aimed at one
    /// stream costs one lock and one wakeup rather than one of each per element.
    ///
    /// `wait_for_claimed` decides what a claimed-but-unpublished head means: a barrier waits
    /// for it, an ordinary drain stops and comes back later.
    async fn drain_recv_queue_v5_locked(
        &self,
        v5: &V5Initialized,
        queue: &mut RecvQueue,
        wait_for_claimed: bool,
    ) -> Option<Error> {
        let publish_timeout = self.shared.config.v5_queue_publish_timeout;
        let mut scratch = mem::take(&mut *self.shared.v5_drain.lock().unwrap());
        let mut ret_err = None;
        let mut popped: u64 = 0;

        'draining: loop {
            if self.is_closed() {
                if wait_for_claimed {
                    ret_err = Some(self.shutdown_error());
                }
                break;
            }
            // Only set while a claimed-but-unpublished slot is blocking the head.
            let mut claimed_at: Option<std::time::Instant> = None;
            loop {
                match queue.try_pop() {
                    Ok(PopResult::Ready(element)) => {
                        claimed_at = None;
                        popped += 1;
                        match element.op {
                            OP_STREAM_DATA => self.batch_v5_data(&element, &mut scratch).await,
                            OP_CLOSE_STREAM => {
                                // Flushed first so the data already queued for this stream is
                                // delivered ahead of the close that follows it.
                                self.flush_v5_batches(v5, &mut scratch);
                                self.handle_v5_close(element.stream_id).await;
                            }
                            // V5 opens a stream implicitly with its first data element, so
                            // there is no open to handle and anything else is unknown.
                            _ => {}
                        }
                    }
                    // A producer reserved this slot and has not published it yet. It cannot be
                    // skipped without reordering the queue.
                    Ok(PopResult::HeadNotReady) => {
                        // Nothing here depends on it, so leave it for the drain that the
                        // producer's own wakeup will trigger once it publishes.
                        if !wait_for_claimed {
                            break;
                        }
                        // A barrier waits but not forever, because a producer
                        // that died mid-claim would block this session permanently.
                        if self.is_closed() {
                            ret_err = Some(self.shutdown_error());
                            break 'draining;
                        }
                        let since = claimed_at.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() >= publish_timeout {
                            ret_err = Some(Error::QueuePublishTimeout);
                            break 'draining;
                        }
                        tokio::task::yield_now().await;
                    }
                    Ok(PopResult::Empty) => break,
                    // A pop errors only on a queue closed under this drain, which happens when
                    // the session is going away.
                    Err(err) => {
                        ret_err = Some(if self.is_closed() {
                            self.shutdown_error()
                        } else {
                            err.into()
                        });
                        break 'draining;
                    }
                }
            }

            self.flush_v5_batches(v5, &mut scratch);
            if matches!(
                self.shared.negotiated_feature,
                NegotiatedFeature::EventQueuePolling { .. }
            ) {
                tokio::task::yield_now().await;
            } else {
                tokio::task::coop::consume_budget().await;
            }
            if queue.finish_draining() {
                break;
            }
        }

        // Anything collected before an error still belongs to its streams.
        self.flush_v5_batches(v5, &mut scratch);
        *self.shared.v5_drain.lock().unwrap() = scratch;
        if popped > 0 {
            self.shared
                .stats
                .v5_recv_queue_count
                .fetch_add(popped, Ordering::SeqCst);
        }
        ret_err
    }

    /// Add one data element to its stream's batch, or release it if no stream wants it.
    async fn batch_v5_data(&self, element: &Element, scratch: &mut V5DrainScratch) {
        let offset = element.offset();
        // Orphan data: nothing will ever read these blocks, so they go straight back to the peer
        // rather than sitting in its segment until the session ends.
        let release = || {
            if let Some(v5) = self.shared.v5.as_deref() {
                let _ = v5.peer.dynshm.free_chain(offset);
            }
        };
        let Some(stream) = self.get_stream(element.stream_id, STREAM_OPENED).await else {
            release();
            return;
        };
        let Some(inbox) = stream.v5_inbox().cloned() else {
            release();
            return;
        };
        let generation = inbox.generation();
        // Only join an open batch that is still for the same generation.
        if let Some(batch) = scratch
            .batches
            .iter_mut()
            .find(|batch| Arc::ptr_eq(&batch.inbox, &inbox) && batch.generation == generation)
        {
            batch.offsets.push(offset);
            return;
        }
        let mut offsets = scratch.spare_offsets.pop().unwrap_or_default();
        offsets.push(offset);
        scratch.batches.push(V5NotifyBatch {
            generation,
            inbox,
            stream,
            offsets,
        });
    }

    /// Hand every open batch to its stream, releasing the blocks of any stream that has been
    /// reused or closed since the batch was opened.
    fn flush_v5_batches(&self, v5: &V5Initialized, scratch: &mut V5DrainScratch) {
        for mut batch in scratch.batches.drain(..) {
            if batch
                .inbox
                .add_pending_batch_if_generation(&batch.offsets, batch.generation)
            {
                batch.stream.notify_readable();
            } else {
                for &offset in &batch.offsets {
                    let _ = v5.peer.dynshm.free_chain(offset);
                }
            }
            batch.offsets.clear();
            scratch.spare_offsets.push(batch.offsets);
        }
    }

    /// The peer is done writing to this stream.
    async fn handle_v5_close(&self, stream_id: u32) {
        if let Some(stream) = self.get_stream(stream_id, STREAM_CLOSED).await {
            stream.half_close();
        }
    }

    /// Drain the V2 to V4 receive queue, whose elements name a buffer slice rather than a chain.
    ///
    /// An element for a stream that no longer exists still owns its slices, so they are recycled
    /// here rather than left in the pool.
    async fn drain_recv_queue_v4(&self) -> Option<Error> {
        let mut ret_err = None;
        loop {
            while let Ok(ele) = self.shared.queues().recv_queue.pop() {
                let state = ele.status & 0xff;
                if let Some(stream) = self.get_stream(ele.seq_id, state).await {
                    if let Err(err) = self.handle_stream_message(
                        stream,
                        BufferSliceWrapper {
                            fallback_slice: None,
                            offset: ele.offset_in_shm_buf,
                        },
                        state,
                    ) {
                        ret_err = Some(err);
                    }
                } else if state == STREAM_OPENED {
                    match self
                        .shared
                        .buffers()
                        .read_buffer_slice(ele.offset_in_shm_buf)
                    {
                        Ok(slice) => {
                            self.shared.buffers().recycle_buffers(slice);
                        }
                        Err(err) => {
                            return Some(err.into());
                        }
                    };
                } else {
                    continue;
                }
            }

            tokio::task::yield_now().await;
            if self.shared.queues().recv_queue.mark_not_working() {
                break;
            }
        }
        ret_err
    }

    /// Deliver a fallback payload the read loop has already read into `payload`, which the
    /// stream takes over as it is: no copy on either protocol.
    pub async fn handle_fallback_payload(
        &self,
        seq_id: u32,
        status: u32,
        payload: Vec<u8>,
    ) -> Option<Error> {
        tracing::warn!(
            "session {} receive fallback data, length:{} seqID:{} status:{}",
            self.shared.name,
            payload.len(),
            seq_id,
            status
        );
        self.shared
            .stats
            .fallback_read_count
            .fetch_add(1, Ordering::SeqCst);

        if let Some(v5) = self.shared.v5.as_deref() {
            let mut queue = v5.peer.recv_queue.lock().await;
            if let Some(err) = self.drain_v5_barrier(v5, &mut queue).await {
                return Some(err);
            }
            let stream = self.get_stream(seq_id, status).await?;
            stream.set_fallback();
            let err = stream
                .fill_fallback_to_read_buffer(Bytes::from(payload))
                .err();
            if status == STREAM_CLOSED {
                stream.half_close();
            }
            return err;
        }

        self.open_circuit_breaker().await;
        let stream = self.get_stream(seq_id, status).await?;
        let mut data = payload;
        let mut fallback_slice = BufferSlice::new(None, &mut data, 0, false);
        fallback_slice.write_index = data.len();
        let wrapper = BufferSliceWrapper {
            fallback_slice: Some(fallback_slice),
            offset: 0,
        };
        // The wrapper now owns the allocation through the raw pointer in its slice.
        std::mem::forget(data);
        self.handle_stream_message(stream, wrapper, status).err()
    }

    pub async fn handle_stream_close(
        &self,
        _event_header: &Header,
        buf: &[u8],
    ) -> (usize, usize, bool, Option<Error>) {
        const ID_LEN: usize = 4;
        if buf.len() < ID_LEN {
            return (0, HEADER_SIZE + ID_LEN, true, None);
        }
        let id = u32::from_be_bytes(buf[..ID_LEN].try_into().unwrap());
        tracing::debug!("receive peer stream[{}] goaway.", id);

        if let Some(v5) = self.shared.v5.as_deref() {
            let mut queue = v5.peer.recv_queue.lock().await;
            if let Some(err) = self.drain_v5_barrier(v5, &mut queue).await {
                return (HEADER_SIZE + ID_LEN, HEADER_SIZE, false, Some(err));
            }
        }

        match self.shared.streams.write().unwrap().remove(&id) {
            Some(stream) => {
                stream.half_close();
            }
            None => {
                tracing::warn!("missing stream: {}", id);
            }
        }
        (HEADER_SIZE + ID_LEN, HEADER_SIZE, false, None)
    }

    async fn get_stream(&self, id: u32, state: u32) -> Option<Stream> {
        if let Some(stream) = self.shared.streams.read().unwrap().get(&id) {
            return Some(stream.clone());
        }
        if !self.shared.is_client && state == STREAM_OPENED {
            let stream = Stream::new(id, 0, self.clone());
            self.shared
                .streams
                .write()
                .unwrap()
                .insert(id, stream.clone());

            let send = std::pin::pin!(self.shared.accept_tx.as_ref().unwrap().send(stream.clone()));
            let notified = std::pin::pin!(self.shared.shutdown_notify.notified());
            futures::future::select(send, notified).await;
            return Some(stream);
        }
        None
    }

    fn handle_stream_message(
        &self,
        stream: Stream,
        wrapper: BufferSliceWrapper,
        state: u32,
    ) -> Result<(), Error> {
        if state == STREAM_CLOSED {
            stream.half_close();
            return Ok(());
        }

        stream.fill_data_to_read_buffer(wrapper)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::negotiation::{ERROR_VERSION_NOT_SUPPORTED, NegotiationError};

    #[test]
    fn v4_fallback_error_requires_typed_network_or_protocol_error() {
        let network_or_protocol =
            NegotiationInitError::network_or_protocol(anyhow!("EOF while reading negotiation"));
        assert!(is_negotiation_fallback_error(&network_or_protocol));

        let rejected = NegotiationInitError::rejected(NegotiationError {
            code: ERROR_VERSION_NOT_SUPPORTED.to_owned(),
            message: "protocol version is not supported".to_owned(),
        });
        assert!(!is_negotiation_fallback_error(&rejected));

        let invalid_response = NegotiationInitError::invalid_response(anyhow!("invalid json"));
        assert!(!is_negotiation_fallback_error(&invalid_response));

        let untyped = anyhow!("v4 negotiation read protocol connection failed");
        assert!(!is_negotiation_fallback_error(&untyped));
    }
}
