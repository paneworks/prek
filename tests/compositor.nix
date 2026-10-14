# Real stock compositor in a disposable VM, with no host device passthrough.
{ pkgs, package }:
let
  tests = package.overrideAttrs (_: {
    pname = "prek-compositor-tests";
    cargoBuildFlags = [ "--test" "hyprland" ];
    doCheck = false;
    installPhase = ''
      runHook preInstall
      mkdir -p "$out/bin"
      count=0
      for candidate in target/*/release/deps/hyprland-* target/release/deps/hyprland-*; do
        if [ -f "$candidate" ] && [ -x "$candidate" ]; then
          install -m 755 "$candidate" "$out/bin/prek-compositor-tests"
          count=$((count + 1))
        fi
      done
      test "$count" -eq 1
      runHook postInstall
    '';
    postInstall = "";
  });
  compositorConfig = pkgs.writeText "prek-hyprland-test.conf" ''
    exec-once = env PREK_BROKER_CLI_FIXTURE=1 PREK_BROKER_APP=${package}/bin/prek ${tests}/bin/prek-compositor-tests --ignored --exact broker_cli_session_fixture --nocapture
    exec-once = env PREK_SESSION_AGENT_FIXTURE=1 PREK_AGENT_SOCKET=/run/prek-session/agent.sock ${tests}/bin/prek-compositor-tests --ignored --exact logind_session_agent_fixture --nocapture
    exec-once = env PREK_SESSION_AGENT_FIXTURE=1 PREK_AGENT_SOCKET=/run/prek-session/seat.sock ${tests}/bin/prek-compositor-tests --ignored --exact logind_session_agent_fixture --nocapture
    monitor = , preferred, auto, 1
    input {
      kb_layout = us
    }
    misc {
      disable_hyprland_logo = true
      disable_splash_rendering = true
    }
  '';
  brokerConfig = pkgs.writeText "prek-broker-fixture.toml" ''
    version = 1
    [policy]
    allowed_devices = ["fixture"]
    [devices.fixture]
    name = "Prek broker fixture"
  '';
in
pkgs.testers.runNixOSTest {
  name = "prek-stock-compositor";
  nodes.machine = {
    users.users.alice = {
      isNormalUser = true;
      uid = 1000;
      password = "test-fixture-only";
      extraGroups = [ "video" "input" "render" ];
    };
    services.getty.autologinUser = "alice";
    programs.hyprland.enable = true;
    hardware.graphics.enable = true;
    boot.kernelModules = [ "uinput" ];
    services.udev.packages = [ package ];
    systemd.tmpfiles.rules = [
      "d /run/prek/devices 0700 root root -"
      "d /run/prek-session 0755 root root -"
      "d /run/prek-control 0755 root root -"
    ];
    environment.systemPackages = [ package tests ];
    programs.bash.loginShellInit = ''
      if [ "$(tty)" = /dev/tty1 ]; then
        exec env LIBGL_ALWAYS_SOFTWARE=1 ${pkgs.hyprland}/bin/Hyprland --config ${compositorConfig}
      fi
    '';
    virtualisation.memorySize = 2048;
    virtualisation.cores = 2;
    virtualisation.qemu.options = [ "-vga none -device virtio-gpu-pci" ];
    system.stateVersion = "25.11";
  };
  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds("test -n \"$(find /run/user/1000/hypr -name .socket.sock 2>/dev/null)\"", timeout=60)
    instance = machine.succeed("basename $(dirname $(find /run/user/1000/hypr -name .socket.sock))").strip()
    environment = f"PREK_COMPOSITOR_VM=1 PREK_ISOLATED_ROUTING=1 PREK_TEST_CHVT=${pkgs.kbd}/bin/chvt PREK_LOGIND_LIBRARY=${pkgs.systemd}/lib/libsystemd.so.0 PREK_HYPR_RUNTIME=/run/user/1000 PREK_HYPR_INSTANCE={instance} PREK_HYPR_EXE=${pkgs.hyprland}/bin/.Hyprland-wrapped PREK_HYPR_UID=1000"
    # Socket creation alone does not establish a responsive backend. Require
    # a successful query using the client's real 200 ms budget.
    machine.wait_until_succeeds(f"{environment} ${tests}/bin/prek-compositor-tests --ignored --exact stock_compositor_queries_use_the_verified_live_socket --nocapture", timeout=30)
    machine.succeed(f"{environment} ${tests}/bin/prek-compositor-tests --ignored --skip logind_session_agent_fixture --skip broker_observer_source_fixture --skip broker_cli_session_fixture --test-threads=1 --nocapture", timeout=30)
    machine.succeed("test -z \"$(ls -A /run/prek/devices)\"")
    machine.succeed("systemd-run --unit prek-broker-source --property=Type=exec --setenv=PREK_COMPOSITOR_VM=1 --setenv=PREK_ISOLATED_ROUTING=1 ${tests}/bin/prek-compositor-tests --ignored --exact broker_observer_source_fixture")
    machine.wait_until_succeeds("test -s /run/prek-control/source", timeout=5)
    source = machine.succeed("cat /run/prek-control/source").strip()
    broker = "${package}/bin/prek broker --config ${brokerConfig} --runtime-dir /run/prek-control --compositor-executable ${pkgs.hyprland}/bin/.Hyprland-wrapped --logind-library ${pkgs.systemd}/lib/libsystemd.so.0"
    machine.succeed(f"systemd-run --unit prek-broker --property=Type=exec {broker}")
    machine.wait_until_succeeds("test -s /run/user/1000/prek-broker-ready", timeout=10)
    parent = int(machine.succeed("systemctl show -p MainPID --value prek-broker").strip())
    worker = int(machine.succeed(f"cat /proc/{parent}/task/{parent}/children").strip())
    machine.succeed(f"! readlink /proc/{parent}/fd/* | grep -E '/dev/input|/dev/uinput'")
    machine.succeed(f"readlink /proc/{worker}/fd/* | grep -Fx {source}")
    machine.fail(broker)
    # A shared UID outside this compositor's PAM session is still unauthorized.
    machine.fail(f"${pkgs.util-linux}/bin/runuser -u alice -- ${package}/bin/prek status --socket /run/prek-control/control.sock --instance {instance} --json")
    # Let actual heartbeat/lease renewal run beyond its one-second expiry.
    machine.sleep(2)
    machine.succeed("systemctl is-active prek-broker")
    machine.succeed("systemctl kill --kill-whom=main --signal=SIGTERM prek-broker")
    machine.wait_until_succeeds(f"test ! -e /proc/{worker}", timeout=3)
    machine.wait_until_succeeds("test -s /run/user/1000/prek-broker-agent-stopped", timeout=3)
    machine.wait_until_succeeds("test ! -e /run/prek-control/control.sock", timeout=3)
    machine.succeed("test \"$(systemctl show -p Result --value prek-broker)\" = success")
    machine.succeed("systemctl stop prek-broker-source")
    machine.wait_until_succeeds(f"test ! -e {source}")
  '';
}
