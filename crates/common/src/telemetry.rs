//! Logs, traces and metrics of the controller and the proxy.
//!
//! Logs always go to stderr as JSON, and metrics can always be scraped in the
//! Prometheus format. Sending logs, traces and metrics over OTLP is configured
//! with the standard `OTEL_*` environment variables and is off until an OTLP
//! endpoint is set.

use anyhow::Context as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{Key, KeyValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::level_filters::LevelFilter;
use tracing::{Span, info, warn};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::logging;
use crate::metrics_provider::MetricsProvider;

/// Target of spans that only go to the trace exporter. Log lines show the
/// fields of the innermost span they are logged in, so a span that divides up
/// a session uses this target to keep the session's fields in those lines.
pub const TRACE_ONLY_TARGET: &str = "trace_only";

/// What is sent to the trace exporter: this workspace's spans, and the
/// Kubernetes API calls made while reconciling. The API calls made outside of
/// a reconcile (watches, lease renewals, polling) would each be a trace of
/// their own, or swamp the session they belong to.
const TRACE_FILTER: &str = "off,api=info,common=info,controller=info,proxy=info,trace_only=info,\
                            kube_client[reconcile]=debug";

/// Crates that take part in exporting logs. Their own logs must not be
/// exported, or each export would cause another one.
const EXPORT_CRATES: [&str; 5] = ["h2", "hyper", "hyper_util", "opentelemetry", "reqwest"];

/// The signals that are sent over OTLP.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OtlpSignals {
    traces: bool,
    metrics: bool,
    logs: bool,
}

impl OtlpSignals {
    /// Reads the settings through `var`, which looks up an environment
    /// variable. A signal is sent if `OTEL_EXPORTER_OTLP_ENDPOINT` or its own
    /// endpoint is set, unless its exporter is `none`. `OTEL_SDK_DISABLED`
    /// turns off all of them.
    fn from_env(var: impl Fn(&str) -> Option<String>) -> Self {
        let is_set = |name: &str| var(name).is_some_and(|value| !value.trim().is_empty());
        if var("OTEL_SDK_DISABLED").is_some_and(|v| v.trim().eq_ignore_ascii_case("true")) {
            return Self::default();
        }
        let enabled = |signal: &str| {
            (is_set("OTEL_EXPORTER_OTLP_ENDPOINT")
                || is_set(&format!("OTEL_EXPORTER_OTLP_{signal}_ENDPOINT")))
                && var(&format!("OTEL_{signal}_EXPORTER")).is_none_or(|e| e.trim() != "none")
        };
        Self {
            traces: enabled("TRACES"),
            metrics: enabled("METRICS"),
            logs: enabled("LOGS"),
        }
    }
}

/// The running telemetry pipelines of a program.
#[must_use = "call `shutdown` before exiting, or the last telemetry is lost"]
pub struct Telemetry {
    metrics: MetricsProvider,
    tracer_provider: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
}

impl Telemetry {
    /// Installs the global logger, messages from this workspace being logged
    /// at `level` (see [`logging`]), and starts the pipelines. `service` names
    /// this program in the exported data unless `OTEL_SERVICE_NAME` does.
    ///
    /// Fails if the OTLP settings are invalid. A collector that cannot be
    /// reached later on only loses telemetry.
    pub fn init(service: &str, version: &str, level: LevelFilter) -> anyhow::Result<Self> {
        let otlp = OtlpSignals::from_env(|name| std::env::var(name).ok());
        if otlp != OtlpSignals::default() {
            // There can be more than one caller; all ask for the same provider.
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        let resource = resource(service, version);

        let tracer_provider = otlp
            .traces
            .then(|| {
                let exporter = SpanExporter::builder()
                    .build()
                    .context("create OTLP trace exporter")?;
                anyhow::Ok(
                    SdkTracerProvider::builder()
                        .with_resource(resource.clone())
                        .with_batch_exporter(exporter)
                        .build(),
                )
            })
            .transpose()?;
        let logger_provider = otlp
            .logs
            .then(|| {
                let exporter = LogExporter::builder()
                    .build()
                    .context("create OTLP log exporter")?;
                anyhow::Ok(
                    SdkLoggerProvider::builder()
                        .with_resource(resource.clone())
                        .with_batch_exporter(exporter)
                        .build(),
                )
            })
            .transpose()?;
        let metric_exporter = otlp
            .metrics
            .then(|| {
                MetricExporter::builder()
                    .build()
                    .context("create OTLP metric exporter")
            })
            .transpose()?;
        let metrics = MetricsProvider::with_otlp(resource, metric_exporter)?;

        let trace_layer = tracer_provider.as_ref().map(|provider| {
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("nix-remote-build-controller"))
                .with_filter(tracing_subscriber::EnvFilter::new(TRACE_FILTER))
        });
        let log_layer = logger_provider.as_ref().map(|provider| {
            let filter = EXPORT_CRATES
                .iter()
                .fold(logging::filter(level), |filter, name| {
                    filter.add_directive(format!("{name}=off").parse().expect("valid directive"))
                });
            OpenTelemetryTracingBridge::new(provider).with_filter(filter)
        });
        // Replacing an installed subscriber only happens in tests.
        let _ = tracing_subscriber::registry()
            .with(logging::stderr_layer(level))
            .with(trace_layer)
            .with(log_layer)
            .try_init();

        info!(
            traces = otlp.traces,
            metrics = otlp.metrics,
            logs = otlp.logs,
            "OTLP export configured"
        );
        Ok(Self {
            metrics,
            tracer_provider,
            logger_provider,
        })
    }

    /// The metrics pipeline.
    pub fn metrics(&self) -> &MetricsProvider {
        &self.metrics
    }

    /// Exports what has not been exported yet and stops the pipelines. Waits
    /// for the exporters, which are bounded by the OTLP timeouts.
    pub async fn shutdown(self) {
        let stopped = tokio::task::spawn_blocking(move || {
            if let Some(provider) = &self.tracer_provider
                && let Err(err) = provider.shutdown()
            {
                warn!(error = %err, "Failed to shut down trace export");
            }
            self.metrics.shutdown();
            if let Some(provider) = &self.logger_provider
                && let Err(err) = provider.shutdown()
            {
                warn!(error = %err, "Failed to shut down log export");
            }
        });
        if let Err(err) = stopped.await {
            warn!(error = %err, "Failed to shut down telemetry");
        }
    }
}

/// Marks `span` as failed. It must have declared the fields `otel.status_code`
/// and `otel.status_description`.
pub fn record_error(span: &Span, message: &str) {
    span.record("otel.status_code", "error");
    span.record("otel.status_description", message);
}

/// Describes this process. `OTEL_RESOURCE_ATTRIBUTES` can add to this, for
/// example the name of the pod.
fn resource(service: &str, version: &str) -> Resource {
    let detected = Resource::builder().build();
    let named = detected
        .get(&Key::from_static_str("service.name"))
        .is_some_and(|name| !name.as_str().starts_with("unknown_service"));
    let builder =
        Resource::builder().with_attribute(KeyValue::new("service.version", version.to_string()));
    if named {
        builder.build()
    } else {
        builder.with_service_name(service.to_string()).build()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn signals(vars: &[(&str, &str)]) -> OtlpSignals {
        let vars: HashMap<&str, &str> = vars.iter().copied().collect();
        OtlpSignals::from_env(|name| vars.get(name).map(ToString::to_string))
    }

    #[test]
    fn otlp_is_off_without_an_endpoint() {
        assert_eq!(signals(&[]), OtlpSignals::default());
        assert_eq!(
            signals(&[("OTEL_EXPORTER_OTLP_ENDPOINT", " ")]),
            OtlpSignals::default()
        );
    }

    #[test]
    fn the_endpoint_enables_every_signal() {
        let all = OtlpSignals {
            traces: true,
            metrics: true,
            logs: true,
        };
        assert_eq!(
            signals(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318")]),
            all
        );
    }

    #[test]
    fn signals_can_be_chosen_one_by_one() {
        assert_eq!(
            signals(&[(
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "http://c:4318/v1/traces"
            )]),
            OtlpSignals {
                traces: true,
                ..Default::default()
            }
        );
        assert_eq!(
            signals(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
                ("OTEL_LOGS_EXPORTER", "none"),
            ]),
            OtlpSignals {
                traces: true,
                metrics: true,
                logs: false,
            }
        );
    }

    #[test]
    fn the_sdk_can_be_disabled() {
        assert_eq!(
            signals(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
                ("OTEL_SDK_DISABLED", "TRUE"),
            ]),
            OtlpSignals::default()
        );
    }

    #[test]
    fn the_resource_names_the_service() {
        let resource = resource("nix-remote-build-proxy", "1.2.3");
        let get = |key: &'static str| {
            resource
                .get(&Key::from_static_str(key))
                .map(|v| v.to_string())
        };
        assert_eq!(
            get("service.name").as_deref(),
            Some("nix-remote-build-proxy")
        );
        assert_eq!(get("service.version").as_deref(), Some("1.2.3"));
    }
}
