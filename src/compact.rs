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
        Self {
            inner,
            read_state: ReadState::Idle,
            write_state: WriteState::Idle,
        }
    }

    pub const fn inner(&self) -> &Stream {
        &self.inner
    }

    /// Returns the inner stream mutably, canceling any pending read and clearing cached EOF.
    pub fn inner_mut(&mut self) -> &mut Stream {
        // The caller may replace `inner`. Cancel a wait on the old stream and clear its EOF state
        // before handing out the mutable reference.
        self.read_state = ReadState::Idle;
        &mut self.inner
    }

    pub fn into_inner(self) -> Stream {
        self.inner
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + Sync + 'a>>;

enum ReadState {
    Idle,
    Reading(BoxFuture<'static, (Stream, Result<(), Error>)>),
    Eof,
    Transitioning,
}

enum WriteState {
    Idle,
    Flushing(BoxFuture<'static, Result<(), Error>>),
    Closing(BoxFuture<'static, Result<(), Error>>),
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

        loop {
            match std::mem::replace(&mut this.read_state, ReadState::Transitioning) {
                ReadState::Idle => {
                    let mut stream = this.inner.clone();
                    let fut = Box::pin(async move {
                        let result = stream.wait_readable().await;
                        (stream, result)
                    });
                    this.read_state = ReadState::Reading(fut);
                }
                ReadState::Reading(mut future) => {
                    let (mut stream, res) = match future.as_mut().poll(cx) {
                        Poll::Pending => {
                            this.read_state = ReadState::Reading(future);
                            return Poll::Pending;
                        }
                        Poll::Ready(res) => res,
                    };
                    match res {
                        Ok(()) => {
                            match stream.read_available_into(buf, MAX_READ_SLICES_PER_POLL) {
                                Ok(read) => {
                                    debug_assert!(read > 0);
                                    this.read_state = ReadState::Idle;
                                    return Poll::Ready(Ok(()));
                                }
                                Err(Error::NotEnoughData) => {
                                    // Another handle may have consumed the data after the wait
                                    // completed. Wait again instead of exposing a spurious EOF.
                                    this.read_state = ReadState::Idle;
                                }
                                Err(Error::EndOfStream) => {
                                    this.read_state = ReadState::Eof;
                                    return Poll::Ready(Ok(()));
                                }
                                Err(e) => {
                                    this.read_state = ReadState::Idle;
                                    return Poll::Ready(Err(e.into()));
                                }
                            }
                        }
                        Err(Error::EndOfStream) => {
                            // AsyncRead represents EOF by successfully reading zero bytes.
                            this.read_state = ReadState::Eof;
                            return Poll::Ready(Ok(()));
                        }
                        Err(e) => {
                            this.read_state = ReadState::Idle;
                            return Poll::Ready(Err(e.into()));
                        }
                    }
                }
                ReadState::Eof => {
                    this.read_state = ReadState::Eof;
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
            match &mut this.write_state {
                WriteState::Idle => {
                    return Poll::Ready(this.inner.write_bytes(buf).map_err(Into::into));
                }
                WriteState::Flushing(future) => {
                    let result = ready!(future.as_mut().poll(cx));
                    this.write_state = WriteState::Idle;
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
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();

        loop {
            match &mut this.write_state {
                WriteState::Idle => {
                    // Only the in-flight future needs its own handle. Keeping the idle state
                    // empty avoids making `StreamExt` carry an extra `Stream` permanently.
                    let mut stream = this.inner.clone();
                    let future = Box::pin(async move { stream.flush(true).await });
                    this.write_state = WriteState::Flushing(future);
                }
                WriteState::Flushing(future) => {
                    let result = ready!(future.as_mut().poll(cx));
                    this.write_state = WriteState::Idle;
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
            }
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();

        loop {
            match &mut this.write_state {
                WriteState::Idle => {
                    let mut stream = this.inner.clone();
                    let future = Box::pin(async move { stream.close().await });
                    this.write_state = WriteState::Closing(future);
                }
                WriteState::Closing(future) => {
                    let result = ready!(future.as_mut().poll(cx));
                    this.write_state = WriteState::Idle;
                    match result {
                        Ok(_) => return Poll::Ready(Ok(())),
                        Err(e) => {
                            return Poll::Ready(Err(e.into()));
                        }
                    }
                }
                WriteState::Flushing(future) => {
                    let result = ready!(future.as_mut().poll(cx));
                    this.write_state = WriteState::Idle;
                    if let Err(e) = result {
                        return Poll::Ready(Err(e.into()));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::{ReadState, StreamExt, WriteState};
    use crate::stream::Stream;

    fn assert_unpin<T: Unpin>() {}

    #[test]
    fn stream_ext_remains_unpin() {
        assert_unpin::<StreamExt>();
    }

    #[test]
    fn states_do_not_inline_a_stream_handle() {
        assert!(size_of::<ReadState>() < size_of::<Stream>());
        assert!(size_of::<WriteState>() < size_of::<Stream>());
    }
}
