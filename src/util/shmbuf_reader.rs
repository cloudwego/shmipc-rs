use crate::buffer::Buf;

pub struct BufReader {
    buf: Buf<'static>,
    consumed: usize,
}

impl BufReader {
    pub const fn new(buf: Buf<'static>) -> Self {
        Self { buf, consumed: 0 }
    }

    // return true: all data is consumed
    pub fn read(&mut self, buf: &mut tokio::io::ReadBuf<'_>) -> bool {
        tracing::trace!(
            "ShmBufReader: buf.len = {}, consumed = {}, read.remaining = {}",
            self.buf.len(),
            self.consumed,
            buf.remaining()
        );
        if self.consumed >= self.buf.len() {
            return true;
        }
        let inner = &self.buf[self.consumed..];
        if inner.len() <= buf.remaining() {
            buf.put_slice(inner);
            self.consumed += inner.len();
            return true;
        }
        let read_len = buf.remaining();
        buf.put_slice(&inner[..read_len]);
        self.consumed += read_len;
        false
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;

    use super::BufReader;
    use crate::buffer::Buf;

    const BUF_LEN: usize = 64;
    const READ_SIZE: usize = 16;

    fn shm_buf(size: usize) -> Buf<'static> {
        let mut buf = BytesMut::with_capacity(size);
        // SAFETY: just for testing
        unsafe { buf.set_len(size) };
        Buf::Exm(buf.freeze())
    }

    #[test]
    fn test_nothing() {
        let mut buf = [0u8; 64];
        let mut readbuf = tokio::io::ReadBuf::new(&mut buf);
        let mut reader = BufReader::new(shm_buf(0));
        assert!(reader.read(&mut readbuf));
        assert_eq!(reader.consumed, 0);
    }

    #[test]
    fn test_read_once() {
        let mut buf = [0u8; BUF_LEN];
        let mut readbuf = tokio::io::ReadBuf::new(&mut buf);
        let mut reader = BufReader::new(shm_buf(READ_SIZE));
        assert!(reader.read(&mut readbuf));
        assert_eq!(reader.consumed, READ_SIZE);
    }

    #[test]
    fn test_read_twice() {
        const DATA_LEN: usize = BUF_LEN + READ_SIZE;

        let mut buf = [0u8; BUF_LEN];
        let mut readbuf = tokio::io::ReadBuf::new(&mut buf);
        let mut reader = BufReader::new(shm_buf(80));
        assert!(!reader.read(&mut readbuf));
        assert_eq!(reader.consumed, BUF_LEN);
        readbuf.clear();
        assert!(reader.read(&mut readbuf));
        assert_eq!(reader.consumed, DATA_LEN);
    }
}
