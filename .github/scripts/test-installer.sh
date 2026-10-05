#!/usr/bin/env bash
# Exercises install.sh against a locally built release in several distros.
#
#   .github/scripts/test-installer.sh <release-dir>
#
# <release-dir> holds rustle-<target>.tar.gz and SHA256SUMS for this
# machine's Linux target (see the installer job in ci.yml). Needs docker.
set -euo pipefail
REL="$(cd "${1:?usage: test-installer.sh <release-dir>}" && pwd)"
HERE="$(cd "$(dirname "$0")/../.." && pwd)"

run_in() { # <image> : runs install.sh in a container, prints its output
    docker run --rm \
        -v "$HERE/install.sh":/install.sh:ro -v "$REL":/rel:ro \
        -e RUSTLE_BASE_URL=file:///rel -e RUSTLE_INSTALL_DIR=/tmp/bin \
        "$1" sh -c '
            command -v curl >/dev/null 2>&1 || {
                (apt-get update -qq && apt-get install -y -qq curl) >/dev/null 2>&1 ||
                    dnf install -y -q curl >/dev/null 2>&1 || true
            }
            sh /install.sh 2>&1; rc=$?
            [ $rc -eq 0 ] && /tmp/bin/rustle --version
            exit $rc'
}

fail=0
# OK_IMAGES: override when the test binary was built on a newer glibc.
for img in ${OK_IMAGES:-ubuntu:22.04 ubuntu:24.04 debian:12-slim fedora:40}; do
    echo "=== $img: must install"
    if out="$(run_in "$img")" && grep -q "installed to /tmp/bin/rustle" <<<"$out" && grep -q '^rustle ' <<<"$out"; then
        echo ok
    else
        echo "$out"; echo "FAILED: $img"; fail=1
    fi
done
# Refusals: glibc too old, and no glibc at all.
for pair in "debian:11-slim|glibc 2.31 is too old" "alpine:3.20|does not use glibc"; do
    img="${pair%%|*}"; want="${pair#*|}"
    echo "=== $img: must refuse ($want)"
    if out="$(run_in "$img")"; then
        echo "$out"; echo "FAILED: $img installed"; fail=1
    elif grep -q "$want" <<<"$out"; then
        echo ok
    else
        echo "$out"; echo "FAILED: $img refused with the wrong message"; fail=1
    fi
done
# A tampered archive must be rejected.
echo "=== checksum mismatch must abort"
tmp="$(mktemp -d)"; cp "$REL"/* "$tmp"/
sed -i.bak 's/^[0-9a-f]\{8\}/00000000/' "$tmp/SHA256SUMS"
out="$(docker run --rm -v "$HERE/install.sh":/install.sh:ro -v "$tmp":/rel:ro \
    -e RUSTLE_BASE_URL=file:///rel -e RUSTLE_INSTALL_DIR=/tmp/bin debian:12-slim \
    sh -c 'apt-get update -qq && apt-get install -y -qq curl >/dev/null 2>&1; sh /install.sh 2>&1 || true')"
grep -q "checksum mismatch" <<<"$out" && echo ok || { echo "$out"; echo "FAILED: tampered archive accepted"; fail=1; }
exit $fail
