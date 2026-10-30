//! Structured JSON logging to stderr.

use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

/// Parses a `--log-level` value. The accepted names match zerolog's, which the
/// deployment manifests were written against: `trace`, `debug`, `info`,
/// `warn`, `error`, plus `fatal`, `panic` and `disabled`, which silence the
/// logs (nothing is logged above error level).
pub fn parse_level(level: &str) -> Result<LevelFilter, String> {
    match level.trim().to_ascii_lowercase().as_str() {
        "trace" => Ok(LevelFilter::TRACE),
        "debug" => Ok(LevelFilter::DEBUG),
        "info" | "" => Ok(LevelFilter::INFO),
        "warn" | "warning" => Ok(LevelFilter::WARN),
        "error" => Ok(LevelFilter::ERROR),
        "fatal" | "panic" | "disabled" | "off" => Ok(LevelFilter::OFF),
        other => Err(format!(
            "unknown level {other:?} (want trace, debug, info, warn, error or disabled)"
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
        _ => {
            let deps = level.min(LevelFilter::WARN);
            EnvFilter::new(format!(
                "{deps},nix_remote_build_controller={level},controller={level},proxy={level}"
            ))
        }
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
        assert_eq!(parse_level("disabled"), Ok(LevelFilter::OFF));
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
