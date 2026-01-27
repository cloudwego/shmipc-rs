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

use crossbeam_queue::ArrayQueue;

use crate::{Error, stream::Stream};

#[derive(Debug)]
pub struct StreamPool {
    streams: ArrayQueue<Stream>,
}

impl StreamPool {
    pub fn new(pool_capacity: usize) -> Self {
        Self {
            streams: ArrayQueue::new(pool_capacity),
        }
    }

    pub async fn push(&self, stream: Stream) -> Result<(), Error> {
        match self.streams.push(stream) {
            Ok(()) => Ok(()),
            Err(mut stream) => {
                stream.safe_close_notify();
                _ = stream.close().await;
                Err(Error::StreamPoolFull)
            }
        }
    }

    pub fn pop(&self) -> Option<Stream> {
        self.streams.pop()
    }

    pub async fn close(&self) {
        while let Some(mut s) = self.pop() {
            s.safe_close_notify();
            _ = s.close().await;
        }
    }
}
