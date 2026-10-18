{ pkgs, package, rootProbe ? null, cage ? pkgs.cage, hyprland ? pkgs.hyprland, aquamarine ? null, stockAquamarine ? null }:
assert aquamarine == null || stockAquamarine == null;
let
  receiver = import ./wayland-receiver.nix { inherit pkgs; };
  runtime = import ./cage-runtime.nix { inherit package; };
  keyRuntime = import ./cage-runtime.nix { inherit package; testName = "key_output_cage"; features = [ "native-test-harness" ]; };
  keyboard = import ./wayland-keyboard.nix { inherit pkgs; };
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
    --key-output-fixture ${keyRuntime}/bin/prek-local-cage-tests \
    --screencopy ${pkgs.grim}/bin/grim \
    --receiver ${receiver}/bin/prek-wayland-receiver \
    --keyboard ${keyboard}/bin/prek-wayland-keyboard \
    ${pkgs.lib.optionalString (rootProbe != null) "--morf-root-probe ${rootProbe}"} \
    --prek-fixture ${runtime}/bin/prek-local-cage-tests --prek-app ${package}/bin/prek "$@"
''
