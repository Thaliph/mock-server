# Security policy

Please report vulnerabilities privately through GitHub's
[security advisories](https://github.com/Thaliph/mock-server/security/advisories/new), not in
public issues. You will get an answer within a week.

mimic intercepts application traffic, including TLS (see `docs/SPEC.md` §5), so its sidecar and
controller are security-sensitive by design. The supported release is `0.2.0-alpha.3` (a
pre-release); only the latest release is supported.

Release images and the Helm chart are signed with cosign (keyless, GitHub OIDC). Verify the two
images and the chart before installing:

```bash
ID=https://github.com/Thaliph/mock-server/.github/workflows/release.yml@refs/tags/v0.2.0-alpha.3
ISSUER=https://token.actions.githubusercontent.com
for ref in ghcr.io/thaliph/mimic:0.2.0-alpha.3 ghcr.io/thaliph/mimic-init:0.2.0-alpha.3 ghcr.io/thaliph/charts/mimic:0.2.0-alpha.3; do
  cosign verify "$ref" --certificate-identity "$ID" --certificate-oidc-issuer "$ISSUER"
done
```
