#!/usr/bin/env bash
# Only the fixed Nix probe is elevated. The invoking user owns every log file.
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
if [[ ${1:-} == --help ]]; then
  cat <<'HELP'
Usage: ./tests/run-root-routing.sh
Launch as your regular user, without sudo. The built runner prompts when ready.
Runs fourteen routing, eight recovery, two transport-characterization, one
generated-keyboard and one managed-worker gate (version-9 receipt, 26 cases).
The managed gate logs nine kernel phases, single-counterpart keyboard/relative
projection and cadence-preserving fallback, software admission, idle lease expiry,
consumer neutral drain and worker cleanup. Native hybrid MT/compositor and
production authorization are not tested.
Repeat/fuzz probes check kernel behavior, not transparent passthrough support.
No physical input devices are opened and no production routing is enabled.
Temporarily installs a test-only runtime udev rule, reloads rules without a trigger,
then removes its own rule. /dev/uinput must already exist; no module is loaded.
Logs: target/root-routing/latest/{console.log,status.json,receipt.json}
HELP
  exit 0
fi
if (( EUID == 0 || $# != 0 )); then
  echo 'Launch ./tests/run-root-routing.sh as your regular user, without sudo or arguments.' >&2
  exit 2
fi
mkdir -p "$repo/target/root-routing"
run=$(mktemp -d "$repo/target/root-routing/run-$(date -u +%Y%m%dT%H%M%SZ)-XXXXXX")
ln -sfn -- "$run" "$repo/target/root-routing/latest"
exec > >(tee -a "$run/console.log") 2>&1
finish() {
  result=$?
  trap - EXIT
  status=failed
  if (( result == 0 )); then status=passed; fi
  printf '{"status":"%s","exit_code":%d}\n' "$status" "$result" > "$run/status.json"
  printf '\nResult: %s (exit %d). Load: %s. Logs: %s\n' "$status" "$result" "$(cut -d' ' -f1-3 /proc/loadavg)" "$run"
  exit "$result"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf '{"status":"running"}\n' > "$run/status.json"
printf 'Isolated kernel routing gates. Started: %s (load %s)\nLogs: %s\n' "$(date -u +%FT%TZ)" "$(cut -d' ' -f1-3 /proc/loadavg)" "$run"
git -C "$repo" rev-parse HEAD
git -C "$repo" status --short
nix --option eval-cache false --extra-experimental-features 'nix-command flakes' \
  --max-jobs 1 --cores 2 build --no-write-lock-file --out-link "$run/runner" "$repo#root-routing-test"
"$run/runner/bin/prek-root-routing-test" | tee "$run/kernel.log"
nix --option eval-cache false --extra-experimental-features 'nix-command flakes' \
  develop "$repo" --no-write-lock-file -c python3 - "$repo" "$run" <<'PY'
import importlib.util
import json
from pathlib import Path
import sys
repo, run = map(Path, sys.argv[1:])
spec = importlib.util.spec_from_file_location("root_routing", repo / "tests/support/root_routing.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
value = module.receipt((run / "kernel.log").read_text())
with (run / "receipt.json").open("x") as output:
    json.dump(value, output, indent=2)
    output.write("\n")
print("Validated isolated kernel receipt:", run / "receipt.json")
PY
