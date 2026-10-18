use super::*;
use crate::config::Millis;

fn buffered(id: &str, trigger: Trigger, priority: i32) -> Rule {
    let mut rule = rule(
        id,
        Action::Keys {
            keys: vec![Key(42)],
        },
    );
    rule.trigger = trigger;
    rule.delivery = Delivery::Buffer;
    rule.priority = priority;
    rule
}
fn tap(count: u32) -> Trigger {
    Trigger::ButtonTaps {
        key: Key(30),
        count,
    }
}
fn hold(after: u64) -> Trigger {
    Trigger::ButtonHold {
        key: Key(30),
        after: Millis(after),
        repeat: None,
    }
}
fn routed(rules: Vec<Rule>) -> Routing {
    let mut config = config(rules);
    config.timing.tap_gap = Millis(20);
    config.timing.join_window = Millis(1);
    config.timing.decision_window = Millis(100);
    let mut routing = Routing::with_key_projection(config, fence()).unwrap();
    context(&mut routing, 0, 1, ContextState::Desktop);
    for id in [1, 2] {
        let mut device = device(id);
        device.capabilities.get_mut(&EV_KEY).unwrap().push(42);
        device.capabilities.insert(4, vec![4]);
        routing
            .attach(device.clone(), &ready(&device), None, ms(0))
            .unwrap();
    }
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), ms(0));
    routing
}
fn tick(routing: &mut Routing, at: u64) -> Vec<KeyWork> {
    routing.push(Record::Tick { at: ms(at) }).unwrap();
    routing
        .take_scoped_dispatches(ms(at))
        .into_iter()
        .map(|dispatch| dispatch.into_key_work().ok().unwrap())
        .collect()
}

#[test]
fn a_buffered_tap_consumes_only_its_presses_not_unrelated_same_source_typing() {
    let mut routing = routed(vec![buffered("tap", tap(1), 0)]);
    let (mut seat, writes) = model();
    assert!(
        submit(
            &mut routing,
            packet(1, 1, 10, &[(30, 1), (30, 0), (31, 1), (31, 0)])
        )
        .is_empty()
    );
    assert!(raw(&mut routing, &seat, 1, 10).is_empty());
    let work = tick(&mut routing, 31);
    assert_eq!(work.len(), 1);
    seat.admit(&routing, work, None, ms(31)).unwrap();
    assert_eq!(raw(&mut routing, &seat, 1, 31), [(31, 1), (31, 0)]);
    poll(&mut seat, &routing, 31);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert!(raw(&mut routing, &seat, 1, 31).is_empty());
}

#[test]
fn unmatched_taps_fall_back_once_with_the_original_inter_report_interval() {
    let mut routing = routed(vec![buffered("double", tap(2), 0)]);
    let (seat, _) = model();
    assert!(submit(&mut routing, packet(1, 1, 10, &[(30, 1)])).is_empty());
    assert!(submit(&mut routing, packet(1, 2, 20, &[(30, 0)])).is_empty());
    assert!(tick(&mut routing, 41).is_empty());
    assert_eq!(raw(&mut routing, &seat, 1, 41), [(30, 1)]);
    assert!(raw(&mut routing, &seat, 1, 50).is_empty());
    assert_eq!(raw(&mut routing, &seat, 1, 51), [(30, 0)]);
    assert!(raw(&mut routing, &seat, 1, 52).is_empty());
}

#[test]
fn an_earlier_deferred_sequence_precedes_a_newer_unexposed_action() {
    let sequence = buffered(
        "sequence",
        Trigger::Sequence {
            steps: vec![tap(1), Trigger::Key { key: Key(31) }],
            timeout: Millis(100),
        },
        0,
    );
    let mut later = buffered("later", Trigger::Key { key: Key(42) }, -10);
    later.action = Action::Keys {
        keys: vec![Key(29)],
    };
    let mut routing = routed(vec![sequence, later]);
    let (mut seat, writes) = model();
    assert!(submit(&mut routing, packet(1, 1, 10, &[(30, 1), (30, 0)])).is_empty());
    assert!(submit(&mut routing, packet(1, 2, 15, &[(31, 1), (31, 0)])).is_empty());
    let work = submit(&mut routing, packet(1, 3, 20, &[(42, 1)]));
    assert_eq!(work.len(), 1);
    seat.admit(&routing, work, None, ms(20)).unwrap();
    poll(&mut seat, &routing, 20);
    assert!(key_events(&writes).is_empty());
    let work = tick(&mut routing, 31);
    assert_eq!(work.len(), 1);
    seat.admit(&routing, work, None, ms(31)).unwrap();
    assert_eq!(seat.deadline(), Some(ms(1020)));
    poll(&mut seat, &routing, 31);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0), (29, 1), (29, 0)]);
}

#[test]
fn older_work_cannot_cross_an_operation_already_exposed_to_the_sink() {
    for prefix in [None, Some(0), Some(1)] {
        let mut later = buffered("later", Trigger::Key { key: Key(31) }, -10);
        later.action = Action::Keys {
            keys: vec![Key(29)],
        };
        let mut routing = routed(vec![
            buffered("earlier", Trigger::Key { key: Key(30) }, 0),
            later,
        ]);
        let (mut seat, writes) = model();
        let earlier = submit(&mut routing, packet(1, 1, 10, &[(30, 1)]))
            .pop()
            .unwrap();
        let later = submit(&mut routing, packet(1, 2, 20, &[(31, 1)]));
        seat.admit(&routing, later, None, ms(20)).unwrap();
        seat.poll(&routing, ms(20)).unwrap();
        if let Some(count) = prefix {
            writes.borrow_mut().steps.push_back(Step::Prefix(count));
            seat.poll(&routing, ms(21)).unwrap();
            assert!(seat.flight.is_some());
        } else {
            poll(&mut seat, &routing, 21);
            assert!(seat.flight.is_none());
        }
        let bytes = seat.bytes;
        assert_eq!(
            seat.enqueue(earlier, ms(21)).err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(seat.bytes, bytes);
        assert!(seat.pending.is_empty());
        poll(&mut seat, &routing, 21);
        assert_eq!(key_events(&writes), [(29, 1), (29, 0)]);
    }
}

#[test]
fn pending_keyboard_input_fences_later_sources_without_bursting_their_clock() {
    let mut routing = routed(vec![buffered("hold", hold(50), 0)]);
    let (mut seat, writes) = model();
    assert!(submit(&mut routing, packet(1, 1, 10, &[(30, 1)])).is_empty());
    assert!(submit(&mut routing, packet(2, 1, 20, &[(31, 1), (31, 0)])).is_empty());
    assert!(raw(&mut routing, &seat, 2, 20).is_empty());
    let work = tick(&mut routing, 60);
    assert_eq!(work.len(), 1);
    seat.admit(&routing, work, None, ms(60)).unwrap();
    poll(&mut seat, &routing, 60);
    assert!(key_events(&writes).is_empty());
    assert!(raw(&mut routing, &seat, 2, 69).is_empty());
    assert_eq!(raw(&mut routing, &seat, 2, 70), [(31, 1), (31, 0)]);
    poll(&mut seat, &routing, 70);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert!(submit(&mut routing, packet(1, 2, 71, &[(30, 0)])).is_empty());
    assert!(raw(&mut routing, &seat, 1, 71).is_empty());
}

#[test]
fn a_completed_lower_priority_key_waits_and_wins_if_the_higher_hold_is_rejected() {
    let mut routing = routed(vec![
        buffered("hold", hold(50), 10),
        buffered("key", Trigger::Key { key: Key(30) }, 0),
    ]);
    let (mut seat, writes) = model();
    assert!(submit(&mut routing, packet(1, 1, 10, &[(30, 1)])).is_empty());
    let work = submit(&mut routing, packet(1, 2, 20, &[(30, 0)]));
    assert!(!work.is_empty());
    assert!(
        work.iter()
            .all(|work| matches!(work.event(), Event::Matched { rule, .. } if rule == "key"))
    );
    seat.admit(&routing, work, None, ms(20)).unwrap();
    poll(&mut seat, &routing, 20);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert!(raw(&mut routing, &seat, 1, 20).is_empty());
}

#[test]
fn a_cross_source_key_sequence_claims_atomically_without_eating_a_later_tail() {
    let mut sequence = observed_sequence();
    sequence.delivery = Delivery::Buffer;
    let mut routing = routed(vec![sequence]);
    let (mut seat, writes) = model();
    assert!(submit(&mut routing, packet(1, 1, 10, &[(30, 1), (30, 0)])).is_empty());
    let work = submit(
        &mut routing,
        packet(2, 1, 20, &[(31, 1), (31, 0), (42, 1), (42, 0)]),
    );
    assert_eq!(work.len(), 1);
    assert!(
        work[0].available(&routing, ms(20)),
        "claimed sequence work lost its live proof"
    );
    assert!(
        work[0].input_prefix_ready(&routing),
        "claimed sequence prefix still blocks"
    );
    seat.admit(&routing, work, None, ms(20)).unwrap();
    poll(&mut seat, &routing, 20);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert!(raw(&mut routing, &seat, 1, 20).is_empty());
    assert!(raw(&mut routing, &seat, 2, 29).is_empty());
    assert_eq!(raw(&mut routing, &seat, 2, 30), [(42, 1), (42, 0)]);
}

#[test]
fn losing_the_pending_source_wakes_the_other_keyboards_retained_raw_input() {
    let mut routing = routed(vec![buffered("hold", hold(50), 0)]);
    let (seat, _) = model();
    submit(&mut routing, packet(1, 1, 10, &[(30, 1)]));
    submit(&mut routing, packet(2, 1, 20, &[(31, 1), (31, 0)]));
    assert!(raw(&mut routing, &seat, 2, 20).is_empty());
    routing.recover(&source(1));
    assert!(
        routing.deadline().is_some(),
        "retained ordinary input lost its wakeup"
    );
    assert_eq!(raw(&mut routing, &seat, 2, 30), [(31, 1), (31, 0)]);
}

#[test]
fn a_claim_keeps_its_admission_owners_transport_dependency_after_member_selection() {
    let mut routing = routed(vec![buffered(
        "chord",
        Trigger::Chord {
            keys: vec![Key(30), Key(31)],
            ordered: false,
            timeout: Millis(100),
        },
        0,
    )]);
    let third = device(3);
    routing
        .attach(third.clone(), &ready(&third), None, ms(0))
        .unwrap();
    assert!(submit(&mut routing, packet(1, 1, 10, &[(30, 1)])).is_empty());
    assert!(submit(&mut routing, packet(2, 1, 11, &[(30, 1)])).is_empty());
    let mut work = submit(&mut routing, packet(3, 1, 12, &[(31, 1)]));
    assert_eq!(work.len(), 1);
    let work = work.pop().unwrap();
    assert_eq!(work.source(), &source(1));
    assert!(work.available(&routing, ms(12)));
    routing.recover(&source(1));
    assert!(
        !work.available(&routing, ms(12)),
        "selected members outlived their admission owner"
    );
}

#[test]
fn pending_scan_metadata_does_not_turn_the_forwarding_deadline_into_a_busy_poll() {
    let mut routing = routed(vec![buffered("hold", hold(50), 0)]);
    let mut input = packet(1, 1, 10, &[(30, 1)]);
    input.events.insert(
        0,
        RawEvent {
            kind: 4,
            code: 4,
            value: 30,
        },
    );
    assert!(submit(&mut routing, input).is_empty());
    assert!(routing.due(&source(1), ms(10)).unwrap().is_none());
    assert_eq!(routing.deadline(), Some(ms(60)));
    let (mut seat, writes) = model();
    let work = tick(&mut routing, 60);
    assert_eq!(work.len(), 1);
    seat.admit(&routing, work, None, ms(60)).unwrap();
    poll(&mut seat, &routing, 60);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert!(routing.due(&source(1), ms(60)).unwrap().is_none());
}

#[test]
fn a_reserved_sequence_keeps_its_future_member_over_a_later_higher_priority_key() {
    let mut sequence = observed_sequence();
    sequence.delivery = Delivery::Reserve;
    let mut key = buffered("later-high", Trigger::Key { key: Key(31) }, 100);
    key.action = Action::Keys {
        keys: vec![Key(29)],
    };
    let mut routing = routed(vec![sequence, key]);
    let (mut seat, writes) = model();
    assert!(submit(&mut routing, packet(1, 1, 10, &[(30, 1), (30, 0)])).is_empty());
    assert!(raw(&mut routing, &seat, 1, 10).is_empty());
    let work = submit(&mut routing, packet(1, 2, 20, &[(31, 1), (31, 0)]));
    assert_eq!(work.len(), 1);
    assert!(matches!(work[0].event(), Event::Matched { rule, .. } if rule == "sequence"));
    seat.admit(&routing, work, None, ms(20)).unwrap();
    poll(&mut seat, &routing, 20);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert!(raw(&mut routing, &seat, 1, 20).is_empty());
}

#[test]
fn a_completed_lower_priority_key_wins_at_the_original_deadline_before_fallback() {
    let mut routing = routed(vec![
        buffered("triple", tap(3), 10),
        buffered("key", Trigger::Key { key: Key(30) }, 0),
    ]);
    let (mut seat, writes) = model();
    assert!(submit(&mut routing, packet(1, 1, 10, &[(30, 1)])).is_empty());
    assert!(tick(&mut routing, 109).is_empty());
    let work = tick(&mut routing, 110);
    assert_eq!(work.len(), 1);
    assert!(matches!(work[0].event(), Event::Matched { rule, .. } if rule == "key"));
    seat.admit(&routing, work, None, ms(110)).unwrap();
    poll(&mut seat, &routing, 110);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert!(raw(&mut routing, &seat, 1, 110).is_empty());
}

#[test]
fn withdrawing_action_availability_releases_pending_keys_without_a_late_gesture() {
    let mut routing = routed(vec![buffered("tap", tap(1), 0)]);
    let (seat, _) = model();
    assert!(submit(&mut routing, packet(1, 1, 10, &[(30, 1)])).is_empty());
    assert!(submit(&mut routing, packet(1, 2, 15, &[(30, 0)])).is_empty());
    routing.actions_available(1, BTreeSet::new(), ms(20));
    assert!(tick(&mut routing, 20).is_empty());
    assert_eq!(raw(&mut routing, &seat, 1, 20), [(30, 1)]);
    assert!(raw(&mut routing, &seat, 1, 24).is_empty());
    assert_eq!(raw(&mut routing, &seat, 1, 25), [(30, 0)]);
    assert!(tick(&mut routing, 35).is_empty());
}
