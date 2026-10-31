//! The controller's metrics.

use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, UpDownCounter};

/// Bucket boundaries of reconcile durations, in seconds.
const RECONCILE_BOUNDARIES: [f64; 40] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.15, 0.2, 0.25, 0.3, 0.35, 0.4, 0.45, 0.5, 0.6, 0.7, 0.8, 0.9,
    1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 15.0, 20.0,
    25.0, 30.0, 40.0, 50.0, 60.0,
];

/// Bucket boundaries of builder pod startup times, in seconds.
const STARTUP_BOUNDARIES: [f64; 13] = [
    0.5, 1.0, 2.0, 3.0, 5.0, 7.5, 10.0, 15.0, 30.0, 60.0, 120.0, 180.0, 300.0,
];

/// The controller's instruments. Prometheus sees each name with dots replaced
/// by underscores, the unit added and `_total` after counters.
#[derive(Clone)]
pub struct Metrics {
    pub(crate) reconcile_total: Counter<u64>,
    pub(crate) reconcile_errors: Counter<u64>,
    pub(crate) reconcile_time: Histogram<f64>,
    pub(crate) max_concurrent_reconciles: Gauge<i64>,
    pub(crate) active_workers: UpDownCounter<i64>,
    pub(crate) builder_pods_created: Counter<u64>,
    pub(crate) builder_pod_startup: Histogram<f64>,
    pub(crate) requests_failed: Counter<u64>,
    pub(crate) leader: Gauge<i64>,
}

impl Metrics {
    /// Creates the instruments on `meter`.
    pub fn new(meter: &Meter) -> Self {
        let metrics = Self {
            reconcile_total: meter
                .u64_counter("nix_remote_build_controller.reconcile")
                .with_description("Reconciles, by result")
                .build(),
            reconcile_errors: meter
                .u64_counter("nix_remote_build_controller.reconcile.errors")
                .with_description("Reconciles that failed and will be retried")
                .build(),
            reconcile_time: meter
                .f64_histogram("nix_remote_build_controller.reconcile.duration")
                .with_description("Time taken by each reconcile")
                .with_unit("s")
                .with_boundaries(RECONCILE_BOUNDARIES.to_vec())
                .build(),
            max_concurrent_reconciles: meter
                .i64_gauge("nix_remote_build_controller.max_concurrent_reconciles")
                .with_description("Reconciles allowed to run at the same time")
                .build(),
            active_workers: meter
                .i64_up_down_counter("nix_remote_build_controller.active_reconciles")
                .with_description("Reconciles running now")
                .build(),
            builder_pods_created: meter
                .u64_counter("nix_remote_build_controller.builder_pods.created")
                .with_description("Builder pods created")
                .build(),
            builder_pod_startup: meter
                .f64_histogram("nix_remote_build_controller.builder_pod.startup.duration")
                .with_description("Time from creating a builder pod until it has an IP address")
                .with_unit("s")
                .with_boundaries(STARTUP_BOUNDARIES.to_vec())
                .build(),
            requests_failed: meter
                .u64_counter("nix_remote_build_controller.build_requests.failed")
                .with_description("NixBuildRequests the controller marked as failed, by reason")
                .build(),
            leader: meter
                .i64_gauge("nix_remote_build_controller.leader")
                .with_description("1 if this replica is reconciling, 0 if it is a standby")
                .build(),
        };
        metrics.leader.record(0, &[]);
        metrics.active_workers.add(0, &[]);
        metrics
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::KeyValue;

    use common::MetricsProvider;

    use super::*;

    #[test]
    fn metrics_render_under_their_prometheus_names() {
        let provider = MetricsProvider::new().unwrap();
        let metrics = Metrics::new(&provider.meter());
        metrics
            .reconcile_total
            .add(1, &[KeyValue::new("result", "success")]);
        metrics.reconcile_errors.add(1, &[]);
        metrics.reconcile_time.record(0.2, &[]);
        metrics.max_concurrent_reconciles.record(10, &[]);
        metrics
            .requests_failed
            .add(1, &[KeyValue::new("reason", "PodFailed")]);
        metrics.builder_pods_created.add(1, &[]);
        metrics.builder_pod_startup.record(3.0, &[]);

        let text = provider.render();
        for line in [
            "nix_remote_build_controller_reconcile_total{result=\"success\"} 1",
            "nix_remote_build_controller_reconcile_errors_total 1",
            "nix_remote_build_controller_reconcile_duration_seconds_count 1",
            "nix_remote_build_controller_max_concurrent_reconciles 10",
            "nix_remote_build_controller_active_reconciles 0",
            "nix_remote_build_controller_build_requests_failed_total{reason=\"PodFailed\"} 1",
            "nix_remote_build_controller_builder_pods_created_total 1",
            "nix_remote_build_controller_builder_pod_startup_duration_seconds_count 1",
            "nix_remote_build_controller_leader 0",
        ] {
            assert!(text.contains(line), "missing {line:?} in:\n{text}");
        }
    }
}
