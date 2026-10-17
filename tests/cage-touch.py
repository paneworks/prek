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
    parser.add_argument("--fixture", required=True)
    parser.add_argument("--app", required=True)
    parser.add_argument("--screencopy", required=True)
    parser.add_argument("--hardware-rendering", action="store_true",
                        help="use the selected GPU driver instead of forcing Mesa software rendering")
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
        "NIRI_SOCKET", "SWAYSOCK", "I3SOCK", "XDG_SESSION_ID", "XDG_SEAT", "XDG_VTNR")}
    env.update(XDG_RUNTIME_DIR=str(runtime), HOME=str(home),
               XDG_CONFIG_HOME=str(home / ".config"), XDG_DATA_HOME=str(home / ".local/share"),
               XDG_STATE_HOME=str(home / ".local/state"), XDG_CACHE_HOME=str(home / ".cache"),
               DBUS_SYSTEM_BUS_ADDRESS=f"unix:path={runtime}/no-system-bus",
               DBUS_SESSION_BUS_ADDRESS=os.environ.get("PREK_TEST_SESSION_BUS", f"unix:path={runtime}/no-session-bus"),
               WLR_BACKENDS="headless", WLR_HEADLESS_OUTPUTS="1", WLR_RENDERER="gles2", WLR_LIBINPUT_NO_DEVICES="1",
               LIBSEAT_BACKEND="seatd", SEATD_SOCK=str(runtime / "no-seatd"),
               WLR_RENDER_DRM_DEVICE=os.environ.get("RENDER_NODE", "/dev/dri/renderD128"),
               LIBGL_ALWAYS_SOFTWARE="0" if args.hardware_rendering else "1", PREK_CAGE_HOST="1", PREK_TEST_TOUCH_FD=str(child.fileno()))
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
                receipt = [row for row in rows()[start:] if row["event"] == "touch_frame"][-1]
                wait_for(lambda:any(row["event"] == "rendered" and
                                    row.get("drawn_sequence", 0) >= receipt["sequence"] for row in rows()))
                return receipt

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
            sockets = [path for path in runtime.glob("wayland-*") if path.is_socket()]
            assert len(sockets) == 1
            routed_start = len(rows())
            fixture_env = dict(env, PREK_LOCAL_CAGE_TOUCH="1", PREK_CAGE_OUTPUT=str(output),
                               PREK_LOCAL_CAGE_APP=args.app, PREK_CAGE_TOUCH_CONTROL_FD=str(parent.fileno()),
                               PREK_CAGE_TOUCH_PID=str(cage.pid), PREK_CAGE_TOUCH_SOCKET=str(sockets[0]),
                               PREK_CAGE_TOUCH_GRIM=args.screencopy)
            fixture_env.pop("PREK_TEST_TOUCH_FD", None)
            subprocess.run([args.fixture, "--exact", "local_cage_lua_touch_routing", "--ignored",
                            "--nocapture", "--test-threads=1"], env=fixture_env,
                           pass_fds=(parent.fileno(),), stdout=log, stderr=log, check=True, timeout=45)
            routed = [row for row in rows()[routed_start:] if row["event"].startswith("touch_")]
            assert [(row["event"], row.get("id")) for row in routed] == [
                ("touch_down",0),("touch_frame",None),("touch_up",0),("touch_frame",None),
                ("touch_down",0),("touch_down",1),("touch_frame",None),
                ("touch_motion",0),("touch_motion",1),("touch_frame",None),
                ("touch_motion",0),("touch_motion",1),("touch_frame",None),
                ("touch_up",0),("touch_up",1),("touch_frame",None)]
            trace = json.loads((output / "touch-routing-receipts.json").read_text())
            assert trace["status"] == "passed" and trace["consumed_pan_forwarded"] == 0
            interval = trace["tap_interval"]["secs"] * 1000 + trace["tap_interval"]["nanos"] / 1_000_000
            assert abs((routed[2]["time_ms"] - routed[0]["time_ms"]) - interval) < 1.1
            assert routed[2]["at_ns"] - routed[0]["at_ns"] >= max(0, interval - 8) * 1_000_000
            restored_frames = [row for row in routed if row["event"] == "touch_frame"][2:]
            for index, y in enumerate((0.25, 0.5, 0.7)):
                for contact, x in zip(restored_frames[index]["contacts"], (0.25, 0.75)):
                    assert abs(contact["x"] - restored_frames[index]["width"] * x) < 1
                    assert abs(contact["y"] - restored_frames[index]["height"] * y) < 1
            assert not restored_frames[-1]["contacts"]
            assert any(row["event"] == "rendered" and row.get("drawn_sequence", 0) >= routed[-1]["sequence"] for row in rows())
            for name in ("restored-down", "restored-motion", "neutral"):
                assert (output / f"touch-{name}.png").stat().st_size > 0
            (output / "touch-routing-app-receipts.json").write_text(json.dumps({
                "status":"passed", "events":routed, "rendered_through":routed[-1]["sequence"],
                "tap_interval_ms":interval, "source_and_readiness":"model", "kernel_input_opened":False}))
            print("PASS: Lua pan consumption, cadence-preserving unmatched tap, acknowledged rule disable and restored two-contact rendering crossed the real routing/helper/Cage path.")
            # Malformed operations terminate this owned fixture without adding
            # contacts or acknowledging unrecognized input.
            parent.sendall(struct.pack("=IIIidd",1,99,0xffffffff,0,0.0,0.0))
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
