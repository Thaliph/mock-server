#!/usr/bin/env bash
# #455, spec §12.29 (ADM-68, ADM-69): the KEDA example of deploy/autoscaling/. Downloads the pinned `keda` chart
# archive, refuses it unless its SHA-256 is the pinned one, renders it with keda-values.yaml (helm template, no
# cluster) and checks the render and the two ScaledObject files. Exit 0 when all hold, 1 on the first failure.
#   KEDA_ARCHIVE=<file> uses a local archive instead of downloading it (it is still checked).
set -euo pipefail
cd "$(dirname "$0")/.."

dir=deploy/autoscaling
version=2.21.1
sha256=76b45621e48e18c9872cdc0c46295e4cbac1236fd379d06bca38ef91d8fb883e
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fail() { echo "FAIL $*" >&2; exit 1; }

# The archive, by its SHA-256.
archive=${KEDA_ARCHIVE:-$tmp/keda-$version.tgz}
if [[ -z ${KEDA_ARCHIVE:-} ]]; then
  curl -fsSL --retry 3 -o "$archive" "https://kedacore.github.io/charts/keda-$version.tgz" || fail "adm_68: cannot download the keda chart $version"
fi
[[ $(sha256sum "$archive" | cut -d' ' -f1) == "$sha256" ]] || fail "adm_68: the keda chart archive is not the pinned $sha256"
grep -qF "$sha256" docs/SPEC.md || fail "adm_68: docs/SPEC.md does not name the archive SHA-256"
grep -qF "keda-$version.tgz" $dir/keda-values.yaml && grep -qF "$sha256" $dir/keda-values.yaml \
  || fail "adm_68: keda-values.yaml does not give the archive check"

helm template keda "$archive" -n keda -f $dir/keda-values.yaml > "$tmp/keda.yaml" || fail "adm_68: the pinned chart does not render"
docs=$(yq -o=json -I0 '.' "$tmp/keda.yaml" | jq -cs '[.[] | select(. != null)]')

# ADM-68: one APIService, verified by the API server.
jq -e '[.[] | select(.kind == "APIService")] | length == 1 and (.[0].metadata.name == "v1beta1.external.metrics.k8s.io")
  and (.[0].spec | has("insecureSkipTLSVerify") | not)' <<<"$docs" >/dev/null \
  || fail "adm_68: not exactly one APIService v1beta1.external.metrics.k8s.io without insecureSkipTLSVerify"
echo "adm_68: one APIService, no insecureSkipTLSVerify"

# ADM-68: three Deployments, with the images of images/Dockerfile (tag and 64-hex digest).
want=$(for s in keda keda-metrics-apiserver keda-admission-webhooks; do scripts/pinned-image.sh "$s"; done | sort)
got=$(jq -r '.[] | select(.kind == "Deployment") | .spec.template.spec | (.containers + (.initContainers // []))[] | .image' <<<"$docs" | sort)
[[ $got == "$want" ]] || fail "adm_68: the Deployment images are not the images/Dockerfile stages: $(tr '\n' ' ' <<<"$got")"
[[ $(jq '[.[] | select(.kind == "Deployment")] | length' <<<"$docs") == 3 ]] || fail "adm_68: not three Deployments"
grep -qE '^FROM ghcr.io/kedacore/keda[a-z-]*:'"$version"'@sha256:[0-9a-f]{64} AS ' images/Dockerfile || fail "adm_68: images/Dockerfile does not pin KEDA $version by digest"
[[ $(jq -r '[.[] | select(.kind == "Deployment") | .spec.template.spec.containers[].image | select(test(":[0-9][^@]*@sha256:[0-9a-f]{64}$"))] | length' <<<"$docs") == 3 ]] \
  || fail "adm_68: an image is not by tag and 64-hex digest"
echo "adm_68: the three images are the images/Dockerfile stages, by digest"

# ADM-68: Pod Security restricted, requests and a memory limit on every container.
pods=$(jq -c '[.[] | select(.kind == "Deployment") | .spec.template.spec]' <<<"$docs")
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
        (if (($sc.capabilities.add // []) | length) == 0 then empty else "\($c.name): a capability is added" end),
        (if $sc.readOnlyRootFilesystem == true then empty else "\($c.name): root filesystem is not read-only" end),
        (if ($c.resources.requests.cpu and $c.resources.requests.memory and $c.resources.limits.memory) then empty else "\($c.name): no requests or memory limit" end))
' <<<"$pods")
[[ -z $violations ]] || fail "adm_68: pods: $(tr '\n' ';' <<<"$violations")"
echo "adm_68: every pod is Pod Security restricted, with requests and a memory limit"

# ADM-68: RBAC. No ClusterRole rule on secrets, a * resource or a * API group; the only Role on secrets is in keda;
# no aggregation label.
bad=$(jq -r '.[] | select(.kind == "ClusterRole") | . as $r | (.rules // [])[]
  | select(((.resources // []) | any(. == "secrets" or . == "*" or startswith("secrets/"))) or ((.apiGroups // []) | any(. == "*")))
  | "\($r.metadata.name): \(. | tojson)"' <<<"$docs")
[[ -z $bad ]] || fail "adm_68: a ClusterRole rule names secrets or *: $bad"
bad=$(jq -r '.[] | select(.kind == "Role") | select(.metadata.namespace != "keda") | select(any(.rules[]?; (.resources // []) | any(. == "secrets"))) | .metadata.name' <<<"$docs")
[[ -z $bad ]] || fail "adm_68: a Role outside keda grants secrets: $bad"
[[ $(jq '[.[] | select(.kind == "Role" and any(.rules[]?; (.resources // []) | any(. == "secrets")))] | length' <<<"$docs") -ge 1 ]] \
  || fail "adm_68: no Role grants secrets in keda (the operator needs its own)"
bad=$(jq -r '.[] | select(.kind == "ClusterRole") | select((.metadata.labels // {}) | keys | any(startswith("rbac.authorization.k8s.io/aggregate-to-"))) | .metadata.name' <<<"$docs")
[[ -z $bad ]] || fail "adm_68: a ClusterRole carries an aggregate-to label: $bad"
[[ $(jq '[.[] | select(.kind == "ClusterRole" and (.aggregationRule != null))] | length' <<<"$docs") == 0 ]] || fail "adm_68: a ClusterRole has an aggregationRule"
echo "adm_68: no ClusterRole rule on secrets or *, secrets only by a Role in keda, no aggregation"

# ADM-68: a NetworkPolicy selects the pods of each Deployment.
np=$(jq -r '
  [.[] | select(.kind == "NetworkPolicy") | .spec.podSelector.matchLabels] as $sel
  | .[] | select(.kind == "Deployment") | . as $d
  | select([$sel[] | . as $m | ($m != null and ($m | to_entries | all(. as $e | $d.spec.template.metadata.labels[$e.key] == $e.value)))] | any | not)
  | .metadata.name' <<<"$docs")
[[ -z $np ]] || fail "adm_68: no NetworkPolicy selects the pods of: $np"
echo "adm_68: a NetworkPolicy selects each Deployment's pods"

# ADM-68 (#564): each Deployment's pods are selected by exactly one NetworkPolicy, with exactly the chart's rules, in order,
# and then one rule of TCP 443 and 6443 with no peer (keda-values.yaml). Ports of a rule compare as a set.
expected_np=$(cat <<'JSON'
{
  "keda-operator": {
    "ingress": [{"from": [{"namespaceSelector": {}}], "ports": [{"protocol": "TCP", "port": 9666}]}],
    "egress": [
      {"ports": [{"protocol": "UDP", "port": 53}, {"protocol": "TCP", "port": 53}]},
      {"to": [{"namespaceSelector": {}}], "ports": [{"protocol": "TCP", "port": 443}, {"protocol": "TCP", "port": 6443}]},
      {"to": [{"namespaceSelector": {}}]},
      {"ports": [{"protocol": "TCP", "port": 443}, {"protocol": "TCP", "port": 6443}]}
    ]
  },
  "keda-operator-metrics-apiserver": {
    "ingress": [{"ports": [{"protocol": "TCP", "port": 6443}]}],
    "egress": [
      {"ports": [{"protocol": "UDP", "port": 53}, {"protocol": "TCP", "port": 53}]},
      {"to": [{"namespaceSelector": {}}], "ports": [{"protocol": "TCP", "port": 443}, {"protocol": "TCP", "port": 6443}]},
      {"to": [{"podSelector": {"matchLabels": {"app": "keda-operator"}}}], "ports": [{"protocol": "TCP", "port": 9666}]},
      {"to": [{"namespaceSelector": {}}]},
      {"ports": [{"protocol": "TCP", "port": 443}, {"protocol": "TCP", "port": 6443}]}
    ]
  },
  "keda-admission-webhooks": {
    "ingress": [{"ports": [{"protocol": "TCP", "port": 9443}]}],
    "egress": [
      {"ports": [{"protocol": "UDP", "port": 53}, {"protocol": "TCP", "port": 53}]},
      {"to": [{"namespaceSelector": {}}], "ports": [{"protocol": "TCP", "port": 443}, {"protocol": "TCP", "port": 6443}]},
      {"to": [{"namespaceSelector": {}}]},
      {"ports": [{"protocol": "TCP", "port": 443}, {"protocol": "TCP", "port": 6443}]}
    ]
  }
}
JSON
)
bad=$(jq -r --argjson want "$expected_np" '
  def norm: map(if has("ports") then .ports |= sort_by(.port, .protocol) else . end);
  . as $all
  | [.[] | select(.kind == "Deployment")] | .[] | . as $d | $d.metadata.name as $n
  | [$all[] | select(.kind == "NetworkPolicy")
      | select(.spec.podSelector.matchLabels as $m | $m != null and ($m | to_entries | all(. as $e | $d.spec.template.metadata.labels[$e.key] == $e.value)))] as $nps
  | if ($want[$n] == null) then "unexpected Deployment \($n)"
    elif ($nps | length) != 1 then "\($n): \($nps | length) NetworkPolicies select its pods"
    elif $nps[0].spec.policyTypes != ["Ingress", "Egress"] then "\($n): policyTypes"
    elif ($nps[0].spec.ingress | norm) != ($want[$n].ingress | norm) then "\($n): ingress rules"
    elif ($nps[0].spec.egress | norm) != ($want[$n].egress | norm) then "\($n): egress rules"
    else empty end' <<<"$docs")
[[ -z $bad ]] || fail "adm_68: NetworkPolicy rules differ from the chart's plus the API server rule: $bad"
echo "adm_68: each NetworkPolicy has exactly the chart's rules and the one TCP 443/6443 rule with no peer"

# ADM-68 (#562): no rule grants any verb on the admission policies, so a compromised operator cannot remove keda-auth-policy.yaml.
bad=$(jq -r '.[] | select(.kind == "ClusterRole" or .kind == "Role") | . as $r | (.rules // [])[]
  | select((.resources // []) | any(. == "validatingadmissionpolicies" or . == "validatingadmissionpolicybindings" or . == "*"))
  | "\($r.metadata.name)"' <<<"$docs")
[[ -z $bad ]] || fail "adm_68: a rule grants a right on validatingadmissionpolicies or their bindings: $bad"
echo "adm_68: no KEDA rule on validatingadmissionpolicies or their bindings"

# ADM-73 (#562): keda-auth-policy.yaml.
pol=$dir/keda-auth-policy.yaml
pdocs=$(yq -o=json -I0 '.' "$pol" | jq -cs '[.[] | select(. != null)]')
[[ $(jq 'length' <<<"$pdocs") == 2 ]] || fail "adm_73: $pol is not exactly one policy and one binding"
jq -e '[.[] | select(.apiVersion == "admissionregistration.k8s.io/v1" and .kind == "ValidatingAdmissionPolicy")] | length == 1' <<<"$pdocs" >/dev/null \
  || fail "adm_73: $pol is not exactly one ValidatingAdmissionPolicy"
jq -e '[.[] | select(.apiVersion == "admissionregistration.k8s.io/v1" and .kind == "ValidatingAdmissionPolicyBinding")] | length == 1' <<<"$pdocs" >/dev/null \
  || fail "adm_73: $pol is not exactly one ValidatingAdmissionPolicyBinding"
ext_address=$(yq -r 'select(.kind == "ScaledObject") | .spec.triggers[0].metadata.serverAddress' $dir/scaledobject-external.yaml)
jq -e --arg addr "$ext_address" '
  (.[] | select(.kind == "ValidatingAdmissionPolicy")) as $p | (.[] | select(.kind == "ValidatingAdmissionPolicyBinding")) as $b
  | $p.metadata.name == "mimic-prometheus-auth" and $b.metadata.name == "mimic-prometheus-auth"
  and $p.spec.failurePolicy == "Fail"
  and ($p.spec.matchConstraints.resourceRules | length) == 1
  and ($p.spec.matchConstraints.resourceRules[0] | .apiGroups == ["keda.sh"] and (.operations | sort) == ["CREATE", "UPDATE"]
       and (.resources | sort) == ["scaledjobs", "scaledobjects"])
  and ([$p.spec.variables[] | select(.name == "address")] | length == 1)
  and ([$p.spec.variables[] | select(.name == "address")][0].expression == "\"" + $addr + "\"")
  and ($addr | startswith("https://"))
  and ([$p.spec.variables[] | select(.name == "uses")][0].expression | contains("mimic-prometheus") and contains("clustertriggerauthentication") and contains("lowerAscii"))
  and ([$p.spec.validations[].expression] | join(" ") | contains("type") and contains("serverAddress") and contains("unsafeSsl") and contains("FromEnv"))
  and ($p.spec.validations | all(.messageExpression | contains("mimic-prometheus")))
  and $b.spec.policyName == "mimic-prometheus-auth" and $b.spec.validationActions == ["Deny"] and ($b.spec | has("paramRef") | not)' <<<"$pdocs" >/dev/null \
  || fail "adm_73: $pol: the policy or its binding is not the one of §10.7"
echo "adm_73: keda-auth-policy.yaml is one policy and one binding bound to the address of scaledobject-external.yaml"

# ADM-75 (#565): keda-scaledobject-editor.yaml.
yq -o=json -I0 '.' $dir/keda-scaledobject-editor.yaml | jq -es '
  length == 1 and .[0].kind == "ClusterRole" and .[0].metadata.name == "keda-scaledobject-editor"
  and (.[0].rules | length) == 1
  and (.[0].rules[0] | .apiGroups == ["keda.sh"] and .resources == ["scaledobjects"]
       and (.verbs | sort) == ["create", "delete", "get", "list", "patch", "update", "watch"])
  and (.[0] | has("aggregationRule") | not)
  and ((.[0].metadata.labels // {}) | keys | any(startswith("rbac.authorization.k8s.io/aggregate-to-")) | not)' >/dev/null \
  || fail "adm_75: keda-scaledobject-editor.yaml is not one ClusterRole with the one scaledobjects rule"
echo "adm_75: keda-scaledobject-editor.yaml is one ClusterRole with one scaledobjects rule"

# ADM-68, ADM-69: the three files hold no secret (CHT-06): no non-empty string at a key named like a secret, outside
# a secretTargetRef (a reference to a Secret, not a value), and no key material or token.
for f in $dir/keda-values.yaml $dir/scaledobject.yaml $dir/scaledobject-external.yaml $dir/keda-auth-policy.yaml $dir/keda-scaledobject-editor.yaml; do
  secrets=$(yq -o=json -I0 '.' "$f" | jq -rs '.[] | [paths(scalars) as $p | {k: ($p | map(tostring) | join(".")), v: getpath($p)}]
    | .[] | select(.k | test("secretTargetRef") | not) | select(.k | test("pass|token|secret|credential|private"; "i"))
    | select((.v | type) == "string" and .v != "") | .k')
  [[ -z $secrets ]] || fail "adm_68: $f sets a secret-like value: $secrets"
  ! grep -qE -- '-----BEGIN|AKIA|ghp_|sk_live_|eyJ[A-Za-z0-9_-]{10,}' "$f" || fail "adm_68: $f holds key or token material"
done
echo "adm_68: the files of deploy/autoscaling hold no secret"

# ADM-69: the ScaledObject files.
query='sum(mimic_admission_utilization{job="mimic-sidecar",namespace="shop",deployment="cart"})'
so_check() { # <file> <external: 0|1>
  local f=$1 ext=$2 so
  [[ $(yq -N '[select(.kind == "ScaledObject")] | length' "$f" | awk '{ s += $1 } END { print s + 0 }') == 1 ]] || fail "adm_69: $f is not exactly one ScaledObject"
  so=$(yq -N -o=json -I0 'select(.kind == "ScaledObject")' "$f")
  jq -e '.apiVersion == "keda.sh/v1alpha1" and .metadata.namespace == "shop"
    and .spec.scaleTargetRef.kind == "Deployment" and (.spec.scaleTargetRef.name | type == "string" and . != "")
    and .spec.minReplicaCount == 1 and (.spec.maxReplicaCount | type == "number") and .spec.maxReplicaCount >= .spec.minReplicaCount
    and ((.spec | has("fallback") or has("idleReplicaCount")) | not)' <<<"$so" >/dev/null \
    || fail "adm_69: $f: scale target, replica bounds, fallback or idleReplicaCount"
  jq -e --arg q "$query" '. as $o | .spec.triggers[0] as $t | (.spec.triggers | length) == 1 and $t.type == "prometheus"
    and $t.metricType == "AverageValue" and $t.metadata.threshold == "0.7" and $t.metadata.ignoreNullValues == "false"
    and $t.metadata.query == $q
    and ($t.metadata.query | contains("mimic_admission_utilization") and contains("job=\"mimic-sidecar\"")
      and contains("namespace=\"" + $o.metadata.namespace + "\"") and contains("deployment=\"" + $o.spec.scaleTargetRef.name + "\""))' <<<"$so" >/dev/null \
    || fail "adm_69: $f: the trigger"
  if [[ $ext == 1 ]]; then
    jq -e '.spec.triggers[0] | .metadata.authModes == "bearer" and (.metadata.serverAddress | startswith("https://"))
      and .authenticationRef.kind == "ClusterTriggerAuthentication" and .authenticationRef.name == "mimic-prometheus"' <<<"$so" >/dev/null \
      || fail "adm_69: $f: authModes, https serverAddress or authenticationRef"
    [[ $(yq -N '[select(.kind == "ClusterTriggerAuthentication")] | length' "$f" | awk '{ s += $1 } END { print s + 0 }') == 1 ]] \
      || fail "adm_69: $f: not exactly one ClusterTriggerAuthentication"
    yq -N -o=json -I0 'select(.kind == "ClusterTriggerAuthentication")' "$f" | jq -e '.metadata.name == "mimic-prometheus"
      and (.spec | keys == ["secretTargetRef"])
      and ([.spec.secretTargetRef[].parameter] | sort == ["bearerToken", "ca"])
      and (.spec.secretTargetRef | all(.name | type == "string") and all(.key | type == "string"))' >/dev/null \
      || fail "adm_69: $f: the ClusterTriggerAuthentication does not take exactly bearerToken and ca from secretTargetRef"
    ! grep -q customHeaders "$f" || fail "adm_69: $f has customHeaders"
  else
    jq -e '.spec.triggers[0].metadata.serverAddress == "http://mimic-prometheus.mimic-system.svc:9090" and (.spec.triggers[0] | has("authenticationRef") | not)' <<<"$so" >/dev/null \
      || fail "adm_69: $f: serverAddress is not the bundled Prometheus"
  fi
}
so_check $dir/scaledobject.yaml 0
so_check $dir/scaledobject-external.yaml 1
echo "adm_69: both ScaledObjects have the shape of §10.7"
