//! Listening helpers and the small HTTP servers used for health and metrics.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::Context as _;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// How long [`HttpServer::shutdown`] waits for in-flight requests.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Binds a TCP listener for a Go-style address: `":2222"` means every
/// interface, otherwise `host:port` (host may be a name, an IPv4 address or a
/// bracketed IPv6 address).
///
/// For the all-interfaces form the listener is a dual-stack IPv6 socket, as
/// Go's `net.Listen("tcp", ":port")` creates, falling back to IPv4 on hosts
/// with IPv6 disabled.
pub async fn bind(addr: &str) -> anyhow::Result<TcpListener> {
    let Some(port) = addr.strip_prefix(':') else {
        return TcpListener::bind(addr)
            .await
            .with_context(|| format!("listen on {addr}"));
    };
    let port: u16 = port
        .parse()
        .with_context(|| format!("invalid port in listen address {addr:?}"))?;
    match bind_dual_stack(port) {
        Ok(listener) => {
            TcpListener::from_std(listener).with_context(|| format!("listen on {addr}"))
        }
        Err(v6_err) => TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))
            .await
            .with_context(|| format!("listen on {addr} (IPv6 attempt failed: {v6_err})")),
    }
}

/// An IPv6 listener on all interfaces that also accepts IPv4, whatever the
/// host's `bindv6only` default.
fn bind_dual_stack(port: u16) -> io::Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_only_v6(false)?;
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// A running HTTP server that stops when [`HttpServer::shutdown`] is called.
pub struct HttpServer {
    addr: SocketAddr,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl HttpServer {
    /// Serves `router` on `listener` in the background.
    pub fn spawn(
        name: &'static str,
        listener: TcpListener,
        router: axum::Router,
    ) -> anyhow::Result<Self> {
        let addr = listener.local_addr()?;
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        info!(address = %addr, "{name} server starting");
        let task = tokio::spawn(async move {
            let result = axum::serve(listener, router)
                .with_graceful_shutdown(async move { stopped.cancelled().await })
                .await;
            if let Err(err) = result {
                error!(error = %err, "{name} server failed");
            }
        });
        Ok(Self { addr, stop, task })
    }

    /// The address the server is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting connections and waits briefly for in-flight requests.
    pub async fn shutdown(self) {
        self.stop.cancel();
        let mut task = self.task;
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut task)
            .await
            .is_err()
        {
            warn!(address = %self.addr, "HTTP server shutdown timed out");
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn binds_go_style_addresses() {
        let any = bind(":0").await.expect("bind :0");
        let any_addr = any.local_addr().unwrap();
        assert!(any_addr.ip().is_unspecified());
        // All-interfaces listeners accept IPv4 even when bound as IPv6.
        tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, any_addr.port()))
            .await
            .expect("IPv4 connection to an all-interfaces listener");
        let local = bind("127.0.0.1:0").await.expect("bind loopback");
        assert!(local.local_addr().unwrap().ip().is_loopback());
        assert!(bind(":notaport").await.is_err());
    }
}
