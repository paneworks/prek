"""Private local Cage/nested Hyprland keyboard check, following mold's sandbox."""
import argparse
import json
import os
from pathlib import Path
import signal
import select
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cage", required=True)
    parser.add_argument("--receiver", required=True)
    parser.add_argument("--keyboard", required=True)
    parser.add_argument("--hyprland", required=True)
    parser.add_argument("--inner", required=True)
    parser.add_argument("--proxy", required=True)
    parser.add_argument("--prek-fixture")
    parser.add_argument("--prek-app")
    parser.add_argument("--layers", action="store_true",
                        help="verify a Lua layer deactivation while its rule stays enabled")
    parser.add_argument("--visible", action="store_true",
                        default=os.environ.get("VISIBLE") == "1")
    parser.add_argument("--output", default="target/cage-host")
    args = parser.parse_args()
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix="run-", dir=output))
    scratch = tempfile.TemporaryDirectory(prefix="pc-")
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
               WLR_BACKENDS="wayland" if args.visible else "headless", WLR_RENDERER="gles2",
               WLR_RENDER_DRM_DEVICE=os.environ.get("RENDER_NODE", "/dev/dri/renderD128"),
               WLR_LIBINPUT_NO_DEVICES="1", LIBGL_ALWAYS_SOFTWARE="1", PREK_CAGE_HOST="1")
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
                   "--receiver", args.receiver, "--proxy", args.proxy, "--output", str(output)]
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
            socket = Path(json.loads((output / "nested.json").read_text())["socket"])
            if socket.parent != runtime or not socket.is_socket():
                raise RuntimeError("expected an owned nested compositor socket")
            keyboard = subprocess.Popen([args.keyboard, str(socket)], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=log, text=True)
            if not select.select([keyboard.stdout], [], [], 5)[0] or keyboard.stdout.readline() != "ready\n":
                raise RuntimeError("keyboard creation handshake failed")
            # A visible nested session can lose its host keyboard enter while
            # still running. Focus only its owned receiver via private IPC.
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
            subprocess.run([hyprctl,"dispatch","focuswindow","class:^(prek-cage-receiver)$"],
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
            if args.prek_fixture:
                if not args.prek_app:
                    parser.error("--prek-fixture requires --prek-app")
                fixture_env = dict(env, PREK_LOCAL_CAGE="1", PREK_CAGE_OUTPUT=str(output),
                                   PREK_LOCAL_CAGE_APP=args.prek_app,
                                   PREK_CAGE_LAYER_TEST="1" if args.layers else "0")
                instances = [path.parent.name for path in (runtime / "hypr").glob("*/.socket2.sock")]
                if len(instances) != 1:
                    raise RuntimeError("expected one owned compositor instance")
                diagnostic_env = dict(fixture_env, HYPRLAND_INSTANCE_SIGNATURE=instances[0])
                hyprctl = str(Path(args.hyprland).parent / "hyprctl")
                for query in ("monitors", "clients", "activewindow", "layers"):
                    with (output / f"hyprland-{query}.json").open("w") as data:
                        subprocess.run([hyprctl,"-j",query],env=diagnostic_env,
                                       stdout=data,stderr=log,check=True,timeout=3)
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
                        keyboard = subprocess.Popen([args.keyboard,str(socket),str(keys_file)],
                                                    stdin=subprocess.PIPE,stdout=subprocess.PIPE,
                                                    stderr=log,text=True)
                        if not select.select([keyboard.stdout], [], [], 5)[0] or keyboard.stdout.readline() != "ready\n":
                            raise RuntimeError("routed keyboard handshake failed")
                        keyboard.communicate("\n",timeout=5)
                        if keyboard.returncode != 0:
                            raise RuntimeError("routed keyboard failed")
                        deadline = time.monotonic() + 3
                        while len([row for row in rows()[start:] if row["event"] == "key"]) < len(expected):
                            if time.monotonic() >= deadline:
                                raise RuntimeError("routed application receipt timeout")
                            time.sleep(0.01)
                        received = rows()
                        events = [row for row in received[start:] if row["event"] == "key"]
                        assert [(row["code"],row["state"]) for row in events] == expected
                        assert received[-1]["text"] == text and not received[-1]["keys"]
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
                    assert len(contexts) == 9 and all(row["outputs"] > 0 and row["uid"] == os.geteuid() for row in contexts)
                    assert len({row["revision"] for row in contexts}) == 9
                    changes = [row["fields"] for row in logs if row["message"] == "local-rules-changed"]
                    assert changes == [{"accepted":True,"revision":"2","state":"accepted"}]
                    layers = [row["fields"] for row in logs if row["message"] == "local-layer-state"]
                    assert layers == [{"activation":"inactive" if args.layers else "active",
                                       "rule_enabled":args.layers}]
                    matches = [row["fields"] for row in logs if row["message"] == "local-matched"]
                    assert matches == [{"rule":"local-a","stage":"begin"},{"rule":"local-a","stage":"end"}]
                    mutation = "layer deactivation" if args.layers else "rule disable"
                    print(f"PASS: actual app received only B before the acknowledged Lua {mutation}, then A and B afterward; context and gesture generations stayed consistent.")
                finally:
                    if fixture.poll() is None:
                        fixture.kill()
                    fixture.wait(timeout=3)
            session_name = "visible nested Hyprland" if args.visible else "local Cage/nested Hyprland"
            print(f"PASS: {session_name} delivered B and Shift+A, six ordered key events, "
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
