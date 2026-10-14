"""Run the receiver in private nested Hyprland, following mold's sandbox."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--hyprland", required=True)
    parser.add_argument("--receiver", required=True)
    parser.add_argument("--proxy", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    output = Path(args.output)
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    parent = runtime / os.environ["WAYLAND_DISPLAY"]
    upstream = runtime / "wayland-parent"
    env = dict(os.environ, WAYLAND_DISPLAY=upstream.name,
               LIBSEAT_BACKEND="seatd", SEATD_SOCK=str(runtime / "no-seatd"),
               HL_NO_SYSTEMD="1", AQ_BACKEND="wayland")
    config = output / "hyprland.conf"
    config.write_text("monitor = , 1280x720@60, auto, 1\n"
                      "input {\n kb_layout = us\n}\n"
                      "misc {\n disable_hyprland_logo = true\n disable_splash_rendering = true\n}\n"
                      "debug {\n disable_logs = false\n}\n")
    children = []

    def wait_for(condition, child):
        until = time.monotonic() + 20
        while not condition():
            if child.poll() is not None or time.monotonic() >= until:
                raise RuntimeError(f"nested compositor startup failed; logs: {output}")
            time.sleep(0.02)

    def stopped(signum, frame):
        raise InterruptedError("owned Cage session stopped")

    signal.signal(signal.SIGTERM, stopped)
    with (output / "proxy.log").open("w") as proxy_log, (output / "hyprland.log").open("w") as hypr_log:
        try:
            proxy = subprocess.Popen([sys.executable, args.proxy, str(upstream), str(parent)],
                                     stdout=proxy_log, stderr=proxy_log)
            children.append(proxy)
            wait_for(upstream.is_socket, proxy)
            hyprland = subprocess.Popen([args.hyprland, "--config", str(config)],
                                        env=env, stdout=hypr_log, stderr=hypr_log)
            children.append(hyprland)
            def sockets():
                return [path for path in runtime.glob("wayland-*")
                        if path.is_socket() and path not in (parent, upstream)]
            wait_for(lambda: len(sockets()) == 1, hyprland)
            nested = sockets()[0]
            report = output / "nested.json"
            temporary = output / "nested.json.new"
            temporary.write_text(json.dumps({"socket":str(nested), "hyprland_pid":hyprland.pid}))
            temporary.replace(report)
            receiver_env = dict(env, WAYLAND_DISPLAY=nested.name)
            receiver = subprocess.Popen([args.receiver, str(output / "receipts.jsonl")],
                                        env=receiver_env, stdout=hypr_log, stderr=hypr_log)
            children.append(receiver)
            while receiver.poll() is None and hyprland.poll() is None:
                time.sleep(0.05)
        except InterruptedError:
            pass
        finally:
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            for child in reversed(children):
                if child.poll() is None:
                    child.terminate()
                try:
                    child.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    child.kill(); child.wait(timeout=2)


if __name__ == "__main__":
    main()
