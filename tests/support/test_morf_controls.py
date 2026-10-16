"""Receipt validation cannot turn a skipped or mismatched probe into acceptance."""
import importlib.util
import base64
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("morf_controls", Path(__file__).with_name("morf-controls.py"))
controls = importlib.util.module_from_spec(spec)
spec.loader.exec_module(controls)


class ScreencopyReceipt(unittest.TestCase):
    def png(self):
        return base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")

    def test_complete_positive_png_dimensions(self):
        self.assertEqual(controls.png_size(self.png()), (1, 1))

    def test_missing_truncated_wrong_header_and_zero_size_are_rejected(self):
        png = self.png()
        for data in (b"", png[:32], png[:-1], b"bad" + png[3:],
                     png[:12] + b"FAIL" + png[16:], png[:16] + b"\x00" * 4 + png[20:]):
            with self.subTest(data=data), self.assertRaises(ValueError):
                controls.png_size(data)


class NativeReceipt(unittest.TestCase):
    def receipt(self):
        return dict(status="passed", uid=0, native_ui_handoff_tested=True,
                    actual_logind_registration=True, compositor_pid=123,
                    native_action_receipts_tested=True, native_action_rendering_tested=False,
                    native_scope_revocation_tested=True, native_gesture_cancel_supported=False,
                    native_gesture_cancel_tested=False,
                    producer_pid=456, output="HEADLESS-1", worker_reaped=True,
                    input_devices_opened=0, physical_takeover_tested=False,
                    broker_grant_tested=False, restored=True)

    def line(self, receipt):
        return "PREK_NATIVE_ROOT_RECEIPT=" + json.dumps(receipt) + "\n"

    def parse(self, stdout):
        return controls.native_root_receipt(stdout, compositor_pid=123, producer_pid=456,
                                            output="HEADLESS-1")

    def test_complete_correlated_receipt(self):
        receipt = self.receipt()
        self.assertEqual(self.parse("running 1 test\n" + self.line(receipt) + "ok\n"), receipt)

    def test_skipped_duplicated_or_malformed_probe_is_not_acceptance(self):
        for stdout in ("running 0 tests\ntest result: ok\n", "", self.line([]),
                       self.line(self.receipt()) * 2, "PREK_NATIVE_ROOT_RECEIPT={\n"):
            with self.subTest(stdout=stdout), self.assertRaises(ValueError):
                self.parse(stdout)

    def test_every_binding_and_scope_field_is_required_and_typed(self):
        for field, value in self.receipt().items():
            for replacement in (None, not value if type(value) is bool else "foreign",
                                0 if type(value) is bool else False):
                receipt = self.receipt()
                receipt[field] = replacement
                with self.subTest(field=field, replacement=replacement), self.assertRaises(ValueError):
                    self.parse(self.line(receipt))
    def test_cancellation_acceptance_requires_the_observed_native_capability(self):
        receipt = self.receipt()
        receipt.update(native_gesture_cancel_supported=True, native_gesture_cancel_tested=True)
        with self.assertRaises(ValueError):
            self.parse(self.line(receipt))
        controls.native_root_receipt(self.line(receipt),compositor_pid=123,producer_pid=456,
                                     output="HEADLESS-1",cancel_supported=True)
        receipt["native_gesture_cancel_tested"] = False
        with self.assertRaises(ValueError):
            controls.native_root_receipt(self.line(receipt),compositor_pid=123,producer_pid=456,
                                         output="HEADLESS-1",cancel_supported=True)


if __name__ == "__main__":
    unittest.main()
