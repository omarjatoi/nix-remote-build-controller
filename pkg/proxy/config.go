package proxy

import "time"

// Config holds the runtime configuration of the SSH proxy.
//
// Every concurrent SSH session (one per Nix build-remote invocation) is handled
// independently: it gets its own NixBuildRequest, its own builder pod and its own
// tunnel goroutines. Nothing in this configuration serialises sessions; the only
// cap is MaxSessions, which is opt-in.
type Config struct {
	// ListenAddr is the TCP address the SSH server listens on, e.g. ":2222".
	ListenAddr string
	// HealthAddr is the address of the HTTP health server. Empty disables it.
	HealthAddr string
	// Namespace is where NixBuildRequests are created.
	Namespace string
	// RemoteUser is the SSH user on builder pods.
	RemoteUser string
	// RemotePort is the SSH port on builder pods.
	RemotePort int32

	// PodReadyTimeout bounds how long a session waits for the controller to
	// report a ready builder pod before the client is told the build failed.
	PodReadyTimeout time.Duration
	// BuildTimeout is written to NixBuildRequest.spec.timeoutSeconds and becomes
	// the builder pod's activeDeadlineSeconds. Zero leaves the CRD default.
	BuildTimeout time.Duration
	// ShutdownTimeout is how long in-flight sessions may keep running after a
	// shutdown signal before they are forcibly closed.
	ShutdownTimeout time.Duration
	// HandshakeTimeout bounds the SSH handshake with a client.
	HandshakeTimeout time.Duration
	// BuilderDialTimeout bounds the TCP+SSH handshake with a builder pod.
	BuilderDialTimeout time.Duration
	// KeepaliveInterval is the interval of SSH keepalive requests sent to both
	// the client and the builder. Zero disables keepalives.
	KeepaliveInterval time.Duration
	// PollInterval is how often the NixBuildRequest status is polled while
	// waiting for the builder pod.
	PollInterval time.Duration
	// ClientCloseGrace is how long the builder side may keep running after the
	// client closed its channel before the tunnel is torn down.
	ClientCloseGrace time.Duration
	// MaxSessions caps concurrent build sessions. Zero means unlimited.
	MaxSessions int

	// OwnerPodName and OwnerPodUID identify the proxy's own pod. When set,
	// every NixBuildRequest gets an ownerReference to it so Kubernetes garbage
	// collection removes orphaned requests if the proxy pod dies.
	OwnerPodName string
	OwnerPodUID  string
}

// Defaults used when the corresponding Config field is zero.
const (
	DefaultPodReadyTimeout    = 5 * time.Minute
	DefaultShutdownTimeout    = 30 * time.Minute
	DefaultHandshakeTimeout   = 30 * time.Second
	DefaultBuilderDialTimeout = 15 * time.Second
	DefaultKeepaliveInterval  = 30 * time.Second
	DefaultPollInterval       = time.Second
	DefaultClientCloseGrace   = 30 * time.Second
	DefaultRemoteUser         = "nixbld"
	DefaultRemotePort         = int32(22)
)

func (c Config) withDefaults() Config {
	if c.PodReadyTimeout <= 0 {
		c.PodReadyTimeout = DefaultPodReadyTimeout
	}
	if c.ShutdownTimeout <= 0 {
		c.ShutdownTimeout = DefaultShutdownTimeout
	}
	if c.HandshakeTimeout <= 0 {
		c.HandshakeTimeout = DefaultHandshakeTimeout
	}
	if c.BuilderDialTimeout <= 0 {
		c.BuilderDialTimeout = DefaultBuilderDialTimeout
	}
	if c.PollInterval <= 0 {
		c.PollInterval = DefaultPollInterval
	}
	if c.ClientCloseGrace <= 0 {
		c.ClientCloseGrace = DefaultClientCloseGrace
	}
	if c.RemoteUser == "" {
		c.RemoteUser = DefaultRemoteUser
	}
	if c.RemotePort == 0 {
		c.RemotePort = DefaultRemotePort
	}
	if c.Namespace == "" {
		c.Namespace = "default"
	}
	return c
}
