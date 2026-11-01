{{/* The chart's name. */}}
{{- define "nrbc.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
The prefix of all object names. It leaves room for the longest suffix
("-builder-nix-config"), because service names are limited to 63 characters.
*/}}
{{- define "nrbc.fullname" -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- $full := .Values.fullnameOverride -}}
{{- if not $full -}}
{{- $full = ternary .Release.Name (printf "%s-%s" .Release.Name $name) (contains $name .Release.Name) -}}
{{- end -}}
{{- $full | trunc 44 | trimSuffix "-" -}}
{{- end -}}

{{/* Labels that select the pods of a component. Args: context, component. */}}
{{- define "nrbc.selectorLabels" -}}
app.kubernetes.io/name: {{ include "nrbc.name" .context }}
app.kubernetes.io/instance: {{ .context.Release.Name }}
{{- with .component }}
app.kubernetes.io/component: {{ . }}
{{- end }}
{{- end -}}

{{/* Labels of all objects. Args: context, component (optional). */}}
{{- define "nrbc.labels" -}}
{{ include "nrbc.selectorLabels" . }}
app.kubernetes.io/version: {{ .context.Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .context.Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .context.Chart.Name .context.Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end -}}

{{- define "nrbc.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "nrbc.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "nrbc.nixConfigName" -}}
{{- include "nrbc.fullname" . }}-builder-nix-config
{{- end -}}

{{/* An image reference. Args: context, image (the image's values). */}}
{{- define "nrbc.image" -}}
{{- printf "%s:%s" .image.repository (.image.tag | default .context.Chart.AppVersion) -}}
{{- end -}}

{{/* Non-empty if the controller uses leader election. */}}
{{- define "nrbc.leaderElect" -}}
{{- if or .Values.controller.leaderElect (gt (int .Values.controller.replicaCount) 1) -}}
true
{{- end -}}
{{- end -}}

{{/* A resource list as a flag value: cpu=1,memory=2Gi. */}}
{{- define "nrbc.resourceFlag" -}}
{{- $items := list -}}
{{- range $name, $quantity := . -}}
{{- $items = append $items (printf "%s=%v" $name $quantity) -}}
{{- end -}}
{{- join "," $items -}}
{{- end -}}

{{/*
A pod annotation that restarts the pods when the chart's secret changes. The
controller and the proxy read the keys only when they start.
*/}}
{{- define "nrbc.sshKeysChecksum" -}}
{{- if .Values.sshKeys.create -}}
checksum/ssh-keys: {{ include (print .Template.BasePath "/ssh-keys-secret.yaml") . | sha256sum }}
{{- end -}}
{{- end -}}

{{/* Pod annotations for scraping Prometheus metrics. */}}
{{- define "nrbc.prometheusAnnotations" -}}
prometheus.io/scrape: "true"
prometheus.io/port: "8082"
prometheus.io/path: /metrics
{{- end -}}

{{/* Container environment for OTLP export. */}}
{{- define "nrbc.telemetryEnv" -}}
{{- with .Values.telemetry.otlpEndpoint -}}
- name: POD_NAME
  valueFrom:
    fieldRef:
      fieldPath: metadata.name
- name: POD_NAMESPACE
  valueFrom:
    fieldRef:
      fieldPath: metadata.namespace
- name: OTEL_RESOURCE_ATTRIBUTES
  value: k8s.namespace.name=$(POD_NAMESPACE),k8s.pod.name=$(POD_NAME)
- name: OTEL_EXPORTER_OTLP_ENDPOINT
  value: {{ . | quote }}
{{- end -}}
{{- end -}}
