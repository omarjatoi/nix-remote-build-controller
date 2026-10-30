//! The SSH key secret shared by the proxy, the controller and builder pods.
//!
//! One secret holds every key the system needs; see `deploy/ssh-keys-secret.yaml`.

use k8s_openapi::api::core::v1::Secret;

/// Private key the proxy uses to authenticate to builder pods (required).
pub const PRIVATE_KEY: &str = "private";
/// The matching public key, mounted into builder pods as authorized_keys
/// (required).
pub const PUBLIC_KEY: &str = "public";
/// The proxy's own SSH host key (optional).
pub const HOST_KEY: &str = "host-key";
/// Host key shared by all builder pods (optional). When present the controller
/// mounts it into builder pods and the proxy verifies it.
pub const BUILDER_HOST_KEY: &str = "builder-host-key";
/// authorized_keys of the Nix clients allowed to use the proxy (optional).
pub const CLIENT_AUTHORIZED_KEYS: &str = "client-authorized-keys";

/// The bytes of a secret entry, or `None` when it is absent or empty.
pub fn entry<'a>(secret: &'a Secret, key: &str) -> Option<&'a [u8]> {
    secret
        .data
        .as_ref()?
        .get(key)
        .map(|v| v.0.as_slice())
        .filter(|v| !v.is_empty())
}
