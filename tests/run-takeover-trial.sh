#!/usr/bin/env bash
# User-launched physical takeover trial on one named device in the current Wayland session.
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
usage() {
  cat <<'HELP'
Usage: ./tests/run-takeover-trial.sh --device "<exact kernel device name>" [--seconds N] [--swipe] [--keys] [--lock] [--edges] [--inject] [--binary PATH]
Launch as your regular user from a terminal of the Wayland session under test;
WAYLAND_DISPLAY and XDG_RUNTIME_DIR select the compositor. The compositor is
the process listening on that display socket, and the session agent joins the
compositor's cgroup so it shares the compositor's login session or unit.
Prompts for sudo before building, then blocks system sleep and idle locking
until it exits; a manual lock still works. Routes only the named device through a
temporary route broker for N seconds (default 60, 10..300) with no rules, so
input should behave exactly as before. The broker takes the device over only
after it has been neutral and idle for 100 ms, and exits by itself when the
time is up, releasing the device; Ctrl-C ends it early.
--swipe adds one rule for a touchscreen: a three-finger swipe down is consumed
and runs a command in the session that appends a timestamp to fired.log in the
run's log directory. Every other touch should behave exactly as before. The
trial then fails unless the swipe ran its command at least once.
--keys adds a three-finger swipe up that presses volume up through a generated
keyboard; the trial fails unless that swipe was recognized at least once.
--lock expects the session to be locked and unlocked during the trial: the
device must return to its compositor while locked, be taken over again after
the last unlock, and the session must end on its desktop.
--edges adds two one-finger rules that start only in a 20 px edge strip: a swipe
up from the bottom edge and a swipe down from the top edge, each appending its
direction to edges.log; the trial fails unless at least one ran.
--inject performs the touchscreen gestures itself once the device is taken over,
by writing synthetic contacts into its kernel event node with
tests/support/inject-input.pl (needs perl): a one-finger swipe up mid-screen that
must be forwarded, then three swipes for each of --swipe and --keys and two per
edge for --edges, which must all be recognized exactly once each, and nothing
else may run. --swipe also injects a three-finger swipe down whose first finger
moves alone for 200 ms, past the join window, before the others land; it stays
with the application and must not run the command. A gesture is skipped once
the device is no longer taken over,
so none reaches a lock screen. Leave the touchscreen alone meanwhile.
--binary uses a prek executable built elsewhere instead of building the package:
it is installed root-owned under /run/prek-trial, and the udev rule is generated
from udev/70-prek.rules for it.
Temporarily installs Prek's udev metadata rule under /run/udev/rules.d and a
root-owned trial configuration under /run/prek-trial; both are removed on exit,
as is the broker's /run/prek device registry when the trial created it.
Device names: grep '^N: Name=' /proc/bus/input/devices
Logs: target/takeover-trial/latest/{console.log,broker.log,session.log,status.json}
The checkout path may contain only letters, digits and _ . / -.
HELP
}
device=""
seconds=60
swipe=""
keys=""
lock=""
inject=""
binary=""
edges=""
while (($#)); do
  case "$1" in
    --device) device=${2:-}; shift 2 ;;
    --seconds) seconds=${2:-}; shift 2 ;;
    --swipe) swipe=1; shift ;;
    --keys) keys=1; shift ;;
    --lock) lock=1; shift ;;
    --inject) inject=1; shift ;;
    --binary) binary=${2:-}; shift 2 ;;
    --edges) edges=1; shift ;;
    --help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done
if ((EUID == 0)) || [[ -z $device ]] || ! [[ $seconds =~ ^[0-9]+$ ]] || ((seconds < 10 || seconds > 300)); then
  usage >&2
  exit 2
fi
: "${XDG_RUNTIME_DIR:?Run as your regular user with a runtime directory}"
: "${WAYLAND_DISPLAY:?Run from a terminal of the Wayland session under test}"
if [[ $repo =~ [^A-Za-z0-9_./-] ]]; then
  echo "The checkout path \"$repo\" has characters the agent launcher cannot quote safely." >&2
  exit 2
fi
if [[ -n $binary ]]; then
  binary=$(realpath -e -- "$binary" 2>/dev/null) || binary=""
  if [[ -z $binary || ! -f $binary || ! -x $binary || $binary =~ [^A-Za-z0-9_./-] ]]; then
    echo "--binary needs an executable file whose path has only letters, digits and _ . / -." >&2
    exit 2
  fi
fi
if [[ $WAYLAND_DISPLAY =~ [^A-Za-z0-9_.-] || $WAYLAND_DISPLAY == . || $WAYLAND_DISPLAY == .. ]]; then
  echo "WAYLAND_DISPLAY \"$WAYLAND_DISPLAY\" is not a socket name inside XDG_RUNTIME_DIR." >&2
  exit 2
fi
devices=${PREK_TRIAL_INPUT_DEVICES:-/proc/bus/input/devices}
run_root=${PREK_TRIAL_RUN_ROOT:-/run}
cgroups=${PREK_TRIAL_CGROUP_ROOT:-/sys/fs/cgroup}
if ! grep -Fxq "N: Name=\"$device\"" "$devices"; then
  echo "No input device is named exactly \"$device\"." >&2
  exit 2
fi
if [[ $device == *[\'\"\\]* ]]; then
  echo "Device names with quotes or backslashes are not supported by this trial." >&2
  exit 2
fi
if [[ -n $inject ]]; then
  if ! perl=$(command -v perl); then
    echo "--inject needs perl on PATH." >&2
    exit 2
  fi
  nodes=$(awk -v name="N: Name=\"$device\"" '
    $0 == name { found = 1 }
    found && /^H: Handlers=/ {
      count = split($0, words, /[ =]/)
      for (i = 1; i <= count; i++) if (words[i] ~ /^event[0-9]+$/) print words[i]
      found = 0
    }
    /^$/ { found = 0 }' "$devices")
  if [[ $(printf '%s\n' "$nodes" | grep -c '^event') != 1 ]]; then
    echo "--inject needs exactly one event node named \"$device\"." >&2
    exit 2
  fi
  node=/dev/input/$nodes
fi
mkdir -p "$repo/target/takeover-trial"
run=$(mktemp -d "$repo/target/takeover-trial/run-$(date -u +%Y%m%dT%H%M%SZ)-XXXXXX")
ln -sfn -- "$run" "$repo/target/takeover-trial/latest"
exec > >(tee -a "$run/console.log") 2>&1
rules=$run_root/udev/rules.d
rule=$rules/70-prek-trial.rules
trial=$run_root/prek-trial
broker=""
prek=""
made_rules=""
made_registry=""
finish() {
  result=$?
  trap - EXIT
  if [[ -f $run/session.pid && ! -f $run/session.status ]]; then
    agent=$(cat "$run/session.pid")
    if [[ $(tr '\0' ' ' < "/proc/$agent/cmdline" 2>/dev/null) == "$prek session "* ]]; then
      kill "$agent" 2>/dev/null || true
    fi
  fi
  if [[ -f $run/launcher.pid && ! -f $run/session.status ]]; then
    kill "$(cat "$run/launcher.pid")" 2>/dev/null || true
  fi
  if [[ -n $broker ]]; then kill -TERM "$broker" 2>/dev/null || true; wait "$broker" 2>/dev/null || true; fi
  sudo -n rm -f "$rule" 2>/dev/null && sudo -n udevadm control --reload-rules 2>/dev/null || true
  if [[ -n $made_rules ]]; then sudo -n rmdir "$rules" 2>/dev/null || true; fi
  if [[ -n $made_registry ]]; then
    sudo -n rmdir "$run_root/prek/devices" "$run_root/prek" 2>/dev/null || true
  fi
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
# Blocks system sleep and idle locking until this script exits.
sudo systemd-inhibit --what=sleep:idle --who=prek-trial --why="A takeover trial is running" --mode=block \
  tail --pid=$$ -f /dev/null </dev/null >/dev/null 2>&1 &
if [[ -n $binary ]]; then
  printf 'Prebuilt binary: %s\n' "$(sha256sum "$binary")"
  prek=$binary
else
  nix --option eval-cache false --extra-experimental-features 'nix-command flakes' \
    --max-jobs 1 --cores 2 build --no-write-lock-file --out-link "$run/prek" "$repo#default"
  prek="$run/prek/bin/prek"
fi
cat > "$run/config.toml" <<CONFIG
version = 1
mode = "route"

[policy]
allowed_devices = ["trial"]
allowed_seats = ["seat0"]

[devices.trial]
name = "$device"
CONFIG
agent_flags=(--observations --context)
sh=$(command -v sh)
if [[ -n $swipe ]]; then
  cat >> "$run/config.toml" <<CONFIG

[[rules]]
id = "trial-swipe"
devices = ["trial"]
contexts = ["desktop"]
delivery = "buffer"
trigger = { kind = "swipe", fingers = 3, direction = "down", travel = "15mm", fallback_travel = "0.1norm" }
action = { kind = "command", argv = ["$sh", "-c", "date -u +%T.%N >> $run/fired.log"] }
CONFIG
  agent_flags+=(--commands)
fi
if [[ -n $keys ]]; then
  cat >> "$run/config.toml" <<CONFIG

[[rules]]
id = "trial-keys"
priority = 1
devices = ["trial"]
contexts = ["desktop"]
delivery = "buffer"
trigger = { kind = "swipe", fingers = 3, direction = "up", travel = "15mm", fallback_travel = "0.1norm" }
action = { kind = "keys", keys = ["volume_up"] }
CONFIG
  agent_flags+=(--keys)
fi
if [[ -n $edges ]]; then
  for edge in "bottom up 3" "top down 4"; do
    read -r side direction priority <<< "$edge"
    cat >> "$run/config.toml" <<CONFIG

[[rules]]
id = "trial-edge-$direction"
priority = $priority
devices = ["trial"]
contexts = ["desktop"]
delivery = "buffer"
region = { edge = "$side", depth = "20px" }
trigger = { kind = "swipe", fingers = 1, direction = "$direction", travel = "10mm", fallback_travel = "0.05norm" }
action = { kind = "command", argv = ["$sh", "-c", "echo $direction >> $run/edges.log"] }
CONFIG
  done
  if [[ -z $swipe ]]; then agent_flags+=(--commands); fi
fi
"$prek" check --config "$run/config.toml" --json > "$run/check.json"
# The compositor holding capabilities is not dumpable, so only root sees its descriptors.
display="$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
listeners=$(sudo ss -Hxlp src "$display" | grep -o 'pid=[0-9]*' | sort -u || true)
if [[ $(printf '%s\n' "$listeners" | grep -c '^pid=') != 1 ]]; then
  echo "Expected exactly one process listening on $display." >&2
  exit 1
fi
pid=${listeners#pid=}
if ! compositor=$(sudo readlink -e "/proc/$pid/exe"); then
  echo "Could not resolve the executable of compositor pid $pid." >&2
  exit 1
fi
cgroup=$(sed -n 's/^0:://p' "/proc/$pid/cgroup")
if [[ -z $cgroup || ! -f $cgroups$cgroup/cgroup.procs ]]; then
  echo "Could not resolve the cgroup of compositor pid $pid." >&2
  exit 1
fi
logind=$(readlink -f "${PREK_TRIAL_LOGIND:-/run/current-system/sw/lib/libsystemd.so.0}")
printf 'Compositor: %s (pid %s, cgroup %s)\nlibsystemd: %s\n' "$compositor" "$pid" "$cgroup" "$logind"
sudo install -d -m 0755 -o root -g root "$trial"
sudo install -m 0644 -o root -g root "$run/config.toml" "$trial/config.toml"
udev_rule=$run/prek/lib/udev/rules.d/70-prek.rules
if [[ -n $binary ]]; then
  sudo install -m 0755 -o root -g root "$binary" "$trial/prek"
  prek=$trial/prek
  udev_rule=$run/70-prek.rules
  sed "s|@prek@|$prek|" "$repo/udev/70-prek.rules" > "$udev_rule"
fi
# udev reads /run/udev/rules.d but does not create it.
if [[ ! -d $rules ]]; then
  sudo install -d -m 0755 -o root -g root "$rules"
  made_rules=1
fi
sudo install -m 0644 -o root -g root "$udev_rule" "$rule"
sudo udevadm control --reload-rules
# The broker creates its device registry under /run/prek when it is missing.
if [[ ! -d $run_root/prek ]]; then made_registry=1; fi
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
# The launcher waits until it is in the compositor's cgroup, then starts the agent.
cat > "$run/agent.sh" <<AGENT
#!/bin/sh
echo \$\$ > "$run/launcher.pid"
while [ ! -e "$run/agent.go" ]; do sleep 0.05; done
"$prek" session --socket "$socket" --display "$WAYLAND_DISPLAY" ${agent_flags[*]} \\
  --duration $((seconds - 2)) > "$run/session.log" 2>&1 &
echo \$! > "$run/session.pid"
wait \$!
echo \$? > "$run/session.status"
AGENT
chmod 0755 "$run/agent.sh"
"$run/agent.sh" </dev/null >/dev/null 2>&1 &
for _ in $(seq 50); do
  if [[ -s $run/launcher.pid ]]; then break; fi
  sleep 0.1
done
if [[ ! -s $run/launcher.pid ]]; then
  echo "The session agent launcher did not start within 5 s." >&2
  exit 1
fi
# shellcheck disable=SC2024
sudo tee -a "$cgroups$cgroup/cgroup.procs" < "$run/launcher.pid" > /dev/null
touch "$run/agent.go"
for _ in $(seq 50); do
  if [[ -s $run/session.pid ]]; then break; fi
  sleep 0.1
done
if [[ ! -s $run/session.pid ]]; then
  echo "The session agent did not start within 5 s." >&2
  exit 1
fi
if [[ -n $inject ]]; then
  printf '\nInjecting gestures into %s once "%s" is taken over; leave it alone\n' "$node" "$device"
  printf 'for %d seconds, or press Ctrl-C; the broker then releases it.\n\n' "$seconds"
else
  printf '\nType and use "%s" normally for %d seconds. If it stops responding,\n' "$device" "$seconds"
  printf 'wait for the timer or press Ctrl-C; the broker then releases it.\n\n'
fi
if [[ -n $swipe && -z $inject ]]; then
  printf 'Also swipe down with three fingers a few times; each swipe should run\n'
  printf 'the trial command instead of reaching the application under your fingers.\n\n'
fi
if [[ -n $keys && -z $inject ]]; then
  printf 'Also swipe up with three fingers a few times; each swipe should raise\n'
  printf 'the volume one step instead of reaching the application under your fingers.\n\n'
fi
if [[ -n $edges && -z $inject ]]; then
  printf 'Also swipe up with one finger from the very bottom edge and down from the\n'
  printf 'very top edge; each should be logged instead of reaching the application.\n\n'
fi
if [[ -n $lock ]]; then
  printf 'Lock the session (power key), wait a few seconds, wake and unlock it,\n'
  printf 'then keep using "%s" until the timer ends.\n\n' "$device"
fi
# Holds while the last takeover is newer than the last context loss.
held() {
  awk '/lost its context/ { n = 0 } /taken over at a neutral idle cut/ { n = 1 } END { exit !n }' \
    "$run/broker.log"
}
downs=0
lates=0
ups=0
edge_ups=0
edge_downs=0
if [[ -n $inject ]]; then
  for _ in $(seq "$((seconds * 10))"); do
    if held || [[ -s $run/session.status ]]; then break; fi
    sleep 0.1
  done
  sleep 1
  steps=(swipe:1:up)
  if [[ -n $swipe ]]; then
    steps+=(swipe:3:down swipe:3:down swipe:3:down "swipe:3:down:0.5,0.25:200")
  fi
  if [[ -n $keys ]]; then steps+=(swipe:3:up swipe:3:up swipe:3:up); fi
  if [[ -n $edges ]]; then
    for _ in 1 2; do steps+=("swipe:1:up:0.5,0.995" "swipe:1:down:0.5,0.005"); done
  fi
  for step in "${steps[@]}"; do
    if ! held; then
      echo "\"$device\" is not taken over, so $step and later gestures were not injected."
      break
    fi
    sudo "$perl" "$repo/tests/support/inject-input.pl" "$node" "$step" wait:1000 | tee -a "$run/inject.log"
    case "$step" in
      swipe:3:down) downs=$((downs + 1)) ;;
      swipe:3:down:*) lates=$((lates + 1)) ;;
      swipe:3:up) ups=$((ups + 1)) ;;
      swipe:1:up:*) edge_ups=$((edge_ups + 1)) ;;
      swipe:1:down:*) edge_downs=$((edge_downs + 1)) ;;
    esac
  done
fi
for _ in $(seq "$((seconds * 10 + 100))"); do
  if [[ -s $run/session.status ]]; then break; fi
  sleep 0.1
done
agent_status=$(cat "$run/session.status" 2>/dev/null || echo unknown)
if [[ $agent_status != 0 ]]; then
  echo "session agent exited with status $agent_status:"
  tail -n 5 "$run/session.log" || true
fi
broker_status=0
wait "$broker" || broker_status=$?
broker=""
grep -E 'taken over|stays with its original consumer|keeps no consumer evidence|forwarded' \
  "$run/broker.log" || true
if ((broker_status != 0)); then
  echo "The broker exited with status $broker_status:"
  grep -E '^prek:|failed' "$run/broker.log" | tail -n 5 || true
fi
if ! grep -q 'taken over at a neutral idle cut' "$run/broker.log"; then
  echo "\"$device\" was never taken over." >&2
  exit 1
fi
forwarded=$(grep -F "routing worker: $device forwarded " "$run/broker.log" | tail -n 1 || true)
if [[ -n $inject && ( -z $forwarded || $forwarded == *" forwarded no frames" ) ]]; then
  echo "The injected one-finger swipe was never forwarded to the compositor." >&2
  exit 1
fi
losses=$(sed -n '/taken over at a neutral idle cut/,$p' "$run/broker.log" | grep -c 'lost its context' || true)
if [[ -n $lock ]]; then
  retaken=$(awk '/lost its context/ { n = 0 } /taken over at a neutral idle cut/ { n++ } END { print n + 0 }' \
    "$run/broker.log")
  printf 'Context losses after takeover: %d; takeovers after the last loss: %d\n' "$losses" "$retaken"
  if ((losses == 0)); then
    echo "The seat never left its desktop, so the lock was not exercised." >&2
    exit 1
  fi
  if ((retaken == 0)); then
    echo "\"$device\" was not taken over again after the last unlock." >&2
    exit 1
  fi
elif ((losses > 0)); then
  echo "The seat lost its context $losses times, so the takeover did not hold." >&2
  exit 1
fi
if [[ $agent_status != 0 ]]; then
  echo "The session agent exited with status $agent_status, so the takeover was not verified." >&2
  exit 1
fi
if [[ -n $lock ]]; then
  last=$(grep -E '^context [A-Za-z]+;' "$run/session.log" | tail -n 1)
  if [[ -n $last && $last != "context Desktop;"* ]]; then
    echo "The session ended outside its desktop: $last" >&2
    exit 1
  fi
else
  left=$(grep -E '^context [A-Za-z]+;' "$run/session.log" | grep -vc '^context Desktop;' || true)
  if ((left > 0)); then
    echo "The seat's context left Desktop $left times during the trial." >&2
    exit 1
  fi
fi
if [[ -n $swipe ]]; then
  matched=$(grep -c '^action offered: trial-swipe ' "$run/session.log" || true)
  fired=0
  if [[ -f $run/fired.log ]]; then fired=$(wc -l < "$run/fired.log"); fi
  grep -E '^action ' "$run/session.log" | tail -n 20 || true
  printf 'Swipes recognized: %d; commands run: %d\n' "$matched" "$fired"
  if ((fired == 0)); then
    echo "The three-finger swipe never ran its command." >&2
    exit 1
  fi
  if [[ -n $inject ]] && ((fired != downs)); then
    echo "The three-finger swipe ran its command $fired times for $downs injected swipes" \
      "and $lates late-joining ones that must not run it." >&2
    exit 1
  fi
fi
if [[ -n $keys ]]; then
  pressed=$(sed -n 's/.*rule trial-keys queued its keys \([0-9]*\) times.*/\1/p' "$run/broker.log" | tail -n 1)
  pressed=${pressed:-0}
  printf 'Swipes up recognized: %d; each should have raised the volume\n' "$pressed"
  if ((pressed == 0)); then
    echo "The three-finger swipe up was never recognized." >&2
    exit 1
  fi
  if [[ -n $inject ]] && ((pressed != ups)); then
    echo "The three-finger swipe up was recognized $pressed times for $ups injected swipes." >&2
    exit 1
  fi
fi
if [[ -n $edges ]]; then
  seen_ups=0
  seen_downs=0
  if [[ -f $run/edges.log ]]; then
    seen_ups=$(grep -cx up "$run/edges.log" || true)
    seen_downs=$(grep -cx down "$run/edges.log" || true)
  fi
  printf 'Edge swipes run: %d up from the bottom, %d down from the top\n' "$seen_ups" "$seen_downs"
  if ((seen_ups + seen_downs == 0)); then
    echo "No edge swipe ran its command." >&2
    exit 1
  fi
  if [[ -n $inject ]] && ((seen_ups != edge_ups || seen_downs != edge_downs)); then
    echo "Injected $edge_ups bottom and $edge_downs top edge swipes." >&2
    exit 1
  fi
fi
if ((broker_status != 0)); then
  echo "The broker exited with status $broker_status, so the trial did not end cleanly." >&2
  exit 1
fi
