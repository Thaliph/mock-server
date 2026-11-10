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
lint loadTest.podMonitor --set loadTest.enabled=true --set image.digest="$digest" --set metrics.podMonitor.enabled=true
# #379 (MON-14): the bundled monitoring lints strictly, alone and with the monitors and the load test.
lint prometheus --set monitoring.prometheus.enabled=true
lint prometheus.namespaces --set monitoring.prometheus.enabled=true --set 'monitoring.prometheus.namespaces[0]=apps'
lint prometheus.monitors --set monitoring.prometheus.enabled=true --set metrics.serviceMonitor.enabled=true \
  --set metrics.podMonitor.enabled=true --set valkey.metrics.enabled=true
lint prometheus.loadTest --set monitoring.prometheus.enabled=true --set loadTest.enabled=true \
  --set image.digest="$digest" --set metrics.podMonitor.enabled=true

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

echo "chart checks passed"
