// Package proxy implements the SSH front door for Nix remote builds.
//
// A Nix client configured with a builder such as
//
//	ssh://nixbld@<proxy> x86_64-linux - 100 1
//
// runs one build-remote hook per derivation it wants built remotely. Each hook
// opens its own SSH connection (or, with ControlMaster, its own channel on a
// shared connection) and executes "nix-store --serve --write". The proxy treats
// every session channel as an independent build session: it creates a
// NixBuildRequest, waits for the controller to bring up a dedicated builder pod
// and then splices the channel onto an SSH session on that pod. Sessions never
// wait on each other, so independent derivations build concurrently as long as
// the client advertises enough remote slots (the "100" above).
package proxy

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"sync"
	"sync/atomic"
	"time"

	"github.com/google/uuid"
	"github.com/rs/zerolog"
	"github.com/rs/zerolog/log"
	"golang.org/x/crypto/ssh"
	"k8s.io/apimachinery/pkg/runtime"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/config"

	"github.com/omarjatoi/nix-remote-build-controller/pkg/apis/nixbuilder/v1alpha1"
)

// SSHProxy accepts SSH connections from Nix clients and routes every session
// channel to a dynamically provisioned builder pod.
type SSHProxy struct {
	cfg              Config
	listener         net.Listener
	k8sClient        client.Client
	sshConfig        *ssh.ServerConfig
	builderClientKey ssh.Signer
	builderHostKey   ssh.HostKeyCallback
	healthServer     *http.Server

	shuttingDown atomic.Bool
	activeConns  sync.WaitGroup

	sessionsMu sync.RWMutex
	sessions   map[string]*Session
}

// New creates a proxy listening on cfg.ListenAddr.
//
// hostKey is presented to Nix clients. builderClientKey authenticates the proxy
// to builder pods. clientAuth may be nil, in which case any client may connect;
// that is only acceptable when network policy restricts who can reach the proxy.
// builderHostKey may be nil to accept any builder host key.
func New(cfg Config, k8sClient client.Client, hostKey, builderClientKey ssh.Signer, clientAuth *ClientAuthorizer, builderHostKey ssh.PublicKey) (*SSHProxy, error) {
	cfg = cfg.withDefaults()
	if k8sClient == nil {
		return nil, errors.New("kubernetes client is required")
	}
	if hostKey == nil || builderClientKey == nil {
		return nil, errors.New("host key and builder client key are required")
	}

	sshConfig := &ssh.ServerConfig{
		ServerVersion: "SSH-2.0-nix-remote-build-proxy",
	}
	sshConfig.AddHostKey(hostKey)
	if clientAuth != nil {
		sshConfig.PublicKeyCallback = clientAuth.Callback
	} else {
		sshConfig.NoClientAuth = true
	}

	listener, err := net.Listen("tcp", cfg.ListenAddr)
	if err != nil {
		return nil, fmt.Errorf("listen on %s: %w", cfg.ListenAddr, err)
	}

	p := &SSHProxy{
		cfg:              cfg,
		listener:         listener,
		k8sClient:        k8sClient,
		sshConfig:        sshConfig,
		builderClientKey: builderClientKey,
		sessions:         make(map[string]*Session),
	}
	if builderHostKey != nil {
		p.builderHostKey = ssh.FixedHostKey(builderHostKey)
	} else {
		p.builderHostKey = ssh.InsecureIgnoreHostKey() //nolint:gosec // pod-internal traffic; see README "Security"
	}

	if cfg.HealthAddr != "" {
		if err := p.startHealthServer(cfg.HealthAddr); err != nil {
			_ = listener.Close()
			return nil, err
		}
	}

	log.Info().
		Str("address", listener.Addr().String()).
		Bool("client_auth", clientAuth != nil).
		Bool("builder_host_key_verification", builderHostKey != nil).
		Int("max_sessions", cfg.MaxSessions).
		Dur("pod_ready_timeout", cfg.PodReadyTimeout).
		Dur("shutdown_timeout", cfg.ShutdownTimeout).
		Msg("SSH proxy listening")
	return p, nil
}

// NewFromCluster builds a proxy using in-cluster (or kubeconfig) credentials and
// loads all key material from the SSH key secret.
func NewFromCluster(ctx context.Context, cfg Config, hostKeyPath, sshKeySecret, clientAuthorizedKeysPath string) (*SSHProxy, error) {
	cfg = cfg.withDefaults()

	scheme := runtime.NewScheme()
	if err := clientgoscheme.AddToScheme(scheme); err != nil {
		return nil, fmt.Errorf("add client-go scheme: %w", err)
	}
	if err := v1alpha1.AddToScheme(scheme); err != nil {
		return nil, fmt.Errorf("add NixBuilder scheme: %w", err)
	}

	k8sConfig, err := config.GetConfig()
	if err != nil {
		return nil, fmt.Errorf("get Kubernetes config: %w", err)
	}
	k8sClient, err := client.New(k8sConfig, client.Options{Scheme: scheme})
	if err != nil {
		return nil, fmt.Errorf("create Kubernetes client: %w", err)
	}

	secret, err := getSecret(ctx, k8sClient, cfg.Namespace, sshKeySecret)
	if err != nil {
		return nil, err
	}

	builderClientKey, ok, err := signerFromSecret(secret, SSHKeySecretPrivateKey)
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, fmt.Errorf("secret %s is missing required key %q", sshKeySecret, SSHKeySecretPrivateKey)
	}
	log.Info().Str("secret", sshKeySecret).Msg("Loaded builder SSH client key from secret")

	var hostKey ssh.Signer
	switch {
	case hostKeyPath != "":
		hostKey, err = loadHostKey(hostKeyPath)
		if err != nil {
			return nil, fmt.Errorf("load host key from %s: %w", hostKeyPath, err)
		}
		log.Info().Str("path", hostKeyPath).Msg("Loaded SSH host key from file")
	default:
		hostKey, ok, err = signerFromSecret(secret, SSHKeySecretHostKey)
		if err != nil {
			return nil, err
		}
		if ok {
			log.Info().Str("secret", sshKeySecret).Msg("Loaded SSH host key from secret")
		} else {
			log.Warn().Msgf("No %q entry in secret %s; generating an ephemeral host key (clients will see a changed host key after every restart)", SSHKeySecretHostKey, sshKeySecret)
			hostKey, err = generateHostKey()
			if err != nil {
				return nil, fmt.Errorf("generate host key: %w", err)
			}
		}
	}

	var builderHostKey ssh.PublicKey
	if signer, ok, err := signerFromSecret(secret, SSHKeySecretBuilderHostKey); err != nil {
		return nil, err
	} else if ok {
		builderHostKey = signer.PublicKey()
		log.Info().Str("fingerprint", ssh.FingerprintSHA256(builderHostKey)).Msg("Builder host key verification enabled")
	} else {
		log.Warn().Msgf("No %q entry in secret %s; builder host keys will not be verified", SSHKeySecretBuilderHostKey, sshKeySecret)
	}

	var clientAuth *ClientAuthorizer
	switch {
	case clientAuthorizedKeysPath != "":
		clientAuth, err = LoadAuthorizedKeys(clientAuthorizedKeysPath)
		if err != nil {
			return nil, err
		}
		log.Info().Str("path", clientAuthorizedKeysPath).Int("keys", clientAuth.Len()).Msg("Client public key authentication enabled")
	case len(secret.Data[SSHKeySecretClientAuthorizedKeys]) > 0:
		clientAuth, err = ParseAuthorizedKeys(secret.Data[SSHKeySecretClientAuthorizedKeys])
		if err != nil {
			return nil, fmt.Errorf("parse %q from secret %s: %w", SSHKeySecretClientAuthorizedKeys, sshKeySecret, err)
		}
		log.Info().Str("secret", sshKeySecret).Int("keys", clientAuth.Len()).Msg("Client public key authentication enabled")
	default:
		log.Warn().Msg("Client authentication is DISABLED: anyone who can reach the proxy can start builder pods and run commands in them. Add a \"client-authorized-keys\" entry to the SSH key secret or pass --client-authorized-keys.")
	}

	return New(cfg, k8sClient, hostKey, builderClientKey, clientAuth, builderHostKey)
}

// Addr returns the address the proxy is listening on.
func (p *SSHProxy) Addr() net.Addr { return p.listener.Addr() }

// SessionCount returns the number of build sessions currently in flight.
func (p *SSHProxy) SessionCount() int {
	p.sessionsMu.RLock()
	defer p.sessionsMu.RUnlock()
	return len(p.sessions)
}

// Start serves connections until ctx is cancelled, then drains in-flight
// sessions for up to Config.ShutdownTimeout. It returns nil after a clean
// shutdown and an error if the listener failed.
func (p *SSHProxy) Start(ctx context.Context) error {
	// Sessions deliberately do not inherit ctx: a shutdown signal must stop new
	// connections but let running builds finish within the drain window.
	sessionCtx, cancelSessions := context.WithCancel(context.Background())
	defer cancelSessions()

	acceptDone := make(chan error, 1)
	go func() {
		for {
			conn, err := p.listener.Accept()
			if err != nil {
				acceptDone <- err
				return
			}
			p.activeConns.Add(1)
			go func() {
				defer p.activeConns.Done()
				p.handleConnection(sessionCtx, conn)
			}()
		}
	}()

	select {
	case <-ctx.Done():
		p.drain(cancelSessions, acceptDone)
		return nil
	case err := <-acceptDone:
		p.shuttingDown.Store(true)
		cancelSessions()
		p.stopHealthServer()
		return fmt.Errorf("accept: %w", err)
	}
}

func (p *SSHProxy) drain(cancelSessions func(), acceptDone <-chan error) {
	p.shuttingDown.Store(true)
	_ = p.listener.Close()
	<-acceptDone // the accept loop has exited, so no further activeConns.Add can race with Wait.

	log.Info().
		Int("active_sessions", p.SessionCount()).
		Dur("timeout", p.cfg.ShutdownTimeout).
		Msg("Shutdown requested: not accepting new connections, draining active sessions")

	done := make(chan struct{})
	go func() {
		p.activeConns.Wait()
		close(done)
	}()

	timer := time.NewTimer(p.cfg.ShutdownTimeout)
	defer timer.Stop()
	select {
	case <-done:
		log.Info().Msg("All sessions completed")
	case <-timer.C:
		log.Warn().Int("active_sessions", p.SessionCount()).Msg("Drain timeout reached, forcibly closing remaining sessions")
		cancelSessions()
		select {
		case <-done:
		case <-time.After(15 * time.Second):
			log.Error().Msg("Sessions did not terminate after forced cancellation")
		}
	}

	p.stopHealthServer()
}

// handleConnection serves one SSH transport connection. Every session channel
// on it becomes an independent build session.
func (p *SSHProxy) handleConnection(ctx context.Context, netConn net.Conn) {
	defer func() { _ = netConn.Close() }()

	_ = netConn.SetDeadline(time.Now().Add(p.cfg.HandshakeTimeout))
	sshConn, chans, reqs, err := ssh.NewServerConn(netConn, p.sshConfig)
	if err != nil {
		log.Warn().Err(err).Str("client_addr", netConn.RemoteAddr().String()).Msg("SSH handshake failed")
		return
	}
	_ = netConn.SetDeadline(time.Time{})
	defer func() { _ = sshConn.Close() }()

	logger := log.With().
		Str("conn_id", shortID()).
		Str("client_addr", sshConn.RemoteAddr().String()).
		Str("client_version", string(sshConn.ClientVersion())).
		Logger()
	if sshConn.Permissions != nil {
		if fp := sshConn.Permissions.Extensions[PermissionExtensionFingerprint]; fp != "" {
			logger = logger.With().Str("client_key", fp).Logger()
		}
	}
	logger.Info().Str("user", sshConn.User()).Msg("New SSH connection")

	// connCtx is cancelled when the client goes away or the proxy is force-shut
	// down; every session on this connection watches it.
	connCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	go func() {
		<-connCtx.Done()
		_ = sshConn.Close()
	}()
	go func() {
		_ = sshConn.Wait()
		cancel()
	}()
	go ssh.DiscardRequests(reqs)
	if p.cfg.KeepaliveInterval > 0 {
		go keepalive(connCtx, sshConn, p.cfg.KeepaliveInterval, func(err error) {
			logger.Warn().Err(err).Msg("Client keepalive failed, closing connection")
			cancel()
		})
	}

	var wg sync.WaitGroup
	for newChannel := range chans {
		wg.Add(1)
		go func(nc ssh.NewChannel) {
			defer wg.Done()
			p.handleChannel(connCtx, logger, nc)
		}(newChannel)
	}
	wg.Wait()
	logger.Info().Msg("SSH connection closed")
}

// handleChannel runs one build session on a freshly requested channel.
func (p *SSHProxy) handleChannel(ctx context.Context, logger zerolog.Logger, newChannel ssh.NewChannel) {
	if newChannel.ChannelType() != "session" {
		_ = newChannel.Reject(ssh.UnknownChannelType, "only session channels are supported")
		return
	}
	if p.cfg.MaxSessions > 0 && p.SessionCount() >= p.cfg.MaxSessions {
		logger.Warn().Int("max_sessions", p.cfg.MaxSessions).Msg("Rejecting session: concurrent session limit reached")
		_ = newChannel.Reject(ssh.ResourceShortage, "too many concurrent build sessions")
		return
	}

	channel, requests, err := newChannel.Accept()
	if err != nil {
		logger.Error().Err(err).Msg("Failed to accept channel")
		return
	}
	defer func() { _ = channel.Close() }()

	session := &Session{
		ID:        generateSessionID(),
		StartTime: time.Now(),
	}
	p.trackSession(session)
	defer p.untrackSession(session)

	logger = logger.With().Str("session_id", session.ID).Logger()
	logger.Info().Int("active_sessions", p.SessionCount()).Msg("Build session opened")

	result := p.runSession(ctx, logger, session, channel, requests)
	p.finalizeBuildRequest(logger, session, result)

	logger.Info().
		Bool("succeeded", result.Err == nil).
		Dur("duration", time.Since(session.StartTime)).
		Msg("Build session closed")
}

func (p *SSHProxy) runSession(ctx context.Context, logger zerolog.Logger, session *Session, channel ssh.Channel, requests <-chan *ssh.Request) SessionResult {
	if err := p.createBuildRequest(ctx, session); err != nil {
		logger.Error().Err(err).Msg("Failed to create NixBuildRequest")
		failClient(channel, requests, "failed to create NixBuildRequest: "+err.Error())
		return SessionResult{Err: err}
	}
	session.Created = true

	podIP, err := p.waitForBuilderPod(ctx, logger, session)
	if err != nil {
		logger.Error().Err(err).Msg("Builder pod did not become ready")
		failClient(channel, requests, "builder pod did not become ready: "+err.Error())
		return SessionResult{Err: err}
	}

	return p.routeToBuilder(ctx, logger, session, channel, requests, podIP)
}

func (p *SSHProxy) trackSession(s *Session) {
	p.sessionsMu.Lock()
	defer p.sessionsMu.Unlock()
	p.sessions[s.ID] = s
}

func (p *SSHProxy) untrackSession(s *Session) {
	p.sessionsMu.Lock()
	defer p.sessionsMu.Unlock()
	delete(p.sessions, s.ID)
}

func generateSessionID() string {
	return uuid.Must(uuid.NewV7()).String()
}

func shortID() string {
	return uuid.Must(uuid.NewV7()).String()[24:]
}

// keepalive sends OpenSSH-style keepalive requests. If the peer does not answer
// within interval the connection is considered dead.
func keepalive(ctx context.Context, conn ssh.Conn, interval time.Duration, onDead func(error)) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
		replied := make(chan error, 1)
		go func() {
			_, _, err := conn.SendRequest("keepalive@openssh.com", true, nil)
			replied <- err
		}()
		select {
		case <-ctx.Done():
			return
		case err := <-replied:
			if err != nil {
				onDead(fmt.Errorf("keepalive: %w", err))
				return
			}
		case <-time.After(interval):
			onDead(errors.New("keepalive: no reply"))
			return
		}
	}
}

func (p *SSHProxy) startHealthServer(addr string) error {
	mux := http.NewServeMux()
	// Liveness: the process is running.
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("ok"))
	})
	// Readiness: accepting new connections.
	mux.HandleFunc("/readyz", func(w http.ResponseWriter, _ *http.Request) {
		if p.shuttingDown.Load() {
			w.WriteHeader(http.StatusServiceUnavailable)
			_, _ = w.Write([]byte("shutting down"))
			return
		}
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("ready"))
	})

	ln, err := net.Listen("tcp", addr)
	if err != nil {
		return fmt.Errorf("listen health server on %s: %w", addr, err)
	}
	p.healthServer = &http.Server{Handler: mux, ReadHeaderTimeout: 5 * time.Second}
	go func() {
		log.Info().Str("address", ln.Addr().String()).Msg("Health server starting")
		if err := p.healthServer.Serve(ln); err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Error().Err(err).Msg("Health server failed")
		}
	}()
	return nil
}

func (p *SSHProxy) stopHealthServer() {
	if p.healthServer == nil {
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := p.healthServer.Shutdown(ctx); err != nil {
		log.Warn().Err(err).Msg("Health server shutdown failed")
	}
}
