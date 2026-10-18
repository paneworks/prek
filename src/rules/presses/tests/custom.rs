use super::*;
use crate::{
    config::Millis,
    recognition::{Definition, InputKind, Phase, Proposal, Ticket},
};

fn custom_trigger() -> Trigger {
    Trigger::Custom {
        recognizer: Definition {
            name: "snapshot".into(),
            selectors: BTreeSet::from(["keys".into()]),
            inputs: BTreeSet::from([InputKind::Key]),
            reply_within: Millis(100),
            continuous: false,
        },
        timeout: Millis(200),
        wake: Some(Millis(50)),
    }
}
fn custom(limit: usize) -> Engine {
    configured(custom_trigger(), limit)
}
fn reply(engine: &mut Engine, ticket: &Ticket, phase: Phase, at: u64) {
    engine
        .recognition_response(
            Proposal {
                fence: ticket.fence,
                ticket: ticket.id,
                phase,
                data: serde_json::json!({}),
            },
            Duration::from_millis(at),
        )
        .unwrap();
}

#[test]
fn same_edge_custom_continuations_require_the_exact_previous_native_candidate() {
    let mut engine = configured(
        Trigger::Sequence {
            steps: vec![
                Trigger::Key {
                    key: crate::config::Key(30),
                },
                custom_trigger(),
            ],
            timeout: Millis(200),
        },
        MAX_BYTES,
    );
    frame(&mut engine, 1, 1, 10, &[(30, 1)]);
    let sequence = &engine.sequences[0];
    let pending = &sequence.pending["seat0"];
    let candidate = sequence.steps[1].workers[0]
        .candidates
        .values()
        .next()
        .unwrap();
    assert_eq!(candidate.key_start, pending.after_key);
    assert!(pending.accepts(candidate, &sequence.triggers));
    let mut altered = candidate.clone();
    altered.continued_from = None;
    assert!(!pending.accepts(&altered, &sequence.triggers));
    altered.continued_from = Some(pending.previous.as_ref().unwrap().id + 1);
    assert!(!pending.accepts(&altered, &sequence.triggers));
    altered.continued_from = candidate.continued_from;
    altered.source.device += 1;
    assert!(!pending.accepts(&altered, &sequence.triggers));
    altered = candidate.clone();
    altered.key_start = Some(InputPosition { frame: 0, event: 0 });
    assert!(!pending.accepts(&altered, &sequence.triggers));
    let mut triggers = sequence.triggers.clone();
    triggers[1] = Trigger::Key {
        key: crate::config::Key(30),
    };
    assert!(!pending.accepts(candidate, &triggers));
    let ticket = engine
        .recognition_requests(Duration::from_millis(10))
        .unwrap()
        .pop()
        .unwrap();
    reply(&mut engine, &ticket, Phase::Discrete, 11);
    let completed = engine.take_route_outcomes();
    assert_eq!(completed.len(), 1);
    assert_eq!(
        positions(&completed[0].intent),
        BTreeSet::from([(1, 1, 0, 30)])
    );
}

fn native_custom(required: Vec<crate::config::Key>) -> Engine {
    configured_required(
        Trigger::Sequence {
            steps: vec![
                Trigger::Key {
                    key: crate::config::Key(30),
                },
                custom_trigger(),
            ],
            timeout: Millis(200),
        },
        MAX_BYTES,
        required,
    )
}

#[test]
fn invalid_sequence_edges_neither_lose_nor_create_a_custom_coordinator() {
    for missing in [false, true] {
        let mut engine = native_custom(vec![]);
        engine.capture_input = true;
        frame(&mut engine, 1, 1, 10, &[(30, 1)]);
        let normalized = engine.captured_input.take().unwrap();
        let ticket = engine
            .recognition_requests(Duration::from_millis(10))
            .unwrap()
            .pop()
            .unwrap();
        let owned = missing.then(|| engine.custom.take());
        engine.input_watermark = 0;
        let error = engine.key_sequences_begin(&normalized).unwrap_err();
        if missing {
            assert!(error.to_string().contains("coordinator is unavailable"));
            assert!(engine.custom.is_none());
        } else {
            assert!(error.to_string().contains("unpositioned sequence input"));
            assert_eq!(
                engine.custom.as_ref().unwrap().gate.candidate(ticket.id),
                Some(ticket.candidate)
            );
        }
        assert!(
            engine.sequences[0]
                .steps
                .iter()
                .all(|step| step.custom.is_none())
        );
        drop(owned);
    }
}

#[test]
fn a_native_custom_handoff_excludes_earlier_keys_and_coalesces_only_its_original_suffix() {
    let mut engine = native_custom(vec![]);
    frame(
        &mut engine,
        1,
        1,
        10,
        &[(31, 1), (31, 0), (30, 1), (32, 1), (32, 0)],
    );
    let tickets = engine
        .recognition_requests(Duration::from_millis(10))
        .unwrap();
    assert_eq!(tickets.len(), 1);
    let crate::input_events::Event::Frame {
        sequence, changes, ..
    } = &tickets[0].input
    else {
        panic!("missing original suffix")
    };
    assert_eq!(*sequence, 1);
    assert_eq!(
        changes,
        &vec![
            Change::Key {
                code: 30,
                state: KeyState::Press
            },
            Change::Key {
                code: 32,
                state: KeyState::Press
            },
            Change::Key {
                code: 32,
                state: KeyState::Release
            },
        ]
    );
    reply(&mut engine, &tickets[0], Phase::Discrete, 11);
    let completed = engine.take_route_outcomes();
    assert_eq!(completed.len(), 1);
    assert_eq!(
        positions(&completed[0].intent),
        BTreeSet::from([(1, 1, 2, 30), (1, 1, 3, 32)])
    );
}

#[test]
fn a_native_custom_handoff_samples_activation_at_its_prefix_not_the_report_end() {
    let mut engine = native_custom(vec![crate::config::Key(42)]);
    frame(&mut engine, 1, 1, 10, &[(42, 1), (30, 1), (42, 0)]);
    let tickets = engine
        .recognition_requests(Duration::from_millis(10))
        .unwrap();
    assert_eq!(tickets.len(), 1);
    let crate::input_events::Event::Frame { changes, .. } = &tickets[0].input else {
        panic!("missing original suffix")
    };
    assert_eq!(
        changes,
        &vec![
            Change::Key {
                code: 30,
                state: KeyState::Press
            },
            Change::Key {
                code: 42,
                state: KeyState::Release
            },
        ]
    );
    reply(&mut engine, &tickets[0], Phase::Discrete, 11);
    assert_eq!(engine.take_route_outcomes().len(), 1);
}

#[test]
fn a_native_custom_suffix_does_not_close_at_an_intermediate_same_report_key_release() {
    let mut engine = native_custom(vec![]);
    frame(
        &mut engine,
        1,
        1,
        10,
        &[(31, 1), (31, 0), (30, 1), (30, 0), (32, 1)],
    );
    let tickets = engine
        .recognition_requests(Duration::from_millis(10))
        .unwrap();
    assert_eq!(tickets.len(), 1);
    reply(&mut engine, &tickets[0], Phase::Discrete, 11);
    let completed = engine.take_route_outcomes();
    assert_eq!(completed.len(), 1);
    assert_eq!(
        positions(&completed[0].intent),
        BTreeSet::from([(1, 1, 2, 30), (1, 1, 4, 32)])
    );
}

#[test]
fn ticket_confirmation_moves_its_charge_and_terminal_cleanup_releases_every_snapshot() {
    let mut engine = custom(MAX_BYTES);
    let budget = engine.press_budget.clone();
    frame(&mut engine, 1, 1, 10, &[(30, 1)]);
    let nodes = budget.used.load(Ordering::Acquire);
    let first = engine
        .recognition_requests(Duration::from_millis(10))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        budget.used.load(Ordering::Acquire),
        nodes + crate::rules::custom::PROOF_BYTES
    );
    reply(&mut engine, &first, Phase::Pending, 11);
    assert_eq!(
        budget.used.load(Ordering::Acquire),
        nodes + crate::rules::custom::PROOF_BYTES
    );
    frame(&mut engine, 1, 2, 20, &[(31, 1)]);
    let nodes = nodes + NODE_BYTES + 5;
    let second = engine
        .recognition_requests(Duration::from_millis(20))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        budget.used.load(Ordering::Acquire),
        nodes + 2 * crate::rules::custom::PROOF_BYTES
    );
    reply(&mut engine, &second, Phase::Discrete, 21);
    let outcomes = engine.take_route_outcomes();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(1, 1, 0, 30), (1, 2, 0, 31)])
    );
    assert_eq!(budget.used.load(Ordering::Acquire), nodes);
    drop(outcomes);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn an_exhausted_ticket_snapshot_budget_never_partially_issues_a_callback() {
    let mut engine = custom(NODE_BYTES + 5);
    let budget = engine.press_budget.clone();
    frame(&mut engine, 1, 1, 10, &[(30, 1)]);
    let nodes = budget.used.load(Ordering::Acquire);
    assert!(
        engine
            .recognition_requests(Duration::from_millis(10))
            .is_err()
    );
    assert_eq!(budget.used.load(Ordering::Acquire), nodes);
    assert!(engine.take_route_outcomes().is_empty());
    drop(engine);
    assert_eq!(budget.used.load(Ordering::Acquire), 0);
}

#[test]
fn same_report_custom_input_keeps_distinct_presses_at_their_original_offsets() {
    let mut engine = custom(MAX_BYTES);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0), (30, 1)]);
    let tickets = engine
        .recognition_requests(Duration::from_millis(10))
        .unwrap();
    assert_eq!(tickets.len(), 1);
    reply(&mut engine, &tickets[0], Phase::Discrete, 11);
    let outcomes = engine.take_route_outcomes();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(1, 1, 0, 30), (1, 1, 2, 30)])
    );
}

#[test]
fn a_new_original_report_retires_a_closed_custom_group_without_reusing_its_press() {
    let mut engine = custom(MAX_BYTES);
    frame(&mut engine, 1, 1, 10, &[(30, 1), (30, 0)]);
    let first = engine
        .recognition_requests(Duration::from_millis(10))
        .unwrap()
        .pop()
        .unwrap();
    frame(&mut engine, 1, 2, 20, &[(30, 1)]);
    let next = engine
        .recognition_requests(Duration::from_millis(20))
        .unwrap()
        .pop()
        .unwrap();
    assert_ne!(first.candidate, next.candidate);
    assert!(
        engine
            .recognition_response(
                Proposal {
                    fence: first.fence,
                    ticket: first.id,
                    phase: Phase::Discrete,
                    data: serde_json::json!({}),
                },
                Duration::from_millis(20)
            )
            .is_err()
    );
    reply(&mut engine, &next, Phase::Discrete, 21);
    let outcomes = engine.take_route_outcomes();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        positions(&outcomes[0].intent),
        BTreeSet::from([(1, 2, 0, 30)])
    );
}
