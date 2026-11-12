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

### Local image builds and the build cache

The build stage of the `Dockerfile` keeps the Rust target in a cache mount (`/src/target`) that every
local build on the daemon shares: there is no per-checkout key. Cargo decides freshness by mtime, and
`COPY` keeps the checkout's mtimes, so a build could reuse artifacts another source tree left in the
cache (#496). The build therefore touches every file under `crates/` to the build time right before
`cargo build`. Sources are then never older than a cached artifact.

The trade-off: every local build that runs the build stage recompiles the workspace crates (`mimic-*`),
not the registry dependencies. The root manifests and `Cargo.lock` are not touched on purpose: cargo
keys the dependencies' artifacts on their versions, features, profile and toolchain, and touching the
lock file made it recompile the dependencies too. A per-checkout cache key was the other option; it
would make every branch and every edit start cold. CI and the release build start from an empty cache
and are not affected.

Measured on a 12-core machine with the local cache warm (#496):

| Local build | Original `Dockerfile` | With the touch |
|---|---|---|
| Same tree, nothing changed | 4 s (image layer cache) | 4 s |
| Another tree (another branch) | wrong code (reused artifacts) | 108 s |
| One line changed in `mimic-trust` | 30 s | 123 s |
| Empty cache | about 5 min | about 5 min |

The two-tree check, run by hand after a change to the `Dockerfile` or to the build stage:

```bash
scripts/check-build-cache.sh   # builds two trees of the working copy on this daemon; asserts each runs its own code
```

It takes two image builds (minutes) and is not part of CI. The cache is shared, so a run next to
`scripts/e2e.sh` slows both down. Local builds with a `Dockerfile` from before this change still reuse
the cache without the touch: the check fails for them, until their branch has this change.

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

## Benchmarks

`scripts/bench.sh` measures what the sidecar costs: fortio against an echo server with a fixed 5 ms
delay, rates 50, 100 and 200 rps, HTTP/1.1 and h2c, with and without the sidecar. It is run by hand on
a quiet machine, never in CI (shared runners are too noisy). Needs `kind`, `kubectl`, `helm`, `docker`
and `jq`.

```bash
scripts/bench.sh --dry-run                       # the plan: 12 runs, nothing started
scripts/bench.sh                                 # the matrix on cluster "mimic-bench" (cluster build first)
RATES="50 100" PROTOCOLS=http1 scripts/bench.sh  # a smaller matrix
KEEP_CLUSTER=0 scripts/bench.sh                  # delete the cluster afterwards
```

- Rates are capped at 200 rps; anything above, or a bad value, exits 2 before any cluster command.
- It exits 3 while another run holds a lock (its own, `mimic-bench.lock`, or `mimic-e2e.lock`) and
  takes its own lock for the run. Until `scripts/e2e.sh` takes the same lock, do not run the two at
  once: they share fixed ports and the kubeconfig.
- The namespace `mimic-bench` is deleted on exit, also after a failure. The cluster stays unless
  `KEEP_CLUSTER=0`.
- Results go to `docs/bench/<date>-<setup>.json`, and one row per run is added to the table of
  `docs/bench/README.md`. Read the p50 to p99.9 latencies, the errors (non-200 responses), and the
  sidecar CPU (cores) and RSS (MiB) of caller and callee. Commit the results with the machine
  description, and say in the README when 200 rps is not clean.
- `scripts/test_bench.sh` is the self-test of the script (dry run only, no cluster).

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
