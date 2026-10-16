"""Fixed isolated kernel test runner. Invoked only by the user through sudo."""
import argparse
import contextlib
import ctypes
import fcntl
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import uuid

PREFIX = "PREK_ROOT_ROUTING_RECEIPT="
EXPECTED = {
    "evdev_boundary::clock_change_with_backlog_reports_loss_before_fresh_input",
    "evdev_boundary::pregrab_backlog_and_postgrab_input_share_the_reader_queue",
    "lifecycle::t57_reader_errors_removal_finish_and_unwind_retire_output_before_releasing_input",
    "t46_isolated_keyboard_receives_passthrough_once_and_drop_removes_nodes",
    "t46_isolated_mouse_preserves_motion_buttons_and_both_wheel_resolutions",
    "t48_isolated_touchpad_preserves_slots_tool_flags_and_button_clicks",
    "t50_configured_swipe_consumes_the_actual_downstream_touch_stream",
    "t50_isolated_claimed_touch_reaches_no_downstream_contact_or_click",
    "t51_configured_unmatched_tap_reaches_the_receiver_once_with_its_cadence",
    "t51_isolated_buffered_tap_preserves_delivery_interval_without_bursting",
    "t55_isolated_keyboard_led_feedback_reaches_source_without_echo",
}


def test_inventory(stdout):
    names = [line.removesuffix(": test") for line in stdout.splitlines() if line.endswith(": test")]
    if len(names) != len(EXPECTED) or set(names) != EXPECTED:
        raise ValueError("isolated test executable does not contain exactly the expected gates")


def test_result(stdout):
    results = re.findall(r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", stdout, re.M)
    if results != [(str(len(EXPECTED)), "0", "0")]:
        raise ValueError("isolated kernel gates did not all execute and pass")


def receipt(stdout):
    values = [json.loads(line[len(PREFIX):]) for line in stdout.splitlines() if line.startswith(PREFIX)]
    if len(values) != 1 or not isinstance(values[0], dict):
        raise ValueError("expected exactly one complete isolated kernel receipt")
    value = values[0]
    expected = dict(version=1, status="passed", uid=0, passed=len(EXPECTED),
                    physical_devices_opened=0, model_desktop_context=True,
                    production_route_admission_tested=False, compositor_consumer_retirement_tested=False,
                    rule_removed=True, registry_clean=True)
    for key, wanted in expected.items():
        if type(value.get(key)) is not type(wanted) or value[key] != wanted:
            raise ValueError(f"invalid isolated kernel receipt: {key}")
    if value.get("tests") != sorted(EXPECTED) or value.get("kernel") != os.uname().release:
        raise ValueError("isolated kernel receipt has no matching inventory/kernel")
    return value


def child_setup(parent):
    """Kill test-owned devices if the supervising root helper itself dies."""
    libc = ctypes.CDLL(None, use_errno=True)
    libc.prctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong]
    libc.prctl.restype = ctypes.c_int
    if libc.prctl(1, signal.SIGKILL, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "cannot establish test parent-death cleanup")
    if os.getppid() != parent:
        os._exit(1)


def same(directory, name, identity):
    try:
        metadata = os.stat(name, dir_fd=directory, follow_symlinks=False)
    except FileNotFoundError:
        return False
    return (metadata.st_dev, metadata.st_ino) == identity


def remove_same(directory, name, identity):
    if not same(directory, name, identity):
        raise RuntimeError(f"refusing to remove replaced root test path: {name}")
    os.unlink(name, dir_fd=directory)


@contextlib.contextmanager
def directories():
    """Open fixed trusted components and remove only directories we created."""
    opened, created = [], []
    try:
        root = os.open("/", os.O_DIRECTORY | os.O_RDONLY | os.O_CLOEXEC)
        opened.append(root)
        cache = {(): root}
        for parts in [("run", "prek", "devices"), ("run", "udev", "rules.d")]:
            for end in range(1, len(parts) + 1):
                key = parts[:end]
                if key in cache:
                    continue
                parent, name = cache[key[:-1]], key[-1]
                made = False
                try:
                    os.mkdir(name, 0o700, dir_fd=parent)
                    made = True
                except FileExistsError:
                    pass
                fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=parent)
                opened.append(fd)
                metadata = os.fstat(fd)
                if metadata.st_uid != 0 or metadata.st_mode & 0o022:
                    raise PermissionError("root test directories must be root-owned without group/other writes")
                cache[key] = fd
                if made:
                    created.append((parent, name, (metadata.st_dev, metadata.st_ino)))
        yield cache[("run", "prek")], cache[("run", "prek", "devices")], cache[("run", "udev", "rules.d")]
    finally:
        for parent, name, identity in reversed(created):
            if same(parent, name, identity):
                try:
                    os.rmdir(name, dir_fd=parent)
                except OSError as error:
                    print(f"Retained nonempty root test directory {name}: {error}", file=sys.stderr)
        for fd in reversed(opened):
            os.close(fd)


def run(args):
    # No filesystem mutation or udev/device access occurs before this check.
    if os.geteuid() != 0:
        raise PermissionError("isolated kernel runner requires user-launched root credentials")
    for path in [args.tests, args.importer, args.udevadm]:
        resolved = Path(path).resolve(strict=True)
        if not str(resolved).startswith("/nix/store/") or not resolved.is_file():
            raise ValueError("isolated root runner accepts immutable packaged executables only")
    env = {"PREK_ISOLATED_ROUTING": "1"}
    listed = subprocess.run([args.tests, "--list", "--ignored"], env=env,
                            capture_output=True, text=True, check=True, timeout=10)
    test_inventory(listed.stdout)
    if not stat.S_ISCHR(os.stat("/dev/uinput").st_mode):
        raise RuntimeError("/dev/uinput is unavailable; the runner does not load kernel modules")
    with directories() as (parent, registry, rules):
        fcntl.flock(parent, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if any(name.startswith("prek-test-") for name in os.listdir(registry)):
            raise RuntimeError("existing isolated device metadata must be inspected before another root run")
        rule = f"70-prek-isolated-{uuid.uuid4().hex}.rules"
        contents = ('ACTION=="add", SUBSYSTEM=="input", KERNEL=="event*", ATTRS{phys}=="prek/test/*", '
                    'ENV{PREK_TEST}="1", ENV{PREK_VIRTUAL}="1", ENV{LIBINPUT_IGNORE_DEVICE}="1", '
                    f'IMPORT{{program}}="{args.importer} udev-import --marker $attr{{phys}}"\n')
        fd = os.open(rule, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                     0o644, dir_fd=rules)
        metadata = os.fstat(fd)
        identity = metadata.st_dev, metadata.st_ino
        child = None
        try:
            with os.fdopen(fd, "w") as file:
                file.write(contents)
                file.flush()
                os.fsync(file.fileno())
            subprocess.run([args.udevadm, "control", "--reload-rules"], check=True, env={}, timeout=10)
            # No trigger: only devices created by these tests receive the rule.
            parent_pid = os.getpid()
            child = subprocess.Popen([args.tests, "--ignored", "--nocapture", "--test-threads=1"],
                                     env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                     start_new_session=True, text=True, preexec_fn=lambda: child_setup(parent_pid))
            try:
                output, _ = child.communicate(timeout=90)
            except BaseException:
                if child.poll() is None:
                    os.killpg(child.pid, signal.SIGKILL)
                output, _ = child.communicate()
                print(output, end="", flush=True)
                raise
            print(output, end="", flush=True)
            if child.returncode != 0:
                raise RuntimeError(f"isolated kernel tests failed with exit {child.returncode}")
            test_result(output)
        finally:
            if child is not None and child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
            remove_same(rules, rule, identity)
            subprocess.run([args.udevadm, "control", "--reload-rules"], check=True, env={}, timeout=10)
        if any(name.startswith("prek-test-") for name in os.listdir(registry)):
            raise RuntimeError("isolated device metadata remains; preserving it for inspection")
    print(PREFIX + json.dumps(dict(version=1, status="passed", uid=0, kernel=os.uname().release,
          tests=sorted(EXPECTED), passed=len(EXPECTED), physical_devices_opened=0,
          model_desktop_context=True, production_route_admission_tested=False,
          compositor_consumer_retirement_tested=False, rule_removed=True, registry_clean=True)), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tests", required=True)
    parser.add_argument("--importer", required=True)
    parser.add_argument("--udevadm", required=True)
    args = parser.parse_args()
    def interrupted(signum, frame):
        raise InterruptedError(f"root test signal {signum}")
    for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(number, interrupted)
    run(args)


if __name__ == "__main__":
    main()
