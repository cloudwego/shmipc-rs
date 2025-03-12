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
pub mod ext;
pub mod manager;
pub mod pool;

use std::{
    collections::HashMap,
    sync::{
        Arc, LazyLock, Mutex, RwLock,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::anyhow;
use config::SessionManagerConfig;
use ext::ConnStreamExt;
use motore::make::MakeConnection;
use nix::libc;
use pool::StreamPool;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    sync::{Notify, mpsc, oneshot},
};
use volo::{
    net::{
        Address,
        conn::{ConnStream, OwnedReadHalf, OwnedWriteHalf},
        dial::{DefaultMakeTransport, MakeTransport},
    },
    util::buf_reader::BufReader,
};

use crate::{
    buffer::{
        manager::{BufferManager, add_global_buffer_manager_ref_count},
        slice::BufferSlice,
    },
    config::Config,
    consts::{EPOCH_INFO_MAX_LEN, FILE_NAME_MAX_LEN, HEADER_SIZE, MemMapType, QUEUE_INFO_MAX_LEN},
    error::Error,
    protocol::{
        event::{EventType, POLLING_EVENT_WITH_VERSION, check_event_valid},
        header::Header,
        init_client_protocol, init_manager, init_server_protocol,
    },
    queue::QueueManager,
    stats::Stats,
    stream::{BufferSliceWrapper, STREAM_CLOSED, STREAM_OPENED, Stream},
};

static BUF_READER_CAPACITY: LazyLock<usize> = LazyLock::new(|| {
    std::env::var("BUF_READER_CAPACITY")
        .map(|s| s.parse().unwrap_or(32 * 1024 * 1024))
        .unwrap_or(32 * 1024 * 1024)
});

/// Session is used to wrap a reliable ordered connection and to
/// multiplex it into multiple streams.
#[derive(Clone, Debug)]
pub struct Session {
    pub(crate) shared: Arc<Shared>,
}

#[derive(Debug)]
pub struct Shared {
    config: Config,
    /// next_stream_id is the next stream we should send.
    ///
    /// In client mode, nextStreamID is odd number.
    ///
    /// In server mode, nextStreamID is even number.
    next_stream_id: AtomicU32,
    pub(crate) buffer_manager: Arc<BufferManager>,
    pub(crate) queue_manager: QueueManager,
    pub(crate) communication_version: u8,
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
    pub(crate) peer_addr: Option<Address>,
}

pub struct SendReady {
    pub(crate) hdr: Option<Header>,
    pub(crate) body: Vec<u8>,
    pub(crate) tx: oneshot::Sender<()>,
}

impl Session {
    pub async fn client(
        session_id: usize,
        epoch_id: u64,
        rand_id: u64,
        sm_config: &mut SessionManagerConfig,
        addr: &Address,
    ) -> Result<Self, Error> {
        let mut mt = DefaultMakeTransport::new();
        mt.set_connect_timeout(sm_config.config().connection_timeout);
        mt.set_read_timeout(sm_config.config().connection_read_timeout);
        mt.set_write_timeout(Some(sm_config.config().connection_write_timeout));
        let conn_stream = mt.make_connection(addr.clone()).await?.stream;

        sm_config
            .config_mut()
            .share_memory_path_prefix
            .push_str(&format!("_{}", std::process::id()));
        if let MemMapType::MemMapTypeDevShmFile = sm_config.config().mem_map_type {
            if sm_config.config().share_memory_path_prefix.len()
                + EPOCH_INFO_MAX_LEN
                + QUEUE_INFO_MAX_LEN
                > FILE_NAME_MAX_LEN
            {
                return Err(Error::FileNameTooLong);
            }
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

        if let MemMapType::MemMapTypeMemFd = sm_config.config().mem_map_type {
            if conn_stream.is_tcp() {
                return Err(anyhow!(
                    "conn_stream must be unix when config.mem_map_type is MemMapTypeMemFd"
                ))?;
            }
        }

        Ok(Self::new(sm_config.config().clone(), conn_stream, None).await?)
    }

    pub async fn server(
        config: Config,
        conn_stream: ConnStream,
        accept_tx: mpsc::Sender<Stream>,
    ) -> Result<Self, Error> {
        Ok(Self::new(config, conn_stream, Some(accept_tx)).await?)
    }

    async fn new(
        mut config: Config,
        conn_stream: ConnStream,
        accept_tx: Option<mpsc::Sender<Stream>>,
    ) -> Result<Session, anyhow::Error> {
        config
            .verify()
            .map_err(|err| err.context("verify config failed"))?;

        let conn_fd = conn_stream.as_raw_fd();
        let peer_addr = conn_stream.peer_addr();
        let (owned_read_half, owned_write_half) = conn_stream.into_split();
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

        let (bm, qm, communication_version) = if is_client {
            let (bm, qm) = init_manager(&mut config).map_err(|err| {
                anyhow!("create share memory buffer manager failed, error={}", err)
            })?;
            let version = init_client_protocol(
                bm.path.clone(),
                bm.memfd,
                qm.path.clone(),
                qm.memfd,
                conn_fd,
                config.mem_map_type,
                config.initialize_timeout,
            )
            .await?;
            (bm, qm, version)
        } else {
            init_server_protocol(conn_fd, config.initialize_timeout).await?
        };

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

        let session = Session {
            shared: Arc::new(Shared {
                pool: StreamPool::new(config.max_stream_num),
                config,
                next_stream_id: AtomicU32::new(next_stream_id),
                buffer_manager: bm,
                name: qm.path.clone(),
                queue_manager: qm,
                communication_version,
                shutdown: AtomicU32::new(0),
                unhealthy: AtomicU32::new(0),
                send_tx,
                shutdown_err: Mutex::new(None),
                streams: RwLock::new(HashMap::new()),
                is_client,
                accept_tx,
                shutdown_notify: Notify::new(),
                stats: Stats::default(),
                peer_addr,
            }),
        };

        // uds read
        tokio::spawn(session.clone().read_loop(owned_read_half));

        // uds write
        tokio::spawn(session.clone().write_loop(owned_write_half, send_rx));

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

        tracing::trace!("{} open stream {}", self.shared.name, id);

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
        tokio::select! {
            _ = self.shared.send_tx.send(ready) => {
                if let Err(err) = rx.await {
                    return Err(anyhow!("wait for send failed, error={}", err).into());
                }
            }
            _ = tokio::time::sleep(self.shared.config.connection_write_timeout) => {
                tracing::debug!("write timeout, send channel is full");
                return Err(Error::ConnectionWriteTimeout);
            }
        };
        Ok(())
    }

    pub async fn wake_up_peer(&self) -> Result<(), Error> {
        if !self.shared.queue_manager.send_queue.mark_working() {
            return Ok(());
        }
        self.shared
            .stats
            .send_polling_event_count
            .fetch_add(1, Ordering::SeqCst);
        _ = self
            .shared
            .send_tx
            .send(SendReady {
                hdr: None,
                body: POLLING_EVENT_WITH_VERSION[self.shared.communication_version as usize]
                    .clone(),
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
        stream_tx: mpsc::UnboundedSender<std::io::Result<Option<Stream>>>,
    ) {
        loop {
            tokio::select! {
                res = rx.recv() => {
                    if let Some(stream) = res {
                        if stream_tx.send(Ok(Some(stream))).is_err() {
                            return;
                        }
                    } else {
                        tracing::warn!("session unhealthy");
                        continue;
                    }
                }
                _ = self.shared.shutdown_notify.notified() => {
                    tracing::info!("session shutdown");
                    return;
                }
            }
        }
    }

    async fn read_loop(self, owned_read_half: OwnedReadHalf) {
        let mut reader = BufReader::with_capacity(*BUF_READER_CAPACITY, owned_read_half);
        let shutdown_notified = self.shared.shutdown_notify.notified();
        let mut len = HEADER_SIZE;
        tokio::pin!(shutdown_notified);
        loop {
            tokio::select! {
                buf = reader.fill_buf_at_least(len) => {
                    if self.shared.shutdown.load(Ordering::SeqCst) == 1 {
                        return;
                    }
                    match buf {
                        Ok(buf) => {
                            let (consumed, required, err) = self.handle_events(buf).await;
                            reader.consume(consumed);
                            len = required;
                            if let Some(err) = err  {
                                if !self.is_closed() {
                                    self.exit_err(err).await;
                                    return;
                                }
                            }
                        }
                        Err(err) => {
                            self.exit_err(err.into()).await;
                            return;
                        }
                    }
                }
                _ = &mut shutdown_notified => {
                    return;
                }
            }
        }
    }

    async fn write_loop(
        self,
        mut owned_write_half: OwnedWriteHalf,
        mut send_rx: mpsc::Receiver<SendReady>,
    ) {
        let shutdown_notified = self.shared.shutdown_notify.notified();
        tokio::pin!(shutdown_notified);
        loop {
            tokio::select! {
                ready = send_rx.recv() => {
                    if let Some(ready) = ready {
                        // send a header if ready
                        if let Some(hdr) = ready.hdr {
                            if let Err(err) = owned_write_half.write_all(hdr.as_slice()).await {
                                drop(ready.tx);
                                self.exit_err(err.into()).await;
                                return;
                            }
                        }
                        // send data from a body if given
                        if !ready.body.is_empty() {
                            if let Err(err) = owned_write_half.write_all(&ready.body).await {
                                drop(ready.tx);
                                self.exit_err(err.into()).await;
                                return;
                            }
                        }

                        // no error, successful send
                        _ = ready.tx.send(());
                    } else {
                        return;
                    }
                }
                _ = &mut shutdown_notified => {
                    return;
                }
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
                    self.handle_polling(&event_header, &buf[consumed + HEADER_SIZE..])
                        .await
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
                            return (HEADER_SIZE, HEADER_SIZE, false, Some(err.into()));
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
        (HEADER_SIZE, HEADER_SIZE, false, ret_err)
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
        let mut data = vec![0u8; payload_len - fallback_data_header];
        data.copy_from_slice(&buf[fallback_data_header..payload_len]);
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
        let mut fallback_slice = BufferSlice::new(None, &mut data, 0, false);
        fallback_slice.write_index = data.len();
        std::mem::forget(data);
        self.shared
            .stats
            .fallback_read_count
            .fetch_add(1, Ordering::SeqCst);
        match self.get_stream(seq_id, status).await {
            Some(stream) => (
                event_len,
                HEADER_SIZE,
                false,
                self.handle_stream_message(
                    stream,
                    BufferSliceWrapper {
                        fallback_slice: Some(fallback_slice),
                        offset: 0,
                    },
                    status,
                )
                .err(),
            ),
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
            tokio::select! {
                _ = self.shared.accept_tx.as_ref().unwrap().send(stream.clone()) => {},
                _ = self.shared.shutdown_notify.notified() => {},
            }
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
