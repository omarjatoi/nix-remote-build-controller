package controller

import (
	"context"
	"fmt"
	"time"

	"github.com/rs/zerolog/log"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"

	nixv1alpha1 "github.com/omarjatoi/nix-remote-build-controller/pkg/apis/nixbuilder/v1alpha1"
)

// NixBuildRequestReconciler reconciles NixBuildRequest objects
type NixBuildRequestReconciler struct {
	client.Client
	Scheme       *runtime.Scheme
	BuilderImage string
	RemotePort   int32
	NixConfigMap string
	SSHKeySecret string
}

// Reconcile handles NixBuildRequest events
func (r *NixBuildRequestReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	// Check for shutdown early
	select {
	case <-ctx.Done():
		log.Info().Str("build_request", req.Name).Msg("Reconciliation cancelled due to shutdown")
		return ctrl.Result{}, ctx.Err()
	default:
	}

	var buildReq nixv1alpha1.NixBuildRequest
	if err := r.Get(ctx, req.NamespacedName, &buildReq); err != nil {
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	// Add finalizer for new resources to ensure cleanup
	if buildReq.DeletionTimestamp.IsZero() && !controllerutil.ContainsFinalizer(&buildReq, "nix.io/cleanup") {
		controllerutil.AddFinalizer(&buildReq, "nix.io/cleanup")
		if err := r.Update(ctx, &buildReq); err != nil {
			return ctrl.Result{}, err
		}
		return ctrl.Result{Requeue: true}, nil
	}

	// Handle deletion with finalizer
	if !buildReq.DeletionTimestamp.IsZero() {
		if err := r.cleanup(ctx, &buildReq); err != nil {
			log.Error().Err(err).Str("session_id", buildReq.Spec.SessionID).Msg("Failed to cleanup build request")
			return ctrl.Result{RequeueAfter: time.Second * 10}, err
		}
		controllerutil.RemoveFinalizer(&buildReq, "nix.io/cleanup")
		return ctrl.Result{}, r.Update(ctx, &buildReq)
	}

	log.Info().Str("session_id", buildReq.Spec.SessionID).Str("phase", string(buildReq.Status.Phase)).Msg("Reconciling NixBuildRequest")

	switch buildReq.Status.Phase {
	case "", nixv1alpha1.BuildPhasePending:
		return r.handlePendingBuild(ctx, &buildReq)
	case nixv1alpha1.BuildPhaseCreating:
		return r.handleCreatingBuild(ctx, &buildReq)
	case nixv1alpha1.BuildPhaseRunning:
		return r.handleRunningBuild(ctx, &buildReq)
	case nixv1alpha1.BuildPhaseCompleted, nixv1alpha1.BuildPhaseFailed:
		return r.handleCompletedBuild(ctx, &buildReq)
	default:
		log.Info().Str("phase", string(buildReq.Status.Phase)).Msg("Unknown build phase")
		return ctrl.Result{}, nil
	}
}

func (r *NixBuildRequestReconciler) handlePendingBuild(ctx context.Context, buildReq *nixv1alpha1.NixBuildRequest) (ctrl.Result, error) {
	log.Info().Str("session_id", buildReq.Spec.SessionID).Msg("Creating builder pod")

	pod := r.createBuilderPod(buildReq)
	if err := r.Create(ctx, pod); err != nil {
		log.Error().Err(err).Str("session_id", buildReq.Spec.SessionID).Msg("Failed to create builder pod")
		return ctrl.Result{}, err
	}

	buildReq.Status.Phase = nixv1alpha1.BuildPhaseCreating
	buildReq.Status.PodName = pod.Name
	buildReq.Status.StartTime = &metav1.Time{Time: time.Now()}
	buildReq.Status.Message = "Builder pod created"

	if err := r.Status().Update(ctx, buildReq); err != nil {
		log.Error().Err(err).Str("session_id", buildReq.Spec.SessionID).Msg("Failed to update build request status")
		return ctrl.Result{}, err
	}

	return ctrl.Result{RequeueAfter: time.Second * 5}, nil
}

func (r *NixBuildRequestReconciler) handleCreatingBuild(ctx context.Context, buildReq *nixv1alpha1.NixBuildRequest) (ctrl.Result, error) {
	var pod corev1.Pod
	if err := r.Get(ctx, client.ObjectKey{
		Namespace: buildReq.Namespace,
		Name:      buildReq.Status.PodName,
	}, &pod); err != nil {
		log.Error().Err(err).Str("session_id", buildReq.Spec.SessionID).Msg("Failed to get builder pod")
		return ctrl.Result{}, err
	}

	if pod.Status.Phase == corev1.PodRunning && pod.Status.PodIP != "" {
		buildReq.Status.Phase = nixv1alpha1.BuildPhaseRunning
		buildReq.Status.PodIP = pod.Status.PodIP
		buildReq.Status.Message = "Builder pod ready for connections"

		if err := r.Status().Update(ctx, buildReq); err != nil {
			log.Error().Err(err).Str("session_id", buildReq.Spec.SessionID).Msg("Failed to update build request status")
			return ctrl.Result{}, err
		}

		log.Info().Str("session_id", buildReq.Spec.SessionID).Str("pod_ip", pod.Status.PodIP).Msg("Builder pod ready")
		return ctrl.Result{}, nil
	}

	return ctrl.Result{RequeueAfter: time.Second * 2}, nil
}

func (r *NixBuildRequestReconciler) handleRunningBuild(ctx context.Context, buildReq *nixv1alpha1.NixBuildRequest) (ctrl.Result, error) {
	var pod corev1.Pod
	err := r.Get(ctx, client.ObjectKey{
		Namespace: buildReq.Namespace,
		Name:      buildReq.Status.PodName,
	}, &pod)

	if err != nil {
		if client.IgnoreNotFound(err) == nil {
			buildReq.Status.Phase = nixv1alpha1.BuildPhaseFailed
			buildReq.Status.CompletionTime = &metav1.Time{Time: time.Now()}
			buildReq.Status.Message = "Builder pod was deleted unexpectedly"
			return r.updateStatus(ctx, buildReq)
		}
		return ctrl.Result{}, err
	}

	if pod.Status.Phase == corev1.PodFailed {
		buildReq.Status.Phase = nixv1alpha1.BuildPhaseFailed
		buildReq.Status.CompletionTime = &metav1.Time{Time: time.Now()}
		buildReq.Status.Message = fmt.Sprintf("Builder pod failed unexpectedly: %s", pod.Status.Message)
		return r.updateStatus(ctx, buildReq)
	}

	return ctrl.Result{RequeueAfter: time.Second * 30}, nil
}

func (r *NixBuildRequestReconciler) handleCompletedBuild(ctx context.Context, buildReq *nixv1alpha1.NixBuildRequest) (ctrl.Result, error) {
	log.Debug().
		Str("session_id", buildReq.Spec.SessionID).
		Str("phase", string(buildReq.Status.Phase)).
		Msg("Build completed, awaiting cleanup via deletion")
	return ctrl.Result{}, nil
}

func (r *NixBuildRequestReconciler) createBuilderPod(buildReq *nixv1alpha1.NixBuildRequest) *corev1.Pod {
	podName := fmt.Sprintf("nix-builder-%s", buildReq.Spec.SessionID)

	pod := &corev1.Pod{
		ObjectMeta: metav1.ObjectMeta{
			Name:      podName,
			Namespace: buildReq.Namespace,
			Labels: map[string]string{
				"app":                  "nix-builder",
				"nix.io/session-id":    buildReq.Spec.SessionID,
				"nix.io/build-request": buildReq.Name,
			},
			OwnerReferences: []metav1.OwnerReference{{
				APIVersion:         buildReq.APIVersion,
				Kind:               buildReq.Kind,
				Name:               buildReq.Name,
				UID:                buildReq.UID,
				Controller:         &[]bool{true}[0],
				BlockOwnerDeletion: &[]bool{true}[0],
			}},
		},
		Spec: corev1.PodSpec{
			RestartPolicy:         corev1.RestartPolicyNever,
			ActiveDeadlineSeconds: buildReq.Spec.TimeoutSeconds,
			NodeSelector:          buildReq.Spec.NodeSelector,
			Containers: []corev1.Container{{
				Name:    "nix-builder",
				Image:   r.getBuilderImage(buildReq),
				Command: []string{"/bin/entrypoint.sh"},
				Ports: []corev1.ContainerPort{{
					ContainerPort: r.RemotePort,
					Protocol:      corev1.ProtocolTCP,
				}},
				Resources: buildReq.Spec.Resources,
				VolumeMounts: []corev1.VolumeMount{{
					Name:      "ssh-keys",
					MountPath: "/home/nixbld/.ssh/authorized_keys",
					SubPath:   "public",
					ReadOnly:  true,
				}},
			}},
			Volumes: []corev1.Volume{{
				Name: "ssh-keys",
				VolumeSource: corev1.VolumeSource{
					Secret: &corev1.SecretVolumeSource{
						SecretName:  r.SSHKeySecret,
						DefaultMode: &[]int32{0644}[0],
					},
				},
			}},
		},
	}

	if r.NixConfigMap != "" {
		pod.Spec.Volumes = append(pod.Spec.Volumes, corev1.Volume{
			Name: "nix-config",
			VolumeSource: corev1.VolumeSource{
				ConfigMap: &corev1.ConfigMapVolumeSource{
					LocalObjectReference: corev1.LocalObjectReference{
						Name: r.NixConfigMap,
					},
				},
			},
		})

		pod.Spec.Containers[0].VolumeMounts = append(pod.Spec.Containers[0].VolumeMounts, corev1.VolumeMount{
			Name:      "nix-config",
			MountPath: "/etc/nix",
			ReadOnly:  true,
		})
	}

	return pod
}

func (r *NixBuildRequestReconciler) getBuilderImage(buildReq *nixv1alpha1.NixBuildRequest) string {
	if buildReq.Spec.Image != "" {
		return buildReq.Spec.Image
	}
	return r.BuilderImage
}

func (r *NixBuildRequestReconciler) updateStatus(ctx context.Context, buildReq *nixv1alpha1.NixBuildRequest) (ctrl.Result, error) {
	if err := r.Status().Update(ctx, buildReq); err != nil {
		return ctrl.Result{}, err
	}
	return ctrl.Result{}, nil
}

func (r *NixBuildRequestReconciler) cleanup(ctx context.Context, buildReq *nixv1alpha1.NixBuildRequest) error {
	log.Info().Str("session_id", buildReq.Spec.SessionID).Msg("Cleaning up build request")

	// Delete associated pod if it exists
	if buildReq.Status.PodName != "" {
		var pod corev1.Pod
		if err := r.Get(ctx, client.ObjectKey{
			Namespace: buildReq.Namespace,
			Name:      buildReq.Status.PodName,
		}, &pod); err == nil {
			if err := r.Delete(ctx, &pod); err != nil {
				log.Error().Err(err).Str("pod_name", buildReq.Status.PodName).Msg("Failed to delete pod during cleanup")
				return err
			}
			log.Info().Str("pod_name", buildReq.Status.PodName).Msg("Deleted pod during cleanup")
		}
	}

	return nil
}

func (r *NixBuildRequestReconciler) GracefulShutdown(ctx context.Context) error {
	log.Info().Msg("Starting graceful controller shutdown")

	// List all pending/creating build requests
	var buildReqs nixv1alpha1.NixBuildRequestList
	if err := r.List(ctx, &buildReqs); err != nil {
		log.Error().Err(err).Msg("Failed to list build requests during shutdown")
		return err
	}

	updatedCount := 0
	for _, buildReq := range buildReqs.Items {
		if buildReq.Status.Phase == nixv1alpha1.BuildPhasePending ||
			buildReq.Status.Phase == nixv1alpha1.BuildPhaseCreating {

			buildReq.Status.Phase = nixv1alpha1.BuildPhaseFailed
			buildReq.Status.Message = "Controller shutdown during processing"
			buildReq.Status.CompletionTime = &metav1.Time{Time: time.Now()}

			if err := r.Status().Update(ctx, &buildReq); err != nil {
				log.Error().Err(err).Str("build_request", buildReq.Name).Msg("Failed to update build request status during shutdown")
			} else {
				updatedCount++
			}
		}
	}

	log.Info().Int("updated_requests", updatedCount).Msg("Completed graceful shutdown cleanup")
	return nil
}

// SetupWithManager sets up the controller with the Manager
func (r *NixBuildRequestReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&nixv1alpha1.NixBuildRequest{}).
		Owns(&corev1.Pod{}).
		Complete(r)
}
