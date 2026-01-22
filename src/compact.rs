use std::{
    pin::Pin,
    task::{Context, Poll, ready},
};

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{buffer::Buf, error::Error, stream::Stream};

/// [`Stream`] compatible with [`AsyncRead`] and [`AsyncWrite`].
pub struct StreamExt {
    inner: Stream,

    read_state: ReadState,
    write_state: WriteState,
}

impl StreamExt {
    pub const fn new(inner: Stream) -> Self {
        Self {
            inner,
            read_state: ReadState::Idle,
            write_state: WriteState::Idle,
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
    Idle,
    Reading(BoxFuture<'static, Result<Buf<'static>, Error>>),
    Consuming(crate::util::shmbuf_reader::BufReader),
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
        loop {
            match &mut this.read_state {
                ReadState::Idle => {
                    if buf.remaining() == 0 {
                        return Poll::Ready(Ok(()));
                    }
                    let fut = this.inner.read();
                    let fut: BoxFuture<'_, Result<Buf<'_>, Error>> = Box::pin(fut);
                    // Safety:
                    // The `fut` returned by `inner.read` captures the lifetime of `inner`.
                    // Since `fut` is stored in `self.read_state` and `self` is pinned, `inner` will
                    // remain valid as long as `fut` exists. We use `transmute`
                    // to erase the lifetime, satisfying the static requirement
                    // of `BoxFuture`.
                    let fut: BoxFuture<'static, Result<Buf<'static>, Error>> =
                        unsafe { std::mem::transmute(fut) };
                    this.read_state = ReadState::Reading(fut);
                }
                ReadState::Reading(fut) => {
                    let res = ready!(fut.as_mut().poll(cx));
                    match res {
                        Ok(b) => {
                            this.read_state =
                                ReadState::Consuming(crate::util::shmbuf_reader::BufReader::new(b));
                        }
                        Err(e) => {
                            this.read_state = ReadState::Idle;
                            return Poll::Ready(Err(e.into()));
                        }
                    }
                }
                ReadState::Consuming(reader) => {
                    if reader.read(buf) {
                        this.read_state = ReadState::Idle;
                    }
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

impl AsyncWrite for StreamExt {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.get_mut();
        Poll::Ready(this.inner.write_bytes(buf).map_err(Into::into))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();

        loop {
            match &mut this.write_state {
                WriteState::Idle => {
                    let fut = this.inner.flush(true);
                    let fut: BoxFuture<'_, Result<(), Error>> = Box::pin(fut);
                    // Safety:
                    // Similar to poll_read, `fut` captures `inner`. `inner` is pinned via `self`.
                    let fut: BoxFuture<'static, Result<(), Error>> =
                        unsafe { std::mem::transmute(fut) };
                    this.write_state = WriteState::Flushing(fut);
                }
                WriteState::Flushing(fut) => {
                    let res = ready!(fut.as_mut().poll(cx));
                    this.write_state = WriteState::Idle;
                    match res {
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
                    let fut = this.inner.close();
                    let fut: BoxFuture<'_, Result<(), Error>> = Box::pin(fut);
                    // Safety:
                    // Similar to poll_read, `fut` captures `inner`. `inner` is pinned via `self`.
                    let fut: BoxFuture<'static, Result<(), Error>> =
                        unsafe { std::mem::transmute(fut) };
                    this.write_state = WriteState::Closing(fut);
                }
                WriteState::Closing(fut) => {
                    let res = ready!(fut.as_mut().poll(cx));
                    this.write_state = WriteState::Idle;
                    match res {
                        Ok(_) => return Poll::Ready(Ok(())),
                        Err(e) => {
                            return Poll::Ready(Err(e.into()));
                        }
                    }
                }
                WriteState::Flushing(_) => {
                    // If we are flushing, we need to drive the flush to completion first.
                    // Since we are in poll_shutdown, and it requires a mutable reference,
                    // and poll_flush also requires one, we can't easily call self.poll_flush(cx).
                    // Instead, we just poll the existing flush future directly here.

                    // Note: We rely on the loop to re-enter this match arm.
                    // However, the Flushing state holds a future that needs to be polled.
                    // We can extract the future temporarily or match on it.
                    // But wait, `this.write_state` is `&mut WriteState`.
                    let WriteState::Flushing(fut) = &mut this.write_state else {
                        unreachable!();
                    };
                    let res = ready!(fut.as_mut().poll(cx));
                    this.write_state = WriteState::Idle;
                    if let Err(e) = res {
                        return Poll::Ready(Err(e.into()));
                    }
                    // Flush completed, continue loop to start closing
                    continue;
                }
            }
        }
    }
}
