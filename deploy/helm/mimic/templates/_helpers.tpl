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
