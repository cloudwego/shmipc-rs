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

use nix::unistd::unlink;
use shmipc::{
    Listener, compact::StreamExt, config::Config, stream::Stream, transport::DefaultUnixListen,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let dir = std::env::current_dir().unwrap();
    let binding = dir.join("../ipc_test.sock");
    let uds_path = binding.to_str().unwrap();
    _ = unlink(uds_path);

    let mut ln = Listener::new(
        DefaultUnixListen,
        SocketAddr::from_pathname(binding).unwrap(),
        Config::default(),
    )
    .await
    .unwrap();

    loop {
        let stream = match ln.accept().await {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("failed to accept conn, err: {e}");
                continue;
            }
        };
        tokio::spawn(handle_stream(stream));
    }
}

async fn handle_stream(stream: Stream) {
    const EXPECTED_REQ: &str = "client say hello world!!!";
    let mut buf = vec![0; 4096];
    let mut conn = StreamExt::new(stream);

    loop {
        match conn.read_exact(&mut buf[..EXPECTED_REQ.len()]).await {
            Ok(len) => println!("read {len}"),
            Err(e) => {
                eprintln!("failed to read msg, err: {e}");
                break;
            }
        }

        println!(
            "server receive request {}",
            str::from_utf8(&buf[..EXPECTED_REQ.len()]).unwrap()
        );

        let resp_msg = "server hello world!!!";
        conn.write_all(resp_msg.as_bytes()).await.unwrap();
        conn.flush().await.unwrap();
    }
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    conn.shutdown().await.unwrap();
}
