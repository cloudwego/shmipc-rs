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
    os::fd::{AsRawFd, OwnedFd},
    sync::{
        Arc, LazyLock, Mutex, OnceLock, RwLock,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::anyhow;
use futures::future::Either;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
};

use self::pool::StreamPool;
pub use self::{config::SessionManagerConfig, manager::SessionManager};
use crate::{
    buffer::{
        manager::{BufferManager, add_global_buffer_manager_ref_count},
        slice::BufferSlice,
    },
    config::{Config, ProtocolConfig, ProtocolMode},
    consts::{EPOCH_INFO_MAX_LEN, FILE_NAME_MAX_LEN, HEADER_SIZE, MemMapType, QUEUE_INFO_MAX_LEN},
    error::Error,
    protocol::{
        event::{EventType, POLLING_EVENT_WITH_VERSION, check_event_valid},
        event_fd::{drain_eventfd, write_eventfd},
        header::Header,
        init_client_protocol, init_manager, init_server_protocol,
        initializer::v4::V4ClientInitError,
        negotiation::NegotiatedFeature,
    },
    queue::QueueManager,
    stats::Stats,
    stream::{BufferSliceWrapper, STREAM_CLOSED, STREAM_OPENED, Stream},
    transport::{TransportConnect, TransportStream},
    util::buf_reader::BufReader,
};

static BUF_READER_CAPACITY: LazyLock<usize> = LazyLock::new(|| {
    std::env::var("BUF_READER_CAPACITY")
        .map(|s| s.parse().unwrap_or(32 * 1024 * 1024))
        .unwrap_or(32 * 1024 * 1024)
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
    pub(crate) buffer_manager: Arc<BufferManager>,
    pub(crate) queue_manager: QueueManager,
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
    accept_tx: Option<mpsc::Sender<Stream>>,
    pub(crate) shutdown_notify: Notify,
    pub(crate) stats: Stats,
    // pub(crate) peer_addr: SocketAddr,
    read_loop: OnceLock<JoinHandle<()>>,
    write_loop: OnceLock<JoinHandle<()>>,
    polling_loop: OnceLock<JoinHandle<()>>,
    eventfd_loop: OnceLock<JoinHandle<()>>,
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

pub struct SendReady {
    pub(crate) hdr: Option<Header>,
    pub(crate) body: Vec<u8>,
    pub(crate) tx: oneshot::Sender<()>,
}

impl Session {
    pub(crate) fn stats_snapshot(&self) -> crate::stats::StatsSnapshot {
        self.shared.stats.snapshot()
    }

    pub async fn client<C>(
        session_id: usize,
        epoch_id: u64,
        rand_id: u64,
        sm_config: &mut SessionManagerConfig,
        connect: &C,
        addr: C::Address,
    ) -> Result<Self, Error>
    where
        C: TransportConnect,
        <C::Stream as TransportStream>::ReadHalf: Send + 'static,
        <C::Stream as TransportStream>::WriteHalf: Send + 'static,
        C::Address: Clone,
    {
        sm_config
            .config_mut()
            .share_memory_path_prefix
            .push_str(&format!("_{}", std::process::id()));
        if let MemMapType::MemMapTypeDevShmFile = sm_config.config().mem_map_type
            && sm_config.config().share_memory_path_prefix.len()
                + EPOCH_INFO_MAX_LEN
                + QUEUE_INFO_MAX_LEN
                > FILE_NAME_MAX_LEN
        {
            return Err(Error::FileNameTooLong);
        }
        if epoch_id > 0 {
            sm_config
                .config_mut()
                .share_memory_path_prefix
                .push_str(&format!("_epoch_{}_{}", epoch_id, rand_id));
        }
        if !sm_config.config().share_memory_path_prefix.is_empty() {
            sm_config.config_mut().queue_path = format!(
                "{}_queue_{}",
                sm_config.config().share_memory_path_prefix.clone(),
                session_id
            );
        }

        let config = sm_config.config().clone();
        let conn_stream = connect.connect(addr.clone()).await?;
        match Self::new(config.clone(), conn_stream, None).await {
            Ok(session) => Ok(session),
            Err(err)
                if matches!(config.protocol.mode, ProtocolMode::V4 { fallback: true })
                    && is_v4_fallback_error(&err) =>
            {
                let mut fallback_config = config;
                fallback_config.protocol = ProtocolConfig::default();
                let conn_stream = match connect.connect(addr).await {
                    Ok(conn_stream) => conn_stream,
                    Err(fallback_error) => {
                        return Err(anyhow::Error::new(V4FallbackFailed {
                            v4_error: err,
                            fallback_error: anyhow::Error::new(fallback_error),
                        })
                        .into());
                    }
                };
                match Self::new(fallback_config, conn_stream, None).await {
                    Ok(session) => Ok(session),
                    Err(fallback_error) => Err(anyhow::Error::new(V4FallbackFailed {
                        v4_error: err,
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

        let (bm, qm, protocol_initialized) = if is_client {
            let proto_version = crate::protocol::initial_client_proto_version(&config);
            let (bm, qm) = init_manager(&mut config, proto_version).map_err(|err| {
                anyhow!("create share memory buffer manager failed, error={}", err)
            })?;
            let initialized = match init_client_protocol(
                bm.path.clone(),
                bm.memfd,
                qm.path.clone(),
                qm.memfd,
                conn_fd,
                config.clone(),
                config.initialize_timeout,
            )
            .await
            {
                Ok(initialized) => initialized,
                Err(err) => {
                    let buffer_path = bm.path.clone();
                    qm.unmap();
                    add_global_buffer_manager_ref_count(&buffer_path, -1).await;
                    return Err(err);
                }
            };
            (bm, qm, initialized)
        } else {
            let mut initialized = init_server_protocol(conn_fd, config.initialize_timeout).await?;
            let (bm, qm) = initialized
                .shared_memory
                .take()
                .ok_or_else(|| anyhow!("server protocol did not initialize shared memory"))?;
            (bm, qm, initialized)
        };

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

        let session = Session {
            shared: Arc::new(Shared {
                pool: StreamPool::new(config.max_stream_num),
                config,
                next_stream_id: AtomicU32::new(next_stream_id),
                buffer_manager: bm,
                name: qm.path.clone(),
                queue_manager: qm,
                proto_version,
                msg_version,
                negotiated_feature,
                eventfd_send,
                shutdown: AtomicU32::new(0),
                unhealthy: AtomicU32::new(0),
                send_tx,
                shutdown_err: Mutex::new(None),
                streams: RwLock::new(HashMap::new()),
                is_client,
                accept_tx,
                shutdown_notify: Notify::new(),
                stats: Stats::default(),
                // peer_addr,
                read_loop: OnceLock::new(),
                write_loop: OnceLock::new(),
                polling_loop: OnceLock::new(),
                eventfd_loop: OnceLock::new(),
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
            .set(tokio::spawn(
                session.clone().write_loop(owned_write_half, send_rx),
            ))
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

        add_global_buffer_manager_ref_count(&self.shared.buffer_manager.path, -1).await;
        self.shared.queue_manager.unmap();
    }

    pub async fn wait_for_send(&self, hdr: Option<Header>, body: Vec<u8>) -> Result<(), Error> {
        let (tx, rx) = oneshot::channel::<()>();
        let ready = SendReady { hdr, body, tx };
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

    pub async fn wake_up_peer(&self) -> Result<(), Error> {
        if matches!(
            self.shared.negotiated_feature,
            NegotiatedFeature::EventQueuePolling { .. }
        ) {
            return Ok(());
        }
        if !self.shared.queue_manager.send_queue.mark_working() {
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
        _ = self
            .shared
            .send_tx
            .send(SendReady {
                hdr: None,
                body: POLLING_EVENT_WITH_VERSION[self.shared.msg_version as usize].clone(),
                tx: oneshot::channel().0,
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
            match buf {
                Ok(buf) => {
                    let (consumed, required, err) = self.handle_events(buf).await;
                    reader.consume(consumed);
                    len = required;
                    if let Some(err) = err
                        && !self.is_closed()
                    {
                        self.exit_err(err).await;
                        return;
                    }
                }
                Err(err) => {
                    self.exit_err(err.into()).await;
                    return;
                }
            }
        }
    }

    async fn write_loop<W>(self, writer: W, mut send_rx: mpsc::Receiver<SendReady>)
    where
        W: tokio::io::AsyncWrite,
    {
        tokio::pin!(writer);
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
                _ => return,
            };
            // send a header if ready
            if let Some(hdr) = ready.hdr
                && let Err(err) = writer.write_all(hdr.as_slice()).await
            {
                drop(ready.tx);
                self.exit_err(err.into()).await;
                return;
            }
            // send data from a body if given
            if !ready.body.is_empty()
                && let Err(err) = writer.write_all(&ready.body).await
            {
                drop(ready.tx);
                self.exit_err(err.into()).await;
                return;
            }

            // no error, successful send
            _ = ready.tx.send(());
        }
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
    "legacy fallback failed after v4 initialization failed; v4 error: {v4_error}; fallback error: \
     {fallback_error}"
)]
struct V4FallbackFailed {
    #[source]
    v4_error: anyhow::Error,
    fallback_error: anyhow::Error,
}

fn is_v4_fallback_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<V4ClientInitError>()
        .is_some_and(V4ClientInitError::is_network_or_protocol)
}

impl Session {
    pub async fn handle_events(&self, buf: &[u8]) -> (usize, usize, Option<Error>) {
        let mut consumed = 0;
        while buf[consumed..].len() >= HEADER_SIZE {
            let event_header = Header(buf[consumed..consumed + HEADER_SIZE].as_ptr() as *mut _);
            if let Err(err) = check_event_valid(&event_header) {
                return (consumed + HEADER_SIZE, HEADER_SIZE, Some(err));
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
                    self.handle_fallback_data(&event_header, &buf[consumed + HEADER_SIZE..])
                        .await
                }
                _ => {
                    return (
                        consumed + HEADER_SIZE,
                        HEADER_SIZE,
                        Some(Error::InvalidMsgType),
                    );
                }
            };
            consumed += n;
            if err.is_some() {
                return (consumed, HEADER_SIZE, err);
            }
            if stop {
                return (consumed, required, None);
            }
        }
        (consumed, HEADER_SIZE, None)
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

    async fn drain_recv_queue(&self) -> Option<Error> {
        let mut _consumed_count = 0;
        let mut ret_err = None;
        loop {
            while let Ok(ele) = self.shared.queue_manager.recv_queue.pop() {
                _consumed_count += 1;
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
                        .buffer_manager
                        .read_buffer_slice(ele.offset_in_shm_buf)
                    {
                        Ok(slice) => {
                            self.shared.buffer_manager.recycle_buffers(slice);
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
            if self.shared.queue_manager.recv_queue.mark_not_working() {
                break;
            }
        }
        ret_err
    }

    pub async fn handle_fallback_data(
        &self,
        event_header: &Header,
        buf: &[u8],
    ) -> (usize, usize, bool, Option<Error>) {
        let event_len = event_header.length() as usize;
        let payload_len = event_len - HEADER_SIZE;
        let fallback_data_header = 8;
        if buf.len() < payload_len {
            return (0, event_len, true, None);
        }
        assert!(payload_len >= fallback_data_header);
        // fallback data layout: eventHeader | seqID | status | payload
        let seq_id = u32::from_be_bytes(buf[..4].try_into().unwrap());
        // now the first byte of status is streamState, and the other byte of status is undefined.
        let status = u32::from_be_bytes(buf[4..8].try_into().unwrap()) & 0xff;
        tracing::warn!(
            "session {} receive fallback data, length:{} seqID:{} status:{}",
            self.shared.name,
            event_len - HEADER_SIZE - fallback_data_header,
            seq_id,
            status
        );
        self.open_circuit_breaker().await;
        self.shared
            .stats
            .fallback_read_count
            .fetch_add(1, Ordering::SeqCst);
        match self.get_stream(seq_id, status).await {
            Some(stream) => {
                let mut data = vec![0u8; payload_len - fallback_data_header];
                data.copy_from_slice(&buf[fallback_data_header..payload_len]);
                let mut fallback_slice = BufferSlice::new(None, &mut data, 0, false);
                fallback_slice.write_index = data.len();
                let wrapper = BufferSliceWrapper {
                    fallback_slice: Some(fallback_slice),
                    offset: 0,
                };
                // The wrapper now owns the allocation through the raw pointer in its slice.
                std::mem::forget(data);
                (
                    event_len,
                    HEADER_SIZE,
                    false,
                    self.handle_stream_message(stream, wrapper, status).err(),
                )
            }
            None => (event_len, HEADER_SIZE, false, None),
        }
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
            V4ClientInitError::network_or_protocol(anyhow!("EOF while reading negotiation"));
        assert!(is_v4_fallback_error(&network_or_protocol));

        let rejected = V4ClientInitError::rejected(NegotiationError {
            code: ERROR_VERSION_NOT_SUPPORTED.to_owned(),
            message: "protocol version is not supported".to_owned(),
        });
        assert!(!is_v4_fallback_error(&rejected));

        let invalid_response = V4ClientInitError::invalid_response(anyhow!("invalid json"));
        assert!(!is_v4_fallback_error(&invalid_response));

        let untyped = anyhow!("v4 negotiation read protocol connection failed");
        assert!(!is_v4_fallback_error(&untyped));
    }
}
