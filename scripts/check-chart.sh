#!/usr/bin/env bash
# #378, spec §12.13 (MON-01): the chart's default render is the golden file, and the Valkey exporter and its
# PodMonitor have the shape of §10.7 when enabled. Renders with `helm template` only, no cluster.
#   helm lint --strict of the variants that the CI chart job lints; the golden diff of the default;
#   assertions on the rendered objects with yq. Exit 0 when all hold, 1 on the first failure.
set -euo pipefail
cd "$(dirname "$0")/.."

chart=deploy/helm/mimic
digest="sha256:$(printf '0%.0s' $(seq 64))"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fail() { echo "FAIL $*" >&2; exit 1; }
# render [helm flags...]: the chart as CI renders it (the Valkey Secret is given, so the output is stable).
render() { helm template mimic "$chart" -n mimic-system --set valkey.existingSecret=golden "$@"; }
# lint <name> [helm flags...]: helm lint --strict of one variant.
lint() {
  local name=$1; shift
  helm lint --strict "$chart" "$@" >/dev/null || fail "chart lint: $name"
  echo "lint ok: $name"
}

lint default
lint podMonitor --set metrics.podMonitor.enabled=true
lint valkey.metrics --set valkey.metrics.enabled=true
lint both --set metrics.podMonitor.enabled=true --set valkey.metrics.enabled=true
lint valkey.metrics.existingSecret --set valkey.metrics.enabled=true --set valkey.metrics.existingSecret=gitops-exporter
lint loadTest.podMonitor --set loadTest.enabled=true --set image.digest="$digest" --set metrics.podMonitor.enabled=true
# #379 (MON-14): the bundled monitoring lints strictly, alone and with the monitors and the load test.
lint prometheus --set monitoring.prometheus.enabled=true
lint prometheus.namespaces --set monitoring.prometheus.enabled=true --set 'monitoring.prometheus.namespaces[0]=apps'
lint prometheus.monitors --set monitoring.prometheus.enabled=true --set metrics.serviceMonitor.enabled=true \
  --set metrics.podMonitor.enabled=true --set valkey.metrics.enabled=true
lint prometheus.loadTest --set monitoring.prometheus.enabled=true --set loadTest.enabled=true \
  --set image.digest="$digest" --set metrics.podMonitor.enabled=true
lint prometheus.netpol --set monitoring.prometheus.enabled=true --set valkey.metrics.enabled=true \
  --set networkPolicy.apiServerCIDR=10.96.0.0/16
lint grafana --set monitoring.grafana.enabled=true
lint grafana.prometheus --set monitoring.grafana.enabled=true --set monitoring.prometheus.enabled=true
lint grafana.all --set monitoring.grafana.enabled=true --set monitoring.prometheus.enabled=true \
  --set valkey.metrics.enabled=true --set metrics.podMonitor.enabled=true --set loadTest.enabled=true \
  --set image.digest="$digest"
# #380 (MON-23): the dashboard ConfigMaps lint strictly, alone and with the bundled monitoring.
lint dashboards --set monitoring.dashboards.enabled=true
lint dashboards.grafana.prometheus --set monitoring.dashboards.enabled=true --set monitoring.grafana.enabled=true \
  --set monitoring.prometheus.enabled=true

# MON-01, default: identical to the golden render (deploy/helm/golden/default.yaml, rendered from main), blank
# lines aside: helm 3 and helm 4 separate the documents with different blank lines.
nonblank() { grep -v '^[[:space:]]*$' "$1"; }
render > "$tmp/default.yaml"
if ! cmp -s <(nonblank "$tmp/default.yaml") <(nonblank deploy/helm/golden/default.yaml); then
  diff -u <(nonblank deploy/helm/golden/default.yaml) <(nonblank "$tmp/default.yaml") >&2 || true
  fail "mon_01: the default render differs from deploy/helm/golden/default.yaml"
fi
echo "mon_01: the default render is the golden file"

# MON-01, exporter and PodMonitor on: the rendered objects of §10.7.
render --set valkey.metrics.enabled=true --set metrics.podMonitor.enabled=true > "$tmp/on.yaml"
# doc <yq select condition>: the matching documents of the rendered file.
doc() { yq "select($1)" "$tmp/on.yaml"; }
sts=$(doc '.kind == "StatefulSet" and .metadata.name == "mimic-valkey"')
valkey=$(printf '%s\n' "$sts" | yq '.spec.template.spec.containers[] | select(.name == "valkey")')
exporter=$(printf '%s\n' "$sts" | yq '.spec.template.spec.containers[] | select(.name == "exporter")')
[[ -n $exporter ]] || fail "mon_01: the Valkey pod has no exporter container"

args=$(printf '%s\n' "$exporter" | yq -r '.args | join(" ")')
[[ $args == "--web.listen-address=0.0.0.0:9121 --redis.addr=redis://localhost:6379" ]] \
  || fail "mon_01: the exporter arguments are not the fixed list: $args"

env_names=$(printf '%s\n' "$exporter" | yq -r '.env[].name' | sort | tr '\n' ' ')
[[ $env_names == "REDIS_PASSWORD REDIS_USER " ]] \
  || fail "mon_01: the exporter env is not REDIS_USER and REDIS_PASSWORD only: $env_names"
[[ $(printf '%s\n' "$exporter" | yq -r '.env[] | select(.name == "REDIS_USER") | .value') == mimic-exporter ]] \
  || fail "mon_01: REDIS_USER is not mimic-exporter"
[[ $(printf '%s\n' "$exporter" | yq -r '.env[] | select(.name == "REDIS_PASSWORD") | .valueFrom.secretKeyRef.name') == mimic-valkey-exporter ]] \
  || fail "mon_01: REDIS_PASSWORD does not come from the exporter Secret"

[[ $(printf '%s\n' "$exporter" | yq -r '.securityContext.runAsUser') == 65532 ]] || fail "mon_01: the exporter is not UID 65532"
[[ $(printf '%s\n' "$exporter" | yq -r '.securityContext.runAsNonRoot') == true ]] || fail "mon_01: the exporter may run as root"
[[ $(printf '%s\n' "$exporter" | yq -r '.securityContext.readOnlyRootFilesystem') == true ]] || fail "mon_01: the exporter root filesystem is writable"
[[ $(printf '%s\n' "$exporter" | yq -r '.securityContext.capabilities.drop | join(",")') == ALL ]] || fail "mon_01: the exporter does not drop ALL capabilities"
[[ $(printf '%s\n' "$exporter" | yq -r '.securityContext.seccompProfile.type') == RuntimeDefault ]] || fail "mon_01: the exporter seccomp is not RuntimeDefault"
[[ $(printf '%s\n' "$exporter" | yq -r '.ports[] | select(.name == "metrics") | .containerPort') == 9121 ]] \
  || fail "mon_01: the exporter port 9121 is not named metrics"
# #399: liveness only. A readiness of the exporter would take the Valkey pod's Service endpoint away on a restart.
[[ $(printf '%s\n' "$exporter" | yq -r '.livenessProbe.tcpSocket.port') == metrics ]] \
  || fail "mon_01: the exporter has no TCP liveness probe on metrics"
[[ $(printf '%s\n' "$exporter" | yq -r '.readinessProbe') == null ]] \
  || fail "mon_01: the exporter has a readinessProbe (its restart would drop the Valkey endpoint)"

valkey_args=$(printf '%s\n' "$valkey" | yq -r '.args[]')
grep -qx -- '--aclfile' <<<"$valkey_args" || fail "mon_01: Valkey has no --aclfile"
grep -qx -- '/etc/valkey-acl/aclfile' <<<"$valkey_args" || fail "mon_01: Valkey does not read the ACL file of the Secret"
! grep -qx -- '--requirepass' <<<"$valkey_args" || fail "mon_01: Valkey has --requirepass with --aclfile (one source of users)"

# The ACL file is built by the valkey-acl init container at each pod start from the Secrets (spec §10.7, #378), so
# no Secret holds it, and the render works whatever existingSecret resolves to.
init=$(printf '%s\n' "$sts" | yq '.spec.template.spec.initContainers[] | select(.name == "valkey-acl")')
[[ -n $init ]] || fail "mon_01: no valkey-acl init container builds the ACL file"
[[ $(printf '%s\n' "$init" | yq -r '.image') == "$(printf '%s\n' "$valkey" | yq -r '.image')" ]] \
  || fail "mon_01: the ACL init container is not the pinned Valkey image"
[[ $(printf '%s\n' "$init" | yq -r '.env[] | select(.name == "VALKEY_PASSWORD") | .valueFrom.secretKeyRef.name') == golden ]] \
  || fail "mon_01: the ACL init container does not read the Valkey password from the Valkey Secret"
[[ $(printf '%s\n' "$init" | yq -r '.env[] | select(.name == "EXPORTER_PASSWORD") | .valueFrom.secretKeyRef.name') == mimic-valkey-exporter ]] \
  || fail "mon_01: the ACL init container does not read the exporter password from the exporter Secret"
[[ $(printf '%s\n' "$init" | yq -r '.securityContext.readOnlyRootFilesystem') == true ]] \
  || fail "mon_01: the ACL init container root filesystem is writable"
[[ $(printf '%s\n' "$init" | yq -r '.securityContext.allowPrivilegeEscalation') == false ]] \
  || fail "mon_01: the ACL init container allows privilege escalation"
[[ $(printf '%s\n' "$init" | yq -r '.securityContext.capabilities.drop | join(",")') == ALL ]] \
  || fail "mon_01: the ACL init container does not drop ALL capabilities"
[[ $(printf '%s\n' "$sts" | yq -r '.spec.template.spec.volumes[] | select(.name == "acl") | .emptyDir.medium') == Memory ]] \
  || fail "mon_01: the ACL volume is not an in-memory emptyDir"
init_script=$(printf '%s\n' "$init" | yq -r '.args[0]')
grep -qF 'user default on #%s ~* &* +@all' <<<"$init_script" || fail "mon_01: the ACL init container writes no default user line"
grep -qF 'user mimic-exporter on #%s +info +ping resetkeys resetchannels' <<<"$init_script" \
  || fail "mon_01: the ACL init container writes no exporter user line"
grep -qF 'umask 0226' <<<"$init_script" || fail "mon_01: the ACL file is not mode 0440"
grep -qF '[ -n "$VALKEY_PASSWORD" ]' <<<"$init_script" && grep -qF '[ -n "$EXPORTER_PASSWORD" ]' <<<"$init_script" \
  || fail "mon_01: the ACL init container does not refuse an empty password"
! grep -qi nopass <<<"$init_script" || fail "mon_01: the ACL init container writes nopass"
! grep -q aclfile <<<"$(doc '.kind == "Secret"')" || fail "mon_01: a Secret holds an ACL file"

# A render without existingSecret: the init container reads the chart's own Valkey Secret.
fresh=$(helm template mimic "$chart" -n mimic-system --set valkey.metrics.enabled=true)
fresh_init=$(yq 'select(.kind == "StatefulSet" and .metadata.name == "mimic-valkey") | .spec.template.spec.initContainers[] | select(.name == "valkey-acl")' <<<"$fresh")
[[ $(printf '%s\n' "$fresh_init" | yq -r '.env[] | select(.name == "VALKEY_PASSWORD") | .valueFrom.secretKeyRef.name') == mimic-valkey ]] \
  || fail "mon_01: without existingSecret the init container does not read the chart's Valkey Secret"
echo "mon_01: the ACL init container builds the file from the Secrets; no Secret holds it"

# #399: the exporter's password. Without valkey.metrics.existingSecret the chart renders its Secret; with it, none, and the
# exporter and valkey-acl read the key password of that Secret. Neither StatefulSet carries a pod annotation (no checksum),
# so a render never restarts Valkey.
exporter_secret_ref() { # exporter_secret_ref <yq doc of the StatefulSet>: the secretKeyRef of the exporter and of valkey-acl
  yq -r '[(.spec.template.spec.containers[] | select(.name == "exporter") | .env[] | select(.name == "REDIS_PASSWORD") | .valueFrom.secretKeyRef),
    (.spec.template.spec.initContainers[] | select(.name == "valkey-acl") | .env[] | select(.name == "EXPORTER_PASSWORD") | .valueFrom.secretKeyRef)]
    | map(.name + "/" + .key) | join(" ")' <<<"$1"
}
[[ -n $(doc '.kind == "Secret" and .metadata.name == "mimic-valkey-exporter"') ]] \
  || fail "mon_01: no exporter Secret mimic-valkey-exporter is rendered by default"
[[ $(exporter_secret_ref "$sts") == "mimic-valkey-exporter/password mimic-valkey-exporter/password" ]] \
  || fail "mon_01: the exporter and valkey-acl do not read the exporter Secret by default"
[[ $(printf '%s\n' "$sts" | yq -r '.spec.template.metadata.annotations') == null ]] \
  || fail "mon_01: the Valkey pod template has an annotation (a checksum would restart Valkey on a render)"
ext=$(render --set valkey.metrics.enabled=true --set valkey.metrics.existingSecret=gitops-exporter)
ext_sts=$(yq 'select(.kind == "StatefulSet" and .metadata.name == "mimic-valkey")' <<<"$ext")
[[ -z $(yq 'select(.kind == "Secret" and .metadata.name == "mimic-valkey-exporter")' <<<"$ext") ]] \
  || fail "mon_01: an exporter Secret is rendered with valkey.metrics.existingSecret"
[[ -z $(yq 'select(.kind == "Secret" and .metadata.name == "gitops-exporter")' <<<"$ext") ]] \
  || fail "mon_01: the existing exporter Secret is rendered by the chart"
[[ $(exporter_secret_ref "$ext_sts") == "gitops-exporter/password gitops-exporter/password" ]] \
  || fail "mon_01: with valkey.metrics.existingSecret the exporter and valkey-acl do not read that Secret's key password"
[[ $(printf '%s\n' "$ext_sts" | yq -r '.spec.template.metadata.annotations') == null ]] \
  || fail "mon_01: the Valkey pod template has an annotation with valkey.metrics.existingSecret"
[[ $(yq -r '.spec.template.spec.containers[] | select(.name == "valkey") | .args | join(" ")' <<<"$ext_sts") == *"--aclfile /etc/valkey-acl/aclfile"* ]] \
  || fail "mon_01: Valkey does not read the ACL file with valkey.metrics.existingSecret"
echo "mon_01: valkey.metrics.existingSecret: no exporter Secret is rendered, and both readers use its key password"

# The Valkey NetworkPolicy: 6379 from the controller, 9121 from the release namespace.
netpol=$(doc '.kind == "NetworkPolicy" and .metadata.name == "mimic-valkey"')
[[ $(printf '%s\n' "$netpol" | yq -r '.spec.ingress[1].ports[0].port') == 9121 ]] \
  || fail "mon_01: the Valkey NetworkPolicy has no 9121 rule"
[[ $(printf '%s\n' "$netpol" | yq -r '.spec.ingress[1].from[0].namespaceSelector.matchLabels["kubernetes.io/metadata.name"]') == mimic-system ]] \
  || fail "mon_01: the 9121 rule is not from the release namespace"

# No rule of a new object names secrets or nodes/*.
new_objects=$(doc '.kind == "PodMonitor" or .kind == "NetworkPolicy"')
if grep -qE 'secrets|nodes/' <<<"$new_objects"; then
  fail "mon_01: a new object names secrets or nodes/*"
fi

# The PodMonitor of the Valkey exporter.
pm=$(doc '.kind == "PodMonitor" and .metadata.name == "mimic-valkey"')
[[ -n $pm ]] || fail "mon_01: no PodMonitor of the Valkey exporter"
[[ $(printf '%s\n' "$pm" | yq -r '.spec.podMetricsEndpoints[0].port') == metrics ]] \
  || fail "mon_01: the PodMonitor does not scrape port metrics"
[[ $(printf '%s\n' "$pm" | yq -r '.spec.namespaceSelector.matchNames[0]') == mimic-system ]] \
  || fail "mon_01: the PodMonitor leaves the release namespace"
[[ $(printf '%s\n' "$pm" | yq -r '.spec.podMetricsEndpoints[0].relabelings[0].replacement') == mimic-valkey ]] \
  || fail "mon_01: the PodMonitor job is not mimic-valkey"
echo "mon_01: the exporter, its Secret, the ACL init container, the NetworkPolicy and the PodMonitor have the shape of §10.7"

# The exporter alone adds no PodMonitor.
render --set valkey.metrics.enabled=true > "$tmp/exporter-only.yaml"
podmonitors=$(yq 'select(.kind == "PodMonitor")' "$tmp/exporter-only.yaml")
[[ -z $podmonitors ]] || fail "mon_01: a PodMonitor rendered without metrics.podMonitor.enabled"

# loadTest.metricsFrom: the fallback is the release namespace peer (no metricsNamespaceSelector) with the
# PodMonitor on; no peer without it; a set value wins. The check reads the policy's CEL expression.
load_expression() {
  yq -r 'select(.kind == "ValidatingAdmissionPolicy" and .metadata.name == "mimic-load-job") | .spec.validations[].expression' "$1"
}
render --set loadTest.enabled=true --set image.digest="$digest" --set metrics.podMonitor.enabled=true > "$tmp/load-pm.yaml"
expr=$(load_expression "$tmp/load-pm.yaml")
grep -qF '[{"namespaceSelector":{"matchLabels":{"kubernetes.io/metadata.name":"mimic-system"}}}]' <<<"$expr" \
  || fail "mon_01: loadTest.metricsFrom does not fall back to the release namespace with metrics.podMonitor.enabled"

render --set loadTest.enabled=true --set image.digest="$digest" > "$tmp/load.yaml"
expr=$(load_expression "$tmp/load.yaml")
grep -qF 'variables.ingress == []' <<<"$expr" \
  || fail "mon_01: loadTest.metricsFrom admits a peer without metrics.podMonitor.enabled"

render --set loadTest.enabled=true --set image.digest="$digest" --set metrics.podMonitor.enabled=true \
  --set 'loadTest.metricsFrom[0].podSelector.matchLabels.app=scrape' > "$tmp/load-set.yaml"
expr=$(load_expression "$tmp/load-set.yaml")
grep -qF '"app":"scrape"' <<<"$expr" || fail "mon_01: a set loadTest.metricsFrom does not win over the fallback"
echo "mon_01: loadTest.metricsFrom falls back, and a set value wins"

# MON-01, PodMonitors (#378, MON-03): the sidecar and load monitors scrape the named port mimic-admin, with the target
# labels of §10.7; the interval and the namespace selector come from the values; the default has none of them; the
# Valkey monitor exists only with valkey.metrics; no NetworkPolicy changes with the monitors.
render --set metrics.podMonitor.enabled=true --set metrics.podMonitor.interval=45s \
  --set metrics.podMonitor.namespaceSelector.any=false --set 'metrics.podMonitor.namespaceSelector.matchNames[0]=apps' > "$tmp/pm.yaml"
# pmdoc <name>: one PodMonitor of the monitor render.
pmdoc() { yq "select(.kind == \"PodMonitor\" and .metadata.name == \"$1\")" "$tmp/pm.yaml"; }
sidecar=$(pmdoc mimic-sidecar)
load=$(pmdoc mimic-load)
[[ -n $sidecar && -n $load ]] || fail "mon_01: the sidecar and load PodMonitors are not rendered"
[[ $(pmdoc mimic-valkey) == "" ]] || fail "mon_01: the Valkey PodMonitor is rendered without valkey.metrics.enabled"

[[ $(printf '%s\n' "$sidecar" | yq -r '.spec.selector.matchExpressions[0].key') == mimic.platform.io/deployment ]] \
  && [[ $(printf '%s\n' "$sidecar" | yq -r '.spec.selector.matchExpressions[0].operator') == Exists ]] \
  || fail "mon_01: the sidecar PodMonitor does not select mimic.platform.io/deployment Exists"
[[ $(printf '%s\n' "$sidecar" | yq -r '.spec.podMetricsEndpoints[0].port') == mimic-admin ]] \
  || fail "mon_01: the sidecar PodMonitor does not scrape the port named mimic-admin"
[[ $(printf '%s\n' "$sidecar" | yq -r '.spec.podMetricsEndpoints[0].path') == /metrics ]] \
  || fail "mon_01: the sidecar PodMonitor path is not /metrics"
[[ $(printf '%s\n' "$sidecar" | yq -r '.spec.podMetricsEndpoints[0].interval') == 45s ]] \
  || fail "mon_01: the sidecar PodMonitor ignores metrics.podMonitor.interval"
targets=$(printf '%s\n' "$sidecar" | yq -r '.spec.podMetricsEndpoints[0].relabelings[].targetLabel' | sort | tr '\n' ' ')
[[ $targets == "deployment job namespace pod " ]] || fail "mon_01: the sidecar target labels are not namespace, pod, deployment, job: $targets"
[[ $(printf '%s\n' "$sidecar" | yq -r '.spec.podMetricsEndpoints[0].relabelings[] | select(.targetLabel == "deployment") | .sourceLabels[0]') == __meta_kubernetes_pod_label_mimic_platform_io_deployment ]] \
  || fail "mon_01: the deployment target label is not the pod label mimic.platform.io/deployment"
[[ $(printf '%s\n' "$sidecar" | yq -r '.spec.podMetricsEndpoints[0].relabelings[] | select(.targetLabel == "job") | .replacement') == mimic-sidecar ]] \
  || fail "mon_01: the sidecar job is not mimic-sidecar"
[[ $(printf '%s\n' "$sidecar" | yq -r '.spec.namespaceSelector.matchNames[0]') == apps ]] \
  || fail "mon_01: the sidecar PodMonitor ignores metrics.podMonitor.namespaceSelector"

[[ $(printf '%s\n' "$load" | yq -r '.spec.selector.matchExpressions[0].key') == mimic.platform.io/load-test ]] \
  && [[ $(printf '%s\n' "$load" | yq -r '.spec.selector.matchExpressions[0].operator') == Exists ]] \
  || fail "mon_01: the load PodMonitor does not select mimic.platform.io/load-test Exists"
[[ $(printf '%s\n' "$load" | yq -r '.spec.podMetricsEndpoints[0].port') == mimic-admin ]] \
  || fail "mon_01: the load PodMonitor does not scrape the port named mimic-admin"
[[ $(printf '%s\n' "$load" | yq -r '.spec.podMetricsEndpoints[0].relabelings[] | select(.targetLabel == "load_test") | .sourceLabels[0]') == __meta_kubernetes_pod_label_mimic_platform_io_load_test ]] \
  || fail "mon_01: the load_test target label is not the pod label mimic.platform.io/load-test"
[[ $(printf '%s\n' "$load" | yq -r '.spec.podMetricsEndpoints[0].relabelings[] | select(.targetLabel == "job") | .replacement') == mimic-load ]] \
  || fail "mon_01: the load job is not mimic-load"

# Default monitors: the namespace selector is every namespace, and the NetworkPolicies are those of the default render.
render --set metrics.podMonitor.enabled=true > "$tmp/pm-default.yaml"
[[ $(yq -r 'select(.kind == "PodMonitor" and .metadata.name == "mimic-sidecar") | .spec.namespaceSelector.any' "$tmp/pm-default.yaml") == true ]] \
  || fail "mon_01: the default PodMonitor namespace selector is not any: true"
[[ $(yq 'select(.kind == "NetworkPolicy")' "$tmp/pm-default.yaml") == "$(yq 'select(.kind == "NetworkPolicy")' "$tmp/default.yaml")" ]] \
  || fail "mon_01: the PodMonitors changed a NetworkPolicy"
echo "mon_01: the sidecar and load PodMonitors scrape mimic-admin with their target labels; the values apply"

# MIMIC_LOAD_NETWORK_POLICY of the controller (spec §10.2, §7.8, LDT-05, LDT-06): with loadTest.enabled the env holds
# the peers of the generator NetworkPolicy as JSON; its metricsFrom is the peers of the load test admission policy
# (the same helper), with and without loadTest.metricsFrom set.
load_env() {
  yq -r 'select(.kind == "Deployment" and .metadata.name == "mimic-controller") | .spec.template.spec.containers[0].env[] | select(.name == "MIMIC_LOAD_NETWORK_POLICY") | .value' "$1"
}
# netpol_check <name> [helm flags...]: the env of one render with loadTest.enabled is the policy's peers.
netpol_check() {
  local name=$1; shift
  render --set loadTest.enabled=true --set image.digest="$digest" "$@" > "$tmp/$name.yaml"
  local env_json peers expr
  env_json=$(load_env "$tmp/$name.yaml")
  [[ -n $env_json ]] || fail "mon_01: $name: MIMIC_LOAD_NETWORK_POLICY is not set with loadTest.enabled"
  yq -p json -o json -I0 '.' <<<"$env_json" >/dev/null 2>&1 || fail "mon_01: $name: MIMIC_LOAD_NETWORK_POLICY is not JSON"
  [[ $(yq -p json -o json -I0 '.controllerLabels' <<<"$env_json") == '{"app.kubernetes.io/instance":"mimic","app.kubernetes.io/name":"mimic-controller"}' ]] \
    || fail "mon_01: $name: the controller labels are not the controller pods' labels"
  [[ $(yq -p json -r '.dnsNamespace' <<<"$env_json") == kube-system ]] || fail "mon_01: $name: dnsNamespace is not loadTest.dnsSelector.namespace"
  [[ $(yq -p json -o json -I0 '.dnsLabels' <<<"$env_json") == '{"k8s-app":"kube-dns"}' ]] || fail "mon_01: $name: dnsLabels is not loadTest.dnsSelector.podLabels"
  peers=$(yq -p json -o json -I0 '.metricsFrom' <<<"$env_json")
  expr=$(load_expression "$tmp/$name.yaml")
  if [[ $peers == "[]" ]]; then
    grep -qF 'variables.ingress == []' <<<"$expr" || fail "mon_01: $name: the admission policy admits a peer the controller does not"
  else
    grep -qF "variables.ingress[0].from == $peers " <<<"$expr" \
      || fail "mon_01: $name: the admission policy's peers are not the controller's metricsFrom $peers"
  fi
  echo "mon_01: $name: MIMIC_LOAD_NETWORK_POLICY is JSON, and metricsFrom $peers is the admission policy's peers"
}
netpol_check fallback-none
netpol_check fallback-podmonitor --set metrics.podMonitor.enabled=true
netpol_check fallback-selector --set metrics.podMonitor.enabled=true \
  --set 'networkPolicy.metricsNamespaceSelector.matchLabels.team=ops'
netpol_check set-metricsfrom --set metrics.podMonitor.enabled=true \
  --set 'loadTest.metricsFrom[0].podSelector.matchLabels.app=scrape'
netpol_check set-metricsfrom-no-podmonitor --set 'loadTest.metricsFrom[0].podSelector.matchLabels.app=scrape'

# networkPolicy off: no env; the default render (loadTest disabled) has none either (golden above).
render --set loadTest.enabled=true --set image.digest="$digest" --set loadTest.networkPolicy=false > "$tmp/no-netpol.yaml"
[[ -z $(load_env "$tmp/no-netpol.yaml") ]] || fail "mon_01: MIMIC_LOAD_NETWORK_POLICY is set with loadTest.networkPolicy false"
[[ -z $(load_env "$tmp/default.yaml") ]] || fail "mon_01: MIMIC_LOAD_NETWORK_POLICY is set in the default render"
echo "mon_01: MIMIC_LOAD_NETWORK_POLICY is absent without loadTest.networkPolicy and in the default render"

# MON-14 (off part, #379): the bundled Prometheus and Grafana are absent by default and with the monitors on.
for f in default pm on load-pm; do
  if yq -r '.metadata.name' "$tmp/$f.yaml" | grep -qE '^mimic-(prometheus|grafana)'; then
    fail "mon_14: a bundled monitoring object is rendered with monitoring off ($f)"
  fi
done
echo "mon_14: no bundled monitoring object with monitoring off"

# MON-14 (#379): the chart has no Helm dependency: no dependencies in Chart.yaml, no Chart.lock and no charts/ directory.
[[ $(yq -r '.dependencies // [] | length' "$chart/Chart.yaml") == 0 ]] || fail "mon_14: Chart.yaml declares a dependency"
[[ ! -e $chart/Chart.lock && ! -e $chart/charts ]] || fail "mon_14: the chart vendors a Chart.lock or charts/"
echo "mon_14: the chart has no Helm dependency"

# MON-17 (#379): mon_17 <render> <jobs, in order> <monitors: yes|no>. prometheus.yml has exactly those jobs, all
# pods role, with the selector, port, path, interval and target labels of the monitor of the same name (the
# ServiceMonitor or PodMonitor; with "yes" it must exist). namespace and pod are the Operator's implicit target
# labels, which prometheus.yml sets for every job, so they are left out of the comparison.
mon_17() {
  local file=$1 expected=$2 monitors=$3 cfg jobs job j m msel psel mport pport mpath ppath mint pint mrel prel
  cfg=$(yq 'select(.kind == "ConfigMap" and .metadata.name == "mimic-prometheus") | .data["prometheus.yml"]' "$file")
  [[ -n $cfg ]] || fail "mon_17: no prometheus.yml in the ConfigMap mimic-prometheus"
  jobs=$(yq -r '.scrape_configs[].job_name' <<<"$cfg" | tr '\n' ' ')
  [[ $jobs == "$expected " ]] || fail "mon_17: the jobs are '$jobs', expected '$expected'"
  [[ $(yq -r '.scrape_configs[].kubernetes_sd_configs[].role' <<<"$cfg" | sort -u) == pod ]] \
    || fail "mon_17: a job discovers other than pods"
  ! grep -qE 'authorization|basic_auth|bearer_token|oauth2|tls_config|nodes|cadvisor|kubelet' <<<"$cfg" \
    || fail "mon_17: prometheus.yml holds credentials or scrapes a node"
  for job in $expected; do
    j=$(yq ".scrape_configs[] | select(.job_name == \"$job\")" <<<"$cfg")
    [[ $(yq -r '.relabel_configs[] | select(.action == "drop") | .regex' <<<"$j") == "Succeeded|Failed" ]] \
      || fail "mon_17: $job does not drop the Succeeded and Failed pods"
    [[ $(yq -r '.metrics_path' <<<"$j") == /metrics ]] || fail "mon_17: $job path is not /metrics"
    pport=$(yq -r '.relabel_configs[] | select(.action == "keep") | .regex' <<<"$j")
    psel=$(yq -r '.kubernetes_sd_configs[0].selectors[0].label' <<<"$j")
    pint=$(yq -r '.scrape_interval' <<<"$j")
    prel=$(yq -r '[.relabel_configs[] | select(.target_label != null and .target_label != "namespace" and .target_label != "pod") | ((.source_labels // []) | join(",")) + "|" + .target_label + "|" + (.replacement // "")] | join(";")' <<<"$j")
    m=$(yq "select((.kind == \"ServiceMonitor\" or .kind == \"PodMonitor\") and .metadata.name == \"$job\")" "$file")
    if [[ -z $m ]]; then
      [[ $monitors == no ]] || fail "mon_17: no ServiceMonitor or PodMonitor $job"
      continue
    fi
    mlabels=$(yq -r '(.spec.selector.matchLabels // {}) | to_entries | sort_by(.key) | map(.key + "=" + .value) | join(",")' <<<"$m")
    mexist=$(yq -r '(.spec.selector.matchExpressions // []) | map(.key) | join(",")' <<<"$m")
    msel=$(printf '%s\n' "$mlabels" "$mexist" | grep -v '^$' | paste -sd, - || true)
    [[ $msel == "$psel" ]] || fail "mon_17: $job selects '$psel', its monitor '$msel'"
    [[ $(yq -r '(.spec.podMetricsEndpoints // .spec.endpoints)[0].port' <<<"$m") == "$pport" ]] \
      || fail "mon_17: $job scrapes port '$pport', its monitor another"
    [[ $(yq -r '(.spec.podMetricsEndpoints // .spec.endpoints)[0].path' <<<"$m") == "$(yq -r '.metrics_path' <<<"$j")" ]] \
      || fail "mon_17: $job path differs from its monitor"
    [[ $(yq -r '(.spec.podMetricsEndpoints // .spec.endpoints)[0].interval' <<<"$m") == "$pint" ]] \
      || fail "mon_17: $job interval differs from its monitor"
    mrel=$(yq -r '((.spec.podMetricsEndpoints // .spec.endpoints)[0].relabelings // []) | map(select(.targetLabel != "namespace" and .targetLabel != "pod") | ((.sourceLabels // []) | join(",")) + "|" + .targetLabel + "|" + (.replacement // "")) | join(";")' <<<"$m")
    [[ $mrel == "$prel" ]] || fail "mon_17: $job relabels '$prel', its monitor '$mrel'"
    echo "mon_17: $job has the selector, port, path, interval and target labels of its monitor"
  done
}

# The four jobs with every monitor on; the valkey job only with valkey.metrics; the namespaces of each job.
render --set monitoring.prometheus.enabled=true --set metrics.serviceMonitor.enabled=true \
  --set metrics.podMonitor.enabled=true --set valkey.metrics.enabled=true > "$tmp/prom.yaml"
mon_17 "$tmp/prom.yaml" "mimic-controller mimic-sidecar mimic-load mimic-valkey" yes

render --set monitoring.prometheus.enabled=true --set metrics.serviceMonitor.enabled=true \
  --set metrics.podMonitor.enabled=true > "$tmp/prom-novalkey.yaml"
mon_17 "$tmp/prom-novalkey.yaml" "mimic-controller mimic-sidecar mimic-load" yes
[[ $(yq 'select(.kind == "PodMonitor" and .metadata.name == "mimic-valkey")' "$tmp/prom-novalkey.yaml") == "" ]] \
  || fail "mon_17: the Valkey PodMonitor is rendered without valkey.metrics.enabled"

render --set monitoring.prometheus.enabled=true > "$tmp/prom-only.yaml"
mon_17 "$tmp/prom-only.yaml" "mimic-controller mimic-sidecar mimic-load" no

# Namespaces (§10.7): the controller job is the release namespace; the pod jobs are every namespace (no names key)
# unless monitoring.prometheus.namespaces is set, then those namespaces and the release namespace.
prom_cfg() {
  yq 'select(.kind == "ConfigMap" and .metadata.name == "mimic-prometheus") | .data["prometheus.yml"]' "$1"
}
job_names() { # job_names <render> <job>: the namespaces names of one job, comma-joined ("" when all)
  yq ".scrape_configs[] | select(.job_name == \"$2\") | .kubernetes_sd_configs[0].namespaces.names // [] | join(\",\")" <<<"$(prom_cfg "$1")"
}
[[ $(job_names "$tmp/prom-only.yaml" mimic-controller) == mimic-system ]] \
  || fail "mon_17: the controller job is not in the release namespace only"
[[ -z $(job_names "$tmp/prom-only.yaml" mimic-sidecar) ]] || fail "mon_17: the sidecar job is not in every namespace"
render --set monitoring.prometheus.enabled=true --set 'monitoring.prometheus.namespaces[0]=apps' > "$tmp/prom-ns.yaml"
[[ $(job_names "$tmp/prom-ns.yaml" mimic-sidecar) == "apps,mimic-system" ]] \
  || fail "mon_17: the sidecar job does not select the listed namespaces and the release namespace"
[[ $(job_names "$tmp/prom-ns.yaml" mimic-load) == "apps,mimic-system" ]] \
  || fail "mon_17: the load job does not select the listed namespaces and the release namespace"
[[ $(job_names "$tmp/prom-ns.yaml" mimic-controller) == mimic-system ]] \
  || fail "mon_17: the controller job leaves the release namespace"
echo "mon_17: the scrape jobs and the monitors agree, and the namespaces follow monitoring.prometheus.namespaces"

# MON-16 (#379): the Prometheus Deployment, its Service and the arguments of §10.7; the security context and the volumes.
render --set monitoring.prometheus.enabled=true --set metrics.podMonitor.enabled=true > "$tmp/prom-deploy.yaml"
prom_dep=$(yq 'select(.kind == "Deployment" and .metadata.name == "mimic-prometheus")' "$tmp/prom-deploy.yaml")
[[ -n $prom_dep ]] || fail "mon_16: no Deployment mimic-prometheus"
ctr=$(yq '.spec.template.spec.containers[0]' <<<"$prom_dep")
pod_sc=$(yq '.spec.template.spec.securityContext' <<<"$prom_dep")
[[ $(yq -r '.spec.replicas' <<<"$prom_dep") == 1 ]] || fail "mon_16: the Deployment has not 1 replica"
[[ $(yq -r '.spec.strategy.type' <<<"$prom_dep") == Recreate ]] || fail "mon_16: the strategy is not Recreate"
[[ $(yq -r '.image' <<<"$ctr") == quay.io/prometheus/prometheus:v3.15.1@sha256:* ]] || fail "mon_16: the image is not the pinned Prometheus"
[[ $(yq -r '.args | join(" ")' <<<"$ctr") == "--config.file=/etc/prometheus/prometheus.yml --storage.tsdb.path=/prometheus --storage.tsdb.retention.time=24h --storage.tsdb.retention.size=800MB --web.listen-address=:9090 --log.format=json" ]] \
  || fail "mon_16: the Prometheus arguments are not those of §10.7: $(yq -r '.args | join(" ")' <<<"$ctr")"
grep -qE 'enable-admin-api|enable-lifecycle|remote-write|otlp' <<<"$ctr" && fail "mon_16: an admin, lifecycle, remote-write or OTLP flag is set"
cs() { yq -r 'select(.kind == "Deployment" and .metadata.name == "mimic-prometheus") | .spec.template.metadata.annotations["checksum/config"]' "$1"; }
render --set monitoring.prometheus.enabled=true --set metrics.podMonitor.enabled=true \
  --set metrics.podMonitor.interval=45s > "$tmp/prom-45s.yaml"
[[ -n $(cs "$tmp/prom-deploy.yaml") && $(cs "$tmp/prom-deploy.yaml") != "$(cs "$tmp/prom-45s.yaml")" ]] \
  || fail "mon_16: the checksum annotation does not change with metrics.podMonitor.interval"
[[ $(yq -r '.runAsUser' <<<"$pod_sc") == 65534 && $(yq -r '.runAsGroup' <<<"$pod_sc") == 65534 \
  && $(yq -r '.runAsNonRoot' <<<"$pod_sc") == true && $(yq -r '.seccompProfile.type' <<<"$pod_sc") == RuntimeDefault ]] \
  || fail "mon_16: the pod security context is not UID 65534, non-root, RuntimeDefault"
[[ $(yq -r '.securityContext.runAsUser' <<<"$ctr") == 65534 && $(yq -r '.securityContext.readOnlyRootFilesystem' <<<"$ctr") == true \
  && $(yq -r '.securityContext.allowPrivilegeEscalation' <<<"$ctr") == false \
  && $(yq -r '.securityContext.capabilities.drop | join(",")' <<<"$ctr") == ALL \
  && $(yq -r '.securityContext.seccompProfile.type' <<<"$ctr") == RuntimeDefault ]] \
  || fail "mon_16: the container security context is not restricted"
[[ $(yq -r '.resources.requests.cpu' <<<"$ctr") == 100m && $(yq -r '.resources.requests.memory' <<<"$ctr") == 256Mi \
  && $(yq -r '.resources.limits.memory' <<<"$ctr") == 512Mi ]] || fail "mon_16: the Prometheus resources are not those of the values"
[[ $(yq -r '.readinessProbe.httpGet.path' <<<"$ctr") == /-/ready && $(yq -r '.livenessProbe.httpGet.path' <<<"$ctr") == /-/healthy ]] \
  || fail "mon_16: the probes are not /-/ready and /-/healthy"
[[ $(yq -r '.volumeMounts[] | select(.name == "config") | .mountPath + " " + (.readOnly | tostring)' <<<"$ctr") == "/etc/prometheus true" ]] \
  || fail "mon_16: the configuration is not mounted read-only at /etc/prometheus"
[[ $(yq -r '.volumeMounts[] | select(.name == "data") | .mountPath' <<<"$ctr") == /prometheus ]] || fail "mon_16: the TSDB is not mounted at /prometheus"
[[ $(yq -r '.spec.template.spec.volumes[] | select(.name == "data") | .emptyDir.sizeLimit' <<<"$prom_dep") == 1Gi ]] \
  || fail "mon_16: the TSDB emptyDir has not sizeLimit 1Gi"
[[ $(yq -r '.spec.template.spec.volumes[] | select(.name == "config") | .configMap.name' <<<"$prom_dep") == mimic-prometheus ]] \
  || fail "mon_16: the configuration volume is not the ConfigMap mimic-prometheus"
prom_svc=$(yq 'select(.kind == "Service" and .metadata.name == "mimic-prometheus")' "$tmp/prom-deploy.yaml")
[[ $(yq -r '.spec.type' <<<"$prom_svc") == ClusterIP && $(yq -r '.spec.ports | length' <<<"$prom_svc") == 1 \
  && $(yq -r '.spec.ports[0].port' <<<"$prom_svc") == 9090 ]] || fail "mon_16: the Service is not ClusterIP on 9090 only"
echo "mon_16: the Prometheus Deployment has the arguments, the security context, the resources and the volumes of §10.7"

# MON-18 (#379, Prometheus part): list and watch on pods only, in a ClusterRole when namespaces is empty.
cr=$(yq 'select(.kind == "ClusterRole" and .metadata.name == "mimic-prometheus")' "$tmp/prom-deploy.yaml")
[[ -n $cr ]] || fail "mon_18: no ClusterRole mimic-prometheus"
[[ $(yq -r '.rules | length' <<<"$cr") == 1 ]] || fail "mon_18: the ClusterRole has more than one rule"
[[ $(yq -r '.rules[0] | [(.apiGroups | join(",")), (.resources | join(",")), (.verbs | join(","))] | join("|")' <<<"$cr") == "|pods|list,watch" ]] \
  || fail "mon_18: the ClusterRole is not list and watch on pods"
crb=$(yq 'select(.kind == "ClusterRoleBinding" and .metadata.name == "mimic-prometheus")' "$tmp/prom-deploy.yaml")
[[ $(yq -r '.roleRef.name' <<<"$crb") == mimic-prometheus && $(yq -r '.subjects[0].name' <<<"$crb") == mimic-prometheus \
  && $(yq -r '.subjects[0].namespace' <<<"$crb") == mimic-system ]] || fail "mon_18: the ClusterRoleBinding does not bind the ServiceAccount"
[[ -z $(yq 'select(.kind == "Role" and .metadata.name == "mimic-prometheus")' "$tmp/prom-deploy.yaml") ]] \
  || fail "mon_18: a Role mimic-prometheus is rendered with namespaces empty"

# With namespaces: a Role and a RoleBinding in each listed namespace and in the release namespace, each once.
render --set monitoring.prometheus.enabled=true --set 'monitoring.prometheus.namespaces[0]=apps' \
  --set 'monitoring.prometheus.namespaces[1]=mimic-system' > "$tmp/prom-rbac-ns.yaml"
[[ -z $(yq 'select(.kind == "ClusterRole" and .metadata.name == "mimic-prometheus")' "$tmp/prom-rbac-ns.yaml") ]] \
  || fail "mon_18: a ClusterRole is rendered with namespaces set"
roles=$(yq 'select(.kind == "Role" and .metadata.name == "mimic-prometheus") | .metadata.namespace' "$tmp/prom-rbac-ns.yaml" | grep -v -e "^---$" -e "^$" | tr "\n" " ")
rbs=$(yq 'select(.kind == "RoleBinding" and .metadata.name == "mimic-prometheus") | .metadata.namespace' "$tmp/prom-rbac-ns.yaml" | grep -v -e "^---$" -e "^$" | tr "\n" " ")
[[ $roles == "apps mimic-system " && $rbs == "apps mimic-system " ]] \
  || fail "mon_18: the Roles are '$roles' and the RoleBindings '$rbs', expected apps and mimic-system once each"
[[ $(yq -r 'select(.kind == "Role" and .metadata.name == "mimic-prometheus") | [(.rules | length), (.rules[0].resources | join(",")), (.rules[0].verbs | join(","))] | join("|")' "$tmp/prom-rbac-ns.yaml" | grep -v -e "^---$" -e "^$" | sort -u) == "1|pods|list,watch" ]] \
  || fail "mon_18: a namespaced Role is not the single rule list and watch on pods"
[[ $(yq -r 'select(.kind == "RoleBinding" and .metadata.name == "mimic-prometheus") | .subjects[0].namespace + "/" + .subjects[0].name' "$tmp/prom-rbac-ns.yaml" | grep -v -e "^---$" -e "^$" | sort -u) == "mimic-system/mimic-prometheus" ]] \
  || fail "mon_18: a RoleBinding does not bind the ServiceAccount of the release"
echo "mon_18: the Prometheus RBAC is list and watch on pods, cluster-wide or in the listed namespaces"

# MON-18, all: no monitoring object of the render names secrets, nodes, a subresource, a wildcard or a write verb.
for f in "$tmp/prom-deploy.yaml" "$tmp/prom-rbac-ns.yaml"; do
  rules=$(yq 'select((.kind == "ClusterRole" or .kind == "Role") and (.metadata.name == "mimic-prometheus")) | .rules' "$f")
  if grep -qE 'secrets|nodes|/|\*|create|update|patch|delete|escalate|bind|impersonate|get' <<<"$rules"; then
    fail "mon_18: a Prometheus rule names secrets, nodes, a subresource, a wildcard or a verb other than list and watch"
  fi
done
echo "mon_18: no Prometheus rule names secrets, nodes, a subresource or a verb other than list and watch"

# MON-20 (#379, Prometheus part): the NetworkPolicy of the bundled Prometheus. No ingress until Grafana is rendered;
# egress exactly to 15100, 8081, 9121 (with valkey.metrics) and 443 and 6443; no DNS.
render --set monitoring.prometheus.enabled=true --set valkey.metrics.enabled=true > "$tmp/np-prom.yaml"
npp=$(yq 'select(.kind == "NetworkPolicy" and .metadata.name == "mimic-prometheus")' "$tmp/np-prom.yaml")
[[ -n $npp ]] || fail "mon_20: no NetworkPolicy mimic-prometheus"
[[ $(yq -r '.spec.policyTypes | join(",")' <<<"$npp") == Ingress,Egress ]] || fail "mon_20: the policy types are not Ingress and Egress"
[[ $(yq -r '.spec.podSelector.matchLabels["app.kubernetes.io/name"]' <<<"$npp") == mimic-prometheus ]] \
  || fail "mon_20: the policy does not select the Prometheus pods"
[[ $(yq -r '.spec.ingress | length' <<<"$npp") == 0 ]] || fail "mon_20: the Prometheus policy admits ingress before Grafana is rendered"
[[ $(yq -r '[.spec.egress[].ports[].port] | join(",")' <<<"$npp") == "15100,8081,9121,443,6443" ]] \
  || fail "mon_20: the Prometheus egress ports are not 15100, 8081, 9121, 443 and 6443: $(yq -r '[.spec.egress[].ports[].port] | join(",")' <<<"$npp")"
[[ $(yq -r '.spec.egress[0].to[0].namespaceSelector | tojson' <<<"$npp") == "{}" ]] \
  || fail "mon_20: the 15100 egress is not to every namespace"
[[ $(yq -r '.spec.egress[1].to[0].podSelector.matchLabels["app.kubernetes.io/name"]' <<<"$npp") == mimic-controller ]] \
  || fail "mon_20: the 8081 egress is not to the controller pods"
[[ $(yq -r '.spec.egress[2].to[0].podSelector.matchLabels["app.kubernetes.io/name"]' <<<"$npp") == mimic-valkey ]] \
  || fail "mon_20: the 9121 egress is not to the Valkey pods"
[[ $(yq -r '.spec.egress[3].to | tojson' <<<"$npp") == null ]] || fail "mon_20: the API server egress has a peer with no apiServerCIDR"
grep -qE 'secrets|nodes/' <<<"$npp" && fail "mon_20: the Prometheus policy names secrets or nodes/*"

render --set monitoring.prometheus.enabled=true > "$tmp/np-prom-novalkey.yaml"
[[ $(yq -r '[.spec.egress[].ports[].port] | join(",")' <(yq 'select(.kind == "NetworkPolicy" and .metadata.name == "mimic-prometheus")' "$tmp/np-prom-novalkey.yaml")) == "15100,8081,443,6443" ]] \
  || fail "mon_20: the Valkey egress is present without valkey.metrics.enabled"

render --set monitoring.prometheus.enabled=true --set networkPolicy.apiServerCIDR=10.96.0.0/16 \
  --set 'monitoring.prometheus.namespaces[0]=apps' > "$tmp/np-prom-cidr.yaml"
npc=$(yq 'select(.kind == "NetworkPolicy" and .metadata.name == "mimic-prometheus")' "$tmp/np-prom-cidr.yaml")
[[ $(yq -r '.spec.egress[-1].to[0].ipBlock.cidr' <<<"$npc") == 10.96.0.0/16 ]] \
  || fail "mon_20: networkPolicy.apiServerCIDR is not the API server peer"
[[ $(yq -r '.spec.egress[0].to[] | .namespaceSelector.matchLabels["kubernetes.io/metadata.name"]' <<<"$npc" | sort | paste -sd, -) == "apps,mimic-system" ]] \
  || fail "mon_20: the 15100 egress does not follow monitoring.prometheus.namespaces"

render --set monitoring.prometheus.enabled=true --set networkPolicy.enabled=false > "$tmp/np-off.yaml"
[[ -z $(yq 'select(.kind == "NetworkPolicy" and .metadata.name == "mimic-prometheus")' "$tmp/np-off.yaml") ]] \
  || fail "mon_20: the Prometheus policy is rendered with networkPolicy.enabled false"
echo "mon_20: the Prometheus NetworkPolicy admits nothing in, and egresses to the scrape, controller, exporter and API server only"

# MON-20 (loadTest.metricsFrom): the bundled Prometheus falls back to the namespace peer as the PodMonitor does; a set value wins.
render --set loadTest.enabled=true --set image.digest="$digest" --set monitoring.prometheus.enabled=true > "$tmp/load-prom.yaml"
expr=$(load_expression "$tmp/load-prom.yaml")
grep -qF '[{"namespaceSelector":{"matchLabels":{"kubernetes.io/metadata.name":"mimic-system"}}}]' <<<"$expr" \
  || fail "mon_20: loadTest.metricsFrom does not fall back to the release namespace with monitoring.prometheus.enabled"
render --set loadTest.enabled=true --set image.digest="$digest" --set monitoring.prometheus.enabled=true \
  --set 'loadTest.metricsFrom[0].podSelector.matchLabels.app=scrape' > "$tmp/load-prom-set.yaml"
expr=$(load_expression "$tmp/load-prom-set.yaml")
grep -qF 'variables.ingress[0].from == [{"podSelector":{"matchLabels":{"app":"scrape"}}}]' <<<"$expr" \
  || fail "mon_20: a set loadTest.metricsFrom does not win over the Prometheus fallback"
echo "mon_20: loadTest.metricsFrom falls back to the release namespace with the bundled Prometheus, and a set value wins"

# MON-19 (#379): the bundled Grafana: a Deployment of the grafana and dashboards containers, the admin Secret as a file in
# grafana only, grafana.ini and the provisioning, the dashboards sidecar without credentials, and a ClusterIP Service.
render --set monitoring.grafana.enabled=true --set monitoring.prometheus.enabled=true > "$tmp/grafana.yaml"
gdep=$(yq 'select(.kind == "Deployment" and .metadata.name == "mimic-grafana")' "$tmp/grafana.yaml")
[[ -n $gdep ]] || fail "mon_19: no Deployment mimic-grafana"
[[ $(yq -r '.spec.replicas' <<<"$gdep") == 1 && $(yq -r '.spec.strategy.type' <<<"$gdep") == Recreate ]] \
  || fail "mon_19: the Grafana Deployment is not 1 replica with strategy Recreate"
[[ $(yq -r '.spec.template.spec.containers[].name' <<<"$gdep" | paste -sd, -) == grafana,dashboards ]] \
  || fail "mon_19: the containers are not grafana and dashboards"
[[ $(yq -r '.spec.template.spec.initContainers' <<<"$gdep") == null ]] || fail "mon_19: the Grafana pod has an init container"
[[ $(yq -r '.spec.template.spec.automountServiceAccountToken' <<<"$gdep") == false ]] || fail "mon_19: the token is automounted"
gctr=$(yq '.spec.template.spec.containers[] | select(.name == "grafana")' <<<"$gdep")
dctr=$(yq '.spec.template.spec.containers[] | select(.name == "dashboards")' <<<"$gdep")
[[ $(yq -r '.image' <<<"$gctr") == docker.io/grafana/grafana:13.2.4-distroless@sha256:* ]] || fail "mon_19: the grafana image is not the pinned one"
[[ $(yq -r '.image' <<<"$dctr") == quay.io/kiwigrid/k8s-sidecar:2.14.1@sha256:* ]] || fail "mon_19: the sidecar image is not the pinned one"
tmpvol=$(yq -r '.volumeMounts[] | select(.mountPath == "/tmp") | .name' <<<"$gctr")
[[ -n $tmpvol && $(yq -r ".spec.template.spec.volumes[] | select(.name == \"$tmpvol\") | .emptyDir.medium" <<<"$gdep") == Memory ]] \
  || fail "mon_19: grafana has no memory emptyDir on /tmp (its backend plugins need it on the read-only root)"
[[ $(yq -r ".spec.template.spec.volumes[] | select(.name == \"$tmpvol\") | .emptyDir.sizeLimit" <<<"$gdep") == 16Mi ]] \
  || fail "mon_19: the /tmp emptyDir of grafana has not sizeLimit 16Mi (§10.7)"
[[ $(yq -r '.spec.template.spec.containers[] | select(.volumeMounts[]?.name == "kube-api-access") | .name' <<<"$gdep") == dashboards ]] \
  || fail "mon_19: the projected token is not mounted in dashboards only"
[[ $(yq -r '.spec.template.spec.volumes[] | select(.name == "kube-api-access") | .projected.sources[0] | has("serviceAccountToken")' <<<"$gdep") == true ]] \
  || fail "mon_19: the projected volume has no service account token"
for c in gctr dctr; do
  ctr_var=${!c}
  [[ $(yq -r '.securityContext.runAsNonRoot' <<<"$ctr_var") == true && $(yq -r '.securityContext.readOnlyRootFilesystem' <<<"$ctr_var") == true \
    && $(yq -r '.securityContext.allowPrivilegeEscalation' <<<"$ctr_var") == false \
    && $(yq -r '.securityContext.capabilities.drop | join(",")' <<<"$ctr_var") == ALL \
    && $(yq -r '.securityContext.seccompProfile.type' <<<"$ctr_var") == RuntimeDefault ]] \
    || fail "mon_19: a Grafana container is not restricted ($c)"
done
[[ $(yq -r '.securityContext.runAsUser' <<<"$gctr") == 472 ]] || fail "mon_19: grafana is not UID 472"
[[ $(yq -r '.securityContext.runAsUser' <<<"$dctr") == 65534 ]] || fail "mon_19: dashboards is not UID 65534"
[[ $(yq -r '.resources.requests.cpu + " " + .resources.limits.memory' <<<"$gctr") == "50m 384Mi" ]] \
  || fail "mon_19: the grafana resources are not those of the values"

# The admin password: a Secret of 32 alphanumeric characters, a file in grafana only, no env value, in no other object.
sec=$(yq 'select(.kind == "Secret" and .metadata.name == "mimic-grafana-admin")' "$tmp/grafana.yaml")
[[ -n $sec ]] || fail "mon_19: no Secret mimic-grafana-admin"
pw=$(yq -r '.data["admin-password"] | @base64d' <<<"$sec")
[[ $pw =~ ^[A-Za-z0-9]{32}$ ]] || fail "mon_19: the admin password is not 32 alphanumeric characters"
b64=$(yq -r '.data["admin-password"]' <<<"$sec")
[[ $(grep -cF -- "$b64" "$tmp/grafana.yaml") == 1 ]] || fail "mon_19: the admin password is in another object"
! grep -qF -- "$pw" "$tmp/grafana.yaml" || fail "mon_19: the admin password is in plain text in the render"
# The password reaches Grafana through grafana.ini (security.admin_password = $__file{...}), not through an env:
# the GF_SECURITY_ADMIN_PASSWORD__FILE form leaves the default admin on "admin" (reproduced with grafana 13.2.4).
[[ $(yq -r '.env // [] | length' <<<"$gctr") == 0 ]] || fail "mon_19: the grafana container has an env"
[[ $(yq -r '.env[].name' <<<"$dctr" | grep -c PASSWORD || true) == 0 ]] || fail "mon_19: the sidecar has a password env"
[[ $(yq -r '.resources.limits.memory' <<<"$dctr") == 128Mi ]] \
  || fail "mon_19: the dashboards memory limit is not that of the values (128Mi, the sidecar needs about 90Mi)"
[[ $(yq -r '[.volumeMounts[].name] | join(",")' <<<"$gctr") == "admin,grafana-ini,provisioning-datasources,provisioning-dashboards,data,tmp,dashboard-files" ]] \
  || fail "mon_19: the grafana mounts are not those of the spec"
grep -q 'secretKeyRef' <<<"$gdep" && fail "mon_19: an env reads a Secret key"
[[ $(yq -r '.spec.template.spec.volumes[] | select(.name == "admin") | .secret.secretName' <<<"$gdep") == mimic-grafana-admin ]] \
  || fail "mon_19: the admin volume is not the Secret"

# grafana.ini and the provisioning (§10.7).
ini=$(yq -r 'select(.kind == "ConfigMap" and .metadata.name == "mimic-grafana") | .data["grafana.ini"]' "$tmp/grafana.yaml")
disabled=$(sed -n 's/^disable_plugins = //p' <<<"$ini")
[[ -n $disabled ]] && ! tr ',' '\n' <<<"$disabled" | grep -qx prometheus \
  || fail "mon_19: grafana.ini does not disable the unused datasource plugins, or disables prometheus"
# #407: the legacy postgres plugin ID is disabled as well (grafana-postgresql-datasource is the current one).
tr ',' '\n' <<<"$disabled" | grep -qx postgres || fail "mon_19: plugins.disable_plugins does not list the legacy postgres plugin"
for line in "reporting_enabled = false" "check_for_updates = false" "check_for_plugin_updates = false" \
  "news_feed_enabled = false" "preinstall_disabled = true" "enabled = false" "allow_sign_up = false" \
  "disable_gravatar = true" "mode = console" "format = json" "admin_user = admin"; do
  grep -qxF -- "$line" <<<"$ini" || fail "mon_19: grafana.ini lacks '$line'"
done
grep -qxF -- 'admin_password = $__file{/etc/grafana-admin/admin-password}' <<<"$ini" \
  || fail "mon_19: grafana.ini does not read the admin password from the Secret file"
prov=$(yq 'select(.kind == "ConfigMap" and .metadata.name == "mimic-grafana-provisioning")' "$tmp/grafana.yaml")
ds=$(yq -r '.data["datasources.yaml"]' <<<"$prov")
[[ $(yq -r '.datasources[0] | [.uid, .type, .url, (.editable | tostring)] | join(" ")' <<<"$ds") == "mimic-prometheus prometheus http://mimic-prometheus:9090 false" ]] \
  || fail "mon_19: the provisioned datasource is not the bundled Prometheus with uid mimic-prometheus"
dash=$(yq -r '.data["dashboards.yaml"]' <<<"$prov")
[[ $(yq -r '.providers[0] | [.type, .options.path, (.updateIntervalSeconds | tostring), (.allowUiUpdates | tostring)] | join(" ")' <<<"$dash") == "file /tmp/dashboards 30 false" ]] \
  || fail "mon_19: the dashboards provider is not a file provider on /tmp/dashboards, 30 s, no UI updates"
[[ $(yq -r '.spec.template.metadata.annotations["checksum/config"]' <<<"$gdep") != null ]] || fail "mon_19: no checksum annotation"

# The sidecar: the label, the value, the resource and the namespace; no REQ_URL, REQ_USERNAME or REQ_PASSWORD.
[[ $(yq -r '[.env[] | .name + "=" + (.value // "")] | join(" ")' <<<"$dctr") == "LABEL=grafana_dashboard LABEL_VALUE=1 RESOURCE=configmap NAMESPACE=mimic-system METHOD=WATCH FOLDER=/tmp/dashboards" ]] \
  || fail "mon_19: the sidecar environment is not the one of the spec"

# Service and objects: ClusterIP on 3000 only; no Ingress, HTTPRoute or PersistentVolumeClaim.
gsvc=$(yq 'select(.kind == "Service" and .metadata.name == "mimic-grafana")' "$tmp/grafana.yaml")
[[ $(yq -r '.spec.type' <<<"$gsvc") == ClusterIP && $(yq -r '.spec.ports | length' <<<"$gsvc") == 1 \
  && $(yq -r '.spec.ports[0].port' <<<"$gsvc") == 3000 ]] || fail "mon_19: the Grafana Service is not ClusterIP on 3000 only"
! grep -qE '^kind: (Ingress|HTTPRoute|PersistentVolumeClaim)$' "$tmp/grafana.yaml" || fail "mon_19: an Ingress, HTTPRoute or PVC is rendered"

# The checksum follows the provisioning (the bundled datasource URL).
render --set monitoring.grafana.enabled=true --set monitoring.prometheus.enabled=true \
  --set monitoring.grafana.datasourceUrl=http://other:9090 > "$tmp/grafana-url.yaml"
gcs() { yq -r 'select(.kind == "Deployment" and .metadata.name == "mimic-grafana") | .spec.template.metadata.annotations["checksum/config"]' "$1"; }
[[ $(gcs "$tmp/grafana.yaml") != "$(gcs "$tmp/grafana-url.yaml")" ]] || fail "mon_19: the checksum does not follow the datasource"
[[ $(yq -r '.spec.template.spec.securityContext.fsGroup' <<<"$gdep") == 472 \
  && $(yq -r '.spec.template.spec.securityContext.supplementalGroups | join(",")' <<<"$gdep") == 472 ]] \
  || fail "mon_19: the pod has no fsGroup 472 and supplementalGroups [472]"
echo "mon_19: the Grafana pod, its Secret, its configuration and its Service have the shape of §10.7"

# MON-18 (Grafana part): the sidecar's Role lists and watches ConfigMaps of the release namespace only, bound to its own
# ServiceAccount, and no other binding names that ServiceAccount.
grole=$(yq 'select(.kind == "Role" and .metadata.name == "mimic-grafana-dashboards")' "$tmp/grafana.yaml")
[[ $(yq -r '.rules | length' <<<"$grole") == 1 \
  && $(yq -r '.rules[0] | [(.apiGroups | join(",")), (.resources | join(",")), (.verbs | join(","))] | join("|")' <<<"$grole") == "|configmaps|get,list,watch" ]] \
  || fail "mon_18: the Grafana Role is not get, list and watch on configmaps"
grb=$(yq 'select(.kind == "RoleBinding" and .metadata.name == "mimic-grafana-dashboards")' "$tmp/grafana.yaml")
[[ $(yq -r '.roleRef.name' <<<"$grb") == mimic-grafana-dashboards && $(yq -r '.subjects[0].name' <<<"$grb") == mimic-grafana \
  && $(yq -r '.subjects[0].namespace' <<<"$grb") == mimic-system ]] || fail "mon_18: the Grafana RoleBinding does not bind its ServiceAccount"
bindings=$(yq -r 'select(.kind == "RoleBinding" or .kind == "ClusterRoleBinding") | .kind + " " + .metadata.name + " " + ([.subjects[]?.name] | join(","))' "$tmp/grafana.yaml" | grep -v '^$' || true)
[[ $(grep -c 'mimic-grafana$' <<<"$bindings") == 1 ]] && grep -q '^RoleBinding mimic-grafana-dashboards ' <<<"$bindings" \
  || fail "mon_18: another binding names the Grafana ServiceAccount"
! grep -qE 'secrets|nodes|\*' <<<"$(yq 'select(.kind == "Role" and .metadata.name == "mimic-grafana-dashboards") | .rules' "$tmp/grafana.yaml")" \
  || fail "mon_18: the Grafana Role names secrets, nodes or a wildcard"
echo "mon_18: the Grafana Role reads configmaps of the release namespace only, and no other binding names its ServiceAccount"

# MON-20 (#379, Grafana part): the Grafana NetworkPolicy, and the Prometheus ingress from the Grafana pods.
gnp=$(yq 'select(.kind == "NetworkPolicy" and .metadata.name == "mimic-grafana")' "$tmp/grafana.yaml")
[[ -n $gnp ]] || fail "mon_20: no NetworkPolicy mimic-grafana"
[[ $(yq -r '.spec.policyTypes | join(",")' <<<"$gnp") == Ingress,Egress ]] || fail "mon_20: the Grafana policy types are not Ingress and Egress"
[[ $(yq -r '.spec.podSelector.matchLabels["app.kubernetes.io/name"]' <<<"$gnp") == mimic-grafana ]] || fail "mon_20: the policy does not select the Grafana pods"
[[ $(yq -r '[.spec.ingress[].ports[].port] | join(",")' <<<"$gnp") == 3000 \
  && $(yq -r '.spec.ingress[0].from[0].namespaceSelector.matchLabels["kubernetes.io/metadata.name"]' <<<"$gnp") == mimic-system ]] \
  || fail "mon_20: the Grafana ingress is not TCP 3000 from the release namespace"
[[ $(yq -r '.spec.egress[0].to[0].podSelector.matchLabels["app.kubernetes.io/name"]' <<<"$gnp") == mimic-prometheus \
  && $(yq -r '.spec.egress[0].ports[0].port' <<<"$gnp") == 9090 ]] || fail "mon_20: the Grafana egress is not 9090 to Prometheus"
[[ $(yq -r '.spec.egress[1].to[0].namespaceSelector.matchLabels["kubernetes.io/metadata.name"]' <<<"$gnp") == kube-system \
  && $(yq -r '.spec.egress[1].to[0].podSelector.matchLabels["k8s-app"]' <<<"$gnp") == kube-dns \
  && $(yq -r '[.spec.egress[1].ports[] | .port + "/" + .protocol] | join(",")' <<<"$gnp") == "53/UDP,53/TCP" ]] \
  || fail "mon_20: the Grafana DNS egress is not UDP and TCP 53 to loadTest.dnsSelector"
[[ $(yq -r '[.spec.egress[2].ports[].port] | join(",")' <<<"$gnp") == "443,6443" && $(yq -r '.spec.egress[2].to | tojson' <<<"$gnp") == null ]] \
  || fail "mon_20: the API server egress of Grafana is not 443 and 6443 with no peer"
[[ $(yq -r '.spec.egress | length' <<<"$gnp") == 3 ]] || fail "mon_20: the Grafana egress has rules beyond Prometheus, DNS and the API server"
grep -qE 'secrets|nodes/' <<<"$gnp" && fail "mon_20: the Grafana policy names secrets or nodes/*"

# The Prometheus ingress: 9090 from the Grafana pods only, and none without Grafana.
[[ $(yq -r '.spec.ingress[0].from[0].podSelector.matchLabels["app.kubernetes.io/name"]' <<<"$(yq 'select(.kind == "NetworkPolicy" and .metadata.name == "mimic-prometheus")' "$tmp/grafana.yaml")") == mimic-grafana ]] \
  || fail "mon_20: the Prometheus ingress is not from the Grafana pods"
[[ $(yq -r '[.spec.ingress[].ports[].port] | join(",")' <<<"$(yq 'select(.kind == "NetworkPolicy" and .metadata.name == "mimic-prometheus")' "$tmp/grafana.yaml")") == 9090 ]] \
  || fail "mon_20: the Prometheus ingress is not TCP 9090"
render --set monitoring.prometheus.enabled=true --set networkPolicy.enabled=true > "$tmp/np-prom-nog.yaml"
[[ $(yq -r '.spec.ingress | length' <<<"$(yq 'select(.kind == "NetworkPolicy" and .metadata.name == "mimic-prometheus")' "$tmp/np-prom-nog.yaml")") == 0 ]] \
  || fail "mon_20: the Prometheus policy admits ingress without Grafana"
render --set monitoring.grafana.enabled=true --set networkPolicy.enabled=false > "$tmp/np-grafana-off.yaml"
[[ -z $(yq 'select(.kind == "NetworkPolicy" and (.metadata.name == "mimic-grafana" or .metadata.name == "mimic-prometheus"))' "$tmp/np-grafana-off.yaml") ]] \
  || fail "mon_20: a monitoring NetworkPolicy is rendered with networkPolicy.enabled false"
echo "mon_20: the Grafana NetworkPolicy admits 3000 from the release namespace, and Prometheus admits 9090 from Grafana only"

# MON-20 (#379, datasourceEgress): the rules of datasourceEgress are appended only with datasourceUrl set.
ext=(--set monitoring.grafana.enabled=true --set networkPolicy.enabled=true
  --set 'monitoring.grafana.datasourceEgress[0].to[0].ipBlock.cidr=10.20.0.0/16'
  --set 'monitoring.grafana.datasourceEgress[0].ports[0].port=9090'
  --set 'monitoring.grafana.datasourceEgress[0].ports[0].protocol=TCP')
render "${ext[@]}" --set monitoring.grafana.datasourceUrl=https://prom.example:9090 > "$tmp/ext-url.yaml"
render "${ext[@]}" > "$tmp/ext-nourl.yaml"
gegress() { yq -r 'select(.kind == "NetworkPolicy" and .metadata.name == "mimic-grafana") | .spec.egress | length' "$1"; }
[[ $(gegress "$tmp/ext-url.yaml") == 4 ]] || fail "mon_20: the datasourceEgress rule is not appended with datasourceUrl set"
[[ $(yq 'select(.kind == "NetworkPolicy" and .metadata.name == "mimic-grafana") | .spec.egress[3].to[0].ipBlock.cidr' "$tmp/ext-url.yaml") == 10.20.0.0/16 ]] \
  || fail "mon_20: the appended datasourceEgress rule is not the one given"
[[ $(gegress "$tmp/ext-nourl.yaml") == 3 ]] || fail "mon_20: datasourceEgress is rendered with datasourceUrl empty"
echo "mon_20: datasourceEgress is appended only with datasourceUrl set"

# MON-22: the dashboards of deploy/helm/mimic/dashboards hold the conventions, the metrics and the panels of §9.1.
python3 -I scripts/check-dashboards.py || fail "mon_22: scripts/check-dashboards.py"
python3 -I scripts/test_check_dashboards.py >/dev/null 2>&1 || fail "mon_22: scripts/test_check_dashboards.py"

# MON-23: one ConfigMap per dashboard file, with monitoring.dashboards.enabled or the bundled Grafana, none by default.
files=$(ls "$chart"/dashboards/*.json | wc -l)
isdash='select(.kind == "ConfigMap" and (.metadata.labels.grafana_dashboard // "") == "1")'
render > "$tmp/dash-off.yaml"
[[ -z $(yq "$isdash" "$tmp/dash-off.yaml") ]] || fail "mon_23: a dashboard ConfigMap is rendered by default"
render --set monitoring.dashboards.enabled=true --set monitoring.dashboards.annotations.grafana_folder=Mimic > "$tmp/dash-on.yaml"
render --set monitoring.grafana.enabled=true > "$tmp/dash-grafana.yaml"
for f in dash-on dash-grafana; do
  [[ $(yq "$isdash | .metadata.name" "$tmp/$f.yaml" | grep -vc '^---$') == "$files" ]] || fail "mon_23: $f has not $files dashboard ConfigMaps"
  for json in "$chart"/dashboards/*.json; do
    base=$(basename "$json" .json)
    cm=$(yq "$isdash | select(.metadata.name == \"mimic-dashboard-$base\")" "$tmp/$f.yaml")
    [[ -n $cm ]] || fail "mon_23: $f has no ConfigMap mimic-dashboard-$base"
    [[ $(yq -r '.metadata.namespace' <<<"$cm") == mimic-system && $(yq -r '.metadata.labels["app.kubernetes.io/part-of"]' <<<"$cm") == mimic \
      && $(yq -r '.data | keys | join(",")' <<<"$cm") == "$base.json" ]] || fail "mon_23: $f, $base: namespace, labels or data key"
    cmp -s <(yq -r ".data[\"$base.json\"]" <<<"$cm") <(cat "$json"; echo) || fail "mon_23: $f, $base: the data differs from the file"
  done
done
# The annotations are on the dashboard ConfigMaps and on no other object; the two renders differ by them only.
[[ $(yq 'select(.metadata.annotations.grafana_folder == "Mimic") | .metadata.name' "$tmp/dash-on.yaml" | grep -vc '^---$') == "$files" \
  && $(yq "$isdash | select(.metadata.annotations.grafana_folder == \"Mimic\") | .metadata.name" "$tmp/dash-on.yaml" | grep -vc '^---$') == "$files" ]] \
  || fail "mon_23: monitoring.dashboards.annotations are not on the dashboard ConfigMaps only"
notdash='select(((.metadata.labels.grafana_dashboard // "") == "1") | not)'
cmp -s <(yq "$notdash" "$tmp/dash-on.yaml") <(yq "$notdash" "$tmp/dash-off.yaml") \
  || fail "mon_23: dashboards.enabled changes more than the dashboard ConfigMaps"
# A dashboard changed in a copy of the chart: the Grafana checksum does not move.
cp -r "$chart" "$tmp/chart-copy"
# A change inside the JSON (the title), not trailing whitespace: YAML block chomping drops a trailing blank line.
sed -i 's/"title": "Mimic \/ Controller"/"title": "Mimic \/ Controller changed"/' "$tmp/chart-copy/dashboards/mimic-controller.json"
grep -q 'Mimic / Controller changed' "$tmp/chart-copy/dashboards/mimic-controller.json" || fail "mon_23: the copy was not changed"
helm template mimic "$tmp/chart-copy" -n mimic-system --set valkey.existingSecret=golden --set monitoring.grafana.enabled=true > "$tmp/dash-copy.yaml"
[[ $(gcs "$tmp/dash-grafana.yaml") == "$(gcs "$tmp/dash-copy.yaml")" ]] || fail "mon_23: a dashboard change moves the Grafana checksum"
cmp -s <(yq "$isdash" "$tmp/dash-copy.yaml") <(yq "$isdash" "$tmp/dash-grafana.yaml") && fail "mon_23: the changed copy renders the same dashboard"
echo "mon_22, mon_23: the dashboards pass the checker and render as one ConfigMap per file"

# #438 (CHT, spec §12.18): the production overlay lints strictly; its render is Pod Security restricted, has requests and
# limits on every container, digests on every image and the shape of CHT-05; and the file holds no secret.
overlay=$chart/values-production.yaml
[[ -f $overlay ]] || fail "cht_01: $overlay does not exist"
lint production -f "$overlay"

helm template mimic "$chart" -n mimic-system --set valkey.existingSecret=golden -f "$overlay" > "$tmp/prod.yaml" \
  || fail "cht_02: the overlay does not render"
# docs: the rendered objects as one JSON array; pods: the pod specs of the Deployments and StatefulSets.
docs=$(yq -o=json -I0 '.' "$tmp/prod.yaml" | jq -cs '[.[] | select(. != null)]')
pods=$(jq -c '[.[] | select(.kind == "Deployment" or .kind == "StatefulSet") | .spec.template.spec]' <<<"$docs")
# obj <kind> <name>: the rendered object, as JSON, or nothing.
obj() { jq -c --arg k "$1" --arg n "$2" '.[] | select(.kind == $k and .metadata.name == $n)' <<<"$docs"; }

# CHT-02, Pod Security restricted: no host namespace or hostPath; runAsNonRoot and seccomp RuntimeDefault on each
# container (its own setting, else the pod's); allowPrivilegeEscalation false, drop ALL, no added capability.
violations=$(jq -r '
  def eff($sc; $p; $f): if ($sc | has($f)) then $sc[$f] else ($p.securityContext[$f]) end;
  .[] | . as $p
  | (if ($p.hostNetwork // false) or ($p.hostPID // false) or ($p.hostIPC // false) then "host namespace" else empty end),
    ((.volumes // [])[] | select(has("hostPath")) | "hostPath volume \(.name)"),
    (((.containers // []) + (.initContainers // []))[] | . as $c | ($c.securityContext // {}) as $sc
      | (if eff($sc; $p; "runAsNonRoot") == true then empty else "\($c.name): not runAsNonRoot" end),
        (if (eff($sc; $p; "seccompProfile") // {}).type == "RuntimeDefault" then empty else "\($c.name): seccomp is not RuntimeDefault" end),
        (if $sc.allowPrivilegeEscalation == false then empty else "\($c.name): allowPrivilegeEscalation is not false" end),
        (if (($sc.capabilities.drop // []) | index("ALL")) != null then empty else "\($c.name): capabilities are not dropped ALL" end),
        (if (($sc.capabilities.add // []) | length) == 0 then empty else "\($c.name): a capability is added" end))
' <<<"$pods")
[[ -z $violations ]] || fail "cht_02: Pod Security restricted: $(tr '\n' ';' <<<"$violations")"
echo "cht_02: every pod of the overlay render is Pod Security restricted"

# CHT-03: requests (cpu, memory) and a memory limit on every container and init container; a cpu limit on the
# controller, Valkey, exporter and the injected sidecar.
missing=$(jq -r '.[] | ((.containers // []) + (.initContainers // []))[]
  | select(.resources.requests.cpu == null or .resources.requests.memory == null or .resources.limits.memory == null) | .name' <<<"$pods")
[[ -z $missing ]] || fail "cht_03: containers without requests or a memory limit: $(tr '\n' ' ' <<<"$missing")"
nocpu=$(jq -r '.[] | (.containers // [])[] | select(.name == "controller" or .name == "valkey" or .name == "exporter")
  | select(.resources.limits.cpu == null) | .name' <<<"$pods")
[[ -z $nocpu ]] || fail "cht_03: no cpu limit on: $(tr '\n' ' ' <<<"$nocpu")"
sidecar=$(jq -r '.[] | select(.kind == "Deployment" and .metadata.name == "mimic-controller") | .spec.template.spec.containers[0].env[]
  | select(.name == "MIMIC_INJECT_SIDECAR_RESOURCES") | .value' <<<"$docs")
jq -e '.requests.cpu and .requests.memory and .limits.cpu and .limits.memory' <<<"$sidecar" >/dev/null \
  || fail "cht_03: MIMIC_INJECT_SIDECAR_RESOURCES lacks requests or limits: $sidecar"
echo "cht_03: every container has requests and a memory limit; the controller, Valkey, exporter and sidecar have a cpu limit"

# CHT-04: every image of the render and the two MIMIC_INJECT_* images is repository@sha256:<64 hex>, and the overlay pins both.
digest_re='^sha256:[0-9a-f]{64}$'
[[ $(jq '[.. | objects | select(has("image"))] | length' <<<"$docs") -gt 0 ]] || fail "cht_04: the render has no image"
badimg=$(jq -r '.. | objects | select(has("image")) | .image | select(type == "string" and test("@sha256:[0-9a-f]{64}$") | not)' <<<"$docs")
[[ -z $badimg ]] || fail "cht_04: an image is not pinned by digest: $badimg"
badenv=$(jq -r '.. | objects | select(.name? == "MIMIC_INJECT_SIDECAR_IMAGE" or .name? == "MIMIC_INJECT_INIT_IMAGE") | .value
  | select(test("@sha256:[0-9a-f]{64}$") | not)' <<<"$docs")
[[ -z $badenv ]] || fail "cht_04: an injected image is not pinned by digest: $badenv"
[[ $(yq -r '.image.digest' "$overlay") =~ $digest_re && $(yq -r '.inject.initImage.digest' "$overlay") =~ $digest_re ]] \
  || fail "cht_04: the overlay does not pin image.digest and inject.initImage.digest"
echo "cht_04: every image of the overlay render is pinned by digest"

# CHT-05: the shape of the production install.
jq -e '.spec.replicas == 3' <<<"$(obj Deployment mimic-controller)" >/dev/null || fail "cht_05: the controller does not run 3 replicas"
spread=$(jq -c '.spec.template.spec.topologySpreadConstraints // []' <<<"$(obj Deployment mimic-controller)")
jq -e 'length == 2 and ([.[].topologyKey] | sort) == ["kubernetes.io/hostname", "topology.kubernetes.io/zone"]
  and all(.[]; .maxSkew == 1 and .whenUnsatisfiable == "ScheduleAnyway" and .labelSelector.matchLabels["app.kubernetes.io/name"] == "mimic-controller")' \
  <<<"$spread" >/dev/null || fail "cht_05: the controller topology spread is not zone and hostname, maxSkew 1, ScheduleAnyway: $spread"
jq -e '.spec.maxUnavailable == 1' <<<"$(obj PodDisruptionBudget mimic-controller)" >/dev/null \
  || fail "cht_05: the PodDisruptionBudget mimic-controller is not maxUnavailable 1"
jq -e '.spec.template.spec.priorityClassName == "mimic-critical"' <<<"$(obj Deployment mimic-controller)" >/dev/null \
  || fail "cht_05: the controller has no priorityClassName mimic-critical"
jq -e '.spec.template.spec.priorityClassName == "mimic-critical"' <<<"$(obj StatefulSet mimic-valkey)" >/dev/null \
  || fail "cht_05: Valkey has no priorityClassName mimic-critical"
[[ -n $(obj ServiceMonitor mimic-controller) ]] || fail "cht_05: no ServiceMonitor mimic-controller"
[[ -n $(obj PodMonitor mimic-sidecar) && -n $(obj PodMonitor mimic-valkey) ]] || fail "cht_05: no PodMonitor of the sidecar and of Valkey"
[[ -n $(obj StatefulSet mimic-valkey | jq -c '.spec.template.spec.containers[] | select(.name == "exporter")') ]] \
  || fail "cht_05: the Valkey exporter is not enabled"
bundled=$(jq -r '[.[] | .metadata.name? // empty | select(test("^mimic-(prometheus|grafana)"))] | join(" ")' <<<"$docs")
[[ -z $bundled ]] || fail "cht_05: a bundled monitoring object is rendered: $bundled"
[[ -z $(obj CustomResourceDefinition mimicloadtests.mimic.platform.io) ]] || fail "cht_05: the MimicLoadTest CRD is rendered with loadTest off"
[[ -z $(jq -c '.[] | select(.kind == "ValidatingAdmissionPolicy" and .metadata.name == "mimic-load-job")' <<<"$docs") ]] \
  || fail "cht_05: the load test admission policy is rendered with loadTest off"
jq -e '.spec.ingress[0].from | length == 1 and (.[0].ipBlock.cidr | type == "string")' \
  <<<"$(obj NetworkPolicy mimic-controller)" >/dev/null || fail "cht_05: networkPolicy.webhookFrom is not one ipBlock"
[[ $(jq -r '.[] | select(.kind == "Deployment" and .metadata.name == "mimic-controller") | .spec.template.spec.containers[0].env[] | select(.name == "MIMIC_LOG") | .value' <<<"$docs") == info ]] \
  || fail "cht_05: the controller MIMIC_LOG is not info"
[[ $(jq -r '.[] | select(.kind == "Deployment" and .metadata.name == "mimic-controller") | .spec.template.spec.containers[0].env[] | select(.name == "MIMIC_INJECT_SIDECAR_LOG") | .value' <<<"$docs") == info ]] \
  || fail "cht_05: MIMIC_INJECT_SIDECAR_LOG is not info"
[[ $(jq '[.[] | select(.kind == "StatefulSet" and .metadata.name == "mimic-valkey") | .spec.volumeClaimTemplates | length] | .[0]' <<<"$docs") == 1 ]] \
  || fail "cht_05: Valkey has no persistent volume claim"
echo "cht_05: 3 spread controllers with a PDB, priority class, monitors, no bundled monitoring, no load test, webhook narrowed"

# CHT-06: no secret in the file: no non-empty scalar at a key named like a secret, and no key material or token.
secrets=$(yq -o=json '.' "$overlay" | jq -r '[paths(scalars) as $p | {k: ($p | map(tostring) | join(".")), v: getpath($p)}]
  | .[] | select(.k | test("pass|token|secret|credential|private"; "i")) | select(.v != "" and .v != null and .v != false) | .k')
[[ -z $secrets ]] || fail "cht_06: values-production.yaml sets a secret-like value: $secrets"
if grep -qE -- '-----BEGIN|AKIA|ghp_|sk_live_' "$overlay"; then fail "cht_06: values-production.yaml holds key or token material"; fi
echo "cht_06: values-production.yaml holds no secret"

# UI-14 (#437, spec §12.7): the controller container declares mimic-ui on 8080/TCP, for information only. The read API
# stays on loopback (no MIMIC_UI_ADDR in the chart), and no Service or NetworkPolicy names the port or uses 8080.
# The 403 for a non-loopback peer is CTL-13 (ctl_13_a_peer_that_is_not_loopback_gets_403), a Rust test.
ui_ctr=$(yq 'select(.kind == "Deployment" and .metadata.name == "mimic-controller") | .spec.template.spec.containers[] | select(.name == "controller")' "$tmp/default.yaml")
[[ -n $ui_ctr ]] || fail "ui_14: no controller container in the default render"
[[ $(printf '%s\n' "$ui_ctr" | yq -r '[.ports[] | select(.name == "mimic-ui")] | length') == 1 ]] \
  || fail "ui_14: the controller container does not declare exactly one port named mimic-ui"
[[ $(printf '%s\n' "$ui_ctr" | yq -r '.ports[] | select(.name == "mimic-ui") | .containerPort') == 8080 ]] \
  || fail "ui_14: mimic-ui is not containerPort 8080"
[[ $(printf '%s\n' "$ui_ctr" | yq -r '.ports[] | select(.name == "mimic-ui") | .protocol') == TCP ]] \
  || fail "ui_14: mimic-ui is not TCP"
[[ $(printf '%s\n' "$ui_ctr" | yq -r '[.env[]? | select(.name == "MIMIC_UI_ADDR")] | length') == 0 ]] \
  || fail "ui_14: the chart sets MIMIC_UI_ADDR (the read API must stay on loopback by default)"
ui_exposed=$(yq 'select(.kind == "Service" or .kind == "NetworkPolicy")' "$tmp/default.yaml")
! grep -q 'mimic-ui' <<<"$ui_exposed" || fail "ui_14: a Service or NetworkPolicy names mimic-ui"
[[ $(yq -r '[.. | select(tag == "!!int" and . == 8080)] | length' <<<"$ui_exposed" | awk '{ s += $1 } END { print s + 0 }') == 0 ]] \
  || fail "ui_14: a Service or NetworkPolicy uses port 8080"
echo "ui_14: the controller declares mimic-ui on 8080/TCP; no Service or NetworkPolicy exposes it"

echo "chart checks passed"
