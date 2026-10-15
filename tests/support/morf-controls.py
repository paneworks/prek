"""Ordinary Morf controller IPC on the owned Cage compositor; no broker grant."""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("morf", "shell", "socket", "hyprctl", "instance", "output"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--pid", required=True, type=int)
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
            def native_rpc(message):
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
                        assert len(raw) + len(part) <= 4096
                        raw += part
                    return json.loads(raw)

            capabilities = native_rpc(dict(version=1, op="control-capabilities"))
            assert capabilities["epoch"] == initial_native["epoch"]
            assert capabilities["root_only"] is True
            assert capabilities["recognizer_ownership"] is False
            assert capabilities["ui_generation"] == "1"
            assert (capabilities["queue"], capabilities["receipts"], capabilities["lease_ms"],
                    capabilities["grant_ms"]) == (8, 64, 1000, 250)
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
            assert handoff["handoff"]["phase"] == "native" and handoff["handoff"]["global_enabled"] is True
            for operation in ("lease", "submit", "revoke", "handoff-quiesce", "handoff-activate", "handoff-release"):
                message = dict(version=1, op=operation, epoch=initial_native["epoch"],
                               ui_generation=capabilities["ui_generation"], fence=dict(broker=[1, 2], configuration=1, session=1))
                if operation == "lease":
                    message["expires_ns"] = str(time.monotonic_ns() + 500_000_000)
                elif operation == "submit":
                    message.update(id="1", deadline_ns=str(time.monotonic_ns() + 100_000_000),
                                   request=request("apps"))
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
            send(request("keyboard_show"))
            assert state()["keyboard"] and state()["keyboard_layout"] == "full"
            toggle = request("keyboard_toggle_layout")
            send(toggle)
            assert state()["keyboard_layout"] == "dev"
            send(toggle, "duplicate")
            assert state()["keyboard_layout"] == "dev"
            send(request("keyboard_hide"))
            assert not state()["keyboard"]
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
                native_control_capabilities=capabilities,
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
