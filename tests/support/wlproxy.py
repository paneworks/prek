#!/usr/bin/env python3
"""A Wayland proxy that lets a client bind a newer xdg_wm_base than the
compositor offers.

    wlproxy.py LISTEN_SOCKET UPSTREAM_SOCKET [INTERFACE=VERSION ...]

cage 0.3 offers xdg_wm_base v5 and Hyprland's Wayland backend (aquamarine)
binds v6, so a nested Hyprland aborts on start. v6 only adds a toplevel
state (suspended) a compositor may simply never send, so advertising v6 to
the client and binding v5 upstream is sound. Everything else passes through
untouched, file descriptors included.

The rewrite needs two messages: wl_registry.global (event 0) carries the
advertised version, and wl_registry.bind (request 0) the requested one.
Registry objects are learnt from wl_display.get_registry (object 1,
request 1).
"""

import array
import os
import select
import socket
import struct
import sys

FDS_MAX = 28


def parse_string(data, offset):
    (length,) = struct.unpack_from("<I", data, offset)
    offset += 4
    text = data[offset:offset + length - 1].decode() if length else ""
    offset += (length + 3) & ~3
    return text, offset


class Connection:
    def __init__(self, client, upstream, advertise):
        self.client = client
        self.upstream = upstream
        self.advertise = advertise  # interface -> version the client sees
        self.registries = set()
        # name -> (interface, real version), per connection
        self.globals = {}
        self.buffers = {client: b"", upstream: b""}
        self.fds = {client: [], upstream: []}

    def other(self, sock):
        return self.upstream if sock is self.client else self.client

    def rewrite(self, sock, message):
        object_id, word = struct.unpack_from("<II", message)
        opcode = word & 0xFFFF
        if sock is self.client:
            # wl_display.get_registry(new_id)
            if object_id == 1 and opcode == 1:
                (registry,) = struct.unpack_from("<I", message, 8)
                self.registries.add(registry)
            # wl_registry.bind(name, interface, version, new_id)
            elif object_id in self.registries and opcode == 0:
                (name,) = struct.unpack_from("<I", message, 8)
                interface, offset = parse_string(message, 12)
                real = self.globals.get(name, (None, None))[1]
                (version,) = struct.unpack_from("<I", message, offset)
                if interface in self.advertise and real is not None and version > real:
                    message = bytearray(message)
                    struct.pack_into("<I", message, offset, real)
                    message = bytes(message)
        else:
            # wl_registry.global(name, interface, version)
            if object_id in self.registries and opcode == 0:
                (name,) = struct.unpack_from("<I", message, 8)
                interface, offset = parse_string(message, 12)
                (version,) = struct.unpack_from("<I", message, offset)
                self.globals[name] = (interface, version)
                wanted = self.advertise.get(interface)
                if wanted is not None and wanted > version:
                    message = bytearray(message)
                    struct.pack_into("<I", message, offset, wanted)
                    message = bytes(message)
        return message

    def pump(self, sock):
        fds = array.array("i")
        try:
            data, ancillary, _, _ = sock.recvmsg(65536, socket.CMSG_LEN(FDS_MAX * fds.itemsize))
        except ConnectionResetError:
            return False
        if not data and not ancillary:
            return False
        for level, kind, payload in ancillary:
            if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                fds.frombytes(payload[: len(payload) - (len(payload) % fds.itemsize)])
        self.fds[sock].extend(fds)
        self.buffers[sock] += data
        out = b""
        buffer = self.buffers[sock]
        while len(buffer) >= 8:
            (_, word) = struct.unpack_from("<II", buffer)
            size = word >> 16
            if size < 8 or len(buffer) < size:
                break
            out += self.rewrite(sock, buffer[:size])
            buffer = buffer[size:]
        self.buffers[sock] = buffer
        if out or self.fds[sock]:
            pending = self.fds[sock]
            self.fds[sock] = []
            target = self.other(sock)
            ancillary = [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", pending))] if pending else []
            target.sendmsg([out], ancillary)
            for fd in pending:
                os.close(fd)
        return True


def main():
    listen_path, upstream_path = sys.argv[1], sys.argv[2]
    advertise = {"xdg_wm_base": 6}
    for spec in sys.argv[3:]:
        interface, version = spec.split("=")
        advertise[interface] = int(version)
    if os.path.exists(listen_path):
        os.unlink(listen_path)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(listen_path)
    server.listen(8)
    connections = {}
    while True:
        readable, _, _ = select.select([server, *connections.keys()], [], [])
        for sock in readable:
            if sock is server:
                client, _ = server.accept()
                upstream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                upstream.connect(upstream_path)
                connection = Connection(client, upstream, advertise)
                connections[client] = connection
                connections[upstream] = connection
                continue
            connection = connections.get(sock)
            if connection is None:
                continue
            if not connection.pump(sock):
                for end in (connection.client, connection.upstream):
                    connections.pop(end, None)
                    end.close()


if __name__ == "__main__":
    main()
