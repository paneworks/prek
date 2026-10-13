# Disposable VM only: no host event nodes, USB devices or production disks.
{ pkgs, package }:
let
  receiver = package.overrideAttrs (_: {
    pname = "prek-routing-tests";
    cargoBuildFlags = [ "--test" "routing" "--test" "supervisor" "--test" "recovery" ];
    doCheck = false;
    installPhase = ''
      runHook preInstall
      mkdir -p "$out/bin"
      for suite in routing supervisor recovery; do
        count=0
        for candidate in target/*/release/deps/$suite-* target/release/deps/$suite-*; do
          if [ -f "$candidate" ] && [ -x "$candidate" ]; then
            install -m 755 "$candidate" "$out/bin/prek-$suite-tests"
            count=$((count + 1))
          fi
        done
        test "$count" -eq 1
      done
      runHook postInstall
    '';
    postInstall = "";
  });
  observerConfig = pkgs.writeText "prek-observer-fixture.toml" ''
    version = 1
    [policy]
    allowed_devices = ["fixture"]
    [devices.fixture]
    name = "Prek observer fixture"
  '';
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
    machine.succeed("PREK_ISOLATED_ROUTING=1 ${receiver}/bin/prek-recovery-tests", timeout=60)
    machine.succeed("test -z \"$(ls -A /run/prek/devices)\"")

    # Actual evdev idle polling, with no injected events and no production nodes.
    machine.succeed("systemd-run --unit prek-observer-fixture --property=Type=exec --property=RuntimeDirectory=prek-observer-fixture --property=RuntimeDirectoryMode=0700 --setenv=PREK_ISOLATED_ROUTING=1 --setenv=PREK_PROCESS_TEST=observer-fixture --setenv=PREK_PROCESS_WITNESS=/run/prek-observer-fixture/source ${receiver}/bin/prek-supervisor-tests")
    machine.wait_until_succeeds("test -s /run/prek-observer-fixture/source", timeout=5)
    source = machine.succeed("cat /run/prek-observer-fixture/source").strip()
    machine.succeed("udevadm settle")
    machine.succeed("${package}/bin/prek run --config ${observerConfig} --duration 3", timeout=10)

    def start_observer(unit):
        machine.succeed(f"systemd-run --unit {unit} --property=Type=exec --property=RemainAfterExit=yes ${package}/bin/prek run --config ${observerConfig}")
        machine.wait_until_succeeds(f"journalctl -u {unit} --no-pager | grep 'Observing /dev/input/'", timeout=5)
        parent = int(machine.succeed(f"systemctl show -p MainPID --value {unit}").strip())
        worker = int(machine.succeed(f"cat /proc/{parent}/task/{parent}/children").strip())
        machine.succeed(f"! readlink /proc/{parent}/fd/* | grep -E '/dev/input|/dev/uinput'")
        machine.succeed(f"readlink /proc/{worker}/fd/* | grep -Fx {source}")
        return worker

    worker = start_observer("prek-observer-stop")
    machine.succeed("systemctl kill --kill-whom=main --signal=SIGTERM prek-observer-stop")
    machine.wait_until_succeeds(f"test ! -e /proc/{worker}", timeout=3)
    machine.wait_until_succeeds("test \"$(systemctl show -p Result --value prek-observer-stop)\" = success", timeout=3)
    machine.succeed("systemctl stop prek-observer-stop")

    worker = start_observer("prek-observer-stall")
    machine.succeed(f"timeout 2 sh -c 'kill -STOP {worker}; while test -e /proc/{worker}; do sleep 0.02; done'")
    machine.wait_until_succeeds("journalctl -u prek-observer-stall --no-pager | grep 'observation worker Stalled'", timeout=3)
    machine.succeed("systemctl stop prek-observer-fixture")
    machine.wait_until_succeeds(f"test ! -e {source}")
  '';
}
