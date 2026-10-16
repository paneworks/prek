#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/project/tests"
cp -- "$repo/tests/install-ubuntu-compositors.sh" "$scratch/project/tests/"
cat > "$scratch/bin/curl" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
case ${*: -1} in
  */cage-kiosk/cage/releases/latest) tag=v0.3.1 ;;
  */hyprwm/Hyprland/releases/latest) tag=v0.56.2 ;;
  *) exit 91 ;;
esac
printf '{"draft":false,"prerelease":false,"tag_name":"%s"}\n' "$tag"
MOCK
cat > "$scratch/bin/nix" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
case " $* " in
  *' flake metadata '*) printf '{"locked":{"rev":"1111111111111111111111111111111111111111"}}\n' ;;
  *'.cage.version '*) printf '%s' "${MOCK_CAGE_VERSION:-0.3.1}" ;;
  *'.hyprland.version '*) printf '%s' "${MOCK_HYPRLAND_VERSION:-0.56.2}" ;;
  *) echo 'Unexpected Nix operation' >&2; exit 92 ;;
esac
MOCK
cat > "$scratch/bin/sudo" <<'MOCK'
#!/usr/bin/env bash
echo 'Unexpected sudo call' >&2
exit 93
MOCK
chmod +x "$scratch/bin/"*
export PATH="$scratch/bin:$PATH"
script="$scratch/project/tests/install-ubuntu-compositors.sh"
"$script" --check > "$scratch/matching.log" 2>&1
jq -e '.cage == "v0.3.1" and .hyprland == "v0.56.2"' \
  "$scratch/project/target/compositor-install/latest/manifest.json" >/dev/null
MOCK_HYPRLAND_VERSION=0.55.0 "$script" --check > "$scratch/fallback.log" 2>&1
jq -e '.hyprland_ref == "github:hyprwm/Hyprland/1111111111111111111111111111111111111111#hyprland"' \
  "$scratch/project/target/compositor-install/latest/manifest.json" >/dev/null
if MOCK_CAGE_VERSION=0.2.1 "$script" --check > "$scratch/stale.log" 2>&1; then
  echo 'Stale Cage unexpectedly accepted' >&2; exit 1
fi
grep -q 'refusing an outdated installation' "$scratch/stale.log"
if "$script" --invalid > "$scratch/invalid.log" 2>&1; then
  echo 'Invalid argument unexpectedly accepted' >&2; exit 1
fi
if PREK_BUILD_CORES=99 "$script" --check > "$scratch/cores.log" 2>&1; then
  echo 'Unbounded core count unexpectedly accepted' >&2; exit 1
fi
if bash "$repo/tests/support/install-compositors-system.sh" > "$scratch/root.log" 2>&1; then
  echo 'System installer unexpectedly accepted an unprivileged invocation' >&2; exit 1
fi
echo 'Six installer checks passed; no builds, sudo or system installation.'
