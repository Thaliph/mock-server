# Developing

Audience: contributors. Rules come from `CLAUDE.md`; the reference is `docs/SPEC.md`.

## Workflow: spec first, then tests, then code

1. **Spec.** Any change of behaviour goes into `docs/SPEC.md` first. Acceptance criteria per feature
   are in §12 (IDs such as `FND-02`, `CTL-20`).
2. **Tests.** Tests are named after the criterion ID (`fnd_02_...`) and are written and seen failing
   before the implementation (TDD).
3. **Code.**
4. One GitHub issue per feature; never push to `main`; merge only on green CI (`ci-ok`) and an
   approving review. Everything is in English: code, comments, docs, commits, issues.

Rules that shape the code:

- `mimic-core` stays pure: no tokio, no I/O, no clock. Process plumbing lives in `mimic-runtime`;
  binaries are thin adapters.
- Deterministic output: `BTreeMap`/`BTreeSet`, never `HashMap` in anything serialised.
- 12-factor: configuration from `MIMIC_*` only (invalid value, exit 2), JSON logs on stdout, stateless
  processes (state in Valkey), graceful `SIGTERM`.
- Security first: no secret in logs, redaction before anything reaches Valkey, distroless non-root
  images, pinned actions and images.
- No traces: logs and metrics only.
- Free tier only: public repo, GitHub-hosted runners, GHCR.

## Repository layout

| Path | Content |
|---|---|
| `docs/SPEC.md` | The specification |
| `docs/wiki/` | This wiki (published to the GitHub wiki) |
| `crates/mimic-core` | Pure logic: HTTP/1 codec, TLS decisions, discovery (templating, schemas, redaction, merge), injection patch |
| `crates/mimic-runtime` | Configuration, logs, metrics, health, shutdown |
| `crates/mimic-sidecar` | The proxy and the discovery recorder / shipper |
| `crates/mimic-controller` | Webhook, `MimicWorkload` cache, sidecar API, Valkey store, read API, status |
| `crates/mimic-init`, `crates/mimic-trust` | Init containers |
| `deploy/helm/mimic` | The Helm chart |
| `images/`, `Dockerfile` | Pinned images (controller/sidecar, init, e2e app) |
| `scripts/` | `e2e.sh`, `e2e/apps/` (Python test services), pin and cooldown checks |
| `fuzz/` | `cargo-fuzz` targets (`http1`, `discovery`) |
| `.claude/` | Agent team (`architect`, `rust-dev`, `reviewer`, `ops`) and skills |

## Build and test

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.99.1 with rustfmt and clippy).

```bash
cargo build --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Run all three checks before pushing. Chart changes: `helm lint --strict deploy/helm/mimic`. Workflow
changes: `zizmor --offline .github/workflows`. Image, chart or cluster changes: `scripts/e2e.sh`.

### Tests that need Valkey

The store, push API and read API integration tests use a real Valkey 9.0 or later whose URL is in
`MIMIC_TEST_VALKEY_URL`; without it they panic with a message saying so. CI uses a service container
with the digest pinned in `images/Dockerfile`.

```bash
docker run -d --rm --name mimic-test-valkey -p 6379:6379 \
  "$(scripts/pinned-image.sh valkey)"
MIMIC_TEST_VALKEY_URL=redis://127.0.0.1:6379 cargo test --workspace
docker stop mimic-test-valkey
```

`scripts/pinned-image.sh valkey` prints the pinned reference (tag and digest). The tests use no
password.

## End-to-end on kind

Needs `kind`, `kubectl`, `helm`, `docker` and `jq`.

```bash
scripts/e2e.sh                                  # build images, create/reuse cluster "mimic-e2e", install, check
SKIP_BUILD=1 IMAGE=ghcr.io/thaliph/mimic:dev scripts/e2e.sh   # reuse an image
KEEP_CLUSTER=0 scripts/e2e.sh                   # delete the cluster afterwards (CI default)
kind delete cluster --name mimic-e2e            # clean up
```

It is idempotent. Each block is `log "<id>: ..."` and fails with `fail "<id>: ..."` so the criterion
ID appears in CI logs. The script exercises the same `users` and `front` services as the
[Quick start](Quick-start). Prefer an integration test in `crates/*/tests` when a behaviour is
observable without a cluster: e2e minutes are the slowest part of CI.

## Fuzzing

`fuzz/fuzz_targets/http1.rs` and `discovery.rs` (needs `cargo-fuzz`); CI runs them on a schedule.

## CI

`ci-ok` aggregates the stages: lint, unit and integration tests, image build and scan, kind e2e, a
chart stage, and a supply-chain stage (`cargo deny`, pins, cooldown checks). Release artefacts are
multi-arch, signed with cosign, with SBOM and provenance.

## Writing the wiki

Pages are Markdown in `docs/wiki/`, GitHub-wiki style: file names like `Quick-start.md`, links written
`[Quick start](Quick-start)`. Document only what is merged; state the feature level; link to the spec
rather than copying it.
