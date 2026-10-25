# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through
[GitHub private vulnerability reporting](https://github.com/Thaliph/mock-server/security/advisories/new),
not in public issues. Include the affected component (sidecar, `mimic-init`, `mimic-cni`,
controller, webhook, `mimicctl`), the version, and the steps to reproduce.

You will get an acknowledgement within 5 working days.

## Supported versions

mimic is in development (`0.x`). Only the latest release receives security fixes.

## Automated checks

Every pull request, every push to `main` and a weekly schedule run
[`.github/workflows/security.yml`](.github/workflows/security.yml):

| Tool | What it checks |
|------|----------------|
| [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) | known vulnerabilities in Rust dependencies (RustSec), licenses, crate sources (`deny.toml`) |
| [CodeQL](https://codeql.github.com/) | static analysis of the Rust code and of the GitHub Actions workflows |
| [zizmor](https://github.com/zizmorcore/zizmor) | GitHub Actions workflow weaknesses (template injection, excessive permissions, unpinned actions); **blocking**: a new finding fails the pull request |
| [Trivy](https://trivy.dev/) (config) | misconfigurations in the `Dockerfile` and the Kubernetes manifests under `deploy/`; **blocking** on HIGH and CRITICAL, MEDIUM is report-only |
| ignores check ([`scripts/check-ignores.sh`](scripts/check-ignores.sh)) | every accepted finding (`.trivyignore.yaml`, `# zizmor: ignore[...]`) carries a reason and a review or expiry date; an expired entry fails the job |
| [OpenSSF Scorecard](https://scorecard.dev/) | repository security practices (`main` only) |

[Dependabot](.github/dependabot.yml) proposes weekly updates of crates, actions and base images.
Results are in the repository's **Security** tab.

Open findings from the October 2026 production review are tracked in [todo.md](todo.md).
