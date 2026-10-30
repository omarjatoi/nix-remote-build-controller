use std::time::Duration;

/// Runtime configuration of the SSH proxy.
///
/// Every concurrent SSH session (one per Nix build-remote invocation) is
/// handled independently: it gets its own `NixBuildRequest`, its own builder
/// pod and its own tunnel tasks. Nothing in this configuration serialises
/// sessions; the only cap is `max_sessions`, which is opt-in.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// TCP address the SSH server listens on, e.g. `":2222"`.
    pub listen_addr: String,
    /// Address of the HTTP health server. Empty disables it.
    pub health_addr: String,
    /// Namespace where `NixBuildRequest`s are created.
    pub namespace: String,
    /// SSH user on builder pods.
    pub remote_user: String,
    /// SSH port on builder pods.
    pub remote_port: u16,

    /// Bounds how long a session waits for the controller to report a ready
    /// builder pod before the client is told the build failed.
    pub pod_ready_timeout: Duration,
    /// Written to `spec.timeoutSeconds` and becomes the builder pod's
    /// activeDeadlineSeconds. Zero leaves the CRD default.
    pub build_timeout: Duration,
    /// How long in-flight sessions may keep running after a shutdown signal
    /// before they are forcibly closed.
    pub shutdown_timeout: Duration,
    /// Bounds the SSH handshake (including authentication) with a client.
    pub handshake_timeout: Duration,
    /// Bounds the TCP+SSH handshake with a builder pod, including retries
    /// while its sshd is still starting.
    pub builder_dial_timeout: Duration,
    /// Keepalive interval: TCP keepalives towards clients, SSH keepalive
    /// requests towards builders. Zero disables keepalives.
    pub keepalive_interval: Duration,
    /// How often the `NixBuildRequest` status is polled while waiting for the
    /// builder pod.
    pub poll_interval: Duration,
    /// How long the builder side may keep running after the client closed its
    /// channel before the tunnel is torn down.
    pub client_close_grace: Duration,
    /// How long a builder may make no progress (read no input, write no
    /// output) while client input waits for it, before the proxy reads more
    /// input anyway. See `INPUT_SOFT_LIMIT` in the tunnel.
    pub input_stall_timeout: Duration,
    /// Caps concurrent build sessions. Zero means unlimited.
    pub max_sessions: usize,

    /// Name and UID of the proxy's own pod. When both are set, every
    /// `NixBuildRequest` gets an ownerReference to it so Kubernetes garbage
    /// collection removes orphaned requests if the proxy pod dies.
    pub owner_pod_name: String,
    pub owner_pod_uid: String,
}

/// Defaults used when the corresponding [`Config`] field is zero or empty.
pub const DEFAULT_POD_READY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_BUILDER_DIAL_TIMEOUT: Duration = Duration::from_secs(15);
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub const DEFAULT_CLIENT_CLOSE_GRACE: Duration = Duration::from_secs(30);
pub const DEFAULT_INPUT_STALL_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_REMOTE_USER: &str = "nixbld";
pub const DEFAULT_REMOTE_PORT: u16 = 22;
pub const DEFAULT_NAMESPACE: &str = "default";

impl Config {
    /// Fills in defaults for zero-valued fields. `keepalive_interval`,
    /// `build_timeout` and `max_sessions` keep zero, which means "disabled".
    pub fn with_defaults(mut self) -> Self {
        fn or(d: &mut Duration, default: Duration) {
            if d.is_zero() {
                *d = default;
            }
        }
        or(&mut self.pod_ready_timeout, DEFAULT_POD_READY_TIMEOUT);
        or(&mut self.shutdown_timeout, DEFAULT_SHUTDOWN_TIMEOUT);
        or(&mut self.handshake_timeout, DEFAULT_HANDSHAKE_TIMEOUT);
        or(&mut self.builder_dial_timeout, DEFAULT_BUILDER_DIAL_TIMEOUT);
        or(&mut self.poll_interval, DEFAULT_POLL_INTERVAL);
        or(&mut self.client_close_grace, DEFAULT_CLIENT_CLOSE_GRACE);
        or(&mut self.input_stall_timeout, DEFAULT_INPUT_STALL_TIMEOUT);
        if self.remote_user.is_empty() {
            self.remote_user = DEFAULT_REMOTE_USER.to_string();
        }
        if self.remote_port == 0 {
            self.remote_port = DEFAULT_REMOTE_PORT;
        }
        if self.namespace.is_empty() {
            self.namespace = DEFAULT_NAMESPACE.to_string();
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_only_zero_fields() {
        let cfg = Config {
            pod_ready_timeout: Duration::from_secs(1),
            ..Default::default()
        }
        .with_defaults();
        assert_eq!(cfg.pod_ready_timeout, Duration::from_secs(1));
        assert_eq!(cfg.shutdown_timeout, DEFAULT_SHUTDOWN_TIMEOUT);
        assert_eq!(cfg.remote_user, "nixbld");
        assert_eq!(cfg.remote_port, 22);
        assert_eq!(cfg.namespace, "default");
        assert!(cfg.keepalive_interval.is_zero());
        assert!(cfg.build_timeout.is_zero());
        assert_eq!(cfg.max_sessions, 0);
    }
}
