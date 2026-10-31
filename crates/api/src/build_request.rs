//! The `nix.io/v1alpha1` `NixBuildRequest` custom resource.
//!
//! The schema served by the API server lives in `deploy/crd.yaml`; these types
//! mirror it field for field (including JSON names) so that objects written by
//! the proxy and controller round-trip unchanged.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::CustomResource;
use serde::{Deserialize, Serialize};

/// API group of the custom resource.
pub const GROUP: &str = "nix.io";
/// API version of the custom resource.
pub const VERSION: &str = "v1alpha1";
/// Kind of the custom resource.
pub const KIND: &str = "NixBuildRequest";

/// Label carrying the session ID, set on `NixBuildRequest`s and builder pods.
pub const LABEL_SESSION_ID: &str = "nix.io/session-id";

/// Annotation with the W3C `traceparent` of the session behind a request, so
/// that the work done for it joins the session's trace.
pub const ANNOTATION_TRACEPARENT: &str = "nix.io/traceparent";
/// Annotation with the W3C `tracestate` that goes with
/// [`ANNOTATION_TRACEPARENT`].
pub const ANNOTATION_TRACESTATE: &str = "nix.io/tracestate";

/// A request for a Nix build that needs a dedicated builder pod.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[kube(
    group = "nix.io",
    version = "v1alpha1",
    kind = "NixBuildRequest",
    namespaced,
    status = "NixBuildRequestStatus",
    shortname = "nbr",
    schema = "disabled"
)]
#[serde(rename_all = "camelCase")]
pub struct NixBuildRequestSpec {
    /// Links this build request to the SSH proxy session.
    #[serde(default)]
    pub session_id: String,

    /// Pod resource requirements. Empty means the controller's defaults.
    #[serde(default)]
    pub resources: ResourceRequirements,

    /// Builder container image. Empty means the controller's default.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,

    /// Build timeout in seconds; becomes the pod's activeDeadlineSeconds.
    /// Absent means the default (3600); zero or less means no deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<i64>,

    /// Node selector for pod placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<BTreeMap<String, String>>,
}

/// The observed state of a build request.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NixBuildRequestStatus {
    /// Current phase. Absent is treated like `Pending`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<BuildPhase>,

    /// Name of the builder pod.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pod_name: String,

    /// IP address of the builder pod, used by the proxy for SSH routing.
    #[serde(rename = "podIP", default, skip_serializing_if = "String::is_empty")]
    pub pod_ip: String,

    /// When the builder pod was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<Time>,

    /// When the build finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_time: Option<Time>,

    /// Human-readable status information.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,

    /// Latest observations of the request's state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<BuildCondition>,
}

/// The phase of a build request.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BuildPhase {
    /// Created, builder pod not yet created.
    Pending,
    /// Builder pod created, waiting for it to get an IP.
    Creating,
    /// Builder pod has an IP and accepts SSH connections.
    Running,
    /// The build finished.
    Completed,
    /// The build or its pod failed.
    Failed,
    /// A phase this version does not know. Never written back.
    #[serde(other)]
    Unknown,
}

impl BuildPhase {
    /// The phase as written in the API.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BuildPhase::Pending => "Pending",
            BuildPhase::Creating => "Creating",
            BuildPhase::Running => "Running",
            BuildPhase::Completed => "Completed",
            BuildPhase::Failed => "Failed",
            BuildPhase::Unknown => "Unknown",
        }
    }
}

impl std::fmt::Display for BuildPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Renders an optional phase the way logs and error messages show it: the
/// phase name, or an empty string when none has been set yet.
pub fn phase_str(phase: Option<BuildPhase>) -> &'static str {
    phase.map_or("", BuildPhase::as_str)
}

/// A condition of a build request. Nothing in this crate sets conditions; the
/// field exists so that conditions written by others survive status updates.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BuildCondition {
    /// Type of condition: `PodReady`, `Completed` or `Failed`.
    #[serde(rename = "type")]
    pub type_: String,
    /// `True`, `False` or `Unknown`.
    pub status: String,
    /// Last time the condition transitioned.
    pub last_transition_time: Time,
    /// Machine-readable reason.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// Human-readable message.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// The current time as a Kubernetes timestamp.
#[must_use]
pub fn now() -> Time {
    Time(k8s_openapi::jiff::Timestamp::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use kube::Resource;
    use serde_json::json;

    #[test]
    fn serializes_with_api_field_names() {
        let mut req = NixBuildRequest::new(
            "build-x",
            NixBuildRequestSpec {
                session_id: "x".into(),
                timeout_seconds: Some(3600),
                node_selector: Some(BTreeMap::from([("arch".into(), "arm64".into())])),
                resources: ResourceRequirements {
                    requests: Some(BTreeMap::from([("cpu".into(), Quantity("1".into()))])),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        req.status = Some(NixBuildRequestStatus {
            phase: Some(BuildPhase::Running),
            pod_name: "nix-builder-x".into(),
            pod_ip: "10.0.0.1".into(),
            ..Default::default()
        });
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["apiVersion"], "nix.io/v1alpha1");
        assert_eq!(value["kind"], "NixBuildRequest");
        assert_eq!(
            value["spec"],
            json!({
                "sessionId": "x",
                "resources": {"requests": {"cpu": "1"}},
                "timeoutSeconds": 3600,
                "nodeSelector": {"arch": "arm64"},
            })
        );
        assert_eq!(
            value["status"],
            json!({"phase": "Running", "podName": "nix-builder-x", "podIP": "10.0.0.1"})
        );

        let back: NixBuildRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back.spec, req.spec);
        assert_eq!(back.status, req.status);
        assert_eq!(NixBuildRequest::group(&()), GROUP);
        assert_eq!(NixBuildRequest::version(&()), VERSION);
        assert_eq!(NixBuildRequest::kind(&()), KIND);
    }

    #[test]
    fn minimal_objects_deserialize() {
        let req: NixBuildRequest = serde_json::from_value(json!({
            "apiVersion": "nix.io/v1alpha1",
            "kind": "NixBuildRequest",
            "metadata": {"name": "build-y"},
            "spec": {"sessionId": "y", "resources": {}},
        }))
        .unwrap();
        assert_eq!(req.spec.session_id, "y");
        assert!(req.status.is_none());

        let status: NixBuildRequestStatus =
            serde_json::from_value(json!({"phase": "SomethingNew"})).unwrap();
        assert_eq!(status.phase, Some(BuildPhase::Unknown));
    }

    #[test]
    fn clones_are_independent() {
        let mut orig = NixBuildRequest::new(
            "build-x",
            NixBuildRequestSpec {
                session_id: "x".into(),
                node_selector: Some(BTreeMap::from([("arch".into(), "arm64".into())])),
                ..Default::default()
            },
        );
        orig.status = Some(NixBuildRequestStatus {
            conditions: vec![BuildCondition {
                type_: "PodReady".into(),
                status: "True".into(),
                last_transition_time: now(),
                reason: String::new(),
                message: String::new(),
            }],
            ..Default::default()
        });
        let mut copy = orig.clone();
        copy.spec
            .node_selector
            .as_mut()
            .unwrap()
            .insert("arch".into(), "amd64".into());
        copy.status.as_mut().unwrap().conditions[0].status = "False".into();
        assert_eq!(orig.spec.node_selector.as_ref().unwrap()["arch"], "arm64");
        assert_eq!(orig.status.as_ref().unwrap().conditions[0].status, "True");
    }
}
