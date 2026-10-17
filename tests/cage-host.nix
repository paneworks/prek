{ pkgs, package, rootProbe ? null, cage ? pkgs.cage, hyprland ? pkgs.hyprland, aquamarine ? null, stockAquamarine ? null }:
assert aquamarine == null || stockAquamarine == null;
let
  receiver = import ./wayland-receiver.nix { inherit pkgs; };
  runtime = import ./cage-runtime.nix { inherit package; };
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
  export MORF_FONT_PATH="${pkgs.material-symbols}/share/fonts:${pkgs.inter}/share/fonts''${MORF_FONT_PATH:+:$MORF_FONT_PATH}"
  ${pkgs.lib.optionalString (aquamarine != null) ''
    export LD_LIBRARY_PATH="${aquamarine}/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    export PREK_CAGE_AQUAMARINE=${aquamarine}
    echo 'TEST-ONLY Aquamarine Wayland startup patch; not unmodified stock acceptance.' >&2
  ''}
  ${pkgs.lib.optionalString (stockAquamarine != null) ''
    export PREK_CAGE_STOCK_AQUAMARINE=${stockAquamarine}
    echo 'Stock pinned Cage ${cage.version}, Hyprland ${hyprland.version}, Aquamarine ${stockAquamarine.version}; no Prek dependency override.' >&2
  ''}
  exec ${pkgs.dbus}/bin/dbus-run-session --config-file=${./support/dbus-session.conf} -- \
    ${pkgs.bash}/bin/bash -c 'export PREK_TEST_SESSION_BUS="$DBUS_SESSION_BUS_ADDRESS"; exec "$@"' -- \
    ${pkgs.python3}/bin/python3 ${./cage-host.py} \
    --cage ${cage}/bin/cage \
    --hyprland ${hyprland}/bin/Hyprland \
    --inner ${./cage-inner.py} --proxy ${./support/wlproxy.py} --morf-lock-check ${./support/morf-lock.py} \
    --morf-control-check ${./support/morf-controls.py} \
    --keyboard-quiesce-check ${./support/keyboard_quiesce.py} \
    --screencopy ${pkgs.grim}/bin/grim \
    --receiver ${receiver}/bin/prek-wayland-receiver \
    --keyboard ${keyboard}/bin/prek-wayland-keyboard \
    ${pkgs.lib.optionalString (rootProbe != null) "--morf-root-probe ${rootProbe}"} \
    --prek-fixture ${runtime}/bin/prek-local-cage-tests --prek-app ${package}/bin/prek "$@"
''
