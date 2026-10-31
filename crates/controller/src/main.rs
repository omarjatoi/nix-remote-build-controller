//! The controller: creates a builder pod for each NixBuildRequest.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};
use k8s_openapi::api::core::v1::Secret;
use kube::runtime::events::Reporter;
use kube::{Api, Client};
use tokio_util::sync::CancellationToken;
use tracing::level_filters::LevelFilter;
use tracing::{error, info, warn};

use api::{KubeControllerClient, secret};
use common::Telemetry;
use common::http::{self, HttpServer};
use common::{duration, error, logging, shutdown};
use controller::leader::{LeaderElectionConfig, LeaderElector};
use controller::{Metrics, Reconciler, ReconcilerConfig, Runner};

/// The version reported by `controller version`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How this program is named in exported telemetry.
const SERVICE_NAME: &str = "nix-remote-build-controller";

const LEADER_ELECTION_ID: &str = "nix-remote-build-controller.nix.io";
const REPORTER_NAME: &str = "nix-remote-build-controller";

/// Kubernetes controller for Nix remote builders.
///
/// Creates a short-lived Nix builder pod for each NixBuildRequest.
#[derive(Parser, Debug)]
#[command(
    name = "controller",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Builder container image (required)
    #[arg(long, required = true)]
    builder_image: Option<String>,

    /// Image pull policy for builder pods: Always, IfNotPresent or Never
    /// (default: Kubernetes default)
    #[arg(long, default_value = "")]
    builder_image_pull_policy: String,

    /// Default resource requests for builder pods, e.g. cpu=1,memory=2Gi
    #[arg(long, default_value = "")]
    builder_requests: String,

    /// Default resource limits for builder pods, e.g. cpu=4,memory=8Gi
    #[arg(long, default_value = "")]
    builder_limits: String,

    /// Service account for builder pods (default: the namespace default)
    #[arg(long, default_value = "")]
    builder_service_account: String,

    /// SSH port in builder pods
    #[arg(long, default_value_t = 22, value_parser = clap::value_parser!(u16).range(1..))]
    remote_port: u16,

    /// ConfigMap containing nix.conf, mounted at /etc/nix in builder pods
    /// (optional)
    #[arg(long, default_value = "")]
    nix_config: String,

    /// Secret with the SSH keypair for builder authentication (keys 'private'
    /// and 'public'; optional 'builder-host-key')
    #[arg(long, default_value = "nix-builder-ssh-keys")]
    ssh_key_secret: String,

    /// Namespace of the SSH key secret and leader election lease
    #[arg(short = 'n', long, default_value = "default")]
    namespace: String,

    /// Health check server port
    #[arg(long, default_value_t = 8081)]
    health_port: u16,

    /// Address for the Prometheus metrics endpoint ("0" disables it)
    #[arg(long, default_value = "0")]
    metrics_addr: String,

    /// How long to wait for in-flight reconciles on shutdown
    #[arg(long, default_value = "30s", value_parser = duration::clap_parser)]
    shutdown_timeout: Duration,

    /// How long Completed/Failed NixBuildRequests are kept before the
    /// controller deletes them
    #[arg(long, default_value = "10m", value_parser = duration::clap_parser)]
    completed_ttl: Duration,

    /// Number of NixBuildRequests reconciled concurrently
    #[arg(long, default_value_t = i64::from(controller::DEFAULT_MAX_CONCURRENT_RECONCILES), allow_negative_numbers = true)]
    max_concurrent_reconciles: i64,

    /// Enable leader election (required when running more than one replica)
    #[arg(long, num_args = 0..=1, default_value_t = false, default_missing_value = "true", action = clap::ArgAction::Set)]
    leader_elect: bool,

    /// Log level: trace, debug, info, warn, error or off
    #[arg(long, default_value = "info", value_parser = logging::parse_level)]
    log_level: LevelFilter,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print the version number
    Version,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(Command::Version) = cli.command {
        println!("v{VERSION}");
        return ExitCode::SUCCESS;
    }
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}", error::chain(&err));
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let telemetry = Telemetry::init(SERVICE_NAME, VERSION, cli.log_level)?;
    let result = serve(cli, &telemetry).await;
    if let Err(err) = &result {
        error!(error = %error::chain(err), "Controller failed");
    }
    telemetry.shutdown().await;
    result
}

async fn serve(cli: Cli, telemetry: &Telemetry) -> anyhow::Result<()> {
    let builder_image = cli.builder_image.clone().unwrap_or_default();
    if builder_image.is_empty() {
        bail!("--builder-image must not be empty");
    }
    let pull_policy = controller::parse_pull_policy(&cli.builder_image_pull_policy)
        .map_err(anyhow::Error::msg)?;
    let default_resources = controller::parse_resources(&cli.builder_requests, &cli.builder_limits)
        .map_err(anyhow::Error::msg)?;
    let workers = match cli.max_concurrent_reconciles {
        n if n <= 0 => controller::DEFAULT_MAX_CONCURRENT_RECONCILES,
        n => u16::try_from(n).unwrap_or(u16::MAX),
    };
    let namespace = non_empty(&cli.namespace).unwrap_or_else(|| "default".to_string());

    let shutdown = shutdown::token();
    let client = Client::try_default()
        .await
        .context("get Kubernetes config")?;

    // Check the secret up front so misconfiguration fails fast, and detect
    // whether a shared builder host key should be mounted.
    let mount_host_key =
        secret_has_builder_host_key(&client, &namespace, &cli.ssh_key_secret).await?;

    let identity = leader_identity();
    let reporter = Reporter {
        controller: REPORTER_NAME.to_string(),
        instance: Some(identity.clone()),
    };
    let metrics = Metrics::new(&telemetry.metrics().meter());
    let reconciler = Reconciler::new(
        KubeControllerClient::new(client.clone(), reporter),
        ReconcilerConfig {
            builder_image: builder_image.clone(),
            builder_image_pull_policy: pull_policy.clone(),
            remote_port: i32::from(cli.remote_port),
            nix_config_map: non_empty(&cli.nix_config),
            ssh_key_secret: cli.ssh_key_secret.clone(),
            mount_builder_host_key: mount_host_key,
            default_resources,
            completed_ttl: cli.completed_ttl,
            builder_service_account: non_empty(&cli.builder_service_account),
        },
        metrics.clone(),
    );

    let health = start_health_server(cli.health_port).await?;
    let metrics_server = telemetry.metrics().serve(&cli.metrics_addr).await?;

    info!(
        version = VERSION,
        builder_image = %builder_image,
        builder_image_pull_policy = pull_policy.as_deref().unwrap_or_default(),
        remote_port = cli.remote_port,
        nix_config = %cli.nix_config,
        ssh_key_secret = %cli.ssh_key_secret,
        mount_builder_host_key = mount_host_key,
        max_concurrent_reconciles = workers,
        completed_ttl = %duration::format(cli.completed_ttl),
        shutdown_timeout = %duration::format(cli.shutdown_timeout),
        leader_election = cli.leader_elect,
        "Starting Nix remote builder controller"
    );

    let runner = Runner {
        client: client.clone(),
        reconciler,
        workers,
        metrics,
        shutdown_timeout: cli.shutdown_timeout,
    };
    let result = if cli.leader_elect {
        let election = LeaderElectionConfig::new(&namespace, LEADER_ELECTION_ID, &identity);
        run_as_leader(runner, LeaderElector::new(client, election), &shutdown).await
    } else {
        runner.run(shutdown).await;
        Ok(())
    };

    if let Some(server) = metrics_server {
        server.shutdown().await;
    }
    health.shutdown().await;
    if result.is_ok() {
        info!("Controller manager stopped");
    }
    result
}

/// Waits for leadership, then reconciles until shutdown. The lease is renewed
/// for as long as reconciling runs, including the drain after a shutdown
/// signal, and released at the end. If leadership is lost, reconciling stops at
/// once, without draining, so it never overlaps with the new leader; the error
/// makes the process exit and restart as a standby.
async fn run_as_leader(
    runner: Runner,
    elector: LeaderElector,
    shutdown: &CancellationToken,
) -> anyhow::Result<()> {
    if !elector.acquire(shutdown).await {
        return Ok(());
    }
    tokio::select! {
        lost = elector.hold() => return Err(lost.into()),
        () = runner.run(shutdown.clone()) => {}
    }
    elector.release().await;
    Ok(())
}

async fn start_health_server(port: u16) -> anyhow::Result<HttpServer> {
    let listener = http::bind(&format!(":{port}"))
        .await
        .context("listen health server")?;
    let ping = || async { "ok" };
    let router = axum::Router::new()
        .route("/healthz", axum::routing::get(ping))
        .route("/readyz", axum::routing::get(ping));
    HttpServer::spawn("Health", listener, router)
}

async fn secret_has_builder_host_key(
    client: &Client,
    namespace: &str,
    name: &str,
) -> anyhow::Result<bool> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let ssh_keys = secrets
        .get(name)
        .await
        .with_context(|| format!("read SSH key secret {namespace}/{name}"))?;
    if secret::entry(&ssh_keys, secret::PUBLIC_KEY).is_none() {
        bail!(
            "SSH key secret {name} is missing the {:?} entry (builder authorized_keys)",
            secret::PUBLIC_KEY
        );
    }
    let has = secret::entry(&ssh_keys, secret::BUILDER_HOST_KEY).is_some();
    if !has {
        warn!(
            "Secret {name} has no {:?} entry; builder pods will generate a fresh host key on every start and the \
             proxy cannot verify them",
            secret::BUILDER_HOST_KEY
        );
    }
    Ok(has)
}

/// A unique identity for leader election and event reporting: the pod name
/// (HOSTNAME in Kubernetes) plus a random suffix.
fn leader_identity() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .and_then(|h| non_empty(&h))
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .and_then(|h| non_empty(&h))
        })
        .unwrap_or_else(|| "controller".to_string());
    format!("{host}_{}", uuid::Uuid::now_v7())
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags() {
        let cli = Cli::try_parse_from([
            "controller",
            "--builder-image=img:1",
            "--leader-elect",
            "--completed-ttl=2m",
            "-n",
            "builds",
            "--max-concurrent-reconciles",
            "-3",
        ])
        .unwrap();
        assert_eq!(cli.builder_image.as_deref(), Some("img:1"));
        assert!(cli.leader_elect);
        assert_eq!(cli.completed_ttl, Duration::from_secs(120));
        assert_eq!(cli.namespace, "builds");
        assert_eq!(cli.max_concurrent_reconciles, -3);
        assert_eq!(cli.shutdown_timeout, Duration::from_secs(30));

        let cli =
            Cli::try_parse_from(["controller", "--builder-image", "x", "--leader-elect=false"])
                .unwrap();
        assert!(!cli.leader_elect);

        assert!(
            Cli::try_parse_from(["controller"]).is_err(),
            "--builder-image is required"
        );
        let version = Cli::try_parse_from(["controller", "version"]).unwrap();
        assert!(matches!(version.command, Some(Command::Version)));
        assert!(
            Cli::try_parse_from(["controller", "--builder-image=x", "--completed-ttl=soon"])
                .is_err()
        );
    }
}
