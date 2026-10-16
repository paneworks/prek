{ pkgs, probe, fixture }:
let
  policy = pkgs.writeText "prek-root-session-policy.json" (builtins.toJSON {
    inherit probe;
    fixture = "${fixture}/bin/prek-cage-host-test";
  });
in
pkgs.writeShellScriptBin "prek-cage-root-test" ''
  if [ "$(${pkgs.coreutils}/bin/id -u)" -eq 0 ]; then
    echo 'Run the launcher as your regular user, without a sudo prefix.' >&2
    exit 2
  fi
  exec sudo -- ${pkgs.python3}/bin/python3 ${./support/root_session.py} \
    "$@" --policy ${policy} --bootstrap
''
