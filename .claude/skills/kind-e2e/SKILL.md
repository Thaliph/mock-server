---
name: kind-e2e
description: Run, debug or extend mimic's end-to-end tests on a local kind cluster (scripts/e2e.sh) - building the image, loading it, installing the Helm chart and checking the cluster. Use when a change touches the image, the chart, the webhook or anything only observable in a cluster, or when CI's e2e job fails.
---

# kind end-to-end tests

Tools: `kind`, `kubectl`, `helm`, `docker` (install with `mise use -g kind helm kubectl` if
missing). The user's account is in the `docker` group.

## Run

```bash
scripts/e2e.sh                       # build image, create/reuse cluster mimic-e2e, install, check
SKIP_BUILD=1 IMAGE=ghcr.io/thaliph/mimic:dev scripts/e2e.sh   # reuse an image already built
KEEP_CLUSTER=0 scripts/e2e.sh        # delete the cluster at the end (CI default)
scripts/check-image.sh ghcr.io/thaliph/mimic:dev               # image-only checks (FND-07)
```

The script is idempotent: rerunning it upgrades the release in place. Locally the cluster is
kept for debugging; `kind delete cluster --name mimic-e2e` removes it.

## Debug

```bash
kubectl -n mimic-system get pods -o wide
kubectl -n mimic-system describe pod -l app.kubernetes.io/name=mimic-controller
kubectl -n mimic-system logs deploy/mimic-controller | jq -c 'select(.level != "DEBUG")'
kubectl get events -A --sort-by=.lastTimestamp | tail -20
```

A pod refused with `violates PodSecurity "restricted"` means a chart change broke the
security baseline (spec §11): fix the chart, never relax the namespace label.

## Extend

Add checks to `scripts/e2e.sh` as a `log "<id>: …"` block that calls `fail "<id>: …"` on error,
so the criterion ID shows in CI logs. Keep each block independent of the order of later
blocks, and prefer integration tests in `crates/*/tests` when the behaviour is observable
without a cluster: e2e minutes are the slowest part of CI.
