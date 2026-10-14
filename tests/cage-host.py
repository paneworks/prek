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
        "NIRI_SOCKET", "SWAYSOCK", "XDG_SESSION_ID", "XDG_SEAT", "XDG_VTNR")}
    env.update(XDG_RUNTIME_DIR=str(runtime), HOME=str(home),
               XDG_CONFIG_HOME=str(home / ".config"), XDG_DATA_HOME=str(home / ".local/share"),
               XDG_STATE_HOME=str(home / ".local/state"), XDG_CACHE_HOME=str(home / ".cache"),
               DBUS_SYSTEM_BUS_ADDRESS=f"unix:path={runtime}/no-system-bus",
               DBUS_SESSION_BUS_ADDRESS=os.environ.get("PREK_TEST_SESSION_BUS", f"unix:path={runtime}/no-session-bus"),
               WLR_BACKENDS="wayland" if args.visible else "headless", WLR_RENDERER="gles2",
               WLR_RENDER_DRM_DEVICE=os.environ.get("RENDER_NODE", "/dev/dri/renderD128"),
               WLR_LIBINPUT_NO_DEVICES="1", LIBGL_ALWAYS_SOFTWARE="1", PREK_CAGE_HOST="1")
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
        cage = subprocess.Popen([args.cage, "--", os.sys.executable, args.inner,
                                 "--hyprland", args.hyprland, "--receiver", args.receiver,
                                 "--proxy", args.proxy, "--output", str(output)],
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
            print(f"PASS: local Cage/nested Hyprland delivered B and Shift+A, six ordered key events, "
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
                raise RuntimeError("owned Cage process was not reaped")
            scratch.cleanup()
    print("Owned Cage, nested Hyprland and receiver stopped; no host evdev devices opened.")


if __name__ == "__main__":
    main()
