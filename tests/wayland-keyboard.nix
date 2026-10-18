{ pkgs }:
pkgs.stdenv.mkDerivation {
  pname = "prek-wayland-keyboard";
  version = "1";
  src = ./support/wayland_keyboard.c;
  dontUnpack = true;
  nativeBuildInputs = [ pkgs.pkg-config pkgs.wayland-scanner ];
  buildInputs = [ pkgs.wayland pkgs.libxkbcommon ];
  buildPhase = ''
    for protocol in keyboard pointer; do
      source=${pkgs.wlroots_0_19.src}/protocol/virtual-$protocol-unstable-v1.xml
      if [ "$protocol" = pointer ]; then source=${pkgs.wlroots_0_19.src}/protocol/wlr-virtual-pointer-unstable-v1.xml; fi
      wayland-scanner client-header "$source" virtual-$protocol-client-protocol.h
      wayland-scanner private-code "$source" virtual-$protocol-protocol.c
    done
    $CC -std=c11 -Wall -Wextra -Werror -O2 -I. "$src" virtual-keyboard-protocol.c virtual-pointer-protocol.c \
      $(pkg-config --cflags --libs wayland-client xkbcommon) -o prek-wayland-keyboard
  '';
  installPhase = ''
    mkdir -p "$out/bin"
    install -m 755 prek-wayland-keyboard "$out/bin/"
  '';
}
