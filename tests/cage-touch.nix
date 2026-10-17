{ pkgs, package }:
assert pkgs.cage.version == "0.2.1";
let
  receiver = import ./wayland-receiver.nix { inherit pkgs; };
  runtime = import ./cage-runtime.nix { inherit package; testName = "cage_touch_runtime"; };
  cage = pkgs.cage.overrideAttrs (old: {
    pname = "prek-cage-touch-fixture";
    mesonFlags = (old.mesonFlags or []) ++ [ "-Dwerror=true" ];
    postPatch = (old.postPatch or "") + ''
      patch --batch --fuzz=0 -p1 < ${./support/cage-touch.patch}
      cp ${./support/cage_touch.c} prek_test_touch.c
    '';
  });
in
pkgs.writeShellScriptBin "prek-cage-touch-test" ''
  exec ${pkgs.dbus}/bin/dbus-run-session --config-file=${./support/dbus-session.conf} -- \
    ${pkgs.bash}/bin/bash -c 'export PREK_TEST_SESSION_BUS="$DBUS_SESSION_BUS_ADDRESS"; exec "$@"' -- \
    ${pkgs.python3}/bin/python3 ${./cage-touch.py} \
    --cage ${cage}/bin/cage --receiver ${receiver}/bin/prek-wayland-receiver \
    --fixture ${runtime}/bin/prek-local-cage-tests --app ${package}/bin/prek \
    --screencopy ${pkgs.grim}/bin/grim "$@"
''
