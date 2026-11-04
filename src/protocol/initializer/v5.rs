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
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::Arc,
};

use anyhow::anyhow;
use tokio::sync::Mutex;

use super::{
    ProtocolInitialized, SharedMemoryMode::PerEndpointPool, ack_and_recv_fds,
    send_shm_metadata_and_fds, wait_event_header,
};
use crate::{
    config::Config,
    consts::{HEADER_SIZE, MEMFD_COUNT, MEMFD_COUNT_EVENTFD_V5},
    protocol::{
        event::EventType,
        event_fd::{EventFdPair, create_eventfd},
        header::Header,
        negotiation::{Negotiated, NegotiatedFeature, PROTO_VERSION_V5, read_message_body},
        protocol_trace,
    },
    v5::{
        dynshm::{LocalManager, RemoteManager},
        queue::{RecvQueue, SendQueue},
    },
};

/// What goes in the size field of the metadata.
///
/// The field is four bytes and the segment is four gigabytes, which is one byte too many to
/// encode, so both SDKs send zero and let the receiver take the size from the descriptor it is
/// about to be handed. See [`RemoteManager::map`].
const ADVERTISED_DYNSHM_SIZE: u32 = 0;
/// Smallest V5 metadata body: a queue path length and a dynamic segment size, with a path between.
const METADATA_MIN_SIZE: usize = 2 + 4;

pub enum ProtocolInitializerV5 {
    Client(Box<Client>),
    Server(Box<Server>),
}

impl ProtocolInitializerV5 {
    pub fn init(&self) -> Result<ProtocolInitialized, anyhow::Error> {
        match self {
            Self::Client(client) => client.init(),
            Self::Server(server) => server.init(),
        }
    }
}

pub struct Client {
    pub(crate) conn_fd: RawFd,
    pub(crate) config: Config,
    pub(crate) negotiated: Negotiated,
}

impl Client {
    /// Publishes this side's shared memory, then maps the peer's.
    pub fn init(&self) -> Result<ProtocolInitialized, anyhow::Error> {
        let own = create_own_shm(&self.config)?;
        let wakeup_recv =
            send_fds_to_peer(self.conn_fd, &self.config.queue_path, &own, self.negotiated)
                .map_err(|err| anyhow!("v5 client publish share memory failed, reason:{}", err))?;

        let (peer, wakeup_send) = recv_fds_from_peer(self.conn_fd, self.negotiated)
            .map_err(|err| anyhow!("v5 client map peer share memory failed, reason:{}", err))?;

        initialized(own, peer, self.negotiated, wakeup_send, wakeup_recv)
    }
}

pub struct Server {
    pub(crate) conn_fd: RawFd,
    pub(crate) config: Config,
    pub(crate) negotiated: Negotiated,
}

impl Server {
    /// Maps the peer's shared memory, then publishes this side's.
    pub fn init(&self) -> Result<ProtocolInitialized, anyhow::Error> {
        let (peer, wakeup_send) = recv_fds_from_peer(self.conn_fd, self.negotiated)
            .map_err(|err| anyhow!("v5 server map peer share memory failed, reason:{}", err))?;

        let own = create_own_shm(&self.config)?;
        let wakeup_recv =
            send_fds_to_peer(self.conn_fd, &self.config.queue_path, &own, self.negotiated)
                .map_err(|err| anyhow!("v5 server publish share memory failed, reason:{}", err))?;

        initialized(own, peer, self.negotiated, wakeup_send, wakeup_recv)
    }
}

/// Creates the queue and the dynamic segment this side publishes.
fn create_own_shm(config: &Config) -> Result<OwnEndpoint, anyhow::Error> {
    let send_queue = SendQueue::new(&config.queue_path, config.queue_cap)?;
    let dynshm = LocalManager::new()?;
    tracing::info!(
        "v5 created own queue (cap={} size={}) and dynshm (size={})",
        config.queue_cap,
        send_queue.size(),
        dynshm.size(),
    );
    Ok(OwnEndpoint { send_queue, dynshm })
}

/// Hands this side's queue and dynamic segment to the peer.
///
/// The returned descriptor is the eventfd this side waits on: each end creates the one it reads
/// and passes it over for the peer to signal, so a wakeup only ever travels in one direction and
/// neither end has to agree with the other about which of a pair is which.
fn send_fds_to_peer(
    conn_fd: RawFd,
    queue_path: &str,
    own: &OwnEndpoint,
    negotiated: Negotiated,
) -> Result<Option<OwnedFd>, anyhow::Error> {
    let wakeup_recv = if negotiated.feature == NegotiatedFeature::EventFd {
        Some(create_eventfd()?)
    } else {
        None
    };

    let mut event = generate_v5_shm_metadata(queue_path, negotiated.message_version)?;
    let h = Header(event.as_mut_ptr());
    tracing::info!(
        "v5 send_fds_to_peer queue fd:{} dynshm fd:{} header:{}",
        own.send_queue.fd().as_raw_fd(),
        own.dynshm.fd().as_raw_fd(),
        h
    );
    protocol_trace(&h, &event[HEADER_SIZE..], true);

    let mut fds = Vec::with_capacity(MEMFD_COUNT_EVENTFD_V5);
    fds.push(own.send_queue.fd().as_raw_fd());
    fds.push(own.dynshm.fd().as_raw_fd());
    if let Some(wakeup_recv) = &wakeup_recv {
        fds.push(wakeup_recv.as_raw_fd());
    }

    send_shm_metadata_and_fds(conn_fd, &event, &fds)?;
    Ok(wakeup_recv)
}

/// Takes the peer's queue and dynamic segment and maps them.
///
/// The returned descriptor is the eventfd the peer created for this side to signal it on.
fn recv_fds_from_peer(
    conn_fd: RawFd,
    negotiated: Negotiated,
) -> Result<(PeerEndpoint, Option<OwnedFd>), anyhow::Error> {
    let mut buf = [0u8; HEADER_SIZE];
    let h = wait_event_header(conn_fd, EventType::TYPE_SHARE_MEMORY_BY_MEMFD, &mut buf)?;
    let body = read_message_body(conn_fd, &h)?;
    let (queue_path, dynshm_size) = extract_v5_shm_metadata(&body)?;
    tracing::info!(
        "v5 recv memfd metadata queue_path:{} dynshm_size:{}",
        queue_path,
        dynshm_size
    );

    let expected_fd_count = if negotiated.feature == NegotiatedFeature::EventFd {
        MEMFD_COUNT_EVENTFD_V5
    } else {
        MEMFD_COUNT
    };
    let mut fds = ack_and_recv_fds(conn_fd, negotiated.message_version, expected_fd_count)?
        .into_iter()
        // SAFETY: each descriptor arrived over SCM_RIGHTS, so this process is its only owner.
        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });

    let (Some(queue_fd), Some(dynshm_fd)) = (fds.next(), fds.next()) else {
        return Err(anyhow!(
            "v5 peer did not send a queue and a dynshm descriptor"
        ));
    };
    let wakeup_send = if negotiated.feature == NegotiatedFeature::EventFd {
        Some(fds.next().ok_or_else(|| {
            anyhow!("v5 peer did not send the wakeup descriptor eventfd_wakeup requires")
        })?)
    } else {
        None
    };
    drop(fds);

    tracing::info!(
        "v5 recv memfd queue fd:{} dynshm fd:{}",
        queue_fd.as_raw_fd(),
        dynshm_fd.as_raw_fd()
    );

    let recv_queue =
        RecvQueue::map(queue_fd).map_err(|err| anyhow!("map peer queue failed, reason:{}", err))?;
    let dynshm = RemoteManager::map(dynshm_fd, dynshm_size)
        .map_err(|err| anyhow!("map peer dynshm failed, reason:{}", err))?;

    tracing::info!("v5 mapped peer queue and dynshm");
    Ok((
        PeerEndpoint {
            recv_queue: Mutex::new(recv_queue),
            dynshm,
        },
        wakeup_send,
    ))
}

/// Pairs up what the two directions produced.
fn initialized(
    own: OwnEndpoint,
    peer: PeerEndpoint,
    negotiated: Negotiated,
    wakeup_send: Option<OwnedFd>,
    wakeup_recv: Option<OwnedFd>,
) -> Result<ProtocolInitialized, anyhow::Error> {
    let Negotiated {
        message_version,
        feature,
    } = negotiated;

    let eventfd = match (wakeup_send, wakeup_recv) {
        (Some(wakeup_send), Some(wakeup_recv)) => Some(EventFdPair {
            wakeup_send,
            wakeup_recv,
        }),
        (None, None) => None,
        _ => return Err(anyhow!("v5 exchanged a wakeup descriptor in one direction")),
    };

    Ok(ProtocolInitialized::new(
        PerEndpointPool(Some(Box::new(V5Initialized { own, peer }))),
        PROTO_VERSION_V5,
        message_version,
        feature,
        eventfd,
    ))
}

/// Builds the metadata that precedes the descriptors: `[header][path_len:2][path][dynshm_size:4]`.
fn generate_v5_shm_metadata(queue_path: &str, version: u8) -> Result<Vec<u8>, anyhow::Error> {
    let path_len = u16::try_from(queue_path.len()).map_err(|_| {
        anyhow!(
            "v5 queue path is {} bytes, too long to send",
            queue_path.len()
        )
    })?;
    if path_len == 0 {
        return Err(anyhow!("v5 requires a non-empty queue path"));
    }

    let mut data = vec![0u8; HEADER_SIZE + 2 + queue_path.len() + 4];
    let mut h = Header(data.as_mut_ptr());
    h.encode(
        data.len() as u32,
        version,
        EventType::TYPE_SHARE_MEMORY_BY_MEMFD,
    );

    let mut offset = HEADER_SIZE;
    data[offset..offset + 2].copy_from_slice(&path_len.to_be_bytes());
    offset += 2;
    data[offset..offset + queue_path.len()].copy_from_slice(queue_path.as_bytes());
    offset += queue_path.len();
    data[offset..offset + 4].copy_from_slice(&ADVERTISED_DYNSHM_SIZE.to_be_bytes());
    Ok(data)
}

/// Reads back what [`generate_v5_shm_metadata`] wrote, from a body a peer chose the contents of.
fn extract_v5_shm_metadata(body: &[u8]) -> Result<(&str, u32), anyhow::Error> {
    if body.len() < METADATA_MIN_SIZE {
        return Err(anyhow!(
            "v5 memfd metadata is {} bytes, too short to hold a queue path and a size",
            body.len()
        ));
    }

    let path_len = u16::from_be_bytes([body[0], body[1]]) as usize;
    let path_end = 2 + path_len;
    if path_len == 0 || body.len() != path_end + 4 {
        return Err(anyhow!(
            "v5 memfd metadata declares a {} byte queue path in a {} byte body",
            path_len,
            body.len()
        ));
    }

    let queue_path = std::str::from_utf8(&body[2..path_end])
        .map_err(|err| anyhow!("v5 memfd metadata queue path is not utf-8: {}", err))?;
    let dynshm_size = u32::from_be_bytes([
        body[path_end],
        body[path_end + 1],
        body[path_end + 2],
        body[path_end + 3],
    ]);
    Ok((queue_path, dynshm_size))
}

/// Everything a V5 session needs once both sides have exchanged shared memory.
#[derive(Debug)]
pub struct V5Initialized {
    /// The objects this process created, which it produces into.
    pub own: OwnEndpoint,
    /// The objects the peer created, which this process consumes from.
    pub peer: PeerEndpoint,
}

impl V5Initialized {
    /// Stops using both directions, the way a session does on its way out.
    ///
    /// Nothing here waits. The receive queue is only closed if it is free at this moment: a
    /// drain holding it is not waited on, because it stops on the session's own shutdown flag
    /// on its next pass anyway. Closing unmaps nothing either way, so a drain still reading
    /// the queue or the peer's segment keeps reading valid memory until it lets go.
    pub fn close(&self) {
        self.own.send_queue.close();
        if let Ok(recv_queue) = self.peer.recv_queue.try_lock() {
            recv_queue.close();
        }
        self.own.dynshm.close_all();
        self.peer.dynshm.close_all();
    }
}

/// The queue this side pushes into and the segment it allocates outgoing payloads from.
#[derive(Debug)]
pub struct OwnEndpoint {
    pub send_queue: SendQueue,
    pub dynshm: Arc<LocalManager>,
}

/// The queue this side consumes from and the segment it reads incoming payloads out of.
#[derive(Debug)]
pub struct PeerEndpoint {
    /// The ring has a single consumer, so the lock is what picks the one task popping from it
    /// at any moment. A drain that needs to be a barrier against another event holds it across
    /// both, so no other drain can land in between and reorder them.
    pub recv_queue: Mutex<RecvQueue>,
    pub dynshm: Arc<RemoteManager>,
}

#[cfg(test)]
mod tests {
    use super::{ADVERTISED_DYNSHM_SIZE, extract_v5_shm_metadata, generate_v5_shm_metadata};
    use crate::{
        consts::HEADER_SIZE,
        protocol::{event::EventType, header::Header, negotiation::DEFAULT_MSG_VERSION},
    };

    const PATH: &str = "/dev/shm/shmipc_queue";

    #[test]
    fn metadata_round_trips() {
        let mut event = generate_v5_shm_metadata(PATH, DEFAULT_MSG_VERSION).unwrap();
        let h = Header(event.as_mut_ptr());

        assert_eq!(event.len() as u32, h.length());
        assert_eq!(DEFAULT_MSG_VERSION, h.version());
        assert_eq!(EventType::TYPE_SHARE_MEMORY_BY_MEMFD, h.msg_type());
        assert_eq!(
            (PATH, ADVERTISED_DYNSHM_SIZE),
            extract_v5_shm_metadata(&event[HEADER_SIZE..]).unwrap()
        );
    }

    #[test]
    fn metadata_body_is_a_length_prefixed_path_and_a_size() {
        let event = generate_v5_shm_metadata(PATH, DEFAULT_MSG_VERSION).unwrap();
        let body = &event[HEADER_SIZE..];

        assert_eq!(2 + PATH.len() + 4, body.len());
        assert_eq!((PATH.len() as u16).to_be_bytes(), body[..2]);
        assert_eq!(PATH.as_bytes(), &body[2..2 + PATH.len()]);
        assert_eq!([0, 0, 0, 0], body[2 + PATH.len()..]);
    }

    #[test]
    fn generating_rejects_a_path_the_peer_would_reject() {
        assert!(
            generate_v5_shm_metadata("", DEFAULT_MSG_VERSION)
                .unwrap_err()
                .to_string()
                .contains("non-empty")
        );
        assert!(
            generate_v5_shm_metadata(&"p".repeat(u16::MAX as usize + 1), DEFAULT_MSG_VERSION)
                .unwrap_err()
                .to_string()
                .contains("too long")
        );
    }

    #[test]
    fn extracting_rejects_a_body_no_sender_could_have_written() {
        // Shorter than the two fields that are always there.
        assert!(extract_v5_shm_metadata(&[]).is_err());
        assert!(extract_v5_shm_metadata(&[0, 1, 0, 0, 0]).is_err());
        // A path length that runs past the body, and one that stops short of the size.
        assert!(extract_v5_shm_metadata(&[0, 8, b'q', 0, 0, 0, 0]).is_err());
        assert!(extract_v5_shm_metadata(&[0, 1, b'q', 0, 0, 0, 0, 0]).is_err());
        // An empty path, which the peer rejects too.
        assert!(extract_v5_shm_metadata(&[0, 0, 0, 0, 0, 0]).is_err());
        // A path that is not text.
        assert!(extract_v5_shm_metadata(&[0, 1, 0xFF, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn extracting_takes_the_size_the_sender_advertised() {
        let body = [0, 1, b'q', 0, 0, 0x10, 0];

        assert_eq!(("q", 4096), extract_v5_shm_metadata(&body).unwrap());
    }
}

/// The handshake itself, which needs the memfds only Linux has.
#[cfg(all(test, target_os = "linux"))]
mod handshake_tests {
    use std::{
        hint,
        os::{fd::AsRawFd, unix::net::UnixStream},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };

    use super::{Client, Server, V5Initialized};
    use crate::{
        config::Config,
        consts::DYNSHM_TOTAL_SIZE,
        protocol::{
            event_fd::{drain_eventfd, write_eventfd},
            initializer::ProtocolInitialized,
            negotiation::{Negotiated, NegotiatedFeature, PROTO_VERSION_V5},
        },
    };

    /// Runs both halves of the exchange over a socket pair, the server on its own thread because
    /// each side blocks waiting on the other.
    fn handshake(feature: NegotiatedFeature) -> (ProtocolInitialized, ProtocolInitialized) {
        let (client_conn, server_conn) = UnixStream::pair().unwrap();
        let negotiated = Negotiated::new(feature);

        let server = thread::spawn(move || {
            let server = Server {
                conn_fd: server_conn.as_raw_fd(),
                config: Config::default(),
                negotiated,
            };
            let initialized = server.init();
            // The connection has to outlive the handshake that runs over it.
            drop(server_conn);
            initialized
        });

        let client = Client {
            conn_fd: client_conn.as_raw_fd(),
            config: Config::default(),
            negotiated,
        };
        let client_initialized = client.init().unwrap();
        let server_initialized = server.join().unwrap().unwrap();

        (client_initialized, server_initialized)
    }

    #[test]
    fn each_side_maps_what_the_other_created() {
        let (client, server) = handshake(NegotiatedFeature::None);

        assert_eq!(PROTO_VERSION_V5, client.proto_version);
        assert_eq!(PROTO_VERSION_V5, server.proto_version);

        let client_v5 = match client.shared_memory {
            crate::protocol::initializer::SharedMemoryMode::PerEndpointPool(Some(v5)) => v5,
            _ => panic!("v5 client did not initialize per-endpoint shared memory"),
        };
        let server_v5 = match server.shared_memory {
            crate::protocol::initializer::SharedMemoryMode::PerEndpointPool(Some(v5)) => v5,
            _ => panic!("v5 server did not initialize per-endpoint shared memory"),
        };

        // What one side sends through is what the other reads out of.
        assert_eq!(
            client_v5.own.send_queue.size(),
            server_v5.peer.recv_queue.blocking_lock().size()
        );
        assert_eq!(
            server_v5.own.send_queue.size(),
            client_v5.peer.recv_queue.blocking_lock().size()
        );
        assert_eq!(DYNSHM_TOTAL_SIZE, client_v5.peer.dynshm.size());
        assert_eq!(DYNSHM_TOTAL_SIZE, server_v5.peer.dynshm.size());

        assert!(client.eventfd.is_none());
        assert!(server.eventfd.is_none());
    }

    /// Each side creates the descriptor it waits on and gives it away, so a signal sent by one
    /// end has to land on the other end and nowhere else.
    #[test]
    fn a_wakeup_crosses_to_the_peer_that_waits_on_it() {
        let (client, server) = handshake(NegotiatedFeature::EventFd);

        let client_eventfd = client.eventfd.unwrap();
        let server_eventfd = server.eventfd.unwrap();

        write_eventfd(client_eventfd.wakeup_send.as_raw_fd()).unwrap();
        assert!(drain_eventfd(server_eventfd.wakeup_recv.as_raw_fd()).unwrap());
        assert!(!drain_eventfd(client_eventfd.wakeup_recv.as_raw_fd()).unwrap());

        write_eventfd(server_eventfd.wakeup_send.as_raw_fd()).unwrap();
        assert!(drain_eventfd(client_eventfd.wakeup_recv.as_raw_fd()).unwrap());
        assert!(!drain_eventfd(server_eventfd.wakeup_recv.as_raw_fd()).unwrap());
    }

    /// A peer that hangs up mid-handshake leaves an error, not a half-built session.
    #[test]
    fn a_peer_that_disappears_fails_the_handshake() {
        let (client_conn, server_conn) = UnixStream::pair().unwrap();
        drop(server_conn);

        let client = Client {
            conn_fd: client_conn.as_raw_fd(),
            config: Config::default(),
            negotiated: Negotiated::new(NegotiatedFeature::None),
        };

        assert!(client.init().is_err());
    }

    /// Closes while the drain holds the receive queue. `close` skips the locked queue and the drain
    /// stops on the session shutdown flag instead.
    #[tokio::test]
    async fn close_while_a_drain_holds_the_receive_queue() {
        let (mut client, mut server) = handshake(NegotiatedFeature::None);

        let _server = server.shared_memory.take_v5().expect("v5 server endpoints");
        let v5: Arc<V5Initialized> =
            Arc::from(client.shared_memory.take_v5().expect("v5 client endpoints"));

        let shutdown = Arc::new(AtomicBool::new(false));
        let holding = Arc::new(tokio::sync::Notify::new());

        let drain = tokio::spawn({
            let v5 = Arc::clone(&v5);
            let shutdown = Arc::clone(&shutdown);
            let holding = Arc::clone(&holding);
            async move {
                let mut queue = v5.peer.recv_queue.lock().await;
                holding.notify_one();

                while !shutdown.load(Ordering::SeqCst) {
                    let _ = queue.try_pop();
                    tokio::task::yield_now().await;
                }

                // Closing refuses further releases but leaves the mapping in place.
                let data = v5.peer.dynshm.data();
                assert_eq!(v5.peer.dynshm.size(), data.len());
                hint::black_box(data[data.len() - 1]);

                // Reads and frees are refused rather than faulting.
                assert!(v5.peer.dynshm.is_closed());
                assert!(v5.peer.dynshm.read(0).is_none());
                assert!(v5.peer.dynshm.free_chain(0).is_err());
            }
        });

        // The drain owns the queue from here on, so the close below is forced down the branch
        // where its `try_lock` fails.
        holding.notified().await;
        shutdown.store(true, Ordering::SeqCst);

        let started = std::time::Instant::now();
        v5.close();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "close blocked for {elapsed:?}, which means it waited on something"
        );

        assert!(v5.own.send_queue.is_closed());
        assert!(v5.own.dynshm.is_closed());
        assert!(v5.peer.dynshm.is_closed());

        drain.await.unwrap();

        // The receive queue is the one thing the close skipped, so it is still open with nobody
        // consuming it. Closing again now that it is free does shut it.
        assert!(!v5.peer.recv_queue.lock().await.is_closed());
        v5.close();
        assert!(v5.peer.recv_queue.lock().await.is_closed());
    }

    /// Runs the close and drain concurrently, allowing either to win the receive-queue lock.
    /// Both must finish without waiting on each other.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_and_drain_racing_both_finish() {
        let (mut client, mut server) = handshake(NegotiatedFeature::None);
        let _server = server.shared_memory.take_v5().expect("v5 server endpoints");
        let v5: Arc<V5Initialized> =
            Arc::from(client.shared_memory.take_v5().expect("v5 client endpoints"));

        let shutdown = Arc::new(AtomicBool::new(false));

        let drain = tokio::spawn({
            let v5 = Arc::clone(&v5);
            let shutdown = Arc::clone(&shutdown);
            async move {
                let mut queue = v5.peer.recv_queue.lock().await;
                while !shutdown.load(Ordering::SeqCst) {
                    let _ = queue.try_pop();
                    tokio::task::yield_now().await;
                }
                // Whether or not the close won the queue, the segment behind it is still mapped.
                let data = v5.peer.dynshm.data();
                hint::black_box(data[data.len() - 1]);
            }
        });

        let closer = tokio::spawn({
            let v5 = Arc::clone(&v5);
            let shutdown = Arc::clone(&shutdown);
            async move {
                shutdown.store(true, Ordering::SeqCst);
                v5.close();
            }
        });

        tokio::time::timeout(Duration::from_secs(10), async {
            closer.await.unwrap();
            drain.await.unwrap();
        })
        .await
        .expect("close and drain did not both finish");

        assert!(v5.own.send_queue.is_closed());
        assert!(v5.own.dynshm.is_closed());
        assert!(v5.peer.dynshm.is_closed());

        v5.close();
        assert!(v5.peer.recv_queue.lock().await.is_closed());
    }
}
