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

use std::sync::atomic::{AtomicU64, Ordering};

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub alloc_shm_error_count: u64,
    pub fallback_write_count: u64,
    pub fallback_read_count: u64,
    pub event_conn_error_count: u64,
    pub queue_full_error_count: u64,
    pub recv_polling_event_count: u64,
    pub send_polling_event_count: u64,
    pub out_flow_bytes: u64,
    pub in_flow_bytes: u64,
}

impl Stats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            alloc_shm_error_count: self.alloc_shm_error_count.load(Ordering::Relaxed),
            fallback_write_count: self.fallback_write_count.load(Ordering::Relaxed),
            fallback_read_count: self.fallback_read_count.load(Ordering::Relaxed),
            event_conn_error_count: self.event_conn_error_count.load(Ordering::Relaxed),
            queue_full_error_count: self.queue_full_error_count.load(Ordering::Relaxed),
            recv_polling_event_count: self.recv_polling_event_count.load(Ordering::Relaxed),
            send_polling_event_count: self.send_polling_event_count.load(Ordering::Relaxed),
            out_flow_bytes: self.out_flow_bytes.load(Ordering::Relaxed),
            in_flow_bytes: self.in_flow_bytes.load(Ordering::Relaxed),
        }
    }
}

impl StatsSnapshot {
    pub fn saturating_sub(self, rhs: Self) -> Self {
        Self {
            alloc_shm_error_count: self
                .alloc_shm_error_count
                .saturating_sub(rhs.alloc_shm_error_count),
            fallback_write_count: self
                .fallback_write_count
                .saturating_sub(rhs.fallback_write_count),
            fallback_read_count: self
                .fallback_read_count
                .saturating_sub(rhs.fallback_read_count),
            event_conn_error_count: self
                .event_conn_error_count
                .saturating_sub(rhs.event_conn_error_count),
            queue_full_error_count: self
                .queue_full_error_count
                .saturating_sub(rhs.queue_full_error_count),
            recv_polling_event_count: self
                .recv_polling_event_count
                .saturating_sub(rhs.recv_polling_event_count),
            send_polling_event_count: self
                .send_polling_event_count
                .saturating_sub(rhs.send_polling_event_count),
            out_flow_bytes: self.out_flow_bytes.saturating_sub(rhs.out_flow_bytes),
            in_flow_bytes: self.in_flow_bytes.saturating_sub(rhs.in_flow_bytes),
        }
    }

    pub fn add_assign(&mut self, rhs: Self) {
        self.alloc_shm_error_count = self
            .alloc_shm_error_count
            .saturating_add(rhs.alloc_shm_error_count);
        self.fallback_write_count = self
            .fallback_write_count
            .saturating_add(rhs.fallback_write_count);
        self.fallback_read_count = self
            .fallback_read_count
            .saturating_add(rhs.fallback_read_count);
        self.event_conn_error_count = self
            .event_conn_error_count
            .saturating_add(rhs.event_conn_error_count);
        self.queue_full_error_count = self
            .queue_full_error_count
            .saturating_add(rhs.queue_full_error_count);
        self.recv_polling_event_count = self
            .recv_polling_event_count
            .saturating_add(rhs.recv_polling_event_count);
        self.send_polling_event_count = self
            .send_polling_event_count
            .saturating_add(rhs.send_polling_event_count);
        self.out_flow_bytes = self.out_flow_bytes.saturating_add(rhs.out_flow_bytes);
        self.in_flow_bytes = self.in_flow_bytes.saturating_add(rhs.in_flow_bytes);
    }
}
