use super::*;
use crate::{
    config::Millis,
    recognition::{Definition, InputKind, Phase, Proposal, Ticket},
};

fn custom(limit: usize) -> Engine {
    configured(
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
        },
        limit,
    )
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
                custom(MAX_BYTES).config.rules[0].trigger.clone(),
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
    assert_eq!(budget.used.load(Ordering::Acquire), nodes + 128);
    reply(&mut engine, &first, Phase::Pending, 11);
    assert_eq!(budget.used.load(Ordering::Acquire), nodes + 128);
    frame(&mut engine, 1, 2, 20, &[(31, 1)]);
    let nodes = nodes + NODE_BYTES + 5;
    let second = engine
        .recognition_requests(Duration::from_millis(20))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(budget.used.load(Ordering::Acquire), nodes + 256);
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
