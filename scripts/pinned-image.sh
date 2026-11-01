#!/usr/bin/env bash
# Prints the digest-pinned reference of one stage of images/Dockerfile (#273), e.g.
#   scripts/pinned-image.sh e2e-app   ->   docker.io/library/python:3.13-alpine@sha256:…
# Exit 2 on a usage error, 1 when the stage is missing or defined twice, or has no digest.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ $# -ne 1 ]; then
  echo "usage: $0 <stage>" >&2
  exit 2
fi
refs=$(awk -v stage="$1" 'toupper($1) == "FROM" && $NF == stage { print $2 }' images/Dockerfile)
case $refs in
  "") echo "no stage $1 in images/Dockerfile" >&2; exit 1 ;;
  *$'\n'*) echo "stage $1 is defined more than once in images/Dockerfile" >&2; exit 1 ;;
esac
case $refs in
  *@sha256:[0-9a-f]*) ;;
  *) echo "stage $1 is not pinned by digest" >&2; exit 1 ;;
esac
printf '%s\n' "$refs"
