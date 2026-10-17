#!/usr/bin/env bash
set -euo pipefail
if (( $# != 0 )); then echo 'Usage: oslo make build-aarch64' >&2; exit 2; fi
export LC_ALL=C
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$repo"
export CARGO_TARGET_DIR="$repo/target/cross-aarch64/system-build"
mkdir -p target/cross-aarch64/evidence
run=$(mktemp -d "$repo/target/cross-aarch64/evidence/run-$(date -u +%Y%m%dT%H%M%SZ)-XXXXXX")
ln -sfn -- "$run" target/cross-aarch64/latest
exec > >(tee -a "$run/build.log") 2>&1
finish() {
  result=$?
  trap - EXIT
  if (( result == 0 )); then
    echo passed > "$run/status.txt"
  else
    rm -f -- "$run/receipt.txt"
    echo failed > "$run/status.txt"
  fi
  printf 'ARM cross-build exit %d. Evidence: %s\n' "$result" "$run"
  exit "$result"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
echo running > "$run/status.txt"
git rev-parse HEAD > "$run/source.revision"
git diff --binary HEAD | sha256sum > "$run/source.sha256"
git status --porcelain=v1 > "$run/source.status"
target=aarch64-unknown-linux-musl
cargo build --locked --release --bin prek --target "$target"
install -m 755 "$CARGO_TARGET_DIR/$target/release/prek" "$run/prek"
readelf --wide --file-header --program-headers --dynamic "$run/prek" > "$run/elf.txt"
grep -Eq 'Class: +ELF64$' "$run/elf.txt"
grep -Eq 'Data: +2.s complement, little endian$' "$run/elf.txt"
grep -Eq 'Machine: +AArch64$' "$run/elf.txt"
grep -Fq '[Requesting program interpreter: /lib/ld-musl-aarch64.so.1]' "$run/elf.txt"
grep -Eq '\(NEEDED\).*\[libc\.so\]' "$run/elf.txt"
grep -Eq '\(NEEDED\).*\[libgcc_s\.so\.1\]' "$run/elf.txt"
if [[ $(grep -c '(NEEDED)' "$run/elf.txt") != 2 ]] || grep -Eq '\((RPATH|RUNPATH)\)' "$run/elf.txt"; then
  echo 'ARM system-linked output must have only libc.so/libgcc_s.so.1 and no runtime search paths.' >&2
  exit 1
fi
git rev-parse HEAD | cmp - "$run/source.revision"
git diff --binary HEAD | sha256sum | cmp - "$run/source.sha256"
git status --porcelain=v1 | cmp - "$run/source.status"
(cd -- "$run"; sha256sum prek > SHA256SUMS)
{
  printf 'target=%s\ninterpreter=/lib/ld-musl-aarch64.so.1\n' "$target"
  printf 'source_revision=%s\n' "$(cat "$run/source.revision")"
  if [[ -s $run/source.status ]]; then echo source_dirty=true; else echo source_dirty=false; fi
  printf 'nix_runtime_references=false\nworkspace_runtime_references=false\nexecution=not-attempted\n'
} > "$run/receipt.txt"
cat "$run/receipt.txt"
cat "$run/SHA256SUMS"
