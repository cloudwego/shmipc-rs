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

use std::sync::atomic::AtomicU64;

#[derive(Debug, Default)]
pub struct Stats {
    #[allow(dead_code)]
    pub alloc_shm_error_count: AtomicU64,
    pub fallback_write_count: AtomicU64,
    pub fallback_read_count: AtomicU64,
    pub event_conn_error_count: AtomicU64,
    pub queue_full_error_count: AtomicU64,
    pub recv_polling_event_count: AtomicU64,
    pub send_polling_event_count: AtomicU64,
    pub out_flow_bytes: AtomicU64,
    pub in_flow_bytes: AtomicU64,
}
