#!/usr/bin/env bash
# #364, spec §12.14 (REL-09): the `Artefacts` section of the release notes. Prints the digests of
# the two images and of the chart and the three `cosign verify` commands, in Markdown.
#
#   scripts/release-artefacts.sh <version> <mimic-digest> <mimic-init-digest> <chart-digest>
#
# Exit 2 on a usage error, 1 when a digest is not `sha256:<64 hex>`.
set -euo pipefail

if [ $# -ne 4 ]; then
  echo "usage: $0 <version> <mimic-digest> <mimic-init-digest> <chart-digest>" >&2
  exit 2
fi
version=$1
app=$2
init=$3
chart=$4
for digest in "$app" "$init" "$chart"; do
  [[ $digest =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "not a sha256 digest: $digest" >&2; exit 1; }
done

identity="https://github.com/Thaliph/mock-server/.github/workflows/release.yml@refs/tags/v$version"
issuer=https://token.actions.githubusercontent.com

cat <<EOF

## Artefacts

| Artefact | Reference |
|---|---|
| mimic (multi-arch index) | \`ghcr.io/thaliph/mimic@$app\` |
| mimic-init (multi-arch index) | \`ghcr.io/thaliph/mimic-init@$init\` |
| Helm chart $version | \`oci://ghcr.io/thaliph/charts/mimic\` $version, \`$chart\` |

Verify the signatures before installing:

\`\`\`sh
cosign verify ghcr.io/thaliph/mimic@$app \\
  --certificate-identity $identity \\
  --certificate-oidc-issuer $issuer
cosign verify ghcr.io/thaliph/mimic-init@$init \\
  --certificate-identity $identity \\
  --certificate-oidc-issuer $issuer
cosign verify ghcr.io/thaliph/charts/mimic@$chart \\
  --certificate-identity $identity \\
  --certificate-oidc-issuer $issuer
\`\`\`
EOF
