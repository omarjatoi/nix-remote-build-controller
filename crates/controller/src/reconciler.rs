//! The reconcile logic: one step towards the desired state per call.

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::{
    ConfigMapVolumeSource, Container, ContainerPort, KeyToPath, Pod, PodSpec, Probe,
    ResourceRequirements, SecretVolumeSource, TCPSocketAction, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference, Time};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::runtime::events::EventType;
use kube::{Resource, ResourceExt};
use tracing::{Instrument, debug, error_span, info, warn};

use api::{BuildEvent, BuildPhase, ControllerClient, NixBuildRequest, NixBuildRequestStatus};
use api::{kube_error, secret};
use common::duration;

/// Finalizer guarding builder pod cleanup.
pub const FINALIZER_NAME: &str = "nix.io/cleanup";

// Labels set on builder pods, alongside `api::LABEL_SESSION_ID`.
/// Label key that marks builder pods.
pub const LABEL_APP: &str = "app";
/// Value of [`LABEL_APP`] on builder pods.
pub const LABEL_APP_VALUE: &str = "nix-builder";
/// Label holding the name of the pod's `NixBuildRequest`.
pub const LABEL_BUILD_REQUEST: &str = "nix.io/build-request";

/// Where an optional shared builder host key is mounted; the builder entrypoint
/// uses it instead of generating one.
pub const BUILDER_HOST_KEY_MOUNT_PATH: &str = "/etc/ssh/ssh_host_ed25519_key";

/// Pod lifetime when the request does not set `timeoutSeconds`.
pub const DEFAULT_BUILD_TIMEOUT_SECONDS: i64 = 3600;
/// How long finished requests are kept before the controller deletes them
/// itself (normally the proxy deletes them first).
pub const DEFAULT_COMPLETED_TTL: Duration = Duration::from_secs(10 * 60);
/// Default number of reconcile workers.
pub const DEFAULT_MAX_CONCURRENT_RECONCILES: u16 = 10;
/// Gives sshd a moment to close sessions when a builder pod is deleted.
pub const DEFAULT_POD_TERMINATION_GRACE_PERIOD_SECONDS: i64 = 10;

pub(crate) const REQUEUE_CREATING: Duration = Duration::from_secs(2);
pub(crate) const REQUEUE_RUNNING: Duration = Duration::from_secs(30);
pub(crate) const REQUEUE_CONFLICT: Duration = Duration::from_millis(100);

/// What the reconciler wants to happen next for an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Reconcile again after this long, or sooner if the object changes.
    Requeue(Duration),
    /// Reconcile again only when the object (or a pod it owns) changes.
    AwaitChange,
}

impl ReconcileAction {
    /// The requeue delay, if any.
    pub fn requeue_after(self) -> Option<Duration> {
        match self {
            Self::Requeue(after) => Some(after),
            Self::AwaitChange => None,
        }
    }
}

/// A failed API operation; the controller retries the object with backoff.
#[must_use]
#[derive(Debug, thiserror::Error)]
#[error("{op}: {source}")]
pub struct ReconcileError {
    op: &'static str,
    source: Box<kube::Error>,
}

impl ReconcileError {
    fn new(op: &'static str, source: kube::Error) -> Self {
        Self {
            op,
            source: Box::new(source),
        }
    }
}

/// Static configuration of the reconciler.
#[derive(Debug, Clone, Default)]
pub struct ReconcilerConfig {
    /// Default builder image; `spec.image` overrides it.
    pub builder_image: String,
    /// Image pull policy for builder pods. `None` uses the Kubernetes default.
    pub builder_image_pull_policy: Option<String>,
    /// SSH port builder pods listen on.
    pub remote_port: i32,
    /// ConfigMap mounted at /etc/nix in builder pods, if any.
    pub nix_config_map: Option<String>,
    /// Secret providing authorized_keys (and optionally the host key).
    pub ssh_key_secret: String,
    /// Mount the secret's `builder-host-key` entry into builder pods.
    pub mount_builder_host_key: bool,
    /// Applied when `spec.resources` is empty.
    pub default_resources: ResourceRequirements,
    /// How long Completed/Failed requests linger. Zero means the default.
    pub completed_ttl: Duration,
    /// Service account for builder pods. `None` uses the namespace default.
    pub builder_service_account: Option<String>,
}

/// Drives `NixBuildRequest`s through Pending → Creating → Running and cleans
/// up their pods on deletion.
pub struct Reconciler<C> {
    client: C,
    cfg: ReconcilerConfig,
}

type ReconcileResult = Result<ReconcileAction, ReconcileError>;

impl<C: ControllerClient> Reconciler<C> {
    /// Creates a reconciler that talks to the API through `client`.
    pub fn new(client: C, cfg: ReconcilerConfig) -> Self {
        Self { client, cfg }
    }

    /// Reconciles one request. `req` is the latest cached copy of the object.
    pub async fn reconcile(&self, req: &NixBuildRequest) -> ReconcileResult {
        let phase = req.status.as_ref().and_then(|s| s.phase);
        let span = error_span!(
            "reconcile",
            build_request = %req.name_any(),
            namespace = %namespace(req),
            session_id = %req.spec.session_id,
            phase = api::phase_str(phase),
        );
        self.reconcile_inner(req, phase).instrument(span).await
    }

    async fn reconcile_inner(
        &self,
        req: &NixBuildRequest,
        phase: Option<BuildPhase>,
    ) -> ReconcileResult {
        if req.metadata.deletion_timestamp.is_some() {
            return self.handle_deletion(req).await;
        }

        if !has_finalizer(req) {
            let mut updated = req.clone();
            updated.finalizers_mut().push(FINALIZER_NAME.to_string());
            // On success the update event re-triggers reconciliation.
            return match self.client.replace_request(&updated).await {
                Ok(_) => Ok(ReconcileAction::AwaitChange),
                Err(err) => retry_on_conflict("add finalizer", err),
            };
        }

        match phase {
            None | Some(BuildPhase::Pending) => self.handle_pending(req).await,
            Some(BuildPhase::Creating) => self.handle_creating(req).await,
            Some(BuildPhase::Running) => self.handle_running(req).await,
            Some(BuildPhase::Completed | BuildPhase::Failed) => self.handle_completed(req).await,
            Some(BuildPhase::Unknown) => {
                warn!("Unknown build phase, ignoring");
                Ok(ReconcileAction::AwaitChange)
            }
        }
    }

    async fn handle_deletion(&self, req: &NixBuildRequest) -> ReconcileResult {
        if !has_finalizer(req) {
            return Ok(ReconcileAction::AwaitChange);
        }
        self.delete_builder_pod(req)
            .await
            .map_err(|err| ReconcileError::new("delete builder pod", err))?;
        let mut updated = req.clone();
        updated.finalizers_mut().retain(|f| f != FINALIZER_NAME);
        if let Err(err) = self.client.replace_request(&updated).await {
            return retry_on_conflict("remove finalizer", err);
        }
        info!("NixBuildRequest cleaned up");
        Ok(ReconcileAction::AwaitChange)
    }

    async fn handle_pending(&self, req: &NixBuildRequest) -> ReconcileResult {
        let pod = self.builder_pod(req);
        let pod_name = pod.name_any();
        match self.client.create_pod(&pod).await {
            Ok(_) => {
                info!(pod = %pod_name, "Created builder pod");
                let note = format!("Created builder pod {pod_name}");
                self.event(req, EventType::Normal, "PodCreated", "CreatePod", note);
            }
            Err(err) if kube_error::is_already_exists(&err) => {
                info!(pod = %pod_name, "Builder pod already exists, adopting it");
            }
            Err(err) if kube_error::is_invalid(&err) => {
                // The pod is built from the request, so retrying cannot help.
                let message = format!("Builder pod is invalid: {err}");
                return self.fail(req, "PodInvalid", &message).await;
            }
            Err(err) => {
                self.event(
                    req,
                    EventType::Warning,
                    "PodCreateFailed",
                    "CreatePod",
                    err.to_string(),
                );
                // Surface the reason to the proxy without failing yet; API
                // errors are usually transient (quota, admission webhooks).
                // An unchanged message is not written again: the write would
                // trigger another reconcile and bypass the retry backoff.
                let message = format!("Failed to create builder pod: {err}");
                if status_message(req) != message {
                    let mut updated = req.clone();
                    status_mut(&mut updated).message = message;
                    self.update_status(&updated, ReconcileAction::AwaitChange)
                        .await?;
                }
                return Err(ReconcileError::new("create builder pod", err));
            }
        }

        let mut updated = req.clone();
        let status = status_mut(&mut updated);
        status.phase = Some(BuildPhase::Creating);
        status.pod_name = pod_name;
        status.start_time = Some(api::now());
        status.message = "Builder pod created, waiting for it to become ready".to_string();
        self.update_status(&updated, ReconcileAction::Requeue(REQUEUE_CREATING))
            .await
    }

    async fn handle_creating(&self, req: &NixBuildRequest) -> ReconcileResult {
        let Some(pod) = self.get_builder_pod(req).await? else {
            return self
                .fail(
                    req,
                    "PodMissing",
                    "Builder pod was deleted before it became ready",
                )
                .await;
        };

        match pod_phase(&pod) {
            "Failed" => {
                let reason = pod_failure_reason(&pod);
                let message = format!("Builder pod failed before it became ready: {reason}");
                return self.fail(req, "PodFailed", &message).await;
            }
            "Succeeded" => {
                return self
                    .fail(
                        req,
                        "PodExited",
                        "Builder pod exited before it became ready",
                    )
                    .await;
            }
            _ => {}
        }

        // Publish the pod IP as soon as the network sandbox has one, without
        // waiting for the readiness probe. The proxy retries its SSH dial until
        // the builder's sshd answers, so that dial *is* the readiness check;
        // gating here would only add the probe period plus a kubelet status
        // round-trip to every session. The probe stays on the pod as an
        // observability signal, but nothing blocks on it.
        let pod_ip = pod
            .status
            .as_ref()
            .and_then(|s| s.pod_ip.as_deref())
            .unwrap_or_default();
        if !pod_ip.is_empty() {
            let pod_name = pod.name_any();
            info!(pod = %pod_name, pod_ip, "Builder pod ready");
            let note = format!("Builder pod {pod_name} has IP {pod_ip}");
            self.event(req, EventType::Normal, "PodReady", "AwaitReady", note);
            let mut updated = req.clone();
            let status = status_mut(&mut updated);
            status.phase = Some(BuildPhase::Running);
            status.pod_ip = pod_ip.to_string();
            status.message =
                "Builder pod has an IP; proxy will dial until sshd answers".to_string();
            return self
                .update_status(&updated, ReconcileAction::Requeue(REQUEUE_RUNNING))
                .await;
        }

        // Still starting: surface scheduling / image pull problems to the proxy logs.
        if let Some(message) = pod_pending_reason(&pod)
            && message != status_message(req)
        {
            let mut updated = req.clone();
            status_mut(&mut updated).message = message;
            return self
                .update_status(&updated, ReconcileAction::Requeue(REQUEUE_CREATING))
                .await;
        }
        Ok(ReconcileAction::Requeue(REQUEUE_CREATING))
    }

    async fn handle_running(&self, req: &NixBuildRequest) -> ReconcileResult {
        let Some(pod) = self.get_builder_pod(req).await? else {
            return self
                .fail(req, "PodMissing", "Builder pod was deleted while running")
                .await;
        };

        match pod_phase(&pod) {
            "Failed" => {
                let reason = pod_failure_reason(&pod);
                let message = format!("Builder pod failed while running: {reason}");
                self.fail(req, "PodFailed", &message).await
            }
            "Succeeded" => {
                let mut updated = req.clone();
                let status = status_mut(&mut updated);
                status.phase = Some(BuildPhase::Completed);
                status.completion_time = Some(api::now());
                status.message = "Builder pod exited".to_string();
                self.update_status(&updated, ReconcileAction::Requeue(self.completed_ttl()))
                    .await
            }
            _ => Ok(ReconcileAction::Requeue(REQUEUE_RUNNING)),
        }
    }

    /// Reaps finished requests that the proxy did not delete (for example
    /// because the proxy crashed) once their TTL has elapsed.
    async fn handle_completed(&self, req: &NixBuildRequest) -> ReconcileResult {
        let ttl = self.completed_ttl();
        // Without a completion time (only possible if something else wrote
        // the terminal phase), count from creation so the request is still
        // reaped eventually.
        let finished = req
            .status
            .as_ref()
            .and_then(|s| s.completion_time.as_ref())
            .or(req.metadata.creation_timestamp.as_ref());
        let remaining = finished.map_or(ttl, |finished| remaining_ttl(finished, ttl));
        if !remaining.is_zero() {
            return Ok(ReconcileAction::Requeue(remaining));
        }
        info!(ttl = %duration::format(ttl), "Deleting finished NixBuildRequest after TTL");
        match self
            .client
            .delete_request(&namespace(req), &req.name_any())
            .await
        {
            Err(err) if !kube_error::is_not_found(&err) => {
                Err(ReconcileError::new("delete finished NixBuildRequest", err))
            }
            _ => Ok(ReconcileAction::AwaitChange),
        }
    }

    async fn fail(
        &self,
        req: &NixBuildRequest,
        reason: &'static str,
        message: &str,
    ) -> ReconcileResult {
        warn!(reason, "{message}");
        self.event(
            req,
            EventType::Warning,
            reason,
            "Reconcile",
            message.to_string(),
        );
        let mut updated = req.clone();
        let status = status_mut(&mut updated);
        status.phase = Some(BuildPhase::Failed);
        status.completion_time = Some(api::now());
        status.message = message.to_string();
        self.update_status(&updated, ReconcileAction::Requeue(self.completed_ttl()))
            .await
    }

    /// Writes the status subresource. Conflicts (for example with the proxy's
    /// final status patch) are not errors: the update event requeues us.
    async fn update_status(
        &self,
        req: &NixBuildRequest,
        on_success: ReconcileAction,
    ) -> ReconcileResult {
        match self.client.replace_request_status(req).await {
            Ok(_) => Ok(on_success),
            Err(err) => retry_on_conflict("update status", err),
        }
    }

    async fn get_builder_pod(&self, req: &NixBuildRequest) -> Result<Option<Pod>, ReconcileError> {
        let Some(pod_name) = req
            .status
            .as_ref()
            .map(|s| s.pod_name.as_str())
            .filter(|n| !n.is_empty())
        else {
            return Ok(None);
        };
        self.client
            .get_pod(&namespace(req), pod_name)
            .await
            .map_err(|err| ReconcileError::new("get builder pod", err))
    }

    async fn delete_builder_pod(&self, req: &NixBuildRequest) -> kube::Result<()> {
        // The pod name is deterministic, so a request deleted before its
        // status was written can still have a pod.
        let pod_name = req
            .status
            .as_ref()
            .map(|s| s.pod_name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| builder_pod_name(req));
        match self.client.delete_pod(&namespace(req), &pod_name).await {
            Ok(()) => {
                info!(pod = %pod_name, "Deleted builder pod");
                Ok(())
            }
            Err(err) if kube_error::is_not_found(&err) => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn event(
        &self,
        req: &NixBuildRequest,
        type_: EventType,
        reason: &'static str,
        action: &'static str,
        note: String,
    ) {
        let event = BuildEvent {
            type_,
            reason,
            action,
            note,
        };
        self.client.publish_event(req, event);
    }

    fn completed_ttl(&self) -> Duration {
        if self.cfg.completed_ttl.is_zero() {
            DEFAULT_COMPLETED_TTL
        } else {
            self.cfg.completed_ttl
        }
    }

    /// Renders the builder pod for a request.
    pub fn builder_pod(&self, req: &NixBuildRequest) -> Pod {
        // activeDeadlineSeconds must be a positive integer; Kubernetes rejects
        // 0. An absent spec value falls back to the default; a value of zero or
        // less means "no deadline".
        let active_deadline = match req.spec.timeout_seconds {
            None => Some(DEFAULT_BUILD_TIMEOUT_SECONDS),
            Some(t) if t > 0 => Some(t),
            Some(_) => None,
        };
        let spec_resources = &req.spec.resources;
        let resources = if is_empty_map(spec_resources.requests.as_ref())
            && is_empty_map(spec_resources.limits.as_ref())
        {
            self.cfg.default_resources.clone()
        } else {
            spec_resources.clone()
        };
        let image = if req.spec.image.is_empty() {
            self.cfg.builder_image.clone()
        } else {
            req.spec.image.clone()
        };

        let mut volume_mounts = vec![VolumeMount {
            name: "ssh-keys".into(),
            mount_path: "/home/nixbld/.ssh/authorized_keys".into(),
            sub_path: Some(secret::PUBLIC_KEY.into()),
            read_only: Some(true),
            ..Default::default()
        }];
        let mut volumes = vec![Volume {
            name: "ssh-keys".into(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(self.cfg.ssh_key_secret.clone()),
                default_mode: Some(0o644),
                ..Default::default()
            }),
            ..Default::default()
        }];

        if self.cfg.mount_builder_host_key {
            volumes.push(Volume {
                name: "ssh-host-key".into(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(self.cfg.ssh_key_secret.clone()),
                    default_mode: Some(0o600),
                    items: Some(vec![KeyToPath {
                        key: secret::BUILDER_HOST_KEY.into(),
                        path: "ssh_host_ed25519_key".into(),
                        mode: None,
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            });
            volume_mounts.push(VolumeMount {
                name: "ssh-host-key".into(),
                mount_path: BUILDER_HOST_KEY_MOUNT_PATH.into(),
                sub_path: Some("ssh_host_ed25519_key".into()),
                read_only: Some(true),
                ..Default::default()
            });
        }

        if let Some(config_map) = &self.cfg.nix_config_map {
            volumes.push(Volume {
                name: "nix-config".into(),
                config_map: Some(ConfigMapVolumeSource {
                    name: config_map.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            });
            volume_mounts.push(VolumeMount {
                name: "nix-config".into(),
                mount_path: "/etc/nix".into(),
                read_only: Some(true),
                ..Default::default()
            });
        }

        Pod {
            metadata: ObjectMeta {
                name: Some(builder_pod_name(req)),
                namespace: req.namespace(),
                labels: Some(BTreeMap::from([
                    (LABEL_APP.to_string(), LABEL_APP_VALUE.to_string()),
                    (
                        api::LABEL_SESSION_ID.to_string(),
                        req.spec.session_id.clone(),
                    ),
                    (LABEL_BUILD_REQUEST.to_string(), req.name_any()),
                ])),
                // Foreground deletion of the request waits for its pod.
                owner_references: req.controller_owner_ref(&()).map(|owner| {
                    vec![OwnerReference {
                        block_owner_deletion: Some(true),
                        ..owner
                    }]
                }),
                ..Default::default()
            },
            spec: Some(PodSpec {
                restart_policy: Some("Never".into()),
                active_deadline_seconds: active_deadline,
                termination_grace_period_seconds: Some(
                    DEFAULT_POD_TERMINATION_GRACE_PERIOD_SECONDS,
                ),
                node_selector: req.spec.node_selector.clone(),
                service_account_name: self.cfg.builder_service_account.clone(),
                containers: vec![Container {
                    name: "nix-builder".into(),
                    image: Some(image),
                    image_pull_policy: self.cfg.builder_image_pull_policy.clone(),
                    ports: Some(vec![ContainerPort {
                        name: Some("ssh".into()),
                        container_port: self.cfg.remote_port,
                        protocol: Some("TCP".into()),
                        ..Default::default()
                    }]),
                    resources: Some(resources),
                    // Nothing blocks on this probe: the proxy dials the pod as
                    // soon as it has an IP. It is kept so `kubectl get pods`
                    // reports something meaningful, hence no initial delay.
                    readiness_probe: Some(Probe {
                        tcp_socket: Some(TCPSocketAction {
                            port: IntOrString::Int(self.cfg.remote_port),
                            host: None,
                        }),
                        initial_delay_seconds: Some(0),
                        period_seconds: Some(1),
                        failure_threshold: Some(60),
                        ..Default::default()
                    }),
                    volume_mounts: Some(volume_mounts),
                    ..Default::default()
                }],
                volumes: Some(volumes),
                ..Default::default()
            }),
            status: None,
        }
    }
}

/// The deterministic builder pod name for a request.
pub fn builder_pod_name(req: &NixBuildRequest) -> String {
    format!("nix-builder-{}", req.spec.session_id)
}

fn namespace(req: &NixBuildRequest) -> String {
    req.namespace().unwrap_or_default()
}

fn has_finalizer(req: &NixBuildRequest) -> bool {
    req.finalizers().iter().any(|f| f == FINALIZER_NAME)
}

fn status_mut(req: &mut NixBuildRequest) -> &mut NixBuildRequestStatus {
    req.status
        .get_or_insert_with(NixBuildRequestStatus::default)
}

fn status_message(req: &NixBuildRequest) -> &str {
    req.status
        .as_ref()
        .map(|s| s.message.as_str())
        .unwrap_or_default()
}

fn is_empty_map<V>(map: Option<&BTreeMap<String, V>>) -> bool {
    map.is_none_or(BTreeMap::is_empty)
}

/// Maps an API error onto the next action. Conflicts mean the object changed
/// under us (for example the proxy's final status patch): requeue promptly
/// with a fresh read. That is expected, not an error. A vanished object needs
/// nothing more.
fn retry_on_conflict(op: &'static str, err: kube::Error) -> ReconcileResult {
    if kube_error::is_conflict(&err) {
        debug!(op, "Conflict, requeuing");
        return Ok(ReconcileAction::Requeue(REQUEUE_CONFLICT));
    }
    if kube_error::is_not_found(&err) {
        return Ok(ReconcileAction::AwaitChange);
    }
    Err(ReconcileError::new(op, err))
}

/// How much of `ttl` is left since `finished`, saturating at zero.
fn remaining_ttl(finished: &Time, ttl: Duration) -> Duration {
    let elapsed = k8s_openapi::jiff::Timestamp::now().duration_since(finished.0);
    // A completion time in the future (clock skew) counts as "just finished".
    let elapsed = Duration::try_from(elapsed).unwrap_or(Duration::ZERO);
    ttl.saturating_sub(elapsed)
}

fn pod_phase(pod: &Pod) -> &str {
    pod.status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or_default()
}

/// Summarises why a pod has not become ready yet.
fn pod_pending_reason(pod: &Pod) -> Option<String> {
    let status = pod.status.as_ref()?;
    for cond in status.conditions.iter().flatten() {
        if cond.type_ == "PodScheduled" && cond.status == "False" {
            return Some(format!(
                "Builder pod not scheduled: {}: {}",
                cond.reason.as_deref().unwrap_or_default(),
                cond.message.as_deref().unwrap_or_default()
            ));
        }
    }
    for cs in status.container_statuses.iter().flatten() {
        if let Some(waiting) = cs.state.as_ref().and_then(|s| s.waiting.as_ref()) {
            let reason = waiting.reason.as_deref().unwrap_or_default();
            if !reason.is_empty() && reason != "ContainerCreating" {
                return Some(format!(
                    "Builder pod waiting: {reason}: {}",
                    waiting.message.as_deref().unwrap_or_default()
                ));
            }
        }
    }
    None
}

/// Summarises why a pod failed.
pub(crate) fn pod_failure_reason(pod: &Pod) -> String {
    let Some(status) = pod.status.as_ref() else {
        return "unknown".to_string();
    };
    let reason = status.reason.as_deref().unwrap_or_default();
    let message = status.message.as_deref().unwrap_or_default();
    if !reason.is_empty() {
        if !message.is_empty() {
            return format!("{reason}: {message}");
        }
        return reason.to_string();
    }
    for cs in status.container_statuses.iter().flatten() {
        if let Some(term) = cs.state.as_ref().and_then(|s| s.terminated.as_ref()) {
            return format!(
                "container {} exited with code {} ({})",
                cs.name,
                term.exit_code,
                term.reason.as_deref().unwrap_or_default()
            );
        }
    }
    if !message.is_empty() {
        return message.to_string();
    }
    "unknown".to_string()
}
