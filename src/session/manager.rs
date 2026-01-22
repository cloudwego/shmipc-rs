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

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use arc_swap::ArcSwap;
use futures::future::Either;
use tokio::sync::Notify;

use super::{Session, config::SessionManagerConfig};
use crate::{
    consts::StateType,
    error::Error,
    stream::Stream,
    transport::{TransportConnect, TransportStream},
};

const SESSION_ROUND_ROBIN_THRESHOLD: usize = 32;

/// SessionManager provides an implementation similar to a connection pool, which provides a pair of
/// connections(stream) for communication between two processes. When `get_stream()` returns an
/// error, you may need to fallback according to your own scenario, such as fallback to a unix
/// domain socket.
///
/// When a client process communicates with a server process, it is best practice to share a
/// SessionManager globally in the client process to create a stream.
///
/// When the client needs to communicate with multiple server processes, a separate
/// SessionManager should be maintained for each different server. At the same time,
/// `queue_path` and `share_memory_path_prefix` need to be kept different.
pub struct SessionManager<C: TransportConnect> {
    inner: Arc<SessionManagerInner<C>>,
}

impl<C: TransportConnect> Clone for SessionManager<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[allow(dead_code)]
struct SessionManagerInner<C: TransportConnect> {
    sm_config: SessionManagerConfig,
    connect: C,
    addr: C::Address,
    sessions: Vec<ArcSwap<Session>>,
    count: AtomicUsize,
    epoch: u64,
    rand_id: u64,
    state: StateType,
    shutdown_tx: Arc<Notify>,
}

impl<C: TransportConnect> SessionManager<C>
where
    C: TransportConnect + Send + Sync + 'static,
    C::Stream: Send,
    <C::Stream as TransportStream>::ReadHalf: Send + 'static,
    <C::Stream as TransportStream>::WriteHalf: Send + 'static,
    C::Address: Clone + Send + Sync,
{
    /// Create a new SessionManager.
    pub async fn new(
        mut sm_config: SessionManagerConfig,
        connect: C,
        addr: C::Address,
    ) -> Result<Self, Error> {
        let mut sessions = Vec::with_capacity(sm_config.session_num());

        for i in 0..sm_config.session_num() {
            let session = Session::client(i, 0, 0, &mut sm_config, &connect, addr.clone()).await?;
            sessions.push(ArcSwap::from_pointee(session));
        }
        let shutdown = Arc::new(Notify::new());
        let sm = Self {
            inner: Arc::new(SessionManagerInner {
                sm_config: sm_config.clone(),
                connect,
                addr,
                sessions,
                count: AtomicUsize::new(0),
                epoch: 0,
                rand_id: 0,
                state: StateType::DefaultState,
                shutdown_tx: shutdown.clone(),
            }),
        };
        for i in 0..sm_config.session_num() {
            tokio::spawn({
                let shutdown_rx = shutdown.clone();
                let sm = sm.clone();
                async move { sm.rebuild_session(shutdown_rx, i).await }
            });
        }

        Ok(sm)
    }

    /// Return a shmipc's stream from stream pool.
    ///
    /// Every stream should explicitly call `put_back()` to return it to SessionManager for next
    /// time using, otherwise it will cause resource leak.
    pub fn get_stream(&self) -> Result<Stream, Error> {
        let sessions = &self.inner.sessions;
        if sessions.is_empty() {
            return Err(Error::SessionUnhealthy);
        }
        let i = ((self.inner.count.fetch_add(1, Ordering::SeqCst) + 1)
            / SESSION_ROUND_ROBIN_THRESHOLD)
            % sessions.len();
        sessions[i].load().get_or_open_stream(i)
    }

    /// Return unused stream to stream pool for next time using.
    pub async fn put_back(&self, stream: Stream) {
        self.inner.sessions[stream.session_id()]
            .load()
            .put_or_close_stream(stream)
            .await;
    }

    /// Close all sessions in SessionManager.
    pub async fn close(&self) {
        self.inner.shutdown_tx.notify_waiters();
        for s in &self.inner.sessions {
            s.load().close().await;
        }
    }

    async fn rebuild_session(self, shutdown_rx: Arc<Notify>, i: usize) {
        let mut shutdown_rx = Box::pin(shutdown_rx.notified());

        loop {
            let session = self.inner.sessions[i].load();
            let notified = std::pin::pin!(session.shared.shutdown_notify.notified());
            match futures::future::select(notified, &mut shutdown_rx).await {
                Either::Left(_) => {}
                Either::Right(_) => return,
            }

            loop {
                tokio::time::sleep(self.inner.sm_config.config().rebuild_interval).await;

                match Session::client(
                    i,
                    0,
                    0,
                    &mut self.inner.sm_config.clone(),
                    &self.inner.connect,
                    self.inner.addr.clone(),
                )
                .await
                {
                    Ok(new_session) => {
                        self.inner.sessions[i].store(Arc::new(new_session));
                        tracing::info!("rebuild session {i} success");
                        break;
                    }
                    Err(e) => {
                        tracing::error!(
                            "rebuild session {i} error: {:?}, retry after {:?}",
                            e,
                            self.inner.sm_config.config().rebuild_interval
                        );
                    }
                }
            }
        }
    }
}
