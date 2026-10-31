//! Forwards a client's SSH session to a session on a builder pod.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::anyhow;
use bytes::Bytes;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{Channel, ChannelId, ChannelMsg, ChannelReadHalf, ChannelWriteHalf, client, server};
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use common::duration;

use crate::SshProxy;
use crate::config::Config;
use crate::metrics::{Direction, Metrics};
use crate::session::SessionResult;
use crate::stream::CancellableStream;

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
    /// Forwards the client's session to a new session on the builder pod.
    /// Returns when the command on the builder has exited or the client has
    /// gone.
    ///
    /// Ordering matters for correctness:
    /// - Channel requests from the client (env, exec) are forwarded in the
    ///   order they arrived, and stdin is only forwarded once the
    ///   exec/shell/subsystem request has been relayed, so the builder's sshd
    ///   never sees data for a command that has not been started yet.
    /// - The client channel is closed only after the builder's output and
    ///   exit status have been forwarded, so Nix sees the real exit code.
    /// - Client disconnects cancel the tunnel, which closes the builder
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

        let dial_start = Instant::now();
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

        self.metrics.builder_dialed(dial_start.elapsed());
        info!(builder_addr = %addr, "Connected to builder pod, tunnelling session");
        let result = tunnel_session(client, channel, &tunnel, &self.cfg, &self.metrics).await;
        // The builder connection lives until the tunnel is done.
        drop(builder);
        result
    }
}

/// Client input that may wait in the proxy for the builder to read it.
///
/// russh grants the client more send window as soon as data arrives, not when
/// it is consumed. Real flow control towards the client therefore only happens
/// when the proxy stops reading the client channel, and that also stops all
/// other traffic on the client's connection, including the builder's output.
/// If the builder is busy writing output and not reading input, stopping would
/// deadlock both directions. So:
///
/// - Up to `INPUT_SOFT_LIMIT` is buffered; beyond that the proxy stops reading
///   and the client is slowed down (a builder with a slow disk, for example).
/// - While output to the client is in progress, reading continues up to
///   `INPUT_HARD_LIMIT`, so the output can always drain.
/// - If the builder makes no progress at all for `input_stall_timeout`, reading
///   also continues up to `INPUT_HARD_LIMIT`. A stopped connection cannot see
///   the client leave; reading again lets the proxy notice it and end the
///   session.
/// - Beyond `INPUT_HARD_LIMIT` in either case, the session fails instead of
///   hanging. Nix never sends large input that the builder does not read, so
///   only abnormal sessions reach this.
const INPUT_SOFT_LIMIT: usize = 4 << 20;
const INPUT_HARD_LIMIT: usize = 128 << 20;

/// State shared by the tunnel tasks.
struct Shared {
    tunnel: CancellationToken,
    exit_status: Mutex<Option<u32>>,
    copy_err: Mutex<Option<anyhow::Error>>,
    /// Kinds of the forwarded requests still awaiting the builder's reply.
    expected_replies: Mutex<VecDeque<&'static str>>,
    /// Client input read but not yet written to the builder, in bytes.
    input_buffered: AtomicUsize,
    /// Set while output is being written to the client.
    output_busy: AtomicBool,
    /// Wakes the client reader when either of the above changes.
    input_room: Notify,
    /// When the builder last read input or the client last received output.
    last_progress: Mutex<Instant>,
    /// No progress for this long counts as a stalled builder.
    stall_timeout: Duration,
    metrics: Metrics,
}

impl Shared {
    fn new(tunnel: CancellationToken, stall_timeout: Duration, metrics: Metrics) -> Self {
        Self {
            tunnel,
            exit_status: Mutex::new(None),
            copy_err: Mutex::new(None),
            expected_replies: Mutex::new(VecDeque::new()),
            input_buffered: AtomicUsize::new(0),
            output_busy: AtomicBool::new(false),
            input_room: Notify::new(),
            last_progress: Mutex::new(Instant::now()),
            stall_timeout,
            metrics,
        }
    }

    fn record_copy_err(&self, direction: &str, err: russh::Error) {
        if self.tunnel.is_cancelled() {
            return;
        }
        let mut slot = self.copy_err.lock().expect("mutex poisoned");
        if slot.is_none() {
            *slot = Some(anyhow!("{direction}: {err}"));
        }
    }

    /// Waits until more client input may be read. `false` means
    /// `INPUT_HARD_LIMIT` was reached while output was in progress or the
    /// builder was stalled.
    async fn wait_for_input_room(&self) -> bool {
        loop {
            let room = self.input_room.notified();
            tokio::pin!(room);
            // Register for wake-ups before checking, so none is missed.
            room.as_mut().enable();
            let buffered = self.input_buffered.load(Ordering::SeqCst);
            if buffered < INPUT_SOFT_LIMIT {
                return true;
            }
            let stalled_at =
                *self.last_progress.lock().expect("mutex poisoned") + self.stall_timeout;
            if self.output_busy.load(Ordering::SeqCst) || Instant::now() >= stalled_at {
                return buffered < INPUT_HARD_LIMIT;
            }
            tokio::select! {
                () = room => {}
                () = tokio::time::sleep_until(stalled_at) => {}
            }
        }
    }

    /// Notes that a forwarded request of `kind` awaits the builder's reply.
    fn expect_reply(&self, kind: &'static str) {
        self.expected_replies
            .lock()
            .expect("mutex poisoned")
            .push_back(kind);
    }

    /// Takes the oldest request still awaiting the builder's reply.
    fn take_expected_reply(&self) -> Option<&'static str> {
        self.expected_replies
            .lock()
            .expect("mutex poisoned")
            .pop_front()
    }

    fn progressed(&self) {
        *self.last_progress.lock().expect("mutex poisoned") = Instant::now();
    }

    fn input_read(&self, msg: &ChannelMsg) {
        if let ChannelMsg::Data { data } = msg {
            self.input_buffered.fetch_add(data.len(), Ordering::SeqCst);
        }
    }

    fn input_written(&self, len: usize) {
        self.input_buffered.fetch_sub(len, Ordering::SeqCst);
        self.progressed();
        self.input_room.notify_waiters();
    }

    /// Runs one write to the client, marked as output in progress.
    async fn write_output<T>(&self, write: impl Future<Output = T>) -> T {
        self.output_busy.store(true, Ordering::SeqCst);
        self.input_room.notify_waiters();
        let result = write.await;
        self.output_busy.store(false, Ordering::SeqCst);
        self.progressed();
        result
    }
}

/// How the client side of a tunnel ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientEnd {
    /// The client closed its channel or its connection.
    Closed,
    /// The client sent more input than the builder read (`INPUT_HARD_LIMIT`).
    InputOverflow,
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
    cfg: &Config,
    metrics: &Metrics,
) -> SessionResult {
    let client_close_grace = cfg.client_close_grace;
    let ClientSide {
        read: client_read,
        writer,
        pending,
        ..
    } = client;
    let (builder_read, builder_write) = builder.split();
    let shared = Arc::new(Shared::new(
        tunnel.clone(),
        cfg.input_stall_timeout,
        metrics.clone(),
    ));
    let (input_tx, input_rx) = mpsc::unbounded_channel();

    let mut from_client: JoinHandle<ClientEnd> = tokio::spawn(read_client(
        client_read,
        pending,
        input_tx,
        Arc::clone(&shared),
    ));
    let to_builder: JoinHandle<()> =
        tokio::spawn(write_builder(input_rx, builder_write, Arc::clone(&shared)));
    let mut to_client: JoinHandle<BuilderEnd> = tokio::spawn(builder_to_client(
        builder_read,
        Arc::clone(&writer),
        Arc::clone(&shared),
    ));
    let mut from_client_done = false;
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
        end = &mut from_client => {
            from_client_done = true;
            match end {
                Ok(ClientEnd::InputOverflow) => {
                    let overflow = anyhow!(
                        "client sent more than {} MiB of input that the builder did not read",
                        INPUT_HARD_LIMIT >> 20
                    );
                    writer.fail(&overflow.to_string()).await;
                    err = Some(overflow);
                }
                // The client closed its channel: EOF has been propagated to
                // the builder; give the remote command a moment to exit.
                _ => tokio::select! {
                    biased;
                    end = &mut to_client => {
                        to_client_done = true;
                        builder_end = Some(end);
                    }
                    () = tunnel.cancelled() => err = Some(anyhow!("session cancelled")),
                    () = tokio::time::sleep(client_close_grace) => {
                        warn!(
                            grace = %duration::format(client_close_grace),
                            "Builder did not exit after client closed the session; forcing teardown"
                        );
                        err = Some(anyhow!("client closed the session before the builder finished"));
                    }
                },
            }
        }
        () = tunnel.cancelled() => err = Some(anyhow!("session cancelled")),
    }

    let exit_status = *shared.exit_status.lock().expect("mutex poisoned");
    match builder_end {
        None | Some(Ok(BuilderEnd::Closed)) => {}
        Some(Ok(BuilderEnd::ConnectionLost)) => {
            if tunnel.is_cancelled() {
                err = Some(anyhow!("session cancelled"));
            } else if exit_status.is_none() {
                let lost = anyhow!("builder connection closed before the remote command finished");
                writer.fail(&lost.to_string()).await;
                err = Some(lost);
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
    // Severs the builder connection and unblocks every task.
    tunnel.cancel();
    from_client.abort();
    to_builder.abort();
    to_client.abort();
    let finish = async {
        if !from_client_done {
            let _ = (&mut from_client).await;
        }
        let _ = to_builder.await;
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

    let err = err.or_else(|| shared.copy_err.lock().expect("mutex poisoned").take());
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

/// Reads the client channel and queues everything for [`write_builder`],
/// within the input limits. Returns when the client closes its channel or its
/// connection goes away; dropping the queue then tells the writer.
async fn read_client(
    mut read: ChannelReadHalf,
    pending: VecDeque<ChannelMsg>,
    queue: mpsc::UnboundedSender<ChannelMsg>,
    shared: Arc<Shared>,
) -> ClientEnd {
    for msg in pending {
        shared.input_read(&msg);
        if queue.send(msg).is_err() {
            return ClientEnd::Closed;
        }
    }
    loop {
        if !shared.wait_for_input_room().await {
            return ClientEnd::InputOverflow;
        }
        let msg = match read.wait().await {
            None | Some(ChannelMsg::Close) => return ClientEnd::Closed,
            Some(msg) => msg,
        };
        shared.input_read(&msg);
        if queue.send(msg).is_err() {
            return ClientEnd::Closed;
        }
    }
}

/// Stdin received before the remote command was started.
enum HeldInput {
    Data(Bytes),
    Eof,
}

/// Forwards the client's requests and stdin to the builder, in order. Stdin
/// waits until the exec/shell/subsystem request has been sent. When the client
/// is gone the builder gets EOF on stdin, so the remote command can finish on
/// its own.
async fn write_builder(
    mut queue: mpsc::UnboundedReceiver<ChannelMsg>,
    builder: ChannelWriteHalf<client::Msg>,
    shared: Arc<Shared>,
) {
    let mut started = false;
    let mut held: Vec<HeldInput> = Vec::new();
    let mut broken = false;
    let mut eof_sent = false;
    let mut stdin_bytes: u64 = 0;

    while let Some(msg) = queue.recv().await {
        if broken {
            // The builder side failed; discard input until the client is gone.
            if let ChannelMsg::Data { data } = &msg {
                shared.input_written(data.len());
            }
            continue;
        }

        let starts_command = matches!(
            msg,
            ChannelMsg::Exec { .. }
                | ChannelMsg::RequestShell { .. }
                | ChannelMsg::RequestSubsystem { .. }
        );
        let mut result = match msg {
            ChannelMsg::Data { data } if started => {
                let len = data.len();
                stdin_bytes += len as u64;
                let sent = builder.data_bytes(data).await;
                shared.metrics.tunnelled(Direction::ClientToBuilder, len);
                shared.input_written(len);
                sent
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
                shared.expect_reply("exec");
                builder.exec(true, command).await
            }
            ChannelMsg::RequestShell { .. } => {
                shared.expect_reply("shell");
                builder.request_shell(true).await
            }
            ChannelMsg::RequestSubsystem { name, .. } => {
                shared.expect_reply("subsystem");
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

        if result.is_ok() && starts_command && !started {
            started = true;
            for input in held.drain(..) {
                result = match input {
                    HeldInput::Data(data) => {
                        let len = data.len();
                        stdin_bytes += len as u64;
                        let sent = builder.data_bytes(data).await;
                        shared.metrics.tunnelled(Direction::ClientToBuilder, len);
                        shared.input_written(len);
                        sent
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
            // Held input will never be sent; release its share of the limit.
            for input in held.drain(..) {
                if let HeldInput::Data(data) = input {
                    shared.input_written(data.len());
                }
            }
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
                shared.take_expected_reply();
                Ok(())
            }
            ChannelMsg::Failure => {
                if let Some(kind) = shared.take_expected_reply() {
                    return BuilderEnd::Rejected(kind);
                }
                Ok(())
            }
            ChannelMsg::ExitStatus { exit_status } => {
                *shared.exit_status.lock().expect("mutex poisoned") = Some(exit_status);
                if broken {
                    Ok(())
                } else {
                    client.write.exit_status(exit_status).await
                }
            }
            _ if broken => Ok(()),
            ChannelMsg::Data { data } => {
                stdout_bytes += data.len() as u64;
                shared
                    .metrics
                    .tunnelled(Direction::BuilderToClient, data.len());
                shared.write_output(client.write.data_bytes(data)).await
            }
            ChannelMsg::ExtendedData { data, ext } => {
                stderr_bytes += data.len() as u64;
                shared
                    .metrics
                    .tunnelled(Direction::BuilderToClient, data.len());
                shared
                    .write_output(client.write.extended_data_bytes(ext, data))
                    .await
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
#[must_use]
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
        let remaining = deadline.saturating_duration_since(Instant::now());
        let err = match dial_once(addr, auth, remaining, cancel).await {
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
                timeout: duration::format(timeout),
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
/// The connection is closed when `cancel` fires, or right away if the attempt
/// fails, so an abandoned handshake cannot linger.
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn shared_with_input(bytes: usize) -> Arc<Shared> {
        let shared = Arc::new(Shared::new(
            CancellationToken::new(),
            Duration::from_secs(60),
            Metrics::new(&opentelemetry::global::meter("test")),
        ));
        shared.input_buffered.store(bytes, Ordering::SeqCst);
        shared
    }

    async fn has_room_now(shared: &Shared) -> Option<bool> {
        tokio::time::timeout(Duration::from_millis(50), shared.wait_for_input_room())
            .await
            .ok()
    }

    #[tokio::test]
    async fn input_below_soft_limit_is_read() {
        let shared = shared_with_input(INPUT_SOFT_LIMIT - 1);
        assert_eq!(has_room_now(&shared).await, Some(true));
    }

    #[tokio::test]
    async fn input_waits_at_soft_limit_until_the_builder_reads() {
        let shared = shared_with_input(INPUT_SOFT_LIMIT);
        assert_eq!(
            has_room_now(&shared).await,
            None,
            "must wait while output is idle"
        );

        let waiter = {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move { shared.wait_for_input_room().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        shared.input_written(1);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .unwrap()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn input_keeps_flowing_while_output_is_written() {
        let shared = shared_with_input(INPUT_SOFT_LIMIT);
        let waiter = {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move { shared.wait_for_input_room().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Output that cannot complete yet keeps the reader going.
        let output = {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move { shared.write_output(std::future::pending::<()>()).await })
        };
        assert!(
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .unwrap()
                .unwrap()
        );
        output.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn input_keeps_flowing_once_the_builder_stalls() {
        let shared = shared_with_input(INPUT_SOFT_LIMIT);
        let waiter = {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move { shared.wait_for_input_room().await })
        };
        tokio::time::sleep(Duration::from_secs(59)).await;
        assert!(
            !waiter.is_finished(),
            "a builder is not stalled before the timeout"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            waiter.await.unwrap(),
            "a stalled builder lets input flow again"
        );

        // Any progress restarts the clock.
        shared.progressed();
        assert_eq!(has_room_now(&shared).await, None);
        // Past the hard limit, a stalled builder ends the session.
        shared
            .input_buffered
            .store(INPUT_HARD_LIMIT, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(61)).await;
        assert_eq!(has_room_now(&shared).await, Some(false));
    }

    #[tokio::test]
    async fn input_overflow_during_output_is_reported() {
        let shared = shared_with_input(INPUT_HARD_LIMIT);
        shared.output_busy.store(true, Ordering::SeqCst);
        assert_eq!(has_room_now(&shared).await, Some(false));
        // Without output in progress the reader just waits for the builder.
        shared.output_busy.store(false, Ordering::SeqCst);
        assert_eq!(has_room_now(&shared).await, None);
    }
}
