{{/*
The scrape targets of the monitoring objects (spec §10.7, MON-17), as JSON: the ServiceMonitor, the PodMonitors and
the jobs of prometheus.yml are all rendered from this list, so their selectors, ports, paths, intervals and target
labels cannot drift. Each target: name (the monitor object), job (the job label), matchLabels, exists (label keys
that must exist), port (the port name), interval, namespaces (the namespaces of its prometheus.yml job; empty: all),
relabel (the monitor's target labels: a source and a target, or a target and a replacement). namespace and pod are
the Operator's implicit target labels, which prometheus.yml sets for every job. The valkey target exists only with
valkey.metrics.enabled.
*/}}
{{- define "mimic.scrapeTargets" -}}
{{- $rel := .Release.Name -}}
{{- $promNamespaces := .Values.monitoring.prometheus.namespaces | default (list) -}}
{{- $podNamespaces := list -}}
{{- if $promNamespaces -}}
{{- $podNamespaces = uniq (sortAlpha (concat $promNamespaces (list .Release.Namespace))) -}}
{{- end -}}
{{- $targets := list
  (dict "name" (printf "%s-controller" $rel) "job" (printf "%s-controller" $rel)
    "matchLabels" (dict "app.kubernetes.io/instance" $rel "app.kubernetes.io/name" "mimic-controller")
    "exists" (list) "port" "admin" "interval" .Values.metrics.serviceMonitor.interval
    "namespaces" (list .Release.Namespace) "relabel" (list))
  (dict "name" (printf "%s-sidecar" $rel) "job" "mimic-sidecar"
    "matchLabels" (dict) "exists" (list "mimic.platform.io/deployment") "port" "mimic-admin"
    "interval" .Values.metrics.podMonitor.interval "namespaces" $podNamespaces
    "relabel" (list
      (dict "source" "__meta_kubernetes_namespace" "target" "namespace")
      (dict "source" "__meta_kubernetes_pod_name" "target" "pod")
      (dict "source" "__meta_kubernetes_pod_label_mimic_platform_io_deployment" "target" "deployment")
      (dict "target" "job" "replacement" "mimic-sidecar")))
  (dict "name" (printf "%s-load" $rel) "job" "mimic-load"
    "matchLabels" (dict) "exists" (list "mimic.platform.io/load-test") "port" "mimic-admin"
    "interval" .Values.metrics.podMonitor.interval "namespaces" $podNamespaces
    "relabel" (list
      (dict "source" "__meta_kubernetes_namespace" "target" "namespace")
      (dict "source" "__meta_kubernetes_pod_name" "target" "pod")
      (dict "source" "__meta_kubernetes_pod_label_mimic_platform_io_load_test" "target" "load_test")
      (dict "target" "job" "replacement" "mimic-load")))
-}}
{{- if .Values.valkey.metrics.enabled -}}
{{- $targets = append $targets (dict "name" (printf "%s-valkey" $rel) "job" "mimic-valkey"
  "matchLabels" (dict "app.kubernetes.io/instance" $rel "app.kubernetes.io/name" "mimic-valkey")
  "exists" (list) "port" "metrics" "interval" .Values.metrics.podMonitor.interval
  "namespaces" (list .Release.Namespace) "relabel" (list (dict "target" "job" "replacement" "mimic-valkey"))) -}}
{{- end -}}
{{- toJson (dict "targets" $targets) -}}
{{- end -}}

{{/* The bundled Prometheus image, pinned by digest (spec §10.7, MON-15). */}}
{{- define "mimic.prometheusImage" -}}
{{ .Values.monitoring.prometheus.image.repository }}:{{ .Values.monitoring.prometheus.image.tag }}{{ with .Values.monitoring.prometheus.image.digest }}@{{ . }}{{ end }}
{{- end -}}

{{/* One target of mimic.scrapeTargets as JSON. Arguments: dict "root" (the root context) and "job" (the job label). */}}
{{- define "mimic.scrapeTarget" -}}
{{- range (include "mimic.scrapeTargets" .root | fromJson).targets -}}
{{- if eq .job $.job -}}
{{- toJson . -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* The label selector string of a target for prometheus.yml: the matchLabels as k=v (sorted), then the keys that must exist. */}}
{{- define "mimic.scrapeLabelSelector" -}}
{{- $parts := list -}}
{{- range $k, $v := .matchLabels -}}
{{- $parts = append $parts (printf "%s=%s" $k $v) -}}
{{- end -}}
{{- join "," (concat $parts .exists) -}}
{{- end -}}

{{/*
prometheus.yml (spec §10.7, MON-17): one scrape job per target of mimic.scrapeTargets, with kubernetes_sd in the
pod role, a server-side label selector, the namespaces of the target, the port by its name, the drop of pods that
have finished, and the target labels of the monitor. Prometheus scrapes no node, kubelet, cAdvisor or API server.
*/}}
{{- define "mimic.prometheusConfig" -}}
scrape_configs:
{{- range $t := (include "mimic.scrapeTargets" . | fromJson).targets }}
  - job_name: {{ $t.job }}
    metrics_path: /metrics
    scrape_interval: {{ $t.interval }}
    kubernetes_sd_configs:
      - role: pod
        {{- with $t.namespaces }}
        namespaces:
          names: {{ toJson . }}
        {{- end }}
        selectors:
          - role: pod
            label: {{ include "mimic.scrapeLabelSelector" $t | quote }}
    relabel_configs:
      - source_labels: [__meta_kubernetes_pod_container_port_name]
        action: keep
        regex: {{ $t.port | quote }}
      - source_labels: [__meta_kubernetes_pod_phase]
        action: drop
        regex: "Succeeded|Failed"
      - source_labels: [__meta_kubernetes_namespace]
        target_label: namespace
      - source_labels: [__meta_kubernetes_pod_name]
        target_label: pod
      {{- range $r := $t.relabel }}
      {{- if not (has $r.target (list "namespace" "pod")) }}
      {{- if $r.source }}
      - source_labels: [{{ $r.source }}]
        target_label: {{ $r.target }}
      {{- else }}
      - target_label: {{ $r.target }}
        replacement: {{ $r.replacement }}
      {{- end }}
      {{- end }}
      {{- end }}
      {{- end }}
{{- end }}
