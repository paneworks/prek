{ pkgs }:
let
  receiver = import ./wayland-receiver.nix { inherit pkgs; };
  cage = pkgs.cage.overrideAttrs (old: {
    pname = "prek-cage-touch-fixture";
    mesonFlags = (old.mesonFlags or []) ++ [ "-Dwerror=true" ];
    nativeBuildInputs = (old.nativeBuildInputs or []) ++ [ pkgs.python3 ];
    postPatch = (old.postPatch or "") + ''
      cp ${./support/cage_touch.c} prek_test_touch.c
      python3 - <<'PY'
      from pathlib import Path
      p=Path('meson.build'); s=p.read_text(); assert "  'cage.c'," in s
      p.write_text(s.replace("  'cage.c',", "  'cage.c',\n  'prek_test_touch.c',", 1))
      p=Path('cage.c'); s=p.read_text(); marker='#include "seat.h"'; assert marker in s
      s=s.replace(marker, marker+'\nbool prek_test_touch_allowed(void);\nbool prek_test_touch_start(struct cg_server *server);', 1)
      marker='server.backend = wlr_backend_autocreate(event_loop, &server.session);'; assert marker in s
      s=s.replace(marker, 'if (!prek_test_touch_allowed()) { ret = 1; goto end; }\n\t'+marker, 1)
      marker='server.seat = seat_create(&server, server.backend);'; assert marker in s
      # Start after the seat failure check, before any primary client is forked.
      marker='server.idle = wlr_idle_notifier_v1_create(server.wl_display);'; assert marker in s
      s=s.replace(marker, 'if (!prek_test_touch_start(&server)) { ret = 1; goto end; }\n\t'+marker, 1)
      p.write_text(s)
      PY
    '';
  });
in
pkgs.writeShellScriptBin "prek-cage-touch-test" ''
  exec ${pkgs.dbus}/bin/dbus-run-session --config-file=${./support/dbus-session.conf} -- \
    ${pkgs.bash}/bin/bash -c 'export PREK_TEST_SESSION_BUS="$DBUS_SESSION_BUS_ADDRESS"; exec "$@"' -- \
    ${pkgs.python3}/bin/python3 ${./cage-touch.py} \
    --cage ${cage}/bin/cage --receiver ${receiver}/bin/prek-wayland-receiver "$@"
''
