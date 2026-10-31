//! The metrics pipeline: instruments feed a Prometheus endpoint and, when
//! configured, an OTLP exporter.

use anyhow::Context as _;
use axum::Router;
use axum::http::header::CONTENT_TYPE;
use opentelemetry::metrics::Meter;
use opentelemetry_otlp::MetricExporter;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use prometheus::{Registry, TEXT_FORMAT, TextEncoder};
use tracing::warn;

use crate::http::{self, HttpServer};

/// Scope of the instruments of the controller and the proxy.
const METER_NAME: &str = "nix-remote-build-controller";

/// Creates [`Meter`]s and serves what they record.
pub struct MetricsProvider {
    provider: SdkMeterProvider,
    registry: Registry,
}

impl MetricsProvider {
    /// A provider that only serves Prometheus metrics.
    pub fn try_new() -> anyhow::Result<Self> {
        Self::try_new_with_otlp(Resource::builder().build(), None)
    }

    /// A provider that also sends metrics to `otlp`, if given. `resource`
    /// describes this process in those metrics.
    pub fn try_new_with_otlp(
        resource: Resource,
        otlp: Option<MetricExporter>,
    ) -> anyhow::Result<Self> {
        let registry = Registry::new();
        let prometheus = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .scope_info_enabled(false)
            .without_target_info()
            .build()
            .context("create Prometheus exporter")?;
        let mut provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(prometheus);
        if let Some(otlp) = otlp {
            provider = provider.with_periodic_exporter(otlp);
        }
        Ok(Self {
            provider: provider.build(),
            registry,
        })
    }

    /// The meter for this program's instruments.
    #[must_use]
    pub fn meter(&self) -> Meter {
        use opentelemetry::metrics::MeterProvider as _;
        self.provider.meter(METER_NAME)
    }

    /// Renders all metrics in the Prometheus text format.
    #[must_use]
    pub fn render(&self) -> String {
        render(&self.registry)
    }

    /// Serves `/metrics` on `addr`. An empty address or `0` turns this off.
    pub async fn serve(&self, addr: &str) -> anyhow::Result<Option<HttpServer>> {
        let addr = addr.trim();
        if addr.is_empty() || addr == "0" {
            return Ok(None);
        }
        let listener = http::bind(addr).await.context("listen metrics server")?;
        let registry = self.registry.clone();
        let router = Router::new().route(
            "/metrics",
            axum::routing::get(move || {
                let registry = registry.clone();
                async move { ([(CONTENT_TYPE, TEXT_FORMAT)], render(&registry)) }
            }),
        );
        HttpServer::spawn("Metrics", listener, router).map(Some)
    }

    /// Sends the metrics not exported yet and stops exporting. Blocks.
    pub(crate) fn shutdown(&self) {
        if let Err(err) = self.provider.shutdown() {
            warn!(error = %err, "Failed to shut down metrics");
        }
    }
}

fn render(registry: &Registry) -> String {
    TextEncoder::new()
        .encode_to_string(&registry.gather())
        .unwrap_or_else(|err| {
            warn!(error = %err, "Failed to encode metrics");
            String::new()
        })
}

#[cfg(test)]
mod tests {
    use opentelemetry::KeyValue;
    use tokio::net::TcpStream;

    use super::*;

    #[test]
    fn instruments_keep_their_prometheus_names() {
        let provider = MetricsProvider::try_new().unwrap();
        let meter = provider.meter();
        meter
            .u64_counter("test.requests")
            .with_description("Requests, by result")
            .build()
            .add(2, &[KeyValue::new("result", "ok")]);
        meter
            .f64_histogram("test.duration")
            .with_unit("s")
            .with_boundaries(vec![1.0])
            .build()
            .record(0.5, &[]);
        meter.i64_up_down_counter("test.active").build().add(3, &[]);

        let text = provider.render();
        assert!(
            text.contains("test_requests_total{result=\"ok\"} 2"),
            "{text}"
        );
        assert!(
            text.contains("# HELP test_requests_total Requests, by result"),
            "{text}"
        );
        assert!(
            text.contains("test_duration_seconds_bucket{le=\"1\"} 1"),
            "{text}"
        );
        assert!(text.contains("test_active 3"), "{text}");
        assert!(!text.contains("target_info"), "{text}");
        assert!(!text.contains("otel_scope"), "{text}");
    }

    #[tokio::test]
    async fn serves_metrics_over_http() {
        let provider = MetricsProvider::try_new().unwrap();
        provider
            .meter()
            .u64_counter("test.requests")
            .build()
            .add(1, &[]);
        assert!(provider.serve("0").await.unwrap().is_none());
        assert!(provider.serve(" ").await.unwrap().is_none());

        let server = provider
            .serve("127.0.0.1:0")
            .await
            .unwrap()
            .expect("server");
        let port = server.local_addr().port();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("test_requests_total 1"), "{response}");
        server.shutdown().await;
    }
}
