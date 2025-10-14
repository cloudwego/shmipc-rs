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

use std::os::unix::net::SocketAddr;

use criterion::{Criterion, criterion_group, criterion_main};
use shmipc::{
    AsyncReadShm, AsyncWriteShm, BufferReader, BufferSlice, Error, LinkedBuffer, Listener,
    SessionManager, SessionManagerConfig, Stream,
    config::SizePercentPair,
    consts::MemMapType,
    transport::{DefaultUnixConnect, DefaultUnixListen},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::oneshot,
    time::Instant,
};

fn criterion_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("shmipc");
    let sizes = [
        64,
        512,
        1024,
        4096,
        16 << 10,
        32 << 10,
        64 << 10,
        256 << 10,
        512 << 10,
        1 << 20,
        4 << 20,
    ];
    for size in sizes {
        group.bench_function(
            format!("benchmark_parallel_ping_pong_by_shmipc_{}b", size),
            |b| {
                b.to_async(
                    tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .unwrap(),
                )
                .iter_custom(|iters| async move {
                    let rand = rand::random::<u64>();
                    let path = format!("/dev/shm/shmipc{}.sock", rand);
                    let mut sm_config = benchmark_config();
                    if size >= 4 << 20 {
                        sm_config.config_mut().share_memory_buffer_cap = 1 << 30;
                    }
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
                    let (tx, rx) = oneshot::channel();
                    let (tx2, mut rx2) = oneshot::channel();
                    tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                r = server.accept() => {
                                    let mut stream = r.unwrap().unwrap();
                                    tokio::spawn(async move {
                                        loop {
                                            if !must_read(&mut stream, size).await {
                                                return;
                                            }
                                            stream.recv_buf().release_previous_read();
                                            must_write(&mut stream, size).await;
                                        }
                                    });
                                }
                                _ = &mut rx2 => {
                                    server.close().await;
                                    tx.send(()).unwrap();
                                    return;
                                }

                            }
                        }
                    });

                    let client = SessionManager::new(
                        sm_config,
                        DefaultUnixConnect,
                        SocketAddr::from_pathname(path).unwrap(),
                    )
                    .await
                    .unwrap();
                    let mut handlers = Vec::with_capacity(200);
                    let start = Instant::now();
                    for _ in 0..199 {
                        let client = client.clone();
                        handlers.push(tokio::spawn(async move {
                            let mut stream = client.get_stream().unwrap();
                            for _ in 0..iters / 200 {
                                must_write(&mut stream, size).await;
                                must_read(&mut stream, size).await;
                                stream.release_read_and_reuse();
                            }
                            stream.close().await.unwrap();
                        }));
                    }
                    for handler in handlers {
                        handler.await.unwrap();
                    }
                    let cost = start.elapsed();
                    client.close().await;
                    tx2.send(()).unwrap();
                    rx.await.unwrap();
                    cost
                })
            },
        );

        group.bench_function(
            format!("benchmark_parallel_ping_pong_by_uds_{}b", size),
            |b| {
                b.to_async(
                    tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .unwrap(),
                )
                .iter_custom(|iters| async move {
                    let rand = rand::random::<u64>();
                    let (tx, rx) = oneshot::channel();
                    tokio::spawn(async move {
                        let listener =
                            UnixListener::bind(format!("/dev/shm/uds{}.sock", rand)).unwrap();
                        _ = tx.send(());
                        loop {
                            let (mut conn, _) = listener.accept().await.unwrap();
                            tokio::spawn(async move {
                                let mut read_buf = vec![0u8; size as usize];
                                let write_buf = vec![0u8; size as usize];
                                loop {
                                    if let Err(err) = conn.read_exact(&mut read_buf).await {
                                        if err.kind() == std::io::ErrorKind::UnexpectedEof {
                                            return;
                                        }
                                        panic!("read err: {}", err);
                                    }
                                    conn.write_all(&write_buf).await.unwrap();
                                    conn.flush().await.unwrap();
                                }
                            });
                        }
                    });
                    _ = rx.await;
                    let mut handlers = Vec::with_capacity(200);
                    let start = Instant::now();
                    for _ in 0..199 {
                        handlers.push(tokio::spawn(async move {
                            let mut stream =
                                UnixStream::connect(format!("/dev/shm/uds{}.sock", rand))
                                    .await
                                    .unwrap();
                            let mut read_buf = vec![0u8; size as usize];
                            let write_buf = vec![0u8; size as usize];
                            for _ in 0..iters / 200 {
                                stream.write_all(&write_buf).await.unwrap();
                                stream.flush().await.unwrap();
                                stream.read_exact(&mut read_buf).await.unwrap();
                            }
                        }));
                    }
                    for handler in handlers {
                        handler.await.unwrap();
                    }
                    start.elapsed()
                })
            },
        );
    }
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);

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
