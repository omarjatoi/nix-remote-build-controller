//! Exports telemetry to a local OTLP receiver, the way the programs do.

use std::process::Command;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::Uri;
use axum::routing::post;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tracing::level_filters::LevelFilter;

use common::{TRACE_ONLY_TARGET, Telemetry};

/// Runs an ignored test of this binary in a process of its own, because the
/// settings are environment variables and the logger is global.
fn run_with_endpoint(test: &str, endpoint: &str) {
    let test_binary = std::env::current_exe().expect("path of this test binary");
    let status = Command::new(test_binary)
        .args(["--exact", test, "--ignored"])
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)
        .status()
        .expect("run the test");
    assert!(status.success(), "{test} failed: {status}");
}

#[test]
fn telemetry_reaches_the_collector_on_shutdown() {
    let runtime = Runtime::new().unwrap();
    let paths = Arc::new(Mutex::new(Vec::<String>::new()));

    let listener = runtime
        .block_on(TcpListener::bind("127.0.0.1:0"))
        .expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::clone(&paths);
    let receiver = Router::new().fallback(post(move |uri: Uri| {
        seen.lock().unwrap().push(uri.path().to_string());
        async { "" }
    }));
    runtime.spawn(async move { axum::serve(listener, receiver).await });

    run_with_endpoint("export_telemetry", &endpoint);

    let mut paths = paths.lock().unwrap().clone();
    paths.sort();
    paths.dedup();
    assert_eq!(
        paths,
        ["/v1/logs", "/v1/metrics", "/v1/traces"],
        "each signal has its own OTLP path"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "run by telemetry_reaches_the_collector_on_shutdown, which sets up its environment"]
async fn export_telemetry() {
    let telemetry = Telemetry::try_new("test", "1.2.3", LevelFilter::INFO).expect("telemetry");
    telemetry
        .metrics()
        .meter()
        .u64_counter("test.requests")
        .build()
        .add(1, &[]);
    tracing::error_span!("session").in_scope(|| {
        tracing::info_span!(target: TRACE_ONLY_TARGET, "tunnel").in_scope(|| {
            tracing::info!("Exported");
        });
    });
    telemetry.shutdown().await;
}

#[test]
fn programs_run_without_export_when_it_cannot_be_set_up() {
    run_with_endpoint("run_with_a_broken_exporter", "http://[not-a-valid-url");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "run by programs_run_without_export_when_it_cannot_be_set_up, which sets up its environment"]
async fn run_with_a_broken_exporter() {
    let telemetry = Telemetry::try_new("test", "1.2.3", LevelFilter::INFO)
        .expect("broken OTLP settings must not stop the program");
    let metrics = telemetry.metrics();
    metrics
        .meter()
        .u64_counter("test.requests")
        .build()
        .add(1, &[]);
    assert!(
        metrics.render().contains("test_requests_total 1"),
        "Prometheus metrics work without OTLP: {}",
        metrics.render()
    );
    telemetry.shutdown().await;
}
