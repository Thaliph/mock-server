# Security policy

Please report vulnerabilities privately through GitHub's
[security advisories](https://github.com/Thaliph/mock-server/security/advisories/new), not in
public issues. You will get an answer within a week.

mimic intercepts application traffic, including TLS (see `docs/SPEC.md` §5), so its sidecar and
controller are security-sensitive by design. Only the latest release is supported.

Release images and the Helm chart are signed with cosign (keyless, GitHub OIDC):

```bash
cosign verify ghcr.io/thaliph/mimic:<version> \
  --certificate-identity-regexp 'https://github.com/Thaliph/mock-server/.github/workflows/release.yml@refs/tags/v.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```
