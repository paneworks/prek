"""Run the receiver in private nested Hyprland, following mold's sandbox."""
import argparse
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import time


def usable_outputs(monitors):
    active = [item for item in (monitors or []) if not item.get("disabled", False)]
    return bool(active) and all(item.get("width", 0) > 0 and item.get("height", 0) > 0
                                for item in active)


def aquamarine_libraries(maps, expected=None):
    paths = [fields[5] for line in maps.splitlines()
             if len(fields := line.split(maxsplit=5)) == 6]
    libraries = sorted({path for path in paths if Path(path).name.startswith("libaquamarine.so")})
    if expected and (not libraries or any(Path(path).parent != Path(expected) / "lib"
                                         or path.endswith(" (deleted)") for path in libraries)):
        raise RuntimeError("owned Hyprland did not load the declared test-only Aquamarine")
    return dict(declared_test_override=expected, loaded_libraries=libraries)


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
            proxy = subprocess.Popen([sys.executable, args.proxy, str(upstream), str(parent), "--no-parent-input"],
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
            def instances():
                return [path.parent.name for path in (runtime / "hypr").glob("*/.socket2.sock")]
            wait_for(lambda:len(instances())==1,hyprland)
            control_env=dict(env,HYPRLAND_INSTANCE_SIGNATURE=instances()[0])
            hyprctl=str(Path(args.hyprland).parent / "hyprctl")
            def monitors():
                result=subprocess.run([hyprctl,"-j","monitors"],env=control_env,
                    capture_output=True,text=True,timeout=3)
                if result.returncode:return None
                return json.loads(result.stdout)
            wait_for(lambda:monitors() is not None,hyprland)
            if not monitors():
                backend="wayland" if os.environ.get("PREK_CAGE_VISIBLE")=="1" else "headless"
                command=[hyprctl,"output","create",backend]
                if backend=="headless":command.append("PREK-HEADLESS")
                subprocess.run(command,env=control_env,check=True,stdout=hypr_log,stderr=hypr_log,timeout=3)
            # As in Mold, the nested compositor gets an explicit output rather
            # than relying on Hyprland's unreported fallback monitor (-1).
            wait_for(lambda:usable_outputs(monitors()),hyprland)
            maps = Path(f"/proc/{hyprland.pid}/maps").read_text()
            (output / "hyprland-libraries.txt").write_text(maps)
            libraries = aquamarine_libraries(maps, os.environ.get("PREK_CAGE_AQUAMARINE"))
            report = output / "nested.json"
            temporary = output / "nested.json.new"
            temporary.write_text(json.dumps({"socket":str(nested), "hyprland_pid":hyprland.pid,
                                             "aquamarine":libraries}))
            temporary.replace(report)
            receiver_env = dict(env, WAYLAND_DISPLAY=nested.name)
            receiver = subprocess.Popen([args.receiver, str(output / "receipts.jsonl")],
                                        env=receiver_env, stdout=hypr_log, stderr=hypr_log)
            children.append(receiver)
            while receiver.poll() is None and hyprland.poll() is None:
                time.sleep(0.05)
            (output / "process-exits.json").write_text(json.dumps({"receiver":receiver.poll(),"hyprland":hyprland.poll()}))
        except InterruptedError:
            pass
        finally:
            for path in (runtime / "hypr").glob("*/hyprland.log"):
                shutil.copyfile(path,output / "hyprland-runtime.log")
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
