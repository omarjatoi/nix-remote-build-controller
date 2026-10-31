//! The API operations the reconciler needs, behind a trait so the reconcile
//! logic can be exercised against an in-memory API in tests.

use async_trait::async_trait;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{DeleteParams, PostParams};
use kube::runtime::events::{Event, EventType, Recorder, Reporter};
use kube::{Api, Client, Resource, ResourceExt};
use tracing::debug;

use crate::NixBuildRequest;
use crate::kube_error;

/// An event recorded on a `NixBuildRequest` (events.k8s.io/v1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildEvent {
    /// Normal or Warning.
    pub type_: EventType,
    /// Machine-readable cause, e.g. `PodCreated`.
    pub reason: &'static str,
    /// The operation being performed, e.g. `CreatePod`.
    pub action: &'static str,
    /// Human-readable description.
    pub note: String,
}

/// Kubernetes operations used by the reconciler.
#[async_trait]
pub trait ControllerClient: Send + Sync + 'static {
    /// Replaces a request (metadata and spec), with optimistic concurrency on
    /// its resourceVersion.
    async fn replace_request(&self, req: &NixBuildRequest) -> kube::Result<NixBuildRequest>;
    /// Replaces a request's status subresource, with optimistic concurrency.
    async fn replace_request_status(&self, req: &NixBuildRequest) -> kube::Result<NixBuildRequest>;
    /// Deletes a request.
    async fn delete_request(&self, namespace: &str, name: &str) -> kube::Result<()>;
    /// Creates a pod.
    async fn create_pod(&self, pod: &Pod) -> kube::Result<Pod>;
    /// Reads a pod; `None` when it does not exist.
    async fn get_pod(&self, namespace: &str, name: &str) -> kube::Result<Option<Pod>>;
    /// Deletes a pod.
    async fn delete_pod(&self, namespace: &str, name: &str) -> kube::Result<()>;
    /// Records an event on a request. Must not block reconciliation; failures
    /// are logged, not returned.
    fn publish_event(&self, req: &NixBuildRequest, event: BuildEvent);
}

/// [`ControllerClient`] backed by the Kubernetes API.
#[derive(Clone)]
pub struct KubeControllerClient {
    client: Client,
    recorder: Recorder,
}

impl KubeControllerClient {
    /// Creates a client; events are reported as `reporter`.
    pub fn new(client: Client, reporter: Reporter) -> Self {
        let recorder = Recorder::new(client.clone(), reporter);
        Self { client, recorder }
    }

    fn requests(&self, namespace: &str) -> Api<NixBuildRequest> {
        Api::namespaced(self.client.clone(), namespace)
    }

    fn pods(&self, namespace: &str) -> Api<Pod> {
        Api::namespaced(self.client.clone(), namespace)
    }
}

fn namespace_of<K: Resource>(obj: &K) -> String {
    obj.meta().namespace.clone().unwrap_or_default()
}

#[async_trait]
impl ControllerClient for KubeControllerClient {
    async fn replace_request(&self, req: &NixBuildRequest) -> kube::Result<NixBuildRequest> {
        self.requests(&namespace_of(req))
            .replace(&req.name_any(), &PostParams::default(), req)
            .await
    }

    async fn replace_request_status(&self, req: &NixBuildRequest) -> kube::Result<NixBuildRequest> {
        self.requests(&namespace_of(req))
            .replace_status(&req.name_any(), &PostParams::default(), req)
            .await
    }

    async fn delete_request(&self, namespace: &str, name: &str) -> kube::Result<()> {
        self.requests(namespace)
            .delete(name, &DeleteParams::default())
            .await
            .map(|_| ())
    }

    async fn create_pod(&self, pod: &Pod) -> kube::Result<Pod> {
        self.pods(&namespace_of(pod))
            .create(&PostParams::default(), pod)
            .await
    }

    async fn get_pod(&self, namespace: &str, name: &str) -> kube::Result<Option<Pod>> {
        match self.pods(namespace).get(name).await {
            Ok(pod) => Ok(Some(pod)),
            Err(err) if kube_error::is_not_found(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }

    async fn delete_pod(&self, namespace: &str, name: &str) -> kube::Result<()> {
        self.pods(namespace)
            .delete(name, &DeleteParams::default())
            .await
            .map(|_| ())
    }

    fn publish_event(&self, req: &NixBuildRequest, event: BuildEvent) {
        let recorder = self.recorder.clone();
        let reference = req.object_ref(&());
        let event = Event {
            type_: event.type_,
            reason: event.reason.to_string(),
            note: Some(event.note),
            action: event.action.to_string(),
            secondary: None,
        };
        // Publishing is fire-and-forget, so an events API hiccup never slows
        // down or fails a reconcile.
        tokio::spawn(async move {
            if let Err(err) = recorder.publish(&event, &reference).await {
                debug!(error = %err, reason = %event.reason, "Failed to publish event");
            }
        });
    }
}
