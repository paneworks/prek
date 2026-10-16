"""Native Morf provider against an owned compositor; no PAM/logind proof."""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import time


def verify_lock_acknowledgement(log, outputs):
    if not outputs or any(item.get("disabled", False) or item.get("dpmsStatus") is not True
                          or item.get("width", 0) <= 0 or item.get("height", 0) <= 0
                          for item in outputs):
        raise ValueError("lock acknowledgement requires active owned outputs")
    if "Session got locked by " not in log or "Sending locked after a " in log:
        raise ValueError("compositor did not acknowledge the owned lock without fallback")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("morf", "socket", "hyprctl", "instance", "output"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--pid", required=True, type=int)
    args = parser.parse_args()
    assert os.environ.get("PREK_CAGE_HOST") == "1"
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    wayland = Path(args.socket)
    output = Path(args.output)
    assert runtime.name == "r" and runtime.parent.name.startswith("pc-") and runtime.parent.parent == Path("/tmp")
    assert wayland.parent == runtime and wayland.is_socket()
    with Path(args.morf).open("rb") as executable:
        assert executable.read(4) == b"\x7fELF", "pass the actual Morf ELF"
    source = output / "morf-lock.lua"
    source.write_text('''local ui=require("morf.ui")
morf.surface.session_lock=true
local ticks=0
-- Explicit fixture authorization, not authentication: hold a real compositor
-- lock long enough to sample it, then exercise its actual unlock/flush path.
morf.timer(100,function()
  if morf.session_lock_state()=="locked" then
    ticks=ticks+1
    if ticks>=20 then morf.surface.session_lock=false end
  end
end)
ui.Rect{color="#203040",width=morf.surface.width,height=morf.surface.height}
''')
    env = dict(os.environ, WAYLAND_DISPLAY=wayland.name,
               HYPRLAND_INSTANCE_SIGNATURE=args.instance,
               MORF_BACKEND="wayland", LIBGL_ALWAYS_SOFTWARE="1")
    def locked():
        reply = subprocess.run([args.hyprctl, "-j", "locked"], env=env,
                               capture_output=True, text=True, check=True, timeout=2)
        state = json.loads(reply.stdout)["locked"]
        assert type(state) is bool
        return state

    assert not locked()
    reports = []
    compositor_logs = []
    locked_outputs = None
    client = None
    endpoint = None
    with (output / "morf-lock.log").open("w") as log:
        child = subprocess.Popen([args.morf, "--clean", str(source)], env=env,
                                 stdout=log, stderr=log, start_new_session=True)
        try:
            until = time.monotonic() + 20
            producer = None
            while time.monotonic() < until:
                if child.poll() is not None:
                    raise RuntimeError(f"Morf exited before native completion; see {output / 'morf-lock.log'}")
                if client is None:
                    endpoints = list((runtime / "morf").glob("*-lock-provider.sock"))
                    if not endpoints:
                        time.sleep(.01)
                        continue
                    assert len(endpoints) == 1
                    endpoint = endpoints[0]
                    assert endpoint.stat().st_uid == os.geteuid() and endpoint.stat().st_mode & 0o077 == 0
                    client = socket.socket(socket.AF_UNIX)
                    client.settimeout(1)
                    client.connect(str(endpoint))
                    producer, owner, _ = struct.unpack("3i", client.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
                    assert owner == os.geteuid()
                    expected = os.stat(args.morf)
                    actual = os.stat(f"/proc/{producer}/exe")
                    assert (actual.st_dev, actual.st_ino) == (expected.st_dev, expected.st_ino)
                client.sendall(b'{"version":1,"op":"state"}\n')
                raw = b""
                while not raw.endswith(b"\n"):
                    part = client.recv(2048)
                    assert part and len(raw) + len(part) <= 2048
                    raw += part
                state = json.loads(raw)
                assert set(state) == {"version", "role", "epoch", "revision", "phase",
                                      "compositor_pid", "compositor_start", "compositor_uid"}
                assert state["version"] == 1 and state["role"] == "lock"
                assert state["compositor_pid"] == int(args.pid) and state["compositor_uid"] == os.geteuid()
                birth = Path(f"/proc/{args.pid}/stat").read_text().rsplit(")", 1)[1].split()[19]
                assert state["compositor_start"] == int(birth)
                assert len(state["epoch"]) == 32 and all(c in "0123456789abcdef" for c in state["epoch"])
                assert state["revision"] == {"pending": 1, "locked": 2, "unlock_dispatched": 3}[state["phase"]]
                if reports:
                    assert state["epoch"] == reports[0]["state"]["epoch"]
                    assert state["revision"] >= reports[-1]["state"]["revision"]
                queried = time.monotonic()
                actual_lock = locked()
                if state["phase"] == "locked" and actual_lock and locked_outputs is None:
                    reply = subprocess.run([args.hyprctl, "-j", "monitors"], env=env,
                                           capture_output=True, text=True, check=True, timeout=2)
                    locked_outputs = json.loads(reply.stdout)
                if (not reports or state["phase"] != reports[-1]["state"]["phase"]
                        or actual_lock != reports[-1]["compositor_locked"]):
                    reply = subprocess.run([args.hyprctl, "rollinglog"], env=env,
                                           capture_output=True, text=True, check=True, timeout=2)
                    compositor_logs.append(reply.stdout)
                reports.append({"state": state, "compositor_locked": actual_lock, "producer_pid": producer,
                                "sampled_at": time.monotonic(), "query_ms": (time.monotonic() - queried) * 1000})
                if state["phase"] == "unlock_dispatched" and not actual_lock:
                    assert any(row["state"]["phase"] == "locked" and row["compositor_locked"] for row in reports)
                    # Own-user readers can inspect evidence, but cannot release it.
                    client.sendall((json.dumps({"version": 1, "op": "release", "epoch": state["epoch"],
                                               "revision": 3}) + "\n").encode())
                    assert client.recv(1) == b""
                    assert endpoint.exists(), "unprivileged release retired the provider"
                    child.wait(timeout=4)
                    assert child.returncode == 0 and not endpoint.exists()
                    break
                time.sleep(.02)
            else:
                raise RuntimeError(f"native Morf lock receipt timed out; see {output / 'morf-lock.log'}")
            compositor_log = "\n".join(compositor_logs)
            (output / "morf-lock-compositor.log").write_text(compositor_log)
            verify_lock_acknowledgement(compositor_log, locked_outputs)
            (output / "morf-lock-receipts.json").write_text(json.dumps({
                "status": "passed", "native_producer": True, "authentication_fixture": True,
                "logind_registered": False, "root_release_tested": False,
                "unprivileged_release_refused": True, "reports": reports,
                "compositor_timeout_acknowledgement": False, "locked_outputs": locked_outputs,
            }) + "\n")
            print("PASS: native Morf provider reported actual owned compositor lock/unlock, retained its identity, refused unprivileged release and removed its endpoint.")
        finally:
            if not (output / "morf-lock-receipts.json").exists():
                (output / "morf-lock-failure.json").write_text(json.dumps({
                    "status": "failed", "process_result": child.poll(),
                    "endpoint_present": endpoint is not None and endpoint.exists(),
                    "reports": reports,
                }) + "\n")
            if client is not None:
                client.close()
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=3)


if __name__ == "__main__":
    main()
