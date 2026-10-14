# Real stock compositor in a disposable VM, with no host device passthrough.
{ pkgs, package, morf ? null, lua ? false }:
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
    exec-once = env PREK_BROKER_CLI_FIXTURE=1 PREK_BROKER_APP=${package}/bin/prek ${pkgs.lib.optionalString lua "PREK_LUA_EVENTS=1"} PREK_LOCK_PYTHON=${pkgs.python3}/bin/python3 PREK_LOCK_SCRIPT=${lockScript} ${pkgs.lib.optionalString (morf != null) "PREK_LOCK_MORF=${morf}/bin/morf PREK_LOCK_MORF_CONFIG=${morfLock}"} ${tests}/bin/prek-compositor-tests --ignored --exact broker_cli_session_fixture --nocapture
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
  lockScript = pkgs.writeText "prek-session-lock-fixture.py" (builtins.readFile ./support/session_lock.py);
  morfLock = pkgs.writeText "prek-morf-lock-fixture.lua" ''
    local ui = require("morf.ui")
    morf.surface.session_lock = true
    morf.ipc.fixture_unlock = function()
      morf.surface.session_lock = false
    end
    ui.Rect { color = "#101010", width = 64, height = 64 }
  '';
  brokerConfig = pkgs.writeText "prek-broker-fixture" (if lua then ''
    local prek = require("prek")
    local callbacks = require("callbacks")
    local marker = prek.actions.define("snapshot-only", {requires={"observations"}}, function()
      error("unused named handler ran")
    end)
    prek.events.on("ready", function(event)
      callbacks.ready(event, marker.handler)
    end)
    prek.events.on("matched", function(event)
      if event.fence.configuration == 2 then while true do end end
      callbacks.matched(event)
    end)
    prek.scheduler.after("10ms", function(event)
      prek.log.info("lua-timer", {generation=event.context.fence.configuration})
    end)
    local cancelled = prek.scheduler.defer(function()error("cancelled timer ran")end)
    cancelled:cancel()
    return prek.config.new {
      policy = { allowed_devices = { "fixture" } },
      devices = { fixture = { name = "Prek broker fixture" } },
      rules = { prek.rules.define {
        id = "broker-a", contexts = { "desktop" },
        trigger = { kind = "key", key = "KEY_A" },
        action = { kind = "event", name = "fixture_press" },
      } },
    }
  '' else ''
    version = 1
    [policy]
    allowed_devices = ["fixture"]
    [devices.fixture]
    name = "Prek broker fixture"
    [[rules]]
    id = "broker-a"
    contexts = ["desktop"]
    trigger = { kind = "key", key = "KEY_A" }
    action = { kind = "event", name = "fixture_press" }
  '');
  brokerCallbacks = pkgs.writeText "prek-approved-callbacks.lua" ''
    local prek = require("prek")
    return {
      ready = function(event, handler)
        prek.log.info("lua-ready", {generation=event.fence.configuration, handler=handler, module="captured"})
      end,
      matched = function(event)
        prek.log.info("lua-match", {rule=event.event.rule, generation=event.fence.configuration, module="captured"})
      end,
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
    systemd.tmpfiles.rules = [
      "d /run/prek/devices 0700 root root -"
      "d /run/prek-session 0755 root root -"
      "d /run/prek-control 0755 root root -"
    ];
    environment.systemPackages = [ package tests ] ++ pkgs.lib.optional (morf != null) morf;
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
  testScript = let script = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    ${pkgs.lib.optionalString lua ''
    machine.succeed("${package}/bin/prek check --config ${package}/share/prek/examples/lua/modular/config.lua --json")
    ''}
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
    machine.succeed("printf press-initial > /run/prek-control/emit")
    machine.wait_until_succeeds("test \"$(cat /run/prek-control/emitted)\" = press-initial", timeout=3)
    machine.succeed("cp ${brokerConfig} /run/prek-control/config.toml && chmod 0600 /run/prek-control/config.toml")
    ${pkgs.lib.optionalString lua ''
    machine.succeed("install -d -m 0700 /run/prek-control/modules && install -m 0600 ${brokerCallbacks} /run/prek-control/modules/callbacks.lua")
    machine.fail("runuser -u alice -- test -r /run/prek-control/modules/callbacks.lua")
    ''}
    broker = "${package}/bin/prek broker --config /run/prek-control/config.toml --runtime-dir /run/prek-control --compositor-executable ${pkgs.hyprland}/bin/.Hyprland-wrapped --logind-library ${pkgs.systemd}/lib/libsystemd.so.0 ${pkgs.lib.optionalString (morf != null) "--morf-executable ${morf}/bin/.morf-wrapped"}"
    machine.succeed(f"systemd-run --unit prek-broker --property=Type=exec {broker}")
    try:
        machine.wait_until_succeeds("test -s /run/user/1000/prek-broker-ready", timeout=10)
    except Exception:
        machine.log(machine.succeed("cat /run/user/1000/prek-broker-agent.jsonl /run/user/1000/prek-broker-agent.stderr 2>/dev/null || true"))
        machine.log(machine.succeed("journalctl -u prek-broker --no-pager -n 30"))
        for query in ["monitors", "clients", "layers", "locked"]:
            machine.log(machine.succeed(f"XDG_RUNTIME_DIR=/run/user/1000 HYPRLAND_INSTANCE_SIGNATURE={instance} ${pkgs.hyprland}/bin/hyprctl -j {query}"))
        raise
    # The worker acknowledges neutral recognition entry; an initially held key
    # cannot become a new press when its session starts receiving observations.
    machine.succeed("! grep -q '\"outcome\":\"matched\"' /run/user/1000/prek-broker-agent.jsonl")
    ${pkgs.lib.optionalString lua ''
    # A changed module cannot alter the approved runtime; its failed reload
    # leaves generation one ready to recognize the next real fixture press.
    machine.succeed("printf \"error('changed module must not be reread')\" > /run/prek-control/modules/callbacks.lua")
    machine.fail("${package}/bin/prek reload --socket /run/prek-control/control.sock --json")
    ''}
    machine.succeed("printf release-initial > /run/prek-control/emit")
    machine.wait_until_succeeds("test \"$(cat /run/prek-control/emitted)\" = release-initial", timeout=3)
    machine.succeed("printf press-fresh > /run/prek-control/emit")
    machine.wait_until_succeeds("test \"$(cat /run/prek-control/emitted)\" = press-fresh", timeout=3)
    machine.wait_until_succeeds("grep -q '\"outcome\":\"matched\"' /run/user/1000/prek-broker-agent.jsonl", timeout=3)
    observations = [__import__("json").loads(line) for line in machine.succeed("cat /run/user/1000/prek-broker-agent.jsonl").splitlines()]
    matched = [row for row in observations if row.get("kind") == "observation" and row["event"]["outcome"] == "matched"]
    assert len(matched) == 1, matched
    assert matched[0]["event"]["rule"] == "broker-a", matched
    assert matched[0]["event"]["source"]["seat"] == "seat0", matched
    assert matched[0]["event"]["session_generation"] == matched[0]["fence"]["session"], matched
    assert matched[0]["event"]["config_generation"] == matched[0]["fence"]["configuration"], matched
    assert "frame" not in matched[0] and "events" not in matched[0]["event"], matched
    def inject(command):
        machine.succeed(f"printf {command} > /run/prek-control/emit")
        machine.wait_until_succeeds(f"test \"$(cat /run/prek-control/emitted)\" = {command}", timeout=3)
    def rule_matches():
        return [__import__("json").loads(line)["event"] for line in machine.succeed("cat /run/user/1000/prek-broker-agent.jsonl").splitlines() if '"outcome":"matched"' in line]
    ${pkgs.lib.optionalString lua ''
    machine.wait_until_succeeds("grep -q lua-match /run/user/1000/prek-broker-agent.jsonl", timeout=3)
    lua_logs = [__import__("json").loads(line) for line in machine.succeed("cat /run/user/1000/prek-broker-agent.jsonl").splitlines() if '"kind":"lua_log"' in line]
    ready = [row for row in lua_logs if row["record"]["message"] == "lua-ready"]
    assert ready and all(row["record"]["fields"]["handler"] == "snapshot-only" and row["record"]["fields"]["module"] == "captured" for row in ready), lua_logs
    machine.wait_until_succeeds("grep -q lua-timer /run/user/1000/prek-broker-agent.jsonl", timeout=3)
    timers = [__import__("json").loads(line) for line in machine.succeed("cat /run/user/1000/prek-broker-agent.jsonl").splitlines() if '"message":"lua-timer"' in line]
    assert timers and all(row["record"]["fields"]["generation"] == row["fence"]["configuration"] == 1 for row in timers), timers
    assert len({row["fence"]["session"] for row in timers}) == len(timers), timers
    matches = [row for row in lua_logs if row["record"]["message"] == "lua-match"]
    assert len(matches) == 1 and matches[0]["record"]["fields"]["rule"] == "broker-a", matches
    assert matches[0]["record"]["fields"]["generation"] == matches[0]["fence"]["configuration"] == 1, matches
    assert matches[0]["record"]["fields"]["module"] == "captured", matches
    machine.succeed("install -m 0600 ${brokerCallbacks} /run/prek-control/modules/callbacks.lua")
    ''}
    # Change the complete file while the old key is held. Successful reload
    # preserves the process and requires a real release before the new hold.
    revised = machine.succeed("cat /run/prek-control/config.toml").replace('kind = "key", key = "KEY_A"', 'kind = "button_hold", key = "KEY_A", after = "600ms"').replace('fixture_press', 'reloaded_hold')
    machine.succeed("cat > /run/prek-control/config.toml <<'CONFIG'\n" + revised + "\nCONFIG")
    reloaded = __import__("json").loads(machine.succeed("${package}/bin/prek reload --socket /run/prek-control/control.sock --json"))
    assert reloaded["configuration"] == 2 and reloaded["error"] is None, reloaded
    machine.wait_until_succeeds("grep -Eq '\"subscribed\":true.*\"configuration\":2|\"configuration\":2.*\"subscribed\":true' /run/user/1000/prek-broker-agent.jsonl", timeout=3)
    machine.sleep(1)
    assert len(rule_matches()) == 1, rule_matches()
    inject("release-old")
    inject("press-reloaded")
    machine.succeed("printf 'version = 999' > /run/prek-control/config.toml")
    machine.fail("${package}/bin/prek reload --socket /run/prek-control/control.sock --json")
    machine.wait_until_succeeds("grep -q reloaded_hold /run/user/1000/prek-broker-agent.jsonl", timeout=3)
    fresh = rule_matches()[-1]
    assert fresh["config_generation"] == 2 and fresh["action"]["name"] == "reloaded_hold", fresh
    ${pkgs.lib.optionalString lua ''
    machine.wait_until_succeeds("grep -q 'Lua runtime unavailable' /run/user/1000/prek-broker-agent.stderr", timeout=3)
    machine.succeed("systemctl is-active prek-broker")
    ''}
    ${pkgs.lib.optionalString lua ''
    # Shared Lua tables expand into independent Rust strings. Failure stays
    # inside the disposable compiler and leaves the held-key rule active.
    oversized = "\n".join([
        "local p = require('prek')",
        "local config = p.config.preset('minimal')",
        "local argv = {'fixture'}",
        "local argument = string.rep('x', 4096)",
        "for i = 2, 256 do argv[i] = argument end",
        "config.rules = {}",
        "for i = 1, 1024 do",
        "  config.rules[i] = {id = 'expanded-' .. i,",
        "    trigger = p.triggers.key {key = 'KEY_A'},",
        "    action = p.actions.command {argv = argv}}",
        "end", "return config",
    ])
    before = len(rule_matches())
    inject("release-before-oversized")
    inject("press-during-oversized")
    machine.succeed("cat > /run/prek-control/config.toml <<'CONFIG'\n" + oversized + "\nCONFIG")
    machine.fail("${package}/bin/prek reload --socket /run/prek-control/control.sock --json", timeout=5)
    machine.wait_until_succeeds(f"test $(grep -c '\"outcome\":\"matched\"' /run/user/1000/prek-broker-agent.jsonl) -gt {before}", timeout=3)
    fresh = rule_matches()[-1]
    assert fresh["config_generation"] == 2 and fresh["action"]["name"] == "reloaded_hold", fresh
    machine.succeed("systemctl is-active prek-broker")
    ''}
    machine.succeed("printf failed-reload > /run/user/1000/prek-broker-inspect")
    machine.wait_until_succeeds("test -s /run/user/1000/prek-broker-status.json", timeout=3)
    status = __import__("json").loads(machine.succeed("cat /run/user/1000/prek-broker-status.json"))
    assert status["fence"]["configuration"] == 2 and status["last_reload_error"], status
    machine.succeed("cat > /run/prek-control/config.toml <<'CONFIG'\n" + revised + "\nCONFIG")
    # Drop every source, then recreate the same identity with its key held.
    # The worker stays live, gives it a fresh generation and suppresses a hold.
    before = len(rule_matches())
    inject("disconnect")
    machine.wait_until_succeeds(f"test ! -e {source}", timeout=3)
    inject("recreate-held")
    source = machine.succeed("cat /run/prek-control/source").strip()
    machine.sleep(1)
    assert len(rule_matches()) == before, rule_matches()
    inject("release-reopened")
    inject("press-reopened")
    machine.wait_until_succeeds(f"test $(grep -c '\"outcome\":\"matched\"' /run/user/1000/prek-broker-agent.jsonl) -gt {before}", timeout=3)
    reopened = rule_matches()[-1]
    assert reopened["source"]["device"] == fresh["source"]["device"] and reopened["source"]["generation"] > fresh["source"]["generation"], reopened
    assert reopened["config_generation"] == 2, reopened
    inject("release-fresh")
    # Whole-file selector changes close an old reader and reopen it with
    # reconciled state, while the same physical test source remains present.
    before = len(rule_matches())
    inject("press-policy-held")
    excluded = revised.replace('name = "Prek broker fixture"', 'name = "Absent broker fixture"')
    machine.succeed("cat > /run/prek-control/config.toml <<'CONFIG'\n" + excluded + "\nCONFIG")
    result = __import__("json").loads(machine.succeed("${package}/bin/prek reload --socket /run/prek-control/control.sock --json"))
    assert result["configuration"] == 3 and result["error"] is None, result
    worker_now = int(machine.succeed("cat /proc/$(systemctl show -p MainPID --value prek-broker)/task/$(systemctl show -p MainPID --value prek-broker)/children").strip())
    machine.wait_until_succeeds(f"! readlink /proc/{worker_now}/fd/* | grep -Fx {source}", timeout=3)
    machine.succeed(f"test -e {source}")
    # A valid hold can finish before exclusion activates when command transport
    # is slow. Measure held-input suppression from the acknowledged exclusion.
    before = len(rule_matches())
    machine.succeed("cat > /run/prek-control/config.toml <<'CONFIG'\n" + revised + "\nCONFIG")
    result = __import__("json").loads(machine.succeed("${package}/bin/prek reload --socket /run/prek-control/control.sock --json"))
    assert result["configuration"] == 4 and result["error"] is None, result
    machine.wait_until_succeeds(f"readlink /proc/{worker_now}/fd/* | grep -Fx {source}", timeout=3)
    machine.sleep(1)
    assert len(rule_matches()) == before, rule_matches()
    inject("release-policy-held")
    inject("press-policy-fresh")
    machine.wait_until_succeeds(f"test $(grep -c '\"outcome\":\"matched\"' /run/user/1000/prek-broker-agent.jsonl) -gt {before}", timeout=3)
    assert rule_matches()[-1]["config_generation"] == 4, rule_matches()
    inject("release-policy-fresh")
    # A writable system file is refused as policy, preserving generation four.
    machine.succeed("chmod 0666 /run/prek-control/config.toml")
    machine.fail("${package}/bin/prek reload --socket /run/prek-control/control.sock --json")
    machine.succeed("chmod 0600 /run/prek-control/config.toml")
    parent = int(machine.succeed("systemctl show -p MainPID --value prek-broker").strip())
    worker = int(machine.succeed(f"cat /proc/{parent}/task/{parent}/children").strip())
    machine.succeed(f"! readlink /proc/{parent}/fd/* | grep -E '/dev/input|/dev/uinput'")
    machine.succeed(f"readlink /proc/{worker}/fd/* | grep -Fx {source}")
    machine.fail(broker)
    # A shared UID outside this compositor's PAM session is still unauthorized.
    machine.fail(f"${pkgs.util-linux}/bin/runuser -u alice -- ${package}/bin/prek status --socket /run/prek-control/control.sock --instance {instance} --json")
    machine.fail("${pkgs.util-linux}/bin/runuser -u alice -- ${package}/bin/prek reload --socket /run/prek-control/control.sock --json")
    # Let actual heartbeat/lease renewal run beyond its one-second expiry.
    machine.sleep(2)
    machine.succeed("systemctl is-active prek-broker")
    # Exercise an actual compositor lock, acknowledged through Wayland. No
    # model snapshot or client-supplied lock flag establishes this transition.
    before = len(rule_matches())
    machine.succeed(f"XDG_RUNTIME_DIR=/run/user/1000 HYPRLAND_INSTANCE_SIGNATURE={instance} ${pkgs.hyprland}/bin/hyprctl dispatch dpms off")
    machine.succeed("touch /run/user/1000/prek-lock-start")
    machine.wait_until_succeeds("test -s /run/user/1000/prek-lock-acknowledged", timeout=10)
    locked_state = "${if morf != null then "locked" else "locking"}"
    try:
        machine.wait_until_succeeds(f"grep -q '\"state\":\"{locked_state}\"' /run/user/1000/prek-broker-agent.jsonl", timeout=15 if locked_state == "locked" else 3)
    except Exception:
        machine.log(machine.succeed("cat /run/user/1000/prek-broker-agent.jsonl /run/user/1000/prek-broker-agent.stderr"))
        machine.log(machine.succeed("cat /run/user/1000/prek-lock-native.log 2>/dev/null || true"))
        machine.log(machine.succeed("journalctl -u prek-broker --no-pager -n 30"))
        machine.log(machine.succeed("ls -la /run/user/1000/morf 2>/dev/null || true"))
        raise
    inject("press-authentication")
    inject("release-authentication")
    machine.sleep(1)
    assert len(rule_matches()) == before, rule_matches()
    machine.succeed("rm -f /run/user/1000/prek-broker-status.json && printf locked > /run/user/1000/prek-broker-inspect")
    machine.wait_until_succeeds("test -s /run/user/1000/prek-broker-status.json", timeout=3)
    status = __import__("json").loads(machine.succeed("cat /run/user/1000/prek-broker-status.json"))
    assert status["state"] == locked_state and status["context_error"], status
    if locked_state == "locked":
        inject("press-native-handoff-held")
    # In the native variant, Morf performs its real unlock and waits for the
    # verified broker acknowledgement; the stock variant uses a bare unlock.
    machine.succeed("touch /run/user/1000/prek-lock-unlock")
    try:
        machine.wait_until_succeeds("test -s /run/user/1000/prek-lock-completed", timeout=10 if locked_state == "locked" else 3)
    except Exception:
        machine.log(machine.succeed("cat /run/user/1000/prek-broker-agent.jsonl /run/user/1000/prek-broker-agent.stderr /run/user/1000/prek-lock-native.log /run/user/1000/prek-lock-unlock-response.log 2>/dev/null || true"))
        machine.log(machine.succeed("journalctl -u prek-broker --no-pager -n 30"))
        raise
    machine.succeed(f"XDG_RUNTIME_DIR=/run/user/1000 HYPRLAND_INSTANCE_SIGNATURE={instance} ${pkgs.hyprland}/bin/hyprctl dispatch dpms on")
    if locked_state == "locked":
        machine.wait_until_succeeds("test $(grep -c '\"session_changed\":true.*\"state\":\"desktop\"' /run/user/1000/prek-broker-agent.jsonl) -gt 1", timeout=5)
        machine.wait_until_succeeds("test $(grep -c '\"subscribed\":true' /run/user/1000/prek-broker-agent.jsonl) -gt 4", timeout=5)
        machine.sleep(1)
        assert len(rule_matches()) == before, rule_matches()
        inject("release-native-handoff-held")
        inject("press-native-unlock-fresh")
        machine.wait_until_succeeds(f"test $(grep -c '\"outcome\":\"matched\"' /run/user/1000/prek-broker-agent.jsonl) -gt {before}", timeout=3)
        before = len(rule_matches())
        inject("release-native-unlock-fresh")
    inject("press-after-unverified-unlock")
    inject("release-after-unverified-unlock")
    machine.sleep(1)
    if locked_state == "locking":
        assert len(rule_matches()) == before, rule_matches()
    # Restart with context-free observation only. The compositor lifetime's
    # lock latch must survive and keep authentication observations disabled.
    if locked_state == "locked":
        # Resume the native client once without retaining stale provider state.
        machine.succeed("rm -f /run/user/1000/prek-broker-status.json && printf native-unlocked > /run/user/1000/prek-broker-inspect")
        machine.wait_until_succeeds("test -s /run/user/1000/prek-broker-status.json", timeout=3)
        status = __import__("json").loads(machine.succeed("cat /run/user/1000/prek-broker-status.json"))
        assert status["state"] == "desktop" and status["context_error"] is None, status
    machine.succeed("touch /run/user/1000/prek-broker-restart")
    machine.wait_until_succeeds("test -s /run/user/1000/prek-broker-restarted", timeout=3)
    machine.wait_until_succeeds("test $(grep -c '\"registered\":true' /run/user/1000/prek-broker-agent.jsonl) -eq 2", timeout=3)
    machine.succeed("rm -f /run/user/1000/prek-broker-status.json && printf reconnected > /run/user/1000/prek-broker-inspect")
    machine.wait_until_succeeds("test -s /run/user/1000/prek-broker-status.json", timeout=3)
    status = __import__("json").loads(machine.succeed("cat /run/user/1000/prek-broker-status.json"))
    assert status["state"] == ("locking" if locked_state == "locking" else "unknown"), status
    inject("press-after-agent-reconnect")
    inject("release-after-agent-reconnect")
    machine.sleep(1)
    assert len(rule_matches()) == before, rule_matches()
    machine.succeed("systemctl kill --kill-whom=main --signal=SIGTERM prek-broker")
    machine.wait_until_succeeds(f"test ! -e /proc/{worker}", timeout=3)
    machine.wait_until_succeeds("test -s /run/user/1000/prek-broker-agent-stopped", timeout=3)
    machine.wait_until_succeeds("test ! -e /run/prek-control/control.sock", timeout=3)
    machine.succeed("test \"$(systemctl show -p Result --value prek-broker)\" = success")
    # A normal duration expiry also waits for the worker's successful reap,
    # rather than reporting a data-socket EOF as a service failure.
    machine.succeed(f"{broker} --duration 1", timeout=5)
    machine.succeed("systemctl stop prek-broker-source")
    machine.wait_until_succeeds(f"test ! -e {source}")
  ''; in if lua then builtins.replaceStrings [ "config.toml" ] [ "config.lua" ] script else script;
}
