// Package controller reconciles NixBuildRequest objects into builder pods.
//
// Each NixBuildRequest corresponds to exactly one SSH build session and one
// builder pod. Reconciliation is stateless and non-blocking: every step reads
// the current state, performs at most one API write and requeues. Multiple
// requests are therefore reconciled concurrently up to MaxConcurrentReconciles
// workers, and a controller restart resumes from the persisted status.
package controller

import (
	"context"
	"fmt"
	"time"

	"github.com/rs/zerolog"
	"github.com/rs/zerolog/log"
	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/util/intstr"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
	"sigs.k8s.io/controller-runtime/pkg/recorder"

	nixv1alpha1 "github.com/omarjatoi/nix-remote-build-controller/pkg/apis/nixbuilder/v1alpha1"
)

const (
	// FinalizerName guards builder pod cleanup.
	FinalizerName = "nix.io/cleanup"

	// LabelApp, LabelSessionID and LabelBuildRequest are set on builder pods.
	LabelApp          = "app"
	LabelAppValue     = "nix-builder"
	LabelSessionID    = "nix.io/session-id"
	LabelBuildRequest = "nix.io/build-request"

	// BuilderHostKeyMountPath is where an optional shared builder host key is
	// mounted; the builder entrypoint uses it instead of generating one.
	BuilderHostKeyMountPath = "/etc/ssh/ssh_host_ed25519_key"
	// BuilderHostKeySecretKey is the secret entry holding that host key.
	BuilderHostKeySecretKey = "builder-host-key"
	// AuthorizedKeysSecretKey is the secret entry mounted as authorized_keys.
	AuthorizedKeysSecretKey = "public"

	DefaultBuildTimeoutSeconds int64 = 3600
	// DefaultCompletedTTL is how long finished requests are kept before the
	// controller deletes them itself (normally the proxy deletes them first).
	DefaultCompletedTTL = 10 * time.Minute
	// DefaultMaxConcurrentReconciles is the default number of reconcile workers.
	DefaultMaxConcurrentReconciles = 10
	// DefaultPodTerminationGracePeriodSeconds gives sshd a moment to close sessions.
	DefaultPodTerminationGracePeriodSeconds int64 = 10

	requeueCreating = 2 * time.Second
	requeueRunning  = 30 * time.Second
	requeueConflict = 100 * time.Millisecond
)

// NixBuildRequestReconciler reconciles NixBuildRequest objects.
type NixBuildRequestReconciler struct {
	client.Client
	Scheme   *runtime.Scheme
	Recorder recorder.EventRecorder

	// BuilderImage is the default builder image; NixBuildRequest.spec.image overrides it.
	BuilderImage string
	// BuilderImagePullPolicy applies to builder pods. Empty uses the Kubernetes default.
	BuilderImagePullPolicy corev1.PullPolicy
	// RemotePort is the SSH port builder pods listen on.
	RemotePort int32
	// NixConfigMap, if set, is mounted at /etc/nix in builder pods.
	NixConfigMap string
	// SSHKeySecret provides authorized_keys (and optionally the host key) for builder pods.
	SSHKeySecret string
	// MountBuilderHostKey mounts SSHKeySecret's "builder-host-key" entry into pods.
	MountBuilderHostKey bool
	// DefaultResources is applied when NixBuildRequest.spec.resources is empty.
	DefaultResources corev1.ResourceRequirements
	// CompletedTTL bounds how long Completed/Failed requests linger.
	CompletedTTL time.Duration
	// MaxConcurrentReconciles is the number of reconcile workers.
	MaxConcurrentReconciles int
	// BuilderServiceAccount, if set, is used by builder pods.
	BuilderServiceAccount string
}

// Reconcile drives a NixBuildRequest through Pending -> Creating -> Running and
// cleans up its pod on deletion.
func (r *NixBuildRequestReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	var buildReq nixv1alpha1.NixBuildRequest
	if err := r.Get(ctx, req.NamespacedName, &buildReq); err != nil {
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}
	logger := log.With().
		Str("build_request", buildReq.Name).
		Str("session_id", buildReq.Spec.SessionID).
		Str("phase", string(buildReq.Status.Phase)).
		Logger()

	if !buildReq.DeletionTimestamp.IsZero() {
		return r.handleDeletion(ctx, logger, &buildReq)
	}

	if !controllerutil.ContainsFinalizer(&buildReq, FinalizerName) {
		controllerutil.AddFinalizer(&buildReq, FinalizerName)
		if err := r.Update(ctx, &buildReq); err != nil {
			return r.retryOnConflict(logger, "add finalizer", err)
		}
		// The update event re-triggers reconciliation.
		return ctrl.Result{}, nil
	}

	switch buildReq.Status.Phase {
	case "", nixv1alpha1.BuildPhasePending:
		return r.handlePending(ctx, logger, &buildReq)
	case nixv1alpha1.BuildPhaseCreating:
		return r.handleCreating(ctx, logger, &buildReq)
	case nixv1alpha1.BuildPhaseRunning:
		return r.handleRunning(ctx, logger, &buildReq)
	case nixv1alpha1.BuildPhaseCompleted, nixv1alpha1.BuildPhaseFailed:
		return r.handleCompleted(ctx, logger, &buildReq)
	default:
		logger.Warn().Msg("Unknown build phase, ignoring")
		return ctrl.Result{}, nil
	}
}

func (r *NixBuildRequestReconciler) handleDeletion(ctx context.Context, logger zerolog.Logger, buildReq *nixv1alpha1.NixBuildRequest) (ctrl.Result, error) {
	if !controllerutil.ContainsFinalizer(buildReq, FinalizerName) {
		return ctrl.Result{}, nil
	}
	if err := r.deleteBuilderPod(ctx, logger, buildReq); err != nil {
		logger.Error().Err(err).Msg("Failed to delete builder pod during cleanup")
		return ctrl.Result{}, err
	}
	controllerutil.RemoveFinalizer(buildReq, FinalizerName)
	if err := r.Update(ctx, buildReq); err != nil {
		return r.retryOnConflict(logger, "remove finalizer", err)
	}
	logger.Info().Msg("NixBuildRequest cleaned up")
	return ctrl.Result{}, nil
}

func (r *NixBuildRequestReconciler) handlePending(ctx context.Context, logger zerolog.Logger, buildReq *nixv1alpha1.NixBuildRequest) (ctrl.Result, error) {
	pod := r.BuilderPod(buildReq)
	if err := r.Create(ctx, pod); err != nil {
		if !apierrors.IsAlreadyExists(err) {
			logger.Error().Err(err).Msg("Failed to create builder pod")
			r.event(buildReq, corev1.EventTypeWarning, "PodCreateFailed", "CreatePod", err.Error())
			// Surface the reason to the proxy without failing yet; API errors
			// are usually transient (quota, admission webhooks).
			buildReq.Status.Message = "Failed to create builder pod: " + err.Error()
			if _, uerr := r.updateStatus(ctx, logger, buildReq); uerr != nil {
				return ctrl.Result{}, uerr
			}
			return ctrl.Result{}, err
		}
		logger.Info().Str("pod", pod.Name).Msg("Builder pod already exists, adopting it")
	} else {
		logger.Info().Str("pod", pod.Name).Msg("Created builder pod")
		r.event(buildReq, corev1.EventTypeNormal, "PodCreated", "CreatePod", "Created builder pod "+pod.Name)
	}

	now := metav1.Now()
	buildReq.Status.Phase = nixv1alpha1.BuildPhaseCreating
	buildReq.Status.PodName = pod.Name
	buildReq.Status.StartTime = &now
	buildReq.Status.Message = "Builder pod created, waiting for it to become ready"
	return r.updateStatus(ctx, logger, buildReq, ctrl.Result{RequeueAfter: requeueCreating})
}

func (r *NixBuildRequestReconciler) handleCreating(ctx context.Context, logger zerolog.Logger, buildReq *nixv1alpha1.NixBuildRequest) (ctrl.Result, error) {
	pod, err := r.getBuilderPod(ctx, buildReq)
	if err != nil {
		return ctrl.Result{}, err
	}
	if pod == nil {
		return r.fail(ctx, logger, buildReq, "PodMissing", "Builder pod was deleted before it became ready")
	}

	switch pod.Status.Phase {
	case corev1.PodFailed:
		return r.fail(ctx, logger, buildReq, "PodFailed", "Builder pod failed before it became ready: "+podFailureReason(pod))
	case corev1.PodSucceeded:
		return r.fail(ctx, logger, buildReq, "PodExited", "Builder pod exited before it became ready")
	case corev1.PodRunning:
		if pod.Status.PodIP != "" && isPodReady(pod) {
			buildReq.Status.Phase = nixv1alpha1.BuildPhaseRunning
			buildReq.Status.PodIP = pod.Status.PodIP
			buildReq.Status.Message = "Builder pod ready for connections"
			logger.Info().Str("pod", pod.Name).Str("pod_ip", pod.Status.PodIP).Msg("Builder pod ready")
			r.event(buildReq, corev1.EventTypeNormal, "PodReady", "AwaitReady", "Builder pod "+pod.Name+" is ready")
			return r.updateStatus(ctx, logger, buildReq, ctrl.Result{RequeueAfter: requeueRunning})
		}
	}

	// Still starting: surface scheduling / image pull problems to the proxy logs.
	if msg := podPendingReason(pod); msg != "" && msg != buildReq.Status.Message {
		buildReq.Status.Message = msg
		return r.updateStatus(ctx, logger, buildReq, ctrl.Result{RequeueAfter: requeueCreating})
	}
	return ctrl.Result{RequeueAfter: requeueCreating}, nil
}

func (r *NixBuildRequestReconciler) handleRunning(ctx context.Context, logger zerolog.Logger, buildReq *nixv1alpha1.NixBuildRequest) (ctrl.Result, error) {
	pod, err := r.getBuilderPod(ctx, buildReq)
	if err != nil {
		return ctrl.Result{}, err
	}
	if pod == nil {
		return r.fail(ctx, logger, buildReq, "PodMissing", "Builder pod was deleted while running")
	}

	switch pod.Status.Phase {
	case corev1.PodFailed:
		return r.fail(ctx, logger, buildReq, "PodFailed", "Builder pod failed while running: "+podFailureReason(pod))
	case corev1.PodSucceeded:
		buildReq.Status.Phase = nixv1alpha1.BuildPhaseCompleted
		buildReq.Status.CompletionTime = &metav1.Time{Time: time.Now()}
		buildReq.Status.Message = "Builder pod exited"
		return r.updateStatus(ctx, logger, buildReq, ctrl.Result{RequeueAfter: r.completedTTL()})
	}
	return ctrl.Result{RequeueAfter: requeueRunning}, nil
}

// handleCompleted reaps finished requests that the proxy did not delete (for
// example because the proxy crashed) once their TTL has elapsed.
func (r *NixBuildRequestReconciler) handleCompleted(ctx context.Context, logger zerolog.Logger, buildReq *nixv1alpha1.NixBuildRequest) (ctrl.Result, error) {
	finished := time.Now()
	if buildReq.Status.CompletionTime != nil {
		finished = buildReq.Status.CompletionTime.Time
	}
	remaining := time.Until(finished.Add(r.completedTTL()))
	if remaining > 0 {
		return ctrl.Result{RequeueAfter: remaining}, nil
	}
	logger.Info().Dur("ttl", r.completedTTL()).Msg("Deleting finished NixBuildRequest after TTL")
	if err := r.Delete(ctx, buildReq); err != nil && !apierrors.IsNotFound(err) {
		return ctrl.Result{}, err
	}
	return ctrl.Result{}, nil
}

func (r *NixBuildRequestReconciler) fail(ctx context.Context, logger zerolog.Logger, buildReq *nixv1alpha1.NixBuildRequest, reason, message string) (ctrl.Result, error) {
	logger.Warn().Str("reason", reason).Msg(message)
	r.event(buildReq, corev1.EventTypeWarning, reason, "Reconcile", message)
	buildReq.Status.Phase = nixv1alpha1.BuildPhaseFailed
	buildReq.Status.CompletionTime = &metav1.Time{Time: time.Now()}
	buildReq.Status.Message = message
	return r.updateStatus(ctx, logger, buildReq, ctrl.Result{RequeueAfter: r.completedTTL()})
}

// updateStatus writes the status subresource. Conflicts (for example with the
// proxy's final status patch) are not errors: the update event requeues us.
func (r *NixBuildRequestReconciler) updateStatus(ctx context.Context, logger zerolog.Logger, buildReq *nixv1alpha1.NixBuildRequest, onSuccess ...ctrl.Result) (ctrl.Result, error) {
	if err := r.Status().Update(ctx, buildReq); err != nil {
		return r.retryOnConflict(logger, "update status", err)
	}
	if len(onSuccess) > 0 {
		return onSuccess[0], nil
	}
	return ctrl.Result{}, nil
}

func (r *NixBuildRequestReconciler) retryOnConflict(logger zerolog.Logger, op string, err error) (ctrl.Result, error) {
	if apierrors.IsConflict(err) {
		// The object changed under us (for example the proxy's final status
		// patch). Requeue promptly with a fresh read; this is expected, not an
		// error, so it is not surfaced to the controller's error handler.
		logger.Debug().Str("op", op).Msg("Conflict, requeuing")
		return ctrl.Result{RequeueAfter: requeueConflict}, nil
	}
	if apierrors.IsNotFound(err) {
		return ctrl.Result{}, nil
	}
	logger.Error().Err(err).Str("op", op).Msg("API request failed")
	return ctrl.Result{}, err
}

func (r *NixBuildRequestReconciler) getBuilderPod(ctx context.Context, buildReq *nixv1alpha1.NixBuildRequest) (*corev1.Pod, error) {
	if buildReq.Status.PodName == "" {
		return nil, nil
	}
	var pod corev1.Pod
	err := r.Get(ctx, client.ObjectKey{Namespace: buildReq.Namespace, Name: buildReq.Status.PodName}, &pod)
	if apierrors.IsNotFound(err) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return &pod, nil
}

func (r *NixBuildRequestReconciler) deleteBuilderPod(ctx context.Context, logger zerolog.Logger, buildReq *nixv1alpha1.NixBuildRequest) error {
	podName := buildReq.Status.PodName
	if podName == "" {
		// The pod name is deterministic, so a request deleted before its status
		// was written can still have a pod.
		podName = BuilderPodName(buildReq)
	}
	pod := &corev1.Pod{ObjectMeta: metav1.ObjectMeta{Namespace: buildReq.Namespace, Name: podName}}
	if err := r.Delete(ctx, pod); err != nil {
		if apierrors.IsNotFound(err) {
			return nil
		}
		return err
	}
	logger.Info().Str("pod", podName).Msg("Deleted builder pod")
	return nil
}

// event records a Kubernetes event on obj using the events/v1 API. action is the
// operation being performed (a short verb), reason is a machine-readable cause.
func (r *NixBuildRequestReconciler) event(obj runtime.Object, eventType, reason, action, message string) {
	if r.Recorder != nil {
		r.Recorder.Eventf(obj, nil, eventType, reason, action, "%s", message)
	}
}

func (r *NixBuildRequestReconciler) completedTTL() time.Duration {
	if r.CompletedTTL > 0 {
		return r.CompletedTTL
	}
	return DefaultCompletedTTL
}

// BuilderPodName returns the deterministic pod name for a request.
func BuilderPodName(buildReq *nixv1alpha1.NixBuildRequest) string {
	return "nix-builder-" + buildReq.Spec.SessionID
}

// BuilderPod renders the pod for a request.
func (r *NixBuildRequestReconciler) BuilderPod(buildReq *nixv1alpha1.NixBuildRequest) *corev1.Pod {
	// activeDeadlineSeconds must be a positive integer; Kubernetes rejects 0.
	// A nil spec value falls back to the default; a value of zero or less is
	// treated as "no deadline" (nil pointer).
	var activeDeadline *int64
	switch {
	case buildReq.Spec.TimeoutSeconds == nil:
		d := DefaultBuildTimeoutSeconds
		activeDeadline = &d
	case *buildReq.Spec.TimeoutSeconds > 0:
		d := *buildReq.Spec.TimeoutSeconds
		activeDeadline = &d
	}
	resources := buildReq.Spec.Resources
	if len(resources.Requests) == 0 && len(resources.Limits) == 0 {
		resources = *r.DefaultResources.DeepCopy()
	}
	grace := DefaultPodTerminationGracePeriodSeconds
	readOnly := true
	authorizedKeysMode := int32(0o644)

	pod := &corev1.Pod{
		ObjectMeta: metav1.ObjectMeta{
			Name:      BuilderPodName(buildReq),
			Namespace: buildReq.Namespace,
			Labels: map[string]string{
				LabelApp:          LabelAppValue,
				LabelSessionID:    buildReq.Spec.SessionID,
				LabelBuildRequest: buildReq.Name,
			},
			OwnerReferences: []metav1.OwnerReference{*metav1.NewControllerRef(buildReq, nixv1alpha1.GroupVersion.WithKind("NixBuildRequest"))},
		},
		Spec: corev1.PodSpec{
			RestartPolicy:                 corev1.RestartPolicyNever,
			ActiveDeadlineSeconds:         activeDeadline,
			TerminationGracePeriodSeconds: &grace,
			NodeSelector:                  buildReq.Spec.NodeSelector,
			ServiceAccountName:            r.BuilderServiceAccount,
			Containers: []corev1.Container{{
				Name:            "nix-builder",
				Image:           r.builderImage(buildReq),
				ImagePullPolicy: r.BuilderImagePullPolicy,
				Ports: []corev1.ContainerPort{{
					Name:          "ssh",
					ContainerPort: r.RemotePort,
					Protocol:      corev1.ProtocolTCP,
				}},
				Resources: resources,
				ReadinessProbe: &corev1.Probe{
					ProbeHandler: corev1.ProbeHandler{
						TCPSocket: &corev1.TCPSocketAction{Port: intstr.FromInt32(r.RemotePort)},
					},
					InitialDelaySeconds: 1,
					PeriodSeconds:       1,
					FailureThreshold:    60,
				},
				VolumeMounts: []corev1.VolumeMount{{
					Name:      "ssh-keys",
					MountPath: "/home/nixbld/.ssh/authorized_keys",
					SubPath:   AuthorizedKeysSecretKey,
					ReadOnly:  readOnly,
				}},
			}},
			Volumes: []corev1.Volume{{
				Name: "ssh-keys",
				VolumeSource: corev1.VolumeSource{
					Secret: &corev1.SecretVolumeSource{
						SecretName:  r.SSHKeySecret,
						DefaultMode: &authorizedKeysMode,
					},
				},
			}},
		},
	}

	if r.MountBuilderHostKey {
		hostKeyMode := int32(0o600)
		pod.Spec.Volumes = append(pod.Spec.Volumes, corev1.Volume{
			Name: "ssh-host-key",
			VolumeSource: corev1.VolumeSource{
				Secret: &corev1.SecretVolumeSource{
					SecretName:  r.SSHKeySecret,
					DefaultMode: &hostKeyMode,
					Items:       []corev1.KeyToPath{{Key: BuilderHostKeySecretKey, Path: "ssh_host_ed25519_key"}},
				},
			},
		})
		pod.Spec.Containers[0].VolumeMounts = append(pod.Spec.Containers[0].VolumeMounts, corev1.VolumeMount{
			Name:      "ssh-host-key",
			MountPath: BuilderHostKeyMountPath,
			SubPath:   "ssh_host_ed25519_key",
			ReadOnly:  readOnly,
		})
	}

	if r.NixConfigMap != "" {
		pod.Spec.Volumes = append(pod.Spec.Volumes, corev1.Volume{
			Name: "nix-config",
			VolumeSource: corev1.VolumeSource{
				ConfigMap: &corev1.ConfigMapVolumeSource{
					LocalObjectReference: corev1.LocalObjectReference{Name: r.NixConfigMap},
				},
			},
		})
		pod.Spec.Containers[0].VolumeMounts = append(pod.Spec.Containers[0].VolumeMounts, corev1.VolumeMount{
			Name:      "nix-config",
			MountPath: "/etc/nix",
			ReadOnly:  readOnly,
		})
	}

	return pod
}

func (r *NixBuildRequestReconciler) builderImage(buildReq *nixv1alpha1.NixBuildRequest) string {
	if buildReq.Spec.Image != "" {
		return buildReq.Spec.Image
	}
	return r.BuilderImage
}

// isPodReady reports whether all containers in the pod are ready.
func isPodReady(pod *corev1.Pod) bool {
	for _, cond := range pod.Status.Conditions {
		if cond.Type == corev1.ContainersReady && cond.Status == corev1.ConditionTrue {
			return true
		}
	}
	return false
}

// podPendingReason summarises why a pod has not become ready yet.
func podPendingReason(pod *corev1.Pod) string {
	for _, cond := range pod.Status.Conditions {
		if cond.Type == corev1.PodScheduled && cond.Status == corev1.ConditionFalse {
			return fmt.Sprintf("Builder pod not scheduled: %s: %s", cond.Reason, cond.Message)
		}
	}
	for _, cs := range pod.Status.ContainerStatuses {
		if cs.State.Waiting != nil && cs.State.Waiting.Reason != "" && cs.State.Waiting.Reason != "ContainerCreating" {
			return fmt.Sprintf("Builder pod waiting: %s: %s", cs.State.Waiting.Reason, cs.State.Waiting.Message)
		}
	}
	return ""
}

// podFailureReason summarises why a pod failed.
func podFailureReason(pod *corev1.Pod) string {
	if pod.Status.Reason != "" {
		if pod.Status.Message != "" {
			return pod.Status.Reason + ": " + pod.Status.Message
		}
		return pod.Status.Reason
	}
	for _, cs := range pod.Status.ContainerStatuses {
		if cs.State.Terminated != nil {
			return fmt.Sprintf("container %s exited with code %d (%s)", cs.Name, cs.State.Terminated.ExitCode, cs.State.Terminated.Reason)
		}
	}
	if pod.Status.Message != "" {
		return pod.Status.Message
	}
	return "unknown"
}

// SetupWithManager registers the reconciler.
func (r *NixBuildRequestReconciler) SetupWithManager(mgr ctrl.Manager) error {
	workers := r.MaxConcurrentReconciles
	if workers <= 0 {
		workers = DefaultMaxConcurrentReconciles
	}
	return ctrl.NewControllerManagedBy(mgr).
		Named("nixbuildrequest").
		For(&nixv1alpha1.NixBuildRequest{}).
		Owns(&corev1.Pod{}).
		WithOptions(controller.Options{MaxConcurrentReconciles: workers}).
		Complete(r)
}
