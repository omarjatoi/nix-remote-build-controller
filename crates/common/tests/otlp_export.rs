//! Exports telemetry to a local OTLP receiver, the way the programs do. This is
//! a test binary of its own because the settings are environment variables and
//! the logger is global.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::Uri;
use axum::routing::post;
use tokio::net::TcpListener;
use tracing::level_filters::LevelFilter;

use common::{TRACE_ONLY_TARGET, Telemetry};

#[test]
fn telemetry_reaches_the_collector_on_shutdown() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
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

    // SAFETY: this is the only test in this binary, and the runtime's threads
    // do not read the environment.
    unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint) };

    runtime.block_on(async {
        let telemetry = Telemetry::init("test", "1.2.3", LevelFilter::INFO).expect("telemetry");
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
    });

    let mut paths = paths.lock().unwrap().clone();
    paths.sort();
    paths.dedup();
    assert_eq!(paths, ["/v1/logs", "/v1/metrics", "/v1/traces"]);
}
