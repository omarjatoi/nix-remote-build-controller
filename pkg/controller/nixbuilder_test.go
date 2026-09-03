package controller

import (
	"context"
	"strings"
	"testing"
	"time"

	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	"k8s.io/client-go/tools/record"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"

	nixv1alpha1 "github.com/omarjatoi/nix-remote-build-controller/pkg/apis/nixbuilder/v1alpha1"
)

const ns = "default"

func newScheme(t *testing.T) *runtime.Scheme {
	t.Helper()
	s := runtime.NewScheme()
	if err := clientgoscheme.AddToScheme(s); err != nil {
		t.Fatal(err)
	}
	if err := nixv1alpha1.AddToScheme(s); err != nil {
		t.Fatal(err)
	}
	return s
}

func newReconciler(t *testing.T, objs ...client.Object) (*NixBuildRequestReconciler, client.Client, *record.FakeRecorder) {
	t.Helper()
	scheme := newScheme(t)
	c := fake.NewClientBuilder().
		WithScheme(scheme).
		WithStatusSubresource(&nixv1alpha1.NixBuildRequest{}).
		WithObjects(objs...).
		Build()
	rec := record.NewFakeRecorder(32)
	r := &NixBuildRequestReconciler{
		Client:                 c,
		Scheme:                 scheme,
		Recorder:               rec,
		BuilderImage:           "ghcr.io/example/builder:v1",
		BuilderImagePullPolicy: corev1.PullIfNotPresent,
		RemotePort:             22,
		NixConfigMap:           "nix-builder-config",
		SSHKeySecret:           "nix-builder-ssh-keys",
		MountBuilderHostKey:    true,
		CompletedTTL:           time.Minute,
		DefaultResources: corev1.ResourceRequirements{
			Requests: corev1.ResourceList{corev1.ResourceCPU: resource.MustParse("1")},
		},
	}
	return r, c, rec
}

func newRequest(session string) *nixv1alpha1.NixBuildRequest {
	return &nixv1alpha1.NixBuildRequest{
		ObjectMeta: metav1.ObjectMeta{Name: "build-" + session, Namespace: ns, UID: types.UID("uid-" + session)},
		Spec:       nixv1alpha1.NixBuildRequestSpec{SessionID: session},
	}
}

func reconcile(t *testing.T, r *NixBuildRequestReconciler, name string) ctrl.Result {
	t.Helper()
	res, err := r.Reconcile(context.Background(), ctrl.Request{NamespacedName: types.NamespacedName{Namespace: ns, Name: name}})
	if err != nil {
		t.Fatalf("reconcile %s: %v", name, err)
	}
	return res
}

func get(t *testing.T, c client.Client, name string) *nixv1alpha1.NixBuildRequest {
	t.Helper()
	var br nixv1alpha1.NixBuildRequest
	if err := c.Get(context.Background(), client.ObjectKey{Namespace: ns, Name: name}, &br); err != nil {
		t.Fatalf("get %s: %v", name, err)
	}
	return &br
}

func getPod(t *testing.T, c client.Client, name string) (*corev1.Pod, bool) {
	t.Helper()
	var pod corev1.Pod
	err := c.Get(context.Background(), client.ObjectKey{Namespace: ns, Name: name}, &pod)
	if client.IgnoreNotFound(err) != nil {
		t.Fatalf("get pod %s: %v", name, err)
	}
	return &pod, err == nil
}

func TestPendingRequestCreatesPodAndMovesToCreating(t *testing.T) {
	br := newRequest("s1")
	r, c, rec := newReconciler(t, br)

	// First pass adds the finalizer only.
	reconcile(t, r, br.Name)
	if got := get(t, c, br.Name); !controllerutil.ContainsFinalizer(got, FinalizerName) {
		t.Fatal("finalizer not added")
	}

	res := reconcile(t, r, br.Name)
	if res.RequeueAfter == 0 {
		t.Error("expected requeue while pod starts")
	}
	got := get(t, c, br.Name)
	if got.Status.Phase != nixv1alpha1.BuildPhaseCreating || got.Status.PodName != "nix-builder-s1" || got.Status.StartTime == nil {
		t.Errorf("status = %+v", got.Status)
	}
	pod, ok := getPod(t, c, "nix-builder-s1")
	if !ok {
		t.Fatal("pod not created")
	}
	if len(pod.OwnerReferences) != 1 || pod.OwnerReferences[0].UID != br.UID || pod.OwnerReferences[0].Controller == nil || !*pod.OwnerReferences[0].Controller {
		t.Errorf("pod owner refs = %+v", pod.OwnerReferences)
	}
	if pod.Spec.Containers[0].Image != "ghcr.io/example/builder:v1" || pod.Spec.Containers[0].ImagePullPolicy != corev1.PullIfNotPresent {
		t.Errorf("container = %+v", pod.Spec.Containers[0])
	}
	if pod.Spec.ActiveDeadlineSeconds == nil || *pod.Spec.ActiveDeadlineSeconds != DefaultBuildTimeoutSeconds {
		t.Errorf("activeDeadlineSeconds = %v", pod.Spec.ActiveDeadlineSeconds)
	}
	if pod.Spec.Containers[0].Resources.Requests.Cpu().String() != "1" {
		t.Errorf("default resources not applied: %+v", pod.Spec.Containers[0].Resources)
	}
	select {
	case ev := <-rec.Events:
		if ev == "" {
			t.Error("empty event")
		}
	default:
		t.Error("expected a PodCreated event")
	}
}

func TestPodSpecHonoursRequestOverrides(t *testing.T) {
	timeout := int64(120)
	br := newRequest("s2")
	br.Spec.Image = "custom/builder:dev"
	br.Spec.TimeoutSeconds = &timeout
	br.Spec.NodeSelector = map[string]string{"kubernetes.io/arch": "arm64"}
	br.Spec.Resources = corev1.ResourceRequirements{Limits: corev1.ResourceList{corev1.ResourceMemory: resource.MustParse("4Gi")}}
	r, _, _ := newReconciler(t)

	pod := r.BuilderPod(br)
	if pod.Spec.Containers[0].Image != "custom/builder:dev" {
		t.Errorf("image = %s", pod.Spec.Containers[0].Image)
	}
	if *pod.Spec.ActiveDeadlineSeconds != 120 {
		t.Errorf("activeDeadlineSeconds = %d", *pod.Spec.ActiveDeadlineSeconds)
	}
	if pod.Spec.NodeSelector["kubernetes.io/arch"] != "arm64" {
		t.Errorf("nodeSelector = %v", pod.Spec.NodeSelector)
	}
	if pod.Spec.Containers[0].Resources.Limits.Memory().String() != "4Gi" || len(pod.Spec.Containers[0].Resources.Requests) != 0 {
		t.Errorf("resources = %+v", pod.Spec.Containers[0].Resources)
	}
	mounts := map[string]string{}
	for _, m := range pod.Spec.Containers[0].VolumeMounts {
		mounts[m.MountPath] = m.SubPath
	}
	if _, ok := mounts["/etc/nix"]; !ok || mounts["/home/nixbld/.ssh/authorized_keys"] != "public" || mounts[BuilderHostKeyMountPath] != "ssh_host_ed25519_key" {
		t.Errorf("mounts = %v", mounts)
	}

	negative := int64(-5)
	br.Spec.TimeoutSeconds = &negative
	if got := *r.BuilderPod(br).Spec.ActiveDeadlineSeconds; got != 0 {
		t.Errorf("negative timeout should clamp to 0, got %d", got)
	}
}

func markReady(pod *corev1.Pod, ip string) {
	pod.Status.Phase = corev1.PodRunning
	pod.Status.PodIP = ip
	pod.Status.Conditions = []corev1.PodCondition{{Type: corev1.ContainersReady, Status: corev1.ConditionTrue}}
}

func TestCreatingMovesToRunningWhenPodReady(t *testing.T) {
	br := newRequest("s3")
	controllerutil.AddFinalizer(br, FinalizerName)
	r, c, _ := newReconciler(t, br)
	reconcile(t, r, br.Name) // Pending -> Creating, pod created

	pod, _ := getPod(t, c, "nix-builder-s3")
	// Not ready yet: stays Creating and surfaces the pending reason.
	pod.Status.Phase = corev1.PodPending
	pod.Status.Conditions = []corev1.PodCondition{{Type: corev1.PodScheduled, Status: corev1.ConditionFalse, Reason: "Unschedulable", Message: "0/1 nodes"}}
	if err := c.Status().Update(context.Background(), pod); err != nil {
		t.Fatal(err)
	}
	reconcile(t, r, br.Name)
	if got := get(t, c, br.Name); got.Status.Phase != nixv1alpha1.BuildPhaseCreating || !strings.HasPrefix(got.Status.Message, "Builder pod not scheduled") {
		t.Errorf("status = %+v", got.Status)
	}

	pod, _ = getPod(t, c, "nix-builder-s3")
	markReady(pod, "10.1.2.3")
	if err := c.Status().Update(context.Background(), pod); err != nil {
		t.Fatal(err)
	}
	reconcile(t, r, br.Name)
	got := get(t, c, br.Name)
	if got.Status.Phase != nixv1alpha1.BuildPhaseRunning || got.Status.PodIP != "10.1.2.3" {
		t.Errorf("status = %+v", got.Status)
	}
}

func TestPodFailureMarksRequestFailed(t *testing.T) {
	br := newRequest("s4")
	controllerutil.AddFinalizer(br, FinalizerName)
	r, c, _ := newReconciler(t, br)
	reconcile(t, r, br.Name)

	pod, _ := getPod(t, c, "nix-builder-s4")
	pod.Status.Phase = corev1.PodFailed
	pod.Status.Reason = "DeadlineExceeded"
	pod.Status.Message = "Pod was active on the node longer than the specified deadline"
	if err := c.Status().Update(context.Background(), pod); err != nil {
		t.Fatal(err)
	}
	reconcile(t, r, br.Name)
	got := get(t, c, br.Name)
	if got.Status.Phase != nixv1alpha1.BuildPhaseFailed || got.Status.CompletionTime == nil {
		t.Errorf("status = %+v", got.Status)
	}
	if !strings.Contains(got.Status.Message, "DeadlineExceeded") {
		t.Errorf("message = %q", got.Status.Message)
	}
}

func TestMissingPodWhileRunningMarksFailed(t *testing.T) {
	br := newRequest("s5")
	controllerutil.AddFinalizer(br, FinalizerName)
	br.Status.Phase = nixv1alpha1.BuildPhaseRunning
	br.Status.PodName = "nix-builder-s5"
	r, c, _ := newReconciler(t, br)
	reconcile(t, r, br.Name)
	if got := get(t, c, br.Name); got.Status.Phase != nixv1alpha1.BuildPhaseFailed {
		t.Errorf("status = %+v", got.Status)
	}
}

func TestDeletionCleansUpPodAndRemovesFinalizer(t *testing.T) {
	br := newRequest("s6")
	controllerutil.AddFinalizer(br, FinalizerName)
	r, c, _ := newReconciler(t, br)
	reconcile(t, r, br.Name)
	if _, ok := getPod(t, c, "nix-builder-s6"); !ok {
		t.Fatal("pod should exist")
	}

	if err := c.Delete(context.Background(), get(t, c, br.Name)); err != nil {
		t.Fatal(err)
	}
	reconcile(t, r, br.Name)
	if _, ok := getPod(t, c, "nix-builder-s6"); ok {
		t.Error("pod should have been deleted")
	}
	var after nixv1alpha1.NixBuildRequest
	if err := c.Get(context.Background(), client.ObjectKey{Namespace: ns, Name: br.Name}, &after); err == nil {
		t.Error("request should be gone once the finalizer is removed")
	}
}

func TestDeletionBeforeStatusStillDeletesDeterministicPod(t *testing.T) {
	br := newRequest("s7")
	controllerutil.AddFinalizer(br, FinalizerName)
	now := metav1.Now()
	br.DeletionTimestamp = &now
	pod := &corev1.Pod{ObjectMeta: metav1.ObjectMeta{Name: "nix-builder-s7", Namespace: ns}}
	r, c, _ := newReconciler(t, br, pod)
	reconcile(t, r, br.Name)
	if _, ok := getPod(t, c, "nix-builder-s7"); ok {
		t.Error("pod should have been deleted even without status.podName")
	}
}

func TestCompletedRequestIsReapedAfterTTL(t *testing.T) {
	br := newRequest("s8")
	controllerutil.AddFinalizer(br, FinalizerName)
	br.Status.Phase = nixv1alpha1.BuildPhaseFailed
	br.Status.CompletionTime = &metav1.Time{Time: time.Now().Add(-2 * time.Minute)}
	r, c, _ := newReconciler(t, br)
	reconcile(t, r, br.Name)
	got := get(t, c, br.Name)
	if got.DeletionTimestamp == nil {
		t.Error("expected the request to be deleted after its TTL")
	}

	fresh := newRequest("s9")
	controllerutil.AddFinalizer(fresh, FinalizerName)
	fresh.Status.Phase = nixv1alpha1.BuildPhaseCompleted
	fresh.Status.CompletionTime = &metav1.Time{Time: time.Now()}
	r2, c2, _ := newReconciler(t, fresh)
	res := reconcile(t, r2, fresh.Name)
	if res.RequeueAfter <= 0 || res.RequeueAfter > time.Minute {
		t.Errorf("expected requeue until TTL, got %v", res.RequeueAfter)
	}
	if got := get(t, c2, fresh.Name); got.DeletionTimestamp != nil {
		t.Error("fresh request must not be deleted before its TTL")
	}
}
