"""Keyboard disable and independent modifier ownership in an owned compositor."""
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
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


def capture_state(args, env, output, name, state, rows, wait_rendered, capture, command):
    if not args.screencopy:
        raise RuntimeError("keyboard characterization requires real captures")
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
    destination = output / f"receiver-{name}.png"
    deadline = time.monotonic() + 3
    for attempt in range(64):
        capture(name)
        result = subprocess.run([args.receiver, "--png-rgb", str(width), str(height), str(destination)],
                                env=env, check=True, capture_output=True, timeout=3)
        if check_pixels(result.stdout, width, height, position, bool(state["keys"])):
            return dict(phase=name, path=str(destination), attempts=attempt + 1,
                        through=through, sha256=hashlib.sha256(destination.read_bytes()).hexdigest())
        bad = output / f"receiver-{name}-first-unmatched.png"
        if not bad.exists():
            shutil.copyfile(destination, bad)
        if time.monotonic() >= deadline:
            break
    raise RuntimeError("capture never showed the receiver identity and held-state pixels")


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
        picture = capture_state(args, env, output, f"quiesce-{name}", state, rows, wait_rendered, capture, command)
        pictures.append(dict(picture, phase=name))

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


def ownership_expectations():
    phases = []
    text = "bA"

    def expect(phase, keyboard, emitted, delivered, keys, added="", operation="batch"):
        nonlocal text
        text += added
        phases.append(dict(phase=phase, keyboard=keyboard, operation=operation, emitted=emitted,
                           delivered=delivered, keys=keys, text=text,
                           modifiers=dict(depressed=int(bool(keys)), latched=0, locked=0, group=0)))

    tap = [[30, 1], [30, 0]]
    expect("persistent-shift", 0, [[42, 1]], [[42, 1]], [42])
    expect("persistent-first-a", 0, tap, tap, [42], "A")
    expect("persistent-second-a", 0, tap, tap, [42], "A")
    expect("persistent-duplicate-shift", 0, [[42, 1]], [], [42])
    expect("persistent-release", 0, [[42, 0]], [[42, 0]], [])
    expect("persistent-duplicate-release", 0, [[42, 0]], [], [])
    for first, second, name in [(0, 1, "second-owner-first-release"), (1, 0, "first-owner-first-release")]:
        expect(f"{name}-a-down", 0, [[42, 1]], [[42, 1]], [42])
        expect(f"{name}-b-down", 1, [[42, 1]], [], [42])
        expect(f"{name}-both-held-a", 1, tap, tap, [42], "A")
        expect(f"{name}-second-up", second, [[42, 0]], [], [42])
        expect(f"{name}-remaining-a", first, tap, tap, [42], "A")
        expect(f"{name}-last-up", first, [[42, 0]], [[42, 0]], [])
    expect("different-a-down", 0, [[42, 1]], [[42, 1]], [42])
    expect("different-b-down", 1, [[54, 1]], [[54, 1]], [42, 54])
    expect("different-b-up", 1, [[54, 0]], [[54, 0]], [42])
    expect("different-remaining-a", 0, tap, tap, [42], "A")
    expect("different-last-up", 0, [[42, 0]], [[42, 0]], [])
    expect("unmodified-a-down", 0, [[42, 1]], [[42, 1]], [42])
    expect("unmodified-first-a", 1, tap, tap, [42], "A")
    expect("unmodified-second-a", 1, tap, tap, [42], "A")
    expect("unmodified-owner-a", 0, tap, tap, [42], "A")
    expect("unmodified-last-up", 0, [[42, 0]], [[42, 0]], [])
    expect("retire-a-down", 0, [[42, 1]], [[42, 1]], [42])
    expect("retire-b-down", 1, [[42, 1]], [], [42])
    expect("retire-b", 1, [], [], [42], operation="retire")
    expect("retire-remaining-a", 0, tap, tap, [42], "A")
    expect("retire-last-up", 0, [[42, 0]], [[42, 0]], [])
    expect("retire-a", 0, [], [], [], operation="retire")
    return phases


def validate_ownership_result(value):
    expected = dict(version=1, status="passed", source="two_owned_wayland_virtual_keyboards",
                    final_neutral=True, production_route_tested=False, kernel_input_tested=False,
                    unmodified_stock_acceptance=False)
    if not isinstance(value, dict):
        raise ValueError("missing keyboard ownership result")
    for key, wanted in expected.items():
        if type(value.get(key)) is not type(wanted) or value[key] != wanted:
            raise ValueError(f"invalid keyboard ownership result: {key}")
    if value.get("config_provider") not in ("lua", "hyprlang"):
        raise ValueError("missing configuration dialect")
    devices = value.get("keyboards")
    if not isinstance(devices, list) or len(devices) != 2 \
            or any(not isinstance(device, dict) or not isinstance(device.get("address"), str)
                   or not re.fullmatch(r"0x[0-9a-f]+", device["address"]) for device in devices) \
            or devices[0]["address"] == devices[1]["address"]:
        raise ValueError("missing distinct owned keyboard identities")
    phases = value.get("phases")
    wanted_phases = ownership_expectations()
    if not isinstance(phases, list) or len(phases) != len(wanted_phases):
        raise ValueError("missing keyboard ownership phases")
    for phase, wanted in zip(phases, wanted_phases):
        if json.dumps(phase, sort_keys=True) != json.dumps(wanted, sort_keys=True):
            raise ValueError(f"keyboard ownership observation differs: {wanted['phase']}")
    captures = value.get("captures")
    names = ["persistent-held", "second-owner-first-release-one-owner", "first-owner-first-release-one-owner",
             "independent-unmodified", "retired-one-owner", "neutral-after"]
    if not isinstance(captures, list) or len(captures) != len(names):
        raise ValueError("missing keyboard ownership captures")
    previous = 0
    for name, picture in zip(names, captures):
        if not isinstance(picture, dict) or picture.get("phase") != f"ownership-{name}" \
                or type(picture.get("attempts")) is not int or not 1 <= picture["attempts"] <= 64 \
                or type(picture.get("through")) is not int or picture["through"] <= previous \
                or not isinstance(picture.get("sha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", picture["sha256"]):
            raise ValueError("invalid keyboard ownership capture")
        previous = picture["through"]
    return value


def run_ownership(args, env, output, socket, rows, wait_rendered, capture):
    output = Path(output)
    hyprctl = str(Path(args.hyprland).parent / "hyprctl")
    keyboards = []
    pictures = []
    phases = []

    def record(kind, **details):
        with (output / "keyboard-ownership-trace.jsonl").open("a") as log:
            log.write(json.dumps(dict(kind=kind, **details)) + "\n")

    def command(arguments):
        result = subprocess.run([hyprctl] + arguments, env=env, check=True,
                                capture_output=True, text=True, timeout=3)
        record("ipc", arguments=arguments, stdout=result.stdout, stderr=result.stderr)
        return result.stdout

    def inventory():
        devices = json.loads(command(["-j", "devices"]))["keyboards"]
        if not isinstance(devices, list) or len(devices) > 64:
            raise RuntimeError("invalid owned keyboard inventory")
        return devices

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

    def spawn():
        before = {device["address"] for device in inventory()}
        index = len(keyboards)
        frame = output / f"ownership-{index}-keys.txt"
        frame.write_text("")
        log = (output / f"keyboard-ownership-{index}.log").open("w")
        process = subprocess.Popen([args.keyboard, str(socket), str(frame), "--persistent"], env=env,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True)
        value = dict(process=process, frame=frame, log=log, held=set(), retired=False)
        keyboards.append(value)
        if not select.select([process.stdout], [], [], 3)[0] or process.stdout.readline() != "ready\n":
            raise RuntimeError("owned keyboard did not register")
        added = [device for device in inventory() if device["address"] not in before]
        if len(added) != 1:
            raise RuntimeError("owned keyboard address is ambiguous")
        value["device"] = added[0]
        record("owned_keyboard", index=index, device=added[0])
        return index

    def send(index, events, phase):
        keyboard = keyboards[index]
        start = len(rows())
        keyboard["frame"].write_text("".join(f"{code} {value}\n" for code, value in events))
        process = keyboard["process"]
        process.stdin.write("\n")
        process.stdin.flush()
        if not select.select([process.stdout], [], [], 3)[0] or process.stdout.readline() != "done\n":
            raise RuntimeError("owned keyboard did not complete its producer roundtrip")
        for code, value in events:
            if value:
                keyboard["held"].add(code)
            else:
                keyboard["held"].discard(code)
        state = barrier()
        delivered = [(row["code"], row["state"]) for row in rows()[start:]
                     if row["event"] == "key" and row["sequence"] <= state["sequence"]]
        value = dict(phase=phase, operation="batch", keyboard=index, emitted=events, delivered=delivered,
                     keys=state["keys"], modifiers=state["modifiers"], text=state["text"])
        phases.append(value)
        record("batch", **value)
        return state

    def picture(name, state):
        pictures.append(capture_state(args, env, output, f"ownership-{name}", state, rows,
                                      wait_rendered, capture, command))

    def retire(index, phase):
        keyboard = keyboards[index]
        start = len(rows())
        process = keyboard["process"]
        process.stdin.close()
        if process.wait(timeout=3) != 0:
            raise RuntimeError("owned keyboard did not retire cleanly")
        keyboard["retired"] = True
        state = barrier()
        if keyboard["device"]["address"] in {device["address"] for device in inventory()}:
            raise RuntimeError("retired keyboard remains registered")
        delivered = [(row["code"], row["state"]) for row in rows()[start:]
                     if row["event"] == "key" and row["sequence"] <= state["sequence"]]
        phases.append(dict(phase=phase, operation="retire", keyboard=index, emitted=[], delivered=delivered,
                           keys=state["keys"], modifiers=state["modifiers"], text=state["text"]))
        record("retired", index=index, receipt=state)
        return state

    try:
        a, b = spawn(), spawn()
        send(a, [(42, 1)], "persistent-shift")
        send(a, [(30, 1), (30, 0)], "persistent-first-a")
        state = send(a, [(30, 1), (30, 0)], "persistent-second-a")
        if state["text"] != "bAAA" or state["modifiers"]["depressed"] != 1:
            raise RuntimeError("persistent producer lost its own modifier across batches")
        picture("persistent-held", state)
        send(a, [(42, 1)], "persistent-duplicate-shift")
        send(a, [(42, 0)], "persistent-release")
        send(a, [(42, 0)], "persistent-duplicate-release")
        for first, second, name in [(a, b, "second-owner-first-release"), (b, a, "first-owner-first-release")]:
            send(a, [(42, 1)], f"{name}-a-down")
            send(b, [(42, 1)], f"{name}-b-down")
            send(b, [(30, 1), (30, 0)], f"{name}-both-held-a")
            state = send(second, [(42, 0)], f"{name}-second-up")
            picture(f"{name}-one-owner", state)
            send(first, [(30, 1), (30, 0)], f"{name}-remaining-a")
            send(first, [(42, 0)], f"{name}-last-up")
        send(a, [(42, 1)], "different-a-down")
        send(b, [(54, 1)], "different-b-down")
        send(b, [(54, 0)], "different-b-up")
        send(a, [(30, 1), (30, 0)], "different-remaining-a")
        send(a, [(42, 0)], "different-last-up")
        send(a, [(42, 1)], "unmodified-a-down")
        send(b, [(30, 1), (30, 0)], "unmodified-first-a")
        state = send(b, [(30, 1), (30, 0)], "unmodified-second-a")
        picture("independent-unmodified", state)
        send(a, [(30, 1), (30, 0)], "unmodified-owner-a")
        send(a, [(42, 0)], "unmodified-last-up")
        send(a, [(42, 1)], "retire-a-down")
        send(b, [(42, 1)], "retire-b-down")
        state = retire(b, "retire-b")
        picture("retired-one-owner", state)
        send(a, [(30, 1), (30, 0)], "retire-remaining-a")
        send(a, [(42, 0)], "retire-last-up")
        state = retire(a, "retire-a")
        if state["keys"] or state["contacts"] or any(state["modifiers"].values()):
            raise RuntimeError("owned keyboard characterization did not finish neutral")
        picture("neutral-after", state)
        result = dict(version=1, status="passed", config_provider=args.hyprland_config,
                      hyprland=json.loads(command(["-j", "version"])), source="two_owned_wayland_virtual_keyboards",
                      keyboards=[keyboard["device"] for keyboard in keyboards], phases=phases, captures=pictures,
                      final_neutral=True, production_route_tested=False, kernel_input_tested=False,
                      unmodified_stock_acceptance=False, dependency_override=env.get("PREK_CAGE_AQUAMARINE"))
        result = json.loads(json.dumps(result))
        validate_ownership_result(result)
        (output / "keyboard-ownership-result.json").write_text(json.dumps(result, indent=2) + "\n")
        print("PASS: two owned protocol keyboards preserve held modifiers across overlap, both release orders and retirement; no native routing permission.")
        return result
    finally:
        for index, keyboard in enumerate(keyboards):
            process = keyboard["process"]
            if process.poll() is None:
                try:
                    send(index, [(code, 0) for code in sorted(keyboard["held"])], "cleanup")
                    retire(index, "cleanup-retire")
                except Exception as error:
                    record("cleanup_error", index=index, error=str(error))
                    if process.poll() is None:
                        process.kill()
                        process.wait(timeout=3)
            for stream in (process.stdin, process.stdout):
                if stream is not None and not stream.closed:
                    stream.close()
            keyboard["log"].close()


def validate_native_key_arbitration(observations, received, inputs):
    expected = [
        ("buffered-tap-selective", [(31, 1), (31, 0), (30, 1), (30, 0)]),
        ("buffered-hold-priority", [(30, 1), (30, 0)]),
        ("buffered-sequence-selective", [(30, 1), (30, 0), (31, 1), (31, 0)]),
        ("buffered-deferred-key-sequence", [(30, 1), (30, 0)]),
        ("buffered-unmatched-tap-fallback", [(31, 1), (31, 0)]),
        ("buffered-deferred-action-order", [(30, 1), (30, 0), (48, 1), (48, 0)]),
    ]
    if len(observations) != 35:
        raise ValueError("missing native key arbitration captures")
    keys = [row for row in received if row.get("event") == "key"]
    if any(any(type(row.get(field)) is not int for field in ("sequence", "code", "state", "at_ns")) for row in keys):
        raise ValueError("invalid native key event fields")
    fallback = []
    for index, (name, wanted) in enumerate(expected, 25):
        lower, upper = observations[index - 1]["receipt"]["sequence"], observations[index]["receipt"]["sequence"]
        if observations[index]["name"] != name or type(lower) is not int or type(upper) is not int or lower >= upper:
            raise ValueError("invalid native key capture boundary")
        events = [row for row in keys if lower < row["sequence"] <= upper]
        if [(row["code"], row["state"]) for row in events] != wanted:
            raise ValueError("native key arbitration leaked, duplicated or reordered app input")
        if name == "buffered-unmatched-tap-fallback":
            fallback = events
    controls = {195, 197, 198, 199, 200, 201}
    actual = [(event["code"], event["value"]) for frame in inputs for event in frame["events"]
              if event["kind"] == 1 and event["code"] in controls]
    wanted = [(195, 1), (195, 0), (195, 1), (195, 0), (197, 1), (197, 0),
              (198, 1), (198, 0), (199, 1), (199, 0), (200, 1), (200, 0),
              (199, 1), (199, 0), (200, 1), (200, 0), (201, 1), (201, 0)]
    if actual != wanted:
        raise ValueError("missing original consumed key evidence")
    deferred = [(index, frame["at"]["secs"] * 1_000_000_000 + frame["at"]["nanos"])
                for index, frame in enumerate(inputs) for event in frame["events"]
                if event["kind"] == 1 and event["code"] in {199, 200, 201}][-6:]
    frames = [index for index, _ in deferred]
    if len(deferred) != 6 or not (frames[0] == frames[1] < frames[2] == frames[3] < frames[4] == frames[5]):
        raise ValueError("deferred actions did not arrive in separate ordered reports")
    arrival_gap = deferred[-1][1] - deferred[0][1]
    if not 0 <= arrival_gap < 20_000_000:
        raise ValueError("later action did not arrive inside the original tap settlement window")
    original = [(frame["at"]["secs"] * 1_000_000_000 + frame["at"]["nanos"], event["value"])
                for frame in inputs for event in frame["events"] if event["kind"] == 1 and event["code"] == 31][-2:]
    if len(original) != 2 or [value for _, value in original] != [1, 0]:
        raise ValueError("missing unmatched tap input interval")
    gap = original[1][0] - original[0][0]
    delivery = fallback[1]["at_ns"] - fallback[0]["at_ns"]
    if gap < 40_000_000 or delivery < gap - 20_000_000:
        raise ValueError("native unmatched key fallback burst its original interval")
    return dict(status="passed", scope="owned_protocol_native_keys", cases=6,
                consumed_control_edges=18, leaked_control_edges=0,
                deferred_arrival_gap_ns=arrival_gap, deferred_arrival_window_ns=20_000_000,
                input_gap_ns=gap, delivery_gap_ns=delivery, slack_ns=20_000_000,
                source_and_readiness_modeled=True, physical_takeover_tested=False)


def run_key_output(args, env, output, socket, rows, wait_rendered, capture):
    if not args.key_output_fixture:
        raise RuntimeError("key-output test requires its explicitly built Rust fixture")
    output = Path(output)
    hyprctl = str(Path(args.hyprland).parent / "hyprctl")
    pictures = []
    phases = []

    def command(arguments):
        return subprocess.run([hyprctl] + arguments, env=env, check=True,
                              capture_output=True, text=True, timeout=3).stdout

    expected = [("physical-held", "bA", [42]), ("overlapping-remap-chord", "bAA", [42]),
                ("remaps-released-physical-held", "bAAA", [42]), ("physical-neutral", "bAAA", []),
                ("cancelled-unwritten-chord", "bAAAa", []), ("retired-remap-one-owner", "bAAAa", [42]),
                ("source-retired-neutral", "bAAAaA", []), ("task-cancelled-sibling-held", "bAAAaA", [42]),
                ("context-cancelled-neutral", "bAAAaA", []), ("rebound-press-kept", "bAAAaAA", [42]),
                ("context-reentered-neutral", "bAAAaAAa", []),
                ("software-owners-held", "bAAAaAAaA", [42]),
                ("software-one-producer-retired", "bAAAaAAaAA", [42]),
                ("software-retired-physical-held", "bAAAaAAaAAA", [42]),
                ("software-retired-neutral", "bAAAaAAaAAAa", [])]
    if args.key_worker:
        expected = [("ordered-fast-remap", "bAA", []), ("scoped-remap-held", "bAA", [42]),
                    ("cancelled-one-sibling-held", "bAA", [42]), ("scoped-unwritten-chord", "bAA", []),
                    ("grant-revoked-neutral", "bAA", []), ("fresh-scoped-chord", "bAAa", []),
                    ("within-report-selective", "bAAaAa", []), ("cross-source-modifier", "bAAaAaA", []),
                    ("scoped-source-retired", "bAAaAaA", []),
                    ("software-scoped-held", "bAAaAaAA", [42]),
                    ("software-scoped-sibling-held", "bAAaAaAAA", [42]),
                    ("software-scoped-expired", "bAAaAaAAAa", []),
                    ("timed-later-modifier-rejected", "bAAaAaAAAa", []),
                    ("timed-begin-only-hold", "bAAaAaAAAaa", []),
                    ("timed-begin-only-taps", "bAAaAaAAAaaa", []),
                    ("chord-throughout-revoked", "bAAaAaAAAaaa", []),
                    ("chord-fresh-admitted", "bAAaAaAAAaaaa", []),
                    ("tap-before-held-tail", "bAAaAaAAAaaaaa", [187, 190]),
                    ("tap-fresh-after-held-tail", "bAAaAaAAAaaaaaa", [190]),
                    ("tap-invalid-activation-rejected", "bAAaAaAAAaaaaaa", []),
                    ("tap-fresh-activation-reentry", "bAAaAaAAAaaaaaaa", []),
                    ("two-sequences-one-report", "bAAaAaAAAaaaaaaaaa", []),
                    ("deferred-same-report-key", "bAAaAaAAAaaaaaaaaaa", []),
                    ("reversed-deferred-key-rejected", "bAAaAaAAAaaaaaaaaaa", []),
                    ("deferred-key-fresh-reentry", "bAAaAaAAAaaaaaaaaaaa", []),
                    ("buffered-tap-selective", "bAAaAaAAAaaaaaaaaaaasa", []),
                    ("buffered-hold-priority", "bAAaAaAAAaaaaaaaaaaasaa", []),
                    ("buffered-sequence-selective", "bAAaAaAAAaaaaaaaaaaasaaas", []),
                    ("buffered-deferred-key-sequence", "bAAaAaAAAaaaaaaaaaaasaaasa", []),
                    ("buffered-unmatched-tap-fallback", "bAAaAaAAAaaaaaaaaaaasaaasas", []),
                    ("buffered-deferred-action-order", "bAAaAaAAAaaaaaaaaaaasaaasasab", []),
                    ("journal-pending-withheld", "bAAaAaAAAaaaaaaaaaaasaaasasab", []),
                    ("journal-decided-consumed", "bAAaAaAAAaaaaaaaaaaasaaasasab", []),
                    ("journal-fallback-cadence", "bAAaAaAAAaaaaaaaaaaasaaasasaba", []),
                    ("software-scoped-shutdown", "bAAaAaAAAaaaaaaaaaaasaaasasaba", [])]
    fixture_env = dict(env, PREK_LOCAL_CAGE="1", PREK_CAGE_OUTPUT=str(output), PREK_CAGE_KEYBOARD=args.keyboard,
                       PREK_CAGE_KEY_SOCKET=str(socket))
    before = {device["address"] for device in json.loads(command(["-j", "devices"]))["keyboards"]}
    with (output / "key-output-runtime.log").open("w") as log:
        test_name = "worker::local_cage_scoped_key_worker" if args.key_worker else "local_cage_generated_key_output"
        process = subprocess.Popen([str(args.key_output_fixture), "--ignored", "--exact", test_name, "--nocapture"],
                                   env=fixture_env, stdout=log, stderr=log, start_new_session=True)
        try:
            for index, (name, text, keys) in enumerate(expected):
                deadline = time.monotonic() + 10
                while not (output / f"key-output-ready-{index}").exists():
                    if process.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError(f"key-output Rust fixture stopped before phase {index}; see {output / 'key-output-runtime.log'}")
                    time.sleep(0.005)
                state = json.loads((output / f"key-output-phase-{index}.json").read_text())
                if state["text"] != text or state["keys"] != keys or state["contacts"] \
                        or state["modifiers"] != dict(depressed=int(any(key in (42, 54) for key in keys)), latched=0, locked=0, group=0):
                    raise RuntimeError(f"actual key output delivery differs at {name}")
                phases.append(dict(name=name, receipt=state))
                pictures.append(capture_state(args, env, output, f"key-output-{name}", state, rows, wait_rendered, capture, command))
                (output / f"key-output-captured-{index}").write_text("captured")
            if process.wait(timeout=5) != 0:
                raise RuntimeError("key-output Rust fixture failed after its captures")
        finally:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=3)
    after = {device["address"] for device in json.loads(command(["-j", "devices"]))["keyboards"]}
    if after != before:
        raise RuntimeError("owned key-output producer did not retire")
    result = json.loads((output / "key-output-runtime-result.json").read_text())
    wanted = dict(status="passed", phases=15, source="two_owned_wayland_virtual_keyboards", production_route_tested=False,
                  kernel_input_tested=False, unmodified_stock_acceptance=False, test_backpressure=True)
    if args.key_worker:
        wanted.update(phases=35, source="four_owned_wayland_virtual_keyboards", scoped_worker_tested=True, authority_modeled=True,
                      transport_readiness_modeled=True, selective_consumption_tested=True,
                      selective_consumption_scope="owned_protocol_native_keys", physical_takeover_tested=False,
                      selective_projection_tested=True, within_frame_order_tested=True, cross_source_order_tested=True,
                      software_actor_tested=True, software_lease_tested=True, software_private_control_tested=False,
                      software_authorization_tested=False, software_repeat_rendering_tested=False,
                      timed_condition_rendering_tested=True, chord_condition_rendering_tested=True,
                      tap_tail_lifecycle_rendering_tested=True, tap_invalid_activation_rendering_tested=True,
                      event_local_sequence_rendering_tested=True, deferred_key_completion_rendering_tested=True,
                      pending_journal_protocol_tested=True, pending_fallback_cadence_tested=True,
                      delayed_gesture_arbitration_tested=True, delayed_gesture_arbitration_scope="native_key_only",
                      mixed_delayed_gesture_arbitration_tested=False)
        journal = json.loads((output / "pending-journal-receipt.json").read_text())
        if (journal.get("status") != "passed" or journal.get("consumed_reports_written") != 0
                or journal.get("fallback_reports_written") != 2
                or journal.get("physical_neutral") is not True or journal.get("delivered_neutral") is not True
                or journal.get("authority_modeled") is not True
                or journal.get("delayed_gesture_arbitration_tested") is not False
                or not all(type(journal.get(key)) is int for key in
                           ("input_gap_ns", "scheduled_gap_ns", "delivery_gap_ns", "slack_ns"))
                or journal["input_gap_ns"] < 80_000_000 or journal["slack_ns"] != 20_000_000
                or journal["scheduled_gap_ns"] != journal["input_gap_ns"]
                or journal["delivery_gap_ns"] < journal["input_gap_ns"] - journal["slack_ns"]):
            raise RuntimeError("invalid pending-key protocol cadence receipt")
        inputs = [json.loads(line) for line in (output / "key-worker-input.jsonl").read_text().splitlines()]
        native_arbitration = validate_native_key_arbitration(phases, rows(), inputs)
        (output / "native-key-arbitration-receipt.json").write_text(json.dumps(native_arbitration, indent=2) + "\n")
    else:
        wanted.update(software_owner_tested=True, software_authorization_tested=False, software_repeat_rendering_tested=False)
    if json.dumps(result, sort_keys=True) != json.dumps(wanted, sort_keys=True):
        raise RuntimeError("invalid key-output runtime receipt")
    result.update(captures=pictures, observations=phases, config_provider=args.hyprland_config,
                  dependency_override=env.get("PREK_CAGE_AQUAMARINE"))
    name = "key-worker" if args.key_worker else "key-output"
    (output / f"{name}-result.json").write_text(json.dumps(result, indent=2) + "\n")
    if args.key_worker:
        print("PASS: native key tap/hold/sequence arbitration selectively consumed exact presses, ordered deferred actions and preserved fallback cadence; thirty-five rendered protocol-sink phases. Authority/readiness modeled; mixed/custom arbitration, native takeover, software authorization and repeat rendering are not tested.")
    else:
        print("PASS: actual key-output driver preserved overlapping owners, software producer isolation, exact cancellation and context reentry; fifteen rendered app phases. Software authorization and repeat rendering are not tested.")
    return result
