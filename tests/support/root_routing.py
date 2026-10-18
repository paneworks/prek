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
KEY_OUTPUT_CASE = "keys::native::tests::isolated_generated_keyboard_executes_owned_keys_and_exact_cancellation"
KEY_OUTPUT_RESULT = "PREK_GENERATED_KEY_OUTPUT_RESULT="
MANAGED_CASE = "managed::selective_key_worker"
MANAGED_INVENTORY = "PREK_MANAGED_KEY_WORKER_INVENTORY_V1"
MANAGED_RESULT = "PREK_MANAGED_KEY_WORKER_RESULT="
MANAGED_PHASE = "PREK_MANAGED_KEY_WORKER_PHASE="
MANAGED_DRAIN = "PREK_MANAGED_KEY_WORKER_DRAIN="
MANAGED_SOFTWARE = "PREK_MANAGED_KEY_WORKER_SOFTWARE="
ALL_EXPECTED = EXPECTED | RECOVERY_EXPECTED | {REPEAT_CASE, FUZZ_CASE, KEY_OUTPUT_CASE, MANAGED_CASE}
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


def key_output_inventory(stdout):
    names = [line.removesuffix(": test") for line in stdout.splitlines() if line.endswith(": test")]
    if names != [KEY_OUTPUT_CASE]:
        raise ValueError("generated key output executable has a different inventory")


def key_output_result(stdout):
    results = re.findall(r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", stdout, re.M)
    values = [json.loads(line[len(KEY_OUTPUT_RESULT):]) for line in stdout.splitlines() if line.startswith(KEY_OUTPUT_RESULT)]
    if results != [("1", "0", "0")] or len(values) != 1 or not isinstance(values[0], dict):
        raise ValueError("generated key output did not execute exactly once")
    value = values[0]
    expected = dict(version=1, status="passed", uid=0, physical_devices_opened=0,
                    native_preparation_tested=True, generated_role_metadata_verified=True,
                    native_key_output_tested=True, native_cancel_before_write_tested=True,
                    native_repeat_event_tested=True, native_partial_write_tested=False,
                    broker_authorization_tested=False, compositor_delivery_tested=False,
                    worker_binding_tested=False, owned_device_removed=True)
    if set(value) != set(expected):
        raise ValueError("generated key output has a different schema")
    for key, wanted in expected.items():
        if type(value[key]) is not type(wanted) or value[key] != wanted:
            raise ValueError(f"invalid generated key output: {key}")
    return value


def managed_inventory(stdout):
    if stdout.splitlines() != [MANAGED_INVENTORY, json.dumps([MANAGED_CASE], separators=(",", ":"))]:
        raise ValueError("managed worker executable has a different inventory")


def managed_software(stdout, phases):
    receipts = [json.loads(line[len(MANAGED_SOFTWARE):]) for line in stdout.splitlines() if line.startswith(MANAGED_SOFTWARE)]
    expected = [(1, 1, "open", False), (1, 1, "open", True), (1, 2, "press", False),
                (1, 2, "press", True), (1, 3, "repeat", True), (1, 4, "release", True),
                (1, 5, "close", True), (1, 1, "open", False),
                (2, 1, "open", True), (2, 2, "press", True),
                (3, 1, "open", True), (3, 2, "press", True),
                (2, 3, "close", True), (3, 3, "repeat", True), (3, 4, "renew", True),
                (3, 5, "renew", False), (3, 1, "open", False),
                (4, 1, "open", True), (4, 2, "press", True), (4, 3, "repeat", True), (4, 4, "close", True)]
    if len(receipts) != len(expected):
        raise ValueError("managed software admissions are incomplete")
    scope = None
    previous = 0
    expires = []
    for index, (receipt, (producer, sequence, kind, accepted)) in enumerate(zip(receipts, expected)):
        if not isinstance(receipt, dict) or set(receipt) != {"version", "request", "accepted", "sent_at_ns", "received_at_ns"} \
                or type(receipt["version"]) is not int or receipt["version"] != 1 \
                or type(receipt["accepted"]) is not bool or receipt["accepted"] != accepted:
            raise ValueError("managed software admission schema or outcome differs")
        sent, received = receipt["sent_at_ns"], receipt["received_at_ns"]
        if any(type(at) is not int or at <= 0 for at in (sent, received)) \
                or not previous <= sent <= received < sent + 250_000_000:
            raise ValueError("managed software admission clocks differ")
        previous = received
        request = receipt["request"]
        if not isinstance(request, dict) or set(request) != {"fence", "source", "marker", "consumer", "producer", "sequence", "operation"} \
                or request["producer"] != [producer, 7] or not isinstance(request["producer"], list) \
                or any(type(word) is not int for word in request["producer"]) \
                or type(request["sequence"]) is not int or request["sequence"] != sequence:
            raise ValueError("managed software request identity differs")
        fence, source, consumer, marker = (request[key] for key in ("fence", "source", "consumer", "marker"))
        if not isinstance(fence, dict) or set(fence) != {"broker", "configuration", "session"} \
                or any(type(fence[key]) is not int or fence[key] != 1 for key in ("configuration", "session")) \
                or not isinstance(fence["broker"], list) or len(fence["broker"]) != 2 \
                or any(type(word) is not int or not 0 <= word < 2**64 for word in fence["broker"]) or not any(fence["broker"]) \
                or not isinstance(source, dict) or set(source) != {"seat", "device", "generation"} or source["seat"] != "seat0" \
                or any(type(source[key]) is not int or not 0 < source[key] < 2**64 for key in ("device", "generation")) \
                or not isinstance(marker, str) or not marker.startswith("prek/test/") or len(marker) > 1024 \
                or any(ord(char) < 32 or ord(char) == 127 for char in marker) \
                or not isinstance(consumer, dict) or set(consumer) != {"isolated_receiver"}:
            raise ValueError("managed software request scope differs")
        identity = consumer["isolated_receiver"]
        if not isinstance(identity, dict) or set(identity) != {"pid", "start"} \
                or type(identity["pid"]) is not int or not 0 < identity["pid"] < 2**32 \
                or type(identity["start"]) is not int or not 0 < identity["start"] < 2**64:
            raise ValueError("managed software receiver identity differs")
        current = dict(fence=fence, source=source, marker=marker, consumer=consumer)
        if index == 1:
            scope = current
            forged = receipts[0]["request"]
            if any(forged[key] != request[key] for key in ("fence", "source", "marker", "operation")) \
                    or forged["consumer"]["isolated_receiver"] != dict(pid=identity["pid"] + 1, start=identity["start"]):
                raise ValueError("managed forged-consumer rejection is not correlated")
        elif index == 2:
            if source == scope["source"] or any(current[key] != scope[key] for key in ("fence", "marker", "consumer")):
                raise ValueError("managed physical alias rejection is not correlated")
        elif index > 2 and current != scope:
            raise ValueError("managed software output incarnation changed")
        operation = request["operation"]
        fields = {"kind"} | ({"expires"} if kind in ("open", "renew") else {"id", "key"} if kind == "press" else {"id"} if kind in ("repeat", "release") else set())
        if not isinstance(operation, dict) or set(operation) != fields or operation["kind"] != kind:
            raise ValueError("managed software operation differs")
        if "id" in fields and (type(operation["id"]) is not int or operation["id"] != 1) \
                or "key" in fields and (type(operation["key"]) is not int or operation["key"] != 42):
            raise ValueError("managed software press handle differs")
        expiry = None
        if "expires" in fields:
            duration = operation["expires"]
            if not isinstance(duration, dict) or set(duration) != {"secs", "nanos"} \
                    or type(duration["secs"]) is not int or not 0 <= duration["secs"] < 2**64 \
                    or type(duration["nanos"]) is not int or not 0 <= duration["nanos"] < 1_000_000_000:
                raise ValueError("managed software lease is not a duration")
            expiry = duration["secs"] * 1_000_000_000 + duration["nanos"]
            if not received < expiry <= sent + 250_000_000:
                raise ValueError("managed software lease exceeds admission scope")
        expires.append(expiry)
    release = phases[3]["generated_times_ns"][0]
    if not expires[14] // 1000 * 1000 <= release < expires[14] + 250_000_000 \
            or receipts[14]["received_at_ns"] > release + 999 \
            or receipts[15]["sent_at_ns"] + 999 < release \
            or phases[0]["generated_times_ns"][0] + 999 < receipts[3]["sent_at_ns"] \
            or phases[0]["generated_times_ns"][-1] > receipts[6]["sent_at_ns"] \
            or phases[6]["generated_times_ns"][0] > receipts[20]["sent_at_ns"]:
        raise ValueError("managed software kernel reports do not match request lifetimes")
    return receipts


def managed_result(stdout):
    values = [json.loads(line[len(MANAGED_RESULT):]) for line in stdout.splitlines() if line.startswith(MANAGED_RESULT)]
    expected = dict(version=3, status="passed", uid=0, physical_devices_opened=0,
                    worker_binding_tested=True, private_control_authenticated=True,
                    consumer_neutral_drain_tested=True, consumer_neutral_drain_acknowledgements=1,
                    raw_outputs=2, generated_outputs=1, source_readers_grabbed=2,
                    native_projection_tested=True, native_repeat_tested=True, native_cancellation_tested=True,
                    kernel_write_order_tested=True, native_partial_write_tested=False,
                    software_producer_control_tested=True, software_producer_expiry_tested=True,
                    software_producer_rejection_tested=True, software_physical_ownership_tested=True,
                    software_request_acknowledgements=21, software_rejections=5, software_public_authorization_tested=False,
                    broker_grant_tested=False, context_authority_modeled=True,
                    production_input_loop_tested=False, production_route_admission_tested=False,
                    compositor_delivery_tested=False, keys_neutral=True, worker_reaped=True, owned_devices_removed=True)
    if len(values) != 1 or not isinstance(values[0], dict) or set(values[0]) != set(expected):
        raise ValueError("managed native worker has a different result schema")
    for key, wanted in expected.items():
        if type(values[0][key]) is not type(wanted) or values[0][key] != wanted:
            raise ValueError(f"invalid managed native result: {key}")
    phases = [json.loads(line[len(MANAGED_PHASE):]) for line in stdout.splitlines() if line.startswith(MANAGED_PHASE)]
    if len(phases) != 8 or [phase.get("phase") for phase in phases if isinstance(phase, dict)] != ["software_fast", "software_overlap", "software_sibling", "software_expiry", "mixed", "cross_source", "software_physical", "revocation"]:
        raise ValueError("managed native worker did not execute eight exact phases")
    wanted_edges = [([], [], [[42, 1], [42, 2], [42, 0]]), ([], [], [[42, 1]]),
                    ([], [], [[42, 2]]), ([], [], [[42, 0]]),
                    ([[31, 1], [31, 0]], [], [[42, 1], [42, 2], [42, 0]]),
                    ([], [[31, 1], [31, 0]], [[42, 1]]),
                    ([], [], [[42, 2]]),
                    ([[31, 1], [31, 0]], [], [[42, 0]])]
    counts = [(0, 0, 3), (0, 0, 1), (0, 0, 1), (0, 0, 1), (2, 0, 3), (0, 1, 1), (0, 0, 1), (1, 0, 1)]
    for phase, edges, frames in zip(phases, wanted_edges, counts):
        if set(phase) != {"phase", "raw0", "raw1", "generated", "raw0_times_ns", "raw1_times_ns", "generated_times_ns"}:
            raise ValueError("managed phase has a different schema")
        for role, wanted, count in zip(("raw0", "raw1", "generated"), edges, frames):
            actual = phase[role]
            times = phase[role + "_times_ns"]
            if not isinstance(actual, list) or actual != wanted or any(not isinstance(edge, list) or any(type(value) is not int for value in edge) for edge in actual):
                raise ValueError("managed phase key stream differs")
            if not isinstance(times, list) or len(times) != count or any(type(value) is not int or value <= 0 for value in times):
                raise ValueError("managed phase kernel timestamps differ")
    mixed, cross, cancelled = phases[4], phases[5], phases[7]
    g, r = mixed["generated_times_ns"], mixed["raw0_times_ns"]
    if not (g[0] <= g[1] < r[0] < g[2] < r[1]) \
            or cross["generated_times_ns"][0] >= cross["raw1_times_ns"][0] \
            or cancelled["generated_times_ns"][0] >= cancelled["raw0_times_ns"][0]:
        raise ValueError("managed phase kernel write order differs")
    previous = 0
    for phase in phases:
        times = phase["generated_times_ns"]
        if times != sorted(times) or times[0] < previous:
            raise ValueError("managed generated phases are not monotonic")
        previous = times[-1]
    managed_software(stdout, phases)
    drains = [json.loads(line[len(MANAGED_DRAIN):]) for line in stdout.splitlines() if line.startswith(MANAGED_DRAIN)]
    if len(drains) != 1 or not isinstance(drains[0], dict):
        raise ValueError("managed native worker did not acknowledge exactly one consumer drain")
    drain = drains[0]
    if set(drain) != {"version", "nonce", "requested_at_ns", "neutral_at_ns", "deadline_ns", "last_report_at_ns", "keys_down", "generated", "reports"}:
        raise ValueError("managed consumer drain schema differs")
    if type(drain["version"]) is not int or drain["version"] != 1 \
            or type(drain["reports"]) is not int or drain["reports"] != 1 \
            or drain["keys_down"] != [] or drain["generated"] != [[42, 0]] \
            or any(type(item) is not int for edge in drain["generated"] for item in edge):
        raise ValueError("managed consumer drain did not consume the final neutral release")
    nonce = drain["nonce"]
    if not isinstance(nonce, list) or len(nonce) != 2 or not any(nonce) \
            or any(type(item) is not int or not 0 <= item < 2**64 for item in nonce):
        raise ValueError("managed consumer drain nonce differs")
    times = [drain[key] for key in ("last_report_at_ns", "requested_at_ns", "neutral_at_ns", "deadline_ns")]
    if any(type(item) is not int or item <= 0 for item in times) \
            or not times[0] <= times[1] <= times[2] < times[3] \
            or times[3] - times[1] > 250_000_000 \
            or times[0] != cancelled["generated_times_ns"][0] \
            or times[2] >= cancelled["raw0_times_ns"][0]:
        raise ValueError("managed consumer drain clock or reentry order differs")
    return values[0]


def receipt(stdout):
    values = [json.loads(line[len(PREFIX):]) for line in stdout.splitlines() if line.startswith(PREFIX)]
    if len(values) != 1 or not isinstance(values[0], dict):
        raise ValueError("expected exactly one complete isolated kernel receipt")
    value = values[0]
    expected = dict(version=8, status="passed", uid=0, passed=len(ALL_EXPECTED),
                    routing_passed=len(EXPECTED), recovery_passed=len(RECOVERY_EXPECTED),
                    repeat_characterization_passed=1, repeat_passthrough_tested=False,
                    fuzz_characterization_passed=1, fuzz_passthrough_tested=False,
                    generated_key_output_passed=1, generated_key_output_authorization_tested=False,
                    generated_key_output_compositor_tested=False, generated_key_output_partial_write_tested=False,
                    managed_key_worker_passed=1, managed_key_worker_binding_tested=True,
                    managed_key_worker_consumer_drain_tested=True,
                    managed_key_worker_software_control_tested=True, managed_key_worker_software_expiry_tested=True,
                    managed_key_worker_software_authorization_tested=False,
                    managed_key_worker_compositor_tested=False, managed_key_worker_partial_write_tested=False,
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
    for path in [args.tests, args.recovery, args.native, args.managed, args.importer, args.udevadm]:
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
    listed = subprocess.run([args.native, KEY_OUTPUT_CASE, "--exact", "--list", "--ignored"], env=env,
                            capture_output=True, text=True, check=True, timeout=10)
    key_output_inventory(listed.stdout)
    listed = subprocess.run([args.managed, "--inventory"], env=env,
                            capture_output=True, text=True, check=True, timeout=10)
    managed_inventory(listed.stdout)
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
            key_output_result(execute([args.native, KEY_OUTPUT_CASE, "--exact", "--ignored", "--nocapture", "--test-threads=1"], env))
            managed_result(execute([args.managed, "--run"], env))
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
    print(PREFIX + json.dumps(dict(version=8, status="passed", uid=0, kernel=os.uname().release,
          tests=sorted(ALL_EXPECTED), passed=len(ALL_EXPECTED), routing_passed=len(EXPECTED), recovery_passed=8, physical_devices_opened=0,
          repeat_characterization_passed=1, repeat_passthrough_tested=False,
          fuzz_characterization_passed=1, fuzz_passthrough_tested=False,
          generated_key_output_passed=1, generated_key_output_authorization_tested=False,
          generated_key_output_compositor_tested=False, generated_key_output_partial_write_tested=False,
          managed_key_worker_passed=1, managed_key_worker_binding_tested=True,
          managed_key_worker_consumer_drain_tested=True,
          managed_key_worker_software_control_tested=True, managed_key_worker_software_expiry_tested=True,
          managed_key_worker_software_authorization_tested=False,
          managed_key_worker_compositor_tested=False, managed_key_worker_partial_write_tested=False,
          model_desktop_context=True, production_route_admission_tested=False,
          compositor_consumer_retirement_tested=False, rule_removed=True, registry_clean=True,
          unrelated_registry_preserved=True, recovery_controls_removed=True)), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tests", required=True)
    parser.add_argument("--recovery", required=True)
    parser.add_argument("--native", required=True)
    parser.add_argument("--managed", required=True)
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
