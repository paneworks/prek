"""Actual local Cage touch receipts from a private, test-only wlroots device."""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cage", required=True)
    parser.add_argument("--receiver", required=True)
    parser.add_argument("--output", default="target/cage-touch")
    args = parser.parse_args()
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix="run-", dir=output))
    scratch = tempfile.TemporaryDirectory(prefix="pct-")
    runtime = Path(scratch.name) / "r"
    runtime.mkdir(mode=0o700)
    home = Path(scratch.name) / "home"
    home.mkdir(mode=0o700)
    parent, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    parent.settimeout(3)
    env = {name:value for name,value in os.environ.items() if name not in (
        "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE",
        "NIRI_SOCKET", "SWAYSOCK", "XDG_SESSION_ID", "XDG_SEAT", "XDG_VTNR")}
    env.update(XDG_RUNTIME_DIR=str(runtime), HOME=str(home),
               XDG_CONFIG_HOME=str(home / ".config"), XDG_DATA_HOME=str(home / ".local/share"),
               XDG_STATE_HOME=str(home / ".local/state"), XDG_CACHE_HOME=str(home / ".cache"),
               DBUS_SYSTEM_BUS_ADDRESS=f"unix:path={runtime}/no-system-bus",
               DBUS_SESSION_BUS_ADDRESS=os.environ.get("PREK_TEST_SESSION_BUS", f"unix:path={runtime}/no-session-bus"),
               WLR_BACKENDS="headless", WLR_RENDERER="gles2", WLR_LIBINPUT_NO_DEVICES="1",
               LIBSEAT_BACKEND="seatd", SEATD_SOCK=str(runtime / "no-seatd"),
               WLR_RENDER_DRM_DEVICE=os.environ.get("RENDER_NODE", "/dev/dri/renderD128"),
               LIBGL_ALWAYS_SOFTWARE="1", PREK_CAGE_HOST="1", PREK_TEST_TOUCH_FD=str(child.fileno()))
    receipts = output / "receipts.jsonl"

    def rows():
        if not receipts.exists(): return []
        return [json.loads(line) for line in receipts.read_text().splitlines(keepends=True)
                if line.endswith("\n")]

    with (output / "cage.log").open("w") as log:
        cage = subprocess.Popen([args.cage,"--",args.receiver,str(receipts)], env=env,
                                pass_fds=(child.fileno(),), stdout=log, stderr=log,
                                start_new_session=True)
        child.close()
        try:
            def wait_for(condition):
                until = time.monotonic() + 10
                while not condition():
                    if cage.poll() is not None or time.monotonic() >= until:
                        raise RuntimeError(f"owned touch session failed; logs: {output}")
                    time.sleep(0.01)

            wait_for(lambda:any(row["event"] == "configured" for row in rows()))
            assert any(row["event"] == "seat" and row["capabilities"] & 4 for row in rows())
            # Initial configure precedes mapping; frame completion establishes
            # that the fullscreen target is actually rendered before contacts.
            wait_for(lambda:any(row["event"] == "rendered" for row in rows()))
            start = len(rows())
            timestamp = 100

            def send(operation, identity=0, x=0.0, y=0.0):
                parent.sendall(struct.pack("=IIIidd",1,operation,timestamp,identity,x,y))
                assert parent.recv(4) == struct.pack("=I",operation)

            def frame(count):
                send(4)
                wait_for(lambda:len([row for row in rows()[start:] if row["event"] == "touch_frame"]) == count)
                return [row for row in rows()[start:] if row["event"] == "touch_frame"][-1]

            send(1,7,0.25,0.25)
            send(1,12,0.75,0.25)
            first = frame(1)
            assert [contact["id"] for contact in first["contacts"]] == [7,12]
            for contact, x in zip(first["contacts"], [0.25,0.75]):
                assert abs(contact["x"] - first["width"] * x) < 1
                assert abs(contact["y"] - first["height"] * 0.25) < 1
            timestamp += 10
            send(2,7,0.3,0.5)
            send(2,12,0.7,0.5)
            moved = frame(2)
            for contact, x in zip(moved["contacts"], [0.3,0.7]):
                assert abs(contact["x"] - moved["width"] * x) < 1
                assert abs(contact["y"] - moved["height"] * 0.5) < 1
            timestamp += 10
            send(3,7)
            one = frame(3)
            assert [contact["id"] for contact in one["contacts"]] == [12]
            timestamp += 10
            send(3,12)
            last = frame(4)
            assert not last["contacts"] and not last["keys"] and last["text"] == ""
            events = [row for row in rows()[start:] if row["event"].startswith("touch_")]
            assert [(row["event"],row.get("id")) for row in events] == [
                ("touch_down",7),("touch_down",12),("touch_frame",None),
                ("touch_motion",7),("touch_motion",12),("touch_frame",None),
                ("touch_up",7),("touch_frame",None),("touch_up",12),("touch_frame",None)]
            # Malformed operations terminate this owned fixture without adding
            # contacts or acknowledging unrecognized input.
            parent.sendall(struct.pack("=IIIidd",1,99,timestamp,0,0.0,0.0))
            try: response=parent.recv(4)
            except ConnectionResetError: response=b""
            assert not response
            cage.wait(timeout=5)
            assert not rows()[-1]["contacts"]
            # A prohibited backend is refused before backend autocreation.
            denied_env=dict(env,WLR_BACKENDS="libinput")
            denied=subprocess.run([args.cage,"--",args.receiver,str(output / "denied.jsonl")],env=denied_env,
                                  stdout=log,stderr=log,timeout=3)
            assert denied.returncode!=0 and not (output / "denied.jsonl").exists()
            print(f"PASS: actual Cage wl_touch delivered two contacts, normalized coordinates, motion, four ordered frames and neutral releases. Receipts: {receipts}")
        finally:
            parent.close()
            try: os.killpg(cage.pid,signal.SIGTERM)
            except ProcessLookupError: pass
            try: cage.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(cage.pid,signal.SIGKILL); cage.wait(timeout=3)
            scratch.cleanup()
    print("Owned touch session stopped; no kernel input devices created or opened.")


if __name__ == "__main__":
    main()
