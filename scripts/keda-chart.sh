#!/usr/bin/env bash
# #455: the pinned KEDA chart archive, the only place that holds its version and SHA-256 (docs/SPEC.md §10.7).
#   scripts/keda-chart.sh <dir>   puts keda-<version>.tgz in <dir> (or takes $KEDA_ARCHIVE), refuses it unless its SHA-256
#                                 is the pinned one, and prints its path
#   scripts/keda-chart.sh --pin   prints "<version> <sha256>"
# Exit 0 on success, 1 when the download fails or the archive is not the pinned one, 2 on a usage error.
set -euo pipefail

version=2.21.1
sha256=76b45621e48e18c9872cdc0c46295e4cbac1236fd379d06bca38ef91d8fb883e

case ${1:-} in
  --pin) echo "$version $sha256"; exit 0 ;;
  "" | -*) echo "usage: $0 <dir> | --pin" >&2; exit 2 ;;
esac
archive=${KEDA_ARCHIVE:-$1/keda-$version.tgz}
if [[ -z ${KEDA_ARCHIVE:-} ]]; then
  curl -fsSL --retry 3 -o "$archive" "https://kedacore.github.io/charts/keda-$version.tgz" \
    || { echo "FAIL cannot download the keda chart $version" >&2; exit 1; }
fi
[[ $(sha256sum "$archive" | cut -d' ' -f1) == "$sha256" ]] \
  || { echo "FAIL the keda chart archive is not the pinned $sha256" >&2; exit 1; }
printf '%s\n' "$archive"
