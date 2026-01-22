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

use std::time::Duration;

use crate::config::Config;

#[derive(Clone, Debug)]
pub struct SessionManagerConfig {
    config: Config,
    session_num: usize,
    stream_max_idle_time: Duration,
}

impl SessionManagerConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub const fn config(&self) -> &Config {
        &self.config
    }

    pub const fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    pub fn with_config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    pub const fn session_num(&self) -> usize {
        self.session_num
    }

    pub fn with_session_num(mut self, session_num: usize) -> Self {
        self.session_num = session_num;
        self
    }

    pub const fn stream_max_idle_time(&self) -> Duration {
        self.stream_max_idle_time
    }

    pub fn with_stream_max_idle_time(mut self, stream_max_idle_time: Duration) -> Self {
        self.stream_max_idle_time = stream_max_idle_time;
        self
    }
}

impl Default for SessionManagerConfig {
    fn default() -> Self {
        Self {
            config: Default::default(),
            session_num: 1,
            stream_max_idle_time: Duration::from_secs(30),
        }
    }
}
