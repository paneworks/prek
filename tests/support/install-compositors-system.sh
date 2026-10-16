#!/usr/bin/env bash
set -euo pipefail
if (( EUID != 0 || $# != 3 )); then
  echo 'Use tests/install-ubuntu-compositors.sh as your regular user.' >&2
  exit 2
fi
cage=$1 hyprland=$2 run=$3
for store in "$cage" "$hyprland"; do
  [[ $store =~ ^/nix/store/[a-z0-9]{32}-[^/]+$ && -d $store ]] || exit 2
done
[[ -f $run/manifest.json && -x $cage/bin/cage && -x $hyprland/bin/Hyprland && -x $hyprland/bin/hyprctl ]] || exit 2
prefix=/opt/prek-compositors
exec 9> /run/lock/prek-compositors.lock
flock -n 9 || { echo 'Another system installation is running.' >&2; exit 2; }
if [[ -e $prefix || -L $prefix ]]; then
  [[ -d $prefix && ! -L $prefix && -f $prefix/.prek-managed ]] || {
    echo "Refusing unmanaged installation: $prefix" >&2; exit 2;
  }
fi
names=(cage Hyprland hyprctl)
if [[ -x $hyprland/bin/start-hyprland ]]; then names+=(start-hyprland); fi
for name in "${names[@]}"; do
  path=/usr/local/bin/$name
  if [[ -e $path || -L $path ]]; then
    [[ -L $path && $(readlink -- "$path") == "$prefix/current/bin/$name" ]] || {
      echo "Refusing to overwrite existing command: $path" >&2; exit 2;
    }
  fi
done
install -d -m 755 "$prefix/releases" /usr/local/bin /nix/var/nix/gcroots/prek-compositors
touch "$prefix/.prek-managed"
release=$(mktemp -d "$prefix/releases/$(date -u +%Y%m%dT%H%M%SZ)-XXXXXX")
chmod 755 "$release"
install -d -m 755 "$release/bin"
install -m 644 -- "$run/manifest.json" "$release/manifest.json"
ln -s -- "$cage/bin/cage" "$release/bin/cage"
for name in "${names[@]:1}"; do ln -s -- "$hyprland/bin/$name" "$release/bin/$name"; done
roots=/nix/var/nix/gcroots/prek-compositors/$(basename -- "$release")
install -d -m 755 "$roots"
ln -s -- "$cage" "$roots/cage"
ln -s -- "$hyprland" "$roots/hyprland"
ln -s -- "$release" "$release/current-link"
mv -Tf -- "$release/current-link" "$prefix/current"
for name in "${names[@]}"; do ln -sfn -- "$prefix/current/bin/$name" "/usr/local/bin/$name"; done
printf 'System installation: %s\nPrevious releases retained in: %s/releases\n' "$release" "$prefix"
