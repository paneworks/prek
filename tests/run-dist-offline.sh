#!/usr/bin/env bash
set -euo pipefail
shopt -s nullglob

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d "$root/target/distribution/offline-XXXXXXXX")"
archives=("$root"/target/distribution/prek-*-nix.tar.gz)
test "${#archives[@]}" -eq 1
(cd "$(dirname "${archives[0]}")" && sha256sum --check "$(basename "${archives[0]%.gz}.sha256")")
tar --extract --gzip --file "${archives[0]}" --directory "$work" --no-same-owner
bundles=("$work"/prek-*-nix)
test "${#bundles[@]}" -eq 1
python3 "${bundles[0]}/install.py" import --bundle "${bundles[0]}" --trust-unsigned \
    --require-missing --store-root "$work/empty-store" | tee "$work/receipt.json"
if python3 "${bundles[0]}/install.py" import --bundle "${bundles[0]}" --trust-unsigned \
    --require-missing --store-root "$work/empty-store" > "$work/repeated.log" 2>&1; then
    echo 'Fresh-import check accepted an existing application' >&2
    exit 1
fi
grep -F 'already exists; not a fresh import' "$work/repeated.log"
nars=("${bundles[0]}"/cache/nar/*)
test "${#nars[@]}" -gt 1
cp -- "${nars[1]}" "${nars[0]}"
(cd "${bundles[0]}" && find . -type f ! -name SHA256SUMS -printf '%P\0' \
    | sort -z | xargs -0 sha256sum > "$work/checksums.new")
mv "$work/checksums.new" "${bundles[0]}/SHA256SUMS"
if python3 "${bundles[0]}/install.py" verify --bundle "${bundles[0]}" > "$work/corrupt.log" 2>&1; then
    echo 'NAR verification accepted corrupt content with updated file checksums' >&2
    exit 1
fi
grep -E 'was modified! expected hash|hash mismatch' "$work/corrupt.log"
printf 'Isolated import evidence: %s\n' "$work"
