import importlib.util
from pathlib import Path
import unittest


spec = importlib.util.spec_from_file_location("cage_inner", Path(__file__).parents[1] / "cage-inner.py")
inner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inner)


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
