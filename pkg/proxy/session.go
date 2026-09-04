package proxy

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"time"

	"github.com/rs/zerolog"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"

	"github.com/omarjatoi/nix-remote-build-controller/pkg/apis/nixbuilder/v1alpha1"
)

// Labels and annotations set on NixBuildRequests created by the proxy.
const (
	LabelSessionID = "nix.io/session-id"
	LabelManagedBy = "app.kubernetes.io/managed-by"
	ManagedByValue = "nix-remote-build-proxy"
	// AnnotationProxyPod records which proxy pod owns a session.
	AnnotationProxyPod = "nix.io/proxy-pod"
)

// Session is one build session: one SSH session channel, one NixBuildRequest,
// one builder pod.
type Session struct {
	ID        string
	StartTime time.Time
	// Created is true once the NixBuildRequest exists and must be cleaned up.
	Created bool
}

// SessionResult describes how a session ended.
type SessionResult struct {
	// ExitStatus is the exit status reported by the builder-side command, if any.
	ExitStatus *uint32
	// Err is non-nil if the session failed before or during tunnelling.
	Err error
}

// BuildRequestName returns the NixBuildRequest name for a session.
func BuildRequestName(sessionID string) string {
	return "build-" + sessionID
}

func (p *SSHProxy) createBuildRequest(ctx context.Context, session *Session) error {
	buildReq := &v1alpha1.NixBuildRequest{
		ObjectMeta: metav1.ObjectMeta{
			Name:      BuildRequestName(session.ID),
			Namespace: p.cfg.Namespace,
			Labels: map[string]string{
				LabelSessionID: session.ID,
				LabelManagedBy: ManagedByValue,
			},
		},
		Spec: v1alpha1.NixBuildRequestSpec{
			SessionID: session.ID,
		},
	}
	// Only set a whole-second, positive deadline; a sub-second BuildTimeout would
	// truncate to 0, which the controller (and Kubernetes) treat as no deadline.
	if seconds := int64(p.cfg.BuildTimeout / time.Second); seconds > 0 {
		buildReq.Spec.TimeoutSeconds = &seconds
	}
	if p.cfg.OwnerPodName != "" && p.cfg.OwnerPodUID != "" {
		buildReq.Annotations = map[string]string{AnnotationProxyPod: p.cfg.OwnerPodName}
		// If this proxy pod disappears, garbage collection removes its requests
		// and the controller tears down the builder pods.
		buildReq.OwnerReferences = []metav1.OwnerReference{{
			APIVersion: "v1",
			Kind:       "Pod",
			Name:       p.cfg.OwnerPodName,
			UID:        types.UID(p.cfg.OwnerPodUID),
		}}
	}

	if err := p.k8sClient.Create(ctx, buildReq); err != nil {
		return fmt.Errorf("create NixBuildRequest %s: %w", buildReq.Name, err)
	}
	return nil
}

// waitForBuilderPod polls the NixBuildRequest until the controller reports a
// ready pod, the request fails, or the timeout expires.
func (p *SSHProxy) waitForBuilderPod(ctx context.Context, logger zerolog.Logger, session *Session) (string, error) {
	name := BuildRequestName(session.ID)
	deadline := time.NewTimer(p.cfg.PodReadyTimeout)
	defer deadline.Stop()
	ticker := time.NewTicker(p.cfg.PollInterval)
	defer ticker.Stop()

	var lastPhase v1alpha1.BuildPhase
	for {
		select {
		case <-ctx.Done():
			return "", fmt.Errorf("session cancelled while waiting for builder pod: %w", ctx.Err())
		case <-deadline.C:
			return "", fmt.Errorf("timed out after %s waiting for builder pod (last phase %q)", p.cfg.PodReadyTimeout, lastPhase)
		case <-ticker.C:
		}

		var buildReq v1alpha1.NixBuildRequest
		if err := p.k8sClient.Get(ctx, client.ObjectKey{Namespace: p.cfg.Namespace, Name: name}, &buildReq); err != nil {
			if apierrors.IsNotFound(err) {
				return "", errors.New("NixBuildRequest disappeared while waiting for builder pod")
			}
			// Transient API errors: keep polling until the deadline.
			logger.Warn().Err(err).Msg("Failed to read NixBuildRequest, retrying")
			continue
		}

		if buildReq.Status.Phase != lastPhase {
			logger.Info().Str("phase", string(buildReq.Status.Phase)).Str("message", buildReq.Status.Message).Msg("NixBuildRequest phase changed")
			lastPhase = buildReq.Status.Phase
		}

		switch {
		case buildReq.DeletionTimestamp != nil:
			return "", errors.New("NixBuildRequest was deleted before the builder pod became ready")
		case buildReq.Status.Phase == v1alpha1.BuildPhaseFailed:
			if buildReq.Status.Message != "" {
				return "", fmt.Errorf("NixBuildRequest failed: %s", buildReq.Status.Message)
			}
			return "", errors.New("NixBuildRequest failed")
		case buildReq.Status.Phase == v1alpha1.BuildPhaseCompleted:
			return "", errors.New("NixBuildRequest completed before the builder pod became ready")
		case buildReq.Status.Phase == v1alpha1.BuildPhaseRunning && buildReq.Status.PodIP != "":
			logger.Info().Str("pod", buildReq.Status.PodName).Str("pod_ip", buildReq.Status.PodIP).Msg("Builder pod ready")
			return buildReq.Status.PodIP, nil
		}
	}
}

// finalizeBuildRequest records the outcome on the NixBuildRequest and deletes
// it. Deleting the request is what triggers the controller to remove the
// builder pod, so this must run even when the session context is gone; it uses
// its own bounded context.
func (p *SSHProxy) finalizeBuildRequest(logger zerolog.Logger, session *Session, result SessionResult) {
	if !session.Created {
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	name := BuildRequestName(session.ID)
	buildReq := &v1alpha1.NixBuildRequest{
		ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: p.cfg.Namespace},
	}

	// Record a terminal status only when there is something worth recording: a
	// session failure, or a non-zero remote exit status. On a clean success the
	// request is deleted immediately, so an extra status write is wasted work.
	phase := v1alpha1.BuildPhaseCompleted
	message := ""
	switch {
	case result.Err != nil:
		phase = v1alpha1.BuildPhaseFailed
		message = "Build session failed: " + result.Err.Error()
	case result.ExitStatus != nil && *result.ExitStatus != 0:
		message = fmt.Sprintf("Build session completed, remote command exited with status %d", *result.ExitStatus)
	}

	if message != "" {
		status := v1alpha1.NixBuildRequestStatus{
			Phase:          phase,
			Message:        message,
			CompletionTime: &metav1.Time{Time: time.Now()},
		}
		patch, err := json.Marshal(map[string]any{"status": status})
		if err == nil {
			err = p.k8sClient.Status().Patch(ctx, buildReq, client.RawPatch(types.MergePatchType, patch))
		}
		if err != nil && !apierrors.IsNotFound(err) {
			logger.Warn().Err(err).Msg("Failed to record final NixBuildRequest status")
		}
	}

	if err := p.k8sClient.Delete(ctx, buildReq); err != nil && !apierrors.IsNotFound(err) {
		logger.Error().Err(err).Msg("Failed to delete NixBuildRequest; the controller will reap it after its TTL")
		return
	}
	logger.Info().Str("phase", string(phase)).Msg("NixBuildRequest finalized and deleted")
}
