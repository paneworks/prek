"""Evaluate generated units/configurations and execute the installed launcher offline."""
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile


def main():
    assert sys.argv[4:] in ([], ["--system-units"])
    system_units = sys.argv[4:] == ["--system-units"]
    cases = json.loads(Path(sys.argv[1]).read_text())
    binary = Path(sys.argv[2])
    for name, case in cases.items():
        unit = case["unit"]
        args = shlex.split(unit["ExecStart"])
        assert args[:2] == [str(binary), "broker"], (name, args)
        mode = "route" if name == "route" else "observe"
        assert args[args.index("--mode") + 1] == mode
        assert args[args.index("--logind-library") + 1].startswith("/nix/store/")
        root = Path(case["configuration"])
        assert args[args.index("--config") + 1] == str(root / "config.lua")
        result = json.loads(subprocess.check_output(
            [binary, "check", "--config", root / "config.lua", "--json"], timeout=10))
        assert result == json.loads((root / "validation.json").read_text())
        assert result["mode"] == mode
        assert case["restartTriggers"] == [str(root)]
        assert not case["userManagerAgent"]
        assert case["installedPackage"] and case["udevPackage"] and case["linkedData"]
        launcher = Path(case["launcher"]) / "bin/prek-session"
        assert case["installedLauncher"] == (name != "sessionDisabled")
        subprocess.run(["shellcheck", launcher], check=True, timeout=10)
        # No display, broker or input devices: --help proves generated flags are
        # accepted by this exact installed executable without registering.
        env = {key: value for key, value in os.environ.items()
               if key not in ("WAYLAND_DISPLAY", "DISPLAY")}
        help_text = subprocess.check_output([launcher, "--help"], env=env, timeout=10, text=True)
        assert "--rule-mutations" in help_text and "--display" in help_text
        assert "--reconnect" in help_text
        assert ("--reconnect" in launcher.read_text()) == (name != "reconnectDisabled")
        assert ("--commands" in launcher.read_text()) == (name == "route")
        assert ("--keys" in launcher.read_text()) == (name == "route")
        assert ("--logind-library" in launcher.read_text()) == (name != "reconnectDisabled")
        assert unit["KillMode"] == "control-group" and unit["TimeoutStopSec"] == 5
        if name == "route":
            assert unit["DeviceAllow"] == ["char-input rw", "/dev/uinput rw"]
            assert unit["RuntimeDirectory"] == ["prek-route", "prek"]
            assert unit["ReadWritePaths"] == ["/run/prek-route", "/run/prek"]
        else:
            assert unit["DeviceAllow"] == ["char-input r"]
            assert len(unit["ReadWritePaths"]) == len(unit["RuntimeDirectory"]) == 1
        assert unit["NoNewPrivileges"] and unit["ProtectHome"] == "read-only"
        # /proc visibility is needed for verified PID/start/executable matching.
        assert "ProtectProc" not in unit and "PrivateUsers" not in unit
        if name == "minimal":
            assert result["rules"] == 0 and result["selectors"] == 0
        if name == "inline":
            assert result["rules"] == 1 and result["selectors"] == 1
            module = root / "modules/bindings/keyboard.lua"
            assert module.is_file() and not module.is_symlink()
            text = launcher.read_text()
            assert "--input" in text and "--rule-mutations" in text and "--recognizers" in text
            assert "/run/prek-test/control.sock" in text
        if name == "fromFile":
            assert result["rules"] > 0
            assert not (root / "modules/bindings/keyboard.lua").is_symlink()
        for preset in ("minimal", "desktop", "phone"):
            result = json.loads(subprocess.check_output([binary, "check", "--config",
                binary.parent.parent / f"share/prek/presets/{preset}.lua", "--json"], timeout=10))
            assert result["status"] == "valid"
        assert (binary.parent.parent / "share/prek/nixos/module.nix").is_file()
        with tempfile.TemporaryDirectory() as scratch:
            # Parse every directive with isolated dependencies and no generators.
            # User-scope syntax validation works inside the Nix sandbox (systemd
            # also touches /run in system scope). The local --system-units pass
            # additionally verifies the exact system unit in a private root.
            verify_root = Path(scratch)
            units = verify_root / "etc/systemd/system"
            units.mkdir(parents=True)
            (verify_root / "run").mkdir()
            executable = verify_root / binary.relative_to("/")
            executable.parent.mkdir(parents=True)
            shutil.copy2(binary, executable)
            for target in ("basic.target", "sysinit.target", "shutdown.target", "exit.target"):
                (units / target).write_text("[Unit]\nDescription=Offline verification dependency\n")
            path = units / "prek.service"
            lines = ["[Unit]", "Description=Prek generated module check", "[Service]"]
            for key, value in unit.items():
                values = value if isinstance(value, list) else [value]
                for item in values:
                    if isinstance(item, bool):
                        item = "yes" if item else "no"
                    lines.append(f"{key}={item}")
            path.write_text("\n".join(lines) + "\n")
            verify_env = dict(env, XDG_RUNTIME_DIR=str(verify_root / "run"),
                              SYSTEMD_UNIT_PATH="/etc/systemd/system" if system_units else str(units), HOME=scratch,
                              XDG_CONFIG_HOME=str(verify_root / "config"),
                              XDG_DATA_HOME=str(verify_root / "data"))
            scope = [f"--root={scratch}"] if system_units else ["--user"]
            subprocess.run(["systemd-analyze", "--generators=no", *scope,
                            "verify", "prek.service" if system_units else str(path)],
                           env=verify_env, check=True, timeout=20)
    Path(sys.argv[3]).write_text(json.dumps({"status": "passed", "cases": list(cases),
        "negative_evaluations": 8, "devices_opened": False, "services_started": False,
        "unit_scope": "system" if system_units else "user_syntax"}) + "\n")


if __name__ == "__main__":
    main()
