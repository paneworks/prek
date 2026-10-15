#!/usr/bin/env bash
# Launch as the desktop user. The packaged runner prompts for sudo only after
# building, and elevates only the fixed native handoff probe.
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
if [[ ${1:-} == --help ]]; then
  cat <<'HELP'
Usage: ./tests/run-root-handoff.sh
Run from your Wayland desktop terminal as your regular user, not with sudo.
The runner asks for your sudo password when the build is ready.
Logs: target/root-handoff/latest/console.log and cage/run-*/morf-native-root.log
Optional paths: PREK_MORF_REPO, PREK_MORF_BINARY, PREK_MORF_SHELL.
The probe tests native Morf handoff/restoration; it opens no evdev/uinput devices.
HELP
  exit 0
fi
if (( EUID == 0 || $# != 0 )); then
  echo 'Launch ./tests/run-root-handoff.sh as your desktop user, without sudo or arguments.' >&2
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

: "${WAYLAND_DISPLAY:?Run this from your Wayland desktop terminal}"
: "${XDG_RUNTIME_DIR:?Run this from your Wayland desktop terminal}"
morf_repo=${PREK_MORF_REPO:-"$repo/../mold"}
morf_default="$morf_repo/target/async-wallpaper-verified/morf"
if [[ -x "$morf_repo/target/native-cancel-verified/morf" ]]; then
  morf_default="$morf_repo/target/native-cancel-verified/morf"
fi
morf_binary=${PREK_MORF_BINARY:-"$morf_default"}
morf_shell=${PREK_MORF_SHELL:-"$morf_repo/examples/shells/caelestia/shell/init.lua"}
if [[ ! -x "$morf_binary" || ! -f "$morf_shell" ]]; then
  printf 'Missing Morf executable or shell: %s / %s\n' "$morf_binary" "$morf_shell" >&2
  echo 'Set PREK_MORF_BINARY and PREK_MORF_SHELL to your local build paths.' >&2
  exit 2
fi
printf 'Morf executable: '
sha256sum -- "$morf_binary"

nix --option eval-cache false --extra-experimental-features 'nix-command flakes' \
  build --no-write-lock-file --out-link "$run/runner" "$repo#cage-root-test"
nix --option eval-cache false --extra-experimental-features 'nix-command flakes' \
  develop "$morf_repo" --no-write-lock-file -c \
  "$run/runner/bin/prek-cage-root-test" --visible --modes --input --power --windows \
  --morf "$morf_binary" --morf-shell "$morf_shell" --output "$run/cage"

shopt -s nullglob
receipts=("$run"/cage/run-*/morf-native-root-receipts.json)
if (( ${#receipts[@]} != 1 )); then
  echo 'Missing unique native root receipt; this run did not establish acceptance.' >&2
  exit 1
fi
printf 'Native root receipt: %s\n' "${receipts[0]}"
cat -- "${receipts[0]}"
