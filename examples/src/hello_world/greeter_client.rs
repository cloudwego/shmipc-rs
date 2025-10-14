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
    AsyncReadShm, AsyncWriteShm, SessionManager, SessionManagerConfig,
    transport::DefaultUnixConnect,
};

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
    let mut stream = sm.get_stream().unwrap();
    let request_msg = "client say hello world!!!";
    println!(
        "size: {}",
        stream.write_bytes(request_msg.as_bytes()).unwrap()
    );
    stream.flush(true).await.unwrap();
    let resp_msg = stream
        .read_bytes("server hello world!!!".len())
        .await
        .unwrap();
    println!(
        "client stream receive response {}",
        String::from_utf8(resp_msg.to_vec()).unwrap()
    );
    sm.put_back(stream).await;
    sm.close().await;
}
