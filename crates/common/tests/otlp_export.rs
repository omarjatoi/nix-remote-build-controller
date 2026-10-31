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

/// The name of the test that exports, for running it on its own.
const EXPORTING_TEST: &str = "export_telemetry";

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

    // The settings are environment variables and the logger is global, so the
    // export gets a process of its own.
    let test_binary = std::env::current_exe().expect("path of this test binary");
    let exported = Command::new(test_binary)
        .args(["--exact", EXPORTING_TEST, "--ignored"])
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)
        .status()
        .expect("run the exporting test");
    assert!(exported.success(), "{EXPORTING_TEST} failed: {exported}");

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
