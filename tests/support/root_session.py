"""User-launched, bounded local PAM session for the SSH-only handoff fixture."""
import argparse
import array
import fcntl
import json
import os
from pathlib import Path
import pwd
import re
import select
import socket
import stat
import struct
import subprocess
import sys
import time
import uuid


ENV = {"PATH": "/run/wrappers/bin:/run/current-system/sw/bin:/usr/sbin:/usr/bin:/sbin:/bin", "LANG": "C.UTF-8"}
TOOLS = {}
MARKER = Path("/run/prek-root-handoff-active")
MAX_MESSAGE = 1024 * 1024
VT = 9


def configure_tools(policy):
    tools = policy["tools"]
    if (not isinstance(tools, dict) or set(tools) != {"systemctl", "loginctl", "systemd-run", "journalctl", "chvt"}
            or any(not isinstance(path, str) or not Path(path).is_absolute() or "\x00" in path
                   or ".." in Path(path).parts for path in tools.values())):
        raise ValueError("root-session tools must be fixed absolute paths from the Nix policy")
    TOOLS.clear()
    TOOLS.update(tools)


def tool(name):
    return TOOLS[name]


def log(message):
    try:
        print(message, flush=True)
    except OSError:
        sys.stdout = open(os.devnull, "w")


def require_root():
    if os.geteuid() != 0:
        raise PermissionError("launch the wrapper as your regular user; it requests sudo itself")


def read_message(connection, limit=MAX_MESSAGE):
    data = bytearray()
    while not data.endswith(b"\n"):
        chunk = connection.recv(min(4096, limit + 1 - len(data)))
        if not chunk:
            raise ValueError("incomplete test transport message")
        data.extend(chunk)
        if len(data) > limit:
            raise ValueError("test transport message exceeds its bound")
    def unique(pairs):
        value = dict(pairs)
        if len(value) != len(pairs):
            raise ValueError("duplicate test transport field")
        return value
    value = json.loads(data, object_pairs_hook=unique)
    if not isinstance(value, dict):
        raise ValueError("test transport message must be an object")
    return value


def exchange(path, message):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(40)
        connection.connect(str(path))
        _pid, uid, _gid = struct.unpack("3i", connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
        if uid != 0:
            raise PermissionError("test coordinator is not root")
        connection.sendall(json.dumps(message).encode() + b"\n")
        reply = read_message(connection)
        if type(reply.get("code")) is not int or reply["code"] != 0:
            raise RuntimeError(reply.get("stdout", "test coordinator rejected request"))
        return reply


def command(arguments, *, check=True, timeout=20):
    result = subprocess.run(arguments, env=ENV, capture_output=True, text=True, timeout=timeout)
    if len(result.stdout) + len(result.stderr) > MAX_MESSAGE:
        raise ValueError("host command output exceeded its bound")
    if check and result.returncode:
        raise RuntimeError(f"{arguments[0]} failed ({result.returncode}): {result.stderr.strip()}")
    return result


def properties(kind, name):
    executable = tool("loginctl" if kind == "session" else "systemctl")
    result = command([executable, "show-session" if kind == "session" else "show", name], check=False, timeout=5)
    if result.returncode:
        if "not found" in result.stderr or "could not be found" in result.stderr or "No session" in result.stderr:
            return {}
        raise RuntimeError(result.stderr.strip())
    return dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)


def sessions():
    result = json.loads(command([tool("loginctl"), "list-sessions", "--json=short"], timeout=5).stdout)
    if not isinstance(result, list) or len(result) > 4096:
        raise ValueError("invalid logind session inventory")
    return result


def validate_session(facts, state, leader):
    expected = {"User": str(state["uid"]), "Leader": str(leader), "Seat": "seat0",
                "VTNr": str(VT), "TTY": f"tty{VT}", "Remote": "no", "Type": "tty", "Service": "login"}
    if any(facts.get(key) != value for key, value in expected.items()):
        raise PermissionError("PAM session does not match the owned user, leader and VT")
    if facts.get("Class") not in ("user", "user-early", "user-light", "user-early-light"):
        raise PermissionError("PAM session is not a user session")
    identifier = facts.get("Id")
    if type(identifier) is not str or not re.fullmatch(r"[A-Za-z0-9]{1,64}", identifier):
        raise PermissionError("PAM session has no bounded logind identifier")
    if facts.get("Scope") != f"session-{identifier}.scope":
        raise PermissionError("PAM session has no bounded logind scope")
    return facts


def local_session(state, leader):
    matches = [row for row in sessions() if row.get("leader") == leader and row.get("uid") == state["uid"]]
    if len(matches) != 1:
        raise PermissionError("the owned service has no unique actual logind session")
    return validate_session(properties("session", matches[0]["session"]), state, leader)


def validate_peer(pid, uid, state):
    if uid != state["uid"]:
        raise PermissionError("test client UID does not match the launch user")
    status = Path(f"/proc/{pid}/status").read_text()
    actual = next(line.split()[1:] for line in status.splitlines() if line.startswith("Uid:"))
    if actual != [str(uid)] * 4:
        raise PermissionError("test client changed credentials")
    groups = Path(f"/proc/{pid}/cgroup").read_text().splitlines()
    if not any(row.startswith("0::/") and state["scope"] in Path(row[3:]).parts for row in groups):
        raise PermissionError("test client is outside the owned PAM session scope")


def validate_bindings(bindings, state):
    if (not isinstance(bindings, list) or len(bindings) != 8
            or any(type(value) is not str or not value or len(value) > 4096
                   or any(ord(char) < 32 for char in value) for value in bindings)):
        raise ValueError("native probe needs exactly eight bounded string bindings")
    if not re.fullmatch(r"/tmp/pc-[A-Za-z0-9_-]+/r", bindings[0]) or bindings[1] != str(state["uid"]):
        raise PermissionError("native runtime or UID is outside the owned fixture")
    if bindings[4] != state["child"]["morf"]:
        raise PermissionError("native producer executable differs from the launch policy")
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,200}", bindings[2]) or not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", bindings[7]):
        raise ValueError("invalid compositor instance or output binding")
    for value in (bindings[5], bindings[6]):
        if not value.isascii() or not value.isdigit() or not 0 < int(value) <= 2**31 - 1 or str(int(value)) != value:
            raise ValueError("invalid owned process PID")
        validate_peer(int(value), state["uid"], state)
    if os.readlink(f"/proc/{bindings[5]}/exe") != bindings[3]:
        raise PermissionError("compositor executable differs from the actual process")
    if os.readlink(f"/proc/{bindings[6]}/exe") != bindings[4]:
        raise PermissionError("producer executable differs from the actual process")
    return bindings


def checked_runtime(path):
    path = Path(path)
    if not re.fullmatch(r"/run/prek-root-handoff-[0-9a-f]{32}", str(path)):
        raise ValueError("invalid private root test runtime")
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
        raise PermissionError("root test runtime is not protected")
    return path


def reserved(runtime):
    descriptor = os.open(MARKER, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    with os.fdopen(descriptor) as file:
        info = os.fstat(file.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_nlink != 1 or info.st_mode & 0o077:
            raise PermissionError("test reservation is not a private root-owned file")
        if file.read(513) != str(runtime) + "\n":
            raise PermissionError("root test reservation no longer belongs to this run")


def read_json(path, *, group_readable=False):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    with os.fdopen(descriptor) as file:
        info = os.fstat(file.fileno())
        forbidden = 0o027 if group_readable else 0o077
        if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_nlink != 1 or info.st_mode & forbidden:
            raise PermissionError("test state is not a private root-owned regular file")
        data = file.read(65537)
        if len(data) > 65536:
            raise ValueError("test state exceeds its bound")
        return json.loads(data)


def write_json(path, value, *, gid=None):
    temporary = path.with_name(path.name + ".new-" + uuid.uuid4().hex)
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    with os.fdopen(descriptor, "w") as file:
        if gid is not None:
            os.fchown(file.fileno(), 0, gid)
            os.fchmod(file.fileno(), 0o640)
        json.dump(value, file)
        file.write("\n")
        file.flush()
        os.fsync(file.fileno())
    os.replace(temporary, path)


def active_vt():
    value = Path("/sys/class/tty/tty0/active").read_text().strip()
    if not re.fullmatch(r"tty[1-9][0-9]?", value) or not 1 <= int(value[3:]) <= 63:
        raise ValueError("cannot identify original kernel VT")
    return int(value[3:])


def check_free_vt():
    with open("/dev/tty0", "rb", buffering=0) as console:
        state = array.array("H", [0, 0, 0])
        fcntl.ioctl(console.fileno(), 0x5603, state, True)
    if state[2] & (1 << VT) or active_vt() == VT:
        raise RuntimeError("tty9 is already allocated; refusing to disturb another console")
    if properties("unit", f"getty@tty{VT}.service").get("ActiveState") not in (None, "inactive", "failed"):
        raise RuntimeError("tty9 has a getty; refusing to stop it")
    if any(row.get("seat") == "seat0" and row.get("class") != "greeter" for row in sessions()):
        raise RuntimeError("another local user session exists; refusing to terminate it")


def service_command(unit, arguments, properties_list, *, wait=False):
    result = [tool("systemd-run"), f"--unit={unit}", "--service-type=exec"]
    if wait:
        result += ["--wait", "--pipe", "--collect"]
    result += [f"--property={value}" for value in properties_list]
    return result + ["--"] + arguments


def invocation(policy_path, mode, runtime):
    return [sys.executable, str(Path(__file__).resolve()), "--policy", str(policy_path), mode, str(runtime)]


def user_properties(state):
    return [f"User={state['uid']}", f"Group={state['gid']}", "PAMName=login",
            f"TTYPath=/dev/tty{VT}", "StandardInput=tty-fail", "TTYReset=yes",
            "StandardOutput=journal", "StandardError=journal", "RuntimeMaxSec=200",
            "TimeoutStopSec=15", "KillMode=control-group", "CollectMode=inactive-or-failed",
            "Environment=XDG_SESSION_TYPE=tty XDG_SESSION_CLASS=user XDG_SEAT=seat0 XDG_VTNR=9"]


def bootstrap(args, policy):
    require_root()
    uid = int(os.environ.get("SUDO_UID", "0"))
    if uid <= 0:
        raise PermissionError("invoke the regular-user launcher, not a root shell")
    account = pwd.getpwuid(uid)
    check_free_vt()
    token = uuid.uuid4().hex
    runtime = Path(f"/run/prek-root-handoff-{token}")
    state = dict(version=1, uid=uid, gid=account.pw_gid, name=account.pw_name, home=account.pw_dir,
                 original_vt=active_vt(), vt=VT, user_unit=f"prek-handoff-user-{token}.service",
                 control_unit=f"prek-handoff-{token}.service", session=None, scope=None, leader=None,
                 dm=dict(ActiveState=properties("unit", "display-manager.service").get("ActiveState")), started=False,
                 user_started=False, activated=False, cleanup=None,
                 child=dict(morf=str(Path(args.morf).resolve(strict=True)),
                            shell=str(Path(args.morf_shell).resolve(strict=True)),
                            repo=str(Path(args.morf_repo).resolve(strict=True)),
                            nix=str(Path(args.nix).resolve(strict=True)), output=str(Path(args.output).resolve()),
                            nvidia=args.nvidia_version))
    runtime.mkdir(mode=0o711)
    os.chmod(runtime, 0o711)
    write_json(runtime / "state.json", state)
    write_json(runtime / "child.json", dict(state, socket=str(runtime / "rpc")), gid=state["gid"])
    marker = os.open(MARKER, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    with os.fdopen(marker, "w") as file:
        file.write(str(runtime) + "\n")
        file.flush()
        os.fsync(file.fileno())
    log(f"Root session guard: {state['control_unit']}; private state: {runtime}")
    cleanup_command = " ".join(invocation(args.policy, "--cleanup", runtime))
    try:
        call = service_command(state["control_unit"], invocation(args.policy, "--supervise", runtime),
                               ["RuntimeMaxSec=240", "TimeoutStopSec=120", "KillMode=control-group",
                                f"ExecStopPost={cleanup_command}"], wait=True)
        result = subprocess.run(call, env=ENV)
    finally:
        final = read_json(runtime / "state.json")
        if not final.get("cleanup") and properties("unit", state["control_unit"]).get("ActiveState") not in (
                "active", "activating", "deactivating"):
            cleanup(runtime)
    final = read_json(runtime / "state.json")
    log("PREK_ROOT_SESSION_RECEIPT=" + json.dumps(final.get("cleanup")))
    return 0 if result.returncode == 0 and (final.get("cleanup") or {}).get("status") == "passed" else 1


def cleanup(runtime):
    require_root()
    runtime = checked_runtime(runtime)
    state = read_json(runtime / "state.json")
    if isinstance(state.get("cleanup"), dict) and state["cleanup"].get("status") == "passed":
        return 0
    reserved(runtime)
    errors = []

    def attempt(name, action):
        try:
            action()
            return True
        except Exception as error:
            errors.append(f"{name}: {error}")
            return False

    if state["started"]:
        if state["session"] is None:
            leader = int(properties("unit", state["user_unit"]).get("MainPID", "0"))
            if leader:
                def recover_session():
                    facts = local_session(state, leader)
                    state.update(session=facts["Id"], scope=facts["Scope"], leader=leader)
                attempt("inspect owned PAM session", recover_session)

        def stop_user():
            if properties("unit", state["user_unit"]).get("ActiveState") not in (None, "inactive", "failed"):
                command([tool("systemctl"), "stop", state["user_unit"]], timeout=15)
        attempt("stop owned user unit", stop_user)

        def stop_session():
            if state["session"] is not None:
                facts = properties("session", state["session"])
                if facts:
                    validate_session(facts, state, state["leader"])
                    command([tool("loginctl"), "terminate-session", state["session"]], timeout=10)
                    until = time.monotonic() + 5
                    while properties("session", state["session"]):
                        if time.monotonic() >= until:
                            raise RuntimeError("owned PAM session did not retire within its bound")
                        time.sleep(.05)
        retired = attempt("stop owned PAM session", stop_session)

        def restore_vt():
            current = active_vt()
            if current == VT and state["activated"]:
                command([tool("chvt"), str(state["original_vt"])], timeout=5)
            elif current != state["original_vt"]:
                raise RuntimeError("console changed outside the test; leaving the new VT untouched")
        attempt("restore original VT", restore_vt)

        def release_vt():
            if not state["user_started"]:
                return
            if not retired:
                raise RuntimeError("PAM session retirement was not confirmed; refusing to deallocate its VT")
            if active_vt() == VT:
                raise RuntimeError("owned VT is still active; refusing to deallocate it")
            if any(row.get("tty") == f"tty{VT}" for row in sessions()):
                raise RuntimeError("owned VT still has a logind session; refusing to deallocate it")
            with open("/dev/tty0", "rb", buffering=0) as console:
                fcntl.ioctl(console.fileno(), 0x5608, VT)
        attempt("release owned VT", release_vt)

        def restore_dm():
            if state["dm"].get("ActiveState") == "active":
                command([tool("systemctl"), "start", "display-manager.service"], timeout=25)
                if properties("unit", "display-manager.service").get("ActiveState") != "active":
                    raise RuntimeError("display manager did not become active again")
        attempt("restore display manager", restore_dm)

        try:
            journal = command([tool("journalctl"), "--no-pager", "-o", "cat", "-n", "2000",
                               "-u", state["user_unit"]], check=False, timeout=3)
            log(journal.stdout)
        except Exception as error:
            log(f"Owned user journal unavailable: {error}")

    socket_path = runtime / "rpc"
    if socket_path.exists():
        socket_path.unlink()
    state["cleanup"] = dict(status="failed" if errors else "passed", errors=errors,
                            display_manager_was_active=state["dm"].get("ActiveState") == "active",
                            original_vt=state["original_vt"], owned_session=state["session"],
                            ssh_sessions_touched=False, sudoers_modified=False)
    write_json(runtime / "state.json", state)
    log("Root session cleanup: " + json.dumps(state["cleanup"]))
    if not errors:
        MARKER.unlink()
    return 1 if errors else 0


def supervise(args, policy):
    require_root()
    runtime = checked_runtime(args.supervise)
    state = read_json(runtime / "state.json")
    reserved(runtime)
    check_free_vt()
    state["started"] = True
    write_json(runtime / "state.json", state)
    if state["dm"].get("ActiveState") == "active":
        log("Stopping display-manager.service for this test; ExecStopPost will restore it.")
        command([tool("systemctl"), "stop", "display-manager.service"])
    ready = False
    probed = False
    probe_code = None
    with socket.socket(socket.AF_UNIX) as server:
        server.bind(str(runtime / "rpc"))
        os.chown(runtime / "rpc", 0, state["gid"])
        os.chmod(runtime / "rpc", 0o660)
        server.listen(4)
        server.settimeout(.3)
        user_call = service_command(state["user_unit"], invocation(args.policy, "--child", runtime), user_properties(state))
        state["user_started"] = True
        write_json(runtime / "state.json", state)
        command(user_call)
        leader = int(properties("unit", state["user_unit"]).get("MainPID", "0"))
        log(f"Owned PAM service: {state['user_unit']}; leader={leader}; tty{VT}")
        deadline = time.monotonic() + 200
        while time.monotonic() < deadline:
            try:
                connection, _address = server.accept()
            except socket.timeout:
                if properties("unit", state["user_unit"]).get("ActiveState") in (None, "inactive", "failed"):
                    raise RuntimeError("owned user fixture exited without its completion report")
                continue
            with connection:
                connection.settimeout(5)
                pid, uid, _gid = struct.unpack("3i", connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
                peer = connection.getsockopt(socket.SOL_SOCKET, getattr(socket, "SO_PEERPIDFD", 77))
                os.set_inheritable(peer, False)
                try:
                    if uid != state["uid"] or select.select([peer], [], [], 0)[0]:
                        raise PermissionError("test client credentials or process lifetime changed")
                    request = read_message(connection, 8192)
                    if request == {"op": "ready"} and not ready and pid == leader:
                        facts = local_session(state, leader)
                        state.update(session=facts["Id"], scope=facts["Scope"], leader=leader)
                        state["activated"] = True
                        write_json(runtime / "state.json", state)
                        command([tool("loginctl"), "activate", state["session"]])
                        until = time.monotonic() + 5
                        while properties("session", state["session"]).get("Active") != "yes":
                            if time.monotonic() >= until:
                                raise RuntimeError("owned logind session did not become active")
                            time.sleep(.05)
                        ready = True
                        log(f"Actual local logind session {state['session']} active on tty{VT}.")
                        reply = dict(code=0)
                    else:
                        if not ready:
                            raise PermissionError("test client requested work before local session activation")
                        validate_peer(pid, uid, state)
                        if set(request) == {"op", "bindings"} and request["op"] == "probe" and not probed:
                            bindings = validate_bindings(request["bindings"], state)
                            probed = True
                            result = command([policy["probe"], *bindings], check=False, timeout=35)
                            probe_code = result.returncode if 0 <= result.returncode <= 255 else 125
                            reply = dict(code=probe_code, stdout=result.stdout + result.stderr)
                            log(f"Fixed native root probe exit: {probe_code}")
                        elif set(request) == {"op", "code"} and request["op"] == "done" and pid == leader:
                            if type(request["code"]) is not int or not 0 <= request["code"] <= 255:
                                raise ValueError("invalid child completion code")
                            code = request["code"]
                            if code == 0 and (not probed or probe_code != 0):
                                raise RuntimeError("fixture reported success without one successful native probe")
                            connection.sendall(b'{"code":0}\n')
                            journal = command([tool("journalctl"), "--no-pager", "-o", "cat", "-n", "2000",
                                               "-u", state["user_unit"]], check=False)
                            log(journal.stdout)
                            return code
                        else:
                            raise ValueError("unknown, duplicated or out-of-order test request")
                    connection.sendall(json.dumps(reply).encode() + b"\n")
                except Exception as error:
                    connection.sendall(json.dumps(dict(code=125, stdout=str(error))).encode() + b"\n")
                    raise
                finally:
                    os.close(peer)
    raise TimeoutError("owned local handoff session exceeded its lifetime")


def fixture_command(config, policy, *, nixos):
    wrappers = [] if nixos else ["nixGL", "nixVulkan"]
    return [config["nix"], "--option", "eval-cache", "false", "--extra-experimental-features",
            "nix-command flakes", "--max-jobs", "1", "--cores", "2", "develop", "--impure",
            config["repo"], "--no-write-lock-file", "-c", *wrappers, policy["fixture"],
            "--hardware-rendering", "--modes", "--input", "--power", "--morf", config["morf"],
            "--morf-shell", config["shell"], "--output", config["output"],
            "--morf-root-probe-socket", config["socket"]]


def child(args, policy):
    runtime = checked_runtime(args.child)
    state = read_json(runtime / "child.json", group_readable=True)
    if os.geteuid() != state["uid"] or os.geteuid() == 0:
        raise PermissionError("PAM fixture must run as the launch user")
    exchange(state["socket"], dict(op="ready"))
    config = state["child"]
    environment = dict(ENV, HOME=state["home"], USER=state["name"], LOGNAME=state["name"])
    nixos = Path("/etc/NIXOS").exists()
    if config["nvidia"] and not nixos:
        environment["NVIDIA_VERSION"] = config["nvidia"]
    code = 125
    try:
        for mode in ("--check-library", "--preflight"):
            subprocess.run([policy["probe"], mode], env=environment, check=True, timeout=10)
        call = fixture_command(dict(config, socket=state["socket"]), policy, nixos=nixos)
        result = subprocess.run(call, env=environment, timeout=170)
        code = result.returncode if 0 <= result.returncode <= 255 else 125
    finally:
        exchange(state["socket"], dict(op="done", code=code))
    return code


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--policy", required=True, type=Path)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--bootstrap", action="store_true")
    for name in ("supervise", "child", "cleanup"):
        mode.add_argument(f"--{name}", type=Path)
    for name in ("morf", "morf-shell", "morf-repo", "nix", "output", "nvidia-version"):
        parser.add_argument(f"--{name}")
    args = parser.parse_args()
    if not args.child:
        require_root()
    policy = json.loads(args.policy.read_text())
    configure_tools(policy)
    if args.bootstrap:
        if any(getattr(args, name) is None for name in ("morf", "morf_shell", "morf_repo", "nix", "output")):
            parser.error("bootstrap requires explicit user fixture paths")
        return bootstrap(args, policy)
    if args.cleanup:
        return cleanup(args.cleanup)
    if args.supervise:
        return supervise(args, policy)
    return child(args, policy)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        log(f"Root session fixture failed: {error}")
        sys.exit(1)
