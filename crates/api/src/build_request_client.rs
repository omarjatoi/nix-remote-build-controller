//! The `NixBuildRequest` operations the proxy performs.

use async_trait::async_trait;
use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
use kube::{Api, Client};

use crate::NixBuildRequest;

/// The API operations the proxy performs on `NixBuildRequest`s.
#[async_trait]
pub trait BuildRequestClient: Send + Sync + 'static {
    /// Creates a request.
    async fn create(&self, req: &NixBuildRequest) -> kube::Result<()>;
    /// Reads a request.
    async fn get(&self, namespace: &str, name: &str) -> kube::Result<NixBuildRequest>;
    /// Applies a JSON merge patch to a request's status subresource.
    async fn patch_status(
        &self,
        namespace: &str,
        name: &str,
        patch: &serde_json::Value,
    ) -> kube::Result<()>;
    /// Deletes a request.
    async fn delete(&self, namespace: &str, name: &str) -> kube::Result<()>;
}

/// [`BuildRequestClient`] backed by the Kubernetes API.
#[derive(Clone)]
pub struct KubeBuildRequestClient {
    client: Client,
}

impl KubeBuildRequestClient {
    /// Wraps a Kubernetes client.
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    fn api(&self, namespace: &str) -> Api<NixBuildRequest> {
        Api::namespaced(self.client.clone(), namespace)
    }
}

#[async_trait]
impl BuildRequestClient for KubeBuildRequestClient {
    async fn create(&self, req: &NixBuildRequest) -> kube::Result<()> {
        let namespace = req.metadata.namespace.clone().unwrap_or_default();
        self.api(&namespace)
            .create(&PostParams::default(), req)
            .await
            .map(|_| ())
    }

    async fn get(&self, namespace: &str, name: &str) -> kube::Result<NixBuildRequest> {
        self.api(namespace).get(name).await
    }

    async fn patch_status(
        &self,
        namespace: &str,
        name: &str,
        patch: &serde_json::Value,
    ) -> kube::Result<()> {
        self.api(namespace)
            .patch_status(name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map(|_| ())
    }

    async fn delete(&self, namespace: &str, name: &str) -> kube::Result<()> {
        self.api(namespace)
            .delete(name, &DeleteParams::default())
            .await
            .map(|_| ())
    }
}
