use super::domains::{hybrid, hybrid_budget, ms, raw, request};
use super::*;
use crate::{
    config::Millis,
    input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, EV_ABS, EV_KEY,
        EV_REL,
    },
    recognition::{InputKind, Phase, Proposal},
};

fn reports(intent: &RouteIntent) -> BTreeSet<(u64, u64, usize)> {
    intent
        .tail_reports()
        .map(|report| {
            (
                report.source.device,
                report.position.frame,
                report.position.event,
            )
        })
        .collect()
}
fn reply(engine: &mut Engine, ticket: &crate::recognition::Ticket, phase: Phase, at: u64) {
    engine
        .recognition_response(
            Proposal {
                fence: ticket.fence,
                ticket: ticket.id,
                phase,
                data: serde_json::json!({}),
            },
            ms(at),
        )
        .unwrap();
}

#[test]
fn mixed_tickets_freeze_original_report_evidence_instead_of_newer_candidate_input() {
    let mut engine = hybrid(&[InputKind::Key, InputKind::Relative], false);
    raw(&mut engine, 1, 10, &[(EV_KEY, 30, 1), (EV_REL, 0, 4)]);
    let saved = engine.route_intents().pop().unwrap();
    let watermark = engine.input_watermark();
    assert_eq!(reports(&saved), BTreeSet::from([(1, watermark, 2)]));
    let first = request(&mut engine, 10);
    raw(&mut engine, 2, 20, &[(EV_REL, 0, 8)]);
    assert_eq!(reports(&saved).len(), 1);
    assert_eq!(reports(&engine.route_intents().pop().unwrap()).len(), 2);
    reply(&mut engine, &first, Phase::Discrete, 21);
    let outcomes = engine.take_route_outcomes();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(reports(&outcomes[0].intent), reports(&saved));
    assert_eq!(positions(&outcomes[0].intent), positions(&saved));
}

#[test]
fn timers_use_only_confirmed_physical_report_evidence() {
    let mut engine = hybrid(&[InputKind::Key, InputKind::Relative], false);
    raw(&mut engine, 1, 10, &[(EV_KEY, 30, 1), (EV_REL, 0, 4)]);
    let saved = engine.route_intents().pop().unwrap();
    let first = request(&mut engine, 10);
    reply(&mut engine, &first, Phase::Pending, 11);
    engine.push(Record::Tick { at: ms(61) }).unwrap();
    let timer = request(&mut engine, 61);
    assert_eq!(timer.cause, crate::recognition::Cause::Timer);
    reply(&mut engine, &timer, Phase::Discrete, 62);
    let outcomes = engine.take_route_outcomes();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(reports(&outcomes[0].intent), reports(&saved));
}

#[test]
fn a_projected_relative_offer_does_not_authorize_its_undeclared_wheel_companion() {
    let mut engine = hybrid(&[InputKind::Key, InputKind::Relative], false);
    raw(
        &mut engine,
        1,
        10,
        &[(EV_KEY, 30, 1), (EV_REL, 0, 4), (EV_REL, 8, 1)],
    );
    let intent = engine.route_intents().pop().unwrap();
    let report = intent.tail_reports().next().unwrap();
    assert!(report.observes(InputKind::Relative));
    assert!(!report.observes(InputKind::Wheel));
    assert!(!report.complete());
    let ticket = request(&mut engine, 10);
    reply(&mut engine, &ticket, Phase::Discrete, 11);
    let outcomes = engine.take_route_outcomes();
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].intent.tail_reports().next().unwrap().complete());
}

#[test]
fn native_partial_proofs_require_the_exact_original_source_and_observed_declaration() {
    let mut engine = hybrid(&[InputKind::Key, InputKind::Relative], false);
    raw(
        &mut engine,
        1,
        10,
        &[(EV_KEY, 30, 1), (EV_REL, 0, 4), (EV_REL, 8, 1)],
    );
    let intent = engine.route_intents().pop().unwrap();
    let Trigger::Custom { recognizer, .. } = &intent.rule.trigger else {
        unreachable!()
    };
    let proof = intent.candidate.input_proof();
    assert!(!intent.tail_reports().next().unwrap().complete());
    assert!(proof.valid_for(intent.source(), recognizer));
    let mut foreign = intent.source().clone();
    foreign.generation += 1;
    assert!(!proof.valid_for(&foreign, recognizer));
    let mut undeclared = recognizer.clone();
    undeclared.inputs.remove(&InputKind::Relative);
    assert!(!proof.valid_for(intent.source(), &undeclared));
    undeclared = recognizer.clone();
    undeclared.inputs.remove(&InputKind::Key);
    assert!(!proof.valid_for(intent.source(), &undeclared));
}

#[test]
fn fully_declared_wheel_companions_and_relative_axes_share_one_original_report() {
    let mut engine = hybrid(&[InputKind::Relative, InputKind::Wheel], false);
    raw(
        &mut engine,
        1,
        10,
        &[
            (EV_REL, 0, 4),
            (EV_REL, 1, 8),
            (EV_REL, 8, 1),
            (EV_REL, 11, 120),
        ],
    );
    let intent = engine.route_intents().pop().unwrap();
    assert_eq!(reports(&intent).len(), 1);
    let report = intent.tail_reports().next().unwrap();
    assert!(report.complete());
    assert!(report.observes(InputKind::Relative));
    assert!(report.observes(InputKind::Wheel));
    assert!(!report.observes(InputKind::Key));
}

#[test]
fn key_only_offers_do_not_inherit_undeclared_non_key_data_or_scan_companions() {
    let mut engine = hybrid(&[InputKind::Key], false);
    raw(
        &mut engine,
        1,
        10,
        &[(4, 4, 123), (EV_KEY, 30, 1), (EV_REL, 0, 4)],
    );
    assert!(reports(&engine.route_intents().pop().unwrap()).is_empty());
    let ticket = request(&mut engine, 10);
    reply(&mut engine, &ticket, Phase::Discrete, 11);
    let outcomes = engine.take_route_outcomes();
    assert_eq!(outcomes.len(), 1);
    assert!(reports(&outcomes[0].intent).is_empty());
    assert_eq!(positions(&outcomes[0].intent).len(), 1);
}

#[test]
fn tail_snapshots_share_the_key_budget_and_release_it_on_native_retirement() {
    let mut engine = hybrid(&[InputKind::Key, InputKind::Relative], false);
    let budget = engine.press_budget.clone();
    raw(&mut engine, 1, 10, &[(EV_KEY, 30, 1), (EV_REL, 0, 4)]);
    let saved = engine.route_intents().pop().unwrap();
    assert_eq!(budget.used.load(Ordering::Acquire), 2 * (NODE_BYTES + 5));
    let ticket = request(&mut engine, 10);
    reply(&mut engine, &ticket, Phase::Discrete, 11);
    let outcomes = engine.take_route_outcomes();
    drop(engine);
    assert_eq!(budget.used.load(Ordering::Acquire), 2 * (NODE_BYTES + 5));
    drop(outcomes);
    assert_eq!(budget.used.load(Ordering::Acquire), 2 * (NODE_BYTES + 5));
    drop(saved);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn absolute_mt_samples_without_contact_evidence_cannot_authorize_a_touch_cohort() {
    let mut engine = hybrid(&[InputKind::Absolute], false);
    raw(
        &mut engine,
        1,
        10,
        &[
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 7),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
        ],
    );
    let intent = engine.route_intents().pop().unwrap();
    let report = intent.tail_reports().next().unwrap();
    assert!(report.observes(InputKind::Absolute));
    assert!(!report.observes(InputKind::Touch));
    assert!(!report.complete());
}

#[test]
fn non_key_budget_exhaustion_cancels_without_issuing_a_partial_ticket() {
    let mut engine = hybrid_budget(
        &[InputKind::Key, InputKind::Relative],
        false,
        2 * (NODE_BYTES + 5) + crate::rules::custom::PROOF_BYTES,
    );
    let budget = engine.press_budget.clone();
    raw(&mut engine, 1, 10, &[(EV_KEY, 30, 1), (EV_REL, 0, 4)]);
    let saved = engine.route_intents().pop().unwrap();
    let ticket = request(&mut engine, 10);
    raw(&mut engine, 2, 20, &[(EV_REL, 0, 8)]);
    assert!(engine.route_intents().is_empty());
    assert!(engine.recognition_requests(ms(20)).unwrap().is_empty());
    assert!(
        engine
            .recognition_response(
                Proposal {
                    fence: ticket.fence,
                    ticket: ticket.id,
                    phase: Phase::Discrete,
                    data: serde_json::json!({})
                },
                ms(21)
            )
            .is_err()
    );
    assert!(engine.take_route_outcomes().is_empty());
    assert_eq!(reports(&saved).len(), 1);
    assert!(engine.take_events().iter().any(|event|
        matches!(event, Event::Cancelled { reason, .. } if reason.contains("provenance budget"))));
    drop(engine);
    drop(saved);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn conflicting_tail_union_does_not_partially_merge_independent_key_evidence() {
    let mut engine = hybrid(&[InputKind::Key, InputKind::Relative], false);
    raw(&mut engine, 1, 10, &[(EV_KEY, 30, 1), (EV_REL, 0, 4)]);
    let saved = engine.route_intents().pop().unwrap();
    raw(&mut engine, 2, 20, &[(EV_KEY, 31, 1), (EV_REL, 0, 8)]);
    let current = engine.route_intents().pop().unwrap();
    let mut proof = saved.candidate.input_proof();
    let mut wrong = saved.tail_reports().next().unwrap().clone();
    wrong.source.generation += 1;
    let mut tails = proof.tails.empty_like();
    tails.insert(wrong).unwrap();
    let other = InputProof {
        key_presses: current.candidate.key_presses.clone(),
        tails,
        position: current.observation_position(),
    };
    let before = engine.press_budget.used.load(Ordering::Acquire);
    assert!(proof.include(&other).is_err());
    assert_eq!(proof.key_presses.iter().count(), 1);
    assert_eq!(proof.tails.iter().count(), 1);
    assert_eq!(engine.press_budget.used.load(Ordering::Acquire), before);
}

#[test]
fn composed_custom_steps_merge_their_actual_report_proofs_not_future_offers() {
    let mut engine = hybrid(&[InputKind::Key, InputKind::Relative], false);
    let mut config = engine.config.clone();
    let step = config.rules[0].trigger.clone();
    config.rules[0].trigger = Trigger::Sequence {
        steps: vec![step.clone(), step],
        timeout: Millis(200),
    };
    engine.reload(config, ms(0)).unwrap();
    raw(&mut engine, 1, 10, &[(EV_KEY, 30, 1), (EV_REL, 0, 4)]);
    let first_position = engine.input_watermark();
    let first = request(&mut engine, 10);
    reply(&mut engine, &first, Phase::Discrete, 11);
    assert!(engine.take_route_outcomes().is_empty());
    raw(
        &mut engine,
        2,
        20,
        &[(EV_KEY, 30, 0), (EV_KEY, 31, 1), (EV_REL, 0, 8)],
    );
    let second_position = engine.input_watermark();
    let second = request(&mut engine, 20);
    reply(&mut engine, &second, Phase::Discrete, 21);
    let outcomes = engine.take_route_outcomes();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        reports(&outcomes[0].intent),
        BTreeSet::from([(1, first_position, 2), (1, second_position, 3)])
    );
    assert_eq!(positions(&outcomes[0].intent).len(), 2);
}

#[test]
fn native_wheel_steps_union_same_report_coverage_and_retain_distinct_report_positions() {
    for extra_axis in [false, true] {
        let mut engine = hybrid(&[InputKind::Wheel], false);
        let mut config = engine.config.clone();
        config.rules[0].trigger = Trigger::Sequence {
            steps: vec![
                Trigger::Wheel {
                    horizontal: false,
                    minimum: 1.0,
                },
                Trigger::Wheel {
                    horizontal: true,
                    minimum: 1.0,
                },
            ],
            timeout: Millis(200),
        };
        engine.reload(config, ms(0)).unwrap();
        let events = if extra_axis {
            vec![(EV_REL, 8, 1), (EV_REL, 6, 1)]
        } else {
            vec![(EV_REL, 8, 1)]
        };
        raw(&mut engine, 1, 10, &events);
        let first = engine.input_watermark();
        if extra_axis {
            let outcomes = engine.take_route_outcomes();
            assert_eq!(outcomes.len(), 1);
            assert_eq!(
                reports(&outcomes[0].intent),
                BTreeSet::from([(1, first, 2)])
            );
            assert!(outcomes[0].intent.tail_reports().next().unwrap().complete());
            continue;
        }
        assert!(engine.take_route_outcomes().is_empty());
        raw(&mut engine, 2, 40, &[(EV_REL, 6, 1)]);
        let second = engine.input_watermark();
        let outcomes = engine.take_route_outcomes();
        assert_eq!(
            outcomes.len(),
            1,
            "extra_axis={extra_axis}, events={:?}",
            engine.take_events()
        );
        assert_eq!(
            reports(&outcomes[0].intent),
            BTreeSet::from([(1, first, events.len()), (1, second, 1)])
        );
        let original = outcomes[0]
            .intent
            .tail_reports()
            .find(|report| report.position.frame == first)
            .unwrap();
        assert!(original.complete());
    }
}

#[test]
fn a_native_single_axis_wheel_does_not_own_the_other_axis_in_the_same_report() {
    let mut engine = hybrid(&[InputKind::Wheel], false);
    let mut config = engine.config.clone();
    config.rules[0].trigger = Trigger::Wheel {
        horizontal: false,
        minimum: 1.0,
    };
    engine.reload(config, ms(0)).unwrap();
    raw(&mut engine, 1, 10, &[(EV_REL, 8, 1), (EV_REL, 6, 1)]);
    let outcomes = engine.take_route_outcomes();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(reports(&outcomes[0].intent).len(), 1);
    assert!(!outcomes[0].intent.tail_reports().next().unwrap().complete());
}

fn disjoint_wheel_proofs() -> (crate::rules::tails::Report, crate::rules::tails::Report) {
    let mut engine = hybrid(&[InputKind::Wheel], false);
    let mut config = engine.config.clone();
    config.rules[0].trigger = Trigger::Wheel {
        horizontal: false,
        minimum: 1.0,
    };
    let mut horizontal = config.rules[0].clone();
    horizontal.id = "horizontal".into();
    horizontal.priority = 1;
    horizontal.trigger = Trigger::Wheel {
        horizontal: true,
        minimum: 1.0,
    };
    config.rules.push(horizontal);
    engine.reload(config, ms(0)).unwrap();
    raw(&mut engine, 1, 10, &[(EV_REL, 8, 1), (EV_REL, 6, 1)]);
    let outcomes = engine.take_route_outcomes();
    assert_eq!(outcomes.len(), 2);
    (
        outcomes[0].intent.tail_reports().next().unwrap().clone(),
        outcomes[1].intent.tail_reports().next().unwrap().clone(),
    )
}

#[test]
fn combining_same_report_coverage_preserves_snapshots_and_reuses_complete_supersets() {
    let (vertical, horizontal) = disjoint_wheel_proofs();
    let budget = budget(MAX_BYTES);
    let mut left = crate::rules::tails::Set::empty(budget.clone());
    let mut right = crate::rules::tails::Set::empty(budget.clone());
    left.insert(vertical).unwrap();
    right.insert(horizontal).unwrap();
    let saved = left.clone();
    left.merge(&right).unwrap();
    assert_eq!(left.iter().count(), 1);
    assert!(left.iter().next().unwrap().complete());
    assert!(!saved.iter().next().unwrap().complete());
    assert!(!right.iter().next().unwrap().complete());
    let bytes = budget.used.load(Ordering::Acquire);
    left.merge(&right).unwrap();
    left.merge(&saved).unwrap();
    right.merge(&left).unwrap();
    assert_eq!(budget.used.load(Ordering::Acquire), bytes);
    assert!(right.iter().next().unwrap().complete());
    drop((left, right, saved));
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn a_failed_coverage_override_refunds_temporary_union_storage_without_mutating_the_proof() {
    let (vertical, horizontal) = disjoint_wheel_proofs();
    let budget = budget(2 * (NODE_BYTES + 5) + NODE_BYTES);
    let mut left = crate::rules::tails::Set::empty(budget.clone());
    let mut right = crate::rules::tails::Set::empty(budget.clone());
    left.insert(vertical).unwrap();
    right.insert(horizontal).unwrap();
    let before = budget.used.load(Ordering::Acquire);
    assert!(left.merge(&right).is_err());
    assert_eq!(budget.used.load(Ordering::Acquire), before);
    assert_eq!(left.iter().count(), 1);
    assert!(!left.iter().next().unwrap().complete());
    drop((left, right));
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn held_contact_continuation_has_no_new_physical_report_to_claim() {
    let mut engine = hybrid(&[InputKind::Touch], false);
    raw(
        &mut engine,
        1,
        10,
        &[
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 7),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
            (EV_KEY, 0x14a, 1),
        ],
    );
    let saved = engine.route_intents().pop().unwrap();
    assert_eq!(reports(&saved).len(), 1);
    assert!(saved.tail_reports().next().unwrap().complete());
    let continued = crate::rules::continuation::candidate(
        &saved.candidate,
        &engine.devices[&1],
        &engine.authority,
        ms(11),
        engine.input_watermark(),
        saved.id() + 1,
    )
    .unwrap();
    assert_eq!(continued.continued_from, Some(saved.id()));
    assert_eq!(continued.tails.iter().count(), 0);
    assert_eq!(continued.key_presses.iter().count(), 0);
}

#[test]
fn deep_report_snapshots_share_nodes_and_retire_without_recursive_teardown() {
    let mut engine = hybrid(&[InputKind::Key, InputKind::Relative], false);
    raw(&mut engine, 1, 10, &[(EV_KEY, 30, 1), (EV_REL, 0, 4)]);
    let original = engine
        .route_intents()
        .pop()
        .unwrap()
        .tail_reports()
        .next()
        .unwrap()
        .clone();
    let budget = budget(MAX_BYTES);
    let mut tails = crate::rules::tails::Set::empty(budget.clone());
    for frame in 1..=MAX_PRESSES as u64 {
        let mut report = original.clone();
        report.position.frame = frame;
        tails.insert(report).unwrap();
    }
    assert_eq!(tails.iter().count(), MAX_PRESSES);
    let mut report = original;
    report.position.frame = MAX_PRESSES as u64 + 1;
    assert!(tails.insert(report).is_err());
    let saved = tails.clone();
    assert_eq!(
        budget.used.load(Ordering::Acquire),
        MAX_PRESSES * (NODE_BYTES + 5)
    );
    drop(tails);
    assert_eq!(saved.iter().count(), MAX_PRESSES);
    drop(saved);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

fn report_domains(intent: &RouteIntent) -> BTreeSet<crate::input::TailDomain> {
    intent
        .input_evidence()
        .tails
        .atoms()
        .map(|atom| atom.domain)
        .collect()
}

#[test]
fn absolute_observations_without_owned_contacts_authorize_no_report_domain() {
    for scalar in [false, true] {
        let mut engine = hybrid(&[InputKind::Absolute, InputKind::Relative], false);
        let mut events = vec![(EV_REL, 0, 4)];
        if scalar {
            events.push((EV_ABS, 0, 20));
        } else {
            events.extend([
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 7),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_ABS, ABS_MT_POSITION_Y, 200),
            ]);
        }
        raw(&mut engine, 1, 10, &events);
        let intent = engine.route_intents().pop().unwrap();
        let report = intent.tail_reports().next().unwrap();
        assert!(report.observes(InputKind::Absolute), "scalar={scalar}");
        assert!(report.observes(InputKind::Relative), "scalar={scalar}");
        assert!(report_domains(&intent).is_empty(), "scalar={scalar}");
    }
}

#[test]
fn owned_contacts_cover_the_coupled_scalar_emulation_of_the_same_report() {
    let mut engine = hybrid(&[InputKind::Touch], false);
    raw(
        &mut engine,
        1,
        10,
        &[
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 7),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
            (EV_KEY, 0x14a, 1),
            (EV_ABS, 0, 100),
            (EV_REL, 0, 4),
        ],
    );
    let intent = engine.route_intents().pop().unwrap();
    assert_eq!(
        report_domains(&intent),
        BTreeSet::from([
            crate::input::TailDomain::Absolute,
            crate::input::TailDomain::Touch
        ])
    );
}

#[test]
fn scalar_absolute_on_a_device_without_contacts_remains_independently_ownable() {
    let mut engine = hybrid(&[InputKind::Absolute, InputKind::Relative], false);
    let mut device = engine.devices[&1].device.clone();
    engine
        .push(Record::Remove {
            source: device.source.clone(),
            at: ms(0),
        })
        .unwrap();
    device.source.generation += 1;
    device.capabilities.insert(EV_KEY, vec![30]);
    device.axes.retain(|code, _| *code == 0);
    device.capabilities.insert(EV_ABS, vec![0]);
    assert!(!crate::input::coupled_absolute(&device));
    engine
        .push(Record::Device {
            device,
            keys: BTreeSet::new(),
            contacts: Vec::new(),
        })
        .unwrap();
    raw(&mut engine, 1, 10, &[(EV_ABS, 0, 20), (EV_REL, 0, 4)]);
    let intent = engine.route_intents().pop().unwrap();
    assert_eq!(
        report_domains(&intent),
        BTreeSet::from([
            crate::input::TailDomain::Relative,
            crate::input::TailDomain::Absolute
        ])
    );
}
