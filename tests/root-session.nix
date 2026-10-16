{ pkgs, probe, fixture }:
let
  policy = pkgs.writeText "prek-root-session-policy.json" (builtins.toJSON {
    inherit probe;
    fixture = "${fixture}/bin/prek-cage-host-test";
    tools = {
      systemctl = "${pkgs.systemd}/bin/systemctl";
      loginctl = "${pkgs.systemd}/bin/loginctl";
      systemd-run = "${pkgs.systemd}/bin/systemd-run";
      journalctl = "${pkgs.systemd}/bin/journalctl";
      chvt = "${pkgs.kbd}/bin/chvt";
    };
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
