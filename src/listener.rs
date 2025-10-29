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
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use futures::future::Either;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    config::Config,
    session::Session,
    stream::Stream,
    transport::{TransportListen, TransportListener, TransportStream},
};

#[derive(Debug)]
pub struct Listener {
    stream_rx: mpsc::UnboundedReceiver<io::Result<Stream>>,
    sessions: Arc<Mutex<Vec<Session>>>,
    accept_handle: JoinHandle<()>,
}

impl Listener {
    pub async fn new<L>(listen: L, addr: L::Address, config: Config) -> Result<Self, io::Error>
    where
        L: TransportListen,
        L::Listener: Send + 'static,
        <L::Listener as TransportListener>::Stream: Send,
        <<L::Listener as TransportListener>::Stream as TransportStream>::ReadHalf: Send + 'static,
        <<L::Listener as TransportListener>::Stream as TransportStream>::WriteHalf: Send + 'static,
        <L::Listener as TransportListener>::Address: Send,
    {
        let listener = listen.listen(addr).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let sessions = Arc::new(Mutex::new(Vec::new()));

        let handle = tokio::spawn(Self::accept_loop(listener, config, tx, sessions.clone()));
        Ok(Self {
            stream_rx: rx,
            sessions,
            accept_handle: handle,
        })
    }

    pub fn poll_accept(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Stream>> {
        match self.stream_rx.poll_recv(cx) {
            Poll::Ready(Some(res)) => Poll::Ready(res),
            Poll::Ready(None) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "shmipc listener is aborted",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Accept a ShmIPC connection [`Stream`].
    ///
    /// NOTE: after using the [`Stream`], you MUST explicitly call `stream.close().await` for
    /// releasing it, otherwise it will cause resource leak.
    pub async fn accept(&mut self) -> io::Result<Stream> {
        match self.stream_rx.recv().await {
            Some(res) => res,
            None => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "shmipc listener is aborted",
            )),
        }
    }

    async fn accept_loop<L>(
        listener: L,
        config: Config,
        stream_tx: mpsc::UnboundedSender<io::Result<Stream>>,
        sessions: Arc<Mutex<Vec<Session>>>,
    ) where
        L: TransportListener,
        <L::Stream as TransportStream>::ReadHalf: Send + 'static,
        <L::Stream as TransportStream>::WriteHalf: Send + 'static,
    {
        let mut closed = std::pin::pin!(stream_tx.closed());
        loop {
            let res = match futures::future::select(std::pin::pin!(listener.accept()), &mut closed)
                .await
            {
                Either::Left((res, _)) => res,
                Either::Right(_) => {
                    tracing::info!("[ShmIPC] session receiver is closed, shutdown listener");
                    return;
                }
            };

            match res {
                Ok((stream, _)) => {
                    let (tx, rx) = mpsc::channel::<Stream>(config.max_stream_num);

                    match Session::server(config.clone(), stream, tx).await {
                        Ok(session) => {
                            sessions.lock().unwrap().push(session.clone());
                            tokio::spawn(session.recv_loop(rx, stream_tx.clone()));
                        }
                        Err(err) => {
                            tracing::warn!("[ShmIPC] failed to create session, err: {err}");
                            continue;
                        }
                    }
                }
                Err(err) => {
                    _ = stream_tx.send(Err(err));
                    return;
                }
            }
        }
    }

    pub async fn close(self) {
        let sessions = self.sessions.lock().unwrap().clone();
        for session in sessions {
            session.close().await;
        }
        self.accept_handle.abort();
    }
}
