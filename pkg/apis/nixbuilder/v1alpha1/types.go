package v1alpha1

import (
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// NixBuildRequest represents a request for a Nix build that needs a dedicated builder pod
type NixBuildRequest struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   NixBuildRequestSpec   `json:"spec,omitempty"`
	Status NixBuildRequestStatus `json:"status,omitempty"`
}

// NixBuildRequestSpec defines the desired state of a Nix build request
type NixBuildRequestSpec struct {
	// SessionID links this build request to the SSH proxy session
	SessionID string `json:"sessionId"`

	// Resources defines the pod resource requirements
	Resources corev1.ResourceRequirements `json:"resources,omitempty"`

	// Image specifies the builder container image
	Image string `json:"image,omitempty"`

	// Timeout for the build in seconds (default: 3600)
	TimeoutSeconds *int32 `json:"timeoutSeconds,omitempty"`

	// NodeSelector for pod placement
	NodeSelector map[string]string `json:"nodeSelector,omitempty"`
}

// NixBuildRequestStatus defines the observed state of a Nix build request
type NixBuildRequestStatus struct {
	// Phase represents the current state of the build request
	Phase BuildPhase `json:"phase,omitempty"`

	// PodName is the name of the created builder pod
	PodName string `json:"podName,omitempty"`

	// PodIP is the IP address of the builder pod for SSH routing
	PodIP string `json:"podIP,omitempty"`

	// StartTime when the build request was created
	StartTime *metav1.Time `json:"startTime,omitempty"`

	// CompletionTime when the build finished
	CompletionTime *metav1.Time `json:"completionTime,omitempty"`

	// Message provides human-readable status information
	Message string `json:"message,omitempty"`

	// Conditions represent the latest observations of the build request state
	Conditions []BuildCondition `json:"conditions,omitempty"`
}

// BuildPhase represents the phase of a build request
type BuildPhase string

const (
	// BuildPhasePending means the build request has been created but pod is not yet scheduled
	BuildPhasePending BuildPhase = "Pending"
	// BuildPhaseCreating means the pod is being created
	BuildPhaseCreating BuildPhase = "Creating"
	// BuildPhaseRunning means the pod is running and ready for SSH connections
	BuildPhaseRunning BuildPhase = "Running"
	// BuildPhaseCompleted means the build finished successfully
	BuildPhaseCompleted BuildPhase = "Completed"
	// BuildPhaseFailed means the build or pod failed
	BuildPhaseFailed BuildPhase = "Failed"
)

// BuildCondition represents a condition of a build request
type BuildCondition struct {
	// Type of condition
	Type BuildConditionType `json:"type"`
	// Status of the condition (True, False, Unknown)
	Status corev1.ConditionStatus `json:"status"`
	// LastTransitionTime is the last time the condition transitioned
	LastTransitionTime metav1.Time `json:"lastTransitionTime"`
	// Reason is a machine-readable reason for the condition
	Reason string `json:"reason,omitempty"`
	// Message is a human-readable message for the condition
	Message string `json:"message,omitempty"`
}

// BuildConditionType represents the type of build condition
type BuildConditionType string

const (
	// BuildConditionPodReady indicates the builder pod is ready for SSH connections
	BuildConditionPodReady BuildConditionType = "PodReady"
	// BuildConditionCompleted indicates the build has completed
	BuildConditionCompleted BuildConditionType = "Completed"
	// BuildConditionFailed indicates the build has failed
	BuildConditionFailed BuildConditionType = "Failed"
)

// NixBuildRequestList contains a list of NixBuildRequest
type NixBuildRequestList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []NixBuildRequest `json:"items"`
}
