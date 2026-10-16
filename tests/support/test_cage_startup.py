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


if __name__ == "__main__":
    unittest.main()
