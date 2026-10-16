#!/usr/bin/env bash
# Launch as the regular user. The packaged runner prompts for sudo only after
# building, and elevates only the fixed native handoff probe.
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
if [[ ${1:-} == --help ]]; then
  cat <<'HELP'
Usage: ./tests/run-root-handoff.sh [--local-wayland-fix]
Launch as your regular user, not with sudo.
Default: visible nested desktop, original unpatched compositor dependencies.
--local-wayland-fix: private headless Cage, explicit test-only Aquamarine patch;
  no desktop/display required, no window-action adapter test on Hyprland0.56.2.
The runner asks for your sudo password when the build is ready.
Logs: target/root-handoff/latest/console.log and cage/run-*/morf-native-root.log
Optional paths: PREK_MORF_REPO, PREK_MORF_BINARY, PREK_MORF_SHELL.
Default Morf: ../morf/target/release/morf (legacy ../mold fallback).
The local variant uses Morf's nixGL/nixVulkan wrappers and detects NVIDIA_VERSION
from the loaded kernel driver unless you supply it explicitly.
The probe tests native Morf handoff/restoration; it opens no evdev/uinput devices.
HELP
  exit 0
fi
mode=desktop
if [[ ${1:-} == --local-wayland-fix && $# == 1 ]]; then
  mode=local-wayland-fix
  shift
fi
if (( EUID == 0 || $# != 0 )); then
  echo 'Launch as your regular user, without sudo; only --local-wayland-fix is optional.' >&2
  exit 2
fi

mkdir -p "$repo/target/root-handoff"
run=$(mktemp -d "$repo/target/root-handoff/run-$(date -u +%Y%m%dT%H%M%SZ)-XXXXXX")
ln -sfn -- "$run" "$repo/target/root-handoff/latest"
exec > >(tee -a "$run/console.log") 2>&1
finish() {
  result=$?
  trap - EXIT
  status=failed
  if (( result == 0 )); then status=passed; fi
  printf '{"status":"%s","exit_code":%d}\n' "$status" "$result" > "$run/status.json"
  printf '\nResult: %s (exit %d). Logs: %s\n' "$status" "$result" "$run"
  exit "$result"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf '{"status":"running"}\n' > "$run/status.json"
printf 'Root native handoff probe. Started: %s\nLogs: %s\n' "$(date -u +%FT%TZ)" "$run"
printf 'Prek revision: '
git -C "$repo" rev-parse HEAD
git -C "$repo" status --short

fixture=cage-root-test
flags=(--visible --modes --input --power --windows)
gpu=()
if [[ $mode == desktop ]]; then
  : "${WAYLAND_DISPLAY:?Run from your Wayland desktop, or pass --local-wayland-fix}"
  : "${XDG_RUNTIME_DIR:?Run from your Wayland desktop, or pass --local-wayland-fix}"
else
  fixture=cage-root-test-wayland-fix
  flags=(--hardware-rendering --modes --input --power)
  gpu=(nixGL nixVulkan)
  if [[ -z ${NVIDIA_VERSION:-} && -r /sys/module/nvidia/version ]]; then
    NVIDIA_VERSION=$(cat /sys/module/nvidia/version)
    export NVIDIA_VERSION
  fi
  echo 'TEST-ONLY local Wayland dependency fix, not unmodified stock acceptance.'
fi
printf 'Fixture: %s; NVIDIA_VERSION=%s\n' "$fixture" "${NVIDIA_VERSION:-not-selected}"

morf_repo=${PREK_MORF_REPO:-"$repo/../morf"}
if [[ -z ${PREK_MORF_REPO:-} && ! -d $morf_repo ]]; then morf_repo="$repo/../mold"; fi
morf_repo=$(realpath -e -- "$morf_repo")
morf_default="$morf_repo/target/release/morf"
if [[ ! -x $morf_default ]]; then
  for build in native-layout-verified semantic-context-verified rendered-context-verified \
               native-cancel-verified async-wallpaper-verified; do
    if [[ -x $morf_repo/target/$build/morf ]]; then
      morf_default="$morf_repo/target/$build/morf"
      break
    fi
  done
fi
morf_binary=${PREK_MORF_BINARY:-"$morf_default"}
morf_shell=${PREK_MORF_SHELL:-"$morf_repo/examples/shells/caelestia/shell/init.lua"}
if [[ ! -x "$morf_binary" || ! -f "$morf_shell" ]]; then
  printf 'Missing Morf executable or shell: %s / %s\n' "$morf_binary" "$morf_shell" >&2
  echo 'Set PREK_MORF_BINARY and PREK_MORF_SHELL to your local build paths.' >&2
  exit 2
fi
morf_binary=$(realpath -e -- "$morf_binary")
morf_shell=$(realpath -e -- "$morf_shell")
if [[ $(head -c 4 -- "$morf_binary") != $'\x7fELF' ]]; then
  echo 'PREK_MORF_BINARY must be the actual ELF, not a shell wrapper.' >&2
  exit 2
fi
printf 'Morf executable: '
sha256sum -- "$morf_binary"

nix --option eval-cache false --extra-experimental-features 'nix-command flakes' --max-jobs 1 --cores 2 \
  build --no-write-lock-file --out-link "$run/runner" "$repo#$fixture"
nix --option eval-cache false --extra-experimental-features 'nix-command flakes' --max-jobs 1 --cores 2 \
  develop --impure "$morf_repo" --no-write-lock-file -c "${gpu[@]}" \
  "$run/runner/bin/prek-cage-root-test" "${flags[@]}" \
  --morf "$morf_binary" --morf-shell "$morf_shell" --output "$run/cage"

shopt -s nullglob
receipts=("$run"/cage/run-*/morf-native-root-receipts.json)
if (( ${#receipts[@]} != 1 )); then
  echo 'Missing unique native root receipt; this run did not establish acceptance.' >&2
  exit 1
fi
printf 'Native root receipt: %s\n' "${receipts[0]}"
cat -- "${receipts[0]}"
