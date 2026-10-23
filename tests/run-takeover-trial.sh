#!/usr/bin/env bash
# User-launched physical takeover trial on one named device in the current Hyprland session.
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
usage() {
  cat <<'HELP'
Usage: ./tests/run-takeover-trial.sh --device "<exact kernel device name>" [--seconds N]
Launch as your regular user while your Hyprland session runs; any terminal
works, and the session agent is started through Hyprland itself.
Prompts for sudo before building. Routes only the named device through a
temporary route broker for N seconds (default 60, 10..300) with no rules, so
input should behave exactly as before. The broker takes the device over only
after it has been neutral and idle for 100 ms, and exits by itself when the
time is up, releasing the device; Ctrl-C ends it early.
Temporarily installs Prek's udev metadata rule under /run/udev/rules.d and a
root-owned trial configuration under /run/prek-trial; both are removed on exit.
Device names: grep '^N: Name=' /proc/bus/input/devices
Logs: target/takeover-trial/latest/{console.log,broker.log,session.log,status.json}
The checkout path may contain only letters, digits and _ . / -.
HELP
}
device=""
seconds=60
while (($#)); do
  case "$1" in
    --device) device=${2:-}; shift 2 ;;
    --seconds) seconds=${2:-}; shift 2 ;;
    --help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done
if ((EUID == 0)) || [[ -z $device ]] || ! [[ $seconds =~ ^[0-9]+$ ]] || ((seconds < 10 || seconds > 300)); then
  usage >&2
  exit 2
fi
: "${XDG_RUNTIME_DIR:?Run as your regular user with a runtime directory}"
if [[ -z ${HYPRLAND_INSTANCE_SIGNATURE:-} ]]; then
  HYPRLAND_INSTANCE_SIGNATURE=$(hyprctl -j instances | jq -er \
    'if length == 1 then .[0].instance else error("expected exactly one running Hyprland") end')
  export HYPRLAND_INSTANCE_SIGNATURE
fi
if [[ $repo =~ [^A-Za-z0-9_./-] ]]; then
  echo "The checkout path \"$repo\" has characters Hyprland's exec cannot pass safely." >&2
  exit 2
fi
devices=${PREK_TRIAL_INPUT_DEVICES:-/proc/bus/input/devices}
run_root=${PREK_TRIAL_RUN_ROOT:-/run}
if ! grep -Fxq "N: Name=\"$device\"" "$devices"; then
  echo "No input device is named exactly \"$device\"." >&2
  exit 2
fi
if [[ $device == *[\'\"\\]* ]]; then
  echo "Device names with quotes or backslashes are not supported by this trial." >&2
  exit 2
fi
mkdir -p "$repo/target/takeover-trial"
run=$(mktemp -d "$repo/target/takeover-trial/run-$(date -u +%Y%m%dT%H%M%SZ)-XXXXXX")
ln -sfn -- "$run" "$repo/target/takeover-trial/latest"
exec > >(tee -a "$run/console.log") 2>&1
rule=$run_root/udev/rules.d/70-prek-trial.rules
trial=$run_root/prek-trial
broker=""
prek=""
finish() {
  result=$?
  trap - EXIT
  if [[ -f $run/session.pid && ! -f $run/session.status ]]; then
    agent=$(cat "$run/session.pid")
    if [[ $(tr '\0' ' ' < "/proc/$agent/cmdline" 2>/dev/null) == "$prek session "* ]]; then
      kill "$agent" 2>/dev/null || true
    fi
  fi
  if [[ -n $broker ]]; then kill -TERM "$broker" 2>/dev/null || true; wait "$broker" 2>/dev/null || true; fi
  sudo -n rm -f "$rule" 2>/dev/null && sudo -n udevadm control --reload-rules 2>/dev/null || true
  sudo -n rm -rf "$trial" 2>/dev/null || true
  if [[ -n ${keepalive:-} ]]; then kill "$keepalive" 2>/dev/null || true; fi
  status=failed
  if ((result == 0)); then status=passed; fi
  printf '{"status":"%s","exit_code":%d}\n' "$status" "$result" > "$run/status.json"
  printf '\nResult: %s (exit %d). Logs: %s\n' "$status" "$result" "$run"
  exit "$result"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf '{"status":"running"}\n' > "$run/status.json"
printf 'Takeover trial for "%s", %d s. Started: %s\nLogs: %s\n' "$device" "$seconds" "$(date -u +%FT%TZ)" "$run"
git -C "$repo" rev-parse HEAD
git -C "$repo" status --short
# Authenticates once, then keeps the sudo timestamp valid until exit.
sudo -v
while sleep 60; do sudo -n -v || exit; done </dev/null >/dev/null 2>&1 &
keepalive=$!
nix --option eval-cache false --extra-experimental-features 'nix-command flakes' \
  --max-jobs 1 --cores 2 build --no-write-lock-file --out-link "$run/prek" "$repo#default"
prek="$run/prek/bin/prek"
cat > "$run/config.toml" <<CONFIG
version = 1
mode = "route"

[policy]
allowed_devices = ["trial"]
allowed_seats = ["seat0"]

[devices.trial]
name = "$device"
CONFIG
"$prek" check --config "$run/config.toml" --json > "$run/check.json"
pid=$(hyprctl -j instances | jq -er --arg instance "$HYPRLAND_INSTANCE_SIGNATURE" \
  '.[] | select(.instance == $instance) | .pid')
compositor=$(readlink -f "/proc/$pid/exe")
logind=$(readlink -f "${PREK_TRIAL_LOGIND:-/run/current-system/sw/lib/libsystemd.so.0}")
printf 'Compositor: %s (pid %s)\nlibsystemd: %s\n' "$compositor" "$pid" "$logind"
sudo install -d -m 0755 -o root -g root "$trial"
sudo install -m 0644 -o root -g root "$run/config.toml" "$trial/config.toml"
sudo install -m 0644 -o root -g root "$run/prek/lib/udev/rules.d/70-prek.rules" "$rule"
sudo udevadm control --reload-rules
# shellcheck disable=SC2024
sudo "$prek" broker --mode route --config "$trial/config.toml" --runtime-dir "$trial/runtime" \
  --compositor-executable "$compositor" --logind-library "$logind" --json --duration "$seconds" \
  > "$run/broker.log" 2>&1 &
broker=$!
socket="$trial/runtime/control.sock"
for _ in $(seq 100); do
  if sudo -n test -S "$socket"; then break; fi
  if ! kill -0 "$broker" 2>/dev/null; then
    status=0
    wait "$broker" || status=$?
    broker=""
    echo "The broker exited with status $status before opening its socket; see $run/broker.log." >&2
    exit 1
  fi
  sleep 0.1
done
if ! sudo -n test -S "$socket"; then
  echo "The broker did not open its socket within 10 s; see $run/broker.log." >&2
  exit 1
fi
# Hyprland starts the agent so it runs in the compositor's own session or unit.
cat > "$run/agent.sh" <<AGENT
#!/bin/sh
"$prek" session --socket "$socket" --observations --context --duration $((seconds - 2)) \\
  > "$run/session.log" 2>&1 &
echo \$! > "$run/session.pid"
wait \$!
echo \$? > "$run/session.status"
AGENT
chmod 0755 "$run/agent.sh"
provider=$(hyprctl -j status | jq -r '.configProvider // "hyprlang"')
if [[ $provider == lua ]]; then
  reply=$(hyprctl dispatch "hl.dsp.exec_cmd(\"$run/agent.sh\")")
else
  reply=$(hyprctl dispatch exec "$run/agent.sh")
fi
if [[ $reply != ok ]]; then
  echo "Hyprland ($provider) refused to start the session agent: $reply" >&2
  exit 1
fi
for _ in $(seq 50); do
  if [[ -s $run/session.pid ]]; then break; fi
  sleep 0.1
done
if [[ ! -s $run/session.pid ]]; then
  echo "Hyprland did not start the session agent within 5 s." >&2
  exit 1
fi
printf '\nType and use "%s" normally for %d seconds. If it stops responding,\n' "$device" "$seconds"
printf 'wait for the timer or press Ctrl-C; the broker then releases it.\n\n'
for _ in $(seq "$((seconds * 10 + 100))"); do
  if [[ -s $run/session.status ]]; then break; fi
  sleep 0.1
done
agent_status=$(cat "$run/session.status" 2>/dev/null || echo unknown)
if [[ $agent_status != 0 ]]; then
  echo "session agent exited with status $agent_status:"
  tail -n 5 "$run/session.log" || true
fi
wait "$broker"
broker=""
grep -E 'taken over|stays with its original consumer|keeps no consumer evidence|lost its device settings' \
  "$run/broker.log" || echo "No takeover outcome was logged for \"$device\"."
