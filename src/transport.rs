use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};

pub trait TransportStream: std::os::fd::AsRawFd {
    type ReadHalf: tokio::io::AsyncRead;
    type WriteHalf: tokio::io::AsyncWrite;

    fn into_split(self) -> (Self::ReadHalf, Self::WriteHalf);
}

pub trait TransportConnect {
    type Stream: TransportStream;
    type Address;

    fn connect(
        &self,
        addr: Self::Address,
    ) -> impl Future<Output = std::io::Result<Self::Stream>> + Send;
}

pub trait TransportListen {
    type Listener: TransportListener;
    type Address;

    fn listen(
        &self,
        addr: Self::Address,
    ) -> impl Future<Output = std::io::Result<Self::Listener>> + Send;
}

pub trait TransportListener {
    type Stream: TransportStream;
    type Address;

    fn accept(&self)
    -> impl Future<Output = std::io::Result<(Self::Stream, Self::Address)>> + Send;
}

impl TransportStream for TcpStream {
    type ReadHalf = tokio::net::tcp::OwnedReadHalf;
    type WriteHalf = tokio::net::tcp::OwnedWriteHalf;

    fn into_split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        self.into_split()
    }
}

impl TransportStream for UnixStream {
    type ReadHalf = tokio::net::unix::OwnedReadHalf;
    type WriteHalf = tokio::net::unix::OwnedWriteHalf;

    fn into_split(self) -> (Self::ReadHalf, Self::WriteHalf) {
        self.into_split()
    }
}

#[derive(Clone, Debug, Default)]
pub struct DefaultTcpConnect;

impl TransportConnect for DefaultTcpConnect {
    type Stream = TcpStream;
    type Address = std::net::SocketAddr;

    fn connect(
        &self,
        addr: Self::Address,
    ) -> impl Future<Output = std::io::Result<Self::Stream>> + Send {
        TcpStream::connect(addr)
    }
}

#[derive(Clone, Debug, Default)]
pub struct DefaultUnixConnect;

impl TransportConnect for DefaultUnixConnect {
    type Stream = UnixStream;
    type Address = std::os::unix::net::SocketAddr;

    async fn connect(&self, addr: Self::Address) -> std::io::Result<Self::Stream> {
        let Some(path) = addr.as_pathname() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "unnamed unix domain socket cannot be connected",
            ));
        };
        UnixStream::connect(path).await
    }
}

#[derive(Clone, Debug, Default)]
pub struct DefaultTcpListen;

impl TransportListen for DefaultTcpListen {
    type Listener = TcpListener;
    type Address = std::net::SocketAddr;

    async fn listen(&self, addr: Self::Address) -> std::io::Result<Self::Listener> {
        TcpListener::from_std(std::net::TcpListener::bind(addr)?)
    }
}

#[derive(Clone, Debug, Default)]
pub struct DefaultUnixListen;

impl TransportListen for DefaultUnixListen {
    type Listener = UnixListener;
    type Address = std::os::unix::net::SocketAddr;

    async fn listen(&self, addr: Self::Address) -> std::io::Result<Self::Listener> {
        let Some(path) = addr.as_pathname() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "unnamed unix domain socket cannot be listened",
            ));
        };
        UnixListener::bind(path)
    }
}

impl TransportListener for TcpListener {
    type Stream = TcpStream;
    type Address = std::net::SocketAddr;

    fn accept(
        &self,
    ) -> impl Future<Output = std::io::Result<(Self::Stream, Self::Address)>> + Send {
        self.accept()
    }
}

impl TransportListener for UnixListener {
    type Stream = UnixStream;
    type Address = tokio::net::unix::SocketAddr;

    fn accept(
        &self,
    ) -> impl Future<Output = std::io::Result<(Self::Stream, Self::Address)>> + Send {
        self.accept()
    }
}
