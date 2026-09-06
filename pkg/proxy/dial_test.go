package proxy

import (
	"context"
	"errors"
	"net"
	"strings"
	"testing"
	"time"

	"github.com/rs/zerolog"
	"golang.org/x/crypto/ssh"
)

// reservePort returns an address on loopback with nothing listening on it, so a
// dial to it is refused the way it would be against a builder pod whose sshd has
// not bound its port yet.
func reservePort(t *testing.T) string {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	addr := ln.Addr().String()
	if err := ln.Close(); err != nil {
		t.Fatal(err)
	}
	return addr
}

// serveMiniSSH accepts a single SSH connection on addr and completes the
// handshake. It is the smallest thing dialBuilder can succeed against.
func serveMiniSSH(addr string, hostKey ssh.Signer, authorized ssh.PublicKey, errc chan<- error) {
	cfg := &ssh.ServerConfig{
		PublicKeyCallback: func(_ ssh.ConnMetadata, key ssh.PublicKey) (*ssh.Permissions, error) {
			if !bytesEqualKey(key, authorized) {
				return nil, errors.New("unauthorized key")
			}
			return &ssh.Permissions{}, nil
		},
	}
	cfg.AddHostKey(hostKey)

	ln, err := net.Listen("tcp", addr)
	if err != nil {
		errc <- err
		return
	}
	defer func() { _ = ln.Close() }()
	errc <- nil

	conn, err := ln.Accept()
	if err != nil {
		return
	}
	sshConn, chans, reqs, err := ssh.NewServerConn(conn, cfg)
	if err != nil {
		return
	}
	go ssh.DiscardRequests(reqs)
	go func() {
		for nc := range chans {
			_ = nc.Reject(ssh.Prohibited, "not needed")
		}
	}()
	time.Sleep(2 * time.Second)
	_ = sshConn.Close()
}

func bytesEqualKey(a, b ssh.PublicKey) bool {
	return string(a.Marshal()) == string(b.Marshal())
}

// The controller hands the proxy a pod IP as soon as the pod has one, which is
// routinely before sshd is listening. The dial is what has to absorb that gap.
func TestDialBuilderRetriesWhileBuilderIsStillStarting(t *testing.T) {
	hostKey := newSigner(t)
	clientKey := newSigner(t)
	addr := reservePort(t)

	cfg := &ssh.ClientConfig{
		User:            "nixbld",
		Auth:            []ssh.AuthMethod{ssh.PublicKeys(clientKey)},
		HostKeyCallback: ssh.FixedHostKey(hostKey.PublicKey()),
		Timeout:         2 * time.Second,
	}

	listenErr := make(chan error, 1)
	go func() {
		// Long enough that the first several dials are refused.
		time.Sleep(300 * time.Millisecond)
		serveMiniSSH(addr, hostKey, clientKey.PublicKey(), listenErr)
	}()

	start := time.Now()
	client, err := dialBuilder(context.Background(), zerolog.Nop(), addr, cfg, 15*time.Second)
	if err != nil {
		t.Fatalf("dialBuilder did not recover from a not-yet-listening builder: %v", err)
	}
	defer func() { _ = client.Close() }()

	if elapsed := time.Since(start); elapsed < 250*time.Millisecond {
		t.Errorf("connected in %v, before the server was started; test is not exercising the retry", elapsed)
	}
	if err := <-listenErr; err != nil {
		t.Fatalf("mini ssh server failed to listen: %v", err)
	}
}

// A builder that never comes up must fail with a bounded, descriptive error
// rather than hanging for the whole session.
func TestDialBuilderGivesUpAfterTimeout(t *testing.T) {
	addr := reservePort(t)
	cfg := &ssh.ClientConfig{
		User:            "nixbld",
		Auth:            []ssh.AuthMethod{ssh.PublicKeys(newSigner(t))},
		HostKeyCallback: ssh.FixedHostKey(newSigner(t).PublicKey()),
		Timeout:         time.Second,
	}

	start := time.Now()
	_, err := dialBuilder(context.Background(), zerolog.Nop(), addr, cfg, 400*time.Millisecond)
	elapsed := time.Since(start)

	if err == nil {
		t.Fatal("expected an error dialling a port with no listener")
	}
	if !strings.Contains(err.Error(), "attempts") {
		t.Errorf("error should report how many attempts were made, got: %v", err)
	}
	if elapsed < 400*time.Millisecond {
		t.Errorf("gave up after %v, before the timeout elapsed", elapsed)
	}
	if elapsed > 5*time.Second {
		t.Errorf("took %v to give up on a 400ms budget", elapsed)
	}
}

// A rejected host key is a misconfiguration, not a pod that is still starting:
// retrying it until the timeout would turn a clear error into a slow one.
func TestDialBuilderDoesNotRetryHostKeyMismatch(t *testing.T) {
	hostKey := newSigner(t)
	clientKey := newSigner(t)
	addr := reservePort(t)

	listenErr := make(chan error, 1)
	go serveMiniSSH(addr, hostKey, clientKey.PublicKey(), listenErr)
	if err := <-listenErr; err != nil {
		t.Fatalf("mini ssh server failed to listen: %v", err)
	}

	cfg := &ssh.ClientConfig{
		User:            "nixbld",
		Auth:            []ssh.AuthMethod{ssh.PublicKeys(clientKey)},
		HostKeyCallback: ssh.FixedHostKey(newSigner(t).PublicKey()), // not the server's key
		Timeout:         2 * time.Second,
	}

	start := time.Now()
	_, err := dialBuilder(context.Background(), zerolog.Nop(), addr, cfg, 10*time.Second)
	elapsed := time.Since(start)

	if err == nil {
		t.Fatal("expected a host key mismatch error")
	}
	if elapsed > 3*time.Second {
		t.Errorf("host key mismatch took %v; it should fail fast, not retry to the timeout", elapsed)
	}
}

func TestIsBuilderNotUpYetClassifiesErrors(t *testing.T) {
	// A refused connection is the normal state while a pod is starting.
	addr := reservePort(t)
	_, err := net.DialTimeout("tcp", addr, time.Second)
	if err == nil {
		t.Skip("port unexpectedly accepted a connection")
	}
	if !isBuilderNotUpYet(err) {
		t.Errorf("connection refused should be treated as still-starting, got %v", err)
	}

	// An arbitrary failure is not.
	if isBuilderNotUpYet(errors.New("ssh: unable to authenticate")) {
		t.Error("an auth failure must not be treated as still-starting")
	}
}
