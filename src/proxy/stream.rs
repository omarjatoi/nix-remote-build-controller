//! A byte stream that can be severed from outside.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

/// Wraps a transport so that cancelling a token fails every pending and future
/// read and write. russh runs each SSH connection on its own task that owns the
/// transport; handing it this wrapper is how the proxy closes a connection at
/// any point, including mid-handshake, without cooperation from that task.
pub struct CancellableStream<S> {
    inner: S,
    cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
    is_cancelled: bool,
}

impl<S> CancellableStream<S> {
    /// Wraps `inner`; `token` severs it.
    pub fn new(inner: S, token: CancellationToken) -> Self {
        Self {
            inner,
            cancelled: Box::pin(token.cancelled_owned()),
            is_cancelled: false,
        }
    }

    fn check(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if !self.is_cancelled && self.cancelled.as_mut().poll(cx).is_ready() {
            self.is_cancelled = true;
        }
        if self.is_cancelled {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "connection closed by proxy",
            ));
        }
        Ok(())
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for CancellableStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Err(err) = self.check(cx) {
            return Poll::Ready(Err(err));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CancellableStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Err(err) = self.check(cx) {
            return Poll::Ready(Err(err));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(err) = self.check(cx) {
            return Poll::Ready(Err(err));
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn cancellation_fails_pending_reads() {
        let (a, mut b) = tokio::io::duplex(64);
        let token = CancellationToken::new();
        let mut stream = CancellableStream::new(a, token.clone());

        b.write_all(b"hi").await.unwrap();
        let mut buf = [0u8; 2];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");

        let reader = tokio::spawn(async move {
            let mut buf = [0u8; 1];
            let err = stream.read(&mut buf).await.unwrap_err();
            (err.kind(), stream)
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        token.cancel();
        let (kind, mut stream) = reader.await.unwrap();
        assert_eq!(kind, io::ErrorKind::ConnectionAborted);
        assert!(stream.write_all(b"x").await.is_err());
    }
}
