#![cfg_attr(not(doctest), doc = include_str!("../README.md"))]

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

mod buffer;
pub use buffer::{BufferReader, BufferWriter};
pub mod config;
pub mod consts;
mod error;
pub use error::Error;
mod protocol;
mod queue;
mod session;
pub use session::{config::SessionManagerConfig, manager::SessionManager};
mod stream;
pub use stream::{AsyncReadShm, AsyncWriteShm, Stream};
mod listener;
pub use listener::Listener;
pub mod stats;
mod util;
pub use buffer::{linked::LinkedBuffer, slice::BufferSlice};
