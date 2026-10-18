import importlib.util
import copy
from pathlib import Path
import unittest


spec = importlib.util.spec_from_file_location("cage_inner", Path(__file__).parents[1] / "cage-inner.py")
inner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inner)

host_spec = importlib.util.spec_from_file_location("cage_host", Path(__file__).parents[1] / "cage-host.py")
host = importlib.util.module_from_spec(host_spec)
host_spec.loader.exec_module(host)

quiesce_spec = importlib.util.spec_from_file_location("keyboard_quiesce", Path(__file__).parent / "keyboard_quiesce.py")
quiesce = importlib.util.module_from_spec(quiesce_spec)
quiesce_spec.loader.exec_module(quiesce)


class CustomKeyArbitration(unittest.TestCase):
    def fixture(self):
        observations = [dict(name="unused", receipt=dict(sequence=(index + 1) * 10)) for index in range(39)]
        observations[31]["name"] = "custom-lua-unseen-key-tail"
        received = [dict(event="key", sequence=311 + index, code=code, state=state, at_ns=1000 + index)
                    for index, (code, state) in enumerate([(31, 1), (31, 0), (30, 1), (30, 0)])]
        source = dict(device=500, generation=1, seat="seat0")
        def stamp(nanos):
            return dict(secs=0, nanos=nanos)
        inputs = [dict(source=source, sequence=index + 1, at=stamp(at),
                       events=[dict(kind=1, code=code, value=value) for code, value in events] + [dict(kind=0, code=0, value=0)])
                  for index, (at, events) in enumerate([(100, [(202, 1)]), (120, [(31, 1), (31, 0)]), (126, [(202, 0)])])]
        ticket = dict(id=1, cause="frame", rule="rendered-custom-key", recognizer="rendered_key", fence=dict(configuration=1),
                      requested_at=stamp(100), deadline=stamp(200), input=dict(source=source, sequence=1, at=stamp(100),
                      changes=[dict(kind="key", code=202, state="press")]))
        runtime = dict(status="passed", scope="owned_protocol_root_key_only_custom", actual_lua_callback=True,
                       source_and_readiness_modeled=True, physical_takeover_tested=False, kernel_input_opened=False,
                       helper_stopped=True, generated_neutral=True, ordinary=[[31, 1], [31, 0]], inputs=inputs, ticket=ticket,
                       activation=dict(require_keys=[], keys_throughout=False),
                       trigger=dict(kind="custom", recognizer=dict(name="rendered_key", selectors=["custom_keys"], inputs=["key"])),
                       work_admitted_at=stamp(125), reply_dispatches=1,
                       proposal=dict(ticket=1, fence=ticket["fence"], phase="discrete", data=dict(trigger=202)),
                       proposed_at=stamp(110), admitted_at=stamp(125))
        return observations, received, runtime

    def test_original_callback_and_actual_selective_edges_pass(self):
        result = quiesce.validate_custom_key_arbitration(*self.fixture())
        self.assertEqual((result["consumed_control_edges"], result["preserved_ordinary_edges"]), (2, 2))
        self.assertFalse(result["physical_takeover_tested"])

    def test_leaked_missing_duplicated_or_reordered_edges_refuse(self):
        for change in ("leak", "missing", "duplicate", "reorder", "bool"):
            observations, received, runtime = self.fixture()
            if change == "leak":
                received.insert(0, dict(received[0], code=202))
            elif change == "missing":
                received.pop()
            elif change == "duplicate":
                received.append(dict(received[-1]))
            elif change == "bool":
                received[0]["state"] = True
            else:
                received[0], received[2] = received[2], received[0]
            with self.subTest(change=change), self.assertRaises(ValueError):
                quiesce.validate_custom_key_arbitration(observations, received, runtime)

    def test_false_scope_and_incomplete_cleanup_refuse(self):
        for field in ("actual_lua_callback", "source_and_readiness_modeled", "helper_stopped", "generated_neutral"):
            for value in (False, 1):
                observations, received, runtime = self.fixture()
                runtime[field] = value
                with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                    quiesce.validate_custom_key_arbitration(observations, received, runtime)

    def test_missing_control_input_and_widened_or_foreign_ticket_refuse(self):
        for change in ("control", "widened", "foreign", "proposal"):
            observations, received, runtime = self.fixture()
            if change == "control":
                runtime["inputs"][0]["events"].clear()
            elif change == "widened":
                runtime["ticket"]["input"]["changes"].append(dict(kind="key", code=31, state="press"))
            elif change == "foreign":
                runtime["ticket"]["input"]["source"] = dict(device=501, generation=1, seat="seat0")
            else:
                runtime["proposal"]["ticket"] = 2
            with self.subTest(change=change), self.assertRaises(ValueError):
                quiesce.validate_custom_key_arbitration(observations, received, runtime)

    def test_original_unseen_boundary_and_clocks_are_required(self):
        for change in ("no_unseen", "late", "bool", "negative", "nanos"):
            observations, received, runtime = self.fixture()
            field = "admitted_at" if change == "late" else "proposed_at"
            runtime[field] = dict(secs=0, nanos={"no_unseen":121, "late":201, "bool":True, "negative":-1, "nanos":1_000_000_000}[change])
            with self.subTest(change=change), self.assertRaises(ValueError):
                quiesce.validate_custom_key_arbitration(observations, received, runtime)

    def test_missing_or_reversed_rendered_phase_refuses(self):
        for missing in (False, True):
            observations, received, runtime = self.fixture()
            if missing:
                observations.pop()
            else:
                observations[31]["receipt"]["sequence"] = 300
            with self.subTest(missing=missing), self.assertRaises(ValueError):
                quiesce.validate_custom_key_arbitration(observations, received, runtime)


class CustomKeyActivation(unittest.TestCase):
    def fixture(self):
        observations, _, runtime = CustomKeyArbitration().fixture()
        observations[32]["name"] = "custom-lua-event-local-activation"
        received = [dict(event="key", sequence=321 + index, code=code, state=state, at_ns=1000 + index)
                    for index, (code, state) in enumerate([(31, 1), (31, 0), (31, 1), (31, 0), (30, 1), (30, 0)])]
        runtime["activation"] = dict(require_keys=[42], keys_throughout=False)
        runtime["ordinary"] *= 2
        runtime["inputs"][0]["events"] = [dict(kind=1, code=code, value=value)
                                          for code, value in [(31, 1), (31, 0), (42, 1), (202, 1), (42, 0)]] + [dict(kind=0, code=0, value=0)]
        runtime["ticket"]["input"]["changes"] = [dict(kind="key", code=code, state=state)
                                                 for code, state in [(42, "press"), (202, "press"), (42, "release")]]
        return observations, received, runtime

    def test_same_report_activation_and_later_release_preserve_earlier_and_unseen_typing(self):
        result = quiesce.validate_custom_key_arbitration(*self.fixture(), event_local=True)
        self.assertEqual((result["consumed_control_edges"], result["preserved_ordinary_edges"]), (4, 4))
        self.assertTrue(result["event_local_activation_tested"])
        self.assertFalse(result["physical_takeover_tested"])

    def test_including_pre_activation_input_or_losing_the_modifier_release_refuses(self):
        for change in ("earlier", "release", "reorder", "frame"):
            observations, received, runtime = self.fixture()
            changes = runtime["ticket"]["input"]["changes"]
            if change == "earlier":
                changes.insert(0, dict(kind="key", code=31, state="press"))
            elif change == "release":
                changes.pop()
            elif change == "reorder":
                changes.reverse()
            else:
                runtime["ticket"]["input"]["sequence"] = 2
            with self.subTest(change=change), self.assertRaises(ValueError):
                quiesce.validate_custom_key_arbitration(observations, received, runtime, event_local=True)

    def test_unconfigured_or_substituted_activation_policy_refuses(self):
        for policy in (dict(require_keys=[], keys_throughout=False), dict(require_keys=[42], keys_throughout=True),
                       dict(require_keys=[42.0], keys_throughout=False), dict(require_keys=[42], keys_throughout=0)):
            observations, received, runtime = self.fixture()
            runtime["activation"] = policy
            with self.subTest(policy=policy), self.assertRaises(ValueError):
                quiesce.validate_custom_key_arbitration(observations, received, runtime, event_local=True)

    def test_leaked_controls_lost_typing_or_early_generated_action_refuse(self):
        for change in ("modifier", "trigger", "missing", "reorder"):
            observations, received, runtime = self.fixture()
            if change in ("modifier", "trigger"):
                received.insert(0, dict(received[0], code=42 if change == "modifier" else 202))
            elif change == "missing":
                received.pop(0)
            else:
                received[0]["code"] = 30
            with self.subTest(change=change), self.assertRaises(ValueError):
                quiesce.validate_custom_key_arbitration(observations, received, runtime, event_local=True)


class ComposedCustomKeys(unittest.TestCase):
    def fixture(self, composition):
        observations, _, runtime = CustomKeyArbitration().fixture()
        native_first = composition == "native_prefix"
        phase = 33 if native_first else 34
        observations[phase]["name"] = "custom-lua-native-prefix" if native_first else "custom-lua-native-suffix"
        custom = runtime["trigger"]
        native = dict(kind="key", key="code:202" if native_first else "code:203")
        runtime["trigger"] = dict(kind="sequence", timeout="200ms", steps=[native, custom] if native_first else [custom, native])
        runtime["scope"] = "owned_protocol_composed_key_only_custom"
        def events(edges):
            return [dict(kind=1, code=code, value=value) for code, value in edges] + [dict(kind=0, code=0, value=0)]
        if native_first:
            runtime["ordinary"] *= 2
            runtime["inputs"][0]["events"] = events([(31, 1), (31, 0), (202, 1)])
        else:
            runtime["ordinary"] = [[203, 1], [203, 0], [31, 1], [31, 0]]
            runtime["inputs"][1]["events"] = events(runtime["ordinary"])
            released = runtime["inputs"][-1]
            fresh = copy.deepcopy(released)
            fresh["events"] = events([(203, 1), (203, 0)])
            released["sequence"] = 4
            released["at"] = dict(secs=0, nanos=128)
            runtime["inputs"].insert(2, fresh)
            runtime["work_admitted_at"] = dict(secs=0, nanos=127)
            runtime["reply_dispatches"] = 0
        received = [dict(event="key", sequence=phase * 10 + 1 + index, code=code, state=state, at_ns=1000 + index)
                    for index, (code, state) in enumerate(runtime["ordinary"] + [[30, 1], [30, 0]])]
        return observations, received, runtime

    def test_actual_native_custom_and_custom_native_compositions_pass(self):
        for composition in ("native_prefix", "native_suffix"):
            result = quiesce.validate_custom_key_arbitration(*self.fixture(composition), composition=composition)
            self.assertEqual(result["composition"], composition)
            self.assertEqual(result["preserved_ordinary_edges"], 4)
            self.assertFalse(result["physical_takeover_tested"])

    def test_substituting_a_root_trigger_order_or_native_key_refuses(self):
        for composition in ("native_prefix", "native_suffix"):
            for change in ("root", "order", "key", "input_kind"):
                observations, received, runtime = self.fixture(composition)
                if change == "root":
                    runtime["trigger"] = next(step for step in runtime["trigger"]["steps"] if step["kind"] == "custom")
                elif change == "order":
                    runtime["trigger"]["steps"].reverse()
                elif change == "key":
                    next(step for step in runtime["trigger"]["steps"] if step["kind"] == "key")["key"] = "code:31"
                else:
                    next(step for step in runtime["trigger"]["steps"] if step["kind"] == "custom")["recognizer"]["inputs"] = ["touch"]
                with self.subTest(composition=composition, change=change), self.assertRaises(ValueError):
                    quiesce.validate_custom_key_arbitration(observations, received, runtime, composition=composition)

    def test_native_suffix_cannot_dispatch_at_reply_or_before_fresh_input(self):
        for change in ("early", "bool", "clock", "missing"):
            observations, received, runtime = self.fixture("native_suffix")
            if change == "early":
                runtime["reply_dispatches"] = 1
            elif change == "bool":
                runtime["reply_dispatches"] = False
            elif change == "clock":
                runtime["work_admitted_at"] = runtime["admitted_at"]
            else:
                runtime["inputs"].pop(2)
            with self.subTest(change=change), self.assertRaises(ValueError):
                quiesce.validate_custom_key_arbitration(observations, received, runtime, composition="native_suffix")

    def test_earlier_typing_leaked_controls_or_hybrid_report_substitution_refuses(self):
        for composition in ("native_prefix", "native_suffix"):
            for change in ("typing", "control", "hybrid", "report"):
                observations, received, runtime = self.fixture(composition)
                if change == "typing":
                    received.pop(0)
                elif change == "control":
                    received.insert(0, dict(received[0], code=202))
                elif change == "hybrid":
                    runtime["inputs"][0]["events"].insert(0, dict(kind=2, code=0, value=1))
                else:
                    runtime["inputs"][0]["events"].pop()
                with self.subTest(composition=composition, change=change), self.assertRaises(ValueError):
                    quiesce.validate_custom_key_arbitration(observations, received, runtime, composition=composition)


class NativeKeyArbitration(unittest.TestCase):
    def fixture(self):
        observations = [dict(name="unused", receipt=dict(sequence=(index + 1) * 10)) for index in range(39)]
        cases = [
            ("buffered-tap-selective", [(31, 1), (31, 0), (30, 1), (30, 0)]),
            ("buffered-hold-priority", [(30, 1), (30, 0)]),
            ("buffered-sequence-selective", [(30, 1), (30, 0), (31, 1), (31, 0)]),
            ("buffered-deferred-key-sequence", [(30, 1), (30, 0)]),
            ("buffered-unmatched-tap-fallback", [(31, 1), (31, 0)]),
            ("buffered-deferred-action-order", [(30, 1), (30, 0), (48, 1), (48, 0)]),
        ]
        received = []
        for index, (name, edges) in enumerate(cases, 25):
            observations[index]["name"] = name
            for offset, (code, state) in enumerate(edges, 1):
                sequence = index * 10 + offset
                received.append(dict(event="key", sequence=sequence, code=code, state=state, at_ns=sequence * 1_000_000))
        received[-6]["at_ns"], received[-5]["at_ns"] = 500_000_000, 540_000_000
        controls = [(195, 1), (195, 0), (195, 1), (195, 0), (197, 1), (197, 0),
                    (198, 1), (198, 0), (199, 1), (199, 0), (200, 1), (200, 0),
                    (199, 1), (199, 0), (200, 1), (200, 0), (201, 1), (201, 0)]
        inputs = [dict(at=dict(secs=0, nanos=100_000_000), events=[dict(kind=1, code=code, value=value) for code, value in controls[:12]])]
        inputs.extend(dict(at=dict(secs=0, nanos=at), events=[dict(kind=1, code=31, value=value)])
                      for at, value in [(200_000_000, 1), (240_000_000, 0)])
        for index in range(3):
            inputs.append(dict(at=dict(secs=0, nanos=300_000_000 + index * 1_000_000),
                               events=[dict(kind=1, code=code, value=value) for code, value in controls[12 + index * 2:14 + index * 2]]))
        return observations, received, inputs

    def test_exact_owned_edges_and_original_cadence_pass(self):
        result = quiesce.validate_native_key_arbitration(*self.fixture())
        self.assertEqual((result["cases"], result["consumed_control_edges"], result["leaked_control_edges"]), (6, 18, 0))
        self.assertFalse(result["physical_takeover_tested"])

    def test_leaked_missing_duplicated_and_reordered_events_refuse(self):
        for change in ("leak", "missing", "duplicate", "reorder"):
            observations, received, inputs = self.fixture()
            if change == "leak":
                received.insert(0, dict(received[0], code=195))
            elif change == "missing":
                received.pop(0)
            elif change == "duplicate":
                received.insert(0, dict(received[0]))
            else:
                received[0], received[2] = received[2], received[0]
            with self.subTest(change=change), self.assertRaises(ValueError):
                quiesce.validate_native_key_arbitration(observations, received, inputs)

    def test_bool_key_fields_refuse(self):
        for field in ("sequence", "code", "state", "at_ns"):
            observations, received, inputs = self.fixture()
            received[0][field] = True
            with self.subTest(field=field), self.assertRaises(ValueError):
                quiesce.validate_native_key_arbitration(observations, received, inputs)

    def test_deferred_order_requires_a_real_overlap_before_settlement(self):
        for change in ("late", "same_report", "reordered"):
            observations, received, inputs = self.fixture()
            if change == "late":
                inputs[-1]["at"]["nanos"] = 321_000_000
            elif change == "same_report":
                inputs[-2]["events"].extend(inputs.pop()["events"])
            else:
                received[-4], received[-2] = received[-2], received[-4]
            with self.subTest(change=change), self.assertRaises(ValueError):
                quiesce.validate_native_key_arbitration(observations, received, inputs)

    def test_missing_original_control_edges_refuse(self):
        observations, received, inputs = self.fixture()
        inputs[0]["events"].pop()
        with self.assertRaises(ValueError):
            quiesce.validate_native_key_arbitration(observations, received, inputs)

    def test_burst_fallback_refuses(self):
        observations, received, inputs = self.fixture()
        received[-5]["at_ns"] = received[-6]["at_ns"] + 1
        with self.assertRaises(ValueError):
            quiesce.validate_native_key_arbitration(observations, received, inputs)

    def test_missing_and_reversed_capture_boundaries_refuse(self):
        for missing in (False, True):
            observations, received, inputs = self.fixture()
            if missing:
                observations.pop()
            else:
                observations[25]["receipt"]["sequence"] = 1
            with self.subTest(missing=missing), self.assertRaises(ValueError):
                quiesce.validate_native_key_arbitration(observations, received, inputs)


class KeyboardOwnership(unittest.TestCase):
    def receipt(self):
        names = ["persistent-held", "second-owner-first-release-one-owner", "first-owner-first-release-one-owner",
                 "independent-unmodified", "retired-one-owner", "neutral-after"]
        return dict(version=1, status="passed", source="two_owned_wayland_virtual_keyboards", config_provider="lua",
                    final_neutral=True, production_route_tested=False, kernel_input_tested=False,
                    unmodified_stock_acceptance=False, keyboards=[dict(address="0xa"), dict(address="0xb")],
                    phases=quiesce.ownership_expectations(),
                    captures=[dict(phase=f"ownership-{name}", attempts=1, through=10 + index, sha256="a" * 64)
                              for index, name in enumerate(names)])

    def test_complete_scoped_receipt_and_both_dialects(self):
        for provider in ("lua", "hyprlang"):
            value = self.receipt()
            value["config_provider"] = provider
            self.assertEqual(quiesce.validate_ownership_result(value), value)

    def test_partial_receipts_and_authority_expansion_are_rejected(self):
        receipt = self.receipt()
        for missing in (None, {}, [], True):
            with self.subTest(missing=missing), self.assertRaises(ValueError):
                quiesce.validate_ownership_result(missing)
        for key in receipt:
            value = copy.deepcopy(receipt)
            del value[key]
            with self.subTest(missing=key), self.assertRaises(ValueError):
                quiesce.validate_ownership_result(value)
        for change in [dict(production_route_tested=True), dict(kernel_input_tested=True), dict(unmodified_stock_acceptance=True),
                       dict(final_neutral=False), dict(version=True), dict(status="observed"), dict(config_provider="other"),
                       dict(keyboards=[]), dict(keyboards=[dict(address="0xa")] * 2), dict(keyboards=[{}, {}]),
                       dict(phases=[]), dict(phases=list(reversed(receipt["phases"]))), dict(captures=[])]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                quiesce.validate_ownership_result(dict(receipt, **change))

    def test_each_phase_requires_exact_keys_text_modifiers_and_ordered_delivery(self):
        receipt = self.receipt()
        for index, phase in enumerate(receipt["phases"]):
            changes = [dict(text=phase["text"] + "a"), dict(keys=[125]), dict(keyboard=True), dict(emitted=[]),
                       dict(delivered=phase["delivered"] + [[125, 0]]), dict(operation="other"), dict(modifiers=dict(depressed=0))]
            if phase["emitted"] == []:
                changes[3] = dict(emitted=[[42, 0]])
            for change in changes:
                value = copy.deepcopy(receipt)
                value["phases"][index].update(change)
                with self.subTest(phase=phase["phase"], change=change), self.assertRaises(ValueError):
                    quiesce.validate_ownership_result(value)

    def test_capture_receipts_require_distinct_ordered_real_frames(self):
        receipt = self.receipt()
        for change in [dict(attempts=True), dict(attempts=0), dict(attempts=65), dict(through=True),
                       dict(through=0), dict(sha256="invalid"), dict(phase="other")]:
            value = copy.deepcopy(receipt)
            value["captures"][0].update(change)
            with self.subTest(change=change), self.assertRaises(ValueError):
                quiesce.validate_ownership_result(value)
        for index in range(1, len(receipt["captures"])):
            value = copy.deepcopy(receipt)
            value["captures"][index]["through"] = value["captures"][index - 1]["through"]
            with self.subTest(index=index), self.assertRaises(ValueError):
                quiesce.validate_ownership_result(value)


class KeyboardQuiescence(unittest.TestCase):
    def test_incomplete_or_positive_handoff_claims_cannot_pass_characterization(self):
        value = dict(version=1, status="passed", source="owned_wayland_virtual_keyboard", config_provider="lua",
                     device_disable_is_neutral=False, disabled_release_dropped=True, reenabled_press_dropped=True,
                     final_neutral=True, production_route_tested=False, kernel_input_tested=False, unmodified_stock_acceptance=False,
                     captures=[dict(phase=phase, attempts=1, through=10, sha256="a" * 64) for phase in
                               ["neutral-before", "held-prefix", "disabled-release", "reenabled-press", "neutral-after"]])
        self.assertEqual(quiesce.validate_result(value), value)
        for missing in [None, {}, [], True]:
            with self.subTest(missing=missing), self.assertRaises(ValueError):
                quiesce.validate_result(missing)
        for key in value:
            missing = dict(value)
            del missing[key]
            with self.subTest(missing=key), self.assertRaises(ValueError):
                quiesce.validate_result(missing)
        for change in [dict(production_route_tested=True), dict(kernel_input_tested=True), dict(unmodified_stock_acceptance=True),
                       dict(final_neutral=False), dict(version=True), dict(captures=[]), dict(captures=value["captures"] * 2),
                       dict(captures=list(reversed(value["captures"])))]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                quiesce.validate_result(dict(value, **change))

    def test_config_dialects_and_device_name_are_explicit(self):
        self.assertEqual(quiesce.enabled_arguments("lua", "virtual-keyboard-1", False),
                         ["eval", 'hl.device({name="virtual-keyboard-1",enabled=false})'])
        self.assertEqual(quiesce.enabled_arguments("hyprlang", "virtual-keyboard-1", True),
                         ["keyword", "device[virtual-keyboard-1]:enabled", "true"])
        for provider, name, enabled in [("other", "virtual-keyboard", True), ("lua", 'x";exit', True),
                                        ("hyprlang", "x]:enabled", True), ("lua", "", True),
                                        ("lua", "x" * 129, True), ("lua", "keyboard", 1)]:
            with self.subTest(provider=provider, name=name), self.assertRaises(ValueError):
                quiesce.enabled_arguments(provider, name, enabled)

    def test_old_request_with_a_later_callback_is_not_a_post_producer_barrier(self):
        rows = [{"event": "sync", "requested_ns": 99, "sequence": 100},
                {"event": "sync", "requested_ns": 100, "sequence": 101},
                {"event": "rendered", "requested_ns": 102, "sequence": 102},
                {"event": "sync", "requested_ns": True, "sequence": 103}]
        self.assertIsNone(quiesce.snapshot_after(rows, 100))
        current = dict(event="sync", requested_ns=101, sequence=104)
        self.assertEqual(quiesce.snapshot_after(rows + [current], 100), current)

    def test_pixels_require_both_application_identity_and_matching_held_state(self):
        width, height = 64, 32
        pixels = bytearray(width * height * 3)
        for x, color in [(12, (0, 0, 255)), (28, (255, 0, 0))]:
            start = (12 * width + x) * 3
            pixels[start:start + 3] = bytes(color)
        self.assertTrue(quiesce.check_pixels(pixels, width, height, [0, 0], True))
        self.assertFalse(quiesce.check_pixels(pixels, width, height, [0, 0], False))
        pixels[(12 * width + 12) * 3:(12 * width + 12) * 3 + 3] = bytes((255, 0, 0))
        self.assertFalse(quiesce.check_pixels(pixels, width, height, [0, 0], True))
        self.assertFalse(quiesce.check_pixels(bytes(len(pixels)), width, height, [0, 0], True))

    def test_capture_bounds_and_coordinates_are_not_guessed(self):
        for width, height, position, held in [(64, 32, [64, 0], True), (0, 32, [0, 0], True),
                                             (64, 32, [0.5, 0], True), (64, 32, [0, 0], 1),
                                             (64, 32, [0], True)]:
            with self.subTest(position=position, held=held), self.assertRaises(ValueError):
                quiesce.check_pixels(bytes(64 * 32 * 3), width, height, position, held)
        with self.assertRaises(ValueError):
            quiesce.check_pixels(b"", 64, 32, [0, 0], True)


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

    def test_declared_stock_capture_verifies_the_exact_package_without_an_override(self):
        package = "/nix/store/stock-aquamarine"
        library = f"{package}/lib/libaquamarine.so.0.15.1"
        self.assertEqual(inner.aquamarine_libraries(self.mapping(library), stock=package),
                         dict(declared_test_override=None, declared_stock=package,
                              loaded_libraries=[library]))

    def test_stock_claim_refuses_missing_patched_mixed_deleted_and_prefix_mappings(self):
        package = "/nix/store/stock-aquamarine"
        library = f"{package}/lib/libaquamarine.so.0.15.1"
        patched = "/nix/store/test-aquamarine/lib/libaquamarine.so.0.15.1"
        for maps in ("", self.mapping(patched), self.mapping(library) + self.mapping(patched),
                     self.mapping(library + " (deleted)"),
                     self.mapping(package + "-foreign/lib/libaquamarine.so.0.15.1")):
            with self.subTest(maps=maps), self.assertRaises(RuntimeError):
                inner.aquamarine_libraries(maps, stock=package)

    def test_stock_and_patched_claims_cannot_be_combined(self):
        with self.assertRaises(RuntimeError):
            inner.aquamarine_libraries("", "/nix/store/patched", "/nix/store/stock")

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
