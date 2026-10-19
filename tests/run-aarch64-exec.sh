#!/usr/bin/env bash
set -euo pipefail
if (( $# != 0 )); then echo 'Usage: oslo make exec-aarch64' >&2; exit 2; fi
export LC_ALL=C
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$repo"
binfmt=${PREK_BINFMT_AARCH64:-/proc/sys/fs/binfmt_misc/aarch64-linux}
store=${PREK_NIX_STORE:-/nix/store}
cc=${CC_aarch64_unknown_linux_musl:?run inside the cross-aarch64 shell}
build=$(readlink -f -- target/cross-aarch64/latest)
mkdir -p target/cross-aarch64/execution
run=$(mktemp -d "$repo/target/cross-aarch64/execution/run-$(date -u +%Y%m%dT%H%M%SZ)-XXXXXX")
ln -sfn -- "$run" target/cross-aarch64/executed
exec > >(tee -a "$run/execution.log") 2>&1
finish() {
  result=$?
  trap - EXIT
  if (( result == 0 )); then
    echo passed > "$run/status.txt"
  else
    rm -f -- "$run/receipt.txt"
    echo failed > "$run/status.txt"
  fi
  printf 'ARM emulated execution exit %d. Evidence: %s\n' "$result" "$run"
  exit "$result"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
echo running > "$run/status.txt"
printf '%s\n' "$build" > "$run/build.path"

# Only a passed, clean cross-build of the current source is executed.
grep -qx passed "$build/status.txt"
grep -qx "source_revision=$(git rev-parse HEAD)" "$build/receipt.txt"
grep -qx source_dirty=false "$build/receipt.txt"
(cd -- "$build"; sha256sum --check --strict SHA256SUMS)
[[ $(head -n 1 -- "$binfmt") == enabled ]]

loader=$("$cc" -print-file-name=ld-musl-aarch64.so.1)
musl=$(dirname -- "$("$cc" -print-file-name=libc.so)")
gcc=$(dirname -- "$("$cc" -print-file-name=libgcc_s.so.1)")
for path in "$loader" "$musl/libc.so" "$gcc/libgcc_s.so.1"; do
  [[ $path == "$store"/* && -e $path ]]
done

# A copy is redirected to the cross runtime; the retained artifact stays unchanged.
install -m 755 "$build/prek" "$run/prek"
patchelf --set-interpreter "$loader" --set-rpath "$musl:$gcc" "$run/prek"
version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' Cargo.toml | head -n 1)
[[ $("$run/prek" --version) == "prek $version" ]]
for config in touch_routing window_keys power; do
  "$run/prek" check --config "examples/lua/$config.lua" --json > "$run/check-$config.json"
  grep -q '"status":"valid"' "$run/check-$config.json"
done
(cd -- "$build"; sha256sum --check --strict SHA256SUMS)

{
  printf 'build=%s\n' "$build"
  printf 'source_revision=%s\n' "$(git rev-parse HEAD)"
  printf 'artifact_sha256=%s\n' "$(cut -d' ' -f1 "$build/SHA256SUMS")"
  printf 'host_machine=%s\n' "$(uname -m)"
  printf 'execution=emulated-binfmt\nhardware=false\nchecked=version,lua-check:touch_routing,window_keys,power\n'
} > "$run/receipt.txt"
cat "$run/receipt.txt"
