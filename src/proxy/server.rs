//! The SSH server: accepts client connections and turns every session channel
//! into a build session.

use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use axum::http::StatusCode;
use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, PublicKeyOffer, VerifiedPublicKey};
use russh::{Channel, ChannelId, ChannelOpenFailure, MethodKind, MethodSet, SshId};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{Instrument, debug, error, error_span, info, warn};

use super::auth::{ClientAuthorizer, fingerprint};
use super::config::Config;
use super::keys::{generate_host_key, load_private_key, private_key_from_secret};
use super::session::{BuildRequestClient, KubeBuildRequestClient, Session, SessionResult};
use super::stream::CancellableStream;
use super::tunnel::{BuilderAuth, ClientSide};
use crate::duration::format as fmt_duration;
use crate::http::HttpServer;
use crate::{lock, secret};

/// How long to wait for sessions after they were force-cancelled at the end of
/// the drain window.
const FORCED_SHUTDOWN_WAIT: Duration = Duration::from_secs(15);
/// While draining, how often a connection checks whether it has become idle.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// Accepts SSH connections from Nix clients and routes every session channel
/// to a dynamically provisioned builder pod.
pub struct SshProxy {
    pub(crate) cfg: Config,
    pub(crate) kube: Arc<dyn BuildRequestClient>,
    pub(crate) builder: BuilderAuth,
    listener: Mutex<Option<TcpListener>>,
    local_addr: SocketAddr,
    ssh_config: Arc<server::Config>,
    client_auth: Option<ClientAuthorizer>,
    health: tokio::sync::Mutex<Option<HttpServer>>,
    /// Cancelled when shutdown starts: no new sessions, idle connections close.
    draining: CancellationToken,
    sessions: Arc<Mutex<HashSet<String>>>,
}

impl SshProxy {
    /// Creates a proxy listening on `cfg.listen_addr`.
    ///
    /// `host_key` is presented to Nix clients. `builder_client_key`
    /// authenticates the proxy to builder pods. Without `client_auth` any
    /// client may connect, which is only acceptable when network policy
    /// restricts who can reach the proxy. Without `builder_host_key` any
    /// builder host key is accepted.
    pub async fn new(
        cfg: Config,
        kube: Arc<dyn BuildRequestClient>,
        host_key: PrivateKey,
        builder_client_key: PrivateKey,
        client_auth: Option<ClientAuthorizer>,
        builder_host_key: Option<PublicKey>,
    ) -> anyhow::Result<Arc<Self>> {
        let cfg = cfg.with_defaults();
        let keepalive = (!cfg.keepalive_interval.is_zero()).then_some(cfg.keepalive_interval);

        let methods = if client_auth.is_some() {
            MethodSet::from(&[MethodKind::PublicKey][..])
        } else {
            MethodSet::from(&[MethodKind::None, MethodKind::PublicKey][..])
        };
        let ssh_config = server::Config {
            server_id: SshId::Standard("SSH-2.0-nix-remote-build-proxy".into()),
            methods,
            // Keys cannot be guessed, so there is nothing to gain from slowing
            // down rejections, and OpenSSH probes "none" before every login.
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![host_key],
            // Builds can be silent for a long time, so idle connections stay
            // open. Dead clients are found with TCP keepalives instead (see
            // `set_tcp_keepalive`).
            inactivity_timeout: None,
            keepalive_interval: None,
            nodelay: true,
            ..Default::default()
        };
        let builder_ssh_config = russh::client::Config {
            inactivity_timeout: None,
            keepalive_interval: keepalive,
            // The connection is dead if a keepalive goes unanswered for a
            // full interval.
            keepalive_max: 1,
            nodelay: true,
            ..Default::default()
        };

        let listener = crate::http::bind(&cfg.listen_addr).await?;
        let local_addr = listener.local_addr()?;
        let draining = CancellationToken::new();
        let health = if cfg.health_addr.is_empty() {
            None
        } else {
            Some(start_health_server(&cfg.health_addr, draining.clone()).await?)
        };

        info!(
            address = %local_addr,
            client_auth = client_auth.is_some(),
            builder_host_key_verification = builder_host_key.is_some(),
            max_sessions = cfg.max_sessions,
            pod_ready_timeout = %fmt_duration(cfg.pod_ready_timeout),
            shutdown_timeout = %fmt_duration(cfg.shutdown_timeout),
            "SSH proxy listening"
        );

        Ok(Arc::new(Self {
            builder: BuilderAuth {
                user: cfg.remote_user.clone(),
                key: Arc::new(builder_client_key),
                host_key: builder_host_key,
                config: Arc::new(builder_ssh_config),
            },
            cfg,
            kube,
            listener: Mutex::new(Some(listener)),
            local_addr,
            ssh_config: Arc::new(ssh_config),
            client_auth,
            health: tokio::sync::Mutex::new(health),
            draining,
            sessions: Arc::new(Mutex::new(HashSet::new())),
        }))
    }

    /// Creates a proxy using in-cluster (or kubeconfig) credentials, loading
    /// all key material from the SSH key secret unless overridden by files.
    pub async fn from_cluster(
        cfg: Config,
        host_key_path: Option<&Path>,
        ssh_key_secret: &str,
        client_authorized_keys_path: Option<&Path>,
    ) -> anyhow::Result<Arc<Self>> {
        let cfg = cfg.with_defaults();
        let client = Client::try_default()
            .await
            .context("get Kubernetes config")?;

        let secret = Api::<Secret>::namespaced(client.clone(), &cfg.namespace)
            .get(ssh_key_secret)
            .await
            .with_context(|| format!("get secret {}/{ssh_key_secret}", cfg.namespace))?;

        let builder_client_key = private_key_from_secret(&secret, secret::PRIVATE_KEY)?
            .ok_or_else(|| {
                anyhow!(
                    "secret {ssh_key_secret} is missing required key {:?}",
                    secret::PRIVATE_KEY
                )
            })?;
        info!(
            secret = ssh_key_secret,
            "Loaded builder SSH client key from secret"
        );

        let host_key = match host_key_path {
            Some(path) => {
                let key = load_private_key(path)
                    .with_context(|| format!("load host key from {}", path.display()))?;
                info!(path = %path.display(), "Loaded SSH host key from file");
                key
            }
            None => match private_key_from_secret(&secret, secret::HOST_KEY)? {
                Some(key) => {
                    info!(secret = ssh_key_secret, "Loaded SSH host key from secret");
                    key
                }
                None => {
                    warn!(
                        "No {key:?} entry in secret {ssh_key_secret}; generating an ephemeral host key \
                         (clients will see a changed host key after every restart)",
                        key = secret::HOST_KEY
                    );
                    generate_host_key().context("generate host key")?
                }
            },
        };

        let builder_host_key = match private_key_from_secret(&secret, secret::BUILDER_HOST_KEY)? {
            Some(key) => {
                let public = key.public_key().clone();
                info!(fingerprint = %fingerprint(&public), "Builder host key verification enabled");
                Some(public)
            }
            None => {
                warn!(
                    "No {key:?} entry in secret {ssh_key_secret}; builder host keys will not be verified",
                    key = secret::BUILDER_HOST_KEY
                );
                None
            }
        };

        let client_auth = if let Some(path) = client_authorized_keys_path {
            let auth = ClientAuthorizer::load(path)?;
            info!(path = %path.display(), keys = auth.len(), "Client public key authentication enabled");
            Some(auth)
        } else if let Some(data) = secret::entry(&secret, secret::CLIENT_AUTHORIZED_KEYS) {
            let auth = ClientAuthorizer::parse(data).with_context(|| {
                format!(
                    "parse {:?} from secret {ssh_key_secret}",
                    secret::CLIENT_AUTHORIZED_KEYS
                )
            })?;
            info!(
                secret = ssh_key_secret,
                keys = auth.len(),
                "Client public key authentication enabled"
            );
            Some(auth)
        } else {
            warn!(
                "Client authentication is DISABLED: anyone who can reach the proxy can start builder pods and run \
                 commands in them. Add a \"client-authorized-keys\" entry to the SSH key secret or pass \
                 --client-authorized-keys."
            );
            None
        };

        Self::new(
            cfg,
            Arc::new(KubeBuildRequestClient::new(client)),
            host_key,
            builder_client_key,
            client_auth,
            builder_host_key,
        )
        .await
    }

    /// The address the SSH server is listening on.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The address of the health server, if it is running.
    pub async fn health_addr(&self) -> Option<SocketAddr> {
        self.health
            .lock()
            .await
            .as_ref()
            .map(HttpServer::local_addr)
    }

    /// The number of build sessions currently in flight.
    pub fn session_count(&self) -> usize {
        lock(&self.sessions).len()
    }

    /// Serves connections until `shutdown` is cancelled, then drains in-flight
    /// connections for up to the configured shutdown timeout. Returns `Ok`
    /// after a clean shutdown and an error if the listener failed.
    pub async fn run(self: &Arc<Self>, shutdown: CancellationToken) -> anyhow::Result<()> {
        let listener = lock(&self.listener)
            .take()
            .context("SSH proxy is already running")?;

        // Sessions deliberately do not inherit `shutdown`: a shutdown signal
        // must stop new connections but let running builds finish within the
        // drain window. `force` ends them when that window runs out.
        let force = CancellationToken::new();
        let connections = TaskTracker::new();
        let mut result = Ok(());

        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        let proxy = Arc::clone(self);
                        let force = force.clone();
                        connections.spawn(async move { proxy.handle_connection(stream, peer, force).await });
                    }
                    Err(err) if is_per_connection_accept_error(&err) => {
                        debug!(error = %err, "Accept failed for one connection");
                    }
                    Err(err) if is_resource_exhaustion(&err) => {
                        // Out of file descriptors or memory: back off instead
                        // of spinning, and keep serving what is in flight.
                        error!(error = %err, "Accept failed, retrying");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(err) => {
                        error!(error = %err, "Listener failed, draining active sessions");
                        result = Err(anyhow!("accept: {err}"));
                        break;
                    }
                },
            }
        }

        self.draining.cancel();
        drop(listener);
        self.drain(&connections, &force).await;
        if let Some(health) = self.health.lock().await.take() {
            health.shutdown().await;
        }
        result
    }

    /// Waits for running builds to finish, up to the shutdown timeout, then
    /// force-cancels any that remain. Connections without a running build
    /// close on their own once draining starts. The listener must already be
    /// closed.
    async fn drain(&self, connections: &TaskTracker, force: &CancellationToken) {
        info!(
            active_sessions = self.session_count(),
            timeout = %fmt_duration(self.cfg.shutdown_timeout),
            "Not accepting new connections, draining active sessions"
        );
        connections.close();
        tokio::select! {
            () = connections.wait() => info!("All sessions completed"),
            () = tokio::time::sleep(self.cfg.shutdown_timeout) => {
                warn!(
                    active_sessions = self.session_count(),
                    "Drain timeout reached, forcibly closing remaining sessions"
                );
                force.cancel();
                if tokio::time::timeout(FORCED_SHUTDOWN_WAIT, connections.wait()).await.is_err() {
                    error!("Sessions did not terminate after forced cancellation");
                }
            }
        }
    }

    /// Serves one SSH transport connection. Every session channel on it becomes
    /// an independent build session.
    async fn handle_connection(
        self: Arc<Self>,
        tcp: TcpStream,
        peer: SocketAddr,
        force: CancellationToken,
    ) {
        let conn_id = short_id();
        let span = error_span!("conn", conn_id = %conn_id, client_addr = %peer);
        async move {
            let _ = tcp.set_nodelay(true);
            if !self.cfg.keepalive_interval.is_zero()
                && let Err(err) = set_tcp_keepalive(&tcp, self.cfg.keepalive_interval)
            {
                debug!(error = %err, "Failed to enable TCP keepalives");
            }
            // Cancelled when the client goes away or the proxy is force-shut
            // down; every session on this connection watches it, and it closes
            // the transport underneath russh.
            let conn_token = force.child_token();
            let stream = CancellableStream::new(tcp, conn_token.clone());
            let channels = TaskTracker::new();
            let (auth_tx, auth_rx) = oneshot::channel();
            let handler = ConnHandler {
                proxy: Arc::clone(&self),
                conn_id,
                peer,
                conn_token: conn_token.clone(),
                channels: channels.clone(),
                auth_tx: Some(auth_tx),
                user: String::new(),
                client_key: None,
            };

            let handshake_deadline = tokio::time::sleep(self.cfg.handshake_timeout);
            tokio::pin!(handshake_deadline);

            let running = tokio::select! {
                started = server::run_stream(Arc::clone(&self.ssh_config), stream, handler) => match started {
                    Ok(running) => running,
                    Err(err) => {
                        conn_token.cancel();
                        warn!(error = %err, "SSH handshake failed");
                        return;
                    }
                },
                () = &mut handshake_deadline => {
                    conn_token.cancel();
                    warn!("SSH handshake failed: timed out");
                    return;
                }
            };
            tokio::pin!(running);

            let auth = tokio::select! {
                auth = auth_rx => auth.ok(),
                ended = &mut running => {
                    conn_token.cancel();
                    match ended {
                        Ok(()) => warn!("SSH handshake failed: client disconnected"),
                        Err(err) => warn!(error = %err, "SSH handshake failed"),
                    }
                    return;
                }
                () = &mut handshake_deadline => {
                    conn_token.cancel();
                    let _ = (&mut running).await;
                    warn!("SSH handshake failed: timed out");
                    return;
                }
                () = self.draining.cancelled() => {
                    conn_token.cancel();
                    let _ = (&mut running).await;
                    debug!("Closed connection during shutdown before login");
                    return;
                }
            };
            let Some(auth) = auth else {
                // The handler was dropped without authenticating: the session
                // task has ended.
                conn_token.cancel();
                if let Err(err) = (&mut running).await {
                    warn!(error = %err, "SSH handshake failed");
                }
                return;
            };
            info!(
                user = %auth.user,
                client_version = %auth.client_version,
                client_key = auth.client_key.as_deref().unwrap_or_default(),
                "New SSH connection"
            );

            // During shutdown a connection without a running build is closed,
            // so idle connections (an SSH ControlMaster, for example) do not
            // hold up the drain.
            let idle_while_draining = async {
                self.draining.cancelled().await;
                while !channels.is_empty() {
                    tokio::time::sleep(IDLE_CHECK_INTERVAL).await;
                }
            };
            tokio::select! {
                ended = &mut running => {
                    if let Err(err) = ended
                        && !conn_token.is_cancelled()
                    {
                        debug!(error = %err, "SSH connection ended with an error");
                    }
                }
                () = idle_while_draining => {
                    info!("Closing idle connection during shutdown");
                    conn_token.cancel();
                    let _ = (&mut running).await;
                }
            }
            conn_token.cancel();
            channels.close();
            channels.wait().await;
            info!("SSH connection closed");
        }
        .instrument(span)
        .await;
    }

    /// Reserves a session slot. The reservation is atomic with the
    /// concurrent-session count, so `max_sessions` is a hard cap even when many
    /// channels open at once. `None` when the cap is reached.
    fn track_session(&self, id: &str) -> Option<SessionGuard> {
        let mut sessions = lock(&self.sessions);
        if self.cfg.max_sessions > 0 && sessions.len() >= self.cfg.max_sessions {
            return None;
        }
        sessions.insert(id.to_string());
        Some(SessionGuard {
            sessions: Arc::clone(&self.sessions),
            id: id.to_string(),
        })
    }

    /// Runs one build session on a freshly accepted channel.
    async fn handle_channel(
        self: Arc<Self>,
        mut session: Session,
        guard: SessionGuard,
        channel: Channel<Msg>,
        handle: server::Handle,
        conn_token: CancellationToken,
    ) {
        info!(
            active_sessions = self.session_count(),
            "Build session opened"
        );
        let client = ClientSide::new(channel, handle);
        let writer = client.writer();

        let result = self.run_session(&mut session, client, &conn_token).await;
        writer.close().await;
        self.finalize_build_request(&session, &result).await;
        drop(guard);

        info!(
            succeeded = result.err.is_none(),
            duration = %fmt_duration(session.start.elapsed()),
            "Build session closed"
        );
    }

    async fn run_session(
        &self,
        session: &mut Session,
        mut client: ClientSide,
        conn_token: &CancellationToken,
    ) -> SessionResult {
        if let Err(err) = self.create_build_request(session).await {
            error!(error = %format!("{err:#}"), "Failed to create NixBuildRequest");
            client
                .writer()
                .fail(&format!("failed to create NixBuildRequest: {err:#}"))
                .await;
            return SessionResult::failed(err);
        }
        session.created = true;

        // The client starts talking (exec, then the nix-store handshake) right
        // away; hold that input until the builder is reachable, and notice if
        // the client gives up in the meantime.
        let pod_ip = match client
            .buffer_while(self.wait_for_builder_pod(session, conn_token))
            .await
        {
            Some(Ok(ip)) => ip,
            Some(Err(err)) => {
                error!(error = %format!("{err:#}"), "Builder pod did not become ready");
                client
                    .writer()
                    .fail(&format!("builder pod did not become ready: {err:#}"))
                    .await;
                return SessionResult::failed(err);
            }
            None => {
                info!("Client closed the session before the builder pod was ready");
                return SessionResult::failed(anyhow!(
                    "client closed the session before the builder pod was ready"
                ));
            }
        };

        self.route_to_builder(client, &pod_ip, conn_token).await
    }
}

/// Removes a session from the active set when dropped.
struct SessionGuard {
    sessions: Arc<Mutex<HashSet<String>>>,
    id: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        lock(&self.sessions).remove(&self.id);
    }
}

struct AuthInfo {
    user: String,
    client_version: String,
    client_key: Option<String>,
}

/// The russh handler for one client connection.
struct ConnHandler {
    proxy: Arc<SshProxy>,
    conn_id: String,
    peer: SocketAddr,
    conn_token: CancellationToken,
    channels: TaskTracker,
    auth_tx: Option<oneshot::Sender<AuthInfo>>,
    user: String,
    client_key: Option<String>,
}

impl ConnHandler {
    fn check_key(&mut self, user: &str, key: &PublicKey) -> Auth {
        self.user = user.to_string();
        match &self.proxy.client_auth {
            None => Auth::Accept,
            Some(auth) if auth.is_authorized(key) => Auth::Accept,
            Some(_) => Auth::reject(),
        }
    }
}

impl server::Handler for ConnHandler {
    type Error = russh::Error;

    async fn auth_none(&mut self, user: &str) -> Result<Auth, Self::Error> {
        self.user = user.to_string();
        if self.proxy.client_auth.is_none() {
            return Ok(Auth::Accept);
        }
        Ok(Auth::Reject {
            proceed_with_methods: Some(MethodSet::from(&[MethodKind::PublicKey][..])),
            partial_success: false,
        })
    }

    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        offer: PublicKeyOffer<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(self.check_key(user, offer.public_key))
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        let auth = self.check_key(user, key.public_key);
        let fp = fingerprint(key.public_key);
        if auth == Auth::Accept {
            self.client_key = Some(fp);
        } else {
            debug!(user, key = %fp, "Public key is not authorized");
        }
        Ok(auth)
    }

    async fn auth_succeeded(&mut self, session: &mut server::Session) -> Result<(), Self::Error> {
        if let Some(tx) = self.auth_tx.take() {
            let _ = tx.send(AuthInfo {
                user: self.user.clone(),
                client_version: String::from_utf8_lossy(session.remote_sshid()).into_owned(),
                client_key: self.client_key.clone(),
            });
        }
        Ok(())
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if self.proxy.draining.is_cancelled() {
            warn!(conn_id = %self.conn_id, "Rejecting session: proxy is shutting down");
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        let build = Session::new();
        let Some(guard) = self.proxy.track_session(&build.id) else {
            warn!(
                conn_id = %self.conn_id,
                max_sessions = self.proxy.cfg.max_sessions,
                "Rejecting session: concurrent session limit reached"
            );
            reply.reject(ChannelOpenFailure::ResourceShortage).await;
            return Ok(());
        };
        reply.accept().await;

        let span = error_span!(
            "session",
            conn_id = %self.conn_id,
            client_addr = %self.peer,
            session_id = %build.id,
        );
        let proxy = Arc::clone(&self.proxy);
        let fut = proxy.handle_channel(
            build,
            guard,
            channel,
            session.handle(),
            self.conn_token.clone(),
        );
        self.channels.spawn(fut.instrument(span));
        Ok(())
    }

    // Channel requests are answered immediately and forwarded to the builder
    // once its session exists. russh tracks one outstanding want-reply flag
    // per channel, so a deferred answer could be misattributed; answering now
    // is also what a client expects while the builder pod starts. If the
    // builder later rejects the command, the client gets the reason on stderr
    // and exit status 255.

    async fn env_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        _value: &str,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn x11_request(
        &mut self,
        channel: ChannelId,
        _single_connection: bool,
        _x11_auth_protocol: &str,
        _x11_auth_cookie: &str,
        _x11_screen_number: u32,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)
    }

    async fn agent_request(
        &mut self,
        _channel: ChannelId,
        _session: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }
}

async fn start_health_server(
    addr: &str,
    draining: CancellationToken,
) -> anyhow::Result<HttpServer> {
    let listener = crate::http::bind(addr)
        .await
        .context("listen health server")?;
    let router = axum::Router::new()
        // Liveness: the process is running.
        .route(
            "/healthz",
            axum::routing::get(|| async { (StatusCode::OK, "ok") }),
        )
        // Readiness: accepting new connections.
        .route(
            "/readyz",
            axum::routing::get(move || {
                let draining = draining.clone();
                async move {
                    if draining.is_cancelled() {
                        (StatusCode::SERVICE_UNAVAILABLE, "shutting down")
                    } else {
                        (StatusCode::OK, "ready")
                    }
                }
            }),
        );
    HttpServer::spawn("Health", listener, router)
}

/// Finds dead clients with TCP keepalives: a probe after `interval` without
/// traffic, then one more after another `interval`.
///
/// SSH keepalive requests are not used towards clients: russh sends them from
/// the start of the connection, and SSH clients (OpenSSH, russh) end the
/// connection when one arrives before authentication is complete. A client
/// that waits for a passphrase or a security key would be disconnected.
fn set_tcp_keepalive(tcp: &TcpStream, interval: Duration) -> io::Result<()> {
    // Some platforms count in whole seconds.
    let interval = interval.max(Duration::from_secs(1));
    let keepalive = socket2::TcpKeepalive::new().with_time(interval);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let keepalive = keepalive.with_interval(interval).with_retries(1);
    socket2::SockRef::from(tcp).set_tcp_keepalive(&keepalive)
}

/// Accept errors that concern only the connection being accepted.
fn is_per_connection_accept_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    )
}

/// Accept errors caused by temporary resource exhaustion: out of file
/// descriptors (EMFILE, ENFILE), buffers (ENOBUFS) or memory (ENOMEM).
fn is_resource_exhaustion(err: &io::Error) -> bool {
    const ENOMEM: i32 = 12;
    const ENFILE: i32 = 23;
    const EMFILE: i32 = 24;
    #[cfg(target_os = "linux")]
    const ENOBUFS: i32 = 105;
    #[cfg(not(target_os = "linux"))]
    const ENOBUFS: i32 = 55;
    err.kind() == io::ErrorKind::OutOfMemory
        || matches!(err.raw_os_error(), Some(code) if code == ENOMEM || code == ENFILE || code == EMFILE || code == ENOBUFS)
}

/// The last 12 hex digits of a fresh UUIDv7, as a short connection ID.
fn short_id() -> String {
    let id = uuid::Uuid::now_v7().to_string();
    id[24..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_accept_errors() {
        let emfile = io::Error::from_raw_os_error(24);
        assert!(is_resource_exhaustion(&emfile) && !is_per_connection_accept_error(&emfile));
        let aborted = io::Error::from(io::ErrorKind::ConnectionAborted);
        assert!(is_per_connection_accept_error(&aborted) && !is_resource_exhaustion(&aborted));
        let fatal = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(!is_per_connection_accept_error(&fatal) && !is_resource_exhaustion(&fatal));
        assert_eq!(short_id().len(), 12);
    }

    #[tokio::test]
    async fn enables_tcp_keepalive() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        // Sub-second intervals are raised to one second, not rejected.
        set_tcp_keepalive(&tcp, Duration::from_millis(200)).unwrap();
        let socket = socket2::SockRef::from(&tcp);
        assert!(socket.keepalive().unwrap());
        assert_eq!(socket.tcp_keepalive_time().unwrap(), Duration::from_secs(1));
    }
}
