package proxy

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"strconv"
	"sync"
	"sync/atomic"
	"time"

	"github.com/rs/zerolog"
	"golang.org/x/crypto/ssh"
)

type exitStatusMsg struct {
	Status uint32
}

type exitSignalMsg struct {
	Signal     string
	CoreDumped bool
	Error      string
	Lang       string
}

// routeToBuilder splices the client's session channel onto a new session on the
// builder pod and returns once the builder-side command has exited (or the
// client went away).
//
// Ordering matters for correctness:
//   - Channel requests from the client (env, exec) are forwarded first; stdin
//     data is only forwarded once the exec/shell/subsystem request has been
//     relayed, so the builder's sshd never sees data for a command that has not
//     been started yet.
//   - The client channel is closed only after the builder's exit-status request
//     has been forwarded, so Nix sees the real exit code.
//   - Client disconnects cancel ctx, which closes the builder connection and
//     therefore kills the remote nix-store process.
func (p *SSHProxy) routeToBuilder(ctx context.Context, logger zerolog.Logger, session *Session, clientCh ssh.Channel, clientReqs <-chan *ssh.Request, podIP string) SessionResult {
	builderAddr := net.JoinHostPort(podIP, strconv.Itoa(int(p.cfg.RemotePort)))

	builderConn, err := dialSSH(ctx, builderAddr, &ssh.ClientConfig{
		User:            p.cfg.RemoteUser,
		Auth:            []ssh.AuthMethod{ssh.PublicKeys(p.builderClientKey)},
		HostKeyCallback: p.builderHostKey,
		Timeout:         p.cfg.BuilderDialTimeout,
	})
	if err != nil {
		err = fmt.Errorf("connect to builder pod %s: %w", builderAddr, err)
		failClient(clientCh, clientReqs, err.Error())
		return SessionResult{Err: err}
	}
	defer func() { _ = builderConn.Close() }()

	tunnelCtx, cancelTunnel := context.WithCancel(ctx)
	defer cancelTunnel()
	go func() {
		<-tunnelCtx.Done()
		_ = builderConn.Close()
	}()
	if p.cfg.KeepaliveInterval > 0 {
		go keepalive(tunnelCtx, builderConn, p.cfg.KeepaliveInterval, func(err error) {
			logger.Warn().Err(err).Msg("Builder keepalive failed, closing tunnel")
			cancelTunnel()
		})
	}

	builderCh, builderReqs, err := builderConn.OpenChannel("session", nil)
	if err != nil {
		err = fmt.Errorf("open session channel on builder pod %s: %w", builderAddr, err)
		failClient(clientCh, clientReqs, err.Error())
		return SessionResult{Err: err}
	}
	defer func() { _ = builderCh.Close() }()

	logger.Info().Str("builder_addr", builderAddr).Msg("Connected to builder pod, tunnelling session")

	var (
		wg          sync.WaitGroup
		result      SessionResult
		exitStatus  atomic.Pointer[uint32] // published by the builder-request goroutine
		execStarted = make(chan struct{})
		execOnce    sync.Once
		clientDone  = make(chan struct{})
		builderDone = make(chan struct{})
		outputDone  = make(chan struct{})
		copyErrMu   sync.Mutex
		copyErr     error
	)
	recordCopyErr := func(direction string, err error) {
		if err == nil || errors.Is(err, io.EOF) || tunnelCtx.Err() != nil {
			return
		}
		copyErrMu.Lock()
		defer copyErrMu.Unlock()
		if copyErr == nil {
			copyErr = fmt.Errorf("%s: %w", direction, err)
		}
	}
	markExecStarted := func() { execOnce.Do(func() { close(execStarted) }) }

	// Requests client -> builder. Ends when the client closes its channel.
	wg.Add(1)
	go func() {
		defer wg.Done()
		defer close(clientDone)
		defer markExecStarted()
		for req := range clientReqs {
			ok, err := builderCh.SendRequest(req.Type, req.WantReply, req.Payload)
			logger.Debug().Str("type", req.Type).Bool("accepted", ok).Err(err).Msg("Forwarded request client->builder")
			if req.WantReply {
				_ = req.Reply(ok && err == nil, nil)
			}
			if err != nil {
				return
			}
			switch req.Type {
			case "exec", "shell", "subsystem":
				markExecStarted()
			}
		}
	}()

	// Data client -> builder (stdin of nix-store --serve).
	wg.Add(1)
	go func() {
		defer wg.Done()
		select {
		case <-execStarted:
		case <-tunnelCtx.Done():
			return
		}
		n, err := io.Copy(builderCh, clientCh)
		logger.Debug().Int64("bytes", n).Err(err).Msg("client->builder stdin copy finished")
		recordCopyErr("client->builder", err)
		_ = builderCh.CloseWrite()
	}()

	// Data builder -> client (stdout and stderr). Once both hit EOF the client
	// gets EOF too.
	var outWg sync.WaitGroup
	outWg.Add(2)
	wg.Add(1)
	go func() {
		defer wg.Done()
		defer outWg.Done()
		n, err := io.Copy(clientCh, builderCh)
		logger.Debug().Int64("bytes", n).Err(err).Msg("builder->client stdout copy finished")
		recordCopyErr("builder->client stdout", err)
	}()
	wg.Add(1)
	go func() {
		defer wg.Done()
		defer outWg.Done()
		n, err := io.Copy(clientCh.Stderr(), builderCh.Stderr())
		logger.Debug().Int64("bytes", n).Err(err).Msg("builder->client stderr copy finished")
		recordCopyErr("builder->client stderr", err)
	}()
	go func() {
		outWg.Wait()
		_ = clientCh.CloseWrite()
		close(outputDone)
	}()

	// Requests builder -> client (exit-status, exit-signal). Ends when the
	// builder closes its channel, i.e. when the remote command has exited.
	wg.Add(1)
	go func() {
		defer wg.Done()
		defer close(builderDone)
		for req := range builderReqs {
			switch req.Type {
			case "exit-status":
				var msg exitStatusMsg
				if ssh.Unmarshal(req.Payload, &msg) == nil {
					status := msg.Status
					exitStatus.Store(&status)
				}
			case "exit-signal":
				var msg exitSignalMsg
				if ssh.Unmarshal(req.Payload, &msg) == nil {
					logger.Warn().Str("signal", msg.Signal).Str("error", msg.Error).Msg("Builder command terminated by signal")
				}
			}
			ok, err := clientCh.SendRequest(req.Type, req.WantReply, req.Payload)
			logger.Debug().Str("type", req.Type).Bool("accepted", ok).Err(err).Msg("Forwarded request builder->client")
			if req.WantReply {
				_ = req.Reply(ok && err == nil, nil)
			}
		}
	}()

	// Wait for the session to end.
	select {
	case <-builderDone:
		// Normal completion: remote command exited and the channel closed.
	case <-clientDone:
		// Client closed its channel: EOF has been propagated to the builder;
		// give the remote command a moment to exit on its own.
		select {
		case <-builderDone:
		case <-time.After(p.cfg.ClientCloseGrace):
			logger.Warn().Dur("grace", p.cfg.ClientCloseGrace).Msg("Builder did not exit after client closed the session; forcing teardown")
			result.Err = errors.New("client closed the session before the builder finished")
		case <-tunnelCtx.Done():
			result.Err = fmt.Errorf("session cancelled: %w", context.Cause(tunnelCtx))
		}
	case <-tunnelCtx.Done():
		result.Err = fmt.Errorf("session cancelled: %w", context.Cause(tunnelCtx))
	}

	// Flush remaining builder output to the client before closing the channel.
	select {
	case <-outputDone:
	case <-tunnelCtx.Done():
	case <-time.After(p.cfg.ClientCloseGrace):
		logger.Warn().Msg("Timed out flushing builder output to client")
	}
	_ = clientCh.Close()
	cancelTunnel() // closes the builder connection and unblocks every goroutine

	finished := make(chan struct{})
	go func() {
		wg.Wait()
		close(finished)
	}()
	select {
	case <-finished:
	case <-time.After(15 * time.Second):
		logger.Warn().Msg("Tunnel goroutines did not finish promptly")
	}

	result.ExitStatus = exitStatus.Load()
	if result.Err == nil {
		copyErrMu.Lock()
		result.Err = copyErr
		copyErrMu.Unlock()
	}

	evt := logger.Info()
	if result.Err != nil {
		evt = logger.Warn().Err(result.Err)
	}
	if result.ExitStatus != nil {
		evt = evt.Uint32("exit_status", *result.ExitStatus)
	}
	evt.Msg("Tunnel closed")
	return result
}

// dialSSH establishes an SSH client connection honouring ctx for the TCP dial
// and cfg.Timeout for the handshake.
func dialSSH(ctx context.Context, addr string, cfg *ssh.ClientConfig) (*ssh.Client, error) {
	dialer := net.Dialer{Timeout: cfg.Timeout}
	conn, err := dialer.DialContext(ctx, "tcp", addr)
	if err != nil {
		return nil, err
	}
	if cfg.Timeout > 0 {
		_ = conn.SetDeadline(time.Now().Add(cfg.Timeout))
	}
	c, chans, reqs, err := ssh.NewClientConn(conn, addr, cfg)
	if err != nil {
		_ = conn.Close()
		return nil, err
	}
	_ = conn.SetDeadline(time.Time{})
	return ssh.NewClient(c, chans, reqs), nil
}

// failClient reports a proxy-side failure to the Nix client in a way that shows
// up in its logs: pending requests (usually the exec) are acknowledged, the
// reason is written to stderr and a non-zero exit status is sent.
func failClient(ch ssh.Channel, requests <-chan *ssh.Request, msg string) {
	drained := false
	for !drained {
		select {
		case req, ok := <-requests:
			if !ok {
				drained = true
			} else if req.WantReply {
				_ = req.Reply(true, nil)
			}
		default:
			drained = true
		}
	}
	_, _ = fmt.Fprintf(ch.Stderr(), "nix-remote-build-controller: %s\r\n", msg)
	_, _ = ch.SendRequest("exit-status", false, ssh.Marshal(exitStatusMsg{Status: 255}))
	_ = ch.CloseWrite()
}
