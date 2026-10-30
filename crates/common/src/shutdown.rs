//! Shutdown on SIGINT and SIGTERM.

use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// A token that is cancelled when SIGINT or SIGTERM is received. Must be
/// called from within a Tokio runtime.
pub fn token() -> CancellationToken {
    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        signal().await;
        info!("Shutdown signal received");
        cancel.cancel();
    });
    token
}

/// Resolves once SIGINT or SIGTERM is received.
async fn signal() {
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
