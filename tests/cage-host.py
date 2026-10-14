"""Owned local Cage keyboard smoke test; full evdev/touch coverage is in cage.nix."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cage", required=True)
    parser.add_argument("--receiver", required=True)
    parser.add_argument("--keyboard", required=True)
    parser.add_argument("--output", default="target/cage-host")
    args = parser.parse_args()
    output = Path(args.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix="run-", dir=output))
    runtime = output / "runtime"
    runtime.mkdir(mode=0o700)
    upstream = os.environ.get("WAYLAND_DISPLAY")
    if not upstream:
        parser.error("run from a Wayland session")
    if not os.path.isabs(upstream):
        upstream = str(Path(os.environ["XDG_RUNTIME_DIR"]) / upstream)
    env = dict(os.environ, WAYLAND_DISPLAY=upstream, XDG_RUNTIME_DIR=str(runtime),
               WLR_BACKENDS="wayland", WLR_RENDERER="pixman",
               LIBGL_ALWAYS_SOFTWARE="1", PREK_CAGE_HOST="1")
    receipts = output / "receipts.jsonl"

    def rows():
        if not receipts.exists():
            return []
        # Ignore only an unfinished final write; malformed complete JSON fails.
        return [json.loads(line) for line in receipts.read_text().splitlines(keepends=True)
                if line.endswith("\n")]

    with (output / "cage.log").open("w") as log:
        cage = subprocess.Popen([args.cage, "--", args.receiver, str(receipts)],
                                env=env, stdout=log, stderr=log, start_new_session=True)
        try:
            deadline = time.monotonic() + 15
            while not {"configured", "keyboard_enter"}.issubset(
                    {row["event"] for row in rows()}):
                if cage.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(f"Cage startup failed; see {output / 'cage.log'}")
                time.sleep(0.02)
            sockets = [path for path in runtime.glob("wayland-*") if path.is_socket()]
            if len(sockets) != 1:
                raise RuntimeError("expected one owned Cage socket")
            subprocess.run([args.keyboard, str(sockets[0])], check=True, timeout=5,
                           stdout=log, stderr=log)
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
            print(f"PASS: local Cage delivered B and Shift+A, six ordered key events, "
                  f"text bA and neutral modifier cleanup. Receipts: {receipts}")
        finally:
            try:
                os.killpg(cage.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                cage.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(cage.pid, signal.SIGKILL)
                cage.wait(timeout=3)
            if cage.poll() is None:
                raise RuntimeError("owned Cage process was not reaped")
    print("Owned Cage and receiver stopped; no host evdev devices opened.")


if __name__ == "__main__":
    main()
