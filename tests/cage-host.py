"""Private local Cage/nested Hyprland keyboard check, following mold's sandbox."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import select
import subprocess
import tempfile
import time


def rendered_through(receipts, sequence):
    return any(row["event"] == "rendered" and row.get("drawn_sequence", 0) >= sequence
               for row in receipts)


def focus_arguments(provider, address):
    if not re.fullmatch(r"0x[0-9a-fA-F]{1,16}", address) or int(address, 16) == 0:
        raise ValueError("invalid owned window address")
    if provider == "lua":
        return ["dispatch", f'hl.dsp.focus({{window="address:{address}"}})']
    if provider != "hyprlang":
        raise ValueError("unknown owned compositor config provider")
    return ["dispatch", "focuswindow", f"address:{address}"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cage", required=True)
    parser.add_argument("--receiver", required=True)
    parser.add_argument("--keyboard", required=True)
    parser.add_argument("--hyprland", required=True)
    parser.add_argument("--hyprland-config", choices=("hyprlang", "lua"), default="hyprlang")
    parser.add_argument("--inner", required=True)
    parser.add_argument("--proxy", required=True)
    parser.add_argument("--prek-fixture")
    parser.add_argument("--prek-app")
    parser.add_argument("--layers", action="store_true",
                        help="verify a Lua layer deactivation while its rule stays enabled")
    parser.add_argument("--modes", action="store_true",
                        help="verify a Lua mode switch while its rule and layer stay enabled")
    parser.add_argument("--input", action="store_true",
                        help="verify normalized source observations through worker IPC and Lua")
    parser.add_argument("--keyboard-quiesce", action="store_true",
                        help="characterize per-device disable without granting production handoff")
    parser.add_argument("--keyboard-quiesce-check", type=Path,
                        default=Path(__file__).parent / "support/keyboard_quiesce.py")
    parser.add_argument("--hardware-rendering", action="store_true",
                        help="use the selected GPU driver instead of forcing Mesa software rendering")
    parser.add_argument("--power", action="store_true",
                        help="verify Lua-planned DPMS blank/wake on the owned nested output")
    parser.add_argument("--windows", action="store_true",
                        help="verify Lua-planned close and adjacent workspace moves on owned test windows")
    parser.add_argument("--morf", type=Path, help="actual Morf ELF for the owned native lock-provider test")
    parser.add_argument("--morf-lock-check", type=Path,
                        default=Path(__file__).parent / "support/morf-lock.py")
    parser.add_argument("--morf-shell", type=Path,
                        help="Caelestia shell init.lua for ordinary typed Morf controller checks")
    parser.add_argument("--screencopy", type=Path,
                        help="grim executable for captures from the private owned output")
    parser.add_argument("--morf-control-check", type=Path,
                        default=Path(__file__).parent / "support/morf-controls.py")
    root_probe = parser.add_mutually_exclusive_group()
    root_probe.add_argument("--morf-root-probe", type=Path,
                            help="opt-in root native probe; requires a pre-authorized sudo ticket")
    root_probe.add_argument("--morf-root-probe-socket", type=Path,
                            help="root-owned coordinator for the explicit remote-session fixture")
    parser.add_argument("--visible", action="store_true",
                        default=os.environ.get("VISIBLE") == "1")
    parser.add_argument("--output", default="target/cage-host")
    args = parser.parse_args()
    if args.keyboard_quiesce and any((args.layers,args.modes,args.input,args.power,args.windows,args.morf)):
        parser.error("--keyboard-quiesce is a separate owned-session characterization")
    if args.morf_shell and not args.morf:
        parser.error("--morf-shell requires --morf with the actual ELF")
    if (args.morf_root_probe or args.morf_root_probe_socket) and not args.morf_shell:
        parser.error("--morf-root-probe requires the owned Morf shell")
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix="run-", dir=output))
    # Nix development shells can set a long TMPDIR; Hyprland's instance plus
    # socket names must still fit sockaddr_un's 107-byte path limit.
    scratch = tempfile.TemporaryDirectory(prefix="pc-", dir="/tmp")
    runtime = Path(scratch.name) / "r"
    runtime.mkdir(mode=0o700)
    home = Path(scratch.name) / "home"
    home.mkdir(mode=0o700)
    upstream = os.environ.get("WAYLAND_DISPLAY")
    if args.visible and not upstream:
        parser.error("run from a Wayland session")
    if args.visible and not os.path.isabs(upstream):
        upstream = str(Path(os.environ["XDG_RUNTIME_DIR"]) / upstream)
    env = {name:value for name,value in os.environ.items() if name not in (
        "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE",
        "NIRI_SOCKET", "SWAYSOCK", "I3SOCK", "XDG_SESSION_ID", "XDG_SEAT", "XDG_VTNR")}
    env.update(XDG_RUNTIME_DIR=str(runtime), HOME=str(home),
               XDG_CONFIG_HOME=str(home / ".config"), XDG_DATA_HOME=str(home / ".local/share"),
               XDG_STATE_HOME=str(home / ".local/state"), XDG_CACHE_HOME=str(home / ".cache"),
               DBUS_SYSTEM_BUS_ADDRESS=f"unix:path={runtime}/no-system-bus",
               DBUS_SESSION_BUS_ADDRESS=os.environ.get("PREK_TEST_SESSION_BUS", f"unix:path={runtime}/no-session-bus"),
               WLR_BACKENDS="wayland" if args.visible else "headless", WLR_HEADLESS_OUTPUTS="1", WLR_RENDERER="gles2",
               WLR_RENDER_DRM_DEVICE=os.environ.get("RENDER_NODE", "/dev/dri/renderD128"),
               WLR_LIBINPUT_NO_DEVICES="1", LIBGL_ALWAYS_SOFTWARE="0" if args.hardware_rendering else "1", PREK_CAGE_HOST="1")
    env["PREK_CAGE_VISIBLE"] = "1" if args.visible else "0"
    if args.visible:
        env["WAYLAND_DISPLAY"] = upstream
    receipts = output / "receipts.jsonl"

    def rows():
        if not receipts.exists():
            return []
        # Ignore only an unfinished final write; malformed complete JSON fails.
        return [json.loads(line) for line in receipts.read_text().splitlines(keepends=True)
                if line.endswith("\n")]

    with (output / "cage.log").open("w") as log:
        command = [os.sys.executable, args.inner, "--hyprland", args.hyprland,
                   "--hyprland-config", args.hyprland_config,
                   "--receiver", args.receiver, "--proxy", args.proxy, "--output", str(output)]
        if args.keyboard_quiesce:
            command.append("--receiver-sync-probe")
        # Mold's visible path connects nested Hyprland through the private proxy
        # directly to the host display; only the headless path needs outer Cage.
        if not args.visible:
            command = [args.cage, "--"] + command
        cage = subprocess.Popen(command,
                                env=env, stdout=log, stderr=log, start_new_session=True)
        keyboard = None
        try:
            deadline = time.monotonic() + 30
            while not any(row["event"] == "configured" for row in rows()):
                if cage.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(f"Cage startup failed; see {output / 'cage.log'}")
                time.sleep(0.02)
            while not any(row["event"] == "rendered" for row in rows()):
                if cage.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(f"owned receiver never rendered; see {output / 'cage.log'}")
                time.sleep(0.02)
            socket = Path(json.loads((output / "nested.json").read_text())["socket"])
            if socket.parent != runtime or not socket.is_socket():
                raise RuntimeError("expected an owned nested compositor socket")
            keyboard = subprocess.Popen([args.keyboard, str(socket)], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=log, text=True)
            if not select.select([keyboard.stdout], [], [], 5)[0] or keyboard.stdout.readline() != "ready\n":
                raise RuntimeError("keyboard creation handshake failed")
            # Parent seat input is masked by the private proxy. Focus only
            # the owned receiver for the prepared synthetic keyboard.
            instances = [path.parent.name for path in (runtime / "hypr").glob("*/.socket2.sock")]
            if len(instances) != 1:
                raise RuntimeError("expected one owned compositor instance")
            control_env = dict(env,HYPRLAND_INSTANCE_SIGNATURE=instances[0])
            hyprctl = str(Path(args.hyprland).parent / "hyprctl")
            deadline = time.monotonic() + 5
            while True:
                clients = subprocess.run([hyprctl,"-j","clients"],env=control_env,
                                         capture_output=True,text=True,check=True,timeout=3)
                (output / "focus-clients.json").write_text(clients.stdout)
                mapped = [client for client in json.loads(clients.stdout)
                          if client.get("class") == "prek-cage-receiver" and client.get("mapped")]
                if len(mapped) == 1:
                    break
                if cage.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("owned receiver did not map before keyboard focus")
                time.sleep(0.02)
            subprocess.run([hyprctl] + focus_arguments(args.hyprland_config, mapped[0]["address"]),
                           env=control_env,stdout=log,stderr=log,check=True,timeout=3)
            deadline = time.monotonic() + 5
            while not any(row["event"] == "keyboard_enter" for row in rows()):
                if keyboard.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("receiver did not get keyboard focus")
                time.sleep(0.02)
            keyboard.communicate("\n", timeout=5)
            if keyboard.returncode != 0:
                raise RuntimeError("keyboard injection failed")
            deadline = time.monotonic() + 3
            while True:
                received = rows()
                keys = [row for row in received if row["event"] == "key"]
                if len(keys) == 6 and received[-1]["modifiers"]["depressed"] == 0:
                    break
                if cage.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(f"keyboard receipt timeout; see {receipts}")
                time.sleep(0.02)
            assert [(row["code"], row["state"]) for row in keys] == [
                (48, 1), (48, 0), (42, 1), (30, 1), (30, 0), (42, 0)]
            assert received[-1]["text"] == "bA", received[-1]
            assert not received[-1]["keys"] and not received[-1]["contacts"]
            assert received[-1]["modifiers"] == {
                "depressed": 0, "latched": 0, "locked": 0, "group": 0}
            assert any(row["modifiers"]["depressed"] for row in received)
            def wait_rendered(sequence):
                deadline = time.monotonic() + 5
                while not rendered_through(rows(), sequence):
                    if cage.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError(f"receiver did not render through receipt {sequence}")
                    time.sleep(0.01)

            wait_rendered(max(row["sequence"] for row in received if row["event"] not in
                              ("sync", "rendered", "presented", "presentation_discarded")))
            def capture(name):
                if not args.screencopy:
                    return
                capture_env = dict(control_env, WAYLAND_DISPLAY=socket.name, WAYLAND_DEBUG="client")
                monitors = subprocess.run([hyprctl, "-j", "monitors"], env=control_env,
                                          check=True, capture_output=True, text=True, timeout=3)
                active = [item for item in json.loads(monitors.stdout) if not item.get("disabled", False)]
                assert len(active) == 1
                with (output / "receiver-screencopy.log").open("a") as capture_log:
                    subprocess.run([str(args.screencopy), "-o", active[0]["name"],
                                    str(output / f"receiver-{name}.png")],
                                   env=capture_env, stdout=capture_log, stderr=capture_log,
                                   check=True, timeout=5)

            capture("baseline")
            if args.keyboard_quiesce:
                spec = importlib.util.spec_from_file_location("keyboard_quiesce", args.keyboard_quiesce_check)
                quiesce = importlib.util.module_from_spec(spec)
                spec.loader.exec_module(quiesce)
                quiesce.validate_result(quiesce.run(args, control_env, output, socket, rows, wait_rendered, capture))
            if args.prek_fixture and not args.keyboard_quiesce:
                if not args.prek_app:
                    parser.error("--prek-fixture requires --prek-app")
                fixture_env = dict(env, PREK_LOCAL_CAGE="1", PREK_CAGE_OUTPUT=str(output),
                                   PREK_LOCAL_CAGE_APP=args.prek_app,
                                   PREK_CAGE_LAYER_TEST="1" if args.layers else "0",
                                   PREK_CAGE_MODE_TEST="1" if args.modes else "0",
                                   PREK_CAGE_INPUT_TEST="1" if args.input else "0")
                fixture_env["PREK_CAGE_POWER_TEST"] = "1" if args.power else "0"
                fixture_env["PREK_CAGE_WINDOW_TEST"] = "1" if args.windows else "0"
                fixture_env["PREK_CAGE_CONFIG_PROVIDER"] = args.hyprland_config
                fixture_env["PREK_CAGE_RECEIVER"] = args.receiver
                fixture_env["PREK_CAGE_HYPRCTL"] = str(Path(args.hyprland).parent / "hyprctl")
                if args.screencopy:
                    fixture_env["PREK_CAGE_SCREENCOPY"] = str(args.screencopy)
                instances = [path.parent.name for path in (runtime / "hypr").glob("*/.socket2.sock")]
                if len(instances) != 1:
                    raise RuntimeError("expected one owned compositor instance")
                diagnostic_env = dict(fixture_env, HYPRLAND_INSTANCE_SIGNATURE=instances[0])
                hyprctl = str(Path(args.hyprland).parent / "hyprctl")
                for query in ("monitors", "clients", "activewindow", "layers"):
                    with (output / f"hyprland-{query}.json").open("w") as data:
                        subprocess.run([hyprctl,"-j",query],env=diagnostic_env,
                                       stdout=data,stderr=log,check=True,timeout=3)
                # Prepare the keymap/hotplug round trip before the fixture
                # acquires its short-lived context. Keep this neutral protocol
                # keyboard alive across both gated batches, avoiding compositor
                # hotplug/teardown work during that lease.
                prepared_keys = output / "phase-keys.txt"
                prepared_keys.write_text("")
                keyboard = subprocess.Popen([args.keyboard,str(socket),str(prepared_keys),"--persistent"],
                                            stdin=subprocess.PIPE,stdout=subprocess.PIPE,
                                            stderr=log,text=True)
                if not select.select([keyboard.stdout], [], [], 5)[0] or keyboard.stdout.readline() != "ready\n":
                    raise RuntimeError("prepared routed keyboard handshake failed")
                fixture = subprocess.Popen([args.prek_fixture,"--ignored","--exact",
                                            "local_cage_lua_routing_and_context","--nocapture"],
                                           env=fixture_env,stdout=log,stderr=log)
                try:
                    deadline = time.monotonic() + 20
                    while not (output / "phase1-ready").exists():
                        if fixture.poll() is not None or time.monotonic() >= deadline:
                            raise RuntimeError("Prek fixture did not prepare phase one")
                        time.sleep(0.01)

                    def inject_phase(path, expected, text):
                        nonlocal keyboard
                        keys = json.loads(path.read_text())
                        assert [(event["code"], event["value"]) for event in keys] == expected
                        keys_file = output / "phase-keys.txt"
                        keys_file.write_text("".join(f"{event['code']} {event['value']}\n" for event in keys))
                        start = len(rows())
                        if keyboard.poll() is not None:
                            raise RuntimeError("prepared routed keyboard stopped")
                        keyboard.stdin.write("\n")
                        keyboard.stdin.flush()
                        if not select.select([keyboard.stdout], [], [], 5)[0] or keyboard.stdout.readline() != "done\n":
                            raise RuntimeError("routed keyboard batch handshake failed")
                        deadline = time.monotonic() + 3
                        while len([row for row in rows()[start:] if row["event"] == "key"]) < len(expected):
                            if time.monotonic() >= deadline:
                                raise RuntimeError("routed application receipt timeout")
                            time.sleep(0.01)
                        received = rows()
                        events = [row for row in received[start:] if row["event"] == "key"]
                        assert [(row["code"],row["state"]) for row in events] == expected
                        assert received[-1]["text"] == text and not received[-1]["keys"]
                        wait_rendered(events[-1]["sequence"])
                        capture(path.stem)
                        modifiers = [row for row in received[start:] if row["event"] == "modifiers"]
                        assert modifiers and all(row["modifiers"]["depressed"] == 0 and row["modifiers"]["latched"] == 0 and row["modifiers"]["locked"] == 0 for row in modifiers)

                    inject_phase(output / "routed-keys.json", [(48,1),(48,0)], "bAb")
                    (output / "phase1-delivered").write_text("application receipt verified\n")
                    deadline = time.monotonic() + 20
                    while not (output / "phase2-ready").exists():
                        if fixture.poll() is not None or time.monotonic() >= deadline:
                            raise RuntimeError("Prek rule-change fixture failed")
                        time.sleep(0.01)
                    receipt = json.loads((output / "rule-receipt.json").read_text())
                    assert receipt == {"id":1,"revision":2,"configuration":1,"session":1}
                    inject_phase(output / "changed-keys.json", [(30,1),(30,0),(48,1),(48,0)], "bAbab")
                    (output / "phase2-delivered").write_text("application receipt verified\n")
                    fixture.wait(timeout=20)
                    if fixture.returncode != 0:
                        raise RuntimeError("Prek fixture failed after phase two")
                    if args.power:
                        power = json.loads((output / "power-receipts.json").read_text())
                        assert [row["completion"] for row in power] == ["applied", "applied"]
                        assert [row["screen"] for row in power] == ["off", "on"]
                        assert power[0]["output"] == power[1]["output"]
                        print("PASS: granted Lua plans blanked and woke only the owned nested output; stock IPC confirmed DPMS off/on.")
                    if args.windows:
                        windows = json.loads((output / "hyprland-receipts.json").read_text())
                        assert [row["action"] for row in windows] == ["move_right_and_follow", "move_left_and_follow", "close"]
                        assert all(row["completion"] == "applied" and row["lua_capabilities_checked"] for row in windows)
                        assert len({row["window"] for row in windows}) == 1
                        assert windows[0]["workspace_after"] == windows[0]["workspace_before"] + 1
                        assert windows[1]["workspace_after"] == windows[1]["workspace_before"] - 1
                        assert windows[2]["target_closed"] and windows[2]["guard_alive"]
                        print("PASS: granted Lua plans moved the owned starting window right/left with workspace follow, then closed it while another window was focused; guard remained alive.")
                        if args.hyprland_config == "lua":
                            layouts = json.loads((output / "hyprland-layouts.json").read_text())
                            assert layouts["status"] == "passed" and len(layouts["snapshots"]) == 4
                            assert all(snapshot["generation"] == 1 for snapshot in layouts["snapshots"])
                            assert [snapshot["revision"] for snapshot in layouts["snapshots"]] == sorted({snapshot["revision"] for snapshot in layouts["snapshots"]})
                            copied = [row["fields"] for row in map(json.loads,(output / "bound-layouts.jsonl").read_text().splitlines())
                                      if row["message"] == "bound-native-layout"]
                            assert len(copied) >= 4 and all(row["windows"] >= 3 for row in copied)
                            assert any(row["layout"] == "scrolling" and row["columns"] >= 3 for row in copied)
                            print("PASS: fixed read-only native layout queries observed owned down/up scrolling columns and unchanged window lifetimes, then restored dwindle; not G11 gesture acceptance.")
                    dispatches = json.loads((output / "dispatches.json").read_text())
                    a = [event for event in dispatches if event["rule"] == "local-a"]
                    b = [event for event in dispatches if event["rule"] == "local-b"]
                    assert [event["stage"] for event in a] == ["begin","end"]
                    assert [event["ordinal"] for event in a] == [1,2] and a[0]["id"] == a[1]["id"]
                    assert [event["stage"] for event in b] == ["begin","end","begin","end"]
                    assert b[0]["id"] == b[1]["id"] and b[2]["id"] == b[3]["id"] > b[0]["id"]
                    assert [event["source"]["generation"] for event in b] == [1,1,2,2]
                    assert all(event["config_generation"] == 1 and event["session_generation"] == 1 for event in dispatches)
                    logs = [json.loads(line) for line in (output / "prek-runtime.jsonl").read_text().splitlines()]
                    contexts = [row["fields"] for row in logs if row["message"] == "local-context"]
                    assert len(contexts) >= 9 and all(row["outputs"] > 0 and row["uid"] == os.geteuid() for row in contexts)
                    assert len({row["revision"] for row in contexts}) == len(contexts)
                    native_layouts = [row["fields"] for row in logs if row["message"] == "local-native-layouts"]
                    if args.hyprland_config == "lua":
                        assert native_layouts and all(row["windows"] > 0 for row in native_layouts)
                        assert all(row["revision"] in {context["revision"] for context in contexts} for row in native_layouts)
                        print("PASS: matched native layouts crossed parent/helper context transfer into copy-only Lua queries without granting ownership.")
                    else:
                        assert not native_layouts
                    changes = [row["fields"] for row in logs if row["message"] == "local-rules-changed"]
                    assert changes == [{"accepted":True,"revision":"2","state":"accepted"}]
                    layers = [row["fields"] for row in logs if row["message"] == "local-layer-state"]
                    assert layers == [{"activation":"inactive" if args.layers and not args.modes else "active",
                                       "rule_enabled":args.layers or args.modes}]
                    modes = [row["fields"] for row in logs if row["message"] == "local-mode-state"]
                    assert modes == [{"selected":"ordinary" if args.modes else "capture",
                                      "revision":"2", "rule_enabled":args.layers or args.modes}]
                    matches = [row["fields"] for row in logs if row["message"] == "local-matched"]
                    assert matches == [{"rule":"local-a","stage":"begin"},{"rule":"local-a","stage":"end"}]
                    hooks = [row["fields"] for row in logs if row["message"] == "local-rule-hook"]
                    assert hooks == [{"rule":"local-b","stage":stage,"ordinal":ordinal}
                                     for stage,ordinal in [("begin","1"),("end","2"),("begin","1"),("end","2")]]
                    print("PASS: four ordered per-rule Lua phase callbacks retained exact ordinals across acknowledged mutation and source replacement.")
                    if args.input:
                        inputs = [row["fields"] for row in logs if row["message"] == "local-input"]
                        assert [(row["code"], row["state"]) for row in inputs] == [(30,"press"),(30,"release"),(48,"press"),(48,"release")] * 2
                        assert [row["sequence"] for row in inputs] == [str(index) for index in range(1,9)]
                        assert [row["generation"] for row in inputs] == ["1"] * 4 + ["2"] * 4
                        resets = [row["fields"] for row in logs if row["message"] == "local-input-reset"]
                        assert resets == [{"generation":"1","reason":"attached"},{"generation":"1","reason":"removed"},{"generation":"2","reason":"attached"}]
                        print("PASS: eight ordered normalized source frames and three lifetime resets crossed actual worker/helper IPC into Lua; app delivery remained correct.")
                    mutation = "mode switch" if args.modes else "layer deactivation" if args.layers else "rule disable"
                    print(f"PASS: actual app received only B before the acknowledged Lua {mutation}, then A and B afterward; context and gesture generations stayed consistent.")
                finally:
                    if fixture.poll() is None:
                        fixture.kill()
                    fixture.wait(timeout=3)
            if args.morf:
                report = json.loads((output / "nested.json").read_text())
                subprocess.run([os.sys.executable, str(args.morf_lock_check),
                    "--morf", str(args.morf.resolve()), "--socket", str(socket),
                    "--hyprctl", hyprctl, "--instance", instances[0],
                    "--pid", str(report["hyprland_pid"]), "--output", str(output)],
                    env=env, check=True, timeout=30)
                if args.morf_shell:
                    subprocess.run([os.sys.executable, str(args.morf_control_check),
                        "--morf", str(args.morf.resolve()), "--shell", str(args.morf_shell.resolve()),
                        "--pid", str(report["hyprland_pid"]),
                        "--socket", str(socket), "--hyprctl", hyprctl,
                        "--instance", instances[0], "--output", str(output)]
                        + (["--screencopy", str(args.screencopy)] if args.screencopy else [])
                        + (["--root-probe", str(args.morf_root_probe)] if args.morf_root_probe else [])
                        + (["--root-probe-socket", str(args.morf_root_probe_socket)] if args.morf_root_probe_socket else []),
                        env=env, check=True, timeout=90)
            session_name = "visible nested Hyprland" if args.visible else "local Cage/nested Hyprland"
            print(f"Baseline PASS: {session_name} delivered B and Shift+A, six ordered key events, "
                  f"text bA and neutral modifier cleanup. Receipts: {receipts}")
        finally:
            if keyboard is not None:
                if keyboard.poll() is None:
                    keyboard.kill()
                keyboard.wait(timeout=3)
            try:
                os.killpg(cage.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                cage.wait(timeout=8)
            except subprocess.TimeoutExpired:
                os.killpg(cage.pid, signal.SIGKILL)
                cage.wait(timeout=3)
            if cage.poll() is None:
                raise RuntimeError("owned local test session was not reaped")
            scratch.cleanup()
    print("Owned local compositor session and receiver stopped; no host evdev devices opened.")


if __name__ == "__main__":
    main()
