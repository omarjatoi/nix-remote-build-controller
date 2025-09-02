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
	SSHPort      int32
	NixConfigMap string
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
	if err := r.Get(ctx, client.ObjectKey{
		Namespace: buildReq.Namespace,
		Name:      buildReq.Status.PodName,
	}, &pod); err != nil {
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	if pod.Status.Phase == corev1.PodSucceeded {
		buildReq.Status.Phase = nixv1alpha1.BuildPhaseCompleted
		buildReq.Status.CompletionTime = &metav1.Time{Time: time.Now()}
		buildReq.Status.Message = "Build completed successfully"
		return r.updateStatus(ctx, buildReq)
	}

	if pod.Status.Phase == corev1.PodFailed {
		buildReq.Status.Phase = nixv1alpha1.BuildPhaseFailed
		buildReq.Status.CompletionTime = &metav1.Time{Time: time.Now()}
		buildReq.Status.Message = "Build failed"
		return r.updateStatus(ctx, buildReq)
	}

	return ctrl.Result{RequeueAfter: time.Second * 10}, nil
}

func (r *NixBuildRequestReconciler) handleCompletedBuild(ctx context.Context, buildReq *nixv1alpha1.NixBuildRequest) (ctrl.Result, error) {
	if time.Since(buildReq.Status.CompletionTime.Time) > time.Minute*5 {
		var pod corev1.Pod
		if err := r.Get(ctx, client.ObjectKey{
			Namespace: buildReq.Namespace,
			Name:      buildReq.Status.PodName,
		}, &pod); err == nil {
			if err := r.Delete(ctx, &pod); err != nil {
				log.Error().Err(err).Str("pod_name", buildReq.Status.PodName).Msg("Failed to delete completed pod")
			} else {
				log.Info().Str("pod_name", buildReq.Status.PodName).Msg("Cleaned up completed pod")
			}
		}
	}

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
			Containers: []corev1.Container{{
				Name:    "nix-builder",
				Image:   r.BuilderImage,
				Command: []string{"/usr/sbin/sshd", "-D", "-e"},
				Ports: []corev1.ContainerPort{{
					ContainerPort: r.SSHPort,
					Protocol:      corev1.ProtocolTCP,
				}},
				Resources: buildReq.Spec.Resources,
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

func (r *NixBuildRequestReconciler) updateStatus(ctx context.Context, buildReq *nixv1alpha1.NixBuildRequest) (ctrl.Result, error) {
	if err := r.Status().Update(ctx, buildReq); err != nil {
		return ctrl.Result{}, err
	}
	return ctrl.Result{}, nil
}

// SetupWithManager sets up the controller with the Manager
func (r *NixBuildRequestReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&nixv1alpha1.NixBuildRequest{}).
		Owns(&corev1.Pod{}).
		Complete(r)
}
