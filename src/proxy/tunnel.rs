//! Splices a client's session channel onto a session on a builder pod.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use bytes::Bytes;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{Channel, ChannelId, ChannelMsg, ChannelReadHalf, ChannelWriteHalf, client, server};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::SshProxy;
use super::session::SessionResult;
use super::stream::CancellableStream;
use crate::duration::format as fmt_duration;
use crate::lock;

/// Client input held while the builder pod starts. Beyond these limits the
/// proxy stops reading the client channel and russh's flow control applies.
const PENDING_LIMIT_BYTES: usize = 1 << 20;
const PENDING_LIMIT_MESSAGES: usize = 1024;

/// Bound on writing a failure report to the client, so a client that stopped
/// reading cannot hold a session open.
const FAIL_CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on waiting for tunnel tasks after they were aborted.
const TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// Bounds for the builder dial retry. The builder pod is handed to the proxy as
/// soon as it has an IP, so the first attempts routinely land before sshd has
/// bound its port; retrying quickly is what turns the dial into the readiness
/// check. The cap keeps a slow-starting pod from being hammered.
const BUILDER_DIAL_RETRY_INITIAL: Duration = Duration::from_millis(10);
const BUILDER_DIAL_RETRY_MAX: Duration = Duration::from_millis(50);

/// The client end of a build session.
pub(crate) struct ClientSide {
    read: ChannelReadHalf,
    writer: Arc<ClientWriter>,
    pending: VecDeque<ChannelMsg>,
    pending_bytes: usize,
}

impl ClientSide {
    pub(crate) fn new(channel: Channel<server::Msg>, handle: server::Handle) -> Self {
        let id = channel.id();
        let (read, write) = channel.split();
        Self {
            read,
            writer: Arc::new(ClientWriter {
                write,
                handle,
                id,
                closed: AtomicBool::new(false),
            }),
            pending: VecDeque::new(),
            pending_bytes: 0,
        }
    }

    pub(crate) fn writer(&self) -> Arc<ClientWriter> {
        Arc::clone(&self.writer)
    }

    /// Drives `fut` to completion while buffering what the client sends in
    /// the meantime. Returns `None` if the client closes the channel (or its
    /// connection) first.
    pub(crate) async fn buffer_while<F: Future>(&mut self, fut: F) -> Option<F::Output> {
        tokio::pin!(fut);
        loop {
            let room = self.pending.len() < PENDING_LIMIT_MESSAGES
                && self.pending_bytes < PENDING_LIMIT_BYTES;
            tokio::select! {
                biased;
                out = &mut fut => return Some(out),
                msg = self.read.wait(), if room => match msg {
                    None | Some(ChannelMsg::Close) => return None,
                    Some(msg) => {
                        if let ChannelMsg::Data { data } = &msg {
                            self.pending_bytes += data.len();
                        }
                        self.pending.push_back(msg);
                    }
                },
            }
        }
    }
}

/// The writing end of the client channel, shared by the session and tunnel
/// tasks.
pub(crate) struct ClientWriter {
    write: ChannelWriteHalf<server::Msg>,
    handle: server::Handle,
    id: ChannelId,
    closed: AtomicBool,
}

impl ClientWriter {
    /// Reports a proxy-side failure to the Nix client in a way that shows up
    /// in its logs: the reason on stderr, then exit status 255 and EOF.
    pub(crate) async fn fail(&self, msg: &str) {
        let report = async {
            let text = format!("nix-remote-build-controller: {msg}\r\n");
            let _ = self.write.extended_data_bytes(1, Bytes::from(text)).await;
            let _ = self.write.exit_status(255).await;
            let _ = self.write.eof().await;
        };
        if tokio::time::timeout(FAIL_CLIENT_TIMEOUT, report)
            .await
            .is_err()
        {
            debug!("Timed out reporting failure to client");
        }
    }

    /// Closes the channel (once).
    pub(crate) async fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = tokio::time::timeout(FAIL_CLIENT_TIMEOUT, self.write.close()).await;
    }
}

impl SshProxy {
    /// Splices the client's session channel onto a new session on the builder
    /// pod and returns once the builder-side command has exited (or the
    /// client went away).
    ///
    /// Ordering matters for correctness:
    /// - Channel requests from the client (env, exec) are forwarded in the
    ///   order they arrived, and stdin is only forwarded once the
    ///   exec/shell/subsystem request has been relayed, so the builder's sshd
    ///   never sees data for a command that has not been started yet.
    /// - The client channel is closed only after the builder's output and
    ///   exit status have been forwarded, so Nix sees the real exit code.
    /// - Client disconnects cancel the tunnel, which severs the builder
    ///   connection and therefore kills the remote nix-store process.
    pub(crate) async fn route_to_builder(
        &self,
        client: ClientSide,
        pod_ip: &str,
        conn_token: &CancellationToken,
    ) -> SessionResult {
        let writer = client.writer();
        let ip: IpAddr = match pod_ip.parse() {
            Ok(ip) => ip,
            Err(_) => {
                let err = anyhow!("builder pod IP {pod_ip:?} is not an IP address");
                writer.fail(&err.to_string()).await;
                return SessionResult::failed(err);
            }
        };
        let addr = SocketAddr::new(ip, self.cfg.remote_port);

        let tunnel = conn_token.child_token();
        let _cancel_tunnel = tunnel.clone().drop_guard();

        let builder =
            match dial_builder(addr, &self.builder, self.cfg.builder_dial_timeout, &tunnel).await {
                Ok(builder) => builder,
                Err(err) => {
                    let err = anyhow!("connect to builder pod {addr}: {err}");
                    writer.fail(&err.to_string()).await;
                    return SessionResult::failed(err);
                }
            };
        let opened = tokio::time::timeout(
            self.cfg.builder_dial_timeout,
            builder.channel_open_session(),
        )
        .await;
        let channel = match opened.unwrap_or(Err(russh::Error::ConnectionTimeout)) {
            Ok(channel) => channel,
            Err(err) => {
                let err = anyhow!("open session channel on builder pod {addr}: {err}");
                writer.fail(&err.to_string()).await;
                return SessionResult::failed(err);
            }
        };

        info!(builder_addr = %addr, "Connected to builder pod, tunnelling session");
        let result = tunnel_session(client, channel, &tunnel, self.cfg.client_close_grace).await;
        // The builder connection lives until the tunnel is done.
        drop(builder);
        result
    }
}

/// State shared by the two tunnel directions.
struct Shared {
    tunnel: CancellationToken,
    exit_status: Mutex<Option<u32>>,
    copy_err: Mutex<Option<anyhow::Error>>,
    /// Kinds of the forwarded requests still awaiting the builder's reply.
    expected_replies: Mutex<VecDeque<&'static str>>,
}

impl Shared {
    fn record_copy_err(&self, direction: &str, err: russh::Error) {
        if self.tunnel.is_cancelled() {
            return;
        }
        let mut slot = lock(&self.copy_err);
        if slot.is_none() {
            *slot = Some(anyhow!("{direction}: {err}"));
        }
    }
}

/// How the builder side of a tunnel ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuilderEnd {
    /// The builder closed the channel: the remote command has exited.
    Closed,
    /// The builder connection went away without closing the channel.
    ConnectionLost,
    /// The builder refused to start the command.
    Rejected(&'static str),
}

async fn tunnel_session(
    client: ClientSide,
    builder: Channel<client::Msg>,
    tunnel: &CancellationToken,
    client_close_grace: Duration,
) -> SessionResult {
    let ClientSide {
        read: client_read,
        writer,
        pending,
        ..
    } = client;
    let (builder_read, builder_write) = builder.split();
    let shared = Arc::new(Shared {
        tunnel: tunnel.clone(),
        exit_status: Mutex::new(None),
        copy_err: Mutex::new(None),
        expected_replies: Mutex::new(VecDeque::new()),
    });

    let mut to_builder: JoinHandle<()> = tokio::spawn(client_to_builder(
        client_read,
        pending,
        builder_write,
        Arc::clone(&shared),
    ));
    let mut to_client: JoinHandle<BuilderEnd> = tokio::spawn(builder_to_client(
        builder_read,
        Arc::clone(&writer),
        Arc::clone(&shared),
    ));
    let mut to_builder_done = false;
    let mut to_client_done = false;

    // Wait for the session to end.
    let mut err: Option<anyhow::Error> = None;
    let mut builder_end = None;
    tokio::select! {
        biased;
        end = &mut to_client => {
            // Normal completion: the remote command exited and the channel
            // closed. Its output has been handed to the client already.
            to_client_done = true;
            builder_end = Some(end);
        }
        _ = &mut to_builder => {
            // The client closed its channel: EOF has been propagated to the
            // builder; give the remote command a moment to exit on its own.
            to_builder_done = true;
            tokio::select! {
                biased;
                end = &mut to_client => {
                    to_client_done = true;
                    builder_end = Some(end);
                }
                () = tunnel.cancelled() => err = Some(anyhow!("session cancelled")),
                () = tokio::time::sleep(client_close_grace) => {
                    warn!(
                        grace = %fmt_duration(client_close_grace),
                        "Builder did not exit after client closed the session; forcing teardown"
                    );
                    err = Some(anyhow!("client closed the session before the builder finished"));
                }
            }
        }
        () = tunnel.cancelled() => err = Some(anyhow!("session cancelled")),
    }

    let exit_status = *lock(&shared.exit_status);
    match builder_end {
        None | Some(Ok(BuilderEnd::Closed)) => {}
        Some(Ok(BuilderEnd::ConnectionLost)) => {
            if tunnel.is_cancelled() {
                err = Some(anyhow!("session cancelled"));
            } else if exit_status.is_none() {
                err = Some(anyhow!(
                    "builder connection closed before the remote command finished"
                ));
            }
        }
        Some(Ok(BuilderEnd::Rejected(kind))) => {
            let rejected = anyhow!("builder pod rejected the {kind} request");
            writer.fail(&rejected.to_string()).await;
            err = Some(rejected);
        }
        Some(Err(join_err)) => err = Some(anyhow!("tunnel task failed: {join_err}")),
    }

    writer.close().await;
    // Severs the builder connection and unblocks both directions.
    tunnel.cancel();
    to_builder.abort();
    to_client.abort();
    let finish = async {
        if !to_builder_done {
            let _ = (&mut to_builder).await;
        }
        if !to_client_done {
            let _ = (&mut to_client).await;
        }
    };
    if tokio::time::timeout(TASK_SHUTDOWN_TIMEOUT, finish)
        .await
        .is_err()
    {
        warn!("Tunnel tasks did not finish promptly");
    }

    let err = err.or_else(|| lock(&shared.copy_err).take());
    match (&err, exit_status) {
        (Some(err), Some(status)) => {
            warn!(error = %format!("{err:#}"), exit_status = status, "Tunnel closed")
        }
        (Some(err), None) => warn!(error = %format!("{err:#}"), "Tunnel closed"),
        (None, Some(status)) => info!(exit_status = status, "Tunnel closed"),
        (None, None) => info!("Tunnel closed"),
    }
    SessionResult { exit_status, err }
}

/// Stdin received before the remote command was started.
enum HeldInput {
    Data(Bytes),
    Eof,
}

/// Forwards requests and stdin from the client to the builder. Returns when
/// the client closes its channel or its connection goes away; in either case
/// the builder gets EOF on stdin so the remote command can finish on its own.
///
/// Stdin is written with the builder's flow control: while the builder is not
/// reading, this task waits, the client channel's buffer in russh fills, and
/// the client is slowed down in turn.
async fn client_to_builder(
    mut read: ChannelReadHalf,
    pending: VecDeque<ChannelMsg>,
    builder: ChannelWriteHalf<client::Msg>,
    shared: Arc<Shared>,
) {
    let mut pending = pending.into_iter();
    let mut started = false;
    let mut held: Vec<HeldInput> = Vec::new();
    let mut broken = false;
    let mut eof_sent = false;
    let mut stdin_bytes: u64 = 0;

    loop {
        let msg = match pending.next() {
            Some(msg) => msg,
            None => match read.wait().await {
                Some(msg) => msg,
                None => break,
            },
        };
        if matches!(msg, ChannelMsg::Close) {
            break;
        }
        if broken {
            // The builder side failed; keep draining until the client closes.
            continue;
        }

        let starts_command = matches!(
            msg,
            ChannelMsg::Exec { .. }
                | ChannelMsg::RequestShell { .. }
                | ChannelMsg::RequestSubsystem { .. }
        );
        let result = match msg {
            ChannelMsg::Data { data } if started => {
                stdin_bytes += data.len() as u64;
                builder.data_bytes(data).await
            }
            ChannelMsg::Data { data } => {
                held.push(HeldInput::Data(data));
                Ok(())
            }
            ChannelMsg::Eof if started => {
                debug!(bytes = stdin_bytes, "client->builder stdin copy finished");
                eof_sent = true;
                builder.eof().await
            }
            ChannelMsg::Eof => {
                held.push(HeldInput::Eof);
                Ok(())
            }
            ChannelMsg::Exec { command, .. } => {
                lock(&shared.expected_replies).push_back("exec");
                builder.exec(true, command).await
            }
            ChannelMsg::RequestShell { .. } => {
                lock(&shared.expected_replies).push_back("shell");
                builder.request_shell(true).await
            }
            ChannelMsg::RequestSubsystem { name, .. } => {
                lock(&shared.expected_replies).push_back("subsystem");
                builder.request_subsystem(true, name).await
            }
            ChannelMsg::SetEnv {
                variable_name,
                variable_value,
                ..
            } => builder.set_env(false, variable_name, variable_value).await,
            ChannelMsg::RequestPty {
                term,
                col_width,
                row_height,
                pix_width,
                pix_height,
                terminal_modes,
                ..
            } => {
                builder
                    .request_pty(
                        false,
                        &term,
                        col_width,
                        row_height,
                        pix_width,
                        pix_height,
                        &terminal_modes,
                    )
                    .await
            }
            ChannelMsg::WindowChange {
                col_width,
                row_height,
                pix_width,
                pix_height,
            } => {
                builder
                    .window_change(col_width, row_height, pix_width, pix_height)
                    .await
            }
            ChannelMsg::Signal { signal } => builder.signal(signal).await,
            other => {
                debug!(msg = ?other, "Ignoring client channel message");
                Ok(())
            }
        };

        let mut result = result;
        if result.is_ok() && starts_command && !started {
            started = true;
            for input in held.drain(..) {
                result = match input {
                    HeldInput::Data(data) => {
                        stdin_bytes += data.len() as u64;
                        builder.data_bytes(data).await
                    }
                    HeldInput::Eof => {
                        debug!(bytes = stdin_bytes, "client->builder stdin copy finished");
                        eof_sent = true;
                        builder.eof().await
                    }
                };
                if result.is_err() {
                    break;
                }
            }
        }
        if let Err(err) = result {
            shared.record_copy_err("client->builder", err);
            broken = true;
        }
    }

    // A client that closes without sending EOF has still stopped writing.
    if started && !eof_sent && !broken {
        debug!(
            bytes = stdin_bytes,
            "client->builder stdin closed without EOF"
        );
        let _ = builder.eof().await;
    }
}

/// Forwards output, exit status and request replies from the builder to the
/// client. Returns when the builder closes the channel or the connection to
/// it goes away.
async fn builder_to_client(
    mut read: ChannelReadHalf,
    client: Arc<ClientWriter>,
    shared: Arc<Shared>,
) -> BuilderEnd {
    let mut broken = false;
    let mut stdout_bytes: u64 = 0;
    let mut stderr_bytes: u64 = 0;
    loop {
        let Some(msg) = read.wait().await else {
            return BuilderEnd::ConnectionLost;
        };
        let result = match msg {
            ChannelMsg::Close => {
                debug!(stdout_bytes, stderr_bytes, "builder->client copy finished");
                return BuilderEnd::Closed;
            }
            ChannelMsg::Success => {
                lock(&shared.expected_replies).pop_front();
                Ok(())
            }
            ChannelMsg::Failure => {
                if let Some(kind) = lock(&shared.expected_replies).pop_front() {
                    return BuilderEnd::Rejected(kind);
                }
                Ok(())
            }
            ChannelMsg::ExitStatus { exit_status } => {
                *lock(&shared.exit_status) = Some(exit_status);
                if broken {
                    Ok(())
                } else {
                    client.write.exit_status(exit_status).await
                }
            }
            _ if broken => Ok(()),
            ChannelMsg::Data { data } => {
                stdout_bytes += data.len() as u64;
                client.write.data_bytes(data).await
            }
            ChannelMsg::ExtendedData { data, ext } => {
                stderr_bytes += data.len() as u64;
                client.write.extended_data_bytes(ext, data).await
            }
            ChannelMsg::Eof => client.write.eof().await,
            ChannelMsg::ExitSignal {
                signal_name,
                core_dumped,
                error_message,
                lang_tag,
            } => {
                warn!(signal = ?signal_name, error = %error_message, "Builder command terminated by signal");
                client
                    .handle
                    .exit_signal_request(
                        client.id,
                        signal_name,
                        core_dumped,
                        error_message,
                        lang_tag,
                    )
                    .await
                    .map_err(|()| russh::Error::SendError)
            }
            _ => Ok(()),
        };
        if let Err(err) = result {
            shared.record_copy_err("builder->client", err);
            broken = true;
        }
    }
}

/// How the proxy authenticates to builder pods.
pub(crate) struct BuilderAuth {
    pub(crate) user: String,
    pub(crate) key: Arc<PrivateKey>,
    /// The expected builder host key; `None` accepts any key.
    pub(crate) host_key: Option<PublicKey>,
    pub(crate) config: Arc<client::Config>,
}

/// russh client handler for builder connections: verifies the host key.
pub(crate) struct BuilderHandler {
    expected: Option<PublicKey>,
}

impl client::Handler for BuilderHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let Some(expected) = &self.expected else {
            // Pod-internal traffic without a configured builder host key; see
            // the README's Security section.
            return Ok(true);
        };
        Ok(match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.key_data() == expected.key_data(),
            _ => false,
        })
    }
}

/// Why a builder could not be reached.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DialError {
    #[error("dial tcp {addr}: {source}")]
    Connect { addr: SocketAddr, source: io::Error },
    #[error("ssh handshake: {0}")]
    Handshake(russh::Error),
    #[error("ssh: builder host key does not match the configured builder-host-key")]
    HostKeyMismatch,
    #[error("ssh: unable to authenticate as {0:?}: public key rejected")]
    AuthRejected(String),
    #[error("i/o timeout")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
    #[error("after {attempts} attempts over {timeout}: {last}")]
    GaveUp {
        attempts: u32,
        timeout: String,
        last: Box<DialError>,
    },
}

impl DialError {
    /// Whether this is the kind of failure a builder pod produces while it is
    /// still starting, as opposed to a real misconfiguration.
    pub(crate) fn is_builder_not_up_yet(&self) -> bool {
        match self {
            DialError::Connect { source, .. } => is_io_not_up_yet(source),
            // A pod whose sshd has bound but not finished starting can accept
            // and then drop the connection before the SSH banner is exchanged.
            DialError::Handshake(err) => match err {
                russh::Error::IO(io) => is_io_not_up_yet(io),
                russh::Error::Disconnect
                | russh::Error::HUP
                | russh::Error::ConnectionTimeout
                | russh::Error::Elapsed(_) => true,
                _ => false,
            },
            DialError::Timeout => true,
            _ => false,
        }
    }
}

fn is_io_not_up_yet(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::NotConnected
            | io::ErrorKind::TimedOut
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
    )
}

/// Connects to a builder pod, retrying while the pod is still coming up. The
/// controller publishes the pod IP before sshd is listening, so "connection
/// refused" is the expected state for the first few attempts rather than an
/// error. Failures that will not fix themselves (a rejected key, a host key
/// mismatch) are returned immediately instead of being retried until the
/// timeout.
pub(crate) async fn dial_builder(
    addr: SocketAddr,
    auth: &BuilderAuth,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<client::Handle<BuilderHandler>, DialError> {
    let deadline = Instant::now() + timeout;
    let mut backoff = BUILDER_DIAL_RETRY_INITIAL;
    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        let err = match dial_once(addr, auth, timeout, cancel).await {
            Ok(handle) => {
                if attempts > 1 {
                    debug!(attempts, addr = %addr, "Builder accepted connection after retrying");
                }
                return Ok(handle);
            }
            Err(err) => err,
        };
        if cancel.is_cancelled() || !err.is_builder_not_up_yet() {
            return Err(err);
        }
        if Instant::now() >= deadline {
            return Err(DialError::GaveUp {
                attempts,
                timeout: fmt_duration(timeout),
                last: Box::new(err),
            });
        }
        tokio::select! {
            () = cancel.cancelled() => return Err(err),
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BUILDER_DIAL_RETRY_MAX);
    }
}

/// One TCP + SSH handshake + authentication attempt, bounded by `timeout`.
/// The connection's transport is severed when `cancel` fires, or right away if
/// the attempt fails, so an abandoned handshake cannot linger.
async fn dial_once(
    addr: SocketAddr,
    auth: &BuilderAuth,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<client::Handle<BuilderHandler>, DialError> {
    let conn_token = cancel.child_token();
    let guard = conn_token.clone().drop_guard();
    let attempt = async {
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|source| DialError::Connect { addr, source })?;
        let _ = tcp.set_nodelay(true);
        let stream = CancellableStream::new(tcp, conn_token.clone());
        let handler = BuilderHandler {
            expected: auth.host_key.clone(),
        };
        let mut handle = client::connect_stream(Arc::clone(&auth.config), stream, handler)
            .await
            .map_err(|err| match err {
                russh::Error::UnknownKey => DialError::HostKeyMismatch,
                err => DialError::Handshake(err),
            })?;
        let hash_alg = if auth.key.algorithm().is_rsa() {
            handle
                .best_supported_rsa_hash()
                .await
                .map_err(DialError::Handshake)?
                .flatten()
        } else {
            None
        };
        let result = handle
            .authenticate_publickey(
                auth.user.clone(),
                PrivateKeyWithHashAlg::new(Arc::clone(&auth.key), hash_alg),
            )
            .await
            .map_err(DialError::Handshake)?;
        if !result.success() {
            return Err(DialError::AuthRejected(auth.user.clone()));
        }
        Ok(handle)
    };
    let result = tokio::select! {
        result = tokio::time::timeout(timeout, attempt) => result.unwrap_or(Err(DialError::Timeout)),
        () = cancel.cancelled() => Err(DialError::Cancelled),
    };
    if result.is_ok() {
        // The connection now lives until `cancel` fires.
        guard.disarm();
    }
    result
}
