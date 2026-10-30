//! Runs the reconciler under kube-runtime's controller loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::header::CONTENT_TYPE;
use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::controller::{self, Action, Controller};
use kube::runtime::watcher;
use kube::{Api, Client, ResourceExt};
use prometheus::{
    Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, Opts, Registry, TEXT_FORMAT,
    TextEncoder,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::{
    KubeControllerClient, LABEL_APP, LABEL_APP_VALUE, ReconcileAction, ReconcileError, Reconciler,
};
use crate::api::NixBuildRequest;
use crate::lock;

/// Per-object retry backoff after a failed reconcile: 5ms doubling up to 1000s,
/// reset on success.
const BACKOFF_BASE: Duration = Duration::from_millis(5);
const BACKOFF_MAX: Duration = Duration::from_secs(1000);

/// Prometheus metrics for the controller.
pub struct Metrics {
    registry: Registry,
    reconcile_total: IntCounterVec,
    reconcile_errors: IntCounter,
    reconcile_time: Histogram,
    max_concurrent_reconciles: IntGauge,
    active_workers: IntGauge,
}

impl Metrics {
    /// Registers the controller metrics in a fresh registry.
    pub fn new() -> Self {
        let registry = Registry::new();
        let reconcile_total = IntCounterVec::new(
            Opts::new(
                "nix_remote_build_controller_reconcile_total",
                "Reconciles, by result",
            ),
            &["result"],
        )
        .expect("valid metric");
        let reconcile_errors = IntCounter::with_opts(Opts::new(
            "nix_remote_build_controller_reconcile_errors_total",
            "Reconciles that failed and will be retried",
        ))
        .expect("valid metric");
        let reconcile_time = Histogram::with_opts(
            HistogramOpts::new(
                "nix_remote_build_controller_reconcile_duration_seconds",
                "Time taken by each reconcile",
            )
            .buckets(vec![
                0.005, 0.01, 0.025, 0.05, 0.1, 0.15, 0.2, 0.25, 0.3, 0.35, 0.4, 0.45, 0.5, 0.6,
                0.7, 0.8, 0.9, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0, 6.0, 7.0,
                8.0, 9.0, 10.0, 15.0, 20.0, 25.0, 30.0, 40.0, 50.0, 60.0,
            ]),
        )
        .expect("valid metric");
        let max_concurrent_reconciles = IntGauge::with_opts(Opts::new(
            "nix_remote_build_controller_max_concurrent_reconciles",
            "Reconciles allowed to run at the same time",
        ))
        .expect("valid metric");
        let active_workers = IntGauge::with_opts(Opts::new(
            "nix_remote_build_controller_active_reconciles",
            "Reconciles running now",
        ))
        .expect("valid metric");
        for collector in [
            Box::new(reconcile_total.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(reconcile_errors.clone()),
            Box::new(reconcile_time.clone()),
            Box::new(max_concurrent_reconciles.clone()),
            Box::new(active_workers.clone()),
        ] {
            registry
                .register(collector)
                .expect("metric registered once");
        }
        Self {
            registry,
            reconcile_total,
            reconcile_errors,
            reconcile_time,
            max_concurrent_reconciles,
            active_workers,
        }
    }

    /// Renders all metrics in the Prometheus text format.
    pub fn render(&self) -> String {
        TextEncoder::new()
            .encode_to_string(&self.registry.gather())
            .unwrap_or_else(|err| {
                warn!(error = %err, "Failed to encode metrics");
                String::new()
            })
    }

    /// An HTTP router serving `/metrics`.
    pub fn router(self: Arc<Self>) -> axum::Router {
        axum::Router::new().route(
            "/metrics",
            axum::routing::get(move || {
                let metrics = Arc::clone(&self);
                async move { ([(CONTENT_TYPE, TEXT_FORMAT)], metrics.render()) }
            }),
        )
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

struct Context {
    reconciler: Reconciler<KubeControllerClient>,
    metrics: Arc<Metrics>,
    /// Consecutive failures per (namespace, name), for error backoff.
    failures: Mutex<HashMap<(String, String), u32>>,
}

fn object_key(obj: &NixBuildRequest) -> (String, String) {
    (obj.namespace().unwrap_or_default(), obj.name_any())
}

/// Counts a reconcile in the active-workers gauge for as long as it lives.
struct ActiveWorker<'a>(&'a IntGauge);

impl<'a> ActiveWorker<'a> {
    fn start(gauge: &'a IntGauge) -> Self {
        gauge.inc();
        Self(gauge)
    }
}

impl Drop for ActiveWorker<'_> {
    fn drop(&mut self) {
        self.0.dec();
    }
}

impl From<ReconcileAction> for Action {
    fn from(action: ReconcileAction) -> Self {
        match action {
            ReconcileAction::Requeue(after) => Action::requeue(after),
            ReconcileAction::AwaitChange => Action::await_change(),
        }
    }
}

async fn reconcile(obj: Arc<NixBuildRequest>, ctx: Arc<Context>) -> Result<Action, ReconcileError> {
    let metrics = &ctx.metrics;
    let _worker = ActiveWorker::start(&metrics.active_workers);
    let start = Instant::now();
    let result = ctx.reconciler.reconcile(&obj).await;
    metrics
        .reconcile_time
        .observe(start.elapsed().as_secs_f64());

    let outcome = match &result {
        Ok(ReconcileAction::Requeue(_)) => "requeue_after",
        Ok(ReconcileAction::AwaitChange) => "success",
        Err(_) => "error",
    };
    metrics.reconcile_total.with_label_values(&[outcome]).inc();
    match result {
        Ok(action) => {
            lock(&ctx.failures).remove(&object_key(&obj));
            Ok(action.into())
        }
        Err(err) => {
            metrics.reconcile_errors.inc();
            Err(err)
        }
    }
}

fn error_policy(obj: Arc<NixBuildRequest>, err: &ReconcileError, ctx: Arc<Context>) -> Action {
    let mut failures = lock(&ctx.failures);
    let count = failures.entry(object_key(&obj)).or_insert(0);
    let delay = backoff(*count);
    *count = count.saturating_add(1);
    error!(
        build_request = %obj.name_any(),
        namespace = %obj.namespace().unwrap_or_default(),
        error = %err,
        retry_in = %crate::duration::format(delay),
        "Reconciler error"
    );
    Action::requeue(delay)
}

/// The delay before retry number `failures` (zero-based).
fn backoff(failures: u32) -> Duration {
    BACKOFF_BASE
        .checked_mul(1u32.checked_shl(failures.min(31)).unwrap_or(u32::MAX))
        .map_or(BACKOFF_MAX, |d| d.min(BACKOFF_MAX))
}

/// Everything the controller loop needs.
pub struct Runner {
    pub client: Client,
    pub reconciler: Reconciler<KubeControllerClient>,
    /// Number of requests reconciled concurrently.
    pub workers: u16,
    pub metrics: Arc<Metrics>,
    /// How long in-flight reconciles may run after shutdown is requested.
    pub shutdown_timeout: Duration,
}

impl Runner {
    /// Runs the controller until `shutdown` is cancelled, then waits up to the
    /// shutdown timeout for in-flight reconciles. In-flight `NixBuildRequest`s
    /// are left untouched: their state is persisted and the next controller
    /// instance resumes them.
    pub async fn run(self, shutdown: CancellationToken) {
        let Self {
            client,
            reconciler,
            workers,
            metrics,
            shutdown_timeout,
        } = self;
        metrics.max_concurrent_reconciles.set(i64::from(workers));
        let ctx = Arc::new(Context {
            reconciler,
            metrics,
            failures: Mutex::new(HashMap::new()),
        });

        let requests: Api<NixBuildRequest> = Api::all(client.clone());
        // Only builder pods matter; filtering the watch keeps the cache small.
        let pods: Api<Pod> = Api::all(client);
        let pod_selector = format!("{LABEL_APP}={LABEL_APP_VALUE}");

        let stop = shutdown.clone();
        let failures = Arc::clone(&ctx);
        let controller = Controller::new(requests, watcher::Config::default())
            .owns(pods, watcher::Config::default().labels(&pod_selector))
            .with_config(controller::Config::default().concurrency(workers))
            .graceful_shutdown_on(async move { stop.cancelled().await })
            .run(reconcile, error_policy, ctx)
            .for_each(move |result| {
                match result {
                    Ok(_) => {}
                    // Logged and retried by error_policy.
                    Err(controller::Error::ReconcilerFailed(..)) => {}
                    Err(controller::Error::ObjectNotFound(obj)) => {
                        debug!(object = %obj, "Object vanished before it could be reconciled");
                        // Forget its retry backoff.
                        let key = (obj.namespace.clone().unwrap_or_default(), obj.name.clone());
                        lock(&failures.failures).remove(&key);
                    }
                    Err(err) => warn!(error = %err, "Controller watch error"),
                }
                std::future::ready(())
            });

        let drain_deadline = async {
            shutdown.cancelled().await;
            tokio::time::sleep(shutdown_timeout).await;
        };
        tokio::select! {
            () = controller => info!("Controller stopped"),
            () = drain_deadline => warn!(
                timeout = %crate::duration::format(shutdown_timeout),
                "Timed out waiting for in-flight reconciles"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(0), Duration::from_millis(5));
        assert_eq!(backoff(1), Duration::from_millis(10));
        assert_eq!(backoff(3), Duration::from_millis(40));
        assert_eq!(backoff(30), BACKOFF_MAX);
        assert_eq!(backoff(u32::MAX), BACKOFF_MAX);
    }

    #[test]
    fn metrics_render() {
        let metrics = Metrics::new();
        metrics
            .reconcile_total
            .with_label_values(&["success"])
            .inc();
        metrics.max_concurrent_reconciles.set(10);
        let text = metrics.render();
        assert!(text.contains("nix_remote_build_controller_reconcile_total{result=\"success\"} 1"));
        assert!(text.contains("nix_remote_build_controller_max_concurrent_reconciles 10"));
    }
}
