"""Pure parser/cleanup tests; receipt fixtures model root, never obtain it."""
import argparse
import contextlib
import io
import json
import os
import select
import shutil
import signal
import stat
import subprocess
import sys
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import root_routing as root


def recovery_inventory():
    return root.RECOVERY_INVENTORY + "\n" + json.dumps(sorted(root.RECOVERY_EXPECTED)) + "\n"


def recovery_output(names=None, **changes):
    names = sorted(root.RECOVERY_EXPECTED) if names is None else names
    cases = "".join(f"kernel recovery: {name.split('::')[1]} / {name.split('::')[2]} ... ok\n" for name in names)
    value = dict(version=1, passed=8, tests=names)
    value.update(changes)
    return cases + root.RECOVERY_RESULT + json.dumps(value) + "\n"


class KernelReceipt(unittest.TestCase):
    def value(self):
        return dict(version=2, status="passed", uid=0, kernel=os.uname().release,
                    tests=sorted(root.ALL_EXPECTED), passed=len(root.ALL_EXPECTED),
                    routing_passed=len(root.EXPECTED), recovery_passed=8, physical_devices_opened=0,
                    model_desktop_context=True, production_route_admission_tested=False,
                    compositor_consumer_retirement_tested=False, rule_removed=True, registry_clean=True,
                    unrelated_registry_preserved=True, recovery_controls_removed=True)

    def line(self, value):
        return root.PREFIX + json.dumps(value) + "\n"

    def test_one_correlated_receipt_is_required(self):
        value = self.value()
        self.assertEqual(root.receipt("test output\n" + self.line(value)), value)
        for output in ["", self.line(value) * 2, root.PREFIX + "{", self.line([])]:
            with self.subTest(output=output), self.assertRaises(ValueError):
                root.receipt(output)

    def test_every_acceptance_field_is_required_and_typed(self):
        for field in self.value():
            for replacement in [None, "foreign", True, 999]:
                value = self.value()
                if replacement == value[field] and type(replacement) is type(value[field]):
                    continue
                value[field] = replacement
                with self.subTest(field=field, replacement=replacement), self.assertRaises(ValueError):
                    root.receipt(self.line(value))

    def test_old_inventory_without_both_native_witness_cases_is_not_acceptance(self):
        names = {name for name in root.EXPECTED if name.startswith("evdev_boundary::witness_")}
        self.assertEqual(len(names), 2)
        old = self.value()
        old.update(tests=sorted(root.ALL_EXPECTED - names), passed=len(root.ALL_EXPECTED) - 2,
                   routing_passed=len(root.EXPECTED) - 2)
        with self.assertRaises(ValueError):
            root.receipt(self.line(old))
        inventory = "\n".join(f"{name}: test" for name in sorted(root.EXPECTED - names))
        with self.assertRaises(ValueError):
            root.test_inventory(inventory)

    def test_receipt_rejects_missing_foreign_or_uncorrelated_fields(self):
        for field in self.value():
            value = self.value()
            del value[field]
            with self.subTest(missing=field), self.assertRaises(ValueError):
                root.receipt(self.line(value))
        for changes in [dict(extra=True), dict(kernel="foreign-kernel"),
                        dict(tests=list(reversed(sorted(root.ALL_EXPECTED)))),
                        dict(tests=sorted(root.EXPECTED))]:
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                root.receipt(self.line(dict(self.value(), **changes)))

    def test_recovery_inventory_is_exact_and_cannot_be_a_skip(self):
        valid = recovery_inventory()
        root.recovery_inventory(valid)
        for output in ["", "skipped\n", valid + "extra\n", valid * 2,
                       valid.replace(root.RECOVERY_INVENTORY, "FOREIGN"),
                       root.RECOVERY_INVENTORY + "\n{", root.RECOVERY_INVENTORY + "\n{}",
                       root.RECOVERY_INVENTORY + "\n" + json.dumps([True] * 8),
                       root.RECOVERY_INVENTORY + "\n" + json.dumps(sorted(root.RECOVERY_EXPECTED)[:-1]),
                       root.RECOVERY_INVENTORY + "\n" + json.dumps([sorted(root.RECOVERY_EXPECTED)[0]] * 8)]:
            with self.subTest(output=output), self.assertRaises(ValueError):
                root.recovery_inventory(output)

    def test_recovery_result_correlates_every_case_and_summary(self):
        valid = recovery_output()
        root.recovery_result(valid)
        root.recovery_result(recovery_output(list(reversed(sorted(root.RECOVERY_EXPECTED)))))
        for output in ["", "kernel recovery skipped\n", valid * 2,
                       valid.replace("keyboard / orderly", "keyboard / skipped", 1),
                       valid.replace(" ... ok", " ... FAILED", 1),
                       valid + "kernel recovery: foreign / orderly ... ok\n",
                       valid.split(root.RECOVERY_RESULT)[0],
                       valid.split(root.RECOVERY_RESULT)[0] + root.RECOVERY_RESULT + "{}\n",
                       recovery_output(sorted(root.RECOVERY_EXPECTED)[:-1]),
                       recovery_output([sorted(root.RECOVERY_EXPECTED)[0]] * 8),
                       recovery_output(tests=list(reversed(sorted(root.RECOVERY_EXPECTED)))),
                       recovery_output(version=True), recovery_output(passed=True),
                       recovery_output(version=2), recovery_output(passed=0), recovery_output(extra=True)]:
            with self.subTest(output=output), self.assertRaises(ValueError):
                root.recovery_result(output)

    def test_exact_ignored_inventory_prevents_zero_or_changed_gates(self):
        expected = "\n".join(name + ": test" for name in sorted(root.EXPECTED))
        root.test_inventory(expected)
        for output in ["", expected + "\nforeign: test", expected + "\n" + expected, expected.split("\n", 1)[1]]:
            with self.subTest(output=output), self.assertRaises(ValueError):
                root.test_inventory(output)

    def test_summary_cannot_convert_skip_failure_or_subset_into_acceptance(self):
        root.test_result(f"test result: ok. {len(root.EXPECTED)} passed; 0 failed; 0 ignored;\n")
        for result in ["", "test result: ok. 0 passed; 0 failed; 11 ignored;\n",
                       "test result: FAILED. 10 passed; 1 failed; 0 ignored;\n",
                       "test result: ok. 1 passed; 0 failed; 0 ignored;\n"]:
            with self.subTest(result=result), self.assertRaises(ValueError):
                root.test_result(result)

    def test_cleanup_preserves_replaced_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary)
            original = path / "owned.rules"
            original.write_text("our test rule")
            metadata = original.stat()
            identity = metadata.st_dev, metadata.st_ino
            fd = os.open(path, os.O_DIRECTORY | os.O_RDONLY)
            try:
                original.rename(path / "retained-original.rules")
                original.write_text("replacement")
                with self.assertRaises(RuntimeError):
                    root.remove_same(fd, original.name, identity)
                self.assertEqual(original.read_text(), "replacement")
                root.remove_same(fd, "retained-original.rules", identity)
                self.assertFalse((path / "retained-original.rules").exists())
            finally:
                os.close(fd)

    def test_recovery_directory_removes_only_its_empty_original(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary)
            fd = os.open(path, os.O_DIRECTORY | os.O_RDONLY)
            try:
                with root.recovery_directory(fd) as (name, identity):
                    directory = path / name
                    self.assertTrue(root.same(fd, name, identity))
                    self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o700)
                self.assertFalse(directory.exists())
                with self.assertRaisesRegex(RuntimeError, "replaced"), root.recovery_directory(fd) as (name, _):
                    directory = path / name
                    directory.rename(path / "retained-original")
                    directory.mkdir()
                    (directory / "foreign").write_text("preserved")
                self.assertEqual((directory / "foreign").read_text(), "preserved")
                self.assertTrue((path / "retained-original").is_dir())
            finally:
                os.close(fd)

    def test_nonempty_controls_are_retained_without_masking_original_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary)
            fd = os.open(path, os.O_DIRECTORY | os.O_RDONLY)
            try:
                with self.assertRaises(OSError), root.recovery_directory(fd) as (name, _):
                    (path / name / "unknown").write_text("retained")
                self.assertEqual((path / name / "unknown").read_text(), "retained")
                diagnostic = io.StringIO()
                with contextlib.redirect_stderr(diagnostic), self.assertRaisesRegex(ValueError, "original failure"):
                    with root.recovery_directory(fd) as (name, _):
                        (path / name / "unknown").write_text("retained")
                        raise ValueError("original failure")
                self.assertIn("Retained recovery controls", diagnostic.getvalue())
                self.assertEqual((path / name / "unknown").read_text(), "retained")
            finally:
                os.close(fd)

    def test_registry_snapshot_preserves_foreign_entries_and_does_not_follow_links(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary)
            (path / "foreign").write_text("preserved")
            (path / "broken-link").symlink_to(path / "absent")
            fd = os.open(path, os.O_DIRECTORY | os.O_RDONLY)
            try:
                original = root.registry_snapshot(fd)
                self.assertEqual(set(original), {"foreign", "broken-link"})
                self.assertTrue(stat.S_ISLNK(original["broken-link"][2]))
                self.assertEqual(root.registry_snapshot(fd), original)
                (path / "foreign").write_text("changed content")
                self.assertNotEqual(root.registry_snapshot(fd), original)
                self.assertTrue((path / "broken-link").is_symlink())
            finally:
                os.close(fd)

    def test_unprivileged_refusal_precedes_filesystem_devices_and_commands(self):
        self.assertNotEqual(os.geteuid(), 0, "these tests must run without root")
        with mock.patch.object(root, "directories") as directories, mock.patch.object(root.os, "stat") as metadata, \
                mock.patch.object(root.subprocess, "run") as command:
            with self.assertRaises(PermissionError):
                root.run(argparse.Namespace())
            directories.assert_not_called()
            metadata.assert_not_called()
            command.assert_not_called()

    def test_execute_keeps_real_child_output_and_nonzero_status(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.assertEqual(root.execute([sys.executable, "-c", "print('owned success')"], {}), "owned success\n")
            with self.assertRaisesRegex(RuntimeError, "exit 17"):
                root.execute([sys.executable, "-c", "print('owned failure', flush=True); raise SystemExit(17)"], {})
        self.assertIn("owned success", output.getvalue())
        self.assertIn("owned failure", output.getvalue())

    def test_timeout_kills_owned_group_even_when_leader_has_already_exited(self):
        source = "import subprocess, sys; child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)']); print(child.pid, flush=True)"
        output = io.StringIO()
        with mock.patch.object(root, "EXECUTION_TIMEOUT", 1), contextlib.redirect_stdout(output):
            with self.assertRaises(subprocess.TimeoutExpired):
                root.execute([sys.executable, "-c", source], {})
        pid = int(output.getvalue().strip())
        try:
            fd = os.pidfd_open(pid)
        except ProcessLookupError:
            return
        try:
            poller = select.poll()
            poller.register(fd, select.POLLIN)
            self.assertTrue(poller.poll(5000), "owned descendant survived timeout")
        finally:
            try:
                signal.pidfd_send_signal(fd, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.close(fd)

    def test_parent_death_kills_only_the_owned_test_child(self):
        # Real unprivileged processes/pidfd; no modeled root credentials.
        producer_source = r'''
import os, subprocess, sys
sys.path.insert(0, sys.argv[1])
import root_routing
parent = os.getpid()
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"],
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    preexec_fn=lambda: root_routing.child_setup(parent))
print(child.pid, flush=True)
sys.stdin.readline()
os._exit(0)
'''
        producer = subprocess.Popen([sys.executable, "-c", producer_source, str(Path(root.__file__).parent)],
                                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        child_fd = None
        try:
            poller = select.poll()
            poller.register(producer.stdout, select.POLLIN)
            self.assertTrue(poller.poll(5000), "test producer did not publish its owned child")
            pid = int(producer.stdout.readline())
            child_fd = os.pidfd_open(pid)
            child_poller = select.poll()
            child_poller.register(child_fd, select.POLLIN)
            self.assertFalse(child_poller.poll(0))
            producer.stdin.write("exit\n")
            producer.stdin.flush()
            self.assertEqual(producer.wait(timeout=5), 0)
            self.assertTrue(child_poller.poll(5000), "owned child survived its producer")
        finally:
            if producer.poll() is None:
                producer.kill()
            producer.wait()
            producer.stdin.close()
            producer.stdout.close()
            if child_fd is not None:
                try:
                    signal.pidfd_send_signal(child_fd, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                os.close(child_fd)

    def test_launcher_retains_failed_build_logs_before_authentication(self):
        launcher = Path(__file__).resolve().parents[1] / "run-root-routing.sh"
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            (repo / "tests").mkdir()
            script = repo / "tests/run-root-routing.sh"
            shutil.copyfile(launcher, script)
            # Both mocks are unprivileged commands. Nix fails before a runner
            # exists, so no authentication or root process can be launched.
            shell = "git(){ return 0; }; nix(){ echo 'deliberate pre-build refusal'; return 77; }; export -f git nix; exec bash \"$1\""
            result = subprocess.run(["bash", "-c", shell, "--", str(script)],
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 77)
            retained = (repo / "target/root-routing/latest").resolve(strict=True)
            self.assertEqual(json.loads((retained / "status.json").read_text()), {"status":"failed", "exit_code":77})
            self.assertIn("deliberate pre-build refusal", (retained / "console.log").read_text())
            self.assertFalse((retained / "receipt.json").exists())
            self.assertFalse((retained / "kernel.log").exists())


class RootRunModel(unittest.TestCase):
    """Privileged orchestration models use only test-owned temporary directories."""

    @contextlib.contextmanager
    def model(self, effect=None):
        with tempfile.TemporaryDirectory() as temporary, contextlib.ExitStack() as stack:
            path = Path(temporary)
            registry, rules = path / "devices", path / "rules"
            registry.mkdir()
            rules.mkdir()
            (registry / "foreign-record").write_text("preserved")
            fds = [os.open(directory, os.O_DIRECTORY | os.O_RDONLY) for directory in (path, registry, rules)]
            for fd in fds:
                stack.callback(os.close, fd)
            actual_stat = os.stat
            def metadata(name, *args, **kwargs):
                if name == "/dev/uinput":
                    return argparse.Namespace(st_mode=stat.S_IFCHR)
                return actual_stat(name, *args, **kwargs)
            def command(argv, **kwargs):
                if argv[0] == "/nix/store/model/routing":
                    return subprocess.CompletedProcess(argv, 0, "\n".join(name + ": test" for name in sorted(root.EXPECTED)))
                if argv[0] == "/nix/store/model/recovery":
                    return subprocess.CompletedProcess(argv, 0, recovery_inventory())
                self.assertEqual(argv, ["/nix/store/model/udevadm", "control", "--reload-rules"])
                self.assertEqual(kwargs["env"], {})
                return subprocess.CompletedProcess(argv, 0)
            def execute(argv, env):
                self.assertEqual(env["PREK_ISOLATED_ROUTING"], "1")
                self.assertEqual(len(list(rules.iterdir())), 1)
                if effect is not None:
                    effect(argv[0], path, registry, rules)
                if argv[0] == "/nix/store/model/recovery":
                    control = path / Path(env["PREK_RECOVERY_DIRECTORY"]).name
                    info = control.stat()
                    self.assertEqual(env["PREK_RECOVERY_IDENTITY"], f"{info.st_dev}:{info.st_ino}")
                    return recovery_output()
                return f"test result: ok. {len(root.EXPECTED)} passed; 0 failed; 0 ignored;\n"
            stack.enter_context(mock.patch.object(root.os, "geteuid", return_value=0))
            stack.enter_context(mock.patch.object(root.Path, "resolve", lambda value, **_: value))
            stack.enter_context(mock.patch.object(root.Path, "is_file", return_value=True))
            stack.enter_context(mock.patch.object(root.os, "stat", side_effect=metadata))
            stack.enter_context(mock.patch.object(root, "directories", return_value=contextlib.nullcontext(tuple(fds))))
            commands = stack.enter_context(mock.patch.object(root.subprocess, "run", side_effect=command))
            stack.enter_context(mock.patch.object(root, "execute", side_effect=execute))
            output = stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
            args = argparse.Namespace(tests="/nix/store/model/routing", recovery="/nix/store/model/recovery",
                                      importer="/nix/store/model/importer", udevadm="/nix/store/model/udevadm")
            yield args, path, registry, rules, commands, output

    def test_model_success_correlates_both_executables_and_removes_controls(self):
        with self.model() as (args, path, registry, rules, commands, output):
            root.run(args)
            self.assertEqual(root.receipt(output.getvalue())["passed"], 21)
            self.assertEqual({entry.name for entry in path.iterdir()}, {"devices", "rules"})
            self.assertEqual(list(rules.iterdir()), [])
            self.assertEqual((registry / "foreign-record").read_text(), "preserved")
            self.assertEqual(commands.call_count, 4)

    def test_model_both_executable_failures_remove_only_original_rule(self):
        for failure in ["routing", "recovery"]:
            def fail(executable, *_):
                if executable.endswith("/" + failure):
                    raise RuntimeError("modeled executable failure")
            with self.model(fail) as (args, _, registry, rules, commands, output):
                with self.subTest(failure=failure), self.assertRaisesRegex(RuntimeError, "modeled executable failure"):
                    root.run(args)
                self.assertEqual(list(rules.iterdir()), [])
                self.assertEqual((registry / "foreign-record").read_text(), "preserved")
                self.assertEqual(commands.call_count, 4)
                self.assertNotIn(root.PREFIX, output.getvalue())

    def test_model_registry_changes_are_retained_and_cannot_pass(self):
        def change(executable, _, registry, rules):
            if executable.endswith("/recovery"):
                (registry / "foreign-record").write_text("external change")
        with self.model(change) as (args, _, registry, rules, _, output):
            with self.assertRaisesRegex(RuntimeError, "unrelated registry state changed"):
                root.run(args)
            self.assertEqual((registry / "foreign-record").read_text(), "external change")
            self.assertEqual(list(rules.iterdir()), [])
            self.assertNotIn(root.PREFIX, output.getvalue())

    def test_model_replaced_rule_is_retained_and_cannot_pass(self):
        def replace(executable, path, _, rules):
            if executable.endswith("/recovery"):
                rule = next(rules.iterdir())
                rule.rename(path / "retained-original-rule")
                rule.write_text("foreign replacement")
        with self.model(replace) as (args, path, registry, rules, _, output):
            with self.assertRaisesRegex(RuntimeError, "replaced root test path"):
                root.run(args)
            self.assertEqual(next(rules.iterdir()).read_text(), "foreign replacement")
            self.assertTrue((path / "retained-original-rule").exists())
            self.assertEqual((registry / "foreign-record").read_text(), "preserved")
            self.assertNotIn(root.PREFIX, output.getvalue())


if __name__ == "__main__":
    unittest.main()
