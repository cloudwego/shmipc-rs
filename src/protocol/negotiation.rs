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

use std::{collections::HashMap, fmt::Display, time::Duration};

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, WakeupMode};

pub const PROTO_VERSION_V4: u8 = 4;
pub const MSG_VERSION_V4: u8 = 0;
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

        Ok(Self {
            supports_protocols: vec![PROTO_VERSION_V4],
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
    pub fn ok(selected_feature: NegotiatedFeature) -> Result<Self, anyhow::Error> {
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
            selected_protocol: PROTO_VERSION_V4,
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
    use std::time::Duration;

    use super::{
        FEATURE_EVENT_QUEUE_POLLING, FEATURE_EVENTFD_WAKEUP, NegotiatedFeature, NegotiationRequest,
        NegotiationResponse, eventfd_supported, select_feature,
    };
    use crate::{config::Config, consts::MemMapType};

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
        let response = NegotiationResponse::ok(NegotiatedFeature::EventQueuePolling {
            interval: Duration::from_micros(321),
        })
        .unwrap();

        assert_eq!(
            NegotiatedFeature::EventQueuePolling {
                interval: Duration::from_micros(321)
            },
            response.selected_feature_for_request(&request).unwrap()
        );
    }
}
