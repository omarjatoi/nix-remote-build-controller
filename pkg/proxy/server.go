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

	"github.com/rs/zerolog/log"
	"golang.org/x/crypto/ssh"
)

type SSHProxy struct {
	listener    net.Listener
	hostKey     ssh.Signer
	sessions    map[string]*ProxySession
	sessionsMux sync.RWMutex
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
		listener: listener,
		hostKey:  hostKey,
		sessions: make(map[string]*ProxySession),
	}

	log.Info().Str("address", addr).Msg("SSH proxy listening")
	return proxy, nil
}

func (p *SSHProxy) Start(ctx context.Context) error {
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
			conn, err := p.listener.Accept()
			if err != nil {
				log.Error().Err(err).Msg("Failed to accept connection")
				continue
			}

			go p.handleConnection(conn)
		}
	}
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
			req.Reply(false, nil)
		case "shell":
			req.Reply(false, nil)
		case "env":
			req.Reply(false, nil)
		case "pty-req":
			req.Reply(false, nil)
		default:
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
	b := make([]byte, 8)
	rand.Read(b)
	return fmt.Sprintf("%x", b)
}
