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
};

use tokio::sync::mpsc;

use crate::{
    config::Config,
    error::Error,
    session::Session,
    stream::Stream,
    transport::{TransportListen, TransportListener, TransportStream},
};

#[derive(Debug)]
pub struct Listener {
    stream_rx: mpsc::UnboundedReceiver<io::Result<Option<Stream>>>,
    sessions: Arc<Mutex<Vec<Session>>>,
}

impl Listener {
    pub async fn new<L>(listen: L, addr: L::Address, config: Config) -> Result<Self, Error>
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

        tokio::spawn(Self::accept_loop(listener, config, tx, sessions.clone()));
        Ok(Self {
            stream_rx: rx,
            sessions,
        })
    }

    pub async fn accept(&mut self) -> io::Result<Option<Stream>> {
        self.stream_rx.recv().await.unwrap_or(Ok(None))
    }

    async fn accept_loop<L>(
        listener: L,
        config: Config,
        stream_tx: mpsc::UnboundedSender<io::Result<Option<Stream>>>,
        sessions: Arc<Mutex<Vec<Session>>>,
    ) where
        L: TransportListener,
        <L::Stream as TransportStream>::ReadHalf: Send + 'static,
        <L::Stream as TransportStream>::WriteHalf: Send + 'static,
    {
        loop {
            let res = tokio::select! {
                res = listener.accept() => res,
                _ = stream_tx.closed() => {
                    tracing::warn!("[ShmIPC] session receiver is closed, shutdown listener");
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
    }
}
