#!/usr/bin/env bash
set -euo pipefail
if (( $# > 1 )) || { (( $# == 1 )) && [[ $1 != /* ]]; }; then
  echo 'Usage: bash tests/run-nix-gate.sh [ABSOLUTE_OUT_LINK]' >&2
  exit 2
fi
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$repo"
nix=(nix --option eval-cache false --extra-experimental-features 'nix-command flakes' --max-jobs 1 --cores 2)
system=$("${nix[@]}" eval --impure --raw --expr builtins.currentSystem)
case $system in
  x86_64-linux|aarch64-linux) ;;
  *) echo "Unsupported native system: $system" >&2; exit 1 ;;
esac
inventory=$("${nix[@]}" eval --no-write-lock-file --raw ".#checks.$system" \
  --apply 'checks: builtins.concatStringsSep "\n" (builtins.attrNames checks)')
if [[ -z $inventory ]]; then
  echo 'The native flake check inventory is empty.' >&2
  exit 1
fi
mapfile -t names <<< "$inventory"
if (( ${#names[@]} > 256 )); then echo 'Too many native flake checks.' >&2; exit 1; fi
declare -A seen=()
build=(build --no-write-lock-file)
if (( $# == 1 )); then build+=(--out-link "$1"); else build+=(--no-link); fi
for name in "${names[@]}"; do
  if [[ ! $name =~ ^[a-z][a-z0-9-]*$ || ${seen[$name]+present} ]]; then
    echo "Invalid or duplicate native check: $name" >&2
    exit 1
  fi
  seen[$name]=1
  build+=(".#checks.$system.$name")
done
printf 'Native Nix gate: %s, %d checks\n' "$system" "${#names[@]}"
printf '  %s\n' "${names[@]}"
exec "${nix[@]}" "${build[@]}"
