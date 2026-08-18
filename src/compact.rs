use std::{
    pin::Pin,
    task::{Context, Poll, ready},
};

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{error::Error, stream::Stream};

const MAX_READ_SLICES_PER_POLL: usize = 64;

/// [`Stream`] compatible with [`AsyncRead`] and [`AsyncWrite`].
pub struct StreamExt {
    inner: Stream,

    read_state: ReadState,
    write_state: WriteState,
}

impl StreamExt {
    pub fn new(inner: Stream) -> Self {
        let read_stream = inner.clone();
        let write_stream = inner.clone();
        Self {
            inner,
            read_state: ReadState::Idle(read_stream),
            write_state: WriteState::Idle(write_stream),
        }
    }

    pub const fn inner(&self) -> &Stream {
        &self.inner
    }

    pub const fn inner_mut(&mut self) -> &mut Stream {
        &mut self.inner
    }

    pub fn into_inner(self) -> Stream {
        self.inner
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + Sync + 'a>>;

enum ReadState {
    Idle(Stream),
    Reading {
        tracked_stream: Stream,
        future: BoxFuture<'static, (Stream, Result<(), Error>)>,
    },
    Eof(Stream),
    Transitioning,
}

impl ReadState {
    fn belongs_to(&self, inner: &Stream) -> bool {
        match self {
            Self::Idle(stream)
            | Self::Eof(stream)
            | Self::Reading {
                tracked_stream: stream,
                ..
            } => stream.shares_inner_with(inner),
            Self::Transitioning => false,
        }
    }
}

enum WriteState {
    // Keep the write-side handle in the state machine so an in-flight future owns it instead of
    // borrowing `StreamExt::inner` across poll calls.
    Idle(Stream),
    Flushing(BoxFuture<'static, (Stream, Result<(), Error>)>),
    Closing(BoxFuture<'static, (Stream, Result<(), Error>)>),
    Transitioning,
}

impl WriteState {
    fn sync_idle_with(&mut self, inner: &Stream) {
        // An operation that is already in flight finishes on the stream where it started. The
        // next operation switches to a replacement installed through `StreamExt::inner_mut`.
        if let Self::Idle(stream) = self
            && !stream.shares_inner_with(inner)
        {
            *stream = inner.clone();
        }
    }
}

impl AsyncRead for StreamExt {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if !this.read_state.belongs_to(&this.inner) {
            this.read_state = ReadState::Idle(this.inner.clone());
        }

        loop {
            match std::mem::replace(&mut this.read_state, ReadState::Transitioning) {
                ReadState::Idle(mut stream) => {
                    let tracked_stream = stream.clone();
                    let fut = Box::pin(async move {
                        let result = stream.wait_readable().await;
                        (stream, result)
                    });
                    this.read_state = ReadState::Reading {
                        tracked_stream,
                        future: fut,
                    };
                }
                ReadState::Reading {
                    tracked_stream,
                    mut future,
                } => {
                    let (mut stream, res) = match future.as_mut().poll(cx) {
                        Poll::Pending => {
                            this.read_state = ReadState::Reading {
                                tracked_stream,
                                future,
                            };
                            return Poll::Pending;
                        }
                        Poll::Ready(res) => res,
                    };
                    match res {
                        Ok(()) => {
                            match stream.read_available_into(buf, MAX_READ_SLICES_PER_POLL) {
                                Ok(read) => {
                                    debug_assert!(read > 0);
                                    this.read_state = ReadState::Idle(stream);
                                    return Poll::Ready(Ok(()));
                                }
                                Err(Error::NotEnoughData) => {
                                    // Another handle may have consumed the data after the wait
                                    // completed. Wait again instead of exposing a spurious EOF.
                                    this.read_state = ReadState::Idle(stream);
                                }
                                Err(Error::EndOfStream) => {
                                    this.read_state = ReadState::Eof(stream);
                                    return Poll::Ready(Ok(()));
                                }
                                Err(e) => {
                                    this.read_state = ReadState::Idle(stream);
                                    return Poll::Ready(Err(e.into()));
                                }
                            }
                        }
                        Err(Error::EndOfStream) => {
                            // AsyncRead represents EOF by successfully reading zero bytes.
                            this.read_state = ReadState::Eof(stream);
                            return Poll::Ready(Ok(()));
                        }
                        Err(e) => {
                            this.read_state = ReadState::Idle(stream);
                            return Poll::Ready(Err(e.into()));
                        }
                    }
                }
                ReadState::Eof(stream) => {
                    this.read_state = ReadState::Eof(stream);
                    return Poll::Ready(Ok(()));
                }
                ReadState::Transitioning => unreachable!("invalid transient read state"),
            }
        }
    }
}

impl AsyncWrite for StreamExt {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.get_mut();
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        loop {
            this.write_state.sync_idle_with(&this.inner);
            match &mut this.write_state {
                WriteState::Idle(stream) => {
                    return Poll::Ready(stream.write_bytes(buf).map_err(Into::into));
                }
                WriteState::Flushing(future) => {
                    let (stream, result) = ready!(future.as_mut().poll(cx));
                    this.write_state = WriteState::Idle(stream);
                    if let Err(error) = result {
                        return Poll::Ready(Err(error.into()));
                    }
                }
                WriteState::Closing(_) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "close in progress during write",
                    )));
                }
                WriteState::Transitioning => unreachable!("invalid transient write state"),
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();

        loop {
            this.write_state.sync_idle_with(&this.inner);
            match &mut this.write_state {
                WriteState::Idle(_) => {
                    let WriteState::Idle(mut stream) =
                        std::mem::replace(&mut this.write_state, WriteState::Transitioning)
                    else {
                        unreachable!();
                    };
                    let future = Box::pin(async move {
                        let result = stream.flush(true).await;
                        (stream, result)
                    });
                    this.write_state = WriteState::Flushing(future);
                }
                WriteState::Flushing(future) => {
                    let (stream, result) = ready!(future.as_mut().poll(cx));
                    this.write_state = WriteState::Idle(stream);
                    match result {
                        Ok(_) | Err(Error::StreamClosed) => return Poll::Ready(Ok(())),
                        Err(e) => return Poll::Ready(Err(e.into())),
                    }
                }
                WriteState::Closing(_) => {
                    return Poll::Ready(Err(std::io::Error::other(
                        "close in progress during flush",
                    )));
                }
                WriteState::Transitioning => unreachable!("invalid transient write state"),
            }
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();

        loop {
            this.write_state.sync_idle_with(&this.inner);
            match &mut this.write_state {
                WriteState::Idle(_) => {
                    let WriteState::Idle(mut stream) =
                        std::mem::replace(&mut this.write_state, WriteState::Transitioning)
                    else {
                        unreachable!();
                    };
                    let future = Box::pin(async move {
                        let result = stream.close().await;
                        (stream, result)
                    });
                    this.write_state = WriteState::Closing(future);
                }
                WriteState::Closing(future) => {
                    let (stream, result) = ready!(future.as_mut().poll(cx));
                    this.write_state = WriteState::Idle(stream);
                    match result {
                        Ok(_) => return Poll::Ready(Ok(())),
                        Err(e) => {
                            return Poll::Ready(Err(e.into()));
                        }
                    }
                }
                WriteState::Flushing(future) => {
                    let (stream, result) = ready!(future.as_mut().poll(cx));
                    this.write_state = WriteState::Idle(stream);
                    if let Err(e) = result {
                        return Poll::Ready(Err(e.into()));
                    }
                }
                WriteState::Transitioning => unreachable!("invalid transient write state"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StreamExt;

    fn assert_unpin<T: Unpin>() {}

    #[test]
    fn stream_ext_remains_unpin() {
        assert_unpin::<StreamExt>();
    }
}
