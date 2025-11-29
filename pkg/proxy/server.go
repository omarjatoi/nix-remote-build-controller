package proxy

import (
	"context"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"encoding/pem"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"sync"
	"sync/atomic"
	"time"

	"github.com/google/uuid"
	"github.com/omarjatoi/nix-remote-build-controller/pkg/apis/nixbuilder/v1alpha1"
	"github.com/rs/zerolog/log"
	"golang.org/x/crypto/ssh"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/config"
)

type SSHProxy struct {
	listener     net.Listener
	hostKey      ssh.Signer
	clientKey    ssh.Signer
	sessions     map[string]*ProxySession
	sessionsMux  sync.RWMutex
	activeConns  sync.WaitGroup
	shutdownChan chan struct{}
	shutdownOnce sync.Once
	k8sClient    client.Client
	namespace    string
	remoteUser   string
	remotePort   int32
	healthServer *http.Server
	shuttingDown atomic.Bool
}

type ProxySession struct {
	ID         string
	SSHConn    ssh.Conn
	BuilderPod string
	Status     SessionStatus
}

type SessionStatus int

const (
	SessionPending SessionStatus = iota
	SessionConnected
	SessionClosed
)

func NewSSHProxy(ctx context.Context, addr, hostKeyPath, namespace, remoteUser string, remotePort int32, healthPort int) (*SSHProxy, error) {
	var hostKey ssh.Signer
	var err error

	if hostKeyPath != "" {
		hostKey, err = loadHostKey(hostKeyPath)
		if err != nil {
			return nil, fmt.Errorf("failed to load host key from %s: %w", hostKeyPath, err)
		}
		log.Info().Str("path", hostKeyPath).Msg("Loaded SSH host key")
	} else {
		log.Info().Msg("Generating temporary SSH host key")
		hostKey, err = generateHostKey()
		if err != nil {
			return nil, fmt.Errorf("failed to generate host key: %w", err)
		}
	}

	clientKey, err := generateHostKey()
	if err != nil {
		return nil, fmt.Errorf("failed to generate client key: %w", err)
	}

	listener, err := net.Listen("tcp", addr)
	if err != nil {
		return nil, fmt.Errorf("failed to listen on %s: %w", addr, err)
	}

	scheme := runtime.NewScheme()
	if err := clientgoscheme.AddToScheme(scheme); err != nil {
		return nil, fmt.Errorf("failed to add client-go scheme: %w", err)
	}
	if err := v1alpha1.AddToScheme(scheme); err != nil {
		return nil, fmt.Errorf("failed to add NixBuilder scheme: %w", err)
	}

	k8sConfig, err := config.GetConfig()
	if err != nil {
		return nil, fmt.Errorf("failed to get Kubernetes config: %w", err)
	}

	k8sClient, err := client.New(k8sConfig, client.Options{
		Scheme: scheme,
	})
	if err != nil {
		return nil, fmt.Errorf("failed to create Kubernetes client: %w", err)
	}

	proxy := &SSHProxy{
		listener:     listener,
		hostKey:      hostKey,
		clientKey:    clientKey,
		sessions:     make(map[string]*ProxySession),
		shutdownChan: make(chan struct{}),
		k8sClient:    k8sClient,
		namespace:    namespace,
		remoteUser:   remoteUser,
		remotePort:   remotePort,
	}

	if err := proxy.startHealthServer(healthPort); err != nil {
		return nil, fmt.Errorf("failed to start health server: %w", err)
	}

	if err := proxy.ensureSSHKeySecret(ctx); err != nil {
		return nil, fmt.Errorf("failed to ensure SSH key secret: %w", err)
	}

	log.Info().Str("address", addr).Msg("SSH proxy listening")
	return proxy, nil
}

func (p *SSHProxy) Start(ctx context.Context) error {
	defer p.listener.Close()

	connChan := make(chan net.Conn)
	errChan := make(chan error)

	go func() {
		for {
			select {
			case <-p.shutdownChan:
				return
			default:
				conn, err := p.listener.Accept()
				if err != nil {
					select {
					case errChan <- err:
					case <-p.shutdownChan:
					}
					return
				}
				select {
				case connChan <- conn:
				case <-p.shutdownChan:
					conn.Close()
					return
				}
			}
		}
	}()

	for {
		select {
		case <-ctx.Done():
			return p.gracefulShutdown(ctx)
		case err := <-errChan:
			if ctx.Err() != nil {
				return ctx.Err()
			}
			log.Error().Err(err).Msg("Failed to accept connection")
			return err
		case conn := <-connChan:
			p.activeConns.Add(1)
			go func() {
				defer p.activeConns.Done()
				p.handleConnection(ctx, conn)
			}()
		}
	}
}

func (p *SSHProxy) gracefulShutdown(ctx context.Context) error {
	// Mark as unhealthy FIRST
	p.shuttingDown.Store(true)
	log.Info().Msg("Marked proxy as unhealthy, no new connections will be accepted")

	p.shutdownOnce.Do(func() {
		close(p.shutdownChan)
	})

	log.Info().Int("active_connections", p.getActiveSessionCount()).Msg("Gracefully terminating, waiting for active connections to complete")

	done := make(chan struct{})
	go func() {
		p.activeConns.Wait()
		close(done)
	}()

	select {
	case <-done:
		log.Info().Msg("All connections completed, terminating the proxy")
	case <-ctx.Done():
		log.Warn().Msg("Shutdown timeout reached, the proxy will be forcefully terminated")
	}

	// Shutdown health server last
	if p.healthServer != nil {
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if err := p.healthServer.Shutdown(shutdownCtx); err != nil {
			log.Warn().Err(err).Msg("Health server shutdown failed")
		} else {
			log.Info().Msg("Health server shutdown completed")
		}
	}

	return ctx.Err()
}

func (p *SSHProxy) getActiveSessionCount() int {
	p.sessionsMux.RLock()
	defer p.sessionsMux.RUnlock()
	return len(p.sessions)
}

func (p *SSHProxy) handleConnection(ctx context.Context, netConn net.Conn) {
	defer netConn.Close()

	config := &ssh.ServerConfig{
		NoClientAuth: true, // TODO: adding ssh auth eventually might be a good idea
	}
	config.AddHostKey(p.hostKey)

	sshConn, chans, reqs, err := ssh.NewServerConn(netConn, config)
	if err != nil {
		log.Error().Err(err).Msg("Failed to create SSH connection")
		return
	}
	defer sshConn.Close()

	sessionID := generateSessionID()
	session := &ProxySession{
		ID:      sessionID,
		SSHConn: sshConn,
		Status:  SessionPending,
	}

	p.sessionsMux.Lock()
	p.sessions[sessionID] = session
	p.sessionsMux.Unlock()
	defer func() {
		p.sessionsMux.Lock()
		delete(p.sessions, sessionID)
		p.sessionsMux.Unlock()
	}()

	log.Info().Str("session_id", sessionID).Str("client_addr", sshConn.RemoteAddr().String()).Msg("New SSH connection")

	go ssh.DiscardRequests(reqs)
	for newChannel := range chans {
		go p.handleChannel(ctx, session, newChannel)
	}
}

func (p *SSHProxy) handleChannel(ctx context.Context, session *ProxySession, newChannel ssh.NewChannel) {
	if newChannel.ChannelType() != "session" {
		newChannel.Reject(ssh.UnknownChannelType, "unknown channel type")
		return
	}

	channel, requests, err := newChannel.Accept()
	if err != nil {
		log.Error().Err(err).Msg("Failed to accept channel")
		return
	}
	defer channel.Close()

	log.Info().Str("session_id", session.ID).Msg("Handling SSH session channel")

	if err := p.createBuildRequest(ctx, session); err != nil {
		log.Error().Err(err).Str("session_id", session.ID).Msg("Failed to create build request")
		return
	}
	defer func() {
		// Delete the build request when the session ends
		deleteCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if err := p.k8sClient.Delete(deleteCtx, &v1alpha1.NixBuildRequest{
			ObjectMeta: metav1.ObjectMeta{
				Name:      fmt.Sprintf("build-%s", session.ID),
				Namespace: p.namespace,
			},
		}); err != nil {
			log.Error().Err(err).Str("session_id", session.ID).Msg("Failed to cleanup build request")
		}
	}()

	podIP, err := p.waitForBuilderPod(ctx, session)
	if err != nil {
		log.Error().Err(err).Str("session_id", session.ID).Msg("Failed to get builder pod")
		return
	}

	if err := p.routeToBuilder(ctx, session, channel, requests, podIP); err != nil {
		log.Error().Err(err).Str("session_id", session.ID).Msg("Failed to route to builder")
		return
	}
}

func (p *SSHProxy) createBuildRequest(ctx context.Context, session *ProxySession) error {
	buildReq := &v1alpha1.NixBuildRequest{
		ObjectMeta: metav1.ObjectMeta{
			Name:      fmt.Sprintf("build-%s", session.ID),
			Namespace: p.namespace,
		},
		Spec: v1alpha1.NixBuildRequestSpec{
			SessionID: session.ID,
		},
	}

	if err := p.k8sClient.Create(ctx, buildReq); err != nil {
		return fmt.Errorf("failed to create NixBuildRequest: %w", err)
	}

	log.Info().Str("session_id", session.ID).Msg("Created NixBuildRequest")
	return nil
}

func (p *SSHProxy) ensureSSHKeySecret(ctx context.Context) error {
	secretName := "nix-builder-keys"
	publicKey := string(ssh.MarshalAuthorizedKey(p.clientKey.PublicKey()))

	secret := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{
			Name:      secretName,
			Namespace: p.namespace,
			Labels: map[string]string{
				"app": "nix-builder",
			},
		},
		StringData: map[string]string{
			"authorized_keys": publicKey,
		},
	}

	if err := p.k8sClient.Create(ctx, secret); err != nil {
		if client.IgnoreAlreadyExists(err) == nil {
			var existingSecret corev1.Secret
			if err := p.k8sClient.Get(ctx, client.ObjectKey{
				Namespace: p.namespace,
				Name:      secretName,
			}, &existingSecret); err != nil {
				return fmt.Errorf("failed to get existing secret: %w", err)
			}

			existingSecret.StringData = secret.StringData
			if err := p.k8sClient.Update(ctx, &existingSecret); err != nil {
				return fmt.Errorf("failed to update SSH key secret: %w", err)
			}
			log.Info().Str("secret", secretName).Msg("Updated existing SSH key secret")
		} else {
			return fmt.Errorf("failed to create SSH key secret: %w", err)
		}
	} else {
		log.Info().Str("secret", secretName).Msg("Created SSH key secret")
	}

	return nil
}

func (p *SSHProxy) waitForBuilderPod(ctx context.Context, session *ProxySession) (string, error) {
	buildReqName := fmt.Sprintf("build-%s", session.ID)

	timeout := time.After(time.Minute * 2)
	ticker := time.NewTicker(time.Second)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return "", ctx.Err()
		case <-timeout:
			return "", fmt.Errorf("timeout waiting for builder pod")
		case <-ticker.C:
			var buildReq v1alpha1.NixBuildRequest
			if err := p.k8sClient.Get(ctx, client.ObjectKey{
				Namespace: p.namespace,
				Name:      buildReqName,
			}, &buildReq); err != nil {
				continue
			}

			if buildReq.Status.Phase == v1alpha1.BuildPhaseRunning && buildReq.Status.PodIP != "" {
				log.Info().Str("session_id", session.ID).Str("pod_ip", buildReq.Status.PodIP).Msg("Builder pod ready")
				return buildReq.Status.PodIP, nil
			}
		}
	}
}

func (p *SSHProxy) routeToBuilder(ctx context.Context, session *ProxySession, channel ssh.Channel, requests <-chan *ssh.Request, podIP string) error {
	builderAddr := fmt.Sprintf("%s:%d", podIP, p.remotePort)

	builderConn, err := ssh.Dial("tcp", builderAddr, &ssh.ClientConfig{
		User:            p.remoteUser,
		Auth:            []ssh.AuthMethod{ssh.PublicKeys(p.clientKey)},
		HostKeyCallback: ssh.InsecureIgnoreHostKey(), // TODO: Proper host key validation
		Timeout:         time.Second * 10,
	})
	if err != nil {
		return fmt.Errorf("failed to connect to builder pod: %w", err)
	}
	defer builderConn.Close()

	builderChannel, builderRequests, err := builderConn.OpenChannel("session", nil)
	if err != nil {
		return fmt.Errorf("failed to open channel on builder: %w", err)
	}
	defer builderChannel.Close()

	log.Info().Str("session_id", session.ID).Str("builder_addr", builderAddr).Msg("Connected to builder pod")

	var wg sync.WaitGroup

	// Forward requests: client -> builder
	wg.Add(1)
	go func() {
		defer wg.Done()
		p.forwardRequests(ctx, requests, builderChannel, session.ID, "client->builder")
	}()

	// Forward requests: builder -> client
	wg.Add(1)
	go func() {
		defer wg.Done()
		p.forwardRequests(ctx, builderRequests, channel, session.ID, "builder->client")
	}()

	// Forward data: client -> builder
	wg.Add(1)
	go func() {
		defer wg.Done()
		_, err := io.Copy(builderChannel, channel)
		if err != nil && err != io.EOF {
			log.Debug().Err(err).Str("session_id", session.ID).Msg("Client -> builder channel ended")
		}
	}()

	// Forward data: builder -> client
	wg.Add(1)
	go func() {
		defer wg.Done()
		_, err := io.Copy(channel, builderChannel)
		if err != nil && err != io.EOF {
			log.Debug().Err(err).Str("session_id", session.ID).Msg("Builder -> client channel ended")
		}
	}()

	wg.Wait()

	log.Info().Str("session_id", session.ID).Str("builder_addr", builderAddr).Msg("Completed build request")

	return nil
}

func (p *SSHProxy) forwardRequests(ctx context.Context, src <-chan *ssh.Request, dst ssh.Channel, sessionID, direction string) {
	for {
		select {
		case <-ctx.Done():
			return
		case req, ok := <-src:
			if !ok {
				return
			}

			log.Debug().
				Str("session_id", sessionID).
				Str("request_type", req.Type).
				Str("direction", direction).
				Bool("want_reply", req.WantReply).
				Msg("Forwarding SSH request transparently")

			accepted, err := dst.SendRequest(req.Type, req.WantReply, req.Payload)
			if err != nil {
				log.Error().
					Err(err).
					Str("session_id", sessionID).
					Str("request_type", req.Type).
					Str("direction", direction).
					Msg("SSH request forwarding failed")
			}
			req.Reply(accepted && err == nil, nil)
		}
	}
}

func generateHostKey() (ssh.Signer, error) {
	privateKey, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		return nil, err
	}

	privateKeyPEM := &pem.Block{
		Type:  "RSA PRIVATE KEY",
		Bytes: x509.MarshalPKCS1PrivateKey(privateKey),
	}

	privateKeyBytes := pem.EncodeToMemory(privateKeyPEM)
	return ssh.ParsePrivateKey(privateKeyBytes)
}

func loadHostKey(path string) (ssh.Signer, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer file.Close()

	keyBytes, err := io.ReadAll(file)
	if err != nil {
		return nil, err
	}

	return ssh.ParsePrivateKey(keyBytes)
}

func generateSessionID() string {
	return uuid.Must(uuid.NewV7()).String()
}

func (p *SSHProxy) startHealthServer(port int) error {
	mux := http.NewServeMux()

	// Liveness probe - "is the process running?"
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
		w.Write([]byte("ok"))
	})

	// Readiness probe - "can you handle new requests?"
	mux.HandleFunc("/readyz", func(w http.ResponseWriter, r *http.Request) {
		if p.shuttingDown.Load() {
			w.WriteHeader(http.StatusServiceUnavailable)
			w.Write([]byte("shutting down"))
			return
		}

		w.WriteHeader(http.StatusOK)
		w.Write([]byte("ready"))
	})

	p.healthServer = &http.Server{
		Addr:    fmt.Sprintf(":%d", port),
		Handler: mux,
	}

	go func() {
		log.Info().Int("port", port).Msg("Health server starting")
		if err := p.healthServer.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Error().Err(err).Msg("Health server failed")
		}
	}()

	return nil
}
