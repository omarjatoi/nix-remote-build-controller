//! End-to-end tests of the proxy: a real russh client talks to the proxy, which
//! talks to an in-process fake builder sshd, with an in-memory Kubernetes API
//! and a fake controller in between.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::server::{self, Auth, Msg, PublicKeyOffer, VerifiedPublicKey};
use russh::{Channel, ChannelId, ChannelMsg, ChannelOpenFailure, client};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::keys::generate_host_key;
use super::session::build_request_name;
use super::tunnel::{BuilderAuth, DialError, dial_builder};
use super::*;
use crate::api::{BuildPhase, NixBuildRequest, NixBuildRequestStatus};
use crate::testutil::FakeKube;

const NS: &str = "nix-test";

fn key() -> PrivateKey {
    generate_host_key().expect("generate key")
}

async fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// Fake builder: an in-process stand-in for a builder pod's sshd, with a few
// commands that make concurrency and lifecycle observable.

#[derive(Default)]
struct BuilderStats {
    active: AtomicI32,
    peak: AtomicI32,
    sessions: AtomicI32,
    /// Monotonic count of sessions that reached the wait-for-peers barrier.
    arrived: AtomicI32,
}

struct FakeBuilder {
    addr: SocketAddr,
    host_key: PublicKey,
    stats: Arc<BuilderStats>,
    hang_ended: tokio::sync::Mutex<mpsc::UnboundedReceiver<()>>,
    stop: CancellationToken,
}

impl Drop for FakeBuilder {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[derive(Clone)]
struct BuilderShared {
    proxy_key: PublicKey,
    stats: Arc<BuilderStats>,
    hang_ended: mpsc::UnboundedSender<()>,
}

impl FakeBuilder {
    async fn start(proxy_key: PublicKey) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        Self::serve(listener, proxy_key)
    }

    fn serve(listener: TcpListener, proxy_key: PublicKey) -> Self {
        let host_key = key();
        let public = host_key.public_key().clone();
        let config = Arc::new(server::Config {
            keys: vec![host_key],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            inactivity_timeout: None,
            ..Default::default()
        });
        let stats = Arc::new(BuilderStats::default());
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = BuilderShared {
            proxy_key,
            stats: Arc::clone(&stats),
            hang_ended: tx,
        };
        let stop = CancellationToken::new();
        let addr = listener.local_addr().unwrap();
        let stopped = stop.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = tokio::select! {
                    () = stopped.cancelled() => return,
                    accepted = listener.accept() => match accepted {
                        Ok(a) => a,
                        Err(_) => return,
                    },
                };
                let handler = BuilderConn {
                    shared: shared.clone(),
                    closed: CancellationToken::new(),
                };
                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    let closed = handler.closed.clone();
                    if let Ok(session) = server::run_stream(config, stream, handler).await {
                        let _ = session.await;
                    }
                    closed.cancel();
                });
            }
        });
        Self {
            addr,
            host_key: public,
            stats,
            hang_ended: tokio::sync::Mutex::new(rx),
            stop,
        }
    }

    async fn wait_hang_ended(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.hang_ended.lock().await.recv())
            .await
            .is_ok()
    }
}

struct BuilderConn {
    shared: BuilderShared,
    /// Cancelled when the connection ends.
    closed: CancellationToken,
}

impl server::Handler for BuilderConn {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        offer: PublicKeyOffer<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(self.check(user, offer.public_key))
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(self.check(user, key.public_key))
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        tokio::spawn(run_builder_session(
            channel,
            self.shared.clone(),
            self.closed.clone(),
        ));
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        _value: &str,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)
    }
}

impl BuilderConn {
    fn check(&self, user: &str, key: &PublicKey) -> Auth {
        if user == "nixbld" && key.key_data() == self.shared.proxy_key.key_data() {
            Auth::Accept
        } else {
            Auth::reject()
        }
    }
}

async fn run_builder_session(
    mut channel: Channel<Msg>,
    shared: BuilderShared,
    closed: CancellationToken,
) {
    let stats = &shared.stats;
    stats.sessions.fetch_add(1, Ordering::SeqCst);
    let n = stats.active.fetch_add(1, Ordering::SeqCst) + 1;
    stats.peak.fetch_max(n, Ordering::SeqCst);

    let mut cmd = None;
    let mut early_stdin = Vec::new();
    while cmd.is_none() {
        match channel.wait().await {
            Some(ChannelMsg::Exec { command, .. }) => {
                cmd = Some(String::from_utf8_lossy(&command).into_owned())
            }
            Some(ChannelMsg::Data { data }) => early_stdin.extend_from_slice(&data),
            Some(_) => {}
            None => {
                stats.active.fetch_sub(1, Ordering::SeqCst);
                return;
            }
        }
    }
    assert!(
        early_stdin.is_empty(),
        "stdin arrived before exec: {early_stdin:?}"
    );
    let cmd = cmd.unwrap();

    let status = match cmd.as_str() {
        "echo-upper" => echo_upper(&mut channel).await,
        "hang" => {
            // Ignore stdin EOF; only end when the connection is torn down.
            while channel.wait().await.is_some() {}
            closed.cancelled().await;
            let _ = shared.hang_ended.send(());
            0
        }
        c if c.starts_with("wait-for-peers=") => {
            let want: i32 = c["wait-for-peers=".len()..].parse().unwrap();
            // A latching barrier: every session that gets here bumps the
            // monotonic counter and waits until `want` have arrived. Passing
            // it proves the proxy ran the sessions concurrently.
            stats.arrived.fetch_add(1, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut ok = true;
            while stats.arrived.load(Ordering::SeqCst) < want {
                if Instant::now() > deadline {
                    let msg = format!(
                        "only {} of {want} peers arrived\n",
                        stats.arrived.load(Ordering::SeqCst)
                    );
                    let _ = channel.extended_data_bytes(1, Bytes::from(msg)).await;
                    ok = false;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            if ok {
                echo_upper(&mut channel).await
            } else {
                2
            }
        }
        c if c.starts_with("exit=") => {
            let code: u32 = c["exit=".len()..].parse().unwrap();
            read_stdin(&mut channel).await;
            let _ = channel
                .extended_data_bytes(1, Bytes::from(format!("exiting with {code}\n")))
                .await;
            code
        }
        other => {
            let _ = channel
                .extended_data_bytes(1, Bytes::from(format!("unknown command {other:?}\n")))
                .await;
            127
        }
    };
    let _ = channel.eof().await;
    let _ = channel.exit_status(status).await;
    let _ = channel.close().await;
    stats.active.fetch_sub(1, Ordering::SeqCst);
}

async fn read_stdin(channel: &mut Channel<Msg>) -> Vec<u8> {
    let mut data = Vec::new();
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Data { data: d }) => data.extend_from_slice(&d),
            Some(ChannelMsg::Eof) | None => return data,
            Some(_) => {}
        }
    }
}

async fn echo_upper(channel: &mut Channel<Msg>) -> u32 {
    let data = read_stdin(channel).await;
    let _ = channel
        .data_bytes(Bytes::from(data.to_ascii_uppercase()))
        .await;
    let _ = channel
        .extended_data_bytes(1, Bytes::from_static(b"stderr-marker\n"))
        .await;
    0
}

// ---------------------------------------------------------------------------
// Fake controller: drives NixBuildRequests to Running (pointing at the fake
// builder on 127.0.0.1) or Failed, and records how many existed at once.

#[derive(Clone, Copy, PartialEq, Eq)]
enum ControllerMode {
    Running,
    Failed,
    Never,
}

#[derive(Default)]
struct ControllerStats {
    seen: BTreeSet<String>,
    peak: usize,
}

fn spawn_fake_controller(
    fake: FakeKube,
    mode: ControllerMode,
    stop: CancellationToken,
) -> Arc<Mutex<ControllerStats>> {
    let stats = Arc::new(Mutex::new(ControllerStats::default()));
    let recorded = Arc::clone(&stats);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
            let requests = fake.requests(NS);
            {
                let mut s = recorded.lock().unwrap();
                s.peak = s.peak.max(requests.len());
                s.seen
                    .extend(requests.iter().map(|r| r.metadata.name.clone().unwrap()));
            }
            for req in requests {
                if req.status.as_ref().and_then(|s| s.phase).is_some()
                    || mode == ControllerMode::Never
                {
                    continue;
                }
                let status = match mode {
                    ControllerMode::Failed => NixBuildRequestStatus {
                        phase: Some(BuildPhase::Failed),
                        message: "Builder pod not scheduled: Unschedulable: no nodes".into(),
                        ..Default::default()
                    },
                    _ => NixBuildRequestStatus {
                        phase: Some(BuildPhase::Running),
                        pod_name: format!("nix-builder-{}", req.spec.session_id),
                        pod_ip: "127.0.0.1".into(),
                        ..Default::default()
                    },
                };
                fake.set_request_status(NS, req.metadata.name.as_deref().unwrap(), status);
            }
        }
    });
    stats
}

// ---------------------------------------------------------------------------
// Harness.

struct Harness {
    proxy: Arc<SshProxy>,
    builder: FakeBuilder,
    controller: Arc<Mutex<ControllerStats>>,
    fake: FakeKube,
    client_key: PrivateKey,
    shutdown: CancellationToken,
    run: Option<JoinHandle<anyhow::Result<()>>>,
    controller_stop: CancellationToken,
}

struct HarnessOpts {
    mode: ControllerMode,
    client_auth: Option<ClientAuthorizer>,
    cfg: fn(&mut Config),
}

impl Default for HarnessOpts {
    fn default() -> Self {
        Self {
            mode: ControllerMode::Running,
            client_auth: None,
            cfg: |_| {},
        }
    }
}

impl Harness {
    async fn new(opts: HarnessOpts) -> Self {
        let fake = FakeKube::new();
        let builder_client_key = key();
        let builder = FakeBuilder::start(builder_client_key.public_key().clone()).await;
        let mut cfg = Config {
            listen_addr: "127.0.0.1:0".into(),
            health_addr: "127.0.0.1:0".into(),
            namespace: NS.into(),
            remote_user: "nixbld".into(),
            remote_port: builder.addr.port(),
            pod_ready_timeout: Duration::from_secs(5),
            poll_interval: Duration::from_millis(10),
            keepalive_interval: Duration::from_millis(200),
            client_close_grace: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(10),
            build_timeout: Duration::from_secs(90 * 60),
            owner_pod_name: "proxy-0".into(),
            owner_pod_uid: "uid-proxy-0".into(),
            ..Default::default()
        };
        (opts.cfg)(&mut cfg);

        let proxy = SshProxy::new(
            cfg,
            Arc::new(fake.clone()),
            key(),
            builder_client_key,
            opts.client_auth,
            Some(builder.host_key.clone()),
        )
        .await
        .expect("create proxy");
        let controller_stop = CancellationToken::new();
        let controller = spawn_fake_controller(fake.clone(), opts.mode, controller_stop.clone());
        let shutdown = CancellationToken::new();
        let run = {
            let proxy = Arc::clone(&proxy);
            let shutdown = shutdown.clone();
            tokio::spawn(async move { proxy.run(shutdown).await })
        };
        Self {
            proxy,
            builder,
            controller,
            fake,
            client_key: key(),
            shutdown,
            run: Some(run),
            controller_stop,
        }
    }

    async fn dial(&self) -> client::Handle<TestClient> {
        dial_proxy(self.proxy.local_addr(), &self.client_key)
            .await
            .expect("dial proxy")
    }

    fn requests(&self) -> Vec<NixBuildRequest> {
        self.fake.requests(NS)
    }

    async fn wait_no_requests(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.requests().is_empty() && self.proxy.session_count() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "expected all NixBuildRequests to be deleted, still have {} (sessions={})",
            self.requests().len(),
            self.proxy.session_count()
        );
    }

    fn stats(&self) -> (usize, usize) {
        let s = self.controller.lock().unwrap();
        (s.peak, s.seen.len())
    }

    /// Signals shutdown and waits for `run` to return.
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.shutdown.cancel();
        let run = self.run.take().expect("proxy still running");
        let result = tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .expect("proxy did not stop")
            .expect("run task panicked");
        self.controller_stop.cancel();
        result
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.controller_stop.cancel();
    }
}

struct TestClient;

impl client::Handler for TestClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

async fn dial_proxy(
    addr: SocketAddr,
    key: &PrivateKey,
) -> anyhow::Result<client::Handle<TestClient>> {
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        ..Default::default()
    });
    let mut handle = client::connect(config, addr, TestClient).await?;
    let auth = handle
        .authenticate_publickey(
            "nixbld",
            PrivateKeyWithHashAlg::new(Arc::new(key.clone()), None),
        )
        .await?;
    anyhow::ensure!(auth.success(), "authentication rejected");
    Ok(handle)
}

#[derive(Debug, Default)]
struct Output {
    stdout: String,
    stderr: String,
    exit_status: Option<u32>,
}

/// Runs `cmd` through the proxy, feeding `stdin`, and collects the output.
async fn run_command(handle: &client::Handle<TestClient>, cmd: &str, stdin: &str) -> Output {
    let channel = handle.channel_open_session().await.expect("open session");
    channel.exec(true, cmd).await.expect("exec");
    channel
        .data_bytes(Bytes::from(stdin.to_string()))
        .await
        .expect("stdin");
    channel.eof().await.expect("eof");
    collect(channel).await
}

async fn collect(mut channel: Channel<client::Msg>) -> Output {
    let mut out = Output::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let read = async {
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => stdout.extend_from_slice(&data),
                Some(ChannelMsg::ExtendedData { data, .. }) => stderr.extend_from_slice(&data),
                Some(ChannelMsg::ExitStatus { exit_status }) => out.exit_status = Some(exit_status),
                Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(20), read)
        .await
        .expect("session did not finish");
    out.stdout = String::from_utf8_lossy(&stdout).into_owned();
    out.stderr = String::from_utf8_lossy(&stderr).into_owned();
    out
}

// ---------------------------------------------------------------------------
// Tests.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_connections_get_independent_builders_and_run_in_parallel() {
    let mut h = Harness::new(HarnessOpts::default()).await;
    const N: usize = 4;
    let addr = h.proxy.local_addr();
    let mut tasks = Vec::new();
    for i in 0..N {
        let key = h.client_key.clone();
        tasks.push(tokio::spawn(async move {
            let client = dial_proxy(addr, &key).await.unwrap();
            let input = format!("payload-{i}\n");
            // Every session blocks inside the fake builder until N sessions are
            // active at once, so this only completes if the proxy runs them
            // concurrently rather than one after another.
            let out = run_command(&client, &format!("wait-for-peers={N}"), &input).await;
            assert_eq!(out.exit_status, Some(0), "session {i}: {out:?}");
            assert_eq!(out.stdout, input.to_uppercase(), "session {i}");
            assert!(
                out.stderr.contains("stderr-marker"),
                "session {i}: stderr not forwarded: {out:?}"
            );
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }

    assert_eq!(
        h.builder.stats.peak.load(Ordering::SeqCst),
        N as i32,
        "peak concurrent builder sessions"
    );
    let (peak, total) = h.stats();
    assert_eq!(total, N, "distinct NixBuildRequests");
    assert_eq!(peak, N, "peak simultaneous NixBuildRequests");
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multiple_channels_on_one_connection_are_independent_sessions() {
    let mut h = Harness::new(HarnessOpts::default()).await;
    let client = Arc::new(h.dial().await);
    const N: usize = 3;
    let mut tasks = Vec::new();
    for i in 0..N {
        let client = Arc::clone(&client);
        tasks.push(tokio::spawn(async move {
            let input = format!("chan-{i}");
            let out = run_command(&client, &format!("wait-for-peers={N}"), &input).await;
            assert_eq!(out.exit_status, Some(0), "channel {i}: {out:?}");
            assert_eq!(out.stdout, input.to_uppercase());
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(h.stats().1, N, "distinct NixBuildRequests");
    drop(client);
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn build_request_carries_timeout_labels_and_owner_reference() {
    let mut h = Harness::new(HarnessOpts {
        mode: ControllerMode::Never,
        cfg: |c| c.pod_ready_timeout = Duration::from_secs(2),
        ..Default::default()
    })
    .await;
    let client = h.dial().await;
    let channel = client.channel_open_session().await.unwrap();
    channel.exec(true, "echo-upper").await.unwrap();

    wait_until("a NixBuildRequest", Duration::from_secs(2), || {
        !h.requests().is_empty()
    })
    .await;
    let reqs = h.requests();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!(req.spec.timeout_seconds, Some(90 * 60));
    assert_eq!(
        req.metadata.name.as_deref(),
        Some(build_request_name(&req.spec.session_id).as_str())
    );
    let labels = req.metadata.labels.as_ref().unwrap();
    assert_eq!(labels[crate::api::LABEL_SESSION_ID], req.spec.session_id);
    assert_eq!(labels[LABEL_MANAGED_BY], MANAGED_BY_VALUE);
    assert_eq!(
        req.metadata.annotations.as_ref().unwrap()[ANNOTATION_PROXY_POD],
        "proxy-0"
    );
    let owners = req.metadata.owner_references.as_ref().unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(
        (
            owners[0].kind.as_str(),
            owners[0].name.as_str(),
            owners[0].uid.as_str()
        ),
        ("Pod", "proxy-0", "uid-proxy-0")
    );
    assert_eq!(owners[0].api_version, "v1");
    assert_eq!(owners[0].controller, None);
    // Session IDs are UUIDv7.
    assert_eq!(
        uuid::Uuid::parse_str(&req.spec.session_id)
            .unwrap()
            .get_version_num(),
        7
    );

    drop(channel);
    drop(client);
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn sub_second_build_timeout_sets_no_deadline_and_no_owner_without_uid() {
    let mut h = Harness::new(HarnessOpts {
        mode: ControllerMode::Never,
        cfg: |c| {
            c.build_timeout = Duration::from_millis(500); // truncates to 0 seconds
            c.pod_ready_timeout = Duration::from_secs(2);
            c.owner_pod_uid = String::new();
        },
        ..Default::default()
    })
    .await;
    let client = h.dial().await;
    let channel = client.channel_open_session().await.unwrap();
    channel.exec(true, "echo-upper").await.unwrap();
    wait_until("a NixBuildRequest", Duration::from_secs(2), || {
        !h.requests().is_empty()
    })
    .await;
    let req = &h.requests()[0];
    assert_eq!(
        req.spec.timeout_seconds, None,
        "sub-second build timeout must not set a (zero) deadline"
    );
    assert!(req.metadata.owner_references.is_none());
    assert!(req.metadata.annotations.is_none());
    drop(channel);
    drop(client);
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn exit_status_is_propagated_and_recorded() {
    let mut h = Harness::new(HarnessOpts::default()).await;
    let client = h.dial().await;
    let out = run_command(&client, "exit=3", "ignored").await;
    assert_eq!(out.exit_status, Some(3), "{out:?}");
    assert!(out.stderr.contains("exiting with 3"), "{out:?}");
    drop(client);
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn unknown_command_exit_status_is_forwarded() {
    let mut h = Harness::new(HarnessOpts::default()).await;
    let client = h.dial().await;
    let out = run_command(&client, "frobnicate", "").await;
    assert_eq!(out.exit_status, Some(127), "{out:?}");
    assert!(out.stderr.contains("unknown command"), "{out:?}");
    drop(client);
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn large_payloads_survive_flow_control() {
    let mut h = Harness::new(HarnessOpts::default()).await;
    let client = h.dial().await;
    // Several SSH windows' worth in both directions.
    let input: String = (0..(6 << 20))
        .map(|i| (b'a' + (i % 26) as u8) as char)
        .collect();
    let out = run_command(&client, "echo-upper", &input).await;
    assert_eq!(out.exit_status, Some(0));
    assert_eq!(out.stdout.len(), input.len());
    assert!(out.stdout == input.to_uppercase(), "payload corrupted");
    drop(client);
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn controller_failure_is_reported_to_client() {
    let mut h = Harness::new(HarnessOpts {
        mode: ControllerMode::Failed,
        ..Default::default()
    })
    .await;
    let client = h.dial().await;
    let out = run_command(&client, "echo-upper", "x").await;
    assert_eq!(out.exit_status, Some(255), "{out:?}");
    assert!(
        out.stderr.contains("Unschedulable"),
        "stderr should carry the controller's message: {out:?}"
    );
    assert!(
        out.stderr.starts_with("nix-remote-build-controller: "),
        "{out:?}"
    );
    assert_eq!(
        h.builder.stats.sessions.load(Ordering::SeqCst),
        0,
        "builder must not be contacted"
    );
    drop(client);
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn pod_ready_timeout_fails_session() {
    let mut h = Harness::new(HarnessOpts {
        mode: ControllerMode::Never,
        cfg: |c| c.pod_ready_timeout = Duration::from_millis(200),
        ..Default::default()
    })
    .await;
    let client = h.dial().await;
    let start = Instant::now();
    let out = run_command(&client, "echo-upper", "x").await;
    assert_eq!(out.exit_status, Some(255), "{out:?}");
    assert!(out.stderr.contains("timed out"), "{out:?}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "timeout took {:?}",
        start.elapsed()
    );
    drop(client);
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn failed_sessions_record_their_outcome_before_deletion() {
    // Keep the request around (via a foreign finalizer) to inspect the status
    // the proxy writes on failure.
    let mut h = Harness::new(HarnessOpts {
        mode: ControllerMode::Never,
        cfg: |c| c.pod_ready_timeout = Duration::from_millis(200),
        ..Default::default()
    })
    .await;
    let client = h.dial().await;
    let channel = client.channel_open_session().await.unwrap();
    channel.exec(true, "echo-upper").await.unwrap();
    wait_until("a NixBuildRequest", Duration::from_secs(2), || {
        !h.requests().is_empty()
    })
    .await;
    let mut req = h.requests().remove(0);
    req.metadata.finalizers = Some(vec!["test/hold".into()]);
    crate::controller::ControllerClient::replace_request(&h.fake, &req)
        .await
        .unwrap();

    let out = collect(channel).await;
    assert_eq!(out.exit_status, Some(255));
    let name = req.metadata.name.clone().unwrap();
    wait_until(
        "the request to be marked deleted",
        Duration::from_secs(5),
        || {
            h.fake
                .request(NS, &name)
                .is_some_and(|r| r.metadata.deletion_timestamp.is_some())
        },
    )
    .await;
    let status = h.fake.request(NS, &name).unwrap().status.unwrap();
    assert_eq!(status.phase, Some(BuildPhase::Failed));
    assert!(
        status
            .message
            .starts_with("Build session failed: timed out"),
        "{}",
        status.message
    );
    assert!(status.completion_time.is_some());
    drop(client);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn client_disconnect_tears_down_builder_session() {
    let mut h = Harness::new(HarnessOpts::default()).await;
    let client = h.dial().await;
    let channel = client.channel_open_session().await.unwrap();
    channel.exec(true, "hang").await.unwrap();
    channel
        .data_bytes(Bytes::from_static(b"hello"))
        .await
        .unwrap();
    wait_until("the builder session", Duration::from_secs(5), || {
        h.builder.stats.active.load(Ordering::SeqCst) == 1
    })
    .await;

    // Simulate the Nix client dying (Ctrl-C): drop the whole connection.
    drop(channel);
    client
        .disconnect(russh::Disconnect::ByApplication, "bye", "")
        .await
        .unwrap();
    drop(client);

    assert!(
        h.builder.wait_hang_ended(Duration::from_secs(5)).await,
        "builder session was not torn down after client disconnect"
    );
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn client_disconnect_while_waiting_for_pod_cleans_up() {
    let mut h = Harness::new(HarnessOpts {
        mode: ControllerMode::Never,
        ..Default::default()
    })
    .await;
    let client = h.dial().await;
    let channel = client.channel_open_session().await.unwrap();
    channel.exec(true, "echo-upper").await.unwrap();
    wait_until("a NixBuildRequest", Duration::from_secs(2), || {
        !h.requests().is_empty()
    })
    .await;
    let start = Instant::now();
    drop(channel);
    drop(client);
    h.wait_no_requests().await;
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "session lingered for the pod-ready timeout"
    );
    h.stop().await.unwrap();
}

#[tokio::test]
async fn client_channel_close_gives_builder_grace_then_forces() {
    let mut h = Harness::new(HarnessOpts {
        cfg: |c| c.client_close_grace = Duration::from_millis(300),
        ..Default::default()
    })
    .await;
    let client = h.dial().await;
    let channel = client.channel_open_session().await.unwrap();
    channel.exec(true, "hang").await.unwrap();
    wait_until("the builder session", Duration::from_secs(5), || {
        h.builder.stats.active.load(Ordering::SeqCst) == 1
    })
    .await;

    let start = Instant::now();
    channel.close().await.unwrap(); // close only the channel; the connection stays up
    assert!(
        h.builder.wait_hang_ended(Duration::from_secs(5)).await,
        "builder session was not torn down after client closed its channel"
    );
    assert!(
        start.elapsed() >= Duration::from_millis(250),
        "builder was torn down before the grace period: {:?}",
        start.elapsed()
    );
    h.wait_no_requests().await;
    // The connection itself is still usable.
    let out = run_command(&client, "echo-upper", "again").await;
    assert_eq!(out.stdout, "AGAIN");
    drop(channel);
    drop(client);
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn client_close_without_eof_still_ends_builder_stdin() {
    let mut h = Harness::new(HarnessOpts {
        cfg: |c| c.client_close_grace = Duration::from_secs(20),
        ..Default::default()
    })
    .await;
    let client = h.dial().await;
    let channel = client.channel_open_session().await.unwrap();
    channel.exec(true, "echo-upper").await.unwrap();
    channel
        .data_bytes(Bytes::from_static(b"partial"))
        .await
        .unwrap();
    wait_until("the builder session", Duration::from_secs(5), || {
        h.builder.stats.active.load(Ordering::SeqCst) == 1
    })
    .await;

    // Close the channel without sending EOF first. The builder must still see
    // end-of-input and exit long before the grace period runs out.
    let start = Instant::now();
    channel.close().await.unwrap();
    wait_until(
        "the builder command to exit",
        Duration::from_secs(3),
        || h.builder.stats.active.load(Ordering::SeqCst) == 0,
    )
    .await;
    h.wait_no_requests().await;
    assert!(start.elapsed() < Duration::from_secs(3));
    drop(channel);
    drop(client);
    h.stop().await.unwrap();
}

#[tokio::test]
async fn max_sessions_rejects_excess_channels() {
    let mut h = Harness::new(HarnessOpts {
        cfg: |c| c.max_sessions = 1,
        ..Default::default()
    })
    .await;
    let client = h.dial().await;
    let first = client.channel_open_session().await.unwrap();
    first.exec(true, "hang").await.unwrap();
    wait_until("the first session", Duration::from_secs(5), || {
        h.proxy.session_count() == 1
    })
    .await;

    match client.channel_open_session().await {
        Ok(_) => panic!("second session should have been rejected"),
        Err(russh::Error::ChannelOpenFailure(ChannelOpenFailure::ResourceShortage)) => {}
        Err(err) => panic!("unexpected rejection error: {err:?}"),
    }
    drop(first);
    drop(client);
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn client_public_key_authentication() {
    let authorized = key();
    let line = authorized.public_key().to_openssh().unwrap();
    let auth = ClientAuthorizer::parse(format!("# comment\n\n{line}\n").as_bytes()).unwrap();
    let mut h = Harness::new(HarnessOpts {
        client_auth: Some(auth),
        ..Default::default()
    })
    .await;

    // Wrong key: rejected during the handshake.
    assert!(
        dial_proxy(h.proxy.local_addr(), &key()).await.is_err(),
        "unauthorized key was accepted"
    );

    // Right key: works end to end.
    h.client_key = authorized;
    let client = h.dial().await;
    let out = run_command(&client, "echo-upper", "ok").await;
    assert_eq!(out.stdout, "OK");
    assert_eq!(out.exit_status, Some(0));
    drop(client);
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn anonymous_clients_are_accepted_without_client_auth() {
    let mut h = Harness::new(HarnessOpts::default()).await;
    let mut handle = client::connect(
        Arc::new(client::Config::default()),
        h.proxy.local_addr(),
        TestClient,
    )
    .await
    .unwrap();
    assert!(handle.authenticate_none("nixbld").await.unwrap().success());
    let out = run_command(&handle, "echo-upper", "anon").await;
    assert_eq!(out.stdout, "ANON");
    drop(handle);
    h.wait_no_requests().await;
    h.stop().await.unwrap();
}

#[tokio::test]
async fn health_endpoints_track_shutdown() {
    let mut h = Harness::new(HarnessOpts::default()).await;
    let addr = h.proxy.health_addr().await.expect("health server running");
    assert_eq!(http_get(addr, "/healthz").await, (200, "ok".to_string()));
    assert_eq!(http_get(addr, "/readyz").await, (200, "ready".to_string()));

    // Hold a session open so the drain keeps the health server up.
    let client = h.dial().await;
    let channel = client.channel_open_session().await.unwrap();
    channel.exec(true, "hang").await.unwrap();
    wait_until("the builder session", Duration::from_secs(5), || {
        h.builder.stats.active.load(Ordering::SeqCst) == 1
    })
    .await;
    h.shutdown.cancel();
    let mut ready = (0, String::new());
    for _ in 0..100 {
        ready = http_get(addr, "/readyz").await;
        if ready.0 == 503 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(ready, (503, "shutting down".to_string()));
    assert_eq!(http_get(addr, "/healthz").await.0, 200);
    drop(channel);
    drop(client);
    h.stop().await.unwrap();
}

async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let code = response[9..12].parse().unwrap();
    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_string();
    (code, body)
}

#[tokio::test]
async fn shutdown_drains_in_flight_sessions_and_refuses_new_connections() {
    let mut h = Harness::new(HarnessOpts::default()).await;
    let client = h.dial().await;
    let mut channel = client.channel_open_session().await.unwrap();
    channel.exec(true, "echo-upper").await.unwrap();
    wait_until("the builder session", Duration::from_secs(5), || {
        h.builder.stats.active.load(Ordering::SeqCst) == 1
    })
    .await;

    // Signal shutdown while the session is mid-flight.
    h.shutdown.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // New connections are refused...
    assert!(
        tokio::net::TcpStream::connect(h.proxy.local_addr())
            .await
            .is_err(),
        "listener should be closed after shutdown signal"
    );
    assert!(
        !h.run.as_ref().unwrap().is_finished(),
        "proxy stopped before in-flight session finished"
    );

    // ...but the in-flight session completes normally.
    channel
        .data_bytes(Bytes::from_static(b"draining"))
        .await
        .unwrap();
    channel.eof().await.unwrap();
    let mut stdout = Vec::new();
    let mut status = None;
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    assert_eq!(status, Some(0));
    assert_eq!(stdout, b"DRAINING");
    drop(channel);
    client
        .disconnect(russh::Disconnect::ByApplication, "done", "")
        .await
        .unwrap();
    drop(client);

    h.stop().await.expect("clean shutdown");
}

#[tokio::test]
async fn shutdown_timeout_forces_sessions_closed() {
    let mut h = Harness::new(HarnessOpts {
        cfg: |c| c.shutdown_timeout = Duration::from_millis(300),
        ..Default::default()
    })
    .await;
    let client = h.dial().await;
    let channel = client.channel_open_session().await.unwrap();
    channel.exec(true, "hang").await.unwrap();
    wait_until("the builder session", Duration::from_secs(5), || {
        h.builder.stats.active.load(Ordering::SeqCst) == 1
    })
    .await;

    let start = Instant::now();
    drop(client);
    h.stop().await.expect("clean shutdown");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "forced shutdown took {:?}",
        start.elapsed()
    );
    let out = collect(channel).await;
    assert_ne!(
        out.exit_status,
        Some(0),
        "client session should have been terminated"
    );
    assert!(h.builder.wait_hang_ended(Duration::from_secs(5)).await);
    h.wait_no_requests().await;
}

#[tokio::test]
async fn run_twice_is_an_error() {
    let mut h = Harness::new(HarnessOpts::default()).await;
    // Let the harness's run task take the listener first.
    drop(h.dial().await);
    let second = tokio::time::timeout(
        Duration::from_secs(5),
        h.proxy.run(CancellationToken::new()),
    )
    .await
    .expect("second run must return immediately");
    assert!(second.is_err());
    h.stop().await.unwrap();
}

// ---------------------------------------------------------------------------
// Builder dial.

fn builder_auth(client_key: &PrivateKey, host_key: Option<PublicKey>) -> BuilderAuth {
    BuilderAuth {
        user: "nixbld".into(),
        key: Arc::new(client_key.clone()),
        host_key,
        config: Arc::new(client::Config {
            inactivity_timeout: None,
            ..Default::default()
        }),
    }
}

/// An address on loopback with nothing listening on it, so a dial is refused
/// the way it is against a builder pod whose sshd has not bound yet.
async fn reserve_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

// The controller hands the proxy a pod IP as soon as the pod has one, which is
// routinely before sshd is listening. The dial is what absorbs that gap.
#[tokio::test]
async fn dial_builder_retries_while_builder_is_still_starting() {
    let client_key = key();
    let addr = reserve_addr().await;
    let host_key = Arc::new(Mutex::new(None::<PublicKey>));
    let started = Arc::new(AtomicBool::new(false));

    let server = {
        let proxy_key = client_key.public_key().clone();
        let host_key = Arc::clone(&host_key);
        let started = Arc::clone(&started);
        tokio::spawn(async move {
            // Long enough that the first several dials are refused.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let listener = TcpListener::bind(addr).await.unwrap();
            let builder = FakeBuilder::serve(listener, proxy_key);
            *host_key.lock().unwrap() = Some(builder.host_key.clone());
            started.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(builder);
        })
    };

    let start = Instant::now();
    let auth = builder_auth(&client_key, None);
    let handle = dial_builder(
        addr,
        &auth,
        Duration::from_secs(15),
        &CancellationToken::new(),
    )
    .await
    .expect("dial_builder did not recover from a not-yet-listening builder");
    assert!(started.load(Ordering::SeqCst));
    assert!(
        start.elapsed() >= Duration::from_millis(250),
        "connected in {:?}, before the server started; not exercising the retry",
        start.elapsed()
    );
    drop(handle);
    server.abort();
}

// A builder that never comes up must fail with a bounded, descriptive error
// rather than hanging for the whole session.
#[tokio::test]
async fn dial_builder_gives_up_after_timeout() {
    let addr = reserve_addr().await;
    let auth = builder_auth(&key(), Some(key().public_key().clone()));
    let start = Instant::now();
    let err = dial_builder(
        addr,
        &auth,
        Duration::from_millis(400),
        &CancellationToken::new(),
    )
    .await
    .err()
    .expect("expected an error dialling a port with no listener");
    let elapsed = start.elapsed();
    assert!(
        err.to_string().contains("attempts"),
        "error should report attempts: {err}"
    );
    assert!(
        elapsed >= Duration::from_millis(400),
        "gave up after {elapsed:?}, before the timeout"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "took {elapsed:?} to give up on a 400ms budget"
    );
}

// A rejected host key is a misconfiguration, not a pod that is still starting:
// retrying it until the timeout would turn a clear error into a slow one.
#[tokio::test]
async fn dial_builder_does_not_retry_host_key_mismatch() {
    let client_key = key();
    let builder = FakeBuilder::start(client_key.public_key().clone()).await;
    let auth = builder_auth(&client_key, Some(key().public_key().clone()));
    let start = Instant::now();
    let err = dial_builder(
        builder.addr,
        &auth,
        Duration::from_secs(10),
        &CancellationToken::new(),
    )
    .await
    .err()
    .expect("expected a host key mismatch error");
    assert!(matches!(err, DialError::HostKeyMismatch), "{err}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "host key mismatch took {:?}",
        start.elapsed()
    );

    // The right host key works.
    let auth = builder_auth(&client_key, Some(builder.host_key.clone()));
    dial_builder(
        builder.addr,
        &auth,
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .await
    .expect("dial with the right host key");
}

#[tokio::test]
async fn dial_builder_does_not_retry_rejected_client_key() {
    let builder = FakeBuilder::start(key().public_key().clone()).await;
    let auth = builder_auth(&key(), None);
    let start = Instant::now();
    let err = dial_builder(
        builder.addr,
        &auth,
        Duration::from_secs(10),
        &CancellationToken::new(),
    )
    .await
    .err()
    .expect("expected an authentication error");
    assert!(matches!(err, DialError::AuthRejected(_)), "{err}");
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn dial_builder_stops_when_cancelled() {
    let addr = reserve_addr().await;
    let auth = builder_auth(&key(), None);
    let cancel = CancellationToken::new();
    let cancel_soon = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_soon.cancel();
    });
    let start = Instant::now();
    assert!(
        dial_builder(addr, &auth, Duration::from_secs(30), &cancel)
            .await
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn builder_not_up_yet_classification() {
    // A refused connection is the normal state while a pod is starting.
    let addr = reserve_addr().await;
    let refused = tokio::net::TcpStream::connect(addr)
        .await
        .expect_err("port should refuse");
    assert!(
        DialError::Connect {
            addr,
            source: refused
        }
        .is_builder_not_up_yet()
    );
    assert!(DialError::Timeout.is_builder_not_up_yet());
    assert!(DialError::Handshake(russh::Error::Disconnect).is_builder_not_up_yet());
    // Misconfigurations are not.
    assert!(!DialError::AuthRejected("nixbld".into()).is_builder_not_up_yet());
    assert!(!DialError::HostKeyMismatch.is_builder_not_up_yet());
    assert!(
        !DialError::Handshake(russh::Error::NoCommonAlgo {
            kind: russh::AlgorithmKind::Kex,
            ours: vec![],
            theirs: vec![],
        })
        .is_builder_not_up_yet()
    );
}
