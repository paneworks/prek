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


def validate_hybrid_keys(observations, received, runtime, raw_trace, generated_trace):
    flags = dict(status="passed", scope="owned_decoded_keyboard_relative_protocol", dispatches=2,
                 source_and_readiness_modeled=True, native_hybrid_topology_tested=False,
                 physical_takeover_tested=False, kernel_input_opened=False, mt_rendering_tested=False,
                 generated_neutral=True, slack_ns=20_000_000)
    if any(type(runtime.get(key)) is not type(value) or runtime[key] != value for key, value in flags.items()):
        raise ValueError("invalid hybrid protocol scope")
    if len(observations) != 42 or [row["name"] for row in observations[38:41]] != [
            "hybrid-pointer-baseline", "hybrid-key-relative-consumed", "hybrid-key-relative-fallback"]:
        raise ValueError("missing hybrid protocol captures")
    def nanos(stamp):
        if (not isinstance(stamp, dict) or set(stamp) != {"secs", "nanos"}
                or any(type(stamp[key]) is not int or stamp[key] < 0 for key in stamp) or stamp["nanos"] >= 1_000_000_000):
            raise ValueError("invalid hybrid timestamp")
        return stamp["secs"] * 1_000_000_000 + stamp["nanos"]
    def events(values):
        return [dict(kind=kind, code=code, value=value) for kind, code, value in values] + [dict(kind=0, code=0, value=0)]
    def equal(left, right):
        return json.dumps(left, sort_keys=True) == json.dumps(right, sort_keys=True)
    expected = [events([(2, 0, 7), (1, 202, 1), (2, 0, 9), (1, 31, 1), (1, 31, 0), (1, 202, 0)]),
                events([(2, 0, 7), (1, 203, 1)]), events([(2, 0, 9), (1, 203, 0)])]
    inputs, delivered = runtime.get("inputs"), runtime.get("delivered")
    if not isinstance(inputs, list) or len(inputs) != 3 or not isinstance(delivered, list) or len(delivered) != 4:
        raise ValueError("invalid hybrid report counts")
    source = dict(device=600, generation=1, seat="seat0")
    for index, frame in enumerate(inputs):
        if not equal(frame.get("source"), source) or type(frame.get("sequence")) is not int or frame["sequence"] != index + 1 or not equal(frame.get("events"), expected[index]):
            raise ValueError("hybrid original report changed")
    gap = nanos(inputs[2]["at"]) - nanos(inputs[1]["at"])
    if gap < 80_000_000 or nanos(inputs[1]["at"]) <= nanos(inputs[0]["at"]):
        raise ValueError("invalid hybrid original cadence")
    projected = [events([(1, 31, 1), (1, 31, 0)]), events([(2, 0, 7), (2, 0, 9)]), expected[1], expected[2]]
    for index, output in enumerate(delivered):
        frame = output["frame"]
        if (not equal(frame.get("source"), source) or type(frame.get("sequence")) is not int or frame["sequence"] != index + 1 or not equal(frame.get("events"), projected[index])
                or frame.get("at") != inputs[max(0, index - 1)]["at"] or nanos(output["sent"]) < nanos(output["due"])):
            raise ValueError("hybrid projection, provenance or delivery changed")
    if (nanos(delivered[3]["due"]) - nanos(delivered[2]["due"]) != gap
            or nanos(delivered[3]["sent"]) - nanos(delivered[2]["sent"]) < gap - flags["slack_ns"]):
        raise ValueError("hybrid fallback cadence collapsed")
    raw = [row["frame"] for row in raw_trace if row.get("event") == "enqueue"]
    if len(raw) != 4 or any(not equal(frame, dict(output["frame"], source=dict(device=962, generation=1, seat="seat0")))
                            for frame, output in zip(raw, delivered)):
        raise ValueError("decoded hybrid sink differs from scheduled reports")
    generated = [event for row in generated_trace if row.get("event") == "enqueue" for event in row["frame"]["events"] if event["kind"] == 1]
    if not equal(generated, [dict(kind=1, code=30, value=1), dict(kind=1, code=30, value=0)]):
        raise ValueError("hybrid generated action leaked or duplicated")
    lower, middle, upper = [observations[index]["receipt"]["sequence"] for index in (38, 39, 40)]
    if any(type(value) is not int for value in (lower, middle, upper)) or not lower < middle < upper:
        raise ValueError("invalid hybrid capture boundaries")
    baseline = [row for row in received if row.get("event") in ("pointer_enter", "pointer_motion") and row["sequence"] <= lower]
    if not baseline:
        raise ValueError("missing actual hybrid pointer baseline")
    x, y = baseline[-1]["x"], baseline[-1]["y"]
    if not isinstance(x, (int, float)) or not isinstance(y, (int, float)):
        raise ValueError("invalid actual pointer baseline")
    claimed = [row for row in received if lower < row["sequence"] <= middle and row.get("event") in ("key", "pointer_motion", "pointer_frame")]
    fallback = [row for row in received if middle < row["sequence"] <= upper and row.get("event") in ("key", "pointer_motion", "pointer_frame")]
    if any(any(type(row.get(field)) is not int for field in ("sequence", "at_ns")) for row in claimed + fallback):
        raise ValueError("invalid hybrid app event identity")
    if any(any(type(row.get(field)) is not int for field in ("code", "state")) for row in claimed + fallback if row["event"] == "key"):
        raise ValueError("invalid hybrid app key event")
    if ([row["event"] for row in claimed] != ["key"] * 4 + ["pointer_motion", "pointer_frame"] + ["key"] * 2 + ["pointer_motion", "pointer_frame"]
            or [(row["code"], row["state"]) for row in claimed[:4]] != [(30, 1), (30, 0), (31, 1), (31, 0)]
            or [(row["code"], row["state"]) for row in claimed[6:8]] != [(105, 1), (105, 0)]
            or (claimed[4]["x"], claimed[4]["y"]) != (x + 16, y)
            or (claimed[8]["x"], claimed[8]["y"]) != (x + 21, y + 3)):
        raise ValueError("hybrid key cut or coherent relative commit changed at the app")
    if ([row["event"] for row in fallback] != ["key", "pointer_motion", "pointer_frame"] * 2
            or [(fallback[index]["code"], fallback[index]["state"]) for index in (0, 3)] != [(203, 1), (203, 0)]
            or [(fallback[index]["x"], fallback[index]["y"]) for index in (1, 4)] != [(x + 28, y + 3), (x + 37, y + 3)]
            or fallback[3]["at_ns"] - fallback[0]["at_ns"] < gap - flags["slack_ns"]):
        raise ValueError("hybrid raw fallback changed payload, ordering or actual app cadence")
    return dict(flags, input_gap_ns=gap, app_gap_ns=fallback[3]["at_ns"] - fallback[0]["at_ns"],
                coherent_relative_reports=4, consumed_control_leaks=0, pointer_displacement=37,
                pointer_displacement_y=3, independent_domain_rendering_tested=True)


def validate_domain_journal(observations, received, runtime, trace):
    flags = dict(status="passed", scope="owned_protocol_configured_domains", source_and_readiness_modeled=True,
                 native_gesture_arbitration_tested=True, physical_takeover_tested=False,
                 kernel_input_opened=False, mt_rendering_tested=False, switch_ownership_tested=False)
    if any(type(runtime.get(key)) is not type(value) or runtime[key] != value for key, value in flags.items()):
        raise ValueError("invalid domain journal scope")
    if len(observations) != 42 or observations[39]["name"] != "hybrid-key-relative-consumed":
        raise ValueError("missing domain journal capture")
    def events(values):
        return [dict(kind=kind, code=code, value=value) for kind, code, value in values] + [dict(kind=0, code=0, value=0)]
    def equal(left, right):
        return json.dumps(left, sort_keys=True) == json.dumps(right, sort_keys=True)
    def nanos(stamp):
        if (not isinstance(stamp, dict) or set(stamp) != {"secs", "nanos"}
                or any(type(stamp[key]) is not int or stamp[key] < 0 for key in stamp) or stamp["nanos"] >= 1_000_000_000):
            raise ValueError("invalid domain journal time")
        return stamp["secs"] * 1_000_000_000 + stamp["nanos"]
    inputs, delivered = runtime.get("inputs"), runtime.get("delivered")
    if not isinstance(inputs, list) or len(inputs) != 2 or not isinstance(delivered, list) or len(delivered) != 2:
        raise ValueError("invalid domain journal report inventory")
    original = [events([(2, 0, 47), (1, 105, 1), (2, 1, 31), (1, 105, 0)]), events([(2, 0, 5), (2, 1, 3)])]
    projected = [events([(1, 105, 1), (1, 105, 0)]), original[1]]
    source = dict(device=601, generation=1, seat="seat0")
    for index, (frame, item) in enumerate(zip(inputs, delivered)):
        if (not equal(frame.get("source"), source) or type(frame.get("sequence")) is not int or frame["sequence"] != index + 1
                or not equal(frame.get("events"), original[index]) or not isinstance(item, dict)):
            raise ValueError("domain journal original input differs")
        scheduled = item.get("frame")
        if (not isinstance(scheduled, dict) or not equal(scheduled.get("source"), source)
                or type(scheduled.get("sequence")) is not int or scheduled["sequence"] != index + 1
                or not equal(scheduled.get("events"), projected[index]) or scheduled.get("at") != frame.get("at")
                or nanos(item["sent"]) < nanos(item["due"]) or nanos(item["due"]) < nanos(frame["at"])):
            raise ValueError("domain journal projection or clock differs")
    if nanos(inputs[1]["at"]) <= nanos(inputs[0]["at"]):
        raise ValueError("domain journal original order differs")
    configured = runtime.get("configured")
    if (not isinstance(configured, dict) or configured.get("supervised_lua") is not True
            or configured.get("helper_stopped") is not True or type(configured.get("cleanup_count")) is not int
            or configured["cleanup_count"] != 2):
        raise ValueError("missing configured domain callback or cleanup")
    ticket, proposal, dispatch = (configured.get(key, {}) for key in ("ticket", "proposal", "dispatch"))
    original = ticket.get("input", {})
    if (type(ticket.get("id")) is not int or ticket["id"] <= 0
            or type(ticket.get("candidate")) is not int or ticket["candidate"] <= 0
            or ticket.get("cause") != "frame" or ticket.get("rule") != "rendered-relative"
            or ticket.get("recognizer") != "rendered_relative" or not isinstance(ticket.get("fence"), dict)
            or original.get("kind") != "frame" or not equal(original.get("source"), source)
            or type(original.get("sequence")) is not int or original["sequence"] != 1
            or original.get("at") != inputs[0]["at"] or original.get("selectors") != ["relative"]
            or not equal(original.get("changes"), [dict(kind="relative", code=0, value=47), dict(kind="relative", code=1, value=31)])
            or any(original.get(field) != [] for field in ("contacts", "contacts_started", "contacts_ended", "ended_contacts"))):
        raise ValueError("configured domain ticket differs from declared original input")
    if (type(proposal.get("ticket")) is not int or proposal["ticket"] != ticket["id"]
            or proposal.get("fence") != ticket["fence"] or proposal.get("phase") != "discrete"
            or not equal(proposal.get("data"), dict(x=47, y=31))
            or dispatch.get("outcome") != "matched" or dispatch.get("rule") != ticket["rule"]
            or type(dispatch.get("id")) is not int or dispatch["id"] != ticket["candidate"]
            or dispatch.get("stage") != "discrete" or not equal(dispatch.get("source"), source)
            or dispatch.get("action") != dict(kind="event", name="relative-consumed")
            or not equal(dispatch.get("evidence"), dict(kind="custom", recognizer="rendered_relative", data=dict(x=47, y=31)))):
        raise ValueError("configured domain proposal and native dispatch do not correlate")
    if not (nanos(inputs[0]["at"]) <= nanos(ticket["requested_at"]) <= nanos(configured["admitted_at"])
            < nanos(ticket["deadline"]) and nanos(dispatch["at"]) <= nanos(configured["admitted_at"])
            < nanos(inputs[1]["at"])):
        raise ValueError("configured domain callback chronology differs")
    ordinary_ticket, ordinary_proposal = (configured.get(key, {}) for key in ("ordinary_ticket", "ordinary_proposal"))
    ordinary = ordinary_ticket.get("input", {})
    if (type(ordinary_ticket.get("id")) is not int or ordinary_ticket["id"] <= ticket["id"]
            or type(ordinary_ticket.get("candidate")) is not int or ordinary_ticket["candidate"] <= ticket["candidate"]
            or ordinary_ticket.get("cause") != "frame" or ordinary_ticket.get("rule") != ticket["rule"]
            or ordinary_ticket.get("recognizer") != ticket["recognizer"] or ordinary_ticket.get("fence") != ticket["fence"]
            or ordinary.get("kind") != "frame" or not equal(ordinary.get("source"), source)
            or type(ordinary.get("sequence")) is not int or ordinary["sequence"] != 2
            or ordinary.get("at") != inputs[1]["at"] or ordinary.get("selectors") != ["relative"]
            or not equal(ordinary.get("changes"), [dict(kind="relative", code=0, value=5), dict(kind="relative", code=1, value=3)])
            or any(ordinary.get(field) != [] for field in ("contacts", "contacts_started", "contacts_ended", "ended_contacts"))
            or type(ordinary_proposal.get("ticket")) is not int or ordinary_proposal["ticket"] != ordinary_ticket["id"]
            or ordinary_proposal.get("fence") != ticket["fence"] or ordinary_proposal.get("phase") != "cancel"
            or not equal(ordinary_proposal.get("data"), dict(x=5, y=3))
            or type(configured.get("ordinary_dispatches")) is not int or configured["ordinary_dispatches"] != 0):
        raise ValueError("ordinary positive control lacks its actual cancelled callback")
    if not (nanos(inputs[1]["at"]) <= nanos(ordinary_ticket["requested_at"])
            <= nanos(configured["ordinary_admitted_at"]) < nanos(ordinary_ticket["deadline"])
            and nanos(configured["ordinary_admitted_at"]) <= nanos(delivered[1]["sent"])):
        raise ValueError("ordinary positive control callback chronology differs")
    sent = [row["frame"] for row in trace if row.get("event") == "enqueue"]
    if len(sent) != 2 or any(not equal(frame, dict(item["frame"], source=dict(device=964, generation=1, seat="seat0")))
                             for frame, item in zip(sent, delivered)):
        raise ValueError("domain journal sink differs")
    lower, upper = (observations[index]["receipt"]["sequence"] for index in (38, 39))
    if type(lower) is not int or type(upper) is not int or lower >= upper:
        raise ValueError("invalid domain journal app boundary")
    window = [row for row in received if lower < row["sequence"] <= upper and row.get("event") in ("key", "pointer_motion", "pointer_frame")]
    if ([row["event"] for row in window] != ["key"] * 4 + ["pointer_motion", "pointer_frame"] + ["key"] * 2 + ["pointer_motion", "pointer_frame"]
            or [(row.get("code"), row.get("state")) for row in window[6:8]] != [(105, 1), (105, 0)]
            or any(type(row.get("at_ns")) is not int for row in window)
            or any(left["at_ns"] > right["at_ns"] for left, right in zip(window, window[1:]))
            or any(type(row.get(field)) is not int for row in window if row["event"] == "key" for field in ("code", "state"))
            or any(type(row.get(field)) not in (int, float) for row in window if row["event"] == "pointer_motion" for field in ("x", "y"))
            or window[6]["at_ns"] < nanos(inputs[0]["at"]) or window[8]["at_ns"] < nanos(configured["ordinary_admitted_at"])
            or (window[8].get("x"), window[8].get("y")) != (window[4].get("x", 0) + 5, window[4].get("y", 0) + 3)):
        raise ValueError("domain journal selective app delivery differs")
    return dict(flags, consumed_motion_reports=1, forwarded_motion_reports=1,
                ordinary_key_edges=2, consumed_motion_leaks=0, pointer_displacement_x=5, pointer_displacement_y=3)


def validate_native_key_arbitration(observations, received, inputs):
    expected = [
        ("buffered-tap-selective", [(31, 1), (31, 0), (30, 1), (30, 0)]),
        ("buffered-hold-priority", [(30, 1), (30, 0)]),
        ("buffered-sequence-selective", [(30, 1), (30, 0), (31, 1), (31, 0)]),
        ("buffered-deferred-key-sequence", [(30, 1), (30, 0)]),
        ("buffered-unmatched-tap-fallback", [(31, 1), (31, 0)]),
        ("buffered-deferred-action-order", [(30, 1), (30, 0), (48, 1), (48, 0)]),
    ]
    if len(observations) != 42:
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


def scoped_order(text):
    """Rendered text with phases 31-33 callback actions at their offered cutpoints."""
    base = "bAAaAaAAAaaaaaaaaaaasaaasasab"
    tail = text[len(base):] if text.startswith(base) else ""
    for old, new in (("sassassa", "assassas"), ("sassa", "assas"), ("sa", "as")):
        if tail.startswith(old):
            return base + new + tail[len(old):]
    return text


def validate_custom_key_arbitration(observations, received, runtime, event_local=False, composition=None, scoped=False):
    if composition not in (None, "native_prefix", "native_suffix") or event_local and composition:
        raise ValueError("invalid custom key composition")
    native_first, custom_first = composition == "native_prefix", composition == "native_suffix"
    flags = dict(status="passed", scope="owned_protocol_composed_key_only_custom" if composition else "owned_protocol_root_key_only_custom", actual_lua_callback=True,
                 source_and_readiness_modeled=True, physical_takeover_tested=False, kernel_input_opened=False,
                 helper_stopped=True, generated_neutral=True)
    if not isinstance(runtime, dict) or set(runtime) != set(flags) | {
            "ordinary", "inputs", "ticket", "proposal", "proposed_at", "admitted_at", "activation", "trigger", "work_admitted_at", "reply_dispatches"}:
        raise ValueError("invalid custom key runtime fields")
    for field, wanted in flags.items():
        if type(runtime.get(field)) is not type(wanted) or runtime[field] != wanted:
            raise ValueError("invalid custom key runtime scope")
    phase = 33 if native_first else 34 if custom_first else 32 if event_local else 31
    if type(runtime["reply_dispatches"]) is not int or runtime["reply_dispatches"] != (0 if custom_first else 1):
        raise ValueError("invalid dispatch count at actual custom reply admission")
    name = "custom-lua-native-prefix" if native_first else "custom-lua-native-suffix" if custom_first else "custom-lua-event-local-activation" if event_local else "custom-lua-unseen-key-tail"
    trigger = runtime["trigger"]
    if composition:
        if not isinstance(trigger, dict) or trigger.get("kind") != "sequence" or trigger.get("timeout") != "200ms" or len(trigger.get("steps", [])) != 2:
            raise ValueError("missing actual configured key sequence")
        native, trigger = (trigger["steps"][0], trigger["steps"][1]) if native_first else (trigger["steps"][1], trigger["steps"][0])
        if native != dict(kind="key", key="code:202" if native_first else "code:203"):
            raise ValueError("invalid actual native sequence step")
    if not isinstance(trigger, dict) or trigger.get("kind") != "custom" or trigger.get("recognizer", {}).get("name") != "rendered_key" or trigger["recognizer"].get("inputs") != ["key"] or trigger["recognizer"].get("selectors") != ["custom_keys"]:
        raise ValueError("invalid actual custom sequence step")
    activation = dict(require_keys=[42] if event_local else [], keys_throughout=False)
    if (runtime["activation"] != activation or type(runtime["activation"].get("keys_throughout")) is not bool
            or any(type(code) is not int for code in runtime["activation"]["require_keys"])):
        raise ValueError("invalid custom key activation policy")
    if len(observations) != 42 or observations[phase]["name"] != name:
        raise ValueError("missing custom key rendered phase")
    lower, upper = (observations[index]["receipt"]["sequence"] for index in (phase - 1, phase))
    if type(lower) is not int or type(upper) is not int or lower >= upper:
        raise ValueError("invalid custom key rendered boundary")
    events = [row for row in received if row.get("event") == "key" and lower < row["sequence"] <= upper]
    if any(any(type(row.get(field)) is not int for field in ("sequence", "code", "state", "at_ns")) for row in events):
        raise ValueError("invalid custom key app event fields")
    ordinary = [(203, 1), (203, 0), (31, 1), (31, 0)] if custom_first else [(31, 1), (31, 0)] * (2 if event_local or native_first else 1)
    # Scoped routing writes a callback action at its offered cutpoint, before later unseen input.
    cut = len(ordinary) if custom_first or not scoped else len(ordinary) - 2
    if [(row["code"], row["state"]) for row in events] != ordinary[:cut] + [(30, 1), (30, 0)] + ordinary[cut:]:
        raise ValueError("custom reply consumed unrelated input or reordered its action")
    def stamp(value):
        if not isinstance(value, dict) or set(value) != {"secs", "nanos"} or any(type(value[key]) is not int for key in value):
            raise ValueError("invalid custom key clock")
        if value["secs"] < 0 or not 0 <= value["nanos"] < 1_000_000_000:
            raise ValueError("invalid custom key clock range")
        return value["secs"] * 1_000_000_000 + value["nanos"]
    inputs, ticket, proposal = (runtime[key] for key in ("inputs", "ticket", "proposal"))
    count = 4 if custom_first else 3
    if not isinstance(inputs, list) or len(inputs) != count:
        raise ValueError("missing original custom key input")
    edges = [[(event["code"], event["value"]) for event in frame["events"] if event["kind"] == 1] for frame in inputs]
    first_edges = [(31, 1), (31, 0), (42, 1), (202, 1), (42, 0)] if event_local else [(31, 1), (31, 0), (202, 1)] if native_first else [(202, 1)]
    expected_edges = [first_edges, [(203, 1), (203, 0), (31, 1), (31, 0)], [(203, 1), (203, 0)], [(202, 0)]] if custom_first else [first_edges, [(31, 1), (31, 0)], [(202, 0)]]
    if edges != expected_edges or runtime["ordinary"] != [list(edge) for edge in ordinary]:
        raise ValueError("invalid original custom key consumption")
    if any(any(type(event.get(field)) is not int for field in ("kind", "code", "value"))
           for frame in inputs for event in frame["events"]):
        raise ValueError("invalid original custom key event fields")
    if any(not frame["events"] or frame["events"][-1] != dict(kind=0, code=0, value=0) or any(event["kind"] != 1 for event in frame["events"][:-1]) for frame in inputs):
        raise ValueError("invalid original custom key report boundaries")
    if [frame["sequence"] for frame in inputs] != list(range(1, count + 1)) or any(frame["source"] != inputs[0]["source"] for frame in inputs):
        raise ValueError("invalid custom key original source or order")
    original = ticket.get("input", {})
    changes = [dict(kind="key", code=202, state="press")]
    if event_local:
        changes = [dict(kind="key", code=42, state="press")] + changes + [dict(kind="key", code=42, state="release")]
    if (type(ticket.get("id")) is not int or ticket["id"] <= 0 or ticket.get("cause") != "frame"
            or ticket.get("rule") != "rendered-custom-key" or ticket.get("recognizer") != "rendered_key"
            or original.get("source") != inputs[0]["source"] or original.get("sequence") != 1
            or original.get("at") != inputs[0]["at"]
            or original.get("changes") != changes):
        raise ValueError("custom key ticket was not the original callback input")
    if (proposal.get("ticket") != ticket["id"] or proposal.get("fence") != ticket.get("fence")
            or proposal.get("phase") != "discrete" or proposal.get("data") != dict(trigger=202)):
        raise ValueError("missing actual custom key callback proposal")
    first, later, released = (stamp(inputs[index]["at"]) for index in (0, 1, -1))
    proposed, admitted = stamp(runtime["proposed_at"]), stamp(runtime["admitted_at"])
    work = stamp(runtime["work_admitted_at"])
    if not first <= proposed < later < admitted <= released or stamp(ticket["requested_at"]) != first or admitted >= stamp(ticket["deadline"]):
        raise ValueError("custom key reply did not cross a real unseen-input boundary")
    if custom_first:
        if not admitted < stamp(inputs[2]["at"]) <= work <= released or work >= stamp(ticket["deadline"]):
            raise ValueError("custom key native suffix was not fresh post-reply input")
    elif work != admitted:
        raise ValueError("custom key action admission did not match its reply")
    return dict(status="passed", scope=flags["scope"], cases=1, actual_lua_callback=True,
                consumed_control_edges=4 if event_local or custom_first else 2, leaked_control_edges=0, preserved_ordinary_edges=len(ordinary),
                event_local_activation_tested=event_local,
                composition=composition,
                source_and_readiness_modeled=True, physical_takeover_tested=False, kernel_input_opened=False,
                unseen_input_gap_ns=later - proposed)


def run_key_output(args, env, output, socket, rows, wait_rendered, capture):
    if not args.key_output_fixture:
        raise RuntimeError("key-output test requires its explicitly built Rust fixture")
    output = Path(output)
    hyprctl = str(Path(args.hyprland).parent / "hyprctl")
    pictures = []
    phases = []
    scoped = os.environ.get("PREK_KEY_WORKER_ROUTING") == "scoped"

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
                    ("custom-lua-unseen-key-tail", "bAAaAaAAAaaaaaaaaaaasaaasasabsa", []),
                    ("custom-lua-event-local-activation", "bAAaAaAAAaaaaaaaaaaasaaasasabsassa", []),
                    ("custom-lua-native-prefix", "bAAaAaAAAaaaaaaaaaaasaaasasabsassa" + "ssa", []),
                    ("custom-lua-native-suffix", "bAAaAaAAAaaaaaaaaaaasaaasasabsassa" + "ssa" + "sa", []),
                    ("journal-pending-withheld", "bAAaAaAAAaaaaaaaaaaasaaasasabsassa" + "ssa" + "sa", []),
                    ("journal-decided-consumed", "bAAaAaAAAaaaaaaaaaaasaaasasabsassa" + "ssa" + "sa", []),
                    ("journal-fallback-cadence", "bAAaAaAAAaaaaaaaaaaasaaasasabsassa" + "ssa" + "sa" + "a", []),
                    ("hybrid-pointer-baseline", "bAAaAaAAAaaaaaaaaaaasaaasasabsassa" + "ssa" + "sa" + "a", []),
                    ("hybrid-key-relative-consumed", "bAAaAaAAAaaaaaaaaaaasaaasasabsassa" + "ssa" + "sa" + "aas", []),
                    ("hybrid-key-relative-fallback", "bAAaAaAAAaaaaaaaaaaasaaasasabsassa" + "ssa" + "sa" + "aas", []),
                    ("software-scoped-shutdown", "bAAaAaAAAaaaaaaaaaaasaaasasabsassa" + "ssa" + "sa" + "aas", [])]
        if scoped:
            expected = [(name, scoped_order(text), keys) for name, text, keys in expected]
    fixture_env = dict(env, PREK_LOCAL_CAGE="1", PREK_CAGE_OUTPUT=str(output), PREK_CAGE_KEYBOARD=args.keyboard,
                       PREK_CAGE_KEY_SOCKET=str(socket))
    if args.key_worker:
        if not args.prek_app:
            raise RuntimeError("custom Lua keyboard test requires its explicit helper binary")
        fixture_env["PREK_TEST_LUA_BINARY"] = str(args.prek_app)
    devices = json.loads(command(["-j", "devices"]))
    before = {device["address"] for device in devices["keyboards"]}
    pointers_before = {device["address"] for device in devices["mice"]}
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
    if {device["address"] for device in json.loads(command(["-j", "devices"]))["mice"]} != pointers_before:
        raise RuntimeError("owned pointer producer did not retire")
    result = json.loads((output / "key-output-runtime-result.json").read_text())
    wanted = dict(status="passed", phases=15, source="two_owned_wayland_virtual_keyboards", production_route_tested=False,
                  kernel_input_tested=False, unmodified_stock_acceptance=False, test_backpressure=True)
    if args.key_worker:
        wanted.update(phases=42, routing="scoped" if scoped else "key",
                      source="fifteen_owned_wayland_virtual_keyboards_and_two_pointers", scoped_worker_tested=True, authority_modeled=True,
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
                      root_custom_key_arbitration_rendering_tested=True,
                      event_local_custom_activation_rendering_tested=True,
                      composed_custom_key_arbitration_rendering_tested=True,
                      hybrid_key_relative_rendering_tested=True, hybrid_key_fallback_cadence_tested=True,
                      independent_domain_rendering_tested=True, domain_gesture_arbitration_tested=False,
                      native_hybrid_topology_tested=False, hybrid_mt_rendering_tested=False,
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
        custom_arbitration = validate_custom_key_arbitration(phases, rows(), json.loads((output / "custom-key-runtime.json").read_text()), scoped=scoped)
        (output / "custom-key-arbitration-receipt.json").write_text(json.dumps(custom_arbitration, indent=2) + "\n")
        activation = validate_custom_key_arbitration(phases, rows(), json.loads((output / "custom-key-activation-runtime.json").read_text()), event_local=True, scoped=scoped)
        (output / "custom-key-activation-receipt.json").write_text(json.dumps(activation, indent=2) + "\n")
        for composition in ("native_prefix", "native_suffix"):
            suffix = "native-prefix" if composition == "native_prefix" else "native-suffix"
            receipt = validate_custom_key_arbitration(phases, rows(), json.loads((output / f"custom-key-{suffix}-runtime.json").read_text()), composition=composition, scoped=scoped)
            (output / f"custom-key-{suffix}-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
        trace = lambda device: [json.loads(line) for line in (output / f"key-output-{device}.jsonl").read_text().splitlines()]
        hybrid = validate_hybrid_keys(phases, rows(), json.loads((output / "hybrid-key-runtime.json").read_text()), trace(962), trace(963))
        (output / "hybrid-key-receipt.json").write_text(json.dumps(hybrid, indent=2) + "\n")
        domains = validate_domain_journal(phases, rows(), json.loads((output / "domain-journal-runtime.json").read_text()), trace(964))
        (output / "domain-journal-receipt.json").write_text(json.dumps(domains, indent=2) + "\n")
    else:
        wanted.update(software_owner_tested=True, software_authorization_tested=False, software_repeat_rendering_tested=False)
    if json.dumps(result, sort_keys=True) != json.dumps(wanted, sort_keys=True):
        raise RuntimeError("invalid key-output runtime receipt")
    result.update(captures=pictures, observations=phases, config_provider=args.hyprland_config,
                  dependency_override=env.get("PREK_CAGE_AQUAMARINE"))
    name = "key-worker" if args.key_worker else "key-output"
    (output / f"{name}-result.json").write_text(json.dumps(result, indent=2) + "\n")
    if args.key_worker:
        print("PASS: native/custom keys, coherent hybrid reports and configured Lua relative consumption/cancellation; forty-two rendered protocol-sink phases. Authority/readiness modeled; native hybrid topology, MT rendering, general non-key/mixed arbitration, physical takeover and software authorization remain untested.")
    else:
        print("PASS: actual key-output driver preserved overlapping owners, software producer isolation, exact cancellation and context reentry; fifteen rendered app phases. Software authorization and repeat rendering are not tested.")
    return result
