package v1alpha1

import (
	"testing"

	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// TestDeepCopyIsIndependent guards the hand-written DeepCopy methods: mutating a
// copy must never affect the original. If a pointer/slice/map field is added to
// these types without updating DeepCopyInto, this test catches the resulting
// shared-reference bug (which would otherwise corrupt the controller cache).
func TestDeepCopyIsIndependent(t *testing.T) {
	timeout := int64(3600)
	orig := &NixBuildRequest{
		ObjectMeta: metav1.ObjectMeta{
			Name:        "build-x",
			Namespace:   "default",
			Labels:      map[string]string{"k": "v"},
			Annotations: map[string]string{"a": "b"},
		},
		Spec: NixBuildRequestSpec{
			SessionID:      "x",
			TimeoutSeconds: &timeout,
			NodeSelector:   map[string]string{"arch": "arm64"},
			Resources: corev1.ResourceRequirements{
				Requests: corev1.ResourceList{corev1.ResourceCPU: resource.MustParse("1")},
			},
		},
		Status: NixBuildRequestStatus{
			Phase:      BuildPhaseRunning,
			PodName:    "nix-builder-x",
			StartTime:  &metav1.Time{},
			Conditions: []BuildCondition{{Type: BuildConditionPodReady, Status: corev1.ConditionTrue}},
		},
	}

	cp := orig.DeepCopy()

	// Mutate every pointer/slice/map field on the copy.
	cp.Labels["k"] = "mutated"
	cp.Annotations["a"] = "mutated"
	*cp.Spec.TimeoutSeconds = 1
	cp.Spec.NodeSelector["arch"] = "amd64"
	cp.Spec.Resources.Requests[corev1.ResourceCPU] = resource.MustParse("99")
	cp.Status.Conditions[0].Status = corev1.ConditionFalse
	cp.Status.PodName = "other"

	if orig.Labels["k"] != "v" || orig.Annotations["a"] != "b" {
		t.Error("ObjectMeta maps are shared between copy and original")
	}
	if *orig.Spec.TimeoutSeconds != 3600 {
		t.Error("Spec.TimeoutSeconds pointer is shared")
	}
	if orig.Spec.NodeSelector["arch"] != "arm64" {
		t.Error("Spec.NodeSelector map is shared")
	}
	if orig.Spec.Resources.Requests.Cpu().String() != "1" {
		t.Error("Spec.Resources map is shared")
	}
	if orig.Status.Conditions[0].Status != corev1.ConditionTrue {
		t.Error("Status.Conditions slice is shared")
	}
	if orig.Status.PodName != "nix-builder-x" {
		t.Error("Status was unexpectedly mutated")
	}
	if orig.Status.StartTime == cp.Status.StartTime {
		t.Error("Status.StartTime pointer is shared")
	}
}

func TestListDeepCopyIsIndependent(t *testing.T) {
	orig := &NixBuildRequestList{
		Items: []NixBuildRequest{
			{ObjectMeta: metav1.ObjectMeta{Name: "a"}, Spec: NixBuildRequestSpec{SessionID: "a"}},
		},
	}
	cp := orig.DeepCopy()
	cp.Items[0].Spec.SessionID = "mutated"
	if orig.Items[0].Spec.SessionID != "a" {
		t.Error("List Items slice is shared between copy and original")
	}
	if obj := orig.DeepCopyObject(); obj == nil {
		t.Error("DeepCopyObject returned nil")
	}
}
