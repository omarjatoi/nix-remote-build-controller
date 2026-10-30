//! The `NixBuildRequest` custom resource and the Kubernetes calls the
//! controller and proxy make on it.

mod build_request;
mod build_request_client;
mod controller_client;
#[cfg(any(test, feature = "testing"))]
pub mod fake;
pub mod kube_error;
pub mod secret;

pub use build_request::*;
pub use build_request_client::{BuildRequestClient, KubeBuildRequestClient};
pub use controller_client::{BuildEvent, ControllerClient, KubeControllerClient};
