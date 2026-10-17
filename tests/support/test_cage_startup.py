import importlib.util
from pathlib import Path
import unittest


spec = importlib.util.spec_from_file_location("cage_inner", Path(__file__).parents[1] / "cage-inner.py")
inner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inner)

host_spec = importlib.util.spec_from_file_location("cage_host", Path(__file__).parents[1] / "cage-host.py")
host = importlib.util.module_from_spec(host_spec)
host_spec.loader.exec_module(host)


class RenderedState(unittest.TestCase):
    def test_initial_or_unversioned_frame_does_not_acknowledge_later_input(self):
        for rows in ([], [{"event": "rendered"}],
                     [{"event": "rendered", "drawn_sequence": 3, "sequence": 20}]):
            with self.subTest(rows=rows):
                self.assertFalse(host.rendered_through(rows, 10))

    def test_current_callback_state_does_not_replace_the_submitted_watermark(self):
        self.assertFalse(host.rendered_through([
            {"event": "key", "sequence": 10},
            {"event": "rendered", "sequence": 11, "drawn_sequence": 9, "text": "bA"}], 10))

    def test_acknowledgement_can_cover_a_coalesced_state(self):
        for drawn in (10, 12):
            with self.subTest(drawn=drawn):
                self.assertTrue(host.rendered_through([
                    {"event": "rendered", "drawn_sequence": drawn}], 10))

    def test_non_frame_receipt_cannot_acknowledge_a_draw(self):
        self.assertFalse(host.rendered_through([
            {"event": "key", "drawn_sequence": 20}], 10))


class ConfigProvider(unittest.TestCase):
    def test_each_config_has_its_own_extension_and_deterministic_output(self):
        name, source = inner.configuration("lua")
        self.assertEqual(name, "hyprland.lua")
        self.assertIn('hl.monitor({output="",mode="1280x720@60"', source)
        self.assertIn('animations={enabled=false}', source)
        name, source = inner.configuration("hyprlang")
        self.assertEqual(name, "hyprland.conf")
        self.assertIn("monitor = , 1280x720@60", source)
        self.assertNotIn("hl.monitor", source)

    def test_focus_dialect_never_uses_current_focus_or_interpolated_selectors(self):
        self.assertEqual(host.focus_arguments("hyprlang", "0x1234"),
                         ["dispatch", "focuswindow", "address:0x1234"])
        self.assertEqual(host.focus_arguments("lua", "0x1234"),
                         ["dispatch", 'hl.dsp.focus({window="address:0x1234"})'])
        for value in ("active", "0x0", "0x1;exit", '0x1"', "0x10000000000000000"):
            for provider in ("lua", "hyprlang"):
                with self.subTest(value=value, provider=provider), self.assertRaises(ValueError):
                    host.focus_arguments(provider, value)

    def test_unknown_provider_does_not_fall_back_to_legacy(self):
        with self.assertRaises(ValueError):
            inner.configuration("other")
        with self.assertRaises(ValueError):
            host.focus_arguments("other", "0x1234")


class OutputReadiness(unittest.TestCase):
    def test_missing_zero_negative_and_disabled_outputs_are_unusable(self):
        for monitors in (None, [], [{}], [{"width": 0, "height": 720}],
                         [{"width": 1280, "height": -1}],
                         [{"width": 1280, "height": 720, "disabled": True}]):
            with self.subTest(monitors=monitors):
                self.assertFalse(inner.usable_outputs(monitors))

    def test_positive_active_outputs_are_usable(self):
        self.assertTrue(inner.usable_outputs([{"width": 1280, "height": 720}]))
        self.assertTrue(inner.usable_outputs([
            {"width": 1280, "height": 720}, {"disabled": True}]))

    def test_one_valid_output_does_not_hide_another_invalid_active_output(self):
        self.assertFalse(inner.usable_outputs([
            {"width": 1280, "height": 720}, {"width": 0, "height": 0}]))


class LibraryProvenance(unittest.TestCase):
    def mapping(self, path):
        return f"7f000-7f100 r-xp 00000 08:01 123 {path}\n"

    def test_stock_capture_does_not_claim_a_declared_override(self):
        library = "/nix/store/stock-aquamarine/lib/libaquamarine.so.10.1.0"
        self.assertEqual(inner.aquamarine_libraries(self.mapping(library)),
                         dict(declared_test_override=None, loaded_libraries=[library]))

    def test_exact_override_is_required_and_duplicate_mappings_are_coalesced(self):
        package = "/nix/store/test-aquamarine"
        library = f"{package}/lib/libaquamarine.so.10.1.0"
        self.assertEqual(inner.aquamarine_libraries(self.mapping(library) * 3, package),
                         dict(declared_test_override=package, loaded_libraries=[library]))

    def test_missing_mixed_foreign_deleted_and_prefix_matches_are_rejected(self):
        package = "/nix/store/test-aquamarine"
        library = f"{package}/lib/libaquamarine.so.10.1.0"
        foreign = "/nix/store/stock-aquamarine/lib/libaquamarine.so.10.1.0"
        for maps in ("", "[heap]\n", self.mapping(foreign),
                     self.mapping(library) + self.mapping(foreign),
                     self.mapping(library + " (deleted)"),
                     self.mapping(package + "-foreign/lib/libaquamarine.so.10")):
            with self.subTest(maps=maps), self.assertRaises(RuntimeError):
                inner.aquamarine_libraries(maps, package)


if __name__ == "__main__":
    unittest.main()
