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
    "evdev_boundary::witness_prefix_retains_concurrent_motion_after_the_grab",
    "evdev_boundary::witness_without_prefix_preserves_a_postgrab_held_key",
    "lifecycle::t57_reader_errors_removal_finish_and_unwind_retire_output_before_releasing_input",
    "lifecycle::t58_reader_cutover_preserves_prefix_cursor_and_downstream_tail",
    "t46_isolated_keyboard_receives_passthrough_once_and_drop_removes_nodes",
    "t46_isolated_mouse_preserves_motion_buttons_and_both_wheel_resolutions",
    "t48_isolated_touchpad_preserves_slots_tool_flags_and_button_clicks",
    "t50_configured_swipe_consumes_the_actual_downstream_touch_stream",
    "t50_isolated_claimed_touch_reaches_no_downstream_contact_or_click",
    "t51_configured_unmatched_tap_reaches_the_receiver_once_with_its_cadence",
    "t51_isolated_buffered_tap_preserves_delivery_interval_without_bursting",
    "t55_isolated_keyboard_led_feedback_reaches_source_without_echo",
}
RECOVERY_EXPECTED = {f"recovery::{source}::{failure}" for source in ("keyboard", "touch")
                     for failure in ("orderly", "worker", "parent", "stall")}
REPEAT_CASE = "kernel::tests::repeat::isolated_uinput_repeat_timer_and_consumer_settings_are_not_transparent"
REPEAT_RESULT = "PREK_UINPUT_REPEAT_RESULT="
FUZZ_CASE = "kernel::tests::fuzz::isolated_uinput_fuzz_refiltering_and_slot_restore_are_not_transparent"
FUZZ_RESULT = "PREK_UINPUT_FUZZ_RESULT="
ALL_EXPECTED = EXPECTED | RECOVERY_EXPECTED | {REPEAT_CASE, FUZZ_CASE}
RECOVERY_INVENTORY = "PREK_KERNEL_RECOVERY_INVENTORY_V1"
RECOVERY_RESULT = "PREK_KERNEL_RECOVERY_RESULT="
EXECUTION_TIMEOUT = 90


def recovery_inventory(stdout):
    lines = stdout.splitlines()
    if len(lines) != 2 or lines[0] != RECOVERY_INVENTORY:
        raise ValueError("missing recovery inventory")
    names = json.loads(lines[1])
    if not isinstance(names, list) or len(names) != 8 or not all(isinstance(name, str) for name in names) or sorted(names) != sorted(RECOVERY_EXPECTED):
        raise ValueError("recovery executable has a different inventory")


def recovery_result(stdout):
    names = [f"recovery::{source}::{failure}" for source, failure in
             re.findall(r"^kernel recovery: (keyboard|touch) / (orderly|worker|parent|stall) \.\.\. ok$", stdout, re.M)]
    values = [json.loads(line[len(RECOVERY_RESULT):]) for line in stdout.splitlines() if line.startswith(RECOVERY_RESULT)]
    if len(names) != 8 or len([line for line in stdout.splitlines() if line.startswith("kernel recovery:")]) != 8 \
            or sorted(names) != sorted(RECOVERY_EXPECTED) or len(values) != 1:
        raise ValueError("not all eight recovery gates executed exactly once")
    value = values[0]
    if not isinstance(value, dict) or set(value) != {"version", "passed", "tests"} or type(value["version"]) is not int or value["version"] != 1 \
            or type(value["passed"]) is not int or value["passed"] != 8 or value["tests"] != names:
        raise ValueError("recovery gate summary differs from its actual cases")


def test_inventory(stdout):
    names = [line.removesuffix(": test") for line in stdout.splitlines() if line.endswith(": test")]
    if len(names) != len(EXPECTED) or set(names) != EXPECTED:
        raise ValueError("isolated test executable does not contain exactly the expected gates")


def test_result(stdout):
    results = re.findall(r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", stdout, re.M)
    if results != [(str(len(EXPECTED)), "0", "0")]:
        raise ValueError("isolated kernel gates did not all execute and pass")


def repeat_inventory(stdout):
    names = [line.removesuffix(": test") for line in stdout.splitlines() if line.endswith(": test")]
    if names != [REPEAT_CASE]:
        raise ValueError("repeat characterization executable has a different inventory")


def repeat_result(stdout):
    results = re.findall(r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", stdout, re.M)
    values = [json.loads(line[len(REPEAT_RESULT):]) for line in stdout.splitlines() if line.startswith(REPEAT_RESULT)]
    if results != [("1", "0", "0")] or len(values) != 1 or not isinstance(values[0], dict):
        raise ValueError("repeat characterization did not execute exactly once")
    value = values[0]
    expected = dict(version=1, initial=[250, 33], disabled_forwarded=1, disabled_observed=1,
                    enabled_forwarded=1, consumer_feedback_codes=[0, 1], keys_neutral=True,
                    physical_devices_opened=0, repeat_passthrough_tested=False)
    if set(value) != set(expected) | {"enabled_observed"}:
        raise ValueError("repeat characterization has a different schema")
    for key, wanted in expected.items():
        actual = value[key]
        if type(actual) is not type(wanted) or actual != wanted \
                or isinstance(wanted, list) and any(type(item) is not int for item in actual):
            raise ValueError(f"invalid repeat characterization: {key}")
    if type(value["enabled_observed"]) is not int or not 1 < value["enabled_observed"] <= 255:
        raise ValueError("repeat characterization did not observe extra kernel repeats")
    return value


def fuzz_inventory(stdout):
    names = [line.removesuffix(": test") for line in stdout.splitlines() if line.endswith(": test")]
    if names != [FUZZ_CASE]:
        raise ValueError("fuzz characterization executable has a different inventory")


def fuzz_result(stdout):
    results = re.findall(r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", stdout, re.M)
    values = [json.loads(line[len(FUZZ_RESULT):]) for line in stdout.splitlines() if line.startswith(FUZZ_RESULT)]
    if results != [("1", "0", "0")] or len(values) != 1 or not isinstance(values[0], dict):
        raise ValueError("fuzz characterization did not execute exactly once")
    value = values[0]
    expected = dict(version=1, fuzz=16, source_raw=[16, 24], source_observed=[8, 16],
                    mirror_raw=8, mirror_observed=2, restore_raw=16, restore_observed=8,
                    gap_raw=[31, 32], gap_observed=[15, 32], zero_fuzz_observed=16,
                    inactive_slot_preserved=True, contacts_neutral=True,
                    physical_devices_opened=0, fuzz_passthrough_tested=False)
    if set(value) != set(expected):
        raise ValueError("fuzz characterization has a different schema")
    for key, wanted in expected.items():
        actual = value[key]
        if type(actual) is not type(wanted) or actual != wanted \
                or isinstance(wanted, list) and any(type(item) is not int for item in actual):
            raise ValueError(f"invalid fuzz characterization: {key}")
    return value


def receipt(stdout):
    values = [json.loads(line[len(PREFIX):]) for line in stdout.splitlines() if line.startswith(PREFIX)]
    if len(values) != 1 or not isinstance(values[0], dict):
        raise ValueError("expected exactly one complete isolated kernel receipt")
    value = values[0]
    expected = dict(version=4, status="passed", uid=0, passed=len(ALL_EXPECTED),
                    routing_passed=len(EXPECTED), recovery_passed=len(RECOVERY_EXPECTED),
                    repeat_characterization_passed=1, repeat_passthrough_tested=False,
                    fuzz_characterization_passed=1, fuzz_passthrough_tested=False,
                    physical_devices_opened=0, model_desktop_context=True,
                    production_route_admission_tested=False, compositor_consumer_retirement_tested=False,
                    rule_removed=True, registry_clean=True, unrelated_registry_preserved=True, recovery_controls_removed=True)
    if set(value) != set(expected) | {"tests", "kernel"}:
        raise ValueError("isolated kernel receipt has a different schema")
    for key, wanted in expected.items():
        if type(value.get(key)) is not type(wanted) or value[key] != wanted:
            raise ValueError(f"invalid isolated kernel receipt: {key}")
    if value.get("tests") != sorted(ALL_EXPECTED) or value.get("kernel") != os.uname().release:
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


def registry_snapshot(directory):
    entries = {}
    with os.scandir(directory) as iterator:
        for index, entry in enumerate(iterator):
            if index >= 4096:
                raise RuntimeError("registry snapshot exceeds bounds")
            metadata = entry.stat(follow_symlinks=False)
            entries[entry.name] = (metadata.st_dev, metadata.st_ino, metadata.st_mode, metadata.st_size, metadata.st_mtime_ns)
    return entries


@contextlib.contextmanager
def recovery_directory(parent):
    name = f"recovery-{uuid.uuid4().hex}"
    os.mkdir(name, 0o700, dir_fd=parent)
    fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=parent)
    metadata = os.fstat(fd)
    identity = metadata.st_dev, metadata.st_ino
    os.close(fd)
    try:
        yield name, identity
    finally:
        failing = sys.exc_info()[0] is not None
        try:
            if not same(parent, name, identity):
                raise RuntimeError("recovery control directory was replaced; retaining it")
            os.rmdir(name, dir_fd=parent)
        except (OSError, RuntimeError) as error:
            if not failing:
                raise
            print(f"Retained recovery controls {name}: {error}", file=sys.stderr)


def execute(command, env):
    parent_pid = os.getpid()
    child = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                             start_new_session=True, text=True, preexec_fn=lambda: child_setup(parent_pid))
    try:
        output, _ = child.communicate(timeout=EXECUTION_TIMEOUT)
    except BaseException:
        if child.returncode is None:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        output, _ = child.communicate()
        print(output, end="", flush=True)
        raise
    print(output, end="", flush=True)
    if child.returncode != 0:
        raise RuntimeError(f"isolated kernel executable failed with exit {child.returncode}")
    return output


def run(args):
    # No filesystem mutation or udev/device access occurs before this check.
    if os.geteuid() != 0:
        raise PermissionError("isolated kernel runner requires user-launched root credentials")
    for path in [args.tests, args.recovery, args.native, args.importer, args.udevadm]:
        resolved = Path(path).resolve(strict=True)
        if not str(resolved).startswith("/nix/store/") or not resolved.is_file():
            raise ValueError("isolated root runner accepts immutable packaged executables only")
    env = {"PREK_ISOLATED_ROUTING": "1"}
    listed = subprocess.run([args.tests, "--list", "--ignored"], env=env,
                            capture_output=True, text=True, check=True, timeout=10)
    test_inventory(listed.stdout)
    listed = subprocess.run([args.recovery, "--inventory"], env={}, capture_output=True, text=True, check=True, timeout=10)
    recovery_inventory(listed.stdout)
    listed = subprocess.run([args.native, REPEAT_CASE, "--exact", "--list", "--ignored"], env=env,
                            capture_output=True, text=True, check=True, timeout=10)
    repeat_inventory(listed.stdout)
    listed = subprocess.run([args.native, FUZZ_CASE, "--exact", "--list", "--ignored"], env=env,
                            capture_output=True, text=True, check=True, timeout=10)
    fuzz_inventory(listed.stdout)
    if not stat.S_ISCHR(os.stat("/dev/uinput").st_mode):
        raise RuntimeError("/dev/uinput is unavailable; the runner does not load kernel modules")
    with directories() as (parent, registry, rules):
        fcntl.flock(parent, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if any(name.startswith("prek-test-") for name in os.listdir(registry)):
            raise RuntimeError("existing isolated device metadata must be inspected before another root run")
        original_registry = registry_snapshot(registry)
        rule = f"70-prek-isolated-{uuid.uuid4().hex}.rules"
        contents = ('ACTION=="add", SUBSYSTEM=="input", KERNEL=="event*", ATTRS{phys}=="prek/test/*", '
                    'ENV{PREK_TEST}="1", ENV{PREK_VIRTUAL}="1", ENV{LIBINPUT_IGNORE_DEVICE}="1", '
                    f'IMPORT{{program}}="{args.importer} udev-import --marker $attr{{phys}}"\n')
        fd = os.open(rule, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                     0o644, dir_fd=rules)
        metadata = os.fstat(fd)
        identity = metadata.st_dev, metadata.st_ino
        try:
            with os.fdopen(fd, "w") as file:
                file.write(contents)
                file.flush()
                os.fsync(file.fileno())
            subprocess.run([args.udevadm, "control", "--reload-rules"], check=True, env={}, timeout=10)
            # No trigger: only devices created by these tests receive the rule.
            output = execute([args.tests, "--ignored", "--nocapture", "--test-threads=1"], env)
            test_result(output)
            repeat_result(execute([args.native, REPEAT_CASE, "--exact", "--ignored", "--nocapture", "--test-threads=1"], env))
            fuzz_result(execute([args.native, FUZZ_CASE, "--exact", "--ignored", "--nocapture", "--test-threads=1"], env))
            with recovery_directory(parent) as (name, run_identity):
                recovery_env = dict(env, PREK_RECOVERY_DIRECTORY=f"/run/prek/{name}",
                                    PREK_RECOVERY_IDENTITY=f"{run_identity[0]}:{run_identity[1]}")
                recovery_result(execute([args.recovery], recovery_env))
        finally:
            remove_same(rules, rule, identity)
            subprocess.run([args.udevadm, "control", "--reload-rules"], check=True, env={}, timeout=10)
        if any(name.startswith("prek-test-") for name in os.listdir(registry)):
            raise RuntimeError("isolated device metadata remains; preserving it for inspection")
        if registry_snapshot(registry) != original_registry:
            raise RuntimeError("unrelated registry state changed; preserving it for inspection")
    print(PREFIX + json.dumps(dict(version=4, status="passed", uid=0, kernel=os.uname().release,
          tests=sorted(ALL_EXPECTED), passed=len(ALL_EXPECTED), routing_passed=len(EXPECTED), recovery_passed=8, physical_devices_opened=0,
          repeat_characterization_passed=1, repeat_passthrough_tested=False,
          fuzz_characterization_passed=1, fuzz_passthrough_tested=False,
          model_desktop_context=True, production_route_admission_tested=False,
          compositor_consumer_retirement_tested=False, rule_removed=True, registry_clean=True,
          unrelated_registry_preserved=True, recovery_controls_removed=True)), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tests", required=True)
    parser.add_argument("--recovery", required=True)
    parser.add_argument("--native", required=True)
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
