{{- define "mimic.labels" -}}
app.kubernetes.io/part-of: mimic
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "mimic.image" -}}
{{- if .Values.image.digest -}}
{{ .Values.image.repository }}@{{ .Values.image.digest }}
{{- else -}}
{{ .Values.image.repository }}:{{ .Values.image.tag | default .Chart.AppVersion }}
{{- end -}}
{{- end -}}

{{- define "mimic.initImage" -}}
{{- $img := .Values.inject.initImage -}}
{{- if $img.digest -}}
{{ $img.repository }}@{{ $img.digest }}
{{- else -}}
{{ $img.repository }}:{{ $img.tag | default .Chart.AppVersion }}
{{- end -}}
{{- end -}}

{{/* Names shared by the controller, the webhook configuration and the RBAC (spec §7.1, §11). */}}
{{- define "mimic.webhookSecret" -}}
{{ .Release.Name }}-webhook-tls
{{- end -}}

{{/*
CEL projection of the webhooks of the injector configuration without clientConfig.caBundle
(spec §11). The argument is the root variable, "object" or "oldObject". Optional fields are
guarded with has(); dyn() keeps the map literal heterogeneous.
*/}}
{{- define "mimic.webhookProjection" -}}
{{ . }}.webhooks.map(w, {
  "name": dyn(w.name),
  "service": dyn(has(w.clientConfig.service) ? w.clientConfig.service : null),
  "url": dyn(has(w.clientConfig.url) ? w.clientConfig.url : null),
  "rules": dyn(has(w.rules) ? w.rules : null),
  "namespaceSelector": dyn(has(w.namespaceSelector) ? w.namespaceSelector : null),
  "objectSelector": dyn(has(w.objectSelector) ? w.objectSelector : null),
  "failurePolicy": dyn(has(w.failurePolicy) ? w.failurePolicy : null),
  "sideEffects": dyn(has(w.sideEffects) ? w.sideEffects : null),
  "reinvocationPolicy": dyn(has(w.reinvocationPolicy) ? w.reinvocationPolicy : null),
  "timeoutSeconds": dyn(has(w.timeoutSeconds) ? w.timeoutSeconds : null),
  "admissionReviewVersions": dyn(has(w.admissionReviewVersions) ? w.admissionReviewVersions : null),
  "matchPolicy": dyn(has(w.matchPolicy) ? w.matchPolicy : null)
})
{{- end -}}

{{- define "mimic.webhookConfiguration" -}}
{{ .Release.Name }}-injector
{{- end -}}

{{- define "mimic.controllerService" -}}
{{ .Release.Name }}-controller
{{- end -}}

{{- define "mimic.valkeyImage" -}}
{{ .Values.valkey.image.repository }}:{{ .Values.valkey.image.tag }}{{ with .Values.valkey.image.digest }}@{{ . }}{{ end }}
{{- end -}}

{{- define "mimic.valkeySecret" -}}
{{ .Values.valkey.existingSecret | default (printf "%s-valkey" .Release.Name) }}
{{- end -}}

{{/* Pod Security "restricted" container settings (spec §11). */}}
{{- define "mimic.containerSecurity" -}}
allowPrivilegeEscalation: false
readOnlyRootFilesystem: true
runAsNonRoot: true
capabilities:
  drop: [ALL]
seccompProfile:
  type: RuntimeDefault
{{- end -}}
