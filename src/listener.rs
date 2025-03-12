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
use volo::net::{Address, DefaultIncoming, MakeIncoming, incoming::Incoming};

use crate::{config::Config, session::Session, stream::Stream};

#[derive(Debug)]
pub struct Listener {
    stream_rx: mpsc::UnboundedReceiver<io::Result<Option<Stream>>>,
    sessions: Arc<Mutex<Vec<Session>>>,
}

impl Listener {
    pub async fn new(addr: impl Into<Address>, config: Config) -> anyhow::Result<Self> {
        let addr = addr.into();
        if let Address::Unix(socket) = &addr {
            if let Some(path) = socket.as_pathname() {
                if path.exists() {
                    std::fs::remove_file(path)?;
                }
            }
        }
        let incoming = addr.make_incoming().await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let sessions = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(Self::accept_loop(incoming, config, tx, sessions.clone()));
        Ok(Self {
            stream_rx: rx,
            sessions,
        })
    }

    pub async fn accept(&mut self) -> io::Result<Option<Stream>> {
        self.stream_rx.recv().await.unwrap_or(Ok(None))
    }

    async fn accept_loop(
        mut incoming: DefaultIncoming,
        config: Config,
        stream_tx: mpsc::UnboundedSender<io::Result<Option<Stream>>>,
        sessions: Arc<Mutex<Vec<Session>>>,
    ) {
        loop {
            tokio::select! {
                _ = stream_tx.closed() => {
                    return;
                }
                res = incoming.accept() => {
                    match res {
                        Ok(Some(conn)) => {
                            let (tx, rx) = mpsc::channel::<Stream>(config.max_stream_num);

                            match Session::server(config.clone(), conn.stream, tx).await {
                                Ok(session) => {
                                    sessions.lock().unwrap().push(session.clone());
                                    tokio::spawn(session.recv_loop(rx, stream_tx.clone()));
                                }
                                Err(err) => {
                                    tracing::warn!("failed to create shmipc session: {}", err);
                                    continue;
                                }
                            }
                        }
                        Ok(None) => {
                            _ = stream_tx.send(Ok(None));
                            return;
                        }
                        Err(err) => {
                            _ = stream_tx.send(Err(err));
                            return;
                        }
                    }
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
