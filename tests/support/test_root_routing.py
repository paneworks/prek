"""Pure parser/cleanup tests; receipt fixtures model root, never obtain it."""
import json
import os
import select
import shutil
import signal
import subprocess
import sys
from pathlib import Path
import tempfile
import unittest
import root_routing as root


class KernelReceipt(unittest.TestCase):
    def value(self):
        return dict(version=1, status="passed", uid=0, kernel=os.uname().release,
                    tests=sorted(root.EXPECTED), passed=len(root.EXPECTED), physical_devices_opened=0,
                    model_desktop_context=True, production_route_admission_tested=False,
                    compositor_consumer_retirement_tested=False, rule_removed=True, registry_clean=True)

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


if __name__ == "__main__":
    unittest.main()
