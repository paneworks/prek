#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
if [[ ${1:-} == --help ]]; then
  cat <<'HELP'
Usage: ./tests/install-ubuntu-compositors.sh [--check]
Run as your desktop user, NOT with sudo. Requires existing Nix, curl and jq.
Resolves latest stable upstream releases, builds with Nix (one job/two cores),
then prompts for sudo to install system-wide commands under /usr/local/bin.
--check resolves versions only: no builds, sudo or installation.
Keeps Ubuntu packages, desktop settings, services and GPU drivers unchanged.
Logs: target/compositor-install/latest/{console.log,status.json,manifest.json}
HELP
  exit 0
fi
if (( EUID == 0 )) || [[ $# -gt 1 || ${1:-} != '' && ${1:-} != --check ]]; then
  echo 'Run as your regular user: ./tests/install-ubuntu-compositors.sh [--check]' >&2
  exit 2
fi
for tool in nix curl jq sudo flock; do
  command -v "$tool" >/dev/null || { echo "Required command missing: $tool" >&2; exit 2; }
done
# shellcheck disable=SC1091
. /etc/os-release
[[ $ID == ubuntu ]] || { echo 'This installer is for Ubuntu.' >&2; exit 2; }
jobs=${PREK_BUILD_CORES:-2}
[[ $jobs =~ ^[1-8]$ ]] || { echo 'PREK_BUILD_CORES must be 1..8.' >&2; exit 2; }
mkdir -p "$repo/target/compositor-install"
exec 9> "$repo/target/compositor-install/lock"
flock -n 9 || { echo 'Another compositor installation is running.' >&2; exit 2; }
run=$(mktemp -d "$repo/target/compositor-install/run-$(date -u +%Y%m%dT%H%M%SZ)-XXXXXX")
ln -sfn -- "$run" "$repo/target/compositor-install/latest"
exec > >(tee -a "$run/console.log") 2>&1
finish() {
  result=$?
  trap - EXIT
  status=failed
  if (( result == 0 )); then status=passed; fi
  printf '{"status":"%s","exit_code":%d,"mode":"%s"}\n' "$status" "$result" "$mode" > "$run/status.json"
  printf '\n%s (exit %d). Logs: %s\n' "$status" "$result" "$run"
  exit "$result"
}
mode=install
if [[ ${1:-} == --check ]]; then mode=check; fi
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf '{"status":"running"}\n' > "$run/status.json"
nix_cmd=(nix --extra-experimental-features 'nix-command flakes'
  --option eval-cache false --option accept-flake-config false --max-jobs 1 --cores "$jobs")
release() {
  curl --fail --silent --show-error --location --retry 2 --proto '=https' \
    --connect-timeout 15 --max-time 90 "https://api.github.com/repos/$1/releases/latest"
}
release cage-kiosk/cage > "$run/cage-release.json"
release hyprwm/Hyprland > "$run/hyprland-release.json"
cage_tag=$(jq -er 'select(.draft == false and .prerelease == false) | .tag_name' "$run/cage-release.json")
hypr_tag=$(jq -er 'select(.draft == false and .prerelease == false) | .tag_name' "$run/hyprland-release.json")
for tag in "$cage_tag" "$hypr_tag"; do
  [[ $tag =~ ^v?[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Unexpected stable release: $tag" >&2; exit 2; }
done
printf 'Latest stable releases: Cage %s; Hyprland %s\n' "$cage_tag" "$hypr_tag"
"${nix_cmd[@]}" flake metadata --refresh --no-write-lock-file --json \
  github:NixOS/nixpkgs/nixos-unstable > "$run/nixpkgs.json"
rev=$(jq -er '.locked.rev | select(test("^[0-9a-f]{40}$"))' "$run/nixpkgs.json")
pkgs="github:NixOS/nixpkgs/$rev"
case $(uname -m) in
  x86_64) system=x86_64-linux ;;
  aarch64) system=aarch64-linux ;;
  *) echo 'Unsupported architecture.' >&2; exit 2 ;;
esac
cage_ref="$pkgs#legacyPackages.$system.cage"
hypr_ref="$pkgs#legacyPackages.$system.hyprland"
cage_version=$("${nix_cmd[@]}" eval --no-write-lock-file --raw "$cage_ref.version")
hypr_version=$("${nix_cmd[@]}" eval --no-write-lock-file --raw "$hypr_ref.version")
[[ $cage_version == "${cage_tag#v}" ]] || {
  echo "Nixpkgs Cage $cage_version is not upstream $cage_tag; refusing an outdated installation." >&2
  exit 2
}
if [[ $hypr_version != "${hypr_tag#v}" ]]; then
  "${nix_cmd[@]}" flake metadata --no-write-lock-file --json \
    "github:hyprwm/Hyprland/$hypr_tag" > "$run/hyprland-source.json"
  hypr_rev=$(jq -er '.locked.rev | select(test("^[0-9a-f]{40}$"))' "$run/hyprland-source.json")
  hypr_ref="github:hyprwm/Hyprland/$hypr_rev#hyprland"
fi
jq -n --arg cage "$cage_tag" --arg hyprland "$hypr_tag" --arg cage_ref "$cage_ref" \
  --arg hyprland_ref "$hypr_ref" --arg nixpkgs "$rev" --arg system "$system" \
  '{cage:$cage,hyprland:$hyprland,cage_ref:$cage_ref,hyprland_ref:$hyprland_ref,nixpkgs:$nixpkgs,system:$system}' > "$run/manifest.json"
cat "$run/manifest.json"
if [[ ${1:-} == --check ]]; then exit 0; fi
"${nix_cmd[@]}" build --no-write-lock-file --out-link "$run/cage" "$cage_ref"
"${nix_cmd[@]}" build --no-write-lock-file --out-link "$run/hyprland" "$hypr_ref"
cage=$(readlink -f "$run/cage")
hyprland=$(readlink -f "$run/hyprland")
"$cage/bin/cage" -v > "$run/cage-version.txt" 2>&1
"$hyprland/bin/Hyprland" --version > "$run/hyprland-version.txt" 2>&1
grep -F -- "${cage_tag#v}" "$run/cage-version.txt"
grep -F -- "${hypr_tag#v}" "$run/hyprland-version.txt"
jq --arg cage "$cage" --arg hyprland "$hyprland" \
  '. + {cage_store:$cage,hyprland_store:$hyprland}' "$run/manifest.json" > "$run/manifest.tmp"
mv -- "$run/manifest.tmp" "$run/manifest.json"
printf '\nBuilds verified. Installing /opt/prek-compositors and /usr/local/bin commands.\n'
sudo /usr/bin/bash "$repo/tests/support/install-compositors-system.sh" "$cage" "$hyprland" "$run"
printf '\nInstalled. No compositor/session was started. Ubuntu packages remain installed.\n'
