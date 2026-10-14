# Disposable graphics/input VM, with no host event devices passed through.
{ pkgs, package }:
let
  receiver = import ./wayland-receiver.nix { inherit pkgs; };
  tests = package.overrideAttrs (_: {
    pname = "prek-cage-tests";
    cargoBuildFlags = [ "--test" "cage" ];
    doCheck = false;
    installPhase = ''
      mkdir -p "$out/bin"
      count=0
      for candidate in target/*/release/deps/cage-* target/release/deps/cage-*; do
        if [ -f "$candidate" ] && [ -x "$candidate" ]; then
          install -m 755 "$candidate" "$out/bin/prek-cage-tests"
          count=$((count + 1))
        fi
      done
      test "$count" -eq 1
    '';
    postInstall = "";
  });
  config = pkgs.writeText "prek-cage.lua" ''
    local prek = require("prek")
    return prek.config.new {
      mode = "route",
      policy = { allowed_devices = { "keyboard", "touch" } },
      devices = {
        keyboard = { name = "Prek test keyboard 1" },
        touch = { name = "Prek test touch 2" },
      },
      rules = {
        prek.rules.define {
          id = "cage-a", priority = 20, devices = { "keyboard" }, contexts = { "desktop" },
          delivery = "reserve", trigger = prek.triggers.key { key = "KEY_A" },
          action = prek.actions.event { name = "cage-a-claimed" },
        },
        prek.rules.define {
          id = "cage-down", priority = 10, devices = { "touch" }, contexts = { "desktop" },
          delivery = "buffer", trigger = prek.triggers.swipe {
            fingers = 2, direction = "down", travel = "40mm",
            fallback_travel = "0.2norm", continuous = false,
          },
          action = prek.actions.event { name = "cage-down-claimed" },
        },
      },
    }
  '';
in
pkgs.testers.runNixOSTest {
  name = "prek-cage-input";
  nodes.machine = {
    users.users.alice = {
      isNormalUser = true;
      uid = 1000;
      password = "test-fixture-only";
      extraGroups = [ "video" "input" "render" ];
    };
    services.getty.autologinUser = "alice";
    hardware.graphics.enable = true;
    boot.kernelModules = [ "uinput" ];
    services.udev.packages = [ package ];
    # Only this VM lets its deliberately marked test outputs reach libinput.
    # Packaged production/ordinary kernel-test exclusion remains unchanged.
    services.udev.extraRules = ''
      SUBSYSTEM=="input", KERNEL=="event*", ENV{PREK_TEST}=="1", ENV{PREK_VIRTUAL}=="1", ENV{LIBINPUT_IGNORE_DEVICE}="0"
    '';
    systemd.tmpfiles.rules = [
      "d /run/prek/devices 0700 root root -"
      "d /run/prek-cage 0755 root root -"
    ];
    fonts.packages = [ pkgs.dejavu_fonts ];
    environment.systemPackages = [ package tests receiver pkgs.cage pkgs.procps ];
    programs.bash.loginShellInit = ''
      if [ "$(tty)" = /dev/tty1 ]; then
        exec env PREK_CAGE_VM=1 LIBGL_ALWAYS_SOFTWARE=1 WLR_RENDERER=pixman \
          ${pkgs.cage}/bin/cage -s ${receiver}/bin/prek-wayland-receiver /run/user/1000/prek-cage.jsonl
      fi
    '';
    virtualisation.memorySize = 1536;
    virtualisation.cores = 2;
    virtualisation.qemu.options = [ "-vga none -device virtio-gpu-pci" ];
    system.stateVersion = "25.11";
  };
  testScript = ''
    import json
    import time
    import shlex

    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.wait_until_succeeds("grep -q '\"event\":\"configured\"' /run/user/1000/prek-cage.jsonl", timeout=60)
    machine.succeed("test -c /dev/uinput && udevadm settle")
    machine.log(machine.succeed("${pkgs.cage}/bin/cage -v"))
    machine.log("Pinned wlroots: ${pkgs.cage.version} Cage; ${pkgs.wlroots_0_19.version} wlroots")
    receipt_path = "/run/user/1000/prek-cage.jsonl"
    def receipts():
        return [json.loads(line) for line in machine.succeed(f"cat {receipt_path}").splitlines()]
    def diagnostics():
        machine.log(machine.succeed("journalctl -u prek-cage-fixture --no-pager -n 40"))
        machine.log(json.dumps(receipts()[-8:]))
        machine.log(machine.succeed("cat /run/prek-cage/dispatch.jsonl 2>/dev/null || true"))
        machine.screenshot("cage-failure")
    def wait_for(predicate, timeout=3):
        deadline = time.monotonic() + timeout
        while True:
            if predicate():
                return
            if time.monotonic() >= deadline:
                diagnostics()
                raise AssertionError("application receipt/fixture deadline expired")
            time.sleep(0.02)
    size = next(row for row in receipts() if row["event"] == "configured")
    machine.succeed(f"printf '%s' '{size['width']} {size['height']}' > /run/prek-cage/geometry")
    machine.succeed("pgrep -x .cage-wrapped > /run/prek-cage/cage-pid")
    machine.succeed("cp ${config} /run/prek-cage/config.lua && chmod 0600 /run/prek-cage/config.lua")
    machine.succeed("${package}/bin/prek check --config /run/prek-cage/config.lua")
    machine.succeed("systemd-run --unit prek-cage-fixture --property=Type=exec --property=RemainAfterExit=yes --property=RuntimeMaxSec=70 --setenv=PREK_CAGE_VM=1 --setenv=PREK_ISOLATED_ROUTING=1 --setenv=PREK_CAGE_EXE=${pkgs.cage}/bin/.cage-wrapped ${tests}/bin/prek-cage-tests --ignored --exact cage_controlled_input_fixture --nocapture")
    try:
        machine.wait_until_succeeds("test -s /run/prek-cage/ready", timeout=10)
    except Exception:
        diagnostics()
        raise
    wait_for(lambda: any(row["event"] == "keymap" for row in receipts()))
    counter = 0
    def command(name):
        global counter
        counter += 1
        message = f"{counter} {name}"
        machine.succeed(f"printf '%s' {shlex.quote(message)} > /run/prek-cage/emit.new && mv /run/prek-cage/emit.new /run/prek-cage/emit")
        wait_for(lambda: machine.succeed("cat /run/prek-cage/emitted 2>/dev/null || true") == message)
    def keys(code, start=0):
        return [row for row in receipts()[start:] if row["event"] == "key" and row["code"] == code]
    def touch_rows(start):
        return [row for row in receipts()[start:] if row["event"].startswith("touch_")]
    def neutral():
        row = receipts()[-1]
        return not row["keys"] and not row["contacts"] and row["modifiers"]["depressed"] == 0

    # First prove the real receiver, libinput classification and coordinates.
    start = len(receipts())
    command("a-down")
    wait_for(lambda: len(keys(30, start)) == 1)
    command("a-up")
    wait_for(lambda: len(keys(30, start)) == 2)
    assert [row["state"] for row in keys(30, start)] == [1, 0]
    assert receipts()[-1]["text"] == "a"
    command("meta-down")
    wait_for(lambda: receipts()[-1]["modifiers"]["depressed"] != 0)
    command("meta-up")
    wait_for(neutral)
    start = len(receipts())
    command("two-down")
    wait_for(lambda: len([row for row in touch_rows(start) if row["event"] == "touch_down"]) == 2)
    down = [row for row in touch_rows(start) if row["event"] == "touch_down"][-1]
    assert len(down["contacts"]) == 2
    for contact, x in zip(down["contacts"], [1000, 1500]):
        assert abs(contact["x"] - x / 4095 * size["width"]) < 2
        assert abs(contact["y"] - 1000 / 4095 * size["height"]) < 2
    command("two-move")
    wait_for(lambda: len([row for row in touch_rows(start) if row["event"] == "touch_motion"]) == 2)
    command("two-up")
    wait_for(lambda: len([row for row in touch_rows(start) if row["event"] == "touch_up"]) == 2)
    wait_for(neutral)

    # Lua-configured Routing + the real kernel transport. The context snapshot
    # is a model; production broker authentication/handoff are separate gates.
    command("route")
    wait_for(lambda: machine.succeed("test -s /run/prek-cage/touch-output && echo ready").strip() == "ready")
    start = len(receipts())
    command("a-down")
    command("a-up")
    def dispatches():
        return [json.loads(line) for line in machine.succeed("cat /run/prek-cage/dispatch.jsonl").splitlines()]
    wait_for(lambda: any(row.get("rule") == "cage-a" for row in dispatches()))
    command("b-down")
    wait_for(lambda: len(keys(48, start)) == 1)
    command("b-up")
    wait_for(lambda: len(keys(48, start)) == 2)
    assert keys(30, start) == []
    assert [row["state"] for row in keys(48, start)] == [1, 0]
    assert receipts()[-1]["text"] == "ab"

    # A nonmatching one-finger stroke passes in order; the claimed two-finger
    # downward swipe emits its semantic event and leaks no contacts or click.
    start = len(receipts())
    command("one-down")
    command("one-move")
    command("one-up")
    wait_for(lambda: len([row for row in touch_rows(start) if row["event"] == "touch_up"]) == 1)
    ordinary = touch_rows(start)
    assert [row["event"] for row in ordinary if row["event"] != "touch_frame"] == ["touch_down", "touch_motion", "touch_up"]
    assert len({row["id"] for row in ordinary if "id" in row}) == 1
    assert len([row for row in ordinary if row["event"] == "touch_frame"]) == 3
    down = next(row for row in ordinary if row["event"] == "touch_down")
    assert abs(down["contacts"][0]["x"] - 1000 / 4095 * size["width"]) < 2
    assert abs(down["contacts"][0]["y"] - 1000 / 4095 * size["height"]) < 2
    wait_for(neutral)
    start = len(receipts())
    command("two-down")
    time.sleep(0.14)
    command("two-far-move")
    command("two-up")
    wait_for(lambda: any(row.get("rule") == "cage-down" for row in dispatches()))
    time.sleep(0.35)
    assert touch_rows(start) == []
    assert len([row for row in dispatches() if row.get("rule") == "cage-down"]) == 1

    # Reuse the consumed gesture's unchanged slot-1 coordinates. evdev can
    # omit both the cursor selection and positions; virtual state must agree.
    start = len(receipts())
    command("tail-down")
    wait_for(lambda: any(row["event"] == "touch_down" for row in touch_rows(start)))
    down = next(row for row in touch_rows(start) if row["event"] == "touch_down")
    assert abs(down["contacts"][0]["x"] - 1500 / 4095 * size["width"]) < 2
    assert abs(down["contacts"][0]["y"] - 3000 / 4095 * size["height"]) < 2
    command("head-down")
    wait_for(lambda: len(receipts()[-1]["contacts"]) == 2)
    contacts = receipts()[-1]["contacts"]
    assert len({contact["id"] for contact in contacts}) == 2
    for actual, x in zip(sorted(contact["x"] for contact in contacts), [1000, 1500]):
        assert abs(actual - x / 4095 * size["width"]) < 2
    command("two-up")
    wait_for(neutral)

    # Recovery while a forwarded modifier/contact is held sends neutral input,
    # retires virtual output before releasing originals, then removes sources.
    command("shift-down")
    wait_for(lambda: receipts()[-1]["modifiers"]["depressed"] != 0)
    command("one-down")
    wait_for(lambda: bool(receipts()[-1]["contacts"]))
    machine.screenshot("cage-held-input")
    originals = [machine.succeed(f"cat /run/prek-cage/{name}").strip() for name in ["keyboard", "touch"]]
    outputs = [machine.succeed(f"cat /run/prek-cage/{name}-output").strip() for name in ["keyboard", "touch"]]
    command("stop")
    machine.wait_until_succeeds("test -s /run/prek-cage/finished", timeout=3)
    wait_for(neutral)
    machine.wait_until_succeeds("test \"$(systemctl show -p Result --value prek-cage-fixture)\" = success", timeout=3)
    for path in originals + outputs:
        machine.wait_until_succeeds(f"test ! -e {path}", timeout=3)
    machine.succeed("test -z \"$(ls -A /run/prek/devices)\"")
    machine.screenshot("cage-neutral-input")
    rows = receipts()
    assert [row["sequence"] for row in rows] == list(range(1, len(rows) + 1))
    assert all(a["at_ns"] <= b["at_ns"] for a, b in zip(rows, rows[1:]))
    machine.succeed("systemctl stop prek-cage-fixture")
    cage = int(machine.succeed("cat /run/prek-cage/cage-pid").strip())
    machine.succeed("systemctl stop getty@tty1.service")
    machine.succeed(f"if test -e /proc/{cage}; then kill -TERM {cage}; fi")
    machine.wait_until_succeeds(f"test ! -e /proc/{cage}", timeout=3)
    machine.wait_until_succeeds("! pgrep -f '^.*/bin/prek-(cage-tests|wayland-receiver)' ", timeout=3)
    machine.log("Cage receiver delivery, Lua-configured consumption/passthrough and held-state cleanup passed")
  '';
}
