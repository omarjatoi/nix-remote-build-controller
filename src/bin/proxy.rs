//! The SSH proxy: forwards each Nix build session to its own builder pod.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use nix_remote_build_controller::duration::clap_parser as parse_duration;
use nix_remote_build_controller::proxy::{self, SshProxy};
use nix_remote_build_controller::{VERSION, error_chain, logging, shutdown_token};
use tracing::info;
use tracing::level_filters::LevelFilter;

/// SSH proxy for Nix remote builders.
///
/// Forwards each Nix build session to its own Kubernetes builder pod, so
/// builds run in parallel.
#[derive(Parser, Debug)]
#[command(name = "proxy", args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// SSH proxy server port
    #[arg(short = 'p', long, default_value_t = 2222)]
    port: u16,

    /// Health check server port
    #[arg(long, default_value_t = 8080)]
    health_port: u16,

    /// Path to the proxy's SSH host private key (default: 'host-key' entry of
    /// the SSH key secret, else an ephemeral key)
    #[arg(short = 'k', long = "host-key", default_value = "")]
    host_key: String,

    /// Path to an authorized_keys file of Nix clients allowed to connect
    /// (default: 'client-authorized-keys' entry of the SSH key secret; if
    /// neither is set, authentication is disabled)
    #[arg(long, default_value = "")]
    client_authorized_keys: String,

    /// Kubernetes namespace for build requests
    #[arg(short = 'n', long, default_value = "default")]
    namespace: String,

    /// SSH username on builder pods
    #[arg(short = 'u', long, default_value = "nixbld")]
    remote_user: String,

    /// SSH port on builder pods
    #[arg(short = 'r', long, default_value_t = 22)]
    remote_port: u16,

    /// Secret containing the SSH keypair used to authenticate to builder pods
    /// (keys 'private' and 'public')
    #[arg(long, default_value = "nix-builder-ssh-keys")]
    ssh_key_secret: String,

    /// How long a session waits for its builder pod to become ready
    #[arg(long, default_value = "5m", value_parser = parse_duration)]
    pod_ready_timeout: Duration,

    /// Maximum lifetime of a builder pod (NixBuildRequest.spec.timeoutSeconds);
    /// 0 uses the CRD default
    #[arg(long, default_value = "1h", value_parser = parse_duration)]
    build_timeout: Duration,

    /// How long in-flight build sessions may continue after SIGTERM before
    /// being closed
    #[arg(long, default_value = "30m", value_parser = parse_duration)]
    shutdown_timeout: Duration,

    /// SSH keepalive interval towards clients and builders (0 disables)
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    keepalive_interval: Duration,

    /// Maximum number of concurrent build sessions (0 = unlimited)
    #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
    max_sessions: i64,

    /// Name of this proxy's pod; NixBuildRequests get an ownerReference to it
    #[arg(long, env = "PROXY_POD_NAME", default_value = "")]
    owner_pod_name: String,

    /// UID of this proxy's pod
    #[arg(long, env = "PROXY_POD_UID", default_value = "")]
    owner_pod_uid: String,

    /// Log level (trace, debug, info, warn, error)
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
            eprintln!("{}", error_chain(&err));
            ExitCode::FAILURE
        }
    }
}

fn config(cli: &Cli) -> proxy::Config {
    proxy::Config {
        listen_addr: format!(":{}", cli.port),
        health_addr: format!(":{}", cli.health_port),
        namespace: cli.namespace.clone(),
        remote_user: cli.remote_user.clone(),
        remote_port: cli.remote_port,
        pod_ready_timeout: cli.pod_ready_timeout,
        build_timeout: cli.build_timeout,
        shutdown_timeout: cli.shutdown_timeout,
        keepalive_interval: cli.keepalive_interval,
        max_sessions: usize::try_from(cli.max_sessions).unwrap_or(0),
        owner_pod_name: cli.owner_pod_name.clone(),
        owner_pod_uid: cli.owner_pod_uid.clone(),
        ..Default::default()
    }
}

fn optional_path(s: &str) -> Option<PathBuf> {
    (!s.is_empty()).then(|| PathBuf::from(s))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    logging::init(cli.log_level);

    let shutdown = shutdown_token();
    let host_key = optional_path(&cli.host_key);
    let client_keys = optional_path(&cli.client_authorized_keys);
    let ssh_proxy = SshProxy::from_cluster(
        config(&cli),
        host_key.as_deref(),
        &cli.ssh_key_secret,
        client_keys.as_deref(),
    )
    .await
    .context("create SSH proxy")?;

    info!(
        version = VERSION,
        port = cli.port,
        "Starting Nix remote builder SSH proxy"
    );
    ssh_proxy.run(shutdown).await.context("SSH proxy failed")?;
    info!("SSH proxy stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_like_the_go_version() {
        let cli = Cli::try_parse_from([
            "proxy",
            "-p",
            "2200",
            "--pod-ready-timeout=3m",
            "--build-timeout=20m",
            "--keepalive-interval=0",
            "--max-sessions=4",
            "-u",
            "builder",
            "--owner-pod-name=proxy-0",
        ])
        .unwrap();
        let cfg = config(&cli);
        assert_eq!(cfg.listen_addr, ":2200");
        assert_eq!(cfg.health_addr, ":8080");
        assert_eq!(cfg.pod_ready_timeout, Duration::from_secs(180));
        assert_eq!(cfg.build_timeout, Duration::from_secs(1200));
        assert!(cfg.keepalive_interval.is_zero());
        assert_eq!(cfg.max_sessions, 4);
        assert_eq!(cfg.remote_user, "builder");
        assert_eq!(cfg.owner_pod_name, "proxy-0");
        assert_eq!(cfg.shutdown_timeout, Duration::from_secs(1800));

        let negative = Cli::try_parse_from(["proxy", "--max-sessions", "-1"]).unwrap();
        assert_eq!(config(&negative).max_sessions, 0);
        assert!(matches!(
            Cli::try_parse_from(["proxy", "version"]).unwrap().command,
            Some(Command::Version)
        ));
    }
}
