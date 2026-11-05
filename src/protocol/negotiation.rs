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

use std::{collections::HashMap, env, fmt::Display, os::fd::RawFd, time::Duration};

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::{Config, ProtocolMode, WakeupMode},
    consts::HEADER_SIZE,
    protocol::{
        block_io::{block_read_full, block_write_full},
        event::EventType,
        header::Header,
        initializer::wait_event_header,
        protocol_trace,
    },
};

pub const PROTO_VERSION_V4: u8 = 4;
pub const PROTO_VERSION_V5: u8 = 5;
pub const DEFAULT_MSG_VERSION: u8 = 0;
pub const SDK_NAME: &str = "shmipc-rs";
pub const FEATURE_EVENT_QUEUE_POLLING: &str = "event_queue_polling";
pub const FEATURE_EVENTFD_WAKEUP: &str = "eventfd_wakeup";
pub const ERROR_VERSION_NOT_SUPPORTED: &str = "VERSION_NOT_SUPPORTED";
pub const ERROR_JSON_PARSE: &str = "JSON_PARSE_ERROR";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NegotiationRequest {
    pub supports_protocols: Vec<u8>,
    pub sdk_name: String,
    pub sdk_version: String,
    #[serde(default)]
    pub supports_features: Vec<String>,
    #[serde(default)]
    pub settings: HashMap<String, Value>,
}

impl NegotiationRequest {
    pub fn from_config(config: &Config) -> Result<Self, anyhow::Error> {
        let mut supports_features = Vec::new();
        let mut settings = HashMap::new();

        match config.protocol.wakeup {
            WakeupMode::Default => {}
            WakeupMode::EventQueuePolling { interval } => {
                supports_features.push(FEATURE_EVENT_QUEUE_POLLING.to_owned());
                let setting = EventQueuePollingSettings::from_interval(interval)?;
                settings.insert(
                    FEATURE_EVENT_QUEUE_POLLING.to_owned(),
                    serde_json::to_value(setting)?,
                );
            }
            WakeupMode::EventFd => {
                if !eventfd_supported() {
                    return Err(anyhow!("eventfd_wakeup is only supported on Linux"));
                }
                supports_features.push(FEATURE_EVENTFD_WAKEUP.to_owned());
            }
        }
        let supports_protocols = match config.protocol.mode {
            ProtocolMode::V5 { .. } => vec![PROTO_VERSION_V5, PROTO_VERSION_V4],
            ProtocolMode::V4 { .. } => vec![PROTO_VERSION_V4],
            // A legacy-mode client never gets here
            ProtocolMode::Legacy => vec![],
        };

        Ok(Self {
            supports_protocols,
            sdk_name: SDK_NAME.to_owned(),
            sdk_version: env!("CARGO_PKG_VERSION").to_owned(),
            supports_features,
            settings,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NegotiationResponse {
    pub selected_protocol: u8,
    pub sdk_name: String,
    pub sdk_version: String,
    #[serde(default)]
    pub selected_features: Vec<String>,
    #[serde(default)]
    pub settings: HashMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<NegotiationError>,
}

impl NegotiationResponse {
    pub fn ok(
        selected_protocol: u8,
        selected_feature: NegotiatedFeature,
    ) -> Result<Self, anyhow::Error> {
        let mut selected_features = Vec::new();
        let mut settings = HashMap::new();

        match selected_feature {
            NegotiatedFeature::None => {}
            NegotiatedFeature::EventQueuePolling { interval } => {
                selected_features.push(FEATURE_EVENT_QUEUE_POLLING.to_owned());
                let setting = EventQueuePollingSettings::from_interval(interval)?;
                settings.insert(
                    FEATURE_EVENT_QUEUE_POLLING.to_owned(),
                    serde_json::to_value(setting)?,
                );
            }
            NegotiatedFeature::EventFd => {
                selected_features.push(FEATURE_EVENTFD_WAKEUP.to_owned());
            }
        }

        Ok(Self {
            selected_protocol,
            sdk_name: SDK_NAME.to_owned(),
            sdk_version: env!("CARGO_PKG_VERSION").to_owned(),
            selected_features,
            settings,
            error: None,
        })
    }

    pub fn version_not_supported() -> Self {
        Self::error(
            ERROR_VERSION_NOT_SUPPORTED,
            "protocol version is not supported",
        )
    }

    pub fn json_parse_error(message: impl Into<String>) -> Self {
        Self::error(ERROR_JSON_PARSE, message)
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            selected_protocol: 0,
            sdk_name: SDK_NAME.to_owned(),
            sdk_version: env!("CARGO_PKG_VERSION").to_owned(),
            selected_features: Vec::new(),
            settings: HashMap::new(),
            error: Some(NegotiationError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }

    pub fn selected_feature_for_request(
        &self,
        request: &NegotiationRequest,
    ) -> Result<NegotiatedFeature, anyhow::Error> {
        self.selected_feature_with_request(Some(request))
    }

    fn selected_feature_with_request(
        &self,
        request: Option<&NegotiationRequest>,
    ) -> Result<NegotiatedFeature, anyhow::Error> {
        if self
            .selected_features
            .iter()
            .any(|feature| feature == FEATURE_EVENT_QUEUE_POLLING)
        {
            let setting = self
                .settings
                .get(FEATURE_EVENT_QUEUE_POLLING)
                .or_else(|| {
                    request.and_then(|request| request.settings.get(FEATURE_EVENT_QUEUE_POLLING))
                })
                .ok_or_else(|| anyhow!("missing event_queue_polling settings"))?;
            let setting: EventQueuePollingSettings = serde_json::from_value(setting.clone())?;
            return Ok(NegotiatedFeature::EventQueuePolling {
                interval: setting.interval()?,
            });
        }

        if self
            .selected_features
            .iter()
            .any(|feature| feature == FEATURE_EVENTFD_WAKEUP)
        {
            return Ok(NegotiatedFeature::EventFd);
        }

        Ok(NegotiatedFeature::None)
    }
}

/// Runs the client half of the JSON negotiation: send what this side speaks, take what the peer
/// picked.
///
/// The response is returned alongside the feature both sides agreed on, so the caller can build
/// the initializer for the selected version. Every failure is a [`NegotiationInitError`], which is
/// what lets a client tell a transport failure worth falling back to legacy from an outright
/// rejection that should reach the caller.
pub fn negotiate_client(
    conn_fd: RawFd,
    config: &Config,
) -> Result<(NegotiationResponse, NegotiatedFeature), anyhow::Error> {
    let request = NegotiationRequest::from_config(config)?;

    write_negotiation(conn_fd, &request).map_err(NegotiationInitError::network_or_protocol)?;

    let mut buf = [0u8; HEADER_SIZE];
    let response_header = wait_event_header(conn_fd, EventType::TYPE_NEGOTIATION, &mut buf)
        .map_err(NegotiationInitError::network_or_protocol)?;
    let response_body = read_message_body(conn_fd, &response_header)
        .map_err(NegotiationInitError::network_or_protocol)?;

    let response: NegotiationResponse =
        serde_json::from_slice(&response_body).map_err(NegotiationInitError::invalid_response)?;

    if let Some(err) = response.error {
        return Err(NegotiationInitError::rejected(err));
    }

    if !request
        .supports_protocols
        .contains(&response.selected_protocol)
    {
        return Err(NegotiationInitError::unsupported_protocol(
            response.selected_protocol,
        ));
    }

    let feature = response
        .selected_feature_for_request(&request)
        .map_err(NegotiationInitError::invalid_response)?;

    Ok((response, feature))
}

/// Runs the server half of the JSON negotiation, having already read `first_event` off the wire.
///
/// A request this side cannot satisfy is answered before the error is returned, so the peer learns
/// why rather than seeing the connection drop.
pub fn negotiate_server(
    conn_fd: RawFd,
    first_event: &Header,
) -> Result<(NegotiationResponse, NegotiatedFeature), anyhow::Error> {
    if first_event.msg_type() != EventType::TYPE_NEGOTIATION {
        return Err(anyhow!(
            "expected negotiation event, got:{}",
            first_event.msg_type().inner()
        ));
    }

    let body = read_message_body(conn_fd, first_event)?;
    let request: NegotiationRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            let _ = write_negotiation(
                conn_fd,
                &NegotiationResponse::json_parse_error(err.to_string()),
            );
            return Err(err.into());
        }
    };

    let selected_protocol = match select_protocol(&request) {
        Some(version) => version,
        None => {
            write_negotiation(conn_fd, &NegotiationResponse::version_not_supported())?;
            return Err(anyhow!("client does not support any supported protocol"));
        }
    };

    let feature = select_feature(&request);

    let response = NegotiationResponse::ok(selected_protocol, feature)?;

    write_negotiation(conn_fd, &response)?;

    Ok((response, feature))
}

/// Picks the newest version both sides speak, or `None` when the request names none of them.
pub fn select_protocol(request: &NegotiationRequest) -> Option<u8> {
    if request.supports_protocols.contains(&PROTO_VERSION_V5) {
        return Some(PROTO_VERSION_V5);
    }

    if request.supports_protocols.contains(&PROTO_VERSION_V4) {
        return Some(PROTO_VERSION_V4);
    }

    None
}

/// Sends one negotiation message: the JSON body of `value` behind an event header framing it.
pub(crate) fn write_negotiation<T>(conn_fd: RawFd, value: &T) -> Result<(), anyhow::Error>
where
    T: Serialize,
{
    let payload = serde_json::to_vec(value)?;
    let mut data = vec![0u8; HEADER_SIZE + payload.len()];
    let mut header = Header(data.as_mut_ptr());
    header.encode(
        data.len() as u32,
        DEFAULT_MSG_VERSION,
        EventType::TYPE_NEGOTIATION,
    );
    data[HEADER_SIZE..].copy_from_slice(&payload);
    protocol_trace(&header, &payload, true);
    block_write_full(conn_fd, &data)
}

/// Reads the body `h` announces, having already consumed the header itself.
///
/// The length comes from the peer, so a header claiming less than a header's worth is rejected
/// rather than wrapped around into a huge read.
pub(crate) fn read_message_body(conn_fd: RawFd, h: &Header) -> Result<Vec<u8>, anyhow::Error> {
    if h.length() < HEADER_SIZE as u32 {
        return Err(anyhow!(
            "invalid message length {}, header size {}",
            h.length(),
            HEADER_SIZE
        ));
    }
    let mut body = vec![0u8; h.length() as usize - HEADER_SIZE];
    if !body.is_empty() {
        block_read_full(conn_fd, &mut body)
            .map_err(|err| anyhow!("read message body failed, reason:{}", err))?;
    }
    protocol_trace(h, &body, false);
    Ok(body)
}

/// Why a negotiation failed, in the one distinction a client acts on.
///
/// Only [`Self::NetworkOrProtocol`] is worth reconnecting and retrying as legacy: the peer never
/// answered, so it may not speak negotiation at all. The other variants mean it answered and said
/// no, which is the caller's to see.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NegotiationInitError {
    #[error("negotiation failed due to network/protocol error: {source}")]
    NetworkOrProtocol {
        #[source]
        source: anyhow::Error,
    },
    #[error("negotiation rejected: {0}")]
    Rejected(crate::protocol::negotiation::NegotiationError),
    #[error("negotiation returned unsupported protocol {0}")]
    UnsupportedProtocol(u8),
    #[error("negotiation response is invalid: {source}")]
    InvalidResponse {
        #[source]
        source: anyhow::Error,
    },
}

impl NegotiationInitError {
    /// The peer could not be reached or did not answer in the shape the protocol requires.
    pub(crate) fn network_or_protocol(err: impl Into<anyhow::Error>) -> anyhow::Error {
        anyhow::Error::new(Self::NetworkOrProtocol { source: err.into() })
    }

    /// The peer answered and refused, naming a reason.
    pub(crate) fn rejected(err: crate::protocol::negotiation::NegotiationError) -> anyhow::Error {
        anyhow::Error::new(Self::Rejected(err))
    }

    /// The peer selected a version this side never offered.
    pub(crate) fn unsupported_protocol(version: u8) -> anyhow::Error {
        anyhow::Error::new(Self::UnsupportedProtocol(version))
    }

    /// The peer answered, but the answer could not be parsed or contradicts the request.
    pub(crate) fn invalid_response(err: impl Into<anyhow::Error>) -> anyhow::Error {
        anyhow::Error::new(Self::InvalidResponse { source: err.into() })
    }

    /// Whether this is the one failure a client may retry as legacy.
    pub(crate) const fn is_network_or_protocol(&self) -> bool {
        matches!(self, Self::NetworkOrProtocol { .. })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NegotiationError {
    pub code: String,
    pub message: String,
}

impl Display for NegotiationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventQueuePollingSettings {
    pub polling_interval_us: u64,
}

impl EventQueuePollingSettings {
    pub fn from_interval(interval: Duration) -> Result<Self, anyhow::Error> {
        let polling_interval_us = interval.as_micros().try_into().map_err(|_| {
            anyhow!("event_queue_polling interval is too large to encode in microseconds")
        })?;
        if polling_interval_us == 0 {
            return Err(anyhow!(
                "event_queue_polling interval must be greater than zero"
            ));
        }
        Ok(Self {
            polling_interval_us,
        })
    }

    pub fn interval(&self) -> Result<Duration, anyhow::Error> {
        if self.polling_interval_us == 0 {
            return Err(anyhow!(
                "event_queue_polling interval must be greater than zero"
            ));
        }
        Ok(Duration::from_micros(self.polling_interval_us))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NegotiatedFeature {
    #[default]
    None,
    EventQueuePolling {
        interval: Duration,
    },
    EventFd,
}

/// What negotiation settled, handed to the initializer of the selected protocol.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Negotiated {
    pub message_version: u8,
    pub feature: NegotiatedFeature,
}

impl Negotiated {
    pub const fn new(feature: NegotiatedFeature) -> Self {
        Self {
            message_version: DEFAULT_MSG_VERSION,
            feature,
        }
    }
}

pub fn select_feature(request: &NegotiationRequest) -> NegotiatedFeature {
    if request
        .supports_features
        .iter()
        .any(|feature| feature == FEATURE_EVENT_QUEUE_POLLING)
        && let Some(setting) = request.settings.get(FEATURE_EVENT_QUEUE_POLLING)
        && let Ok(setting) = serde_json::from_value::<EventQueuePollingSettings>(setting.clone())
        && let Ok(interval) = setting.interval()
    {
        return NegotiatedFeature::EventQueuePolling { interval };
    }

    if request
        .supports_features
        .iter()
        .any(|feature| feature == FEATURE_EVENTFD_WAKEUP)
        && eventfd_supported()
    {
        return NegotiatedFeature::EventFd;
    }

    NegotiatedFeature::None
}

pub const fn eventfd_supported() -> bool {
    cfg!(target_os = "linux")
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, time::Duration};

    use super::{
        FEATURE_EVENT_QUEUE_POLLING, FEATURE_EVENTFD_WAKEUP, NegotiatedFeature, NegotiationRequest,
        NegotiationResponse, eventfd_supported, select_feature,
    };
    use crate::{
        config::Config,
        consts::MemMapType,
        protocol::negotiation::{PROTO_VERSION_V4, PROTO_VERSION_V5, SDK_NAME, select_protocol},
    };

    #[test]
    fn request_encodes_polling_settings() {
        let config = Config::default().with_v4_event_queue_polling(Duration::from_micros(250));

        let request = NegotiationRequest::from_config(&config).unwrap();

        assert_eq!(
            vec![FEATURE_EVENT_QUEUE_POLLING.to_owned()],
            request.supports_features
        );
        assert_eq!(
            250,
            request.settings[FEATURE_EVENT_QUEUE_POLLING]["polling_interval_us"]
        );
    }

    #[test]
    fn request_encodes_eventfd_feature_when_supported() {
        let config = Config {
            mem_map_type: MemMapType::MemMapTypeMemFd,
            ..Config::default().with_v4_eventfd()
        };

        let result = NegotiationRequest::from_config(&config);
        if eventfd_supported() {
            assert_eq!(
                vec![FEATURE_EVENTFD_WAKEUP.to_owned()],
                result.unwrap().supports_features
            );
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn select_feature_prefers_polling_over_eventfd() {
        let config = Config::default().with_v4_event_queue_polling(Duration::from_micros(100));
        let mut request = NegotiationRequest::from_config(&config).unwrap();
        request
            .supports_features
            .push(FEATURE_EVENTFD_WAKEUP.to_owned());

        assert_eq!(
            NegotiatedFeature::EventQueuePolling {
                interval: Duration::from_micros(100)
            },
            select_feature(&request)
        );
    }

    #[test]
    fn response_decodes_selected_polling_settings() {
        let request = NegotiationRequest::from_config(
            &Config::default().with_v4_event_queue_polling(Duration::from_micros(321)),
        )
        .unwrap();
        let response = NegotiationResponse::ok(
            PROTO_VERSION_V4,
            NegotiatedFeature::EventQueuePolling {
                interval: Duration::from_micros(321),
            },
        )
        .unwrap();

        assert_eq!(
            NegotiatedFeature::EventQueuePolling {
                interval: Duration::from_micros(321)
            },
            response.selected_feature_for_request(&request).unwrap()
        );
    }

    #[test]
    fn response_decodes_no_selected_feature() {
        let response = NegotiationResponse::ok(PROTO_VERSION_V4, NegotiatedFeature::None).unwrap();

        let request = NegotiationRequest {
            supports_protocols: vec![PROTO_VERSION_V4],
            sdk_name: SDK_NAME.to_owned(),
            sdk_version: "test".to_owned(),
            supports_features: vec![],
            settings: HashMap::new(),
        };

        assert_eq!(
            response.selected_feature_for_request(&request).unwrap(),
            NegotiatedFeature::None
        );
    }

    #[test]
    fn rejects_protocol_not_supported_by_request() {
        let request = NegotiationRequest {
            supports_protocols: vec![PROTO_VERSION_V4],
            sdk_name: SDK_NAME.to_owned(),
            sdk_version: "test".to_owned(),
            supports_features: vec![],
            settings: HashMap::new(),
        };

        let response = NegotiationResponse::ok(PROTO_VERSION_V5, NegotiatedFeature::None).unwrap();

        assert!(
            !request
                .supports_protocols
                .contains(&response.selected_protocol)
        );
    }

    #[test]
    fn select_protocol_prefers_v5() {
        let request = NegotiationRequest {
            supports_protocols: vec![PROTO_VERSION_V4, PROTO_VERSION_V5],
            sdk_name: SDK_NAME.to_owned(),
            sdk_version: "test".to_owned(),
            supports_features: vec![],
            settings: HashMap::new(),
        };

        assert_eq!(select_protocol(&request), Some(PROTO_VERSION_V5));
    }

    #[test]
    fn select_protocol_returns_none_when_unsupported() {
        let request = NegotiationRequest {
            supports_protocols: vec![3],
            sdk_name: SDK_NAME.to_owned(),
            sdk_version: "test".to_owned(),
            supports_features: vec![],
            settings: HashMap::new(),
        };

        assert_eq!(select_protocol(&request), None);
    }
}
