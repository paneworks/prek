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
