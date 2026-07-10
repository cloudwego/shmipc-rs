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
    future::Future,
    io,
    os::unix::net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use shmipc::{
    Error, Listener,
    buffer::{BufferReader, BufferSlice, LinkedBuffer},
    config::SizePercentPair,
    consts::MemMapType,
    session::{SessionManager, SessionManagerConfig},
    stream::Stream,
    transport::{DefaultUnixConnect, DefaultUnixListen, TransportConnect},
};
use tokio::net::UnixStream;

#[tokio::test(flavor = "multi_thread")]
async fn test_ping_pong_by_shmipc() {
    let rand = rand::random::<u64>();
    let path = format!("/tmp/shmipc{}.sock", rand);
    let mut sm_config = benchmark_config();
    let size = 1 << 20;

    sm_config.config_mut().buffer_slice_sizes = vec![
        SizePercentPair {
            size: size + 256,
            percent: 70,
        },
        SizePercentPair {
            size: (16 << 10) + 256,
            percent: 20,
        },
        SizePercentPair {
            size: (64 << 10) + 256,
            percent: 10,
        },
    ];
    sm_config
        .config_mut()
        .share_memory_path_prefix
        .push_str(rand.to_string().as_str());
    sm_config = sm_config.with_session_num(1);
    let mut server = Listener::new(
        DefaultUnixListen,
        SocketAddr::from_pathname(path.clone()).unwrap(),
        sm_config.config().clone(),
    )
    .await
    .unwrap();
    let start = Instant::now();

    tokio_scoped::scope(|s| {
        s.spawn(async move {
            let mut stream = server.accept().await.unwrap();
            must_read(&mut stream, size).await;
            stream.recv_buf().release_previous_read();
            must_write(&mut stream, size).await;
        });
        s.spawn(async move {
            let client = SessionManager::new(
                sm_config,
                DefaultUnixConnect,
                SocketAddr::from_pathname(path).unwrap(),
            )
            .await
            .unwrap();
            let mut stream = client.get_stream().unwrap();
            must_write(&mut stream, size).await;
            must_read(&mut stream, size).await;
            stream.release_read_and_reuse();
            stream.close().await.unwrap();
        });
    });

    let elapsed: Duration = start.elapsed();
    println!("elapsed: {:?}", elapsed);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fallback_data_before_stream_close() {
    let rand = rand::random::<u64>();
    let path = format!("/tmp/shmipc{}.sock", rand);
    let size = 2 << 20;
    let mut sm_config = benchmark_config();

    sm_config.config_mut().share_memory_buffer_cap = 1 << 20;
    sm_config.config_mut().buffer_slice_sizes = vec![SizePercentPair {
        size: 4096,
        percent: 100,
    }];
    sm_config
        .config_mut()
        .share_memory_path_prefix
        .push_str(rand.to_string().as_str());
    sm_config = sm_config.with_session_num(1);

    let mut server = Listener::new(
        DefaultUnixListen,
        SocketAddr::from_pathname(path.clone()).unwrap(),
        sm_config.config().clone(),
    )
    .await
    .unwrap();

    tokio_scoped::scope(|s| {
        s.spawn(async move {
            let mut stream = server.accept().await.unwrap();
            assert!(must_read(&mut stream, size).await);
            assert!(!must_read(&mut stream, 1).await);
        });
        s.spawn(async move {
            let client = SessionManager::new(
                sm_config,
                DefaultUnixConnect,
                SocketAddr::from_pathname(path).unwrap(),
            )
            .await
            .unwrap();
            let mut stream = client.get_stream().unwrap();
            must_write(&mut stream, size).await;
            assert!(stream.fallback_state());
            stream.close().await.unwrap();
        });
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn test_v4_default_ping_pong_by_shmipc() {
    let sm_config = benchmark_config().with_config(benchmark_config().config().clone().with_v4());
    run_ping_pong(sm_config, 64 << 10).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_v4_event_queue_polling_ping_pong_by_shmipc() {
    let sm_config = benchmark_config().with_config(
        benchmark_config()
            .config()
            .clone()
            .with_v4_event_queue_polling(Duration::from_micros(100)),
    );
    run_ping_pong(sm_config, 64 << 10).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_v4_eventfd_ping_pong_by_shmipc() {
    let sm_config =
        benchmark_config().with_config(benchmark_config().config().clone().with_v4_eventfd());
    run_ping_pong(sm_config, 64 << 10).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_v4_polling_multiple_streams_by_shmipc() {
    let sm_config = benchmark_config().with_config(
        benchmark_config()
            .config()
            .clone()
            .with_v4_event_queue_polling(Duration::from_micros(100)),
    );
    run_multi_stream_ping_pong(sm_config, 8, 8 << 10).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_v4_polling_close_stops_idle_tasks() {
    let sm_config = benchmark_config().with_config(
        benchmark_config()
            .config()
            .clone()
            .with_v4_event_queue_polling(Duration::from_micros(100)),
    );
    run_idle_close(sm_config).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_v4_eventfd_close_stops_idle_tasks() {
    let sm_config =
        benchmark_config().with_config(benchmark_config().config().clone().with_v4_eventfd());
    run_idle_close(sm_config).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_v4_fallback_recreates_devshm_resources() {
    let rand = rand::random::<u64>();
    let sock = PathBuf::from(format!("/tmp/shmipc-fallback-{}.sock", rand));
    let base_prefix = format!("/tmp/shmipc-fallback-{}", rand);
    let effective_prefix = format!("{}_{}", base_prefix, std::process::id());
    let buffer_path = format!("{}_buffer", effective_prefix);
    let queue_path = format!("{}_queue_0", effective_prefix);
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&buffer_path);
    let _ = std::fs::remove_file(&queue_path);

    let mut config = benchmark_config().config().clone().with_v4();
    config.mem_map_type = MemMapType::MemMapTypeDevShmFile;
    config.share_memory_path_prefix = base_prefix;
    config.share_memory_buffer_cap = 1 << 20;
    config.queue_cap = 1024;
    config.buffer_slice_sizes = vec![SizePercentPair {
        size: 4096,
        percent: 100,
    }];
    let sm_config = SessionManagerConfig::new()
        .with_config(config.clone())
        .with_session_num(1);

    let mut server = Listener::new(
        DefaultUnixListen,
        SocketAddr::from_pathname(&sock).unwrap(),
        config,
    )
    .await
    .unwrap();
    let (server_done_tx, server_done_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(async move {
        let mut stream = server.accept().await.unwrap();
        must_read(&mut stream, 4096).await;
        stream.recv_buf().release_previous_read();
        must_write(&mut stream, 4096).await;
        stream.release_read_and_reuse();
        let _ = server_done_rx.await;
        stream.close().await.unwrap();
        drop(stream);
        server.close().await;
    });

    let attempts = Arc::new(AtomicUsize::new(0));
    let connect = FallbackOnceConnect {
        sock: sock.clone(),
        attempts: attempts.clone(),
        buffer_path: PathBuf::from(&buffer_path),
        queue_path: PathBuf::from(&queue_path),
    };
    let client = SessionManager::new(sm_config, connect, ()).await.unwrap();
    assert_eq!(2, attempts.load(Ordering::SeqCst));

    let mut stream = client.get_stream().unwrap();
    must_write(&mut stream, 4096).await;
    must_read(&mut stream, 4096).await;
    stream.release_read_and_reuse();
    stream.close().await.unwrap();
    drop(stream);
    client.close().await;
    drop(client);
    let _ = server_done_tx.send(());
    server_task.await.unwrap();

    assert!(!std::path::Path::new(&queue_path).exists());
    let _ = std::fs::remove_file(&buffer_path);
    let _ = std::fs::remove_file(&sock);
}

async fn run_ping_pong(mut sm_config: SessionManagerConfig, size: u32) {
    let rand = rand::random::<u64>();
    let path = format!("/tmp/shmipc{}.sock", rand);

    sm_config
        .config_mut()
        .share_memory_path_prefix
        .push_str(rand.to_string().as_str());
    sm_config = sm_config.with_session_num(1);

    let mut server = Listener::new(
        DefaultUnixListen,
        SocketAddr::from_pathname(path.clone()).unwrap(),
        sm_config.config().clone(),
    )
    .await
    .unwrap();

    tokio_scoped::scope(|s| {
        s.spawn(async move {
            let mut stream = server.accept().await.unwrap();
            must_read(&mut stream, size).await;
            stream.recv_buf().release_previous_read();
            must_write(&mut stream, size).await;
        });
        s.spawn(async move {
            let client = SessionManager::new(
                sm_config,
                DefaultUnixConnect,
                SocketAddr::from_pathname(path).unwrap(),
            )
            .await
            .unwrap();
            let mut stream = client.get_stream().unwrap();
            must_write(&mut stream, size).await;
            must_read(&mut stream, size).await;
            stream.release_read_and_reuse();
            stream.close().await.unwrap();
            client.close().await;
        });
    });
}

async fn run_idle_close(mut sm_config: SessionManagerConfig) {
    let rand = rand::random::<u64>();
    let path = format!("/tmp/shmipc-idle-close-{}.sock", rand);
    sm_config
        .config_mut()
        .share_memory_path_prefix
        .push_str(rand.to_string().as_str());
    sm_config = sm_config.with_session_num(1);

    let mut server = Listener::new(
        DefaultUnixListen,
        SocketAddr::from_pathname(path.clone()).unwrap(),
        sm_config.config().clone(),
    )
    .await
    .unwrap();
    let client = SessionManager::new(
        sm_config,
        DefaultUnixConnect,
        SocketAddr::from_pathname(path).unwrap(),
    )
    .await
    .unwrap();

    let mut client_stream = client.get_stream().unwrap();
    must_write(&mut client_stream, 4096).await;
    let mut server_stream = within("server accept idle close", server.accept())
        .await
        .unwrap();
    assert!(must_read(&mut server_stream, 4096).await);
    server_stream.recv_buf().release_previous_read();

    within("client stream close", client_stream.close())
        .await
        .unwrap();
    within("client session close", client.close()).await;
    within("server stream close", server_stream.close())
        .await
        .unwrap();
    within("listener close", server.close()).await;
}

async fn run_multi_stream_ping_pong(
    mut sm_config: SessionManagerConfig,
    stream_count: usize,
    size: u32,
) {
    let rand = rand::random::<u64>();
    let path = format!("/tmp/shmipc-multi-{}.sock", rand);
    sm_config
        .config_mut()
        .share_memory_path_prefix
        .push_str(rand.to_string().as_str());
    sm_config = sm_config.with_session_num(1);

    let mut server = Listener::new(
        DefaultUnixListen,
        SocketAddr::from_pathname(path.clone()).unwrap(),
        sm_config.config().clone(),
    )
    .await
    .unwrap();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        let mut handlers = Vec::with_capacity(stream_count);
        for _ in 0..stream_count {
            let mut stream = within("server accept multi stream", server.accept())
                .await
                .unwrap();
            handlers.push(tokio::spawn(async move {
                assert!(must_read(&mut stream, size).await);
                stream.recv_buf().release_previous_read();
                must_write(&mut stream, size).await;
                stream.release_read_and_reuse();
            }));
        }
        for handler in handlers {
            handler.await.unwrap();
        }
        let _ = done_rx.await;
        server.close().await;
    });

    let client = SessionManager::new(
        sm_config,
        DefaultUnixConnect,
        SocketAddr::from_pathname(path).unwrap(),
    )
    .await
    .unwrap();
    let mut handlers = Vec::with_capacity(stream_count);
    for _ in 0..stream_count {
        let client = client.clone();
        handlers.push(tokio::spawn(async move {
            let mut stream = client.get_stream().unwrap();
            must_write(&mut stream, size).await;
            assert!(must_read(&mut stream, size).await);
            stream.release_read_and_reuse();
            stream.close().await.unwrap();
        }));
    }
    for handler in handlers {
        handler.await.unwrap();
    }
    let _ = done_tx.send(());
    client.close().await;
    server_task.await.unwrap();
}

async fn within<T, F>(description: &'static str, future: F) -> T
where
    F: Future<Output = T>,
{
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .unwrap_or_else(|_| panic!("timed out: {description}"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "stress test"]
async fn stress_v4_repeated_connect_close() {
    for _ in 0..20 {
        run_idle_close(
            benchmark_config().with_config(benchmark_config().config().clone().with_v4()),
        )
        .await;
        run_idle_close(
            benchmark_config().with_config(
                benchmark_config()
                    .config()
                    .clone()
                    .with_v4_event_queue_polling(Duration::from_micros(100)),
            ),
        )
        .await;
        run_idle_close(
            benchmark_config().with_config(benchmark_config().config().clone().with_v4_eventfd()),
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "stress test"]
async fn stress_v4_many_streams() {
    run_multi_stream_ping_pong(
        benchmark_config().with_config(benchmark_config().config().clone().with_v4()),
        64,
        4096,
    )
    .await;
    run_multi_stream_ping_pong(
        benchmark_config().with_config(
            benchmark_config()
                .config()
                .clone()
                .with_v4_event_queue_polling(Duration::from_micros(100)),
        ),
        64,
        4096,
    )
    .await;
    run_multi_stream_ping_pong(
        benchmark_config().with_config(benchmark_config().config().clone().with_v4_eventfd()),
        64,
        4096,
    )
    .await;
}

#[derive(Clone, Debug)]
struct FallbackOnceConnect {
    sock: PathBuf,
    attempts: Arc<AtomicUsize>,
    buffer_path: PathBuf,
    queue_path: PathBuf,
}

impl TransportConnect for FallbackOnceConnect {
    type Stream = UnixStream;
    type Address = ();

    async fn connect(&self, _addr: Self::Address) -> io::Result<Self::Stream> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            let (client, peer) = UnixStream::pair()?;
            drop(peer);
            return Ok(client);
        }
        assert!(!self.queue_path.exists());
        assert!(!self.buffer_path.exists());
        UnixStream::connect(&self.sock).await
    }
}

fn benchmark_config() -> SessionManagerConfig {
    let mut c = SessionManagerConfig::new();
    c.config_mut().queue_cap = 65536;
    c.config_mut().connection_write_timeout = std::time::Duration::from_secs(1);
    c.config_mut().share_memory_buffer_cap = 256 << 20;
    c.config_mut().mem_map_type = MemMapType::MemMapTypeMemFd;
    c
}

async fn must_write(s: &mut Stream, size: u32) {
    write_empty_buffer(s.send_buf(), size);
    loop {
        match s.flush(false).await {
            Err(e) => match e {
                Error::QueueFull => {
                    tokio::time::sleep(std::time::Duration::from_micros(1)).await;
                    continue;
                }
                _ => {
                    panic!("must write err:{}", e);
                }
            },
            Ok(_) => {
                return;
            }
        }
    }
}

fn write_empty_buffer(l: &mut LinkedBuffer, size: u32) {
    if size == 0 {
        return;
    }
    let mut wrote = 0;
    loop {
        if l.slice_list().write_slice.is_none() {
            l.alloc(size - wrote);
            l.slice_list_mut().write_slice = l.slice_list().front_slice;
        }
        wrote += write_empty_slice(l.slice_list().write_mut().unwrap(), size - wrote);
        if wrote < size {
            if l.slice_list().write().unwrap().next().is_none() {
                l.alloc(size - wrote);
            }
            l.slice_list_mut().write_slice = l.slice_list().write().unwrap().next_slice;
        } else {
            break;
        }
    }
    *l.len_mut() += size as usize;
}

fn write_empty_slice(slice: &mut BufferSlice, size: u32) -> u32 {
    slice.write_index += size as usize;
    if slice.write_index > slice.cap as usize {
        let wrote = slice.cap - (slice.write_index as u32 - size);
        slice.write_index = slice.cap as usize;
        return wrote;
    }
    size
}

async fn must_read(s: &mut Stream, size: u32) -> bool {
    match s.discard(size as usize).await {
        Err(e) => match e {
            Error::StreamClosed | Error::EndOfStream => false,
            _ => {
                panic!("must read err:{}", e);
            }
        },
        Ok(_) => true,
    }
}
