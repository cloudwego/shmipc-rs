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
use shmipc::{Listener, config::Config, stream::Stream, transport::DefaultUnixListen};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let dir = std::env::current_dir().unwrap();
    let binding = dir.join("../ipc_test.sock");
    let uds_path = binding.to_str().unwrap();
    _ = unlink(uds_path);

    // A server using the default protocol config can accept legacy V2/V3 clients and V4 clients.
    // V4 optional wakeup features are selected from the client negotiation request.
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

async fn handle_stream(mut stream: Stream) {
    loop {
        let req_msg = match stream
            .read_exact_bytes("client say hello world!!!".len())
            .await
        {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("failed to read msg, err: {e}");
                break;
            }
        };
        println!(
            "server receive request {}",
            String::from_utf8(req_msg.to_vec()).unwrap()
        );

        let resp_msg = "server hello world!!!";
        stream.write_bytes(resp_msg.as_bytes()).unwrap();
        stream.flush(true).await.unwrap();
    }
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    stream.close().await.unwrap();
}
