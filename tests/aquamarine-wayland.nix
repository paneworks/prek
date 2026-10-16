{ pkgs }:
assert pkgs.aquamarine.version == "0.15.1";
pkgs.aquamarine.overrideAttrs (old: {
  patches = (old.patches or [ ]) ++ [ ./support/aquamarine-wayland-startup.patch ];
})
