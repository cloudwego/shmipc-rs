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

#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// InvalidVersion means that we received a frame with an invalid version.
    #[error("invalid protocol version")]
    InvalidVersion,

    /// InvalidMsgType means that we received a frame with an invalid message type.
    #[error("invalid msg type")]
    InvalidMsgType,

    /// SessionShutdown is used if there is a shutdown during an operation.
    #[error("session shutdown")]
    SessionShutdown,

    /// StreamsExhausted is returned if we have no more stream ids to issue.
    #[error("streams exhausted")]
    StreamsExhausted,

    /// DuplicateStream is used if a duplicate stream is opened inbound.
    #[error("duplicate stream initiated")]
    DuplicateStream,

    /// Timeout is used when we reach an IO deadline.
    #[error("i/o deadline reached")]
    Timeout,

    /// StreamClosed is returned when using a closed stream.
    #[error("stream closed")]
    StreamClosed,

    /// StreamResetByPeer is returned when the peer reset the stream.
    #[error("stream reset by peer")]
    StreamReset,

    /// ConnectionWriteTimeout indicates that we hit the "safety valve" timeout writing to the
    /// underlying stream connection.
    #[error("connection write timeout")]
    ConnectionWriteTimeout,

    #[error("connection timeout")]
    ConnectionTimeout,

    /// KeepAliveTimeout is sent if a missed keepalive caused the stream close.
    #[error("keepalive timeout")]
    KeepAliveTimeout,

    /// EndOfStream means that the stream is end, user shouldn't read from the stream.
    #[error("end of stream")]
    EndOfStream,

    /// SessionUnhealthy occurred at `session.open_stream()`, which means that the session is
    /// overload.
    ///
    /// User should retry after 60 seconds, and the following situation will result in
    /// SessionUnhealthy.
    ///
    /// 1. When local share memory is not enough, client send request data via unix domain socket.
    ///
    /// 2. When peer share memory is not enough, client receive response data from unix domain
    ///    socket.
    #[error("now the session is unhealthy, please retry later")]
    SessionUnhealthy,

    /// NotEnoughData means that the real read size < expect read size.
    ///
    /// In general, which happened on application protocol is buffered.
    #[error("current buffer is not enough data to read")]
    NotEnoughData,

    /// NoMoreBuffer means that the share memory is busy, and no more buffer to allocate.
    #[error("share memory no more buffer")]
    NoMoreBuffer,

    /// SizeTooLarge means that the allocated size exceeded.
    #[error("alloc size exceed")]
    SizeTooLarge,

    /// BrokenBuffer means that the share memory's layout had broken, which happens in that the
    /// share memory was alter by external or internal bug.
    #[error("share memory's buffer had broken")]
    BrokenBuffer,

    #[error("share memory had not left space")]
    ShareMemoryHadNotLeftSpace,

    #[error("stream callbacks had existed")]
    StreamCallbackHadExisted,

    /// ExchangeConfig means message type error during exchange config phase.
    #[error("exchange config protocol invalid")]
    ExchangeConfig,

    /// ExchangeConfigTimeout means client exchange config timeout.
    #[error("exchange config timeout")]
    ExchangeConfigTimeout,

    #[error("shmipc just support linux OS now")]
    OSNonSupported,

    #[error("shmipc just support amd64 or arm64 arch")]
    ArchNonSupported,

    /// Ensure once hot restart succeed
    #[error("hot restart in progress, try again later")]
    HotRestartInProgress,

    #[error("session in handshake stage, try again later")]
    InHandshakeStage,

    /// File name max len 255
    #[error("share memory path prefix too long")]
    FileNameTooLong,

    #[error("the queue is empty")]
    QueueEmpty,

    #[error("the queue is full")]
    QueueFull,

    #[error("stream pool is full")]
    StreamPoolFull,

    #[error("stream had unread data, size: {0}")]
    StreamHasUnreadData(usize),

    #[error("stream had pending data, pending slice len: {0}")]
    StreamHasPendingData(usize),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Others(#[from] anyhow::Error),
}
