//! An in-memory stand-in for the Kubernetes API, for tests.
//!
//! It models the parts of API server behaviour the controller and proxy rely
//! on: optimistic concurrency on resourceVersion, the status subresource
//! (spec/metadata writes ignore status and vice versa), finalizers holding
//! back deletion, AlreadyExists/NotFound errors and JSON merge patches.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{Pod, PodStatus};
use kube::ResourceExt;

use crate::api::{self, NixBuildRequest, NixBuildRequestStatus};
use crate::controller::{BuildEvent, ControllerClient};
use crate::kube_util::api_error;

type Key = (String, String);

#[derive(Default)]
struct State {
    requests: BTreeMap<Key, NixBuildRequest>,
    pods: BTreeMap<Key, Pod>,
    events: Vec<BuildEvent>,
    next_version: u64,
    /// When set, the next pod create fails with this (code, reason, message).
    fail_next_pod_create: Option<(u16, String, String)>,
}

impl State {
    fn bump(&mut self) -> String {
        self.next_version += 1;
        self.next_version.to_string()
    }
}

/// A cheaply cloneable handle to a shared in-memory API.
#[derive(Clone, Default)]
pub struct FakeKube {
    state: Arc<Mutex<State>>,
}

fn key(namespace: &str, name: &str) -> Key {
    (namespace.to_string(), name.to_string())
}

fn not_found(kind: &str, name: &str) -> kube::Error {
    api_error(404, "NotFound", format!("{kind} \"{name}\" not found"))
}

fn conflict(kind: &str, name: &str) -> kube::Error {
    api_error(
        409,
        "Conflict",
        format!("Operation cannot be fulfilled on {kind} \"{name}\": the object has been modified"),
    )
}

fn already_exists(kind: &str, name: &str) -> kube::Error {
    api_error(
        409,
        "AlreadyExists",
        format!("{kind} \"{name}\" already exists"),
    )
}

impl FakeKube {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        crate::lock(&self.state)
    }

    /// Stores a request as-is (status included), like seeding a fake client.
    pub fn insert_request(&self, mut req: NixBuildRequest) {
        let mut state = self.state();
        req.metadata.resource_version = Some(state.bump());
        if req.metadata.uid.is_none() {
            req.metadata.uid = Some(format!("uid-{}", req.name_any()));
        }
        state.requests.insert(
            key(&req.namespace().unwrap_or_default(), &req.name_any()),
            req,
        );
    }

    /// Stores a pod as-is.
    pub fn insert_pod(&self, mut pod: Pod) {
        let mut state = self.state();
        pod.metadata.resource_version = Some(state.bump());
        state.pods.insert(
            key(&pod.namespace().unwrap_or_default(), &pod.name_any()),
            pod,
        );
    }

    pub fn request(&self, namespace: &str, name: &str) -> Option<NixBuildRequest> {
        self.state().requests.get(&key(namespace, name)).cloned()
    }

    pub fn pod(&self, namespace: &str, name: &str) -> Option<Pod> {
        self.state().pods.get(&key(namespace, name)).cloned()
    }

    /// Replaces a pod's status, as the kubelet would.
    pub fn set_pod_status(&self, namespace: &str, name: &str, status: PodStatus) {
        let mut state = self.state();
        let version = state.bump();
        let pod = state
            .pods
            .get_mut(&key(namespace, name))
            .expect("pod exists");
        pod.status = Some(status);
        pod.metadata.resource_version = Some(version);
    }

    /// Writes a request's status unconditionally, as a test controller would.
    pub fn set_request_status(
        &self,
        namespace: &str,
        name: &str,
        status: NixBuildRequestStatus,
    ) -> bool {
        let mut state = self.state();
        let version = state.bump();
        match state.requests.get_mut(&key(namespace, name)) {
            Some(req) => {
                req.status = Some(status);
                req.metadata.resource_version = Some(version);
                true
            }
            None => false,
        }
    }

    /// Makes the next pod create fail with an API error.
    pub fn fail_next_pod_create(&self, code: u16, reason: &str, message: &str) {
        self.state().fail_next_pod_create = Some((code, reason.to_string(), message.to_string()));
    }

    pub fn events(&self) -> Vec<BuildEvent> {
        self.state().events.clone()
    }

    /// Deletes a request; finalizers hold it back with a deletion timestamp.
    fn delete_request_object(&self, namespace: &str, name: &str) -> kube::Result<()> {
        let mut state = self.state();
        let k = key(namespace, name);
        let version = state.bump();
        let stored = state
            .requests
            .get_mut(&k)
            .ok_or_else(|| not_found("nixbuildrequests.nix.io", name))?;
        if stored.finalizers().is_empty() {
            state.requests.remove(&k);
        } else if stored.metadata.deletion_timestamp.is_none() {
            stored.metadata.deletion_timestamp = Some(api::now());
            stored.metadata.resource_version = Some(version);
        }
        Ok(())
    }

    fn check_version(
        stored: Option<&String>,
        given: Option<&String>,
        name: &str,
    ) -> kube::Result<()> {
        match given {
            Some(v) if Some(v) != stored => Err(conflict("nixbuildrequests.nix.io", name)),
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl ControllerClient for FakeKube {
    async fn replace_request(&self, req: &NixBuildRequest) -> kube::Result<NixBuildRequest> {
        let mut state = self.state();
        let k = key(&req.namespace().unwrap_or_default(), &req.name_any());
        let name = req.name_any();
        let version = state.bump();
        let stored = state
            .requests
            .get_mut(&k)
            .ok_or_else(|| not_found("nixbuildrequests.nix.io", &name))?;
        Self::check_version(
            stored.metadata.resource_version.as_ref(),
            req.metadata.resource_version.as_ref(),
            &name,
        )?;
        // Status and server-managed metadata are not writable through the
        // main resource.
        let mut metadata = req.metadata.clone();
        metadata.deletion_timestamp = stored.metadata.deletion_timestamp.clone();
        metadata.uid = stored.metadata.uid.clone();
        metadata.creation_timestamp = stored.metadata.creation_timestamp.clone();
        metadata.resource_version = Some(version);
        stored.metadata = metadata;
        stored.spec = req.spec.clone();
        let updated = stored.clone();
        if updated.metadata.deletion_timestamp.is_some() && updated.finalizers().is_empty() {
            state.requests.remove(&k);
        }
        Ok(updated)
    }

    async fn replace_request_status(&self, req: &NixBuildRequest) -> kube::Result<NixBuildRequest> {
        let mut state = self.state();
        let k = key(&req.namespace().unwrap_or_default(), &req.name_any());
        let name = req.name_any();
        let version = state.bump();
        let stored = state
            .requests
            .get_mut(&k)
            .ok_or_else(|| not_found("nixbuildrequests.nix.io", &name))?;
        Self::check_version(
            stored.metadata.resource_version.as_ref(),
            req.metadata.resource_version.as_ref(),
            &name,
        )?;
        stored.status = req.status.clone();
        stored.metadata.resource_version = Some(version);
        Ok(stored.clone())
    }

    async fn delete_request(&self, namespace: &str, name: &str) -> kube::Result<()> {
        self.delete_request_object(namespace, name)
    }

    async fn create_pod(&self, pod: &Pod) -> kube::Result<Pod> {
        let mut state = self.state();
        if let Some((code, reason, message)) = state.fail_next_pod_create.take() {
            return Err(api_error(code, &reason, message));
        }
        let k = key(&pod.namespace().unwrap_or_default(), &pod.name_any());
        if state.pods.contains_key(&k) {
            return Err(already_exists("pods", &pod.name_any()));
        }
        let mut created = pod.clone();
        created.metadata.resource_version = Some(state.bump());
        created.metadata.uid = Some(format!("uid-pod-{}", pod.name_any()));
        created.status = Some(PodStatus {
            phase: Some("Pending".into()),
            ..Default::default()
        });
        state.pods.insert(k, created.clone());
        Ok(created)
    }

    async fn get_pod(&self, namespace: &str, name: &str) -> kube::Result<Option<Pod>> {
        Ok(self.pod(namespace, name))
    }

    async fn delete_pod(&self, namespace: &str, name: &str) -> kube::Result<()> {
        self.state()
            .pods
            .remove(&key(namespace, name))
            .map(|_| ())
            .ok_or_else(|| not_found("pods", name))
    }

    fn publish_event(&self, _req: &NixBuildRequest, event: BuildEvent) {
        self.state().events.push(event);
    }
}
