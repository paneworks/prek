{ pkgs }:
let
  receiver = import ./wayland-receiver.nix { inherit pkgs; };
  keyboard = pkgs.stdenv.mkDerivation {
    pname = "prek-wayland-keyboard";
    version = "1";
    src = ./support/wayland_keyboard.c;
    dontUnpack = true;
    nativeBuildInputs = [ pkgs.pkg-config pkgs.wayland-scanner ];
    buildInputs = [ pkgs.wayland pkgs.libxkbcommon ];
    buildPhase = ''
      wayland-scanner client-header ${pkgs.wlroots_0_19.src}/protocol/virtual-keyboard-unstable-v1.xml virtual-keyboard-client-protocol.h
      wayland-scanner private-code ${pkgs.wlroots_0_19.src}/protocol/virtual-keyboard-unstable-v1.xml virtual-keyboard-protocol.c
      $CC -std=c11 -Wall -Wextra -Werror -O2 -I. "$src" virtual-keyboard-protocol.c \
        $(pkg-config --cflags --libs wayland-client xkbcommon) -o prek-wayland-keyboard
    '';
    installPhase = ''
      mkdir -p "$out/bin"
      install -m 755 prek-wayland-keyboard "$out/bin/"
    '';
  };
in
pkgs.writeShellScriptBin "prek-cage-host-test" ''
  exec ${pkgs.python3}/bin/python3 ${./cage-host.py} \
    --cage ${pkgs.cage}/bin/cage \
    --receiver ${receiver}/bin/prek-wayland-receiver \
    --keyboard ${keyboard}/bin/prek-wayland-keyboard "$@"
''
