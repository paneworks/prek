"""A compositor fallback is not a successful native lock presentation."""
import importlib.util
from pathlib import Path
import unittest


spec = importlib.util.spec_from_file_location("morf_lock", Path(__file__).with_name("morf-lock.py"))
lock = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lock)


class LockAcknowledgement(unittest.TestCase):
    def outputs(self):
        return [dict(name="WAYLAND-1", width=1280, height=720, dpmsStatus=True)]

    def test_active_output_and_real_lock_without_fallback(self):
        lock.verify_lock_acknowledgement("Session got locked by 123abc\n", self.outputs())

    def test_missing_lock_and_timeout_acknowledgements_are_rejected(self):
        for log in ("", "Session got locked by 123abc\nSending locked after a 5 second timeout."):
            with self.subTest(log=log), self.assertRaises(ValueError):
                lock.verify_lock_acknowledgement(log, self.outputs())

    def test_inactive_missing_and_invalid_outputs_cannot_skip_rendering(self):
        for outputs in (None, [], [{}], [dict(self.outputs()[0], dpmsStatus=False)],
                        [dict(self.outputs()[0], dpmsStatus=1)],
                        [dict(self.outputs()[0], disabled=True)],
                        [dict(self.outputs()[0], width=0)],
                        [dict(self.outputs()[0], height=-1)]):
            with self.subTest(outputs=outputs), self.assertRaises(ValueError):
                lock.verify_lock_acknowledgement("Session got locked by 123abc", outputs)


if __name__ == "__main__":
    unittest.main()
