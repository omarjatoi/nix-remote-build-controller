//! Kubernetes controller and SSH proxy that turn a cluster into an elastic pool
//! of Nix remote builders.
//!
//! The crate builds two binaries:
//!
//! - `proxy`: the SSH front door. Every SSH session channel opened by
//!   a Nix client becomes its own [`api::NixBuildRequest`] and builder pod, and
//!   the channel is spliced onto an SSH session on that pod.
//! - `controller` ([`controller`]): reconciles `NixBuildRequest`s into
//!   short-lived builder pods and reaps them afterwards.

pub mod api;
pub mod controller;
pub mod duration;
pub mod http;
pub mod kube_util;
pub mod logging;
pub mod secret;

#[cfg(test)]
pub(crate) mod testutil;

use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// The version reported by `controller version` and `proxy version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Renders an error and its causes as `outer: inner: root`, skipping causes
/// whose text the message already contains (kube and russh errors embed their
/// sources in their own message, so a plain chain would repeat them).
pub fn error_chain(err: &anyhow::Error) -> String {
    let mut out = String::new();
    for cause in err.chain() {
        let msg = cause.to_string();
        if out.contains(&msg) {
            continue;
        }
        if !out.is_empty() {
            out.push_str(": ");
        }
        out.push_str(&msg);
    }
    out
}

/// A token that is cancelled when SIGINT or SIGTERM is received. Must be
/// called from within a Tokio runtime.
pub fn shutdown_token() -> CancellationToken {
    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutdown signal received");
        cancel.cancel();
    });
    token
}

/// Locks a mutex, ignoring poisoning: every critical section in this crate
/// leaves its data consistent even if it panics part-way.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Resolves once SIGINT or SIGTERM is received.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            error!(error = %err, "Failed to listen for SIGINT");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                error!(error = %err, "Failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_chain_skips_repeated_causes() {
        let root = anyhow::anyhow!("no such file");
        let err = root
            .context("read kubeconfig: no such file")
            .context("get Kubernetes config");
        assert_eq!(
            error_chain(&err),
            "get Kubernetes config: read kubeconfig: no such file"
        );
        let plain = anyhow::anyhow!("a").context("b");
        assert_eq!(error_chain(&plain), "b: a");
    }
}
