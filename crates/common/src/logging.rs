//! Structured JSON logging to stderr.

use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

/// Parses a `--log-level` value: `trace`, `debug`, `info`, `warn`, `error` or
/// `off`.
pub fn parse_level(level: &str) -> Result<LevelFilter, String> {
    match level.trim().to_ascii_lowercase().as_str() {
        "trace" => Ok(LevelFilter::TRACE),
        "debug" => Ok(LevelFilter::DEBUG),
        "info" => Ok(LevelFilter::INFO),
        "warn" => Ok(LevelFilter::WARN),
        "error" => Ok(LevelFilter::ERROR),
        "off" => Ok(LevelFilter::OFF),
        other => Err(format!(
            "unknown level {other:?} (want trace, debug, info, warn, error or off)"
        )),
    }
}

/// Installs the global JSON logger. Messages from this crate are logged at
/// `level`; dependencies (kube, russh, hyper) are capped at warn so the output
/// stays focused on the proxy and controller. `RUST_LOG`, when set, overrides
/// the filter entirely.
pub fn init(level: LevelFilter) {
    let filter = match std::env::var("RUST_LOG") {
        Ok(spec) if !spec.trim().is_empty() => EnvFilter::new(spec),
        _ => default_filter(level),
    };
    let _ = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(false)
        .with_target(false)
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Logs this workspace's crates at `level` and everything else at warn or
/// below.
fn default_filter(level: LevelFilter) -> EnvFilter {
    let deps = level.min(LevelFilter::WARN);
    let ours = WORKSPACE_CRATES
        .iter()
        .map(|krate| format!(",{krate}={level}"))
        .collect::<String>();
    EnvFilter::new(format!("{deps}{ours}"))
}

/// The crates of this workspace, as tracing targets.
const WORKSPACE_CRATES: [&str; 4] = ["api", "common", "controller", "proxy"];

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    #[test]
    fn parses_known_levels() {
        assert_eq!(parse_level("info"), Ok(LevelFilter::INFO));
        assert_eq!(parse_level("DEBUG"), Ok(LevelFilter::DEBUG));
        assert_eq!(parse_level("warn"), Ok(LevelFilter::WARN));
        assert_eq!(parse_level("off"), Ok(LevelFilter::OFF));
        assert!(parse_level("disabled").is_err());
        assert!(parse_level("verbose").is_err());
    }

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Buffer;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn default_filter_logs_every_workspace_crate() {
        let buf = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(default_filter(LevelFilter::INFO))
            .with_writer(buf.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "api::controller_client", "from api");
            tracing::info!(target: "common::http", "from common");
            tracing::info!(target: "controller::runner", "from controller");
            tracing::info!(target: "proxy::server", "from proxy");
            tracing::info!(target: "kube_client::client", "from kube");
            tracing::warn!(target: "kube_client::client", "kube warning");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        for krate in WORKSPACE_CRATES {
            assert!(
                out.contains(&format!("from {krate}")),
                "{krate} info is logged: {out}"
            );
        }
        assert!(
            !out.contains("from kube"),
            "dependency info is hidden: {out}"
        );
        assert!(
            out.contains("kube warning"),
            "dependency warnings are logged: {out}"
        );
    }

    /// Context spans must survive a warn-only filter, or warnings lose the
    /// connection and session they belong to.
    #[test]
    fn warnings_keep_span_fields_at_warn_level() {
        let buf = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .with_max_level(LevelFilter::WARN)
            .with_writer(buf.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::error_span!("session", session_id = "abc");
            let _enter = span.enter();
            tracing::info!("hidden");
            tracing::warn!("shown");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(!out.contains("hidden"));
        let line: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
        assert_eq!(line["message"], "shown");
        assert_eq!(line["span"]["session_id"], "abc");
    }
}
