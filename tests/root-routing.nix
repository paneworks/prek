{ pkgs, package }:
let
  # No caller-selected command, source checkout or output path is elevated.
  probe = pkgs.writeShellScript "prek-isolated-root-routing-probe" ''
    test "$#" -eq 0 || exit 2
    exec ${pkgs.coreutils}/bin/env -i \
      ${pkgs.python3}/bin/python3 ${./support/root_routing.py} \
      --tests ${package.nativeTests}/libexec/prek-routing-tests \
      --importer ${package}/bin/prek --udevadm ${pkgs.systemd}/bin/udevadm
  '';
in pkgs.writeShellScriptBin "prek-root-routing-test" ''
  test "$#" -eq 0 || exit 2
  if [ "$(${pkgs.coreutils}/bin/id -u)" -eq 0 ]; then
    echo 'Launch tests/run-root-routing.sh as your regular user; only its fixed probe uses sudo.' >&2
    exit 2
  fi
  sudo -v || exit $?
  exec sudo -n -- ${probe}
''
