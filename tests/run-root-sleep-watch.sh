#!/usr/bin/env bash
set -euo pipefail
if (( $# )); then echo 'Usage: ./tests/run-root-sleep-watch.sh' >&2; exit 2; fi
root=$(git -C "$(dirname -- "$0")" rev-parse --show-toplevel)
cd "$root"
mkdir -p target/root-sleep-watch
run=$(mktemp -d "$root/target/root-sleep-watch/run-$(date -u +%Y%m%dT%H%M%SZ)-XXXXXXXX")
ln -sfn -- "$run" "$root/target/root-sleep-watch/latest"
exec > >(tee "$run/probe.log") 2>&1
echo "Delay-only root sleep probe: $(date -u +%FT%TZ)"
echo "Logs: $run"
git rev-parse HEAD
git status --short
echo 'Acquires and removes only its own short sleep-delay inhibitor; does not suspend, stop services, switch VT or open input.'
nix --option eval-cache false --extra-experimental-features 'nix-command flakes' \
    --max-jobs 1 --cores 2 build --no-write-lock-file --out-link "$run/probe" .#root-sleep-watch-test
probe=$(readlink -f "$run/probe/bin/prek-root-sleep-watch-test")
case "$probe" in /nix/store/*/bin/prek-root-sleep-watch-test) ;; *) echo 'Refusing a non-store probe.' >&2; exit 1;; esac
"$probe" --check-library
if sudo -- "$probe"; then
    printf '{"status":"passed","revision":"%s"}\n' "$(git rev-parse HEAD)" > "$run/result.json"
    echo "Passed. Logs: $run"
else
    result=$?
    printf '{"status":"failed","exit":%d}\n' "$result" > "$run/result.json"
    echo "Failed (exit $result). Logs: $run"
    exit "$result"
fi
