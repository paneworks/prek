"""Pure parser/cleanup tests; receipt fixtures model root, never obtain it."""
import argparse
import contextlib
import io
import json
import os
import select
import shutil
import signal
import stat
import subprocess
import sys
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import root_routing as root


def recovery_inventory():
    return root.RECOVERY_INVENTORY + "\n" + json.dumps(sorted(root.RECOVERY_EXPECTED)) + "\n"


def recovery_output(names=None, **changes):
    names = sorted(root.RECOVERY_EXPECTED) if names is None else names
    cases = "".join(f"kernel recovery: {name.split('::')[1]} / {name.split('::')[2]} ... ok\n" for name in names)
    value = dict(version=1, passed=8, tests=names)
    value.update(changes)
    return cases + root.RECOVERY_RESULT + json.dumps(value) + "\n"


def repeat_output(**changes):
    value = dict(version=1, initial=[250, 33], disabled_forwarded=1, disabled_observed=1,
                 enabled_forwarded=1, enabled_observed=4, consumer_feedback_codes=[0, 1],
                 keys_neutral=True, physical_devices_opened=0, repeat_passthrough_tested=False)
    value.update(changes)
    return root.REPEAT_RESULT + json.dumps(value) + "\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"


def fuzz_output(**changes):
    value = dict(version=1, fuzz=16, source_raw=[16, 24], source_observed=[8, 16],
                 mirror_raw=8, mirror_observed=2, restore_raw=16, restore_observed=8,
                 gap_raw=[31, 32], gap_observed=[15, 32], zero_fuzz_observed=16,
                 inactive_slot_preserved=True, contacts_neutral=True,
                 physical_devices_opened=0, fuzz_passthrough_tested=False)
    value.update(changes)
    return root.FUZZ_RESULT + json.dumps(value) + "\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"


def key_output(**changes):
    value = dict(version=1, status="passed", uid=0, physical_devices_opened=0,
                 native_preparation_tested=True, generated_role_metadata_verified=True,
                 native_key_output_tested=True, native_cancel_before_write_tested=True,
                 native_repeat_event_tested=True, native_partial_write_tested=False,
                 broker_authorization_tested=False, compositor_delivery_tested=False,
                 worker_binding_tested=False, owned_device_removed=True)
    value.update(changes)
    return root.KEY_OUTPUT_RESULT + json.dumps(value) + "\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n"


def managed_phases():
    return [dict(phase="software_fast", raw0=[], raw1=[], generated=[[42, 1], [42, 2], [42, 0]],
                 raw0_times_ns=[], raw1_times_ns=[], generated_times_ns=[10_000, 11_000, 12_000]),
            dict(phase="software_overlap", raw0=[], raw1=[], generated=[[42, 1]],
                 raw0_times_ns=[], raw1_times_ns=[], generated_times_ns=[30_000]),
            dict(phase="software_sibling", raw0=[], raw1=[], generated=[[42, 2]],
                 raw0_times_ns=[], raw1_times_ns=[], generated_times_ns=[40_000]),
            dict(phase="software_expiry", raw0=[], raw1=[], generated=[[42, 0]],
                 raw0_times_ns=[], raw1_times_ns=[], generated_times_ns=[100_000_000]),
            dict(phase="mixed", raw0=[[31, 1], [31, 0]], raw1=[], generated=[[42, 1], [42, 2], [42, 0]],
                 raw0_times_ns=[200_000_003, 200_000_005, 200_000_006], raw1_times_ns=[], generated_times_ns=[200_000_001, 200_000_002, 200_000_004]),
            dict(phase="hybrid_fallback", raw0=[[32, 1], [32, 0]], raw1=[], generated=[],
                 raw0_times_ns=[310_000_000, 390_000_000], raw1_times_ns=[], generated_times_ns=[]),
            dict(phase="cross_source", raw0=[], raw1=[[31, 1], [31, 0]], generated=[[42, 1]],
                 raw0_times_ns=[], raw1_times_ns=[500_000_011], generated_times_ns=[500_000_010]),
            dict(phase="software_physical", raw0=[], raw1=[], generated=[[42, 2]],
                 raw0_times_ns=[], raw1_times_ns=[], generated_times_ns=[500_005_000]),
            dict(phase="revocation", raw0=[[31, 1], [31, 0]], raw1=[], generated=[[42, 0]],
                 raw0_times_ns=[500_010_001], raw1_times_ns=[], generated_times_ns=[500_010_000])]


def managed_software():
    entries = [(1, 1, "open", False, 1000), (1, 1, "open", True, 2000), (1, 2, "press", False, 3000),
               (1, 2, "press", True, 4000), (1, 3, "repeat", True, 5000), (1, 4, "release", True, 6000),
               (1, 5, "close", True, 13000), (1, 1, "open", False, 14000),
               (2, 1, "open", True, 15000), (2, 2, "press", True, 16000),
               (3, 1, "open", True, 17000), (3, 2, "press", True, 18000),
               (2, 3, "close", True, 31000), (3, 3, "repeat", True, 32000), (3, 4, "renew", True, 33000),
               (3, 5, "renew", False, 100_001_000), (3, 1, "open", False, 100_002_000),
               (4, 1, "open", True, 500_001_000), (4, 2, "press", True, 500_002_000),
               (4, 3, "repeat", True, 500_003_000), (4, 4, "close", True, 500_006_000)]
    receipts = []
    for index, (producer, sequence, kind, accepted, at) in enumerate(entries):
        operation = dict(kind=kind)
        if kind in ("open", "renew"):
            expiry = 100_000_000 if index == 14 else at + 200_000_000
            operation["expires"] = dict(secs=0, nanos=expiry)
        if kind in ("press", "repeat", "release"):
            operation["id"] = 1
        if kind == "press":
            operation["key"] = 42
        request = dict(fence=dict(broker=[1, 2], configuration=1, session=1),
                       source=dict(seat="seat0", device=100 if index != 2 else 1, generation=1),
                       marker="prek/test/seat0/100/1/keyboard/pub/1/2",
                       consumer=dict(isolated_receiver=dict(pid=2 if index == 0 else 1, start=1)),
                       producer=[producer, 7], sequence=sequence, operation=operation)
        receipts.append(dict(version=1, request=request, accepted=accepted, sent_at_ns=at, received_at_ns=at + 10))
    receipts[0]["request"]["operation"] = dict(receipts[1]["request"]["operation"])
    return receipts


def managed_drain():
    return dict(version=1, nonce=[1, 2], requested_at_ns=500_010_000, neutral_at_ns=500_010_000,
                deadline_ns=750_010_000, last_report_at_ns=500_010_000, keys_down=[], generated=[[42, 0]], reports=1)


def managed_hybrid():
    source = dict(device=1, generation=1, seat="seat0")
    generated = dict(device=100, generation=1, seat="seat0")
    capabilities = {"0": [0], "1": [30, 31, 32], "2": [0]}
    def frame(sequence, at, values, identity=source):
        return dict(source=identity, sequence=sequence, at=dict(secs=0, nanos=at),
                    events=[dict(kind=kind, code=code, value=value) for kind, code, value in values] + [dict(kind=0, code=0, value=0)])
    mixed_input = frame(1, 199_000_000, [(2, 0, 7), (1, 30, 1), (1, 30, 2), (1, 31, 1), (2, 0, 9), (1, 30, 0), (1, 31, 0)])
    mixed_raw = [frame(1, 200_000_003, [(1, 31, 1)]), frame(2, 200_000_005, [(1, 31, 0)]), frame(3, 200_000_006, [(2, 0, 7), (2, 0, 9)])]
    mixed_generated = [frame(7 + index, at, [(1, 42, state)], generated)
                       for index, (at, state) in enumerate(zip((200_000_001, 200_000_002, 200_000_004), (1, 2, 0)))]
    return dict(version=1, source=source, generated_source=generated, counterpart_event_nodes=1,
                counterpart=dict(path="/dev/input/event101", descriptor=dict(source=source, capabilities=capabilities,
                    isolated=True, marker="prek/test/0000000000000001/0000000000000001/00000000000000000000000000000001", **{"class": "mouse"})),
                observed_capabilities=capabilities, mixed_input=mixed_input, mixed_raw=mixed_raw, mixed_generated=mixed_generated,
                fallback_inputs=[frame(2, 210_000_000, [(2, 0, 7), (1, 32, 1)]), frame(3, 290_000_000, [(2, 0, 9), (1, 32, 0)])],
                fallback_raw=[frame(4, 310_000_000, [(2, 0, 7), (1, 32, 1)]), frame(5, 390_000_000, [(2, 0, 9), (1, 32, 0)])],
                fallback_generated=[], scheduler_slack_ns=20_000_000, physical_devices_opened=0, compositor_delivery_tested=False, mt_tested=False)


def managed_output(phases=None, drains=None, software=None, hybrid=None, **changes):
    value = dict(version=4, status="passed", uid=0, physical_devices_opened=0,
                 worker_binding_tested=True, private_control_authenticated=True,
                 consumer_neutral_drain_tested=True, consumer_neutral_drain_acknowledgements=1,
                 raw_outputs=2, generated_outputs=1, source_readers_grabbed=2,
                 native_projection_tested=True, native_repeat_tested=True, native_cancellation_tested=True,
                 kernel_write_order_tested=True, native_partial_write_tested=False,
                 hybrid_relative_tested=True, hybrid_counterpart_tested=True, hybrid_fallback_cadence_tested=True, hybrid_mt_tested=False,
                 software_producer_control_tested=True, software_producer_expiry_tested=True,
                 software_producer_rejection_tested=True, software_physical_ownership_tested=True,
                 software_request_acknowledgements=21, software_rejections=5, software_public_authorization_tested=False,
                 broker_grant_tested=False, context_authority_modeled=True,
                 production_input_loop_tested=False, production_route_admission_tested=False,
                 compositor_delivery_tested=False, keys_neutral=True, worker_reaped=True, owned_devices_removed=True)
    value.update(changes)
    phases = managed_phases() if phases is None else phases
    drains = [managed_drain()] if drains is None else drains
    software = managed_software() if software is None else software
    hybrid = [managed_hybrid()] if hybrid is None else hybrid
    return ("".join(root.MANAGED_PHASE + json.dumps(phase) + "\n" for phase in phases)
            + "".join(root.MANAGED_DRAIN + json.dumps(drain) + "\n" for drain in drains)
            + "".join(root.MANAGED_SOFTWARE + json.dumps(receipt) + "\n" for receipt in software)
            + "".join(root.MANAGED_HYBRID + json.dumps(receipt) + "\n" for receipt in hybrid)
            + root.MANAGED_RESULT + json.dumps(value) + "\n")


class KernelReceipt(unittest.TestCase):
    def value(self):
        return dict(version=10, status="passed", uid=0, kernel=os.uname().release,
                    tests=sorted(root.ALL_EXPECTED), passed=len(root.ALL_EXPECTED),
                    routing_passed=len(root.EXPECTED), recovery_passed=8, physical_devices_opened=0,
                    repeat_characterization_passed=1, repeat_passthrough_tested=False,
                    fuzz_characterization_passed=1, fuzz_passthrough_tested=False,
                    generated_key_output_passed=1, generated_key_output_authorization_tested=False,
                    generated_key_output_compositor_tested=False, generated_key_output_partial_write_tested=False,
                    managed_key_worker_passed=1, managed_key_worker_binding_tested=True,
                    managed_key_worker_consumer_drain_tested=True,
                    managed_key_worker_software_control_tested=True, managed_key_worker_software_expiry_tested=True,
                    managed_key_worker_software_authorization_tested=False,
                    managed_key_worker_hybrid_relative_tested=True, managed_key_worker_hybrid_counterpart_tested=True,
                    managed_key_worker_hybrid_fallback_cadence_tested=True, managed_key_worker_hybrid_mt_tested=False,
                    managed_key_worker_compositor_tested=False, managed_key_worker_partial_write_tested=False,
                    model_desktop_context=True, production_route_admission_tested=False,
                    compositor_consumer_retirement_tested=False, rule_removed=True, registry_clean=True,
                    unrelated_registry_preserved=True, recovery_controls_removed=True)

    def line(self, value):
        return root.PREFIX + json.dumps(value) + "\n"

    def test_managed_inventory_is_exact_and_cannot_be_a_skip(self):
        valid = root.MANAGED_INVENTORY + "\n" + json.dumps([root.MANAGED_CASE]) + "\n"
        root.managed_inventory(valid)
        for value in ["", "skipped\n", valid * 2, valid + "extra\n", valid.replace(root.MANAGED_CASE, "foreign")]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                root.managed_inventory(value)

    def test_hybrid_evidence_requires_one_counterpart_and_every_original_report(self):
        value = root.managed_hybrid(managed_output(), managed_phases())
        self.assertEqual(value["counterpart_event_nodes"], 1)
        self.assertFalse(value["mt_tested"])
        for evidence in ([], [managed_hybrid(), managed_hybrid()], [{}]):
            with self.subTest(evidence=evidence), self.assertRaises(ValueError):
                root.managed_result(managed_output(hybrid=evidence))
        for field in value:
            incomplete = managed_hybrid()
            del incomplete[field]
            with self.subTest(missing=field), self.assertRaises(ValueError):
                root.managed_result(managed_output(hybrid=[incomplete]))

    def test_hybrid_projection_metadata_clocks_and_kernel_correlation_are_strict(self):
        for change in ("node-count", "path", "observed", "declared", "source", "generated-alias", "mixed-input", "mixed-tail",
                       "mixed-timing", "input-gap", "output-gap", "output-payload", "extra-generated", "bool", "compositor", "mt"):
            value = managed_hybrid()
            if change == "node-count":
                value["counterpart_event_nodes"] = 2
            elif change == "path":
                value["counterpart"]["path"] = "/tmp/event101"
            elif change == "observed":
                value["observed_capabilities"] = {"0": [0], "1": [30, 31, 32]}
            elif change == "declared":
                value["counterpart"]["descriptor"]["class"] = "keyboard"
            elif change == "source":
                value["mixed_raw"][0]["source"] = dict(device=2, generation=1, seat="seat0")
            elif change == "generated-alias":
                value["generated_source"] = value["source"]
            elif change == "mixed-input":
                value["mixed_input"]["events"].pop(0)
            elif change == "mixed-tail":
                value["mixed_raw"][-1]["events"].reverse()
            elif change == "mixed-timing":
                value["mixed_raw"][-1]["at"]["nanos"] += 1
            elif change == "input-gap":
                value["fallback_inputs"][1]["at"]["nanos"] = 211_000_000
            elif change == "output-gap":
                value["fallback_raw"][1]["at"]["nanos"] = 311_000_000
            elif change == "output-payload":
                value["fallback_raw"][0]["events"][0]["value"] = 8
            elif change == "extra-generated":
                value["fallback_generated"] = value["mixed_generated"]
            elif change == "bool":
                value["mixed_input"]["events"][1]["value"] = True
            elif change == "compositor":
                value["compositor_delivery_tested"] = True
            else:
                value["mt_tested"] = True
            with self.subTest(change=change), self.assertRaises(ValueError):
                root.managed_result(managed_output(hybrid=[value]))

    def test_hybrid_generated_keyboard_may_share_only_the_device_number_of_its_source(self):
        value = managed_hybrid()
        value["generated_source"] = dict(device=1, generation=2, seat="seat0")
        for frame in value["mixed_generated"]:
            frame["source"] = value["generated_source"]
        accepted = root.managed_hybrid(managed_output(hybrid=[value]), managed_phases())
        self.assertEqual(accepted["generated_source"], value["generated_source"])

    def test_old_managed_and_root_receipts_cannot_claim_native_hybrid_evidence(self):
        with self.assertRaises(ValueError):
            root.managed_result(managed_output(version=3))
        old = self.value()
        old["version"] = 8
        for field in list(old):
            if field.startswith("managed_key_worker_hybrid_"):
                del old[field]
        with self.assertRaises(ValueError):
            root.receipt(self.line(old))

    def test_managed_scope_is_exact_typed_and_cannot_promote_unproven_gates(self):
        valid = managed_output()
        value = root.managed_result(valid)
        self.assertTrue(value["worker_binding_tested"])
        self.assertFalse(value["broker_grant_tested"])
        for output in ["", valid * 2, managed_output(extra=True), root.MANAGED_RESULT + "{", valid.replace(root.MANAGED_RESULT, "FOREIGN=")]:
            with self.subTest(output=output), self.assertRaises((ValueError, json.JSONDecodeError)):
                root.managed_result(output)
        for key, expected in value.items():
            missing = dict(value)
            del missing[key]
            phases = "\n".join(valid.splitlines()[:-1]) + "\n"
            with self.subTest(missing=key), self.assertRaises(ValueError):
                root.managed_result(phases + root.MANAGED_RESULT + json.dumps(missing))
            changed = not expected if type(expected) is bool else str(expected) if type(expected) is int else "failed"
            with self.subTest(changed=key), self.assertRaises(ValueError):
                root.managed_result(managed_output(**{key: changed}))

    def test_managed_phases_require_exact_keys_reports_and_strict_cross_fd_order(self):
        for phases in [[], managed_phases()[:2], managed_phases()[::-1], managed_phases() + managed_phases()[:1], [None] * 3]:
            with self.subTest(phases=phases), self.assertRaises(ValueError):
                root.managed_result(managed_output(phases=phases))
        for index, field, changed in [(4, "generated", [[42, 1], [42, 0]]), (4, "raw0", [[30, 1], [31, 0]]),
                                      (4, "raw0_times_ns", [200_000_002, 200_000_005]), (5, "generated_times_ns", [200_000_012]),
                                      (7, "generated_times_ns", [200_010_001]), (0, "generated_times_ns", [True, 2, 4]),
                                      (0, "raw1_times_ns", [3]), (7, "raw0", [[31, True], [31, 0]]),
                                      (4, "raw0_times_ns", [200_000_003.0, 200_000_005]), (1, "extra", True),
                                      (2, "generated", [[42, 0]]), (6, "generated", [[42, 2], [42, 0]])]:
            phases = managed_phases()
            phases[index][field] = changed
            with self.subTest(index=index, field=field), self.assertRaises(ValueError):
                root.managed_result(managed_output(phases=phases))

    def test_software_admissions_require_the_exact_complete_correlated_stream(self):
        for receipts in [[], managed_software()[:-1], managed_software()[::-1], managed_software() * 2, [None] * 21]:
            with self.subTest(receipts=receipts), self.assertRaises(ValueError):
                root.managed_result(managed_output(software=receipts))
        for index in range(21):
            for key in managed_software()[index]:
                receipts = managed_software()
                del receipts[index][key]
                with self.subTest(index=index, missing=key), self.assertRaises(ValueError):
                    root.managed_result(managed_output(software=receipts))

    def test_software_scope_sequence_operation_and_clock_changes_are_rejected(self):
        mutations = [("producer", [True, 7]), ("producer", [2, 7]), ("sequence", 2), ("sequence", True),
                     ("source", dict(seat="seat1", device=100, generation=1)),
                     ("source", dict(seat="seat0", device=True, generation=1)),
                     ("marker", "unbound"), ("consumer", dict(isolated_receiver=dict(pid=2, start=1))),
                     ("consumer", dict(isolated_receiver=dict(pid=True, start=1))),
                     ("fence", dict(broker=[True, 2], configuration=1, session=1)),
                     ("operation", dict(kind="open", expires=dict(secs=0, nanos=True))),
                     ("operation", dict(kind="open", expires=dict(secs=1, nanos=0))),
                     ("operation", dict(kind="open", expires=dict(secs=0, nanos=2000))),
                     ("extra", True)]
        for key, value in mutations:
            receipts = managed_software()
            receipts[1]["request"][key] = value
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                root.managed_result(managed_output(software=receipts))
        for index, key, value in [(0, "accepted", True), (1, "accepted", 1), (1, "version", True),
                                  (1, "received_at_ns", 1000), (1, "received_at_ns", 250_002_000),
                                  (1, "sent_at_ns", True), (1, "extra", True),
                                  (15, "sent_at_ns", 99_999_000)]:
            receipts = managed_software()
            receipts[index][key] = value
            with self.subTest(index=index, key=key), self.assertRaises(ValueError):
                root.managed_result(managed_output(software=receipts))

    def test_software_expiry_and_settled_before_close_are_correlated_to_kernel_reports(self):
        for index, at in [(0, 3000), (3, 99_998_000), (3, 350_000_000), (7, 500_007_000)]:
            phases = managed_phases()
            phases[index]["generated_times_ns"][0] = at
            with self.subTest(index=index, at=at), self.assertRaises(ValueError):
                root.managed_result(managed_output(phases=phases))
        receipts = managed_software()
        receipts[14]["request"]["operation"]["expires"]["nanos"] = 100_000_999
        root.managed_result(managed_output(software=receipts))

    def test_v7_receipt_cannot_claim_software_control_and_v2_managed_cannot_pass(self):
        old = self.value()
        old.update(version=7)
        for key in list(old):
            if key.startswith("managed_key_worker_software_"):
                del old[key]
        with self.assertRaises(ValueError):
            root.receipt(self.line(old))
        with self.assertRaises(ValueError):
            root.managed_result(managed_output(version=2))

    def test_v5_receipt_cannot_claim_the_new_managed_worker_gate(self):
        old = self.value()
        old.update(version=5, passed=25, tests=sorted(root.ALL_EXPECTED - {root.MANAGED_CASE}))
        for key in list(old):
            if key.startswith("managed_"):
                del old[key]
        with self.assertRaises(ValueError):
            root.receipt(self.line(old))

    def test_v6_and_old_managed_receipts_cannot_claim_consumer_drain(self):
        old = self.value()
        old.update(version=6)
        del old["managed_key_worker_consumer_drain_tested"]
        with self.assertRaises(ValueError):
            root.receipt(self.line(old))
        with self.assertRaises(ValueError):
            root.managed_result(managed_output(version=1))

    def test_managed_drain_requires_a_live_neutral_sample_and_correlated_final_release(self):
        root.managed_result(managed_output())
        for drains in [[], [managed_drain(), managed_drain()], [None]]:
            with self.subTest(drains=drains), self.assertRaises(ValueError):
                root.managed_result(managed_output(drains=drains))
        wrong = [dict(keys_down=[42]), dict(generated=[[42, False]]), dict(reports=True),
                 dict(reports=0), dict(last_report_at_ns=19), dict(requested_at_ns=19),
                 dict(neutral_at_ns=21), dict(neutral_at_ns=19), dict(deadline_ns=20),
                 dict(deadline_ns=450_010_001), dict(nonce=[0, 0]), dict(nonce=[True, 2]),
                 dict(nonce=[-1, 2]), dict(nonce=[2**64, 2]), dict(nonce="foreign"), dict(extra=True)]
        for key in ("requested_at_ns", "neutral_at_ns", "deadline_ns", "last_report_at_ns"):
            wrong.extend({key: value} for value in (True, 20.0, None, 0))
        for changes in wrong:
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                root.managed_result(managed_output(drains=[dict(managed_drain(), **changes)]))
        for key in managed_drain():
            incomplete = managed_drain()
            del incomplete[key]
            with self.subTest(missing=key), self.assertRaises(ValueError):
                root.managed_result(managed_output(drains=[incomplete]))

    def test_generated_key_output_requires_one_exact_inventory(self):
        root.key_output_inventory(root.KEY_OUTPUT_CASE + ": test\n")
        for output in ["", (root.KEY_OUTPUT_CASE + ": test\n") * 2, root.FUZZ_CASE + ": test\n"]:
            with self.subTest(output=output), self.assertRaises(ValueError):
                root.key_output_inventory(output)

    def test_generated_key_output_scope_is_complete_exact_and_typed(self):
        valid = key_output()
        self.assertTrue(root.key_output_result(valid)["native_key_output_tested"])
        for output in ["", valid * 2, key_output(extra=True), valid.replace("1 passed", "0 passed"),
                       root.KEY_OUTPUT_RESULT + "{", valid.replace(root.KEY_OUTPUT_RESULT, "FOREIGN=")]:
            with self.subTest(output=output), self.assertRaises((ValueError, TypeError)):
                root.key_output_result(output)
        original = json.loads(valid.splitlines()[0][len(root.KEY_OUTPUT_RESULT):])
        for field in original:
            missing = dict(original)
            del missing[field]
            with self.subTest(missing=field), self.assertRaises(ValueError):
                root.key_output_result(root.KEY_OUTPUT_RESULT + json.dumps(missing) + "\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n")
            for replacement in [None, "foreign", True, False, 0, 1, []]:
                if type(replacement) is type(original[field]) and replacement == original[field]:
                    continue
                with self.subTest(field=field, replacement=replacement), self.assertRaises(ValueError):
                    root.key_output_result(key_output(**{field: replacement}))

    def test_v4_receipt_without_generated_output_is_not_current_acceptance(self):
        old = self.value()
        old.update(version=4, passed=24, tests=sorted(root.ALL_EXPECTED - {root.KEY_OUTPUT_CASE}))
        for field in list(old):
            if field.startswith("generated_key_output_"):
                del old[field]
        with self.assertRaises(ValueError):
            root.receipt(self.line(old))

    def test_one_correlated_receipt_is_required(self):
        value = self.value()
        self.assertEqual(root.receipt("test output\n" + self.line(value)), value)
        for output in ["", self.line(value) * 2, root.PREFIX + "{", self.line([])]:
            with self.subTest(output=output), self.assertRaises(ValueError):
                root.receipt(output)

    def test_every_acceptance_field_is_required_and_typed(self):
        for field in self.value():
            for replacement in [None, "foreign", True, 999]:
                value = self.value()
                if replacement == value[field] and type(replacement) is type(value[field]):
                    continue
                value[field] = replacement
                with self.subTest(field=field, replacement=replacement), self.assertRaises(ValueError):
                    root.receipt(self.line(value))

    def test_old_inventory_without_both_native_witness_cases_is_not_acceptance(self):
        names = {name for name in root.EXPECTED if name.startswith("evdev_boundary::witness_")}
        self.assertEqual(len(names), 2)
        old = self.value()
        old.update(tests=sorted(root.ALL_EXPECTED - names), passed=len(root.ALL_EXPECTED) - 2,
                   routing_passed=len(root.EXPECTED) - 2)
        with self.assertRaises(ValueError):
            root.receipt(self.line(old))
        inventory = "\n".join(f"{name}: test" for name in sorted(root.EXPECTED - names))
        with self.assertRaises(ValueError):
            root.test_inventory(inventory)

    def test_inventory_without_the_real_reader_cutover_case_is_not_acceptance(self):
        name = "lifecycle::t58_reader_cutover_preserves_prefix_cursor_and_downstream_tail"
        self.assertIn(name, root.EXPECTED)
        old = self.value()
        old.update(tests=sorted(root.ALL_EXPECTED - {name}), passed=len(root.ALL_EXPECTED) - 1,
                   routing_passed=len(root.EXPECTED) - 1)
        with self.assertRaises(ValueError):
            root.receipt(self.line(old))

    def test_receipt_rejects_missing_foreign_or_uncorrelated_fields(self):
        for field in self.value():
            value = self.value()
            del value[field]
            with self.subTest(missing=field), self.assertRaises(ValueError):
                root.receipt(self.line(value))
        for changes in [dict(extra=True), dict(kernel="foreign-kernel"),
                        dict(tests=list(reversed(sorted(root.ALL_EXPECTED)))),
                        dict(tests=sorted(root.EXPECTED))]:
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                root.receipt(self.line(dict(self.value(), **changes)))

    def test_old_receipt_without_repeat_characterization_is_not_acceptance(self):
        value = self.value()
        value.update(version=2, tests=sorted(root.EXPECTED | root.RECOVERY_EXPECTED), passed=22)
        del value["repeat_characterization_passed"]
        del value["repeat_passthrough_tested"]
        with self.assertRaises(ValueError):
            root.receipt(self.line(value))

    def test_repeat_inventory_requires_the_one_exact_ignored_case(self):
        inventory = root.REPEAT_CASE + ": test\n"
        root.repeat_inventory(inventory)
        for value in ["", inventory * 2, inventory + "foreign: test\n", "skipped\n", "foreign: test\n"]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                root.repeat_inventory(value)

    def test_old_receipt_without_fuzz_characterization_is_not_acceptance(self):
        value = self.value()
        value.update(version=3, tests=sorted(root.ALL_EXPECTED - {root.FUZZ_CASE, root.KEY_OUTPUT_CASE}), passed=23)
        del value["fuzz_characterization_passed"]
        del value["fuzz_passthrough_tested"]
        with self.assertRaises(ValueError):
            root.receipt(self.line(value))

    def test_fuzz_inventory_requires_the_one_exact_ignored_case(self):
        inventory = root.FUZZ_CASE + ": test\n"
        root.fuzz_inventory(inventory)
        for value in ["", inventory * 2, inventory + "foreign: test\n", root.REPEAT_CASE + ": test\n"]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                root.fuzz_inventory(value)

    def test_fuzz_result_requires_actual_refiltering_and_complete_typed_evidence(self):
        valid = fuzz_output()
        self.assertEqual(root.fuzz_result(valid)["source_observed"], [8, 16])
        for value in ["", valid * 2, valid.split("test result:")[0],
                      valid.replace("1 passed", "0 passed"), valid.replace("0 ignored", "1 ignored"),
                      valid.replace(root.FUZZ_RESULT, "FOREIGN="),
                      fuzz_output(mirror_observed=8), fuzz_output(restore_observed=16),
                      fuzz_output(fuzz_passthrough_tested=True), fuzz_output(extra=True),
                      fuzz_output(source_raw=[True, 24]), fuzz_output(gap_observed=[15.0, 32])]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                root.fuzz_result(value)
        original = json.loads(valid.splitlines()[0][len(root.FUZZ_RESULT):])
        for field in original:
            missing = dict(original)
            del missing[field]
            with self.subTest(missing=field), self.assertRaises(ValueError):
                root.fuzz_result(root.FUZZ_RESULT + json.dumps(missing) + "\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n")
            for replacement in [None, "foreign", True, 999]:
                if type(replacement) is type(original[field]) and replacement == original[field]:
                    continue
                changed = dict(original, **{field: replacement})
                with self.subTest(field=field, replacement=replacement), self.assertRaises(ValueError):
                    root.fuzz_result(root.FUZZ_RESULT + json.dumps(changed) + "\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n")

    def test_repeat_result_requires_real_extra_repeats_and_complete_typed_evidence(self):
        valid = repeat_output()
        self.assertEqual(root.repeat_result(valid)["enabled_observed"], 4)
        for value in ["", valid * 2, valid.split("test result:")[0],
                      valid.replace("1 passed", "0 passed"), valid.replace("0 ignored", "1 ignored"),
                      valid.replace(root.REPEAT_RESULT, "FOREIGN="),
                      repeat_output(enabled_observed=1), repeat_output(enabled_observed=True),
                      repeat_output(enabled_observed=256), repeat_output(enabled_observed="4"),
                      repeat_output(initial=[True, 33]), repeat_output(initial=[0, 0]),
                      repeat_output(consumer_feedback_codes=[False, True]),
                      repeat_output(consumer_feedback_codes=[0]), repeat_output(extra=True)]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                root.repeat_result(value)
        original = json.loads(valid.splitlines()[0][len(root.REPEAT_RESULT):])
        for field in original:
            missing = dict(original)
            del missing[field]
            with self.subTest(missing=field), self.assertRaises(ValueError):
                root.repeat_result(root.REPEAT_RESULT + json.dumps(missing) + "\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n")
            for replacement in [None, "foreign", True, 999]:
                if type(replacement) is type(original[field]) and replacement == original[field]:
                    continue
                changed = dict(original, **{field: replacement})
                with self.subTest(field=field, replacement=replacement), self.assertRaises(ValueError):
                    root.repeat_result(root.REPEAT_RESULT + json.dumps(changed) + "\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n")

    def test_recovery_inventory_is_exact_and_cannot_be_a_skip(self):
        valid = recovery_inventory()
        root.recovery_inventory(valid)
        for output in ["", "skipped\n", valid + "extra\n", valid * 2,
                       valid.replace(root.RECOVERY_INVENTORY, "FOREIGN"),
                       root.RECOVERY_INVENTORY + "\n{", root.RECOVERY_INVENTORY + "\n{}",
                       root.RECOVERY_INVENTORY + "\n" + json.dumps([True] * 8),
                       root.RECOVERY_INVENTORY + "\n" + json.dumps(sorted(root.RECOVERY_EXPECTED)[:-1]),
                       root.RECOVERY_INVENTORY + "\n" + json.dumps([sorted(root.RECOVERY_EXPECTED)[0]] * 8)]:
            with self.subTest(output=output), self.assertRaises(ValueError):
                root.recovery_inventory(output)

    def test_recovery_result_correlates_every_case_and_summary(self):
        valid = recovery_output()
        root.recovery_result(valid)
        root.recovery_result(recovery_output(list(reversed(sorted(root.RECOVERY_EXPECTED)))))
        for output in ["", "kernel recovery skipped\n", valid * 2,
                       valid.replace("keyboard / orderly", "keyboard / skipped", 1),
                       valid.replace(" ... ok", " ... FAILED", 1),
                       valid + "kernel recovery: foreign / orderly ... ok\n",
                       valid.split(root.RECOVERY_RESULT)[0],
                       valid.split(root.RECOVERY_RESULT)[0] + root.RECOVERY_RESULT + "{}\n",
                       recovery_output(sorted(root.RECOVERY_EXPECTED)[:-1]),
                       recovery_output([sorted(root.RECOVERY_EXPECTED)[0]] * 8),
                       recovery_output(tests=list(reversed(sorted(root.RECOVERY_EXPECTED)))),
                       recovery_output(version=True), recovery_output(passed=True),
                       recovery_output(version=2), recovery_output(passed=0), recovery_output(extra=True)]:
            with self.subTest(output=output), self.assertRaises(ValueError):
                root.recovery_result(output)

    def test_exact_ignored_inventory_prevents_zero_or_changed_gates(self):
        expected = "\n".join(name + ": test" for name in sorted(root.EXPECTED))
        root.test_inventory(expected)
        for output in ["", expected + "\nforeign: test", expected + "\n" + expected, expected.split("\n", 1)[1]]:
            with self.subTest(output=output), self.assertRaises(ValueError):
                root.test_inventory(output)

    def test_summary_cannot_convert_skip_failure_or_subset_into_acceptance(self):
        root.test_result(f"test result: ok. {len(root.EXPECTED)} passed; 0 failed; 0 ignored;\n")
        for result in ["", "test result: ok. 0 passed; 0 failed; 11 ignored;\n",
                       "test result: FAILED. 10 passed; 1 failed; 0 ignored;\n",
                       "test result: ok. 1 passed; 0 failed; 0 ignored;\n"]:
            with self.subTest(result=result), self.assertRaises(ValueError):
                root.test_result(result)

    def test_cleanup_preserves_replaced_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary)
            original = path / "owned.rules"
            original.write_text("our test rule")
            metadata = original.stat()
            identity = metadata.st_dev, metadata.st_ino
            fd = os.open(path, os.O_DIRECTORY | os.O_RDONLY)
            try:
                original.rename(path / "retained-original.rules")
                original.write_text("replacement")
                with self.assertRaises(RuntimeError):
                    root.remove_same(fd, original.name, identity)
                self.assertEqual(original.read_text(), "replacement")
                root.remove_same(fd, "retained-original.rules", identity)
                self.assertFalse((path / "retained-original.rules").exists())
            finally:
                os.close(fd)

    def test_recovery_directory_removes_only_its_empty_original(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary)
            fd = os.open(path, os.O_DIRECTORY | os.O_RDONLY)
            try:
                with root.recovery_directory(fd) as (name, identity):
                    directory = path / name
                    self.assertTrue(root.same(fd, name, identity))
                    self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o700)
                self.assertFalse(directory.exists())
                with self.assertRaisesRegex(RuntimeError, "replaced"), root.recovery_directory(fd) as (name, _):
                    directory = path / name
                    directory.rename(path / "retained-original")
                    directory.mkdir()
                    (directory / "foreign").write_text("preserved")
                self.assertEqual((directory / "foreign").read_text(), "preserved")
                self.assertTrue((path / "retained-original").is_dir())
            finally:
                os.close(fd)

    def test_nonempty_controls_are_retained_without_masking_original_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary)
            fd = os.open(path, os.O_DIRECTORY | os.O_RDONLY)
            try:
                with self.assertRaises(OSError), root.recovery_directory(fd) as (name, _):
                    (path / name / "unknown").write_text("retained")
                self.assertEqual((path / name / "unknown").read_text(), "retained")
                diagnostic = io.StringIO()
                with contextlib.redirect_stderr(diagnostic), self.assertRaisesRegex(ValueError, "original failure"):
                    with root.recovery_directory(fd) as (name, _):
                        (path / name / "unknown").write_text("retained")
                        raise ValueError("original failure")
                self.assertIn("Retained recovery controls", diagnostic.getvalue())
                self.assertEqual((path / name / "unknown").read_text(), "retained")
            finally:
                os.close(fd)

    def test_registry_snapshot_preserves_foreign_entries_and_does_not_follow_links(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary)
            (path / "foreign").write_text("preserved")
            (path / "broken-link").symlink_to(path / "absent")
            fd = os.open(path, os.O_DIRECTORY | os.O_RDONLY)
            try:
                original = root.registry_snapshot(fd)
                self.assertEqual(set(original), {"foreign", "broken-link"})
                self.assertTrue(stat.S_ISLNK(original["broken-link"][2]))
                self.assertEqual(root.registry_snapshot(fd), original)
                (path / "foreign").write_text("changed content")
                self.assertNotEqual(root.registry_snapshot(fd), original)
                self.assertTrue((path / "broken-link").is_symlink())
            finally:
                os.close(fd)

    def test_unprivileged_refusal_precedes_filesystem_devices_and_commands(self):
        self.assertNotEqual(os.geteuid(), 0, "these tests must run without root")
        with mock.patch.object(root, "directories") as directories, mock.patch.object(root.os, "stat") as metadata, \
                mock.patch.object(root.subprocess, "run") as command:
            with self.assertRaises(PermissionError):
                root.run(argparse.Namespace())
            directories.assert_not_called()
            metadata.assert_not_called()
            command.assert_not_called()

    def test_execute_keeps_real_child_output_and_nonzero_status(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.assertEqual(root.execute([sys.executable, "-c", "print('owned success')"], {}), "owned success\n")
            with self.assertRaisesRegex(RuntimeError, "exit 17"):
                root.execute([sys.executable, "-c", "print('owned failure', flush=True); raise SystemExit(17)"], {})
        self.assertIn("owned success", output.getvalue())
        self.assertIn("owned failure", output.getvalue())

    def test_timeout_kills_owned_group_even_when_leader_has_already_exited(self):
        source = "import subprocess, sys; child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)']); print(child.pid, flush=True)"
        output = io.StringIO()
        with mock.patch.object(root, "EXECUTION_TIMEOUT", 1), contextlib.redirect_stdout(output):
            with self.assertRaises(subprocess.TimeoutExpired):
                root.execute([sys.executable, "-c", source], {})
        pid = int(output.getvalue().strip())
        try:
            fd = os.pidfd_open(pid)
        except ProcessLookupError:
            return
        try:
            poller = select.poll()
            poller.register(fd, select.POLLIN)
            self.assertTrue(poller.poll(5000), "owned descendant survived timeout")
        finally:
            try:
                signal.pidfd_send_signal(fd, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.close(fd)

    def test_parent_death_kills_only_the_owned_test_child(self):
        # Real unprivileged processes/pidfd; no modeled root credentials.
        producer_source = r'''
import os, subprocess, sys
sys.path.insert(0, sys.argv[1])
import root_routing
parent = os.getpid()
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"],
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    preexec_fn=lambda: root_routing.child_setup(parent))
print(child.pid, flush=True)
sys.stdin.readline()
os._exit(0)
'''
        producer = subprocess.Popen([sys.executable, "-c", producer_source, str(Path(root.__file__).parent)],
                                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        child_fd = None
        try:
            poller = select.poll()
            poller.register(producer.stdout, select.POLLIN)
            self.assertTrue(poller.poll(5000), "test producer did not publish its owned child")
            pid = int(producer.stdout.readline())
            child_fd = os.pidfd_open(pid)
            child_poller = select.poll()
            child_poller.register(child_fd, select.POLLIN)
            self.assertFalse(child_poller.poll(0))
            producer.stdin.write("exit\n")
            producer.stdin.flush()
            self.assertEqual(producer.wait(timeout=5), 0)
            self.assertTrue(child_poller.poll(5000), "owned child survived its producer")
        finally:
            if producer.poll() is None:
                producer.kill()
            producer.wait()
            producer.stdin.close()
            producer.stdout.close()
            if child_fd is not None:
                try:
                    signal.pidfd_send_signal(child_fd, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                os.close(child_fd)

    def test_takeover_trial_runs_one_bounded_broker_and_session_then_removes_its_rule(self):
        for opens_socket, provisioned, holds, listeners, agent in (
                (True, False, True, 1, "ok"), (True, True, True, 1, "ok"), (False, False, True, 1, "ok"),
                (True, False, False, 1, "ok"), (True, False, True, 2, "ok"), (True, False, True, 1, "fails"),
                (True, False, True, 1, "leaves")):
            with self.subTest(opens_socket=opens_socket, provisioned=provisioned, holds=holds, listeners=listeners,
                              agent=agent):
                self.takeover_trial(opens_socket, provisioned, holds, listeners, agent)

    def takeover_trial(self, opens_socket, provisioned, holds, listeners, agent):
        trial = Path(__file__).resolve().parents[1] / "run-takeover-trial.sh"
        with tempfile.TemporaryDirectory() as temporary:
            repo, mocks, run_root = Path(temporary), Path(temporary) / "bin", Path(temporary) / "run"
            (repo / "tests").mkdir()
            (run_root / "udev").mkdir(parents=True)
            if provisioned:
                (run_root / "udev/rules.d").mkdir()
                (run_root / "prek/devices").mkdir(parents=True)
            mocks.mkdir()
            script = repo / "tests/run-takeover-trial.sh"
            shutil.copyfile(trial, script)
            devices = repo / "devices"
            devices.write_text('N: Name="Prek test keyboard"\n')
            calls, arguments = repo / "sudo-calls", repo / "agent-arguments"
            cgroup = next(line[3:] for line in Path("/proc/self/cgroup").read_text().splitlines()
                          if line.startswith("0::"))
            cgroups = repo / "cgroups"
            (cgroups / cgroup.lstrip("/")).mkdir(parents=True)
            procs = cgroups / cgroup.lstrip("/") / "cgroup.procs"
            procs.write_text("")
            prek = mocks / "prek-package"
            (prek / "bin").mkdir(parents=True)
            (prek / "lib/udev/rules.d").mkdir(parents=True)
            (prek / "lib/udev/rules.d/70-prek.rules").write_text("# rule\n")
            owners = " ".join(f'users:(("compositor",pid={os.getpid() + index},fd=3))' for index in range(listeners))
            scripts = {
                # Unprivileged stand-ins: sudo runs the remaining argv as this user.
                "sudo": f"""#!/bin/sh
printf '%s\\n' "$*" >> '{calls}'
[ "$1" = -n ] && shift
[ "$1" = -v ] && exit 0
exec "$@"
""",
                "git": "#!/bin/sh\nexit 0\n",
                "udevadm": "#!/bin/sh\nexit 0\n",
                "ss": f"""#!/bin/sh
echo 'u_str LISTEN 0 128 {temporary}/wayland-test 1 * 0 {owners}'
""",
                "install": """#!/bin/sh
directory=0
while [ $# -gt 2 ]; do
  case "$1" in -d) directory=1; shift ;; -m|-o|-g) shift 2 ;; *) break ;; esac
done
if [ "$directory" = 1 ]; then mkdir -p "$1"; else cp "$1" "$2"; fi
""",
                "nix": f"""#!/bin/sh
while [ $# -gt 0 ]; do
  if [ "$1" = --out-link ]; then ln -s '{prek}' "$2"; fi
  shift
done
""",
            }
            for name, text in scripts.items():
                (mocks / name).write_text(text)
                (mocks / name).chmod(0o755)
            bind = (f"'{sys.executable}' -c 'import socket,sys; socket.socket(socket.AF_UNIX).bind(sys.argv[1])' "
                    '"$2/control.sock"') if opens_socket else "exit 3"
            (prek / "bin/prek").write_text(f"""#!/bin/sh
case "$1" in
  check) echo '{{"status":"valid","mode":"route","rules":0,"selectors":1,"version":1}}' ;;
  broker)
    while [ "$1" != --runtime-dir ]; do shift; done
    mkdir -p "$2" "$2/../../prek/devices"
    {bind}
    echo "routing worker: seat seat0 lost its context: StaleSnapshot" >&2
    echo "routing worker: Prek test keyboard taken over at a neutral idle cut" >&2
    {"" if holds else 'echo "routing worker: seat seat0 lost its context: state Unknown" >&2'}
    sleep 1 ;;
  session) echo "$*" > '{arguments}'; sleep 0.2
    {'exit 1' if agent == "fails" else ''}
    {'echo "context Unknown; previous gesture progress cancelled"' if agent == "leaves" else ''} ;;
esac
""")
            (prek / "bin/prek").chmod(0o755)
            env = {**os.environ, "PATH": f"{mocks}:{os.environ['PATH']}", "XDG_RUNTIME_DIR": temporary,
                   "WAYLAND_DISPLAY": "wayland-test",
                   "PREK_TRIAL_INPUT_DEVICES": str(devices), "PREK_TRIAL_LOGIND": sys.executable,
                   "PREK_TRIAL_RUN_ROOT": str(run_root), "PREK_TRIAL_CGROUP_ROOT": str(cgroups)}
            result = subprocess.run(["bash", str(script), "--device", "Prek test keyboard", "--seconds", "10"],
                                    env=env, capture_output=True, text=True, timeout=60)
            run = (repo / "target/takeover-trial/latest").resolve(strict=True)
            self.assertFalse((run_root / "udev/rules.d/70-prek-trial.rules").exists())
            self.assertEqual((run_root / "udev/rules.d").is_dir(), provisioned)
            self.assertEqual((run_root / "prek/devices").is_dir(), provisioned)
            self.assertEqual((run_root / "prek").is_dir(), provisioned)
            self.assertFalse((run_root / "prek-trial").exists())
            if listeners != 1:
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"Expected exactly one process listening on {temporary}/wayland-test",
                              (run / "console.log").read_text())
                self.assertFalse((run / "broker.log").exists())
                return
            if not opens_socket:
                self.assertEqual(result.returncode, 1)
                self.assertEqual(json.loads((run / "status.json").read_text())["status"], "failed")
                self.assertIn("exited with status 3 before opening its socket", (run / "console.log").read_text())
                self.assertFalse((run / "session.log").exists())
                return
            if not holds:
                self.assertEqual(result.returncode, 1)
                self.assertEqual(json.loads((run / "status.json").read_text())["status"], "failed")
                self.assertIn("lost its context 1 times, so the takeover did not hold",
                              (run / "console.log").read_text())
                return
            if agent != "ok":
                self.assertEqual(result.returncode, 1)
                self.assertEqual(json.loads((run / "status.json").read_text())["status"], "failed")
                expected = ("session agent exited with status 1" if agent == "fails"
                            else "context left Desktop 1 times")
                self.assertIn(expected, (run / "console.log").read_text())
                return
            self.assertEqual(result.returncode, 0, (run / "console.log").read_text())
            self.assertEqual(json.loads((run / "status.json").read_text()), {"status": "passed", "exit_code": 0})
            self.assertIn("taken over at a neutral idle cut", (run / "console.log").read_text())
            self.assertIn("--display wayland-test", arguments.read_text())
            self.assertEqual((run / "session.status").read_text().strip(), "0")
            self.assertEqual(procs.read_text().split(), [(run / "launcher.pid").read_text().strip()])
            self.assertIn('name = "Prek test keyboard"', (run / "config.toml").read_text())
            sudo = calls.read_text().splitlines()
            self.assertEqual(sudo[0], "-v")
            self.assertIn(f"ss -Hxlp src {temporary}/wayland-test", sudo)
            self.assertIn(f"readlink -e /proc/{os.getpid()}/exe", sudo)
            self.assertIn(f"tee -a {procs}", sudo)
            created = f"install -d -m 0755 -o root -g root {run_root}/udev/rules.d"
            self.assertEqual(created in sudo, not provisioned)
            self.assertEqual(f"-n rmdir {run_root}/udev/rules.d" in sudo, not provisioned)
            broker = next(line for line in sudo if " broker " in line)
            self.assertIn("--mode route", broker)
            self.assertIn("--duration 10", broker)
            self.assertIn(f"--compositor-executable {os.path.realpath(sys.executable)}", broker)

    def test_launcher_authenticates_first_and_retains_failed_build_logs(self):
        launcher = Path(__file__).resolve().parents[1] / "run-root-routing.sh"
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            (repo / "tests").mkdir()
            script = repo / "tests/run-root-routing.sh"
            shutil.copyfile(launcher, script)
            calls, mocks = repo / "sudo-calls", repo / "bin"
            mocks.mkdir()
            (mocks / "sudo").write_text(f"#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{calls}'\n")
            (mocks / "sudo").chmod(0o755)
            # All mocks are unprivileged commands. Nix fails before a runner
            # exists, so only the up-front credential check is requested.
            shell = "git(){ return 0; }; nix(){ echo 'deliberate pre-build refusal'; return 77; }; export -f git nix; exec bash \"$1\""
            result = subprocess.run(["bash", "-c", shell, "--", str(script)], env={**os.environ, "PATH": f"{mocks}:{os.environ['PATH']}"},
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 77)
            self.assertEqual(calls.read_text(), "-v\n")
            retained = (repo / "target/root-routing/latest").resolve(strict=True)
            self.assertEqual(json.loads((retained / "status.json").read_text()), {"status":"failed", "exit_code":77})
            self.assertIn("deliberate pre-build refusal", (retained / "console.log").read_text())
            self.assertFalse((retained / "receipt.json").exists())
            self.assertFalse((retained / "kernel.log").exists())


class RootRunModel(unittest.TestCase):
    """Privileged orchestration models use only test-owned temporary directories."""

    @contextlib.contextmanager
    def model(self, effect=None):
        with tempfile.TemporaryDirectory() as temporary, contextlib.ExitStack() as stack:
            path = Path(temporary)
            registry, rules = path / "devices", path / "rules"
            registry.mkdir()
            rules.mkdir()
            (registry / "foreign-record").write_text("preserved")
            fds = [os.open(directory, os.O_DIRECTORY | os.O_RDONLY) for directory in (path, registry, rules)]
            for fd in fds:
                stack.callback(os.close, fd)
            actual_stat = os.stat
            def metadata(name, *args, **kwargs):
                if name == "/dev/uinput":
                    return argparse.Namespace(st_mode=stat.S_IFCHR)
                return actual_stat(name, *args, **kwargs)
            def command(argv, **kwargs):
                if argv[0] == "/nix/store/model/routing":
                    return subprocess.CompletedProcess(argv, 0, "\n".join(name + ": test" for name in sorted(root.EXPECTED)))
                if argv[0] == "/nix/store/model/recovery":
                    return subprocess.CompletedProcess(argv, 0, recovery_inventory())
                if argv[0] == "/nix/store/model/managed":
                    self.assertEqual(argv[1:], ["--inventory"])
                    return subprocess.CompletedProcess(argv, 0, root.MANAGED_INVENTORY + "\n" + json.dumps([root.MANAGED_CASE]) + "\n")
                if argv[0] == "/nix/store/model/native":
                    self.assertIn(argv[1], (root.REPEAT_CASE, root.FUZZ_CASE, root.KEY_OUTPUT_CASE))
                    self.assertEqual(argv[2:], ["--exact", "--list", "--ignored"])
                    return subprocess.CompletedProcess(argv, 0, argv[1] + ": test\n")
                self.assertEqual(argv, ["/nix/store/model/udevadm", "control", "--reload-rules"])
                self.assertEqual(kwargs["env"], {})
                return subprocess.CompletedProcess(argv, 0)
            def execute(argv, env):
                self.assertEqual(env["PREK_ISOLATED_ROUTING"], "1")
                self.assertEqual(len(list(rules.iterdir())), 1)
                if effect is not None:
                    effect(argv[0], path, registry, rules)
                if argv[0] == "/nix/store/model/managed":
                    self.assertEqual(argv[1:], ["--run"])
                    return managed_output()
                if argv[0] == "/nix/store/model/native":
                    self.assertIn(argv[1], (root.REPEAT_CASE, root.FUZZ_CASE, root.KEY_OUTPUT_CASE))
                    self.assertEqual(argv[2:], ["--exact", "--ignored", "--nocapture", "--test-threads=1"])
                    return {root.REPEAT_CASE: repeat_output, root.FUZZ_CASE: fuzz_output, root.KEY_OUTPUT_CASE: key_output}[argv[1]]()
                if argv[0] == "/nix/store/model/recovery":
                    control = path / Path(env["PREK_RECOVERY_DIRECTORY"]).name
                    info = control.stat()
                    self.assertEqual(env["PREK_RECOVERY_IDENTITY"], f"{info.st_dev}:{info.st_ino}")
                    return recovery_output()
                return f"test result: ok. {len(root.EXPECTED)} passed; 0 failed; 0 ignored;\n"
            stack.enter_context(mock.patch.object(root.os, "geteuid", return_value=0))
            stack.enter_context(mock.patch.object(root.Path, "resolve", lambda value, **_: value))
            stack.enter_context(mock.patch.object(root.Path, "is_file", return_value=True))
            stack.enter_context(mock.patch.object(root.os, "stat", side_effect=metadata))
            stack.enter_context(mock.patch.object(root, "directories", return_value=contextlib.nullcontext(tuple(fds))))
            commands = stack.enter_context(mock.patch.object(root.subprocess, "run", side_effect=command))
            stack.enter_context(mock.patch.object(root, "execute", side_effect=execute))
            output = stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
            args = argparse.Namespace(tests="/nix/store/model/routing", recovery="/nix/store/model/recovery", native="/nix/store/model/native", managed="/nix/store/model/managed",
                                      importer="/nix/store/model/importer", udevadm="/nix/store/model/udevadm")
            yield args, path, registry, rules, commands, output

    def test_model_success_correlates_all_executables_and_removes_controls(self):
        with self.model() as (args, path, registry, rules, commands, output):
            root.run(args)
            self.assertEqual(root.receipt(output.getvalue())["passed"], 27)
            self.assertEqual({entry.name for entry in path.iterdir()}, {"devices", "rules"})
            self.assertEqual(list(rules.iterdir()), [])
            self.assertEqual((registry / "foreign-record").read_text(), "preserved")
            self.assertEqual(commands.call_count, 8)

    def test_model_all_executable_failures_remove_only_original_rule(self):
        for failure in ["routing", "recovery", "native", "managed"]:
            def fail(executable, *_):
                if executable.endswith("/" + failure):
                    raise RuntimeError("modeled executable failure")
            with self.model(fail) as (args, _, registry, rules, commands, output):
                with self.subTest(failure=failure), self.assertRaisesRegex(RuntimeError, "modeled executable failure"):
                    root.run(args)
                self.assertEqual(list(rules.iterdir()), [])
                self.assertEqual((registry / "foreign-record").read_text(), "preserved")
                self.assertEqual(commands.call_count, 8)
                self.assertNotIn(root.PREFIX, output.getvalue())

    def test_model_malformed_repeat_evidence_removes_the_rule_without_acceptance(self):
        with self.model() as (args, _, registry, rules, commands, output):
            with mock.patch.object(root, "repeat_result", side_effect=ValueError("invalid repeat evidence")):
                with self.assertRaisesRegex(ValueError, "invalid repeat evidence"):
                    root.run(args)
            self.assertEqual(list(rules.iterdir()), [])
            self.assertEqual((registry / "foreign-record").read_text(), "preserved")
            self.assertEqual(commands.call_count, 8)
            self.assertNotIn(root.PREFIX, output.getvalue())

    def test_model_malformed_fuzz_evidence_removes_the_rule_without_acceptance(self):
        with self.model() as (args, _, registry, rules, commands, output):
            with mock.patch.object(root, "fuzz_result", side_effect=ValueError("invalid fuzz evidence")):
                with self.assertRaisesRegex(ValueError, "invalid fuzz evidence"):
                    root.run(args)
            self.assertEqual(list(rules.iterdir()), [])
            self.assertEqual((registry / "foreign-record").read_text(), "preserved")
            self.assertEqual(commands.call_count, 8)
            self.assertNotIn(root.PREFIX, output.getvalue())

    def test_model_malformed_generated_output_cleans_its_rule_without_acceptance(self):
        with self.model() as (args, _, registry, rules, commands, output):
            with mock.patch.object(root, "key_output_result", side_effect=ValueError("invalid generated output evidence")):
                with self.assertRaisesRegex(ValueError, "invalid generated output evidence"):
                    root.run(args)
            self.assertEqual(list(rules.iterdir()), [])
            self.assertEqual((registry / "foreign-record").read_text(), "preserved")
            self.assertEqual(commands.call_count, 8)
            self.assertNotIn(root.PREFIX, output.getvalue())

    def test_model_malformed_managed_output_cleans_its_rule_without_acceptance(self):
        with self.model() as (args, _, registry, rules, commands, output):
            with mock.patch.object(root, "managed_result", side_effect=ValueError("invalid managed evidence")):
                with self.assertRaisesRegex(ValueError, "invalid managed evidence"):
                    root.run(args)
            self.assertEqual(list(rules.iterdir()), [])
            self.assertEqual((registry / "foreign-record").read_text(), "preserved")
            self.assertEqual(commands.call_count, 8)
            self.assertNotIn(root.PREFIX, output.getvalue())

    def test_model_registry_changes_are_retained_and_cannot_pass(self):
        def change(executable, _, registry, rules):
            if executable.endswith("/recovery"):
                (registry / "foreign-record").write_text("external change")
        with self.model(change) as (args, _, registry, rules, _, output):
            with self.assertRaisesRegex(RuntimeError, "unrelated registry state changed"):
                root.run(args)
            self.assertEqual((registry / "foreign-record").read_text(), "external change")
            self.assertEqual(list(rules.iterdir()), [])
            self.assertNotIn(root.PREFIX, output.getvalue())

    def test_model_replaced_rule_is_retained_and_cannot_pass(self):
        def replace(executable, path, _, rules):
            if executable.endswith("/recovery"):
                rule = next(rules.iterdir())
                rule.rename(path / "retained-original-rule")
                rule.write_text("foreign replacement")
        with self.model(replace) as (args, path, registry, rules, _, output):
            with self.assertRaisesRegex(RuntimeError, "replaced root test path"):
                root.run(args)
            self.assertEqual(next(rules.iterdir()).read_text(), "foreign replacement")
            self.assertTrue((path / "retained-original-rule").exists())
            self.assertEqual((registry / "foreign-record").read_text(), "preserved")
            self.assertNotIn(root.PREFIX, output.getvalue())


if __name__ == "__main__":
    unittest.main()
