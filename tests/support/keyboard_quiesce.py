"""Negative keyboard-disable characterization in one explicitly owned compositor."""
import hashlib
import json
from pathlib import Path
import re
import select
import shutil
import subprocess
import time


def enabled_arguments(provider, name, enabled):
    if not isinstance(name, str) or not re.fullmatch(r"[a-z][a-z0-9_-]{0,127}", name):
        raise ValueError("invalid owned keyboard name")
    if type(enabled) is not bool:
        raise ValueError("enabled must be boolean")
    value = "true" if enabled else "false"
    if provider == "lua":
        return ["eval", f'hl.device({{name="{name}",enabled={value}}})']
    if provider == "hyprlang":
        return ["keyword", f"device[{name}]:enabled", value]
    raise ValueError("unknown owned config provider")


def snapshot_after(receipts, after):
    for row in receipts:
        requested = row.get("requested_ns")
        if row.get("event") == "sync" and type(requested) is int and requested > after:
            return row
    return None


def check_pixels(rgb, width, height, position, held):
    if type(width) is not int or type(height) is not int or not 0 < width <= 8192 or not 0 < height <= 8192 \
            or len(position) != 2 or any(type(value) is not int for value in position) or type(held) is not bool:
        raise ValueError("invalid capture coordinates or held state")
    if len(rgb) != width * height * 3:
        raise ValueError("capture dimensions differ from RGB bytes")
    x, y = position
    points = [(x + 12, y + 12), (x + 28, y + 12)]
    if any(not 0 <= px < width or not 0 <= py < height for px, py in points):
        raise ValueError("receiver markers are outside the captured output")
    actual = [tuple(rgb[(py * width + px) * 3:(py * width + px) * 3 + 3]) for px, py in points]
    return actual == [(0, 0, 255), (255, 0, 0) if held else (0, 255, 0)]


def validate_result(value):
    expected = dict(version=1, status="passed", source="owned_wayland_virtual_keyboard",
                    device_disable_is_neutral=False, disabled_release_dropped=True,
                    reenabled_press_dropped=True, final_neutral=True,
                    production_route_tested=False, kernel_input_tested=False, unmodified_stock_acceptance=False)
    if not isinstance(value, dict):
        raise ValueError("missing keyboard characterization result")
    for key, wanted in expected.items():
        if type(value.get(key)) is not type(wanted) or value[key] != wanted:
            raise ValueError(f"invalid keyboard characterization: {key}")
    if value.get("config_provider") not in ("lua", "hyprlang"):
        raise ValueError("missing configuration dialect")
    captures = value.get("captures")
    phases = ["neutral-before", "held-prefix", "disabled-release", "reenabled-press", "neutral-after"]
    if not isinstance(captures, list) or len(captures) != len(phases):
        raise ValueError("missing real keyboard phase captures")
    for phase, picture in zip(phases, captures):
        if not isinstance(picture, dict) or picture.get("phase") != phase \
                or type(picture.get("attempts")) is not int or not 1 <= picture["attempts"] <= 64 \
                or type(picture.get("through")) is not int or picture["through"] < 1 \
                or not isinstance(picture.get("sha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", picture["sha256"]):
            raise ValueError("invalid real keyboard phase capture")
    return value


def run(args, env, output, socket, rows, wait_rendered, capture):
    output = Path(output)
    trace = []
    pictures = []
    keyboard = None
    enabled = True
    target = None
    completed = False
    frame = output / "quiesce-keys.txt"
    hyprctl = str(Path(args.hyprland).parent / "hyprctl")

    def record(kind, **details):
        value = dict(kind=kind, **details)
        trace.append(value)
        with (output / "keyboard-quiesce-trace.jsonl").open("a") as log:
            log.write(json.dumps(value) + "\n")

    def command(arguments):
        result = subprocess.run([hyprctl] + arguments, env=env, check=True,
                                capture_output=True, text=True, timeout=3)
        record("ipc", arguments=arguments, stdout=result.stdout, stderr=result.stderr)
        return result.stdout

    def inventory():
        values = json.loads(command(["-j", "devices"]))["keyboards"]
        if not isinstance(values, list) or len(values) > 64:
            raise RuntimeError("invalid owned keyboard inventory")
        return values

    def barrier():
        after = time.monotonic_ns()
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            value = snapshot_after(rows(), after)
            if value is not None:
                record("application_barrier", receipt=value)
                return value
            time.sleep(0.005)
        raise RuntimeError("receiver did not complete a newly requested Wayland sync")

    def send(events):
        start = len(rows())
        frame.write_text("".join(f"{code} {value}\n" for code, value in events))
        keyboard.stdin.write("\n")
        keyboard.stdin.flush()
        if not select.select([keyboard.stdout], [], [], 3)[0] or keyboard.stdout.readline() != "done\n":
            raise RuntimeError("owned keyboard did not complete its producer roundtrip")
        state = barrier()
        delivered = [(row["code"], row["state"]) for row in rows()[start:]
                     if row["event"] == "key" and row["sequence"] <= state["sequence"]]
        record("synthetic_batch", emitted=events, delivered=delivered, through=state["sequence"])
        return state, delivered

    def configure(value, keys):
        nonlocal enabled
        command(enabled_arguments(args.hyprland_config, target["name"], value))
        enabled = value
        for attempt in range(8):
            state, delivered = send([(44, 1), (44, 0)])
            if state["keys"] != keys:
                raise RuntimeError("keyboard config changed the held prefix; characterization no longer matches")
            if delivered not in ([], [(44, 1), (44, 0)]):
                raise RuntimeError("keyboard config split a diagnostic press/release")
            record("enabled_probe", requested=value, attempt=attempt + 1, delivered=delivered)
            if bool(delivered) == value:
                return state
        raise RuntimeError("per-device configuration never reached the requested behavior")

    def picture(name, state):
        if not args.screencopy:
            raise RuntimeError("quiesce characterization requires real captures")
        current = rows()
        through = max(row["sequence"] for row in current if row["sequence"] <= state["sequence"]
                      and row["event"] not in ("sync", "rendered", "presented", "presentation_discarded"))
        wait_rendered(through)
        monitors = json.loads(command(["-j", "monitors"]))
        if len(monitors) != 1 or monitors[0].get("scale") != 1 or monitors[0].get("transform") != 0 \
                or monitors[0].get("x") != 0 or monitors[0].get("y") != 0:
            raise RuntimeError("unexpected owned capture coordinate space")
        width, height = monitors[0]["width"], monitors[0]["height"]
        clients = [client for client in json.loads(command(["-j", "clients"]))
                   if client.get("class") == "prek-cage-receiver" and client.get("mapped")]
        if len(clients) != 1:
            raise RuntimeError("owned receiver is not uniquely mapped")
        position = clients[0]["at"]
        destination = output / f"receiver-quiesce-{name}.png"
        deadline = time.monotonic() + 3
        for attempt in range(64):
            capture(f"quiesce-{name}")
            result = subprocess.run([args.receiver, "--png-rgb", str(width), str(height), str(destination)],
                                    env=env, check=True, capture_output=True, timeout=3)
            if check_pixels(result.stdout, width, height, position, bool(state["keys"])):
                pictures.append(dict(phase=name, path=str(destination), attempts=attempt + 1,
                                     through=through, sha256=hashlib.sha256(destination.read_bytes()).hexdigest()))
                return
            bad = output / f"receiver-quiesce-{name}-first-unmatched.png"
            if not bad.exists():
                shutil.copyfile(destination, bad)
            if time.monotonic() >= deadline:
                break
        raise RuntimeError("capture never showed the receiver identity and held-state pixels")

    before = {device["address"] for device in inventory()}
    frame.write_text("")
    with (output / "keyboard-quiesce-helper.log").open("w") as log:
        try:
            keyboard = subprocess.Popen([args.keyboard, str(socket), str(frame), "--persistent"],
                                        env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=log, text=True)
            if not select.select([keyboard.stdout], [], [], 3)[0] or keyboard.stdout.readline() != "ready\n":
                raise RuntimeError("owned characterization keyboard did not register")
            devices = inventory()
            added = [device for device in devices if device["address"] not in before]
            if len(added) != 1 or sum(device["name"] == added[0]["name"] for device in devices) != 1:
                raise RuntimeError("newly owned keyboard is ambiguous")
            target = added[0]
            state = configure(True, [])
            picture("neutral-before", state)
            state, delivered = send([(46, 1)])
            if delivered != [(46, 1)] or state["keys"] != [46]:
                raise RuntimeError("receiver did not observe the held prefix")
            picture("held-prefix", state)
            configure(False, [46])
            state, delivered = send([(46, 0)])
            if delivered or state["keys"] != [46]:
                raise RuntimeError("disabled release did not exhibit the expected retained app key")
            picture("disabled-release", state)
            state = configure(True, [46])
            state, delivered = send([(46, 1)])
            if delivered or state["keys"] != [46]:
                raise RuntimeError("re-enabled press did not exhibit the expected stale-key suppression")
            picture("reenabled-press", state)
            state, delivered = send([(46, 0)])
            if delivered != [(46, 0)] or state["keys"] or state["contacts"] \
                    or any(state["modifiers"][field] for field in ("depressed", "latched", "locked", "group")):
                raise RuntimeError("owned application did not finish neutral")
            picture("neutral-after", state)
            result = dict(version=1, status="passed", config_provider=args.hyprland_config,
                          hyprland=json.loads(command(["-j", "version"])), keyboard=target,
                          source="owned_wayland_virtual_keyboard", device_disable_is_neutral=False,
                          disabled_release_dropped=True, reenabled_press_dropped=True, final_neutral=True,
                          production_route_tested=False, kernel_input_tested=False, captures=pictures,
                          unmodified_stock_acceptance=False, dependency_override=env.get("PREK_CAGE_AQUAMARINE"))
            completed = True
        finally:
            if keyboard is not None and keyboard.poll() is None:
                try:
                    if target is not None and not completed:
                        if not enabled:
                            command(enabled_arguments(args.hyprland_config, target["name"], True))
                        send([(44, 0), (46, 0)])
                except Exception as error:
                    record("cleanup_error", error=str(error))
                finally:
                    keyboard.stdin.close()
                    try:
                        keyboard.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        keyboard.kill()
                        keyboard.wait(timeout=3)
    if keyboard.returncode != 0:
        raise RuntimeError("owned keyboard did not retire cleanly")
    cleanup = barrier()
    if cleanup["keys"] or cleanup["contacts"] or any(cleanup["modifiers"].values()):
        raise RuntimeError("receiver was not neutral after keyboard retirement")
    validate_result(result)
    (output / "keyboard-quiesce-result.json").write_text(json.dumps(result, indent=2) + "\n")
    print("PASS: negative keyboard-disable characterization; stale held key and dropped release/press verified; no handoff permission.")
    return result
