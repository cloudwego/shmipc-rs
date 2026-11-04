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
    Listener, config::Config, stats::DynShmStats, stream::Stream, transport::DefaultUnixListen,
};

fn used_bytes(stats: &DynShmStats) -> usize {
    stats.shared_bytes.saturating_sub(stats.shared_free_bytes) + stats.dedicated_used
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let dir = std::env::current_dir().unwrap();
    let binding = dir.join("../ipc_test.sock");
    let uds_path = binding.to_str().unwrap().to_owned();
    _ = unlink(uds_path.as_str());

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
    // Each message is `[u32 length][u32 chunk][payload]`. The chunk size is echoed back as the
    // size of each `write_bytes` call.
    let mut round = 0usize;
    let mut live = stream.v5_dynshm_stats();

    loop {
        let header = match stream.read_exact_bytes(8).await {
            Ok(header) => header,
            Err(e) => {
                println!("server stream ended after {round} rounds: {e}");
                break;
            }
        };
        let size = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let chunk = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        drop(header);

        let req_msg = match stream.read_exact_bytes(size).await {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("failed to read body of {size} bytes, err: {e}");
                break;
            }
        };
        round += 1;

        let echo = req_msg.to_vec();
        drop(req_msg);

        stream
            .write_bytes(&(echo.len() as u32).to_le_bytes())
            .unwrap();
        stream.write_bytes(&(chunk as u32).to_le_bytes()).unwrap();
        for part in echo.chunks(chunk.max(1)) {
            stream.write_bytes(part).unwrap();
        }
        stream.flush(true).await.unwrap();

        if let Some(stats) = stream.v5_dynshm_stats() {
            if round.is_multiple_of(10) {
                println!(
                    "server round {round:>3} | echoed {:>7} B in {:>6} B writes | in use {:>9} B \
                     | shared active {} | dedicated {} regions, {} active",
                    echo.len(),
                    chunk,
                    used_bytes(&stats),
                    stats.shared_active,
                    stats.dedicated_regions,
                    stats.dedicated_active,
                );
            }
            live = Some(stats);
        }
    }

    if let Some(stats) = live {
        println!(
            "server total: {round} rounds | at the last frame {} B in use, dedicated {} regions",
            used_bytes(&stats),
            stats.dedicated_regions,
        );
    }

    let _ = stream.close().await;
}
