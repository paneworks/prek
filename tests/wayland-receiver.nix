{ pkgs }:
pkgs.stdenv.mkDerivation {
  pname = "prek-wayland-receiver";
  version = "1";
  src = ./support/wayland_receiver.c;
  dontUnpack = true;
  nativeBuildInputs = [ pkgs.pkg-config pkgs.wayland-scanner ];
  buildInputs = [ pkgs.wayland pkgs.libxkbcommon pkgs.cairo ];
  buildPhase = ''
    runHook preBuild
    wayland-scanner client-header ${pkgs.wayland-protocols}/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml xdg-shell-client-protocol.h
    wayland-scanner private-code ${pkgs.wayland-protocols}/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml xdg-shell-protocol.c
    wayland-scanner client-header ${pkgs.wayland-protocols}/share/wayland-protocols/stable/presentation-time/presentation-time.xml presentation-time-client-protocol.h
    wayland-scanner private-code ${pkgs.wayland-protocols}/share/wayland-protocols/stable/presentation-time/presentation-time.xml presentation-time-protocol.c
    $CC -std=c11 -Wall -Wextra -Werror -Wno-unused-parameter -Wno-misleading-indentation -O2 -I. "$src" xdg-shell-protocol.c presentation-time-protocol.c \
      $(pkg-config --cflags --libs wayland-client xkbcommon cairo) -o prek-wayland-receiver
    runHook postBuild
  '';
  installPhase = ''
    mkdir -p "$out/bin"
    install -m 755 prek-wayland-receiver "$out/bin/"
  '';
}
