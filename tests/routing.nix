# Disposable VM only: no host event nodes, USB devices or production disks.
{ pkgs, package }:
let
  receiver = package.overrideAttrs (_: {
    pname = "prek-routing-tests";
    cargoBuildFlags = [ "--test" "routing" ];
    doCheck = false;
    installPhase = ''
      runHook preInstall
      mkdir -p "$out/bin"
      count=0
      for candidate in target/*/release/deps/routing-* target/release/deps/routing-*; do
        if [ -f "$candidate" ] && [ -x "$candidate" ]; then
          install -m 755 "$candidate" "$out/bin/prek-routing-tests"
          count=$((count + 1))
        fi
      done
      test "$count" -eq 1
      runHook postInstall
    '';
    postInstall = "";
  });
in
pkgs.testers.runNixOSTest {
  name = "prek-isolated-routing";
  nodes.machine = {
    boot.kernelModules = [ "uinput" ];
    services.udev.packages = [ package ];
    systemd.tmpfiles.rules = [ "d /run/prek/devices 0700 root root -" ];
    environment.systemPackages = [ package receiver ];
    virtualisation.memorySize = 768;
    virtualisation.cores = 2;
    system.stateVersion = "25.11";
  };
  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.succeed("test -c /dev/uinput")
    machine.succeed("udevadm settle")
    machine.succeed("PREK_ISOLATED_ROUTING=1 ${receiver}/bin/prek-routing-tests --ignored --test-threads=1 --nocapture", timeout=60)
    machine.succeed("test -z \"$(ls -A /run/prek/devices)\"")
  '';
}
