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

use std::os::fd::AsRawFd;

use volo::net::conn::ConnStream;

pub trait ConnStreamExt {
    fn is_tcp(&self) -> bool;
    fn as_raw_fd(&self) -> i32;
}

impl ConnStreamExt for ConnStream {
    #[inline]
    fn is_tcp(&self) -> bool {
        matches!(self, Self::Tcp(_))
    }

    #[inline]
    fn as_raw_fd(&self) -> i32 {
        match self {
            Self::Tcp(s) => s.as_raw_fd(),
            #[cfg(target_family = "unix")]
            Self::Unix(s) => s.as_raw_fd(),
        }
    }
}
