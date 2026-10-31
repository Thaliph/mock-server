# Instructions for Claude Code

- The spec is the source of truth: `docs/SPEC.md`. Any change of behaviour goes into the spec
  first, then tests, then code.
- Features are delivered with the `deliver-feature` skill and the agents in `.claude/agents/`
  (architect, rust-dev, reviewer, ops). One GitHub issue per feature.
- TDD: tests named after the acceptance criterion IDs of `docs/SPEC.md` §12 (`fnd_02_…`) are
  written and seen failing before the implementation.
- `mimic-core` stays pure: no tokio, no I/O, no clock. Process plumbing lives in
  `mimic-runtime`; binaries are thin adapters.
- Deterministic outputs: `BTreeMap`/`BTreeSet`, never `HashMap` in anything serialised.
- 12-factor: configuration from `MIMIC_*` environment variables only (invalid value → exit 2),
  JSON logs on stdout, stateless processes (state in Valkey), graceful `SIGTERM`.
- Security first: no secret in logs; nothing sensitive reaches Valkey without redaction;
  distroless non-root images; pinned actions and images.
- No traces (logs and metrics only). Mesh traffic management is v2.
- Before pushing: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`; chart changes: `helm lint --strict deploy/helm/mimic`; workflow
  changes: `zizmor --offline .github/workflows`; image/chart/cluster changes: `scripts/e2e.sh`.
- Never push to `main`; merge only on green CI (`ci-ok`) and an approving review.
- English everywhere: code, comments, docs, commits, issues.
- Free tier only: public repo, GitHub-hosted runners, GHCR.
