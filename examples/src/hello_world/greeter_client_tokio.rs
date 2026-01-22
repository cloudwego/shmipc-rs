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

use shmipc::{
    compact::StreamExt,
    session::{SessionManager, SessionManagerConfig},
    transport::DefaultUnixConnect,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let dir = std::env::current_dir().unwrap();
    let uds_path = SocketAddr::from_pathname(dir.join("../ipc_test.sock")).unwrap();

    let mut conf = SessionManagerConfig::new();
    conf.config_mut().mem_map_type = shmipc::consts::MemMapType::MemMapTypeMemFd;
    conf.config_mut().share_memory_path_prefix = "/dev/shm/client.ipc.shm".to_string();
    #[cfg(target_os = "macos")]
    {
        conf.config.share_memory_path_prefix = "/tmp/client.ipc.shm".to_string();
        conf.config.queue_path = "/tmp/client.ipc.shm_queue".to_string();
    }

    let sm = SessionManager::new(conf, DefaultUnixConnect, uds_path)
        .await
        .unwrap();
    let stream = sm.get_stream().unwrap();
    let request_msg = "client say hello world!!!";

    let mut conn = StreamExt::new(stream);

    println!(
        "size: {}",
        conn.write(request_msg.as_bytes()).await.unwrap(),
    );
    conn.flush().await.unwrap();

    const EXPECTED_RESP: &str = "server hello world!!!";
    let mut buf = vec![0; 4096];

    conn.read_exact(&mut buf[..EXPECTED_RESP.len()])
        .await
        .unwrap();
    println!(
        "client stream receive response {}",
        str::from_utf8(&buf[..EXPECTED_RESP.len()]).unwrap()
    );
    conn.inner().reuse().await;
    sm.close().await;
}
