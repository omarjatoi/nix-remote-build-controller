//! Creates and deletes builder pods for `NixBuildRequest` objects.
//!
//! Each `NixBuildRequest` belongs to one SSH build session and has one builder
//! pod. Each reconcile step reads the current state, makes at most one change
//! and then returns, so many requests can be processed at the same time. All
//! state is in the request status, so a restarted controller continues where
//! the old one stopped.

mod config;
pub mod leader;
mod reconciler;
mod runner;

pub use config::{parse_pull_policy, parse_resources};
pub use reconciler::{
    BUILDER_HOST_KEY_MOUNT_PATH, DEFAULT_BUILD_TIMEOUT_SECONDS, DEFAULT_COMPLETED_TTL,
    DEFAULT_MAX_CONCURRENT_RECONCILES, DEFAULT_POD_TERMINATION_GRACE_PERIOD_SECONDS,
    FINALIZER_NAME, LABEL_APP, LABEL_APP_VALUE, LABEL_BUILD_REQUEST, ReconcileAction,
    ReconcileError, Reconciler, ReconcilerConfig, builder_pod_name,
};
pub use runner::{Metrics, Runner};

#[cfg(test)]
mod tests;
