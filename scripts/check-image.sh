#!/usr/bin/env bash
# FND-07: the image runs as UID 65532, has no shell, and the binaries print their version.
# TLS-14: the static mimic-trust binary has no loader and no needed library, and it writes a
# trust bundle in busybox:musl and in an image that holds nothing but the binary and a store.
# Usage: scripts/check-image.sh <image>
set -euo pipefail
image=${1:?usage: check-image.sh <image>}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$(dirname "$0")/../Cargo.toml" | head -1)
# busybox:musl, pinned by digest (the multi-arch index).
busybox=busybox:musl@sha256:80e38f9a1b39188b8bdb28879178edb322a4402b081a9aedda1924609decd6f6

fail() { echo "FAIL: $*" >&2; exit 1; }

user=$(docker inspect --format '{{.Config.User}}' "$image")
[[ $user == 65532:65532 ]] || fail "fnd_07: image user is '$user', want 65532:65532"

for shell in /bin/sh /bin/bash /busybox/sh; do
  if docker run --rm --entrypoint "$shell" "$image" -c true >/dev/null 2>&1; then
    fail "fnd_07: $shell exists in the image"
  fi
done

for bin in mimic-sidecar mimic-controller mimic-trust; do
  out=$(docker run --rm --entrypoint "/usr/local/bin/$bin" "$image" --version)
  [[ $out == "$bin $version" ]] || fail "fnd_07: $bin --version printed '$out', want '$bin $version'"
done
echo "ok fnd_07: user 65532, no shell, versions $version"

# TLS-14. The binary is copied out of the image (no container is started from it).
work=$(mktemp -d)
chmod 0755 "$work"
scratch=mimic-trust-scratch:check-image-$$
platform=$(docker image inspect --format '{{.Os}}/{{.Architecture}}' "$image")
cid=$(docker create "$image")
trap 'docker rm -f "$cid" >/dev/null 2>&1 || true; docker image rm -f "$scratch" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
# files/ is the only directory the test containers see; the CA keys stay in keys/.
mkdir -m 0755 "$work/files" "$work/out-busybox" "$work/out-scratch"
mkdir -m 0700 "$work/keys"
chmod 0777 "$work/out-busybox" "$work/out-scratch"
docker cp "$cid:/usr/local/bin/mimic-trust" "$work/files/mimic-trust"
chmod 0755 "$work/files/mimic-trust"
bin="$work/files/mimic-trust"

# No program interpreter and no shared library: readelf output is captured first so that
# grep -q cannot close the pipe early.
program_headers=$(readelf -lW "$bin")
[[ $program_headers != *INTERP* ]] || fail "tls_14: mimic-trust has a PT_INTERP segment"
dynamic=$(readelf -dW "$bin" 2>&1 || true)
[[ $dynamic != *NEEDED* ]] || fail "tls_14: mimic-trust has a DT_NEEDED entry"
echo "ok tls_14: mimic-trust has no PT_INTERP and no DT_NEEDED"

# Two self-signed roots: the deployment root (passed in MIMIC_TRUST_CA, PEM text) and the
# image's own store (one certificate). A bundle from both holds exactly two certificates.
for name in root store; do
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -subj "/CN=mimic test $name" -days 2 \
    -keyout "$work/keys/$name.key" -out "$work/files/$name.pem" 2>/dev/null
done
root_pem=$(cat "$work/files/root.pem")

# check_bundle <name>: the five files of spec §7.1 are present and the bundle has two certificates.
check_bundle() {
  local dir=$work/out-$1 file
  for file in image-ca.pem mimic-ca.pem ca-bundle.pem node-extra-ca.pem truststore.p12; do
    [[ -s $dir/$file ]] || fail "tls_14: $1 wrote no $file"
  done
  local certs
  certs=$(grep -c 'BEGIN CERTIFICATE' "$dir/ca-bundle.pem" || true)
  [[ $certs == 2 ]] || fail "tls_14: $1 ca-bundle.pem holds $certs certificates, want 2"
  echo "ok tls_14: $1 wrote the trust bundle"
}

# Busybox: the files come in through a read-only bind mount, the store is the fallback.
# Every test container runs on the platform of the image under test (the pinned busybox index
# holds all of them).
docker run --rm --platform "$platform" --network none --read-only --user 1337:1337 \
  -v "$work/files:/t:ro" -v "$work/out-busybox:/out" \
  -e "MIMIC_TRUST_CA=$root_pem" -e MIMIC_TRUST_OUT=/out -e MIMIC_TRUST_FALLBACK=/t/store.pem \
  "$busybox" /t/mimic-trust || fail "tls_14: mimic-trust failed in busybox:musl"
check_bundle busybox

# Scratch: an image holding the binary and a copied CA store at the Debian path (no fallback).
mkdir -p "$work/scratch"
cp "$bin" "$work/files/store.pem" "$work/scratch/"
printf '%s\n' 'FROM scratch' 'COPY mimic-trust /usr/local/bin/mimic-trust' \
  'COPY store.pem /etc/ssl/certs/ca-certificates.crt' >"$work/scratch/Dockerfile"
docker build -q --platform "$platform" -t "$scratch" "$work/scratch" >/dev/null
docker run --rm --platform "$platform" --network none --read-only --user 1337:1337 \
  -v "$work/out-scratch:/out" \
  -e "MIMIC_TRUST_CA=$root_pem" -e MIMIC_TRUST_OUT=/out -e MIMIC_TRUST_FALLBACK=/nonexistent.pem \
  "$scratch" /usr/local/bin/mimic-trust || fail "tls_14: mimic-trust failed in the scratch image"
check_bundle scratch
echo "ok tls_14: mimic-trust is static and writes bundles in busybox:musl and scratch"
