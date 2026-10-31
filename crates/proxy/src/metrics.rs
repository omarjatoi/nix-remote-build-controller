//! The proxy's metrics.

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};

/// Bucket boundaries of session durations, in seconds.
const SESSION_BOUNDARIES: [f64; 14] = [
    1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0, 14400.0,
    28800.0,
];

/// Bucket boundaries of the time spent waiting for a builder, in seconds.
const BUILDER_BOUNDARIES: [f64; 15] = [
    0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 7.5, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0,
];

/// How a build session ended, as a metric label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionOutcome {
    /// The remote command exited with status 0, or without a status.
    Success,
    /// The remote command exited with another status: usually a failed build.
    CommandFailed,
    /// The proxy could not finish the session.
    Error,
}

impl SessionOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::CommandFailed => "command_failed",
            Self::Error => "error",
        }
    }
}

/// Why a session channel was refused, as a metric label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RejectReason {
    /// The proxy is shutting down.
    ShuttingDown,
    /// `max_sessions` was reached.
    SessionLimit,
}

impl RejectReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ShuttingDown => "shutting_down",
            Self::SessionLimit => "session_limit",
        }
    }
}

/// The direction of tunnelled bytes, as a metric label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    ClientToBuilder,
    BuilderToClient,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClientToBuilder => "client_to_builder",
            Self::BuilderToClient => "builder_to_client",
        }
    }
}

/// The proxy's instruments. Prometheus sees each name with dots replaced by
/// underscores, the unit added and `_total` after counters.
#[derive(Clone)]
pub struct Metrics {
    connections: Counter<u64>,
    auth_rejections: Counter<u64>,
    sessions: Counter<u64>,
    sessions_rejected: Counter<u64>,
    active_sessions: UpDownCounter<i64>,
    session_duration: Histogram<f64>,
    builder_wait: Histogram<f64>,
    builder_dial: Histogram<f64>,
    tunnel_bytes: Counter<u64>,
}

impl Metrics {
    /// Creates the instruments on `meter`.
    pub fn new(meter: &Meter) -> Self {
        let metrics = Self {
            connections: meter
                .u64_counter("nix_remote_build_proxy.connections")
                .with_description("SSH connections accepted")
                .build(),
            auth_rejections: meter
                .u64_counter("nix_remote_build_proxy.auth.rejections")
                .with_description("Client public keys that were not authorized")
                .build(),
            sessions: meter
                .u64_counter("nix_remote_build_proxy.sessions")
                .with_description("Build sessions that ended, by outcome")
                .build(),
            sessions_rejected: meter
                .u64_counter("nix_remote_build_proxy.sessions.rejected")
                .with_description("Session channels refused, by reason")
                .build(),
            active_sessions: meter
                .i64_up_down_counter("nix_remote_build_proxy.active_sessions")
                .with_description("Build sessions running now")
                .build(),
            session_duration: meter
                .f64_histogram("nix_remote_build_proxy.session.duration")
                .with_description("Length of build sessions")
                .with_unit("s")
                .with_boundaries(SESSION_BOUNDARIES.to_vec())
                .build(),
            builder_wait: meter
                .f64_histogram("nix_remote_build_proxy.builder.wait.duration")
                .with_description("Time from the start of a session until its pod has an address")
                .with_unit("s")
                .with_boundaries(BUILDER_BOUNDARIES.to_vec())
                .build(),
            builder_dial: meter
                .f64_histogram("nix_remote_build_proxy.builder.dial.duration")
                .with_description("Time to connect to a builder pod, including retries")
                .with_unit("s")
                .with_boundaries(BUILDER_BOUNDARIES.to_vec())
                .build(),
            tunnel_bytes: meter
                .u64_counter("nix_remote_build_proxy.tunnel.transferred")
                .with_description("Session data forwarded, by direction")
                .with_unit("By")
                .build(),
        };
        metrics.active_sessions.add(0, &[]);
        metrics
    }

    pub(crate) fn connection_accepted(&self) {
        self.connections.add(1, &[]);
    }

    pub(crate) fn auth_rejected(&self) {
        self.auth_rejections.add(1, &[]);
    }

    pub(crate) fn session_rejected(&self, reason: RejectReason) {
        self.sessions_rejected
            .add(1, &[KeyValue::new("reason", reason.as_str())]);
    }

    /// Counts a session as running; `-1` ends that.
    pub(crate) fn add_active_sessions(&self, sessions: i64) {
        self.active_sessions.add(sessions, &[]);
    }

    pub(crate) fn session_ended(&self, outcome: SessionOutcome, duration: Duration) {
        let outcome = [KeyValue::new("outcome", outcome.as_str())];
        self.sessions.add(1, &outcome);
        self.session_duration
            .record(duration.as_secs_f64(), &outcome);
    }

    pub(crate) fn builder_ready(&self, waited: Duration) {
        self.builder_wait.record(waited.as_secs_f64(), &[]);
    }

    pub(crate) fn builder_dialed(&self, took: Duration) {
        self.builder_dial.record(took.as_secs_f64(), &[]);
    }

    pub(crate) fn tunnelled(&self, direction: Direction, bytes: usize) {
        self.tunnel_bytes.add(
            bytes as u64,
            &[KeyValue::new("direction", direction.as_str())],
        );
    }
}

#[cfg(test)]
mod tests {
    use common::MetricsProvider;

    use super::*;

    #[test]
    fn metrics_render_under_their_prometheus_names() {
        let provider = MetricsProvider::new().unwrap();
        let metrics = Metrics::new(&provider.meter());
        metrics.connection_accepted();
        metrics.auth_rejected();
        metrics.session_rejected(RejectReason::SessionLimit);
        metrics.add_active_sessions(1);
        metrics.session_ended(SessionOutcome::CommandFailed, Duration::from_secs(3));
        metrics.builder_ready(Duration::from_secs(2));
        metrics.builder_dialed(Duration::from_millis(30));
        metrics.tunnelled(Direction::BuilderToClient, 1024);

        let text = provider.render();
        for line in [
            "nix_remote_build_proxy_connections_total 1",
            "nix_remote_build_proxy_auth_rejections_total 1",
            "nix_remote_build_proxy_sessions_rejected_total{reason=\"session_limit\"} 1",
            "nix_remote_build_proxy_active_sessions 1",
            "nix_remote_build_proxy_sessions_total{outcome=\"command_failed\"} 1",
            "nix_remote_build_proxy_session_duration_seconds_count{outcome=\"command_failed\"} 1",
            "nix_remote_build_proxy_builder_wait_duration_seconds_count 1",
            "nix_remote_build_proxy_builder_dial_duration_seconds_count 1",
            "nix_remote_build_proxy_tunnel_transferred_bytes_total{direction=\"builder_to_client\"} 1024",
        ] {
            assert!(text.contains(line), "missing {line:?} in:\n{text}");
        }
    }
}
