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
	"os"
	"sync"
	"time"

	"github.com/google/uuid"
	"github.com/omarjatoi/nix-remote-build-controller/pkg/apis/nixbuilder/v1alpha1"
	"github.com/rs/zerolog/log"
	"golang.org/x/crypto/ssh"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/config"
)

type SSHProxy struct {
	listener     net.Listener
	hostKey      ssh.Signer
	sessions     map[string]*ProxySession
	sessionsMux  sync.RWMutex
	activeConns  sync.WaitGroup
	shutdownChan chan struct{}
	shutdownOnce sync.Once
	k8sClient    client.Client
	namespace    string
	remoteUser   string
	remotePort   int32
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

func NewSSHProxy(addr, hostKeyPath, namespace, remoteUser string, remotePort int32) (*SSHProxy, error) {
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

	listener, err := net.Listen("tcp", addr)
	if err != nil {
		return nil, fmt.Errorf("failed to listen on %s: %w", addr, err)
	}

	// Create Kubernetes client
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
		sessions:     make(map[string]*ProxySession),
		shutdownChan: make(chan struct{}),
		k8sClient:    k8sClient,
		namespace:    namespace,
		remoteUser:   remoteUser,
		remotePort:   remotePort,
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
	p.shutdownOnce.Do(func() {
		close(p.shutdownChan)
	})

	log.Info().Int("active_connections", p.getActiveSessionCount()).Msg("Gracefully terminating, no new connections will be accepted")

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
		HostKeyCallback: ssh.InsecureIgnoreHostKey(), // TODO: Proper host key validation
		Timeout:         time.Second * 10,
	})
	if err != nil {
		return fmt.Errorf("failed to connect to builder pod: %w", err)
	}
	defer builderConn.Close()

	builderSession, err := builderConn.NewSession()
	if err != nil {
		return fmt.Errorf("failed to create session on builder: %w", err)
	}
	defer builderSession.Close()

	log.Info().Str("session_id", session.ID).Str("builder_addr", builderAddr).Msg("Connected to builder pod")

	for req := range requests {
		log.Debug().Str("session_id", session.ID).Str("request_type", req.Type).Msg("Forwarding SSH request to builder")
		req.Reply(false, nil) // TODO: Forward to builder
	}

	return nil
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
