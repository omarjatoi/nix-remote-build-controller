//! Structured JSON logging to stderr.

use std::fmt;

use opentelemetry::Context;
use tracing::level_filters::LevelFilter;
use tracing::{Event, Subscriber};
use tracing_subscriber::filter::Directive;
use tracing_subscriber::fmt::format::{JsonFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer};

use crate::telemetry::TRACE_ONLY_TARGET;

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

/// The layer that writes JSON logs to stderr. Messages from this workspace are
/// logged at `level`; dependencies (kube, russh, hyper) are capped at warn so
/// the output stays focused on the proxy and controller. `RUST_LOG`, when set,
/// overrides the filter entirely.
pub(crate) fn stderr_layer<S>(level: LevelFilter) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let format = tracing_subscriber::fmt::format()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(false)
        .with_target(false);
    tracing_subscriber::fmt::layer()
        .event_format(TraceIdFormat(format))
        .fmt_fields(JsonFields::new())
        .with_writer(std::io::stderr)
        .with_filter(filter(level))
}

/// The filter for logs: see [`stderr_layer`].
pub(crate) fn filter(level: LevelFilter) -> EnvFilter {
    let filter = match std::env::var("RUST_LOG") {
        Ok(spec) if !spec.trim().is_empty() => EnvFilter::new(spec),
        _ => default_filter(level),
    };
    filter.add_directive(off(TRACE_ONLY_TARGET))
}

/// A directive that filters out everything from `target`.
pub(crate) fn off(target: &str) -> Directive {
    format!("{target}=off")
        .parse()
        .expect("a target with a level is a valid directive")
}

/// Adds the current trace and span IDs to the JSON objects of another format,
/// to find the trace of a log line and the other way round.
struct TraceIdFormat<F>(F);

impl<S, N, F> FormatEvent<S, N> for TraceIdFormat<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut line = String::new();
        self.0.format_event(ctx, Writer::new(&mut line), event)?;
        let ids = Context::map_current(|context| {
            use opentelemetry::trace::TraceContextExt as _;
            let span = context.span();
            let span = span.span_context();
            span.is_valid().then(|| {
                format!(
                    ",\"trace_id\":\"{}\",\"span_id\":\"{}\"}}\n",
                    span.trace_id(),
                    span.span_id()
                )
            })
        });
        match (ids, line.strip_suffix("}\n")) {
            (Some(ids), Some(object)) => write!(writer, "{object}{ids}"),
            _ => writer.write_str(&line),
        }
    }
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
pub(crate) const WORKSPACE_CRATES: [&str; 4] = ["api", "common", "controller", "proxy"];

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use opentelemetry_sdk::trace::SdkTracerProvider;
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

    #[test]
    fn log_lines_carry_the_ids_of_their_trace() {
        use opentelemetry::trace::TracerProvider as _;
        use tracing_subscriber::layer::SubscriberExt as _;

        let buf = Buffer::default();
        let provider = SdkTracerProvider::builder().build();
        let format = tracing_subscriber::fmt::format().json().flatten_event(true);
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")))
            .with(
                tracing_subscriber::fmt::layer()
                    .event_format(TraceIdFormat(format))
                    .fmt_fields(JsonFields::new())
                    .with_writer(buf.clone()),
            );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("outside");
            let span = tracing::error_span!("session");
            let _enter = span.enter();
            tracing::info!("inside");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let mut lines = out
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap());
        let outside = lines.next().unwrap();
        assert_eq!(outside["message"], "outside");
        assert!(outside.get("trace_id").is_none());
        let inside = lines.next().unwrap();
        assert_eq!(inside["message"], "inside");
        assert_eq!(inside["trace_id"].as_str().unwrap().len(), 32);
        assert_eq!(inside["span_id"].as_str().unwrap().len(), 16);
    }

    /// Spans that only go to the trace exporter must not hide the fields of
    /// the span around them.
    #[test]
    fn log_lines_show_the_session_inside_trace_only_spans() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let buf = Buffer::default();
        let format = tracing_subscriber::fmt::format()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false);
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .event_format(format)
                .fmt_fields(JsonFields::new())
                .with_writer(buf.clone())
                .with_filter(filter(LevelFilter::INFO)),
        );
        tracing::subscriber::with_default(subscriber, || {
            let session = tracing::error_span!("session", session_id = "abc");
            let _session = session.enter();
            let tunnel = tracing::info_span!(target: TRACE_ONLY_TARGET, "tunnel", other = 1);
            let _tunnel = tunnel.enter();
            tracing::warn!("shown");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let line: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
        assert_eq!(line["span"]["session_id"], "abc", "{out}");
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
