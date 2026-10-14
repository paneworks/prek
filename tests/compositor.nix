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
    monitor = , preferred, auto, 1
    input {
      kb_layout = us
    }
    misc {
      disable_hyprland_logo = true
      disable_splash_rendering = true
    }
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
    systemd.tmpfiles.rules = [ "d /run/prek/devices 0700 root root -" ];
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
    # The endpoint appears before rendering/backend initialization completes.
    # Wait for a real request, rather than treating socket creation as readiness.
    machine.wait_until_succeeds(f"XDG_RUNTIME_DIR=/run/user/1000 ${pkgs.hyprland}/bin/hyprctl -i {instance} -j version", timeout=30)
    environment = f"PREK_COMPOSITOR_VM=1 PREK_ISOLATED_ROUTING=1 PREK_HYPR_RUNTIME=/run/user/1000 PREK_HYPR_INSTANCE={instance} PREK_HYPR_EXE=${pkgs.hyprland}/bin/.Hyprland-wrapped PREK_HYPR_UID=1000"
    machine.succeed(f"{environment} ${tests}/bin/prek-compositor-tests --ignored --test-threads=1 --nocapture", timeout=30)
    machine.succeed("test -z \"$(ls -A /run/prek/devices)\"")
  '';
}
