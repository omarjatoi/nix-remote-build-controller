package proxy

import (
	"bytes"
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"errors"
	"fmt"
	"io"
	"net"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"golang.org/x/crypto/ssh"
	"k8s.io/apimachinery/pkg/runtime"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"

	"github.com/omarjatoi/nix-remote-build-controller/pkg/apis/nixbuilder/v1alpha1"
)

const testNamespace = "nix-test"

func newSigner(t *testing.T) ssh.Signer {
	t.Helper()
	_, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	signer, err := ssh.NewSignerFromKey(priv)
	if err != nil {
		t.Fatal(err)
	}
	return signer
}

// fakeBuilder is an in-process stand-in for a builder pod's sshd. It supports a
// few commands that make concurrency and lifecycle observable from tests.
type fakeBuilder struct {
	t        *testing.T
	listener net.Listener
	config   *ssh.ServerConfig
	hostKey  ssh.Signer

	active    atomic.Int32
	peak      atomic.Int32
	sessions  atomic.Int32
	arrived   atomic.Int32 // monotonic count of sessions that reached the barrier
	hangEnded chan struct{}
}

func newFakeBuilder(t *testing.T, proxyKey ssh.PublicKey) *fakeBuilder {
	t.Helper()
	hostKey := newSigner(t)
	b := &fakeBuilder{t: t, hostKey: hostKey, hangEnded: make(chan struct{}, 16)}
	b.config = &ssh.ServerConfig{
		PublicKeyCallback: func(conn ssh.ConnMetadata, key ssh.PublicKey) (*ssh.Permissions, error) {
			if conn.User() != "nixbld" {
				return nil, fmt.Errorf("unexpected user %q", conn.User())
			}
			if !bytes.Equal(key.Marshal(), proxyKey.Marshal()) {
				return nil, errors.New("unauthorized key")
			}
			return &ssh.Permissions{}, nil
		},
	}
	b.config.AddHostKey(hostKey)
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	b.listener = ln
	go b.serve()
	t.Cleanup(func() { _ = ln.Close() })
	return b
}

func (b *fakeBuilder) port() int32 {
	return int32(b.listener.Addr().(*net.TCPAddr).Port)
}

func (b *fakeBuilder) serve() {
	for {
		conn, err := b.listener.Accept()
		if err != nil {
			return
		}
		go b.handleConn(conn)
	}
}

func (b *fakeBuilder) handleConn(conn net.Conn) {
	sshConn, chans, reqs, err := ssh.NewServerConn(conn, b.config)
	if err != nil {
		return
	}
	defer func() { _ = sshConn.Close() }()
	go ssh.DiscardRequests(reqs)
	connClosed := make(chan struct{})
	go func() {
		_ = sshConn.Wait()
		close(connClosed)
	}()
	for nc := range chans {
		go b.handleSession(nc, connClosed)
	}
}

func (b *fakeBuilder) handleSession(nc ssh.NewChannel, connClosed <-chan struct{}) {
	ch, reqs, err := nc.Accept()
	if err != nil {
		return
	}
	defer func() { _ = ch.Close() }()
	b.sessions.Add(1)
	n := b.active.Add(1)
	for {
		p := b.peak.Load()
		if n <= p || b.peak.CompareAndSwap(p, n) {
			break
		}
	}
	defer b.active.Add(-1)

	var cmd string
	for req := range reqs {
		if req.Type == "exec" {
			var m struct{ Command string }
			_ = ssh.Unmarshal(req.Payload, &m)
			cmd = m.Command
			_ = req.Reply(true, nil)
			break
		}
		if req.WantReply {
			_ = req.Reply(false, nil)
		}
	}
	go func() {
		for req := range reqs {
			if req.WantReply {
				_ = req.Reply(false, nil)
			}
		}
	}()

	status := b.run(cmd, ch, connClosed)
	_ = ch.CloseWrite()
	_, _ = ch.SendRequest("exit-status", false, ssh.Marshal(exitStatusMsg{Status: status}))
}

func (b *fakeBuilder) run(cmd string, ch ssh.Channel, connClosed <-chan struct{}) uint32 {
	switch {
	case cmd == "echo-upper":
		return echoUpper(ch)
	case strings.HasPrefix(cmd, "wait-for-peers="):
		want, _ := strconv.Atoi(strings.TrimPrefix(cmd, "wait-for-peers="))
		// A latching barrier: every session that reaches this point bumps the
		// monotonic "arrived" counter and waits until "want" sessions have
		// arrived. Because arrived never decreases, no session can slip past
		// before all of them are concurrently blocked here, so passing the
		// barrier proves the proxy ran them in parallel.
		b.arrived.Add(1)
		deadline := time.Now().Add(8 * time.Second)
		for int(b.arrived.Load()) < want {
			if time.Now().After(deadline) {
				_, _ = fmt.Fprintf(ch.Stderr(), "only %d of %d peers arrived\n", b.arrived.Load(), want)
				return 2
			}
			time.Sleep(2 * time.Millisecond)
		}
		return echoUpper(ch)
	case strings.HasPrefix(cmd, "exit="):
		_, _ = io.Copy(io.Discard, ch)
		code, _ := strconv.Atoi(strings.TrimPrefix(cmd, "exit="))
		_, _ = fmt.Fprintf(ch.Stderr(), "exiting with %d\n", code)
		return uint32(code)
	case cmd == "hang":
		// Ignore stdin EOF; only end when the connection is torn down.
		_, _ = io.Copy(io.Discard, ch)
		<-connClosed
		b.hangEnded <- struct{}{}
		return 0
	default:
		_, _ = fmt.Fprintf(ch.Stderr(), "unknown command %q\n", cmd)
		return 127
	}
}

func echoUpper(ch ssh.Channel) uint32 {
	data, _ := io.ReadAll(ch)
	_, _ = ch.Write(bytes.ToUpper(data))
	_, _ = ch.Stderr().Write([]byte("stderr-marker\n"))
	return 0
}

// fakeController mimics the real controller against the fake API client.
type fakeController struct {
	c    client.Client
	mode string // "running" or "failed" or "never"

	mu    sync.Mutex
	seen  map[string]struct{}
	peak  int
	total int
}

func newFakeController(c client.Client, mode string) *fakeController {
	return &fakeController{c: c, mode: mode, seen: make(map[string]struct{})}
}

func (f *fakeController) run(ctx context.Context) {
	ticker := time.NewTicker(10 * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
		var list v1alpha1.NixBuildRequestList
		if err := f.c.List(ctx, &list, client.InNamespace(testNamespace)); err != nil {
			continue
		}
		f.mu.Lock()
		if len(list.Items) > f.peak {
			f.peak = len(list.Items)
		}
		for i := range list.Items {
			f.seen[list.Items[i].Name] = struct{}{}
		}
		f.total = len(f.seen)
		f.mu.Unlock()

		for i := range list.Items {
			br := &list.Items[i]
			if br.Status.Phase != "" || f.mode == "never" {
				continue
			}
			switch f.mode {
			case "failed":
				br.Status.Phase = v1alpha1.BuildPhaseFailed
				br.Status.Message = "Builder pod not scheduled: Unschedulable: no nodes"
			default:
				br.Status.Phase = v1alpha1.BuildPhaseRunning
				br.Status.PodName = "nix-builder-" + br.Spec.SessionID
				br.Status.PodIP = "127.0.0.1"
			}
			_ = f.c.Status().Update(ctx, br)
		}
	}
}

func (f *fakeController) stats() (peak, total int) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.peak, f.total
}

type harness struct {
	proxy      *SSHProxy
	builder    *fakeBuilder
	controller *fakeController
	k8s        client.Client
	clientKey  ssh.Signer
	cancel     context.CancelFunc
	started    chan error
}

type harnessOpts struct {
	controllerMode string
	clientAuth     *ClientAuthorizer
	cfg            func(*Config)
}

func newHarness(t *testing.T, opts harnessOpts) *harness {
	t.Helper()
	scheme := runtime.NewScheme()
	if err := clientgoscheme.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	if err := v1alpha1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	k8s := fake.NewClientBuilder().WithScheme(scheme).WithStatusSubresource(&v1alpha1.NixBuildRequest{}).Build()

	builderClientKey := newSigner(t)
	builder := newFakeBuilder(t, builderClientKey.PublicKey())

	cfg := Config{
		ListenAddr:        "127.0.0.1:0",
		Namespace:         testNamespace,
		RemoteUser:        "nixbld",
		RemotePort:        builder.port(),
		PodReadyTimeout:   5 * time.Second,
		PollInterval:      10 * time.Millisecond,
		KeepaliveInterval: 200 * time.Millisecond,
		ClientCloseGrace:  time.Second,
		ShutdownTimeout:   10 * time.Second,
		BuildTimeout:      90 * time.Minute,
		OwnerPodName:      "proxy-0",
		OwnerPodUID:       "uid-proxy-0",
	}
	if opts.cfg != nil {
		opts.cfg(&cfg)
	}
	mode := opts.controllerMode
	if mode == "" {
		mode = "running"
	}

	p, err := New(cfg, k8s, newSigner(t), builderClientKey, opts.clientAuth, builder.hostKey.PublicKey())
	if err != nil {
		t.Fatal(err)
	}

	ctx, cancel := context.WithCancel(context.Background())
	fc := newFakeController(k8s, mode)
	go fc.run(ctx)
	h := &harness{proxy: p, builder: builder, controller: fc, k8s: k8s, clientKey: newSigner(t), cancel: cancel, started: make(chan error, 1)}
	go func() { h.started <- p.Start(ctx) }()
	t.Cleanup(func() {
		cancel()
		select {
		case <-h.started:
		case <-time.After(30 * time.Second):
			t.Error("proxy did not stop")
		}
	})
	return h
}

func (h *harness) dial(t *testing.T) *ssh.Client {
	t.Helper()
	c, err := ssh.Dial("tcp", h.proxy.Addr().String(), &ssh.ClientConfig{
		User:            "nixbld",
		Auth:            []ssh.AuthMethod{ssh.PublicKeys(h.clientKey)},
		HostKeyCallback: ssh.InsecureIgnoreHostKey(), //nolint:gosec // test
		Timeout:         5 * time.Second,
	})
	if err != nil {
		t.Fatalf("dial proxy: %v", err)
	}
	t.Cleanup(func() { _ = c.Close() })
	return c
}

// runCommand executes cmd through the proxy, feeding stdin, and returns stdout,
// stderr and the exit error (nil for status 0).
func runCommand(t *testing.T, c *ssh.Client, cmd, stdin string) (string, string, error) {
	t.Helper()
	sess, err := c.NewSession()
	if err != nil {
		t.Fatalf("new session: %v", err)
	}
	defer func() { _ = sess.Close() }()
	var stdout, stderr bytes.Buffer
	sess.Stdout = &stdout
	sess.Stderr = &stderr
	sess.Stdin = strings.NewReader(stdin)
	if err := sess.Start(cmd); err != nil {
		t.Fatalf("start %q: %v", cmd, err)
	}
	err = sess.Wait()
	return stdout.String(), stderr.String(), err
}

func (h *harness) listRequests(t *testing.T) []v1alpha1.NixBuildRequest {
	t.Helper()
	var list v1alpha1.NixBuildRequestList
	if err := h.k8s.List(context.Background(), &list, client.InNamespace(testNamespace)); err != nil {
		t.Fatal(err)
	}
	return list.Items
}

func (h *harness) waitNoRequests(t *testing.T) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if len(h.listRequests(t)) == 0 && h.proxy.SessionCount() == 0 {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatalf("expected all NixBuildRequests to be deleted, still have %d (sessions=%d)", len(h.listRequests(t)), h.proxy.SessionCount())
}

func exitStatus(err error) int {
	var ee *ssh.ExitError
	if errors.As(err, &ee) {
		return ee.ExitStatus()
	}
	if err == nil {
		return 0
	}
	return -1
}

func TestConcurrentConnectionsGetIndependentBuildersAndRunInParallel(t *testing.T) {
	h := newHarness(t, harnessOpts{})
	const n = 4

	var wg sync.WaitGroup
	errs := make(chan error, n)
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			c := h.dial(t)
			in := fmt.Sprintf("payload-%d\n", i)
			// Every session blocks inside the fake builder until n sessions are
			// active at once, so this only completes if the proxy runs them
			// concurrently rather than one after another.
			out, stderr, err := runCommand(t, c, fmt.Sprintf("wait-for-peers=%d", n), in)
			if err != nil {
				errs <- fmt.Errorf("session %d: %v (stderr %q)", i, err, stderr)
				return
			}
			if out != strings.ToUpper(in) {
				errs <- fmt.Errorf("session %d: stdout %q", i, out)
			}
			if !strings.Contains(stderr, "stderr-marker") {
				errs <- fmt.Errorf("session %d: stderr not forwarded: %q", i, stderr)
			}
		}(i)
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		t.Error(err)
	}

	if peak := h.builder.peak.Load(); peak != n {
		t.Errorf("peak concurrent builder sessions = %d, want %d", peak, n)
	}
	peakReqs, total := h.controller.stats()
	if total != n {
		t.Errorf("distinct NixBuildRequests = %d, want %d", total, n)
	}
	if peakReqs != n {
		t.Errorf("peak simultaneous NixBuildRequests = %d, want %d", peakReqs, n)
	}
	h.waitNoRequests(t)
}

func TestMultipleChannelsOnOneConnectionAreIndependentSessions(t *testing.T) {
	h := newHarness(t, harnessOpts{})
	c := h.dial(t)
	const n = 3

	var wg sync.WaitGroup
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			in := fmt.Sprintf("chan-%d", i)
			out, _, err := runCommand(t, c, fmt.Sprintf("wait-for-peers=%d", n), in)
			if err != nil || out != strings.ToUpper(in) {
				t.Errorf("channel %d: out=%q err=%v", i, out, err)
			}
		}(i)
	}
	wg.Wait()
	if _, total := h.controller.stats(); total != n {
		t.Errorf("distinct NixBuildRequests = %d, want %d", total, n)
	}
	h.waitNoRequests(t)
}

func TestBuildRequestCarriesTimeoutAndOwnerReference(t *testing.T) {
	h := newHarness(t, harnessOpts{controllerMode: "never", cfg: func(c *Config) { c.PodReadyTimeout = 2 * time.Second }})
	c := h.dial(t)
	sess, err := c.NewSession()
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = sess.Close() }()
	if err := sess.Start("echo-upper"); err != nil {
		t.Fatal(err)
	}

	var reqs []v1alpha1.NixBuildRequest
	deadline := time.Now().Add(2 * time.Second)
	for len(reqs) == 0 && time.Now().Before(deadline) {
		reqs = h.listRequests(t)
		time.Sleep(10 * time.Millisecond)
	}
	if len(reqs) != 1 {
		t.Fatalf("expected one NixBuildRequest, got %d", len(reqs))
	}
	br := reqs[0]
	if br.Spec.TimeoutSeconds == nil || *br.Spec.TimeoutSeconds != 90*60 {
		t.Errorf("timeoutSeconds = %v, want 5400", br.Spec.TimeoutSeconds)
	}
	if br.Name != BuildRequestName(br.Spec.SessionID) || br.Labels[LabelSessionID] != br.Spec.SessionID {
		t.Errorf("unexpected naming/labels: %+v", br.ObjectMeta)
	}
	if len(br.OwnerReferences) != 1 || br.OwnerReferences[0].Kind != "Pod" || br.OwnerReferences[0].Name != "proxy-0" {
		t.Errorf("ownerReferences = %+v", br.OwnerReferences)
	}
	_ = sess.Close()
	_ = c.Close()
	h.waitNoRequests(t)
}

func TestSubSecondBuildTimeoutSetsNoDeadline(t *testing.T) {
	h := newHarness(t, harnessOpts{controllerMode: "never", cfg: func(c *Config) {
		c.BuildTimeout = 500 * time.Millisecond // truncates to 0 seconds
		c.PodReadyTimeout = 2 * time.Second
	}})
	c := h.dial(t)
	sess, err := c.NewSession()
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = sess.Close() }()
	if err := sess.Start("echo-upper"); err != nil {
		t.Fatal(err)
	}

	var reqs []v1alpha1.NixBuildRequest
	deadline := time.Now().Add(2 * time.Second)
	for len(reqs) == 0 && time.Now().Before(deadline) {
		reqs = h.listRequests(t)
		time.Sleep(10 * time.Millisecond)
	}
	if len(reqs) != 1 {
		t.Fatalf("expected one NixBuildRequest, got %d", len(reqs))
	}
	if reqs[0].Spec.TimeoutSeconds != nil {
		t.Errorf("sub-second BuildTimeout must not set a (zero) deadline, got %d", *reqs[0].Spec.TimeoutSeconds)
	}
	_ = sess.Close()
	_ = c.Close()
	h.waitNoRequests(t)
}

func TestExitStatusIsPropagated(t *testing.T) {
	h := newHarness(t, harnessOpts{})
	c := h.dial(t)
	_, stderr, err := runCommand(t, c, "exit=3", "ignored")
	if got := exitStatus(err); got != 3 {
		t.Fatalf("exit status = %d (err %v), want 3", got, err)
	}
	if !strings.Contains(stderr, "exiting with 3") {
		t.Errorf("stderr = %q", stderr)
	}
	h.waitNoRequests(t)
}

func TestControllerFailureIsReportedToClient(t *testing.T) {
	h := newHarness(t, harnessOpts{controllerMode: "failed"})
	c := h.dial(t)
	_, stderr, err := runCommand(t, c, "echo-upper", "x")
	if got := exitStatus(err); got != 255 {
		t.Fatalf("exit status = %d (err %v), want 255", got, err)
	}
	if !strings.Contains(stderr, "Unschedulable") {
		t.Errorf("stderr should carry the controller's message, got %q", stderr)
	}
	if h.builder.sessions.Load() != 0 {
		t.Error("builder must not be contacted when the request failed")
	}
	h.waitNoRequests(t)
}

func TestPodReadyTimeoutFailsSession(t *testing.T) {
	h := newHarness(t, harnessOpts{controllerMode: "never", cfg: func(c *Config) { c.PodReadyTimeout = 200 * time.Millisecond }})
	c := h.dial(t)
	start := time.Now()
	_, stderr, err := runCommand(t, c, "echo-upper", "x")
	if got := exitStatus(err); got != 255 {
		t.Fatalf("exit status = %d (err %v), want 255", got, err)
	}
	if !strings.Contains(stderr, "timed out") {
		t.Errorf("stderr = %q", stderr)
	}
	if time.Since(start) > 3*time.Second {
		t.Errorf("timeout took %s", time.Since(start))
	}
	h.waitNoRequests(t)
}

func TestClientDisconnectTearsDownBuilderSession(t *testing.T) {
	h := newHarness(t, harnessOpts{})
	c := h.dial(t)
	sess, err := c.NewSession()
	if err != nil {
		t.Fatal(err)
	}
	stdin, err := sess.StdinPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err := sess.Start("hang"); err != nil {
		t.Fatal(err)
	}
	if _, err := stdin.Write([]byte("hello")); err != nil {
		t.Fatal(err)
	}
	// Wait until the builder actually has the session.
	deadline := time.Now().Add(5 * time.Second)
	for h.builder.active.Load() == 0 && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	if h.builder.active.Load() != 1 {
		t.Fatal("builder session not established")
	}

	// Simulate the Nix client dying (Ctrl-C): drop the whole connection.
	_ = c.Close()

	select {
	case <-h.builder.hangEnded:
	case <-time.After(5 * time.Second):
		t.Fatal("builder session was not torn down after client disconnect")
	}
	h.waitNoRequests(t)
}

func TestClientChannelCloseGivesBuilderGraceThenForces(t *testing.T) {
	h := newHarness(t, harnessOpts{cfg: func(c *Config) { c.ClientCloseGrace = 300 * time.Millisecond }})
	c := h.dial(t)
	sess, err := c.NewSession()
	if err != nil {
		t.Fatal(err)
	}
	if err := sess.Start("hang"); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(5 * time.Second)
	for h.builder.active.Load() == 0 && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	start := time.Now()
	_ = sess.Close() // close only the channel; the connection stays up
	select {
	case <-h.builder.hangEnded:
		if time.Since(start) < 250*time.Millisecond {
			t.Errorf("builder was torn down before the grace period: %s", time.Since(start))
		}
	case <-time.After(5 * time.Second):
		t.Fatal("builder session was not torn down after client closed its channel")
	}
	h.waitNoRequests(t)
}

func TestMaxSessionsRejectsExcessChannels(t *testing.T) {
	h := newHarness(t, harnessOpts{cfg: func(c *Config) { c.MaxSessions = 1 }})
	c := h.dial(t)
	first, err := c.NewSession()
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = first.Close() }()
	if err := first.Start("hang"); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(5 * time.Second)
	for h.proxy.SessionCount() == 0 && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}

	second, err := c.NewSession()
	if err == nil {
		_ = second.Close()
		t.Fatal("second session should have been rejected")
	}
	var oce *ssh.OpenChannelError
	if !errors.As(err, &oce) || oce.Reason != ssh.ResourceShortage {
		t.Errorf("unexpected rejection error: %v", err)
	}
}

func TestClientPublicKeyAuthentication(t *testing.T) {
	authorized := newSigner(t)
	authKeys := ssh.MarshalAuthorizedKey(authorized.PublicKey())
	auth, err := ParseAuthorizedKeys(append([]byte("# comment\n\n"), authKeys...))
	if err != nil {
		t.Fatal(err)
	}
	h := newHarness(t, harnessOpts{clientAuth: auth})

	// Wrong key: rejected during the handshake.
	if _, err := ssh.Dial("tcp", h.proxy.Addr().String(), &ssh.ClientConfig{
		User:            "nixbld",
		Auth:            []ssh.AuthMethod{ssh.PublicKeys(newSigner(t))},
		HostKeyCallback: ssh.InsecureIgnoreHostKey(), //nolint:gosec // test
		Timeout:         5 * time.Second,
	}); err == nil {
		t.Fatal("unauthorized key was accepted")
	}

	// Right key: works end to end.
	h.clientKey = authorized
	c := h.dial(t)
	out, _, err := runCommand(t, c, "echo-upper", "ok")
	if err != nil || out != "OK" {
		t.Fatalf("out=%q err=%v", out, err)
	}
	h.waitNoRequests(t)
}

func TestParseAuthorizedKeysRejectsGarbage(t *testing.T) {
	if _, err := ParseAuthorizedKeys([]byte("ssh-ed25519 not-base64 x\n")); err == nil {
		t.Error("expected error for invalid key line")
	}
	if _, err := ParseAuthorizedKeys([]byte("# only comments\n")); err == nil {
		t.Error("expected error for empty key set")
	}
}

func TestShutdownDrainsInFlightSessionsAndRefusesNewConnections(t *testing.T) {
	h := newHarness(t, harnessOpts{})
	c := h.dial(t)
	sess, err := c.NewSession()
	if err != nil {
		t.Fatal(err)
	}
	stdin, err := sess.StdinPipe()
	if err != nil {
		t.Fatal(err)
	}
	var stdout bytes.Buffer
	sess.Stdout = &stdout
	if err := sess.Start("echo-upper"); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(5 * time.Second)
	for h.builder.active.Load() == 0 && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}

	// Signal shutdown while the session is mid-flight.
	h.cancel()
	time.Sleep(100 * time.Millisecond)

	// New connections are refused...
	if _, err := net.DialTimeout("tcp", h.proxy.Addr().String(), time.Second); err == nil {
		t.Error("listener should be closed after shutdown signal")
	}
	select {
	case err := <-h.started:
		t.Fatalf("proxy stopped before in-flight session finished: %v", err)
	default:
	}

	// ...but the in-flight session completes normally.
	if _, err := stdin.Write([]byte("draining")); err != nil {
		t.Fatal(err)
	}
	_ = stdin.Close()
	if err := sess.Wait(); err != nil {
		t.Fatalf("session failed during drain: %v", err)
	}
	if stdout.String() != "DRAINING" {
		t.Errorf("stdout = %q", stdout.String())
	}
	_ = c.Close()

	select {
	case err := <-h.started:
		if err != nil {
			t.Errorf("Start returned %v", err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("proxy did not stop after sessions drained")
	}
	h.started <- nil // let Cleanup observe a result
}

func TestShutdownTimeoutForcesSessionsClosed(t *testing.T) {
	h := newHarness(t, harnessOpts{cfg: func(c *Config) { c.ShutdownTimeout = 300 * time.Millisecond }})
	c := h.dial(t)
	sess, err := c.NewSession()
	if err != nil {
		t.Fatal(err)
	}
	if err := sess.Start("hang"); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(5 * time.Second)
	for h.builder.active.Load() == 0 && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}

	h.cancel()
	select {
	case err := <-h.started:
		if err != nil {
			t.Errorf("Start returned %v", err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("proxy did not force-close sessions after the drain timeout")
	}
	h.started <- nil
	if err := sess.Wait(); err == nil {
		t.Error("client session should have been terminated")
	}
	h.waitNoRequests(t)
}
