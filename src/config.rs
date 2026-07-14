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

use std::{fmt::Debug, time::Duration};

use anyhow::anyhow;

use crate::consts::{
    DEFAULT_BUFFER_SLICE_SIZES, DEFAULT_QUEUE_CAP, DEFAULT_QUEUE_PATH, DEFAULT_SHARE_MEMORY_CAP,
    MemMapType, SESSION_REBUILD_INTERVAL,
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SizePercentPair {
    #[serde(alias = "Size")]
    pub size: u32,
    #[serde(alias = "Percent")]
    pub percent: u32,
}

/// Config is used to tune the shmipc session
///
/// `Config::default()` preserves the legacy protocol behavior:
///
/// - [`MemMapType::MemMapTypeDevShmFile`] uses the V2 protocol.
/// - [`MemMapType::MemMapTypeMemFd`] uses the V3 protocol.
///
/// V4 is opt-in through [`Config::with_v4`], [`Config::with_v4_eventfd`], or
/// [`Config::with_v4_event_queue_polling`]. Server-side V4 feature selection is negotiated from
/// the client request, so a server using the default protocol config can accept legacy and V4
/// clients. If a server is configured with [`MemMapType::MemMapTypeDevShmFile`], V4 eventfd
/// requests are rejected because eventfd negotiation requires memfd fd-passing.
#[derive(Debug, Clone)]
pub struct Config {
    /// connection_write_timeout is meant to be a "safety value" timeout after
    /// which we will suspect a problem with the underlying connection and
    /// close it. This is only applied to writes, where there's generally
    /// an expectation that things will move along quickly.
    pub connection_write_timeout: Duration,

    pub connection_read_timeout: Option<Duration>,

    pub connection_timeout: Option<Duration>,

    /// initialize_timeout is meant timeout during server and client exchange config phase
    pub initialize_timeout: Duration,

    /// the max number of pending request
    pub queue_cap: u32,

    /// share memory path of the underlying queue
    pub queue_path: String,

    /// the capacity of buffer in share memory
    pub share_memory_buffer_cap: u32,

    /// the share memory path prefix of buffer
    pub share_memory_path_prefix: String,

    /// guess request or response's size for improving performance, and the default value is 4096
    pub buffer_slice_sizes: Vec<SizePercentPair>,

    /// mmap map type, MemMapTypeDevShmFile or MemMapTypeMemFd (server set)
    pub mem_map_type: MemMapType,

    /// client rebuild session interval
    pub rebuild_interval: Duration,

    pub max_stream_num: usize,

    /// Protocol and wakeup negotiation settings.
    ///
    /// Keep this as [`ProtocolConfig::default`] for legacy behavior. Set it explicitly only when
    /// the client should initiate V4 negotiation.
    pub protocol: ProtocolConfig,
}

/// Protocol-level configuration.
///
/// `mode` chooses legacy vs V4 negotiation. `wakeup` selects an optional V4 wakeup feature. Wakeup
/// features are only valid with [`ProtocolMode::V4`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProtocolConfig {
    /// Protocol negotiation mode.
    pub mode: ProtocolMode,
    /// Optional wakeup feature requested by the client during V4 negotiation.
    pub wakeup: WakeupMode,
}

/// Protocol negotiation mode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ProtocolMode {
    /// Preserve the pre-V4 behavior.
    ///
    /// File-path shared memory uses V2 and memfd shared memory uses V3.
    #[default]
    Legacy,
    /// Use Go-compatible V4 JSON negotiation.
    ///
    /// When `fallback` is `true`, a client reconnects and falls back to legacy V3 only if the V4
    /// initialization failed because of a network/protocol failure. An explicit V4 negotiation
    /// rejection, such as `VERSION_NOT_SUPPORTED`, is returned to the caller instead of being
    /// silently downgraded.
    V4 {
        /// Whether a client should reconnect and retry legacy V3 after network/protocol failures
        /// during initial V4 setup.
        fallback: bool,
    },
}

/// Optional V4 wakeup feature.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WakeupMode {
    /// Use the legacy Unix-socket polling message wakeup path.
    #[default]
    Default,
    /// Let the peer periodically drain the shared-memory queue instead of sending a wakeup per
    /// queue element.
    ///
    /// The interval is encoded into the V4 negotiation request and both sides use the negotiated
    /// interval. Lower intervals reduce latency but increase timer activity. The interval must be
    /// greater than zero.
    EventQueuePolling { interval: Duration },
    /// Use Linux `eventfd` for queue wakeups.
    ///
    /// This mode requires [`MemMapType::MemMapTypeMemFd`] because V4 eventfd negotiation passes
    /// four file descriptors: buffer fd, queue fd, and two eventfds.
    EventFd,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            connection_write_timeout: Duration::from_secs(10),
            connection_read_timeout: None,
            connection_timeout: None,
            initialize_timeout: Duration::from_millis(1000),
            queue_cap: DEFAULT_QUEUE_CAP,
            queue_path: DEFAULT_QUEUE_PATH.to_owned(),
            share_memory_buffer_cap: DEFAULT_SHARE_MEMORY_CAP,
            share_memory_path_prefix: "/dev/shm/shmipc".to_owned(),
            buffer_slice_sizes: DEFAULT_BUFFER_SLICE_SIZES.to_vec(),
            mem_map_type: Default::default(),
            rebuild_interval: SESSION_REBUILD_INTERVAL,
            max_stream_num: 4096,
            protocol: ProtocolConfig::default(),
        }
    }
}

impl Config {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable V4 negotiation with the default Unix-socket polling wakeup path.
    ///
    /// The helper sets [`ProtocolMode::V4`] with `fallback = true` and keeps
    /// [`WakeupMode::Default`].
    pub fn with_v4(mut self) -> Self {
        self.protocol.mode = ProtocolMode::V4 { fallback: true };
        self
    }

    /// Enable V4 negotiation and request Linux `eventfd` wakeups.
    ///
    /// This is the recommended V4 mode for low-latency small-message workloads when both peers use
    /// memfd shared memory.
    pub fn with_v4_eventfd(mut self) -> Self {
        self.protocol.mode = ProtocolMode::V4 { fallback: true };
        self.protocol.wakeup = WakeupMode::EventFd;
        self
    }

    /// Enable V4 negotiation and request periodic queue polling.
    ///
    /// Use this when reducing per-message wakeup traffic is more important than immediate
    /// delivery. The interval must be greater than zero and is shared with the peer through V4
    /// negotiation.
    pub fn with_v4_event_queue_polling(mut self, interval: Duration) -> Self {
        self.protocol.mode = ProtocolMode::V4 { fallback: true };
        self.protocol.wakeup = WakeupMode::EventQueuePolling { interval };
        self
    }

    pub fn verify(&mut self) -> Result<(), anyhow::Error> {
        if matches!(self.protocol.mode, ProtocolMode::Legacy)
            && !matches!(self.protocol.wakeup, WakeupMode::Default)
        {
            return Err(anyhow!(
                "wakeup features require ProtocolMode::V4; legacy mode cannot enable {:?}",
                self.protocol.wakeup
            ));
        }
        if let WakeupMode::EventQueuePolling { interval } = self.protocol.wakeup
            && interval.is_zero()
        {
            return Err(anyhow!(
                "event_queue_polling interval must be greater than zero"
            ));
        }
        if matches!(self.protocol.wakeup, WakeupMode::EventFd)
            && !matches!(self.mem_map_type, MemMapType::MemMapTypeMemFd)
        {
            return Err(anyhow!("eventfd wakeup requires memfd shared memory"));
        }

        if self.share_memory_buffer_cap < (1 << 20) {
            return Err(anyhow!(
                "share memory size is too small:{}, must greater than {}",
                self.share_memory_buffer_cap,
                1 << 20
            ));
        }
        if self.buffer_slice_sizes.is_empty() {
            return Err(anyhow!("buffer_slice_sizes could not be nil"));
        }

        let mut sum = 0;
        for pair in self.buffer_slice_sizes.iter_mut() {
            sum += pair.percent;
            if pair.size > self.share_memory_buffer_cap {
                return Err(anyhow!(
                    "buffer_slice_sizes's size:{} couldn't greater than share_memory_buffer_cap:{}",
                    pair.size,
                    self.share_memory_buffer_cap
                ));
            }

            let aligned = (pair.size + 3) & !3;
            if aligned != pair.size {
                pair.size = aligned;
            }
        }

        if sum != 100 {
            return Err(anyhow!(
                "the sum of buffer_slice_sizes's percent should be 100",
            ));
        }

        let aligned_queue_cap = (self.queue_cap + 7) & !7;
        if aligned_queue_cap != self.queue_cap {
            self.queue_cap = aligned_queue_cap;
        }

        if self.share_memory_path_prefix.is_empty() || self.queue_path.is_empty() {
            return Err(anyhow!("buffer path or queue path could not be nil"));
        }

        #[cfg(not(target_os = "linux"))]
        {
            return Err(anyhow!("shmipc just support linux OS now"));
        }

        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            return Err(anyhow!("shmipc just support amd64 or arm64 arch"));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Config, ProtocolMode, SizePercentPair, WakeupMode};

    #[test]
    fn size_percent_pair_accepts_lowercase_and_pascal_case_fields() {
        let pairs: Vec<SizePercentPair> = serde_json::from_str(
            r#"[
                {"size": 16384, "percent": 40},
                {"Size": 32768, "Percent": 60}
            ]"#,
        )
        .unwrap();

        assert_eq!(pairs[0].size, 16384);
        assert_eq!(pairs[0].percent, 40);
        assert_eq!(pairs[1].size, 32768);
        assert_eq!(pairs[1].percent, 60);
    }

    #[test]
    fn verify_aligns_buffer_slice_sizes_and_queue_cap() {
        let mut config = Config {
            queue_cap: 8193,
            buffer_slice_sizes: vec![
                SizePercentPair {
                    size: 4097,
                    percent: 50,
                },
                SizePercentPair {
                    size: 8193,
                    percent: 50,
                },
            ],
            ..Config::default()
        };

        config.verify().unwrap();

        assert_eq!(8200, config.queue_cap);
        assert_eq!(4100, config.buffer_slice_sizes[0].size);
        assert_eq!(8196, config.buffer_slice_sizes[1].size);
    }

    #[test]
    fn verify_rejects_wakeup_features_in_legacy_mode() {
        let mut config = Config::default().with_v4_event_queue_polling(Duration::from_micros(100));
        config.protocol.mode = ProtocolMode::Legacy;

        assert!(config.verify().is_err());
    }

    #[test]
    fn verify_rejects_zero_polling_interval() {
        let mut config = Config::default().with_v4_event_queue_polling(Duration::ZERO);

        assert!(config.verify().is_err());
    }

    #[test]
    fn verify_rejects_eventfd_without_memfd() {
        let mut config = Config {
            mem_map_type: crate::consts::MemMapType::MemMapTypeDevShmFile,
            ..Config::default().with_v4_eventfd()
        };

        assert!(config.verify().is_err());
    }

    #[test]
    fn v4_helpers_set_protocol_config() {
        let config = Config::default().with_v4_eventfd();

        assert_eq!(ProtocolMode::V4 { fallback: true }, config.protocol.mode);
        assert_eq!(WakeupMode::EventFd, config.protocol.wakeup);
    }
}
