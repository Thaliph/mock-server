#!/usr/bin/env bash
# Checks of rendered Kubernetes objects and example files shared by scripts/check-chart.sh (CHT-02, CHT-06) and
# scripts/check-autoscaling.sh (ADM-68, ADM-69). Sourced, needs yq and jq.
# shellcheck shell=bash

# The key material and token prefixes that no checked file may hold.
TOKEN_RE='-----BEGIN|AKIA|ghp_|sk_live_|eyJ[A-Za-z0-9_-]{10,}'

# render_docs <file>: the YAML documents of the file as one JSON array.
render_docs() { yq -o=json -I0 '.' "$1" | jq -cs '[.[] | select(. != null)]'; }

# pss_violations: reads a JSON array of pod specs on stdin and prints one line per breach of Pod Security restricted (CHT-02):
# a host namespace or hostPath volume; per container (init containers too) no runAsNonRoot, a seccomp profile other than
# RuntimeDefault, allowPrivilegeEscalation not false, capabilities not dropped ALL, or an added capability. A container's
# own setting wins over the pod's.
pss_violations() {
  jq -r '
    def eff($sc; $p; $f): if ($sc | has($f)) then $sc[$f] else ($p.securityContext[$f]) end;
    .[] | . as $p
    | (if ($p.hostNetwork // false) or ($p.hostPID // false) or ($p.hostIPC // false) then "host namespace" else empty end),
      ((.volumes // [])[] | select(has("hostPath")) | "hostPath volume \(.name)"),
      (((.containers // []) + (.initContainers // []))[] | . as $c | ($c.securityContext // {}) as $sc
        | (if eff($sc; $p; "runAsNonRoot") == true then empty else "\($c.name): not runAsNonRoot" end),
          (if (eff($sc; $p; "seccompProfile") // {}).type == "RuntimeDefault" then empty else "\($c.name): seccomp is not RuntimeDefault" end),
          (if $sc.allowPrivilegeEscalation == false then empty else "\($c.name): allowPrivilegeEscalation is not false" end),
          (if (($sc.capabilities.drop // []) | index("ALL")) != null then empty else "\($c.name): capabilities are not dropped ALL" end),
          (if (($sc.capabilities.add // []) | length) == 0 then empty else "\($c.name): a capability is added" end))'
}

# secret_keys <file> [ignore-regex]: the key paths of the file (every document) whose name looks like a secret (pass, token,
# secret, credential, private) and that hold a non-empty string or number (a boolean cannot be a secret). A path that matches ignore-regex is skipped
# (for example a secretTargetRef, which names a Secret and holds no value). Empty output means none.
secret_keys() {
  yq -o=json -I0 '.' "$1" | jq -rs --arg ign "${2:-}" '.[] | [paths(scalars) as $p | {k: ($p | map(tostring) | join(".")), v: getpath($p)}]
    | .[] | select($ign == "" or (.k | test($ign) | not)) | select(.k | test("pass|token|secret|credential|private"; "i"))
    | select(.v != "" and .v != null and (.v | type) != "boolean") | .k'
}

# has_token_material <file>: succeeds when the file holds key material or a token ($TOKEN_RE).
has_token_material() { grep -qE -- "$TOKEN_RE" "$1"; }
