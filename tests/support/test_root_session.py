"""Unprivileged transport tests and explicit models of privileged cleanup."""
import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import select
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock

import root_session as session


TOOLS = {name: f"/nix/store/fixed-test-tools/bin/{name}"
         for name in ("systemctl", "loginctl", "systemd-run", "journalctl", "chvt")}
session.configure_tools(dict(tools=TOOLS))


spec = importlib.util.spec_from_file_location("morf_controls", Path(__file__).with_name("morf-controls.py"))
controls = importlib.util.module_from_spec(spec)
spec.loader.exec_module(controls)


class Policy(unittest.TestCase):
    def test_tools_are_fixed_by_policy_not_path_or_usr_bin(self):
        with mock.patch.dict(session.TOOLS, clear=True):
            for tools in ({}, dict(TOOLS, arbitrary="/tmp/command"), dict(TOOLS, chvt="chvt"),
                          dict(TOOLS, chvt="/fixed/../command"), dict(TOOLS, chvt=None)):
                with self.subTest(tools=tools), self.assertRaises(ValueError):
                    session.configure_tools(dict(tools=tools))
            session.configure_tools(dict(tools=TOOLS))
            with mock.patch.dict(os.environ, PATH="/attacker/bin"), \
                    mock.patch.object(session, "command", return_value=subprocess.CompletedProcess([], 0, "[]", "")) as command:
                self.assertEqual(session.sessions(), [])
            self.assertEqual(command.call_args.args[0], [TOOLS["loginctl"], "list-sessions", "--json=short"])
            self.assertEqual(session.service_command("owned.service", [], [])[0], TOOLS["systemd-run"])

    def test_nixos_uses_native_graphics_and_other_hosts_keep_wrappers(self):
        config = dict(nix="/fixed/nix", repo="/owned/morf-repo", morf="/owned/morf", shell="/owned/init.lua",
                      output="/owned/logs", socket="/run/owned/rpc")
        for nixos in (True, False):
            with self.subTest(nixos=nixos):
                call = session.fixture_command(config, dict(fixture="/fixed/fixture"), nixos=nixos)
                command = call[call.index("-c") + 1:]
                self.assertEqual(command[:1] if nixos else command[:3],
                                 ["/fixed/fixture"] if nixos else ["nixGL", "nixVulkan", "/fixed/fixture"])
                self.assertEqual(command[-2:], ["--morf-root-probe-socket", "/run/owned/rpc"])
                self.assertIn("--hardware-rendering", command)

    def test_pam_child_selects_host_graphics_without_inheriting_nvidia_wrappers(self):
        state = dict(uid=os.getuid(), home="/owned/home", name="test-user", socket="/run/owned/rpc",
                     child=dict(nix="/fixed/nix", repo="/owned/morf-repo", morf="/owned/morf",
                                shell="/owned/init.lua", output="/owned/logs", nvidia="595.71.05"))
        for nixos in (True, False):
            with self.subTest(nixos=nixos), \
                    mock.patch.object(session, "checked_runtime", return_value=Path("/run/owned")), \
                    mock.patch.object(session, "read_json", return_value=state), \
                    mock.patch.object(Path, "exists", return_value=nixos), \
                    mock.patch.object(session, "exchange") as exchange, \
                    mock.patch.object(session.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run, \
                    mock.patch.dict(os.environ, NVIDIA_VERSION="foreign-driver"):
                self.assertEqual(session.child(mock.Mock(child=Path("/run/owned")),
                                               dict(probe="/fixed/probe", fixture="/fixed/fixture")), 0)
                self.assertEqual(run.call_count, 3)
                call = run.call_args.args[0]
                self.assertEqual(call[call.index("-c") + 1], "/fixed/fixture" if nixos else "nixGL")
                self.assertEqual(run.call_args.kwargs["env"].get("NVIDIA_VERSION"),
                                 None if nixos else "595.71.05")
                self.assertEqual(exchange.call_args.args, (state["socket"], dict(op="done", code=0)))

    def state(self):
        return dict(uid=1000, gid=1000, vt=9, leader=123, session="c99", scope="session-c99.scope",
                    child=dict(morf="/owned/morf"), user_unit="prek-owned.service", original_vt=1,
                    started=True, user_started=True, activated=True, cleanup=None,
                    dm=dict(ActiveState="active"))

    def facts(self):
        return dict(Id="c99", User="1000", Leader="123", Seat="seat0", VTNr="9", TTY="tty9",
                    Remote="no", Type="tty", Service="login", Class="user", Scope="session-c99.scope")

    def test_every_real_session_binding_is_required(self):
        for key in self.facts():
            for value in (None, "foreign"):
                facts = self.facts()
                facts[key] = value
                with self.subTest(key=key, value=value), self.assertRaises(PermissionError):
                    session.validate_session(facts, self.state(), 123)
        self.assertEqual(session.validate_session(self.facts(), self.state(), 123), self.facts())

    def test_fixed_user_service_creates_pam_login_not_an_environment_only_mock(self):
        values = session.user_properties(self.state())
        for value in ("User=1000", "Group=1000", "PAMName=login", "TTYPath=/dev/tty9",
                      "StandardInput=tty-fail", "RuntimeMaxSec=200", "KillMode=control-group"):
            self.assertIn(value, values)
        arguments = session.service_command("owned.service", ["/fixed/script", "one argument"],
                                            ["RuntimeMaxSec=240", "ExecStopPost=/fixed/cleanup"], wait=True)
        self.assertIn("--wait", arguments)
        self.assertIn("--pipe", arguments)
        self.assertIn("--property=ExecStopPost=/fixed/cleanup", arguments)
        self.assertEqual(arguments[-3:], ["--", "/fixed/script", "one argument"])

    def test_binding_validation_is_closed_and_never_executes_client_commands(self):
        bindings = ["/tmp/pc-abc_123/r", "1000", "abc_123", "/owned/compositor", "/owned/morf", "123", "456", "WAYLAND-1"]
        with mock.patch.object(session, "validate_peer") as peer, mock.patch.object(session.os, "readlink", side_effect=lambda path:
                "/owned/compositor" if "/123/" in path else "/owned/morf"):
            self.assertEqual(session.validate_bindings(bindings, self.state()), bindings)
            self.assertEqual(peer.call_count, 2)
            bad = [None, {}, [], bindings + ["extra"], [False] * 8]
            for index, value in ((0, "/tmp/pc-abc/../r"), (1, "0"), (2, "../foreign"),
                                 (3, "bad\nvalue"), (4, "/foreign/morf"), (5, "0123"),
                                 (5, str(2**31)), (6, "-1"), (7, "foreign/output"), (7, "x" * 4097)):
                changed = list(bindings)
                changed[index] = value
                bad.append(changed)
            for value in bad:
                with self.subTest(value=value), self.assertRaises((ValueError, PermissionError)):
                    session.validate_bindings(value, self.state())

    def test_foreign_scope_or_credentials_are_rejected(self):
        status = "Name:\towned\nUid:\t1000\t1000\t1000\t1000\n"
        with mock.patch.object(Path, "read_text", side_effect=[status, "0::/user.slice/session-other.scope\n"]):
            with self.assertRaises(PermissionError):
                session.validate_peer(123, 1000, self.state())
        with self.assertRaises(PermissionError):
            session.validate_peer(123, 1001, self.state())

    def test_root_guard_refuses_before_any_policy_or_host_access(self):
        if os.geteuid() == 0:
            self.skipTest("requires unprivileged execution")
        with mock.patch.object(session, "checked_runtime", side_effect=AssertionError("host access before root guard")):
            with self.assertRaises(PermissionError):
                session.cleanup(Path("/run/not-opened"))
        result = subprocess.run([sys.executable, session.__file__, "--policy", "/missing/policy", "--supervise", "/run/not-opened"],
                                capture_output=True, text=True, timeout=3)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requests sudo itself", result.stdout)
        self.assertNotIn("No such file", result.stdout)

    def test_disconnected_console_logging_does_not_abort_cleanup(self):
        output = mock.Mock()
        output.write.side_effect = BrokenPipeError("SSH disconnected")
        with mock.patch("sys.stdout", output), mock.patch("builtins.open", return_value=io.StringIO()) as opened:
            session.log("cleanup must continue")
            opened.assert_called_once_with(os.devnull, "w")

    def test_state_reads_reject_real_user_files_symlinks_and_fifos_without_blocking(self):
        if os.geteuid() == 0:
            self.skipTest("requires unprivileged execution")
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            regular = directory / "state"
            regular.write_text('{}\n')
            regular.chmod(0o600)
            link = directory / "link"
            link.symlink_to(regular)
            fifo = directory / "fifo"
            os.mkfifo(fifo, 0o600)
            for path in (regular, link, fifo):
                with self.subTest(path=path), self.assertRaises(OSError):
                    session.read_json(path)


class Transport(unittest.TestCase):
    def test_real_owned_socket_framing_and_strict_bounds(self):
        for data, valid in ((b'{"op":"ready"}\n', True), (b'[]\n', False),
                            (b'{"op":"ready","op":"probe"}\n', False),
                            (b'{"op":"ready"}', False), (b'x' * 65, False)):
            left, right = socket.socketpair()
            with left, right:
                right.sendall(data)
                right.shutdown(socket.SHUT_WR)
                if valid:
                    self.assertEqual(session.read_message(left, 64), {"op": "ready"})
                else:
                    with self.assertRaises(ValueError):
                        session.read_message(left, 64)

    def test_kernel_socket_peer_pidfd_pins_the_actual_owned_process(self):
        left, right = socket.socketpair()
        with left, right:
            pid, uid, _gid = struct.unpack("3i", left.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            self.assertEqual((pid, uid), (os.getpid(), os.getuid()))
            descriptor = left.getsockopt(socket.SOL_SOCKET, getattr(socket, "SO_PEERPIDFD", 77))
            try:
                self.assertEqual(select.select([descriptor], [], [], 0)[0], [])
            finally:
                os.close(descriptor)

    def test_actual_unprivileged_fake_root_servers_are_refused_before_a_request(self):
        if os.geteuid() == 0:
            self.skipTest("requires unprivileged execution")
        for client in (lambda path: session.exchange(path, {"op": "ready"}),
                       lambda path: controls.root_probe_exchange(path, ["not sent"])):
            with tempfile.TemporaryDirectory() as temporary, socket.socket(socket.AF_UNIX) as server:
                path = Path(temporary) / "rpc"
                server.bind(str(path))
                server.listen(1)
                server.settimeout(2)
                received = []
                def accept():
                    connection, _ = server.accept()
                    with connection:
                        connection.settimeout(2)
                        received.append(connection.recv(100))
                thread = threading.Thread(target=accept)
                thread.start()
                try:
                    with self.assertRaises(PermissionError):
                        client(path)
                finally:
                    thread.join(timeout=3)
                self.assertFalse(thread.is_alive())
                self.assertEqual(received, [b""])


class CleanupModel(unittest.TestCase):
    state = Policy.state
    facts = Policy.facts
    def run_cleanup(self, state=None, fail=None, foreign=False, initial_vt=9):
        state = copy.deepcopy(state or self.state())
        calls = []
        current_vt = [initial_vt]
        alive = [state["session"] is not None]
        snapshots = []
        marker = mock.Mock()

        def properties(kind, name):
            if kind == "session":
                if not alive[0]:
                    return {}
                facts = self.facts()
                if foreign:
                    facts["User"] = "2000"
                return facts
            if name == state["user_unit"]:
                return dict(ActiveState="active" if alive[0] else "inactive", MainPID="123" if alive[0] else "0")
            return dict(ActiveState="active")

        def command(arguments, **_kwargs):
            calls.append(arguments)
            if fail and fail in arguments:
                raise RuntimeError("modeled host failure")
            if "terminate-session" in arguments:
                alive[0] = False
            if TOOLS["chvt"] in arguments:
                current_vt[0] = int(arguments[-1])
            return subprocess.CompletedProcess(arguments, 0, "", "")

        with tempfile.TemporaryDirectory() as temporary, \
                mock.patch.object(session, "require_root"), \
                mock.patch.object(session, "checked_runtime", return_value=Path(temporary)), \
                mock.patch.object(session, "reserved"), \
                mock.patch.object(session, "read_json", return_value=state), \
                mock.patch.object(session, "write_json", side_effect=lambda _path, value: snapshots.append(copy.deepcopy(value))), \
                mock.patch.object(session, "properties", side_effect=properties), \
                mock.patch.object(session, "active_vt", side_effect=lambda: current_vt[0]), \
                mock.patch.object(session, "sessions", return_value=[]), \
                mock.patch.object(session, "command", side_effect=command), \
                mock.patch.object(session, "MARKER", marker), \
                mock.patch("builtins.open", mock.mock_open()), \
                mock.patch.object(session.fcntl, "ioctl") as ioctl, \
                mock.patch("sys.stdout", new_callable=io.StringIO):
            code = session.cleanup(Path(temporary))
        return code, calls, snapshots, marker, ioctl

    def test_success_stops_only_owned_unit_session_and_restores_dm_vt(self):
        code, calls, snapshots, marker, ioctl = self.run_cleanup()
        self.assertEqual(code, 0)
        self.assertIn([TOOLS["systemctl"], "stop", "prek-owned.service"], calls)
        self.assertIn([TOOLS["loginctl"], "terminate-session", "c99"], calls)
        self.assertIn([TOOLS["chvt"], "1"], calls)
        self.assertIn([TOOLS["systemctl"], "start", "display-manager.service"], calls)
        self.assertEqual(snapshots[-1]["cleanup"]["status"], "passed")
        self.assertFalse(snapshots[-1]["cleanup"]["ssh_sessions_touched"])
        self.assertFalse(snapshots[-1]["cleanup"]["sudoers_modified"])
        marker.unlink.assert_called_once()
        self.assertEqual(ioctl.call_args.args[1:], (0x5608, 9))

    def test_every_prior_cleanup_failure_still_attempts_display_manager_restore(self):
        for failure in ("stop", "terminate-session", TOOLS["chvt"]):
            with self.subTest(failure=failure):
                code, calls, snapshots, marker, _ioctl = self.run_cleanup(fail=failure)
                self.assertEqual(code, 1)
                self.assertIn([TOOLS["systemctl"], "start", "display-manager.service"], calls)
                self.assertEqual(snapshots[-1]["cleanup"]["status"], "failed")
                marker.unlink.assert_not_called()

    def test_foreign_session_is_not_terminated_even_during_failure_cleanup(self):
        code, calls, _snapshots, _marker, _ioctl = self.run_cleanup(foreign=True)
        self.assertEqual(code, 1)
        self.assertFalse(any("terminate-session" in call for call in calls))
        self.assertIn([TOOLS["systemctl"], "start", "display-manager.service"], calls)

    def test_unowned_console_is_not_switched(self):
        code, calls, _snapshots, _marker, _ioctl = self.run_cleanup(initial_vt=8)
        self.assertEqual(code, 1)
        self.assertFalse(any(TOOLS["chvt"] in call for call in calls))

    def test_initially_inactive_dm_and_unstarted_user_are_left_untouched(self):
        state = self.state()
        state.update(session=None, leader=None, user_started=False, activated=False, dm=dict(ActiveState="inactive"))
        code, calls, _snapshots, marker, ioctl = self.run_cleanup(state, initial_vt=1)
        self.assertEqual(code, 0)
        self.assertFalse(any("start" in call or "terminate-session" in call or TOOLS["chvt"] in call for call in calls))
        ioctl.assert_not_called()
        marker.unlink.assert_called_once()

    def test_no_host_changes_when_guard_was_never_started(self):
        state = self.state()
        state["started"] = False
        code, calls, _snapshots, marker, ioctl = self.run_cleanup(state)
        self.assertEqual(code, 0)
        self.assertEqual(calls, [])
        ioctl.assert_not_called()
        marker.unlink.assert_called_once()

    def test_already_completed_cleanup_is_idempotent(self):
        state = self.state()
        state["cleanup"] = dict(status="passed")
        code, calls, snapshots, marker, ioctl = self.run_cleanup(state)
        self.assertEqual((code, calls, snapshots), (0, [], []))
        marker.unlink.assert_not_called()
        ioctl.assert_not_called()


class ProtocolModel(unittest.TestCase):
    def run_protocol(self, requests, *, probe_code=0):
        state = Policy().state()
        state.update(uid=os.getuid(), gid=os.getgid(), dm=dict(ActiveState="inactive"))
        facts = Policy().facts()
        facts.update(User=str(os.getuid()), Leader=str(os.getpid()), Active="yes")
        calls = []
        replies = []
        threads = []

        with tempfile.TemporaryDirectory() as temporary:
            runtime = Path(temporary)
            def client():
                try:
                    for request in requests:
                        with socket.socket(socket.AF_UNIX) as connection:
                            connection.settimeout(3)
                            connection.connect(str(runtime / "rpc"))
                            connection.sendall(json.dumps(request).encode() + b"\n")
                            reply = session.read_message(connection)
                            replies.append(reply)
                            if reply["code"] != 0 and request["op"] != "probe":
                                break
                except Exception as error:
                    replies.append(error)

            def command(arguments, **_kwargs):
                calls.append(arguments)
                if arguments[0] == TOOLS["systemd-run"]:
                    thread = threading.Thread(target=client)
                    threads.append(thread)
                    thread.start()
                if arguments[0] == "/fixed/native-probe":
                    return subprocess.CompletedProcess(arguments, probe_code, "explicit model output", "")
                return subprocess.CompletedProcess(arguments, 0, "", "")

            with mock.patch.object(session, "require_root"), \
                    mock.patch.object(session, "checked_runtime", return_value=runtime), \
                    mock.patch.object(session, "reserved"), \
                    mock.patch.object(session, "read_json", return_value=state), \
                    mock.patch.object(session, "write_json"), \
                    mock.patch.object(session, "check_free_vt"), \
                    mock.patch.object(session, "validate_peer"), \
                    mock.patch.object(session, "local_session", return_value=facts), \
                    mock.patch.object(session, "properties", side_effect=lambda kind, _name:
                                      facts if kind == "session" else dict(MainPID=str(os.getpid()), ActiveState="active")), \
                    mock.patch.object(session, "command", side_effect=command), \
                    mock.patch.object(session.os, "chown"), \
                    mock.patch.object(session.os, "chmod"), \
                    mock.patch.object(session.os, "readlink", side_effect=lambda path:
                                      "/owned/compositor" if "/123/" in path else "/owned/morf"), \
                    mock.patch.object(session.time, "monotonic", side_effect=iter(range(0, 10000, 10))), \
                    mock.patch("sys.stdout", new_callable=io.StringIO):
                arguments = mock.Mock(supervise=runtime, policy=Path("/fixed/policy"))
                try:
                    result = session.supervise(arguments, dict(probe="/fixed/native-probe"))
                except Exception as error:
                    result = error
                finally:
                    for thread in threads:
                        thread.join(timeout=4)
                        self.assertFalse(thread.is_alive())
            return result, replies, calls

    def probe(self):
        return dict(op="probe", bindings=["/tmp/pc-abc/r", str(os.getuid()), "abc_123", "/owned/compositor",
                                          "/owned/morf", "123", "456", "WAYLAND-1"])

    def test_ordered_model_runs_only_the_fixed_probe_once(self):
        result, replies, calls = self.run_protocol([dict(op="ready"), self.probe(), dict(op="done", code=0)])
        self.assertEqual(result, 0)
        self.assertEqual([reply["code"] for reply in replies], [0, 0, 0])
        probes = [call for call in calls if call[0] == "/fixed/native-probe"]
        self.assertEqual(probes, [["/fixed/native-probe", *self.probe()["bindings"]]])

    def test_missing_failed_duplicate_or_out_of_order_probe_never_passes(self):
        for requests, probe_code in (([dict(op="ready"), dict(op="done", code=0)], 0),
                                     ([self.probe()], 0),
                                     ([dict(op="ready"), self.probe(), self.probe()], 0),
                                     ([dict(op="ready"), self.probe(), dict(op="done", code=0)], 1),
                                     ([dict(op="ready"), self.probe(), dict(op="done", code=False)], 0)):
            with self.subTest(requests=requests, probe_code=probe_code):
                result, replies, calls = self.run_protocol(requests, probe_code=probe_code)
                self.assertIsInstance(result, Exception)
                self.assertTrue(replies)
                self.assertEqual(replies[-1]["code"], 125)
                self.assertLessEqual(len([call for call in calls if call[0] == "/fixed/native-probe"]), 1)

    def test_failed_child_without_probe_reports_failure_without_root_execution(self):
        result, replies, calls = self.run_protocol([dict(op="ready"), dict(op="done", code=125)])
        self.assertEqual(result, 125)
        self.assertEqual([reply["code"] for reply in replies], [0, 0])
        self.assertFalse(any(call[0] == "/fixed/native-probe" for call in calls))


if __name__ == "__main__":
    unittest.main()
