"""Real ext-session-lock test client, restricted to the disposable VM.

The VM turns DPMS off before launch so no lock-surface renderer is needed.
This fixture acknowledges the actual compositor event before requesting unlock.
Protocol: wayland-protocols/staging/ext-session-lock/ext-session-lock-v1.xml.
"""
import os
from pathlib import Path
import socket
import struct
import time

assert os.environ.get("PREK_COMPOSITOR_VM") == "1"
runtime = Path(os.environ["XDG_RUNTIME_DIR"])
display = os.environ["WAYLAND_DISPLAY"]
assert runtime == Path("/run/user/1000") and "/" not in display
wire = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
wire.settimeout(10)
wire.connect(str(runtime / display))


def request(object_id, opcode, payload=b""):
    wire.sendall(struct.pack("=II", object_id, ((len(payload) + 8) << 16) | opcode) + payload)


def exact(length):
    result = b""
    while len(result) < length:
        received = wire.recv(length - len(result))
        assert received, "compositor disconnected"
        result += received
    return result


def event():
    object_id, header = struct.unpack("=II", exact(8))
    size, opcode = header >> 16, header & 0xFFFF
    assert 8 <= size <= 65536 and size % 4 == 0
    payload = exact(size - 8)
    assert (object_id, opcode) != (1, 0), "Wayland protocol error: " + repr(payload)
    return object_id, opcode, payload


def string(value):
    raw = value.encode() + b"\0"
    return struct.pack("=I", len(raw)) + raw + b"\0" * (-len(raw) % 4)


request(1, 1, struct.pack("=I", 2))  # wl_display.get_registry
request(1, 0, struct.pack("=I", 3))  # wl_display.sync
manager = None
while True:
    object_id, opcode, payload = event()
    if object_id == 2 and opcode == 0:
        name, length = struct.unpack("=II", payload[:8])
        interface = payload[8:8 + length - 1].decode()
        if interface == "ext_session_lock_manager_v1":
            manager = name
    if object_id == 3 and opcode == 0:
        break
assert manager is not None, "session-lock protocol unavailable"
request(2, 0, struct.pack("=I", manager) + string("ext_session_lock_manager_v1") + struct.pack("=II", 1, 4))
request(4, 1, struct.pack("=I", 5))
while True:
    object_id, opcode, payload = event()
    if object_id == 5:
        assert opcode == 0, "lock was denied"
        break
(runtime / "prek-lock-acknowledged").write_text("locked")
deadline = time.monotonic() + 120
while not (runtime / "prek-lock-unlock").exists():
    assert time.monotonic() < deadline, "unlock fixture instruction missing"
    time.sleep(0.01)
request(5, 2)  # ext_session_lock_v1.unlock_and_destroy
request(1, 0, struct.pack("=I", 6))
while True:
    object_id, opcode, payload = event()
    if object_id == 6 and opcode == 0:
        break
(runtime / "prek-lock-completed").write_text("unlocked")
