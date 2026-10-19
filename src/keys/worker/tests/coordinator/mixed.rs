use super::*;
use crate::{
    input::{EV_REL, EV_SW},
    recognition::{InputKind, Phase},
};

fn custom_rule(id: &str, inputs: &[InputKind]) -> Rule {
    let mut rule = custom::recognizer();
    rule.id = id.into();
    let Trigger::Custom { recognizer, .. } = &mut rule.trigger else {
        unreachable!()
    };
    recognizer.inputs = inputs.iter().copied().collect();
    recognizer.name = format!("{id}-input");
    rule
}
fn routed(rules: Vec<Rule>) -> Routing {
    let mut config = config(rules);
    config.policy.allow_input_events = true;
    config.timing.join_window = crate::config::Millis(1);
    config.timing.decision_window = crate::config::Millis(100);
    let mut routing = Routing::with_input_projection(config, fence()).unwrap();
    context(&mut routing, 0, 1, ContextState::Desktop);
    for id in [1, 2] {
        let mut device = device(id);
        device.capabilities.get_mut(&EV_KEY).unwrap().push(42);
        device.capabilities.insert(EV_REL, vec![0, 1, 6, 8, 11, 12]);
        device.capabilities.insert(EV_SW, vec![0]);
        routing
            .attach(device.clone(), &ready(&device), None, ms(0))
            .unwrap();
    }
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), ms(0));
    routing
}
fn mixed(id: u64, sequence: u64, at: u64, events: &[(u16, u16, i32)]) -> Frame {
    let mut frame = packet(id, sequence, at, &[]);
    frame.events = events
        .iter()
        .map(|&(kind, code, value)| RawEvent { kind, code, value })
        .collect();
    frame.events.push(RawEvent {
        kind: EV_SYN,
        code: SYN_REPORT,
        value: 0,
    });
    frame
}
fn raw_events(routing: &mut Routing, id: u64, at: u64) -> Vec<RawEvent> {
    let mut events = Vec::new();
    while let Some(output) = routing.due(&source(id), ms(at)).unwrap().cloned() {
        events.extend(output.frame.events);
        routing
            .delivered(&source(id), output.ticket, ms(at))
            .unwrap();
    }
    events
}

#[test]
fn configured_relative_callback_consumes_xy_but_preserves_keys_wheel_and_switch() {
    let mut routing = routed(vec![custom_rule("relative", &[InputKind::Relative])]);
    let frame = mixed(
        1,
        1,
        10,
        &[
            (EV_REL, 0, 47),
            (EV_KEY, 31, 1),
            (EV_REL, 1, 31),
            (EV_KEY, 31, 0),
            (EV_REL, 8, 1),
            (EV_SW, 0, 1),
        ],
    );
    assert!(submit(&mut routing, frame).is_empty());
    let prefix = raw_events(&mut routing, 1, 10);
    assert_eq!(
        prefix.iter().filter(|event| event.kind == EV_KEY).count(),
        2
    );
    let ticket = routing.recognition_requests(ms(10)).unwrap().pop().unwrap();
    let work = custom::respond(&mut routing, &ticket, Phase::Discrete, 20);
    assert_eq!(work.len(), 1);
    assert!(work[0].available(&routing, ms(20)));
    let suffix = raw_events(&mut routing, 1, 20);
    assert!(suffix.contains(&RawEvent {
        kind: EV_REL,
        code: 8,
        value: 1
    }));
    assert!(suffix.contains(&RawEvent {
        kind: EV_SW,
        code: 0,
        value: 1
    }));
    assert!(
        !suffix
            .iter()
            .any(|event| event.kind == EV_REL && event.code <= 1)
    );
    assert!(work[0].available(&routing, ms(20)));
}

#[test]
fn ticket_claim_binds_only_original_mixed_report_and_schedules_at_its_syn() {
    let mut routing = routed(vec![custom_rule(
        "mixed",
        &[InputKind::Key, InputKind::Relative],
    )]);
    assert!(
        submit(
            &mut routing,
            mixed(1, 1, 10, &[(EV_KEY, 30, 1), (EV_REL, 0, 9)])
        )
        .is_empty()
    );
    let ticket = routing.recognition_requests(ms(10)).unwrap().pop().unwrap();
    assert!(
        submit(
            &mut routing,
            mixed(1, 2, 15, &[(EV_KEY, 31, 1), (EV_REL, 8, 1)])
        )
        .is_empty()
    );
    let work = custom::respond(&mut routing, &ticket, Phase::Discrete, 20);
    assert_eq!(work.len(), 1);
    assert_eq!(
        work[0].scheduling_position(),
        Some(crate::input::InputPosition { frame: 1, event: 2 })
    );
    assert_eq!(work[0].position(), work[0].scheduling_position());
    let events = raw_events(&mut routing, 1, 25);
    assert!(events.contains(&RawEvent {
        kind: EV_KEY,
        code: 31,
        value: 1
    }));
    assert!(
        !events
            .iter()
            .any(|event| event.kind == EV_KEY && event.code == 30)
    );
    assert!(
        !events
            .iter()
            .any(|event| event.kind == EV_REL && event.code == 0)
    );
    assert!(work[0].available(&routing, ms(25)));
    routing.recover(&source(1));
    assert!(!work[0].available(&routing, ms(25)));
}

#[test]
fn native_wheel_and_custom_relative_can_own_distinct_domains_at_the_same_syn() {
    let mut wheel = rule(
        "wheel",
        Action::Keys {
            keys: vec![Key(29)],
        },
    );
    wheel.trigger = Trigger::Wheel {
        horizontal: false,
        minimum: 1.0,
    };
    wheel.delivery = Delivery::Buffer;
    let mut routing = routed(vec![custom_rule("relative", &[InputKind::Relative]), wheel]);
    let first = submit(
        &mut routing,
        mixed(
            1,
            1,
            10,
            &[(EV_REL, 0, 9), (EV_REL, 8, 1), (EV_REL, 11, 120)],
        ),
    );
    assert_eq!(first.len(), 1);
    let ticket = routing.recognition_requests(ms(10)).unwrap().pop().unwrap();
    let second = custom::respond(&mut routing, &ticket, Phase::Discrete, 20);
    assert_eq!(second.len(), 1);
    assert!(raw_events(&mut routing, 1, 20).is_empty());
    assert!(first[0].available(&routing, ms(20)));
    assert!(second[0].available(&routing, ms(20)));
}

#[test]
fn cancelled_mixed_callback_restores_reports_with_original_cadence() {
    let mut routing = routed(vec![custom_rule(
        "relative",
        &[InputKind::Key, InputKind::Relative],
    )]);
    let first = mixed(1, 1, 10, &[(EV_KEY, 30, 1), (EV_REL, 0, 9)]);
    let second = mixed(1, 2, 30, &[(EV_REL, 1, 7)]);
    assert!(submit(&mut routing, first.clone()).is_empty());
    let ticket = routing.recognition_requests(ms(10)).unwrap().pop().unwrap();
    assert!(submit(&mut routing, second.clone()).is_empty());
    assert!(custom::respond(&mut routing, &ticket, Phase::Cancel, 40).is_empty());
    assert_eq!(raw_events(&mut routing, 1, 40), first.events);
    assert!(raw_events(&mut routing, 1, 59).is_empty());
    assert_eq!(raw_events(&mut routing, 1, 60), second.events);
}

fn cross_source() -> Rule {
    let mut rule = custom_rule("cross", &[InputKind::Relative]);
    rule.trigger = Trigger::Sequence {
        steps: vec![
            Trigger::ButtonHold {
                key: Key(30),
                after: crate::config::Millis(20),
                repeat: None,
            },
            rule.trigger,
        ],
        timeout: crate::config::Millis(200),
    };
    rule
}

#[test]
fn mixed_sequence_dispatch_retains_both_source_authorizations_after_journal_pruning() {
    for retire in [None, Some(1), Some(2)] {
        let mut routing = routed(vec![cross_source()]);
        assert!(submit(&mut routing, packet(1, 1, 10, &[(30, 1)])).is_empty());
        assert!(submit(&mut routing, mixed(2, 1, 35, &[(EV_REL, 0, 9)])).is_empty());
        let ticket = routing.recognition_requests(ms(35)).unwrap().pop().unwrap();
        let work = custom::respond(&mut routing, &ticket, Phase::Discrete, 40);
        assert_eq!(work.len(), 1);
        assert!(raw_events(&mut routing, 1, 40).is_empty());
        assert!(raw_events(&mut routing, 2, 40).is_empty());
        assert!(work[0].affects(&source(1)) && work[0].affects(&source(2)));
        assert_eq!(
            work[0].scheduling_position(),
            Some(crate::input::InputPosition { frame: 2, event: 1 })
        );
        if let Some(id) = retire {
            routing.recover(&source(id));
        }
        assert_eq!(work[0].available(&routing, ms(40)), retire.is_none());
    }
}

#[test]
fn expired_prefix_cannot_partially_consume_a_later_domain_in_another_source() {
    let mut routing = routed(vec![cross_source()]);
    let first = packet(1, 1, 10, &[(30, 1)]);
    let second = mixed(2, 1, 100, &[(EV_REL, 0, 9)]);
    assert!(submit(&mut routing, first.clone()).is_empty());
    assert!(submit(&mut routing, second.clone()).is_empty());
    let ticket = routing
        .recognition_requests(ms(100))
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        routing
            .recognition_response_after_input(
                crate::recognition::Proposal {
                    fence: ticket.fence,
                    ticket: ticket.id,
                    phase: Phase::Discrete,
                    data: serde_json::json!({}),
                },
                ms(100),
                ms(111)
            )
            .is_err()
    );
    assert!(routing.take_scoped_dispatches(ms(111)).is_empty());
    assert_eq!(raw_events(&mut routing, 1, 111), first.events);
    assert_eq!(raw_events(&mut routing, 2, 201), second.events);
}

#[test]
fn winning_relative_competitor_does_not_consume_losing_mixed_candidate_keys() {
    let mut higher = custom_rule("higher", &[InputKind::Relative]);
    higher.priority = 20;
    let lower = custom_rule("lower", &[InputKind::Key, InputKind::Relative]);
    let mut routing = routed(vec![higher, lower]);
    assert!(
        submit(
            &mut routing,
            mixed(1, 1, 10, &[(EV_KEY, 30, 1), (EV_REL, 0, 9)])
        )
        .is_empty()
    );
    let tickets = routing.recognition_requests(ms(10)).unwrap();
    assert_eq!(tickets.len(), 2);
    let high = tickets
        .iter()
        .find(|ticket| ticket.rule == "higher")
        .unwrap();
    let work = custom::respond(&mut routing, high, Phase::Discrete, 20);
    assert_eq!(work.len(), 1);
    routing.push(Record::Tick { at: ms(110) }).unwrap();
    let events = raw_events(&mut routing, 1, 110);
    assert!(events.contains(&RawEvent {
        kind: EV_KEY,
        code: 30,
        value: 1
    }));
    assert!(!events.iter().any(|event| event.kind == EV_REL));
}

#[test]
fn unknown_switch_baseline_never_falls_back_to_coarse_key_or_switch_ownership() {
    let mut routing = routed(vec![custom_rule(
        "switch",
        &[InputKind::Key, InputKind::Switch],
    )]);
    let original = mixed(1, 1, 10, &[(EV_KEY, 30, 1), (EV_SW, 0, 1)]);
    assert!(submit(&mut routing, original.clone()).is_empty());
    assert!(routing.recognition_requests(ms(10)).unwrap().is_empty());
    assert_eq!(raw_events(&mut routing, 1, 10), original.events);
}

#[test]
fn an_older_mixed_ticket_does_not_consume_unseen_relative_reports_in_its_key_group() {
    let mut routing = routed(vec![custom_rule(
        "mixed",
        &[InputKind::Key, InputKind::Relative],
    )]);
    assert!(
        submit(
            &mut routing,
            mixed(1, 1, 10, &[(EV_KEY, 30, 1), (EV_REL, 0, 9)])
        )
        .is_empty()
    );
    let ticket = routing.recognition_requests(ms(10)).unwrap().pop().unwrap();
    let unseen = mixed(1, 2, 20, &[(EV_REL, 1, 7)]);
    assert!(submit(&mut routing, unseen.clone()).is_empty());
    let work = custom::respond(&mut routing, &ticket, Phase::Discrete, 25);
    assert_eq!(work.len(), 1);
    assert_eq!(raw_events(&mut routing, 1, 40), unseen.events);
}

#[test]
fn native_wheel_accumulation_claims_distinct_original_reports_without_losing_typing() {
    let mut wheel = rule(
        "wheel",
        Action::Keys {
            keys: vec![Key(29)],
        },
    );
    wheel.trigger = Trigger::Wheel {
        horizontal: false,
        minimum: 2.0,
    };
    wheel.delivery = Delivery::Buffer;
    let mut routing = routed(vec![wheel]);
    assert!(
        submit(
            &mut routing,
            mixed(1, 1, 10, &[(EV_REL, 8, 1), (EV_KEY, 31, 1)])
        )
        .is_empty()
    );
    let prefix = raw_events(&mut routing, 1, 10);
    assert!(prefix.contains(&RawEvent {
        kind: EV_KEY,
        code: 31,
        value: 1
    }));
    let work = submit(
        &mut routing,
        mixed(1, 2, 20, &[(EV_REL, 8, 1), (EV_KEY, 31, 0)]),
    );
    assert_eq!(work.len(), 1);
    assert_eq!(
        work[0].scheduling_position(),
        Some(crate::input::InputPosition { frame: 2, event: 2 })
    );
    let events = raw_events(&mut routing, 1, 30);
    assert!(events.contains(&RawEvent {
        kind: EV_KEY,
        code: 31,
        value: 0
    }));
    assert!(!events.iter().any(|event| event.kind == EV_REL));
    assert!(work[0].available(&routing, ms(30)));
}
