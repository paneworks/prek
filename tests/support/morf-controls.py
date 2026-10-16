"""Ordinary Morf controller IPC on the owned Cage compositor; no broker grant."""
import argparse
import json
import math
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import time


def native_root_receipt(stdout, *, compositor_pid, producer_pid, output, cancel_supported=False):
    """Require one complete receipt bound to the session we actually launched."""
    prefix = "PREK_NATIVE_ROOT_RECEIPT="
    if type(cancel_supported) is not bool:
        raise ValueError("native cancellation capability must be boolean")
    receipts = [json.loads(line[len(prefix):]) for line in stdout.splitlines()
                if line.startswith(prefix)]
    if len(receipts) != 1 or not isinstance(receipts[0], dict):
        raise ValueError("expected exactly one native root receipt")
    receipt = receipts[0]
    expected = dict(status="passed", uid=0, native_ui_handoff_tested=True,
                    actual_logind_registration=True, compositor_pid=compositor_pid,
                    native_action_receipts_tested=True, native_action_rendering_tested=False,
                    native_scope_revocation_tested=True, native_gesture_cancel_supported=cancel_supported,
                    native_gesture_cancel_tested=cancel_supported,
                    producer_pid=producer_pid, output=output, worker_reaped=True,
                    input_devices_opened=0, physical_takeover_tested=False,
                    broker_grant_tested=False, restored=True)
    for key, value in expected.items():
        if type(receipt.get(key)) is not type(value) or receipt[key] != value:
            raise ValueError(f"native root receipt mismatch: {key}")
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("morf", "shell", "socket", "hyprctl", "instance", "output"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--pid", required=True, type=int)
    parser.add_argument("--root-probe", type=Path)
    args = parser.parse_args()
    assert os.environ.get("PREK_CAGE_HOST") == "1"
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    wayland = Path(args.socket)
    assert runtime.name == "r" and runtime.parent.name.startswith("pc-") and runtime.parent.parent == Path("/tmp")
    assert wayland.parent == runtime and wayland.is_socket()
    with Path(args.morf).open("rb") as executable:
        assert executable.read(4) == b"\x7fELF"
    shell = Path(args.shell).resolve(strict=True)
    assert shell.name == "init.lua" and (shell.parent / "prek_controls.lua").is_file()
    library = shell.parents[4] / "library"
    assert (library / "lib/util/settings.lua").is_file(), "pass the upstream Caelestia source tree"
    endpoint = runtime / "morf" / f"{wayland.name}.sock"
    assert not endpoint.exists()
    output = Path(args.output)
    env = dict(os.environ, WAYLAND_DISPLAY=wayland.name,
               HYPRLAND_INSTANCE_SIGNATURE=args.instance, MORF_BACKEND="wayland",
               CAELESTIA_DRY_RUN="1", CAELESTIA_STYLE="material", CAELESTIA_WALLPAPER="",
               MORF_RUNTIME_PATH=str(library),
               LIBGL_ALWAYS_SOFTWARE="1")

    def stock(command):
        reply = subprocess.run([args.hyprctl, "-j", command], env=env,
                               capture_output=True, text=True, check=True, timeout=3)
        return json.loads(reply.stdout)

    monitors = stock("monitors")
    assert len(monitors) == 1
    name = monitors[0]["name"]
    observations = []
    rpc_timings = []
    rpc_calls = 0
    timing_limit = 256
    producer = None

    def exchange(target, *values):
        nonlocal producer
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(3)
            client.connect(str(endpoint))
            pid, uid, _ = struct.unpack("3i", client.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            assert uid == os.geteuid() and (producer is None or producer == pid)
            actual, expected = os.stat(f"/proc/{pid}/exe"), os.stat(args.morf)
            assert (actual.st_dev, actual.st_ino) == (expected.st_dev, expected.st_ino)
            producer = pid
            message = {"op": "kill"} if target is None else {"op": "call", "target": target, "args": values}
            client.sendall((json.dumps(message) + "\n").encode())
            raw = b""
            while not raw.endswith(b"\n"):
                part = client.recv(4096)
                assert part and len(raw) + len(part) <= 16384
                raw += part
        reply = json.loads(raw)
        assert reply["ok"], reply
        results = reply["result"]
        return json.loads(results[0]) if results and isinstance(results[0], str) else (results[0] if results else None)

    def rpc(target=None, *values):
        nonlocal rpc_calls
        started = time.monotonic_ns()
        outcome = "error"
        try:
            result = exchange(target, *values)
            outcome = "reply"
            return result
        finally:
            rpc_calls += 1
            if len(rpc_timings) < timing_limit:
                rpc_timings.append(dict(target=target or "shutdown", outcome=outcome,
                                        elapsed_ns=time.monotonic_ns() - started))

    serial = 0

    def request(action, phase="discrete"):
        nonlocal serial
        serial += 1
        return dict(version=1, output=name, epoch="a" * 32, gesture=f"{serial:032x}",
                    ordinal=1, action=action, phase=phase, dx=0, dy=0, vx=0, vy=0)

    def send(request, error=None):
        reply = rpc("prek-control", json.dumps(request))
        observations.append(dict(request=dict(request), reply=reply))
        assert reply["version"] == 1 and reply["output"] == name
        for field in ("epoch", "gesture", "ordinal"):
            assert reply[field] == request[field]
        assert reply["status"] == ("rejected" if error else "accepted"), reply
        assert reply.get("error") == error, reply

    def state():
        value = rpc("prek-status", name)
        assert value["version"] == 1 and value["output"] == name
        assert value["recognizer_ownership"] is False and value["blocked"] is False
        return value

    def native_state():
        endpoints = list((runtime / "morf").glob("*-shell-provider.sock"))
        assert len(endpoints) == 1, "expected one actual native shell connection"
        path = endpoints[0]
        metadata = path.stat()
        assert metadata.st_uid == os.geteuid() and metadata.st_mode & 0o077 == 0
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(1)
            client.connect(str(path))
            pid, uid, _ = struct.unpack("3i", client.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            assert pid == producer and uid == os.geteuid()
            client.sendall(b'{"version":1,"op":"state"}\n')
            raw = b""
            while not raw.endswith(b"\n"):
                part = client.recv(2048)
                assert part and len(raw) + len(part) <= 2048
                raw += part
        value = json.loads(raw)
        assert set(value) == {"version", "role", "epoch", "revision", "phase", "output",
                              "compositor_pid", "compositor_start", "compositor_uid"}
        assert value["version"] == 1 and value["role"] == "shell"
        assert value["output"] == name and value["phase"] == "configured" and value["revision"] == 2
        assert value["compositor_pid"] == args.pid and value["compositor_uid"] == os.geteuid()
        birth = Path(f"/proc/{args.pid}/stat").read_text().rsplit(")", 1)[1].split()[19]
        assert value["compositor_start"] == int(birth)
        assert len(value["epoch"]) == 32 and all(c in "0123456789abcdef" for c in value["epoch"])
        return path, value

    with (output / "morf-controls.log").open("w") as log:
        child = subprocess.Popen([args.morf, "--no-plugin", str(shell)], env=env,
                                 stdout=log, stderr=log, start_new_session=True)
        try:
            until = time.monotonic() + 45
            while time.monotonic() < until:
                assert child.poll() is None, f"Morf exited: see {output / 'morf-controls.log'}"
                if endpoint.is_socket():
                    try:
                        initial = state()
                        break
                    except (ConnectionError, TimeoutError, AssertionError):
                        pass
                time.sleep(.1)
            else:
                raise RuntimeError("Morf controller startup timed out")
            layers = stock("layers")
            assert "caelestia-drawers" in json.dumps(layers)
            native_path, initial_native = native_state()
            def native_rpc(message, maximum=4096):
                with socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(1)
                    client.connect(str(native_path))
                    client.sendall((json.dumps(message) + "\n").encode())
                    raw = b""
                    while not raw.endswith(b"\n"):
                        part = client.recv(4096)
                        if not part:
                            assert not raw, "partial native response"
                            return None
                        assert len(raw) + len(part) <= maximum
                        raw += part
                    return json.loads(raw)

            capabilities = native_rpc(dict(version=1, op="control-capabilities"))
            assert capabilities["epoch"] == initial_native["epoch"]
            assert capabilities["root_only"] is True
            assert capabilities["recognizer_ownership"] is False
            assert capabilities["ui_generation"] == "1"
            assert (capabilities["queue"], capabilities["receipts"], capabilities["lease_ms"],
                    capabilities["grant_ms"]) == (8, 64, 1000, 250)
            rendered_observations = []
            layout_observations = []
            def rendered_frame(keyboard=None, layout=None):
                if not capabilities.get("rendered_context", False):
                    return None
                until = time.monotonic() + 5
                while True:
                    before_layers = stock("layers") if capabilities.get("layer_space", False) else None
                    value = native_rpc(dict(version=1, op="rendered-context"), 128 * 1024)
                    after_layers = stock("layers") if before_layers is not None else None
                    with (output / "morf-rendered-observations.jsonl").open("a") as log:
                        log.write(json.dumps(dict(expected_keyboard=keyboard, expected_layout=layout, report=value)) + "\n")
                    assert set(value) == {"version", "epoch", "ui_generation", "revision", "sampled_ns", "coverage", "frame"}
                    assert value["version"] == 1 and value["epoch"] == initial_native["epoch"]
                    assert value["ui_generation"] == capabilities["ui_generation"]
                    assert value["coverage"] == "primary_surface_named_bounds"
                    frame = value["frame"]
                    if frame is not None:
                        assert 0 <= time.monotonic_ns() - int(value["sampled_ns"]) < 250_000_000
                        assert int(value["revision"]) > 0
                        assert frame["width"] > 0 and frame["height"] > 0 and frame["scale_120"] > 0
                        assert len(frame["regions"]) <= 256
                        ids = set()
                        nodes = set()
                        semantics = []
                        for region in frame["regions"]:
                            assert isinstance(region["node"], str) and str(int(region["node"])) == region["node"]
                            assert 0 < int(region["node"]) <= 2**64 - 1 and region["node"] not in nodes
                            nodes.add(region["node"])
                            ids.add(region["id"])
                            if "semantic" in region:
                                semantic = region["semantic"]
                                assert set(semantic) <= {"role", "layout"}
                                assert semantic["role"] in {"keyboard", "secure_overlay", "clock_background", "content_bottom", "physical_bottom", "top_left", "top_right", "side_left", "side_right", "local_control"}
                                assert ("layout" in semantic) == (semantic["role"] == "keyboard")
                                semantics.append((region, semantic))
                            bounds = region["bounds"]
                            assert all(math.isfinite(number) for number in bounds.values())
                            assert 0 <= bounds["x"] < bounds["x"] + bounds["width"] <= frame["width"] + 1e-6
                            assert 0 <= bounds["y"] < bounds["y"] + bounds["height"] <= frame["height"] + 1e-6
                        keyboard_regions = [s for r, s in semantics if s["role"] == "keyboard"]
                        if capabilities.get("semantic_regions", False):
                            roles = {s["role"] for r, s in semantics}
                            if not roles >= {"content_bottom", "physical_bottom", "top_left", "top_right", "side_left", "side_right"}:
                                assert time.monotonic() < until, f"native semantic geometry did not converge: {roles}"
                                time.sleep(.025)
                                continue
                            assert all(not r["interactive"] for r, s in semantics if s["role"] in {"content_bottom", "physical_bottom", "top_left", "top_right", "side_left", "side_right"})
                            if layout is not None and (len(keyboard_regions) != 1 or keyboard_regions[0]["layout"] != layout):
                                assert time.monotonic() < until, "native keyboard layout annotation did not converge"
                                time.sleep(.025)
                                continue
                        if keyboard is None or ("drawer-keyboard" in ids) == keyboard:
                            if capabilities.get("semantic_regions", False):
                                assert bool(keyboard_regions) == ("drawer-keyboard" in ids)
                            if capabilities.get("keyboard_registry", False):
                                registry = frame["keyboard"]
                                assert registry["scope"] == "registered_morf_primary"
                                assert registry["registered"] == 1
                                expected = "visible" if "drawer-keyboard" in ids else "hidden"
                                if registry["state"] != expected or (layout is not None and registry.get("layout") != layout):
                                    assert time.monotonic() < until, "registered keyboard state did not converge"
                                    time.sleep(.025)
                                    continue
                                assert set(registry) == {"scope", "registered", "state", "node", "layout"}
                                assert str(int(registry["node"])) == registry["node"] and int(registry["node"]) > 0
                                assert (registry["node"] in nodes) == (expected == "visible")
                            if before_layers is not None:
                                space = frame["surface"]
                                assert set(space) == {"namespace", "width", "height", "local_scale"}
                                scale = space["local_scale"]
                                assert math.isfinite(scale) and 0 < scale <= 64
                                assert abs(frame["width"] * scale - space["width"]) <= scale + 1e-6
                                assert abs(frame["height"] * scale - space["height"]) <= scale + 1e-6
                                def matching_layers(inventory):
                                    return [dict(layer, level=level) for level, items in inventory[name]["levels"].items()
                                            for layer in items if layer["pid"] == producer and layer["namespace"] == space["namespace"]]
                                before = matching_layers(before_layers)
                                after = matching_layers(after_layers)
                                if len(before) != 1 or before != after or (before[0]["w"], before[0]["h"]) != (space["width"], space["height"]):
                                    assert time.monotonic() < until, "owning compositor layer layout did not converge"
                                    time.sleep(.025)
                                    continue
                                layout_observations.append(dict(basis="stock_layer_layout", producer_pid=producer,
                                                                output=name, surface=space, layer=before[0]))
                            rendered_observations.append(value)
                            return value
                    assert time.monotonic() < until, "native submitted-frame geometry did not converge"
                    time.sleep(.025)
            rendered_frame()
            assert os.geteuid() != 0, "this fixture exercises real unprivileged refusal"
            handoff = native_rpc(dict(version=1, op="handoff-state"))
            until = time.monotonic() + 3
            while handoff["handoff"]["observed_ns"] == "0":
                assert time.monotonic() < until, "native UI contact observation did not arrive"
                time.sleep(.02)
                handoff = native_rpc(dict(version=1, op="handoff-state"))
            assert handoff["version"] == 1 and handoff["epoch"] == initial_native["epoch"]
            assert handoff["ui_generation"] == "1"
            h = handoff["handoff"]
            assert h["phase"] == "native" and h["global_enabled"] is True
            assert h["scope"] == "global_phone_touch" and h["recognizer_ownership"] is False
            assert h["ownership_generation"] == "0" and h["contacts"] == 0
            assert h["native_neutral"] is True and int(h["observed_ns"]) <= time.monotonic_ns()
            # Reload the actual owned shell once. Native connection identity
            # stays fixed while the Lua VM generation retires and advances.
            assert rpc("reload") is True
            until = time.monotonic() + 5
            while True:
                reloaded = native_rpc(dict(version=1, op="handoff-state"))
                assert reloaded["epoch"] == handoff["epoch"]
                if reloaded["ui_generation"] == "2":
                    break
                assert reloaded["ui_generation"] == "1" and time.monotonic() < until
                time.sleep(.02)
            handoff = reloaded
            capabilities = native_rpc(dict(version=1, op="control-capabilities"))
            assert capabilities["ui_generation"] == "2"
            rendered_frame()
            assert handoff["handoff"]["phase"] == "native" and handoff["handoff"]["global_enabled"] is True
            for operation in ("lease", "submit", "revoke", "cancel-gesture", "handoff-quiesce", "handoff-activate", "handoff-release"):
                message = dict(version=1, op=operation, epoch=initial_native["epoch"],
                               ui_generation=capabilities["ui_generation"], fence=dict(broker=[1, 2], configuration=1, session=1))
                if operation == "lease":
                    message["expires_ns"] = str(time.monotonic_ns() + 500_000_000)
                elif operation == "submit":
                    message.update(id="1", deadline_ns=str(time.monotonic_ns() + 100_000_000),
                                   request=request("apps"))
                elif operation == "cancel-gesture":
                    message.update(gesture_epoch="a" * 32, gesture="b" * 32)
                elif operation.startswith("handoff-"):
                    message["ownership_generation"] = "1"
                    if operation == "handoff-release":
                        message["descriptors_released"] = True
                assert native_rpc(message) is None, "unprivileged native mutation was accepted"
            assert native_state()[1] == initial_native
            after = native_rpc(dict(version=1, op="handoff-state"))["handoff"]
            assert after["phase"] == "native" and after["global_enabled"] is True
            assert after["ownership_generation"] == "0"
            for action in ("dashboard", "notifications", "quick_settings", "apps", "web"):
                send(request(action))
                value = state()
                assert value["dashboard"] == (action == "dashboard")
                assert value["sidebar"] == (action in ("notifications", "quick_settings"))
                assert value["launcher"] == (action in ("apps", "web"))
                if action in ("apps", "web"):
                    assert value["menu"] == action
            # Apps/Web may focus an input and auto-show the existing keyboard.
            # Establish a hidden baseline through its real controller first.
            send(request("keyboard_hide"))
            assert not state()["keyboard"]
            rendered_frame(False)
            send(request("keyboard_show"))
            assert state()["keyboard"] and state()["keyboard_layout"] == "full"
            rendered_frame(True, "full")
            toggle = request("keyboard_toggle_layout")
            send(toggle)
            assert state()["keyboard_layout"] == "dev"
            rendered_frame(True, "dev")
            send(toggle, "duplicate")
            assert state()["keyboard_layout"] == "dev"
            send(request("keyboard_hide"))
            assert not state()["keyboard"]
            rendered_frame(False)
            pan = request("workspace_preview", "begin")
            send(pan)
            assert state()["workspace_preview"]
            pan.update(phase="update", ordinal=2, dx=-80)
            send(pan)
            pan.update(phase="cancel", ordinal=3)
            send(pan)
            # Acceptance precedes asynchronous rendering/animation completion.
            # Observe convergence without repeating any mutation or extending
            # the receiver's inactivity/hard lifetime bounds.
            settling = []
            until = time.monotonic() + 2
            while True:
                value = state()
                settling.append(value)
                (output / "morf-controls-settling.json").write_text(json.dumps(settling) + "\n")
                assert not value["active"], "cancel left controller progress active"
                if not value["workspace_preview"]:
                    break
                assert time.monotonic() < until, "workspace cancellation did not settle"
                time.sleep(.05)
            final_state = state()
            final_path, final_native = native_state()
            assert final_native == initial_native and final_path == native_path
            root_receipt = None
            if args.root_probe:
                command = ["sudo", "-n", "--", str(args.root_probe), str(runtime), str(os.getuid()),
                           args.instance, os.readlink(f"/proc/{args.pid}/exe"), str(Path(args.morf).resolve()),
                           str(args.pid), str(child.pid), name]
                root_log = output / "morf-native-root.log"
                with root_log.open("w") as log:
                    result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT,
                                            text=True, timeout=30)
                assert result.returncode == 0, "native root probe failed; see morf-native-root.log"
                root_receipt = native_root_receipt(root_log.read_text(), compositor_pid=args.pid,
                                                  producer_pid=child.pid, output=name,
                                                  cancel_supported=capabilities.get("cancel_gesture", False))
                (output / "morf-native-root-receipts.json").write_text(json.dumps(root_receipt) + "\n")
                assert state()["workspace_preview"] is False
            shutdown_acknowledged = True
            try:
                rpc()
            except (ConnectionError, AssertionError):
                shutdown_acknowledged = False
            child.wait(timeout=5)
            assert child.returncode == 0 and not native_path.exists(), "native binding survived orderly shell shutdown"
            (output / "morf-controls-receipts.json").write_text(json.dumps(dict(
                status="passed", producer_pid=producer, output=name, ordinary_shell_ipc=True,
                broker_grant_tested=False, recognizer_handoff_tested=False,
                native_root_operations_refused=True, root_grant_tested=False,
                native_ui_handoff_tested=root_receipt is not None,
                native_root_receipt=root_receipt,
                native_control_capabilities=capabilities,
                native_rendered_context_tested=bool(rendered_observations),
                native_semantic_regions_tested=bool(rendered_observations) and capabilities.get("semantic_regions", False),
                native_keyboard_registry_tested=bool(rendered_observations) and capabilities.get("keyboard_registry", False),
                native_layer_space_tested=bool(layout_observations),
                native_layer_layout_observations=layout_observations,
                native_rendered_context_broker_tested=False,
                native_rendered_observations=rendered_observations,
                native_handoff_state=handoff, native_handoff_operations_refused=True,
                native_reload_generation_verified=True,
                native_shell_binding=final_native, native_endpoint_removed=True,
                shutdown_acknowledged=shutdown_acknowledged,
                actual_compositor_layers=layers, initial=initial, final=final_state,
                settling=settling, observations=observations,
                rpc_calls=rpc_calls, rpc_timing_limit=timing_limit, rpc_timings=rpc_timings)) + "\n")
            print("PASS: actual Morf shell controllers handled frozen-output panels, keyboard, deduplication and workspace progress on the owned compositor.")
        finally:
            process_before_cleanup = child.poll()
            if process_before_cleanup is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=3)
            if not (output / "morf-controls-receipts.json").exists():
                (output / "morf-controls-failure.json").write_text(json.dumps(dict(
                    status="failed", producer_pid=producer, process_result=child.poll(),
                    process_before_cleanup=process_before_cleanup,
                    root_grant_tested=False, recognizer_handoff_tested=False,
                    observations=observations, rpc_calls=rpc_calls,
                    rpc_timing_limit=timing_limit, rpc_timings=rpc_timings)) + "\n")


if __name__ == "__main__":
    main()
