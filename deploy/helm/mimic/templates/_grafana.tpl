{{/* The bundled Grafana image, pinned by digest (spec §10.7, MON-15). */}}
{{- define "mimic.grafanaImage" -}}
{{ .Values.monitoring.grafana.image.repository }}:{{ .Values.monitoring.grafana.image.tag }}{{ with .Values.monitoring.grafana.image.digest }}@{{ . }}{{ end }}
{{- end -}}

{{/* The dashboard sidecar image, pinned by digest (spec §10.7, MON-15). */}}
{{- define "mimic.grafanaSidecarImage" -}}
{{ .Values.monitoring.grafana.sidecarImage.repository }}:{{ .Values.monitoring.grafana.sidecarImage.tag }}{{ with .Values.monitoring.grafana.sidecarImage.digest }}@{{ . }}{{ end }}
{{- end -}}

{{/* The Secret of the admin password (spec §10.7, MON-19): kept across upgrades by lookup, as the Valkey one is. */}}
{{- define "mimic.grafanaAdminSecret" -}}
{{ .Release.Name }}-grafana-admin
{{- end -}}

{{/* The URL of the provisioned Prometheus datasource (spec §10.7): the value, else the bundled Prometheus. */}}
{{- define "mimic.grafanaDatasourceUrl" -}}
{{- with .Values.monitoring.grafana.datasourceUrl -}}
{{ . }}
{{- else -}}
http://{{ .Release.Name }}-prometheus:9090
{{- end -}}
{{- end -}}

{{/*
grafana.ini (spec §10.7): no analytics, update check, news, plugin install, anonymous access or sign-up, and JSON logs
on the console. The admin user is admin; the password comes from the Secret, as a file (GF_SECURITY_ADMIN_PASSWORD__FILE).
*/}}
{{- define "mimic.grafanaIni" -}}
[analytics]
reporting_enabled = false
check_for_updates = false
check_for_plugin_updates = false

[news]
news_feed_enabled = false

[plugins]
preinstall_disabled = true

[auth.anonymous]
enabled = false

[security]
admin_user = admin
disable_gravatar = true

[users]
allow_sign_up = false

[log]
mode = console

[log.console]
format = json
{{- end -}}

{{/* The provisioned datasource, uid mimic-prometheus (the dashboards of #380 to #384 refer to it). */}}
{{- define "mimic.grafanaDatasources" -}}
apiVersion: 1
datasources:
  - name: mimic-prometheus
    uid: mimic-prometheus
    type: prometheus
    access: proxy
    url: {{ include "mimic.grafanaDatasourceUrl" . | quote }}
    isDefault: true
    editable: false
{{- end -}}

{{/* The dashboards provider: a file provider on /tmp/dashboards, which the sidecar fills (30 s rescan, no UI edits). */}}
{{- define "mimic.grafanaDashboards" -}}
apiVersion: 1
providers:
  - name: mimic
    orgId: 1
    type: file
    allowUiUpdates: false
    updateIntervalSeconds: 30
    options:
      path: /tmp/dashboards
{{- end -}}

{{/* The checksum of the Grafana configuration: a change rolls the pod (spec §10.7). */}}
{{- define "mimic.grafanaConfigChecksum" -}}
{{- printf "%s\n%s\n%s" (include "mimic.grafanaIni" .) (include "mimic.grafanaDatasources" .) (include "mimic.grafanaDashboards" .) | sha256sum -}}
{{- end -}}
