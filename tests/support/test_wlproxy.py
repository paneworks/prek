"""Protocol checks for the owned nested compositor's parent-input boundary."""
import struct
import unittest
from wlproxy import Connection, parse_string


def string(value):
    data = value.encode() + b"\0"
    return struct.pack("<I", len(data)) + data + b"\0" * (-len(data) % 4)


def message(object_id, opcode, body):
    return struct.pack("<II", object_id, ((8 + len(body)) << 16) | opcode) + body


class ParentInput(unittest.TestCase):
    def connection(self, isolate):
        client, upstream = object(), object()
        connection = Connection(client, upstream, {"xdg_wm_base": 6}, isolate)
        connection.rewrite(client, message(1, 1, struct.pack("<I", 2)))
        connection.rewrite(upstream, message(2, 0, struct.pack("<I", 10) + string("wl_seat") + struct.pack("<I", 7)))
        connection.rewrite(client, message(2, 0, struct.pack("<I", 10) + string("wl_seat") + struct.pack("<II", 7, 3)))
        return connection, client, upstream

    def test_only_the_owned_connection_parent_seat_is_masked(self):
        connection, _, upstream = self.connection(True)
        capabilities = message(3, 0, struct.pack("<I", 7))
        self.assertEqual(struct.unpack_from("<I", connection.rewrite(upstream, capabilities), 8)[0], 2)
        name = message(3, 1, string("seat0"))
        self.assertEqual(connection.rewrite(upstream, name), name)
        unrelated = message(99, 0, struct.pack("<I", 7))
        self.assertEqual(connection.rewrite(upstream, unrelated), unrelated)
        ordinary, _, other = self.connection(False)
        self.assertEqual(ordinary.rewrite(other, capabilities), capabilities)

    def test_keyboard_focus_and_keymap_survive_but_parent_keys_and_modifiers_do_not(self):
        connection, client, upstream = self.connection(True)
        connection.rewrite(client, message(3, 1, struct.pack("<I", 4)))
        keymap = message(4, 0, struct.pack("<II", 1, 1024))
        self.assertEqual(connection.rewrite(upstream, keymap), keymap)
        key = message(4, 3, struct.pack("<IIII", 1, 99, 57, 1))
        self.assertEqual(connection.rewrite(upstream, key), b"")
        enter = message(4, 1, struct.pack("<IIIII", 1, 9, 8, 42, 57))
        self.assertEqual(connection.rewrite(upstream, enter), message(4, 1, struct.pack("<III", 1, 9, 0)))
        modifiers = message(4, 4, struct.pack("<IIIII", 1, 1, 2, 4, 3))
        self.assertEqual(connection.rewrite(upstream, modifiers), message(4, 4, struct.pack("<IIIII", 1, 0, 0, 0, 0)))
        connection.rewrite(upstream, message(1, 1, struct.pack("<I", 4)))
        self.assertEqual(connection.rewrite(upstream, key), key)

    def test_parent_pointer_and_touch_interaction_objects_are_suppressed(self):
        connection, client, upstream = self.connection(True)
        for operation, device in ((0, 4), (2, 5)):
            connection.rewrite(client, message(3, operation, struct.pack("<I", device)))
            event = message(device, 0, struct.pack("<I", 7))
            self.assertEqual(connection.rewrite(upstream, event), b"")
            connection.rewrite(upstream, message(1, 1, struct.pack("<I", device)))
            self.assertEqual(connection.rewrite(upstream, event), event)

    def test_deleted_ids_do_not_mask_replacement_interfaces(self):
        connection, _, upstream = self.connection(True)
        connection.rewrite(upstream, message(1, 1, struct.pack("<I", 3)))
        unrelated = message(3, 0, struct.pack("<I", 7))
        self.assertEqual(connection.rewrite(upstream, unrelated), unrelated)

    def test_isolation_preserves_the_xdg_version_bridge(self):
        connection, client, upstream = self.connection(True)
        global_event = message(2, 0, struct.pack("<I", 11) + string("xdg_wm_base") + struct.pack("<I", 5))
        advertised = connection.rewrite(upstream, global_event)
        _, offset = parse_string(advertised, 12)
        self.assertEqual(struct.unpack_from("<I", advertised, offset)[0], 6)
        bind = message(2, 0, struct.pack("<I", 11) + string("xdg_wm_base") + struct.pack("<II", 6, 5))
        forwarded = connection.rewrite(client, bind)
        self.assertEqual(struct.unpack_from("<II", forwarded, offset), (5, 5))


if __name__ == "__main__":
    unittest.main()
