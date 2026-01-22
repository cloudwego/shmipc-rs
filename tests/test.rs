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
    os::unix::net::SocketAddr,
    time::{Duration, Instant},
};

use shmipc::{
    AsyncReadShm, AsyncWriteShm, BufferReader, BufferSlice, Error, LinkedBuffer, Listener,
    SessionManager, SessionManagerConfig, Stream,
    config::SizePercentPair,
    consts::MemMapType,
    transport::{DefaultUnixConnect, DefaultUnixListen},
};

#[tokio::test(flavor = "multi_thread")]
async fn test_ping_pong_by_shmipc() {
    let rand = rand::random::<u64>();
    let path = format!("/dev/shm/shmipc{}.sock", rand);
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
            let mut stream = server.accept().await.unwrap().unwrap();
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
