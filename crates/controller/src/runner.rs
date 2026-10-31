//! Runs the reconciler under kube-runtime's controller loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::controller::{self, Action, Controller};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;
use kube::{Api, Client, ResourceExt};
use opentelemetry::KeyValue;
use opentelemetry::metrics::UpDownCounter;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use api::{KubeControllerClient, NixBuildRequest};
use common::duration;

use crate::{LABEL_APP, LABEL_APP_VALUE, Metrics, ReconcileAction, ReconcileError, Reconciler};

/// Per-object retry backoff after a failed reconcile: 5ms doubling up to 1000s,
/// reset on success.
const BACKOFF_BASE: Duration = Duration::from_millis(5);
const BACKOFF_MAX: Duration = Duration::from_secs(1000);

struct Context {
    reconciler: Reconciler<KubeControllerClient>,
    metrics: Metrics,
    /// Consecutive failures per object, for error backoff.
    failures: Mutex<HashMap<ObjectRef<NixBuildRequest>, u32>>,
}

/// Counts a reconcile as active for as long as it lives.
struct ActiveWorker<'counter>(&'counter UpDownCounter<i64>);

impl<'counter> ActiveWorker<'counter> {
    fn start(counter: &'counter UpDownCounter<i64>) -> Self {
        counter.add(1, &[]);
        Self(counter)
    }
}

impl Drop for ActiveWorker<'_> {
    fn drop(&mut self) {
        self.0.add(-1, &[]);
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
        .record(start.elapsed().as_secs_f64(), &[]);

    let outcome = match &result {
        Ok(ReconcileAction::Requeue(_)) => "requeue_after",
        Ok(ReconcileAction::AwaitChange) => "success",
        Err(_) => "error",
    };
    metrics
        .reconcile_total
        .add(1, &[KeyValue::new("result", outcome)]);
    match result {
        Ok(action) => {
            ctx.failures
                .lock()
                .expect("mutex poisoned")
                .remove(&ObjectRef::from_obj(&*obj));
            Ok(action.into())
        }
        Err(err) => {
            metrics.reconcile_errors.add(1, &[]);
            Err(err)
        }
    }
}

fn error_policy(obj: Arc<NixBuildRequest>, err: &ReconcileError, ctx: Arc<Context>) -> Action {
    let mut failures = ctx.failures.lock().expect("mutex poisoned");
    let count = failures.entry(ObjectRef::from_obj(&*obj)).or_insert(0);
    let delay = backoff(*count);
    *count = count.saturating_add(1);
    error!(
        build_request = %obj.name_any(),
        namespace = %obj.namespace().unwrap_or_default(),
        error = %err,
        retry_in = %duration::format(delay),
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
    /// Client for the watches.
    pub client: Client,
    /// Reconciles each `NixBuildRequest`.
    pub reconciler: Reconciler<KubeControllerClient>,
    /// Number of requests reconciled concurrently.
    pub workers: u16,
    /// Where reconcile metrics are recorded.
    pub metrics: Metrics,
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
        metrics
            .max_concurrent_reconciles
            .record(i64::from(workers), &[]);
        metrics.leader.record(1, &[]);
        let leader = metrics.leader.clone();
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
        let mut results = Controller::new(requests, watcher::Config::default())
            .owns(pods, watcher::Config::default().labels(&pod_selector))
            .with_config(controller::Config::default().concurrency(workers))
            .graceful_shutdown_on(async move { stop.cancelled().await })
            .run(reconcile, error_policy, Arc::clone(&ctx))
            .boxed();
        let controller = async {
            while let Some(result) = results.next().await {
                match result {
                    Ok(_) => {}
                    // Logged and retried by error_policy.
                    Err(controller::Error::ReconcilerFailed(..)) => {}
                    Err(controller::Error::ObjectNotFound(obj)) => {
                        debug!(object = %obj, "Object vanished before it could be reconciled");
                        // Forget its retry backoff.
                        let mut key: ObjectRef<NixBuildRequest> = ObjectRef::new(&obj.name);
                        key.namespace.clone_from(&obj.namespace);
                        ctx.failures.lock().expect("mutex poisoned").remove(&key);
                    }
                    Err(err) => warn!(error = %err, "Controller watch error"),
                }
            }
        };

        let drain_deadline = async {
            shutdown.cancelled().await;
            tokio::time::sleep(shutdown_timeout).await;
        };
        tokio::select! {
            () = controller => info!("Controller stopped"),
            () = drain_deadline => warn!(
                timeout = %duration::format(shutdown_timeout),
                "Timed out waiting for in-flight reconciles"
            ),
        }
        leader.record(0, &[]);
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
}
