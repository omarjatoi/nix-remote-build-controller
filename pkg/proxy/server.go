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

	"github.com/google/uuid"
	"github.com/rs/zerolog/log"
	"golang.org/x/crypto/ssh"
)

type SSHProxy struct {
	listener     net.Listener
	hostKey      ssh.Signer
	sessions     map[string]*ProxySession
	sessionsMux  sync.RWMutex
	activeConns  sync.WaitGroup
	shutdownChan chan struct{}
	shutdownOnce sync.Once
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

func NewSSHProxy(addr, hostKeyPath string) (*SSHProxy, error) {
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

	proxy := &SSHProxy{
		listener:     listener,
		hostKey:      hostKey,
		sessions:     make(map[string]*ProxySession),
		shutdownChan: make(chan struct{}),
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
				p.handleConnection(conn)
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

func (p *SSHProxy) handleConnection(netConn net.Conn) {
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
		go p.handleChannel(session, newChannel)
	}
}

func (p *SSHProxy) handleChannel(session *ProxySession, newChannel ssh.NewChannel) {
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

	for req := range requests {
		log.Info().Str("session_id", session.ID).Str("request_type", req.Type).Msg("New SSH request")
		switch req.Type {
		case "exec":
			log.Debug().Str("session_id", session.ID).Str("command", string(req.Payload)).Msg("Executing command")
			req.Reply(false, nil)
		case "shell":
			log.Debug().Str("session_id", session.ID).Msg("Starting shell")
			req.Reply(false, nil)
		case "env":
			log.Debug().Str("session_id", session.ID).Msg("Setting environment variables")
			req.Reply(false, nil)
		case "pty-req":
			log.Debug().Str("session_id", session.ID).Msg("Requesting pseudo-terminal")
			req.Reply(false, nil)
		default:
			log.Debug().Str("session_id", session.ID).Str("request_type", req.Type).Msg("Unknown SSH request")
			req.Reply(false, nil)
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
