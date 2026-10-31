//! The `NixBuildRequest` side of a build session: create it, wait for the
//! controller to report a builder pod, and record the outcome and delete it.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use api::kube_error;
use api::{BuildPhase, NixBuildRequest, NixBuildRequestSpec};
use common::{TraceContext, duration};

use crate::SshProxy;
use crate::metrics::SessionOutcome;

/// Standard managed-by label.
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
/// Value of the managed-by label.
pub const MANAGED_BY_VALUE: &str = "nix-remote-build-proxy";
/// Records which proxy pod owns a session.
pub const ANNOTATION_PROXY_POD: &str = "nix.io/proxy-pod";

/// Bound on the API calls that record a session's outcome. They run detached
/// from the session's cancellation, because deleting the request is what makes
/// the controller remove the builder pod.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(30);
/// Pause between attempts to delete a request.
const DELETE_RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// One build session: one SSH session channel, one `NixBuildRequest`, one
/// builder pod.
#[derive(Debug)]
pub struct Session {
    /// Unique ID, also used in the `NixBuildRequest` name and labels.
    pub id: String,
    /// When the session started.
    pub start: Instant,
    /// True once the `NixBuildRequest` exists and must be cleaned up.
    pub created: bool,
    /// The session's trace, recorded on the `NixBuildRequest`.
    pub trace_context: Option<TraceContext>,
}

impl Session {
    /// A new session with a fresh, time-ordered ID.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            start: Instant::now(),
            created: false,
            trace_context: None,
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// How a session ended.
#[derive(Debug, Default)]
pub struct SessionResult {
    /// Exit status reported by the builder-side command, if any.
    pub exit_status: Option<u32>,
    /// Set if the session failed before or during tunnelling.
    pub err: Option<anyhow::Error>,
}

impl SessionResult {
    /// A session that failed with `err`.
    #[must_use]
    pub fn failed(err: anyhow::Error) -> Self {
        Self {
            exit_status: None,
            err: Some(err),
        }
    }

    /// The session's outcome for the metrics.
    #[must_use]
    pub(crate) fn outcome(&self) -> SessionOutcome {
        match (&self.err, self.exit_status) {
            (Some(_), _) => SessionOutcome::Error,
            (None, None | Some(0)) => SessionOutcome::Success,
            (None, Some(_)) => SessionOutcome::CommandFailed,
        }
    }
}

/// The `NixBuildRequest` name for a session.
#[must_use]
pub fn build_request_name(session_id: &str) -> String {
    format!("build-{session_id}")
}

impl SshProxy {
    pub(crate) async fn create_build_request(&self, session: &Session) -> anyhow::Result<()> {
        let name = build_request_name(&session.id);
        let mut req = NixBuildRequest::new(
            &name,
            NixBuildRequestSpec {
                session_id: session.id.clone(),
                ..Default::default()
            },
        );
        req.metadata.namespace = Some(self.cfg.namespace.clone());
        req.metadata.labels = Some(BTreeMap::from([
            (api::LABEL_SESSION_ID.to_string(), session.id.clone()),
            (LABEL_MANAGED_BY.to_string(), MANAGED_BY_VALUE.to_string()),
        ]));
        // Only set a whole-second, positive deadline; a sub-second build
        // timeout would truncate to 0, which the controller (and Kubernetes)
        // treat as no deadline.
        let seconds = i64::try_from(self.cfg.build_timeout.as_secs()).unwrap_or(i64::MAX);
        if seconds > 0 {
            req.spec.timeout_seconds = Some(seconds);
        }
        let mut annotations: BTreeMap<String, String> = BTreeMap::new();
        if let Some(context) = &session.trace_context {
            annotations.insert(
                api::ANNOTATION_TRACEPARENT.to_string(),
                context.traceparent.clone(),
            );
            if !context.tracestate.is_empty() {
                annotations.insert(
                    api::ANNOTATION_TRACESTATE.to_string(),
                    context.tracestate.clone(),
                );
            }
        }
        if !self.cfg.owner_pod_name.is_empty() && !self.cfg.owner_pod_uid.is_empty() {
            annotations.insert(
                ANNOTATION_PROXY_POD.to_string(),
                self.cfg.owner_pod_name.clone(),
            );
            // If this proxy pod disappears, garbage collection removes its
            // requests and the controller tears down the builder pods.
            req.metadata.owner_references = Some(vec![OwnerReference {
                api_version: "v1".into(),
                kind: "Pod".into(),
                name: self.cfg.owner_pod_name.clone(),
                uid: self.cfg.owner_pod_uid.clone(),
                ..Default::default()
            }]);
        }
        if !annotations.is_empty() {
            req.metadata.annotations = Some(annotations);
        }

        self.kube
            .create(&req)
            .await
            .with_context(|| format!("create NixBuildRequest {name}"))
    }

    /// Polls the request until the controller reports a pod IP, the request
    /// fails, `cancel` fires or the pod-ready timeout expires.
    pub(crate) async fn wait_for_builder_pod(
        &self,
        session: &Session,
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        let name = build_request_name(&session.id);
        let mut last_phase: Option<BuildPhase> = None;
        let poll = async {
            loop {
                match self.kube.get(&self.cfg.namespace, &name).await {
                    Ok(req) => {
                        let status = req.status.unwrap_or_default();
                        if status.phase != last_phase {
                            info!(
                                phase = api::phase_str(status.phase),
                                message = %status.message,
                                "NixBuildRequest phase changed"
                            );
                            last_phase = status.phase;
                        }
                        if req.metadata.deletion_timestamp.is_some() {
                            bail!(
                                "NixBuildRequest was deleted before the builder pod became ready"
                            );
                        }
                        match status.phase {
                            Some(BuildPhase::Failed) if !status.message.is_empty() => {
                                bail!("NixBuildRequest failed: {}", status.message)
                            }
                            Some(BuildPhase::Failed) => bail!("NixBuildRequest failed"),
                            Some(BuildPhase::Completed) => {
                                bail!(
                                    "NixBuildRequest completed before the builder pod became ready"
                                )
                            }
                            Some(BuildPhase::Running) if !status.pod_ip.is_empty() => {
                                info!(pod = %status.pod_name, pod_ip = %status.pod_ip, "Builder pod ready");
                                return Ok(status.pod_ip);
                            }
                            _ => {}
                        }
                    }
                    Err(err) if kube_error::is_not_found(&err) => {
                        bail!("NixBuildRequest disappeared while waiting for builder pod")
                    }
                    // Transient API errors: keep polling until the deadline.
                    Err(err) => warn!(error = %err, "Failed to read NixBuildRequest, retrying"),
                }
                tokio::time::sleep(self.cfg.poll_interval).await;
            }
        };

        let timeout = self.cfg.pod_ready_timeout;
        let outcome = tokio::select! {
            result = tokio::time::timeout(timeout, poll) => Some(result),
            () = cancel.cancelled() => None,
        };
        match outcome {
            Some(Ok(result)) => result,
            Some(Err(_elapsed)) => Err(anyhow!(
                "timed out after {} waiting for builder pod (last phase {:?})",
                duration::format(timeout),
                api::phase_str(last_phase)
            )),
            None => Err(anyhow!("session cancelled while waiting for builder pod")),
        }
    }

    /// Records the outcome on the request and deletes it. Deleting the request
    /// is what makes the controller remove the builder pod, so this runs even
    /// when the session was cancelled, with its own time bound.
    pub(crate) async fn finalize_build_request(&self, session: &Session, result: &SessionResult) {
        if !session.created {
            return;
        }
        let name = build_request_name(&session.id);
        let namespace = &self.cfg.namespace;

        // Record a terminal status only when there is something worth
        // recording: a session failure, or a non-zero remote exit status. On a
        // clean success the request is deleted immediately, so an extra status
        // write is wasted work.
        let (phase, message) = match (&result.err, result.exit_status) {
            (Some(err), _) => (BuildPhase::Failed, format!("Build session failed: {err:#}")),
            (None, Some(status)) if status != 0 => (
                BuildPhase::Completed,
                format!("Build session completed, remote command exited with status {status}"),
            ),
            _ => (BuildPhase::Completed, String::new()),
        };

        let finalize = async {
            if !message.is_empty() {
                let patch = json!({
                    "status": {
                        "phase": phase,
                        "message": message,
                        "completionTime": api::now(),
                    }
                });
                if let Err(err) = self.kube.patch_status(namespace, &name, &patch).await
                    && !kube_error::is_not_found(&err)
                {
                    warn!(error = %err, "Failed to record final NixBuildRequest status");
                }
            }
            // Retry until the time limit: a request that is not deleted keeps
            // its builder pod until the build timeout ends it.
            loop {
                match self.kube.delete(namespace, &name).await {
                    Err(err) if !kube_error::is_not_found(&err) => {
                        warn!(error = %err, "Failed to delete NixBuildRequest, retrying");
                        tokio::time::sleep(DELETE_RETRY_INTERVAL).await;
                    }
                    _ => break,
                }
            }
            info!(phase = %phase, "NixBuildRequest finalized and deleted");
        };
        if tokio::time::timeout(FINALIZE_TIMEOUT, finalize)
            .await
            .is_err()
        {
            error!(
                "Could not delete NixBuildRequest; the controller removes it and its pod when \
                 the build timeout ends"
            );
        }
    }
}
