use super::*;
use crate::{
    config::Millis,
    recognition::{Definition, InputKind, Phase, Proposal, Ticket},
};

pub(super) fn recognizer() -> Rule {
    let mut rule = rule(
        "custom",
        Action::Keys {
            keys: vec![Key(42)],
        },
    );
    rule.trigger = Trigger::Custom {
        recognizer: Definition {
            name: "custom-key".into(),
            selectors: BTreeSet::from(["keys".into()]),
            inputs: BTreeSet::from([InputKind::Key]),
            reply_within: Millis(100),
            continuous: false,
        },
        timeout: Millis(200),
        wake: Some(Millis(50)),
    };
    rule.delivery = Delivery::Buffer;
    rule.priority = 10;
    rule
}
pub(super) fn respond(
    routing: &mut Routing,
    ticket: &Ticket,
    phase: Phase,
    at: u64,
) -> Vec<KeyWork> {
    routing
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
    routing
        .take_scoped_dispatches(ms(at))
        .into_iter()
        .filter_map(|dispatch| dispatch.into_key_work().ok())
        .collect()
}

fn native_custom(delivery: Delivery) -> Rule {
    let mut rule = recognizer();
    rule.trigger = Trigger::Sequence {
        steps: vec![Trigger::Key { key: Key(30) }, rule.trigger],
        timeout: Millis(200),
    };
    rule.delivery = delivery;
    rule
}

fn cross_source_custom() -> Rule {
    let mut rule = native_custom(Delivery::Buffer);
    let Trigger::Sequence { steps, .. } = &mut rule.trigger else {
        unreachable!()
    };
    steps[0] = Trigger::ButtonHold {
        key: Key(30),
        after: Millis(20),
        repeat: None,
    };
    rule
}

#[test]
fn cross_source_native_custom_admission_keeps_every_parent_transport_dependency() {
    for retire in [None, Some(1), Some(2)] {
        let mut routing = arbitration::routed(vec![cross_source_custom()]);
        let (mut seat, writes) = model();
        submit(&mut routing, packet(1, 1, 10, &[(30, 1)]));
        submit(&mut routing, packet(2, 1, 35, &[(31, 1)]));
        let ticket = routing.recognition_requests(ms(35)).unwrap().pop().unwrap();
        assert_eq!(ticket.input.source().device, 2);
        let work = respond(&mut routing, &ticket, Phase::Discrete, 40);
        assert_eq!(work.len(), 1);
        seat.admit(&routing, work, None, ms(40)).unwrap();
        if let Some(id) = retire {
            routing.recover(&source(id));
        }
        poll(&mut seat, &routing, 40);
        assert_eq!(
            key_events(&writes),
            if retire.is_none() {
                vec![(42, 1), (42, 0)]
            } else {
                vec![]
            }
        );
    }
}

#[test]
fn a_native_custom_reply_is_revoked_by_either_prefix_or_leaf_transport_loss() {
    for retire in [1, 2] {
        let mut routing = arbitration::routed(vec![cross_source_custom()]);
        submit(&mut routing, packet(1, 1, 10, &[(30, 1)]));
        submit(&mut routing, packet(2, 1, 35, &[(31, 1)]));
        let ticket = routing.recognition_requests(ms(35)).unwrap().pop().unwrap();
        routing.recover(&source(retire));
        assert!(
            routing
                .recognition_response(
                    Proposal {
                        fence: ticket.fence,
                        ticket: ticket.id,
                        phase: Phase::Discrete,
                        data: serde_json::json!({}),
                    },
                    ms(40)
                )
                .is_err()
        );
        assert!(routing.take_scoped_dispatches(ms(40)).is_empty());
    }
}

#[test]
fn a_native_custom_reply_does_not_inherit_later_unseen_native_competition() {
    let mut higher = rule(
        "higher",
        Action::Keys {
            keys: vec![Key(29)],
        },
    );
    higher.trigger = Trigger::Key { key: Key(31) };
    higher.delivery = Delivery::Buffer;
    higher.priority = 20;
    let mut routing = arbitration::routed(vec![native_custom(Delivery::Buffer), higher]);
    let (mut seat, writes) = model();
    submit(&mut routing, packet(1, 1, 10, &[(30, 1)]));
    let ticket = routing.recognition_requests(ms(10)).unwrap().pop().unwrap();
    let native = submit(&mut routing, packet(1, 2, 20, &[(31, 1)]));
    assert_eq!(native.len(), 1);
    seat.admit(&routing, native, None, ms(20)).unwrap();
    poll(&mut seat, &routing, 20);
    assert!(key_events(&writes).is_empty());
    let work = respond(&mut routing, &ticket, Phase::Discrete, 25);
    assert_eq!(work.len(), 1);
    seat.admit(&routing, work, None, ms(25)).unwrap();
    poll(&mut seat, &routing, 25);
    assert_eq!(key_events(&writes), [(29, 1), (29, 0), (42, 1), (42, 0)]);
}

#[test]
fn a_buffered_native_custom_sequence_preserves_earlier_and_unseen_typing() {
    let mut routing = arbitration::routed(vec![native_custom(Delivery::Buffer)]);
    let (mut seat, writes) = model();
    assert!(submit(&mut routing, packet(1, 1, 10, &[(31, 1), (31, 0), (30, 1)])).is_empty());
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(31, 1), (31, 0)]);
    let ticket = routing.recognition_requests(ms(10)).unwrap().pop().unwrap();
    assert!(submit(&mut routing, packet(1, 2, 20, &[(31, 1), (31, 0)])).is_empty());
    let work = respond(&mut routing, &ticket, Phase::Discrete, 25);
    assert_eq!(work.len(), 1);
    seat.admit(&routing, work, None, ms(25)).unwrap();
    poll(&mut seat, &routing, 25);
    assert!(key_events(&writes).is_empty());
    assert_eq!(raw(&mut routing, &seat, 1, 35), [(31, 1), (31, 0)]);
    poll(&mut seat, &routing, 35);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
}

#[test]
fn a_reserved_native_custom_sequence_binds_its_leaf_to_the_exact_parent_press_claim() {
    let mut routing = arbitration::routed(vec![native_custom(Delivery::Reserve)]);
    let (mut seat, writes) = model();
    submit(&mut routing, packet(1, 1, 10, &[(31, 1), (31, 0), (30, 1)]));
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(31, 1), (31, 0)]);
    let ticket = routing.recognition_requests(ms(10)).unwrap().pop().unwrap();
    let work = respond(&mut routing, &ticket, Phase::Discrete, 15);
    assert_eq!(work.len(), 1);
    seat.admit(&routing, work, None, ms(15)).unwrap();
    poll(&mut seat, &routing, 15);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    submit(&mut routing, packet(1, 2, 20, &[(30, 0)]));
    assert!(raw(&mut routing, &seat, 1, 20).is_empty());
}

#[test]
fn a_custom_native_sequence_requires_fresh_suffix_input_after_the_actual_reply() {
    let mut rule = recognizer();
    rule.trigger = Trigger::Sequence {
        steps: vec![rule.trigger, Trigger::Key { key: Key(31) }],
        timeout: Millis(200),
    };
    let mut routing = arbitration::routed(vec![rule]);
    let (mut seat, writes) = model();
    submit(&mut routing, packet(1, 1, 10, &[(30, 1)]));
    let ticket = routing.recognition_requests(ms(10)).unwrap().pop().unwrap();
    submit(&mut routing, packet(1, 2, 20, &[(31, 1), (31, 0)]));
    assert!(respond(&mut routing, &ticket, Phase::Discrete, 25).is_empty());
    assert!(raw(&mut routing, &seat, 1, 35).is_empty());
    let work = submit(&mut routing, packet(1, 3, 40, &[(31, 1)]));
    assert_eq!(work.len(), 1);
    seat.admit(&routing, work, None, ms(40)).unwrap();
    poll(&mut seat, &routing, 40);
    assert!(key_events(&writes).is_empty());
    assert_eq!(raw(&mut routing, &seat, 1, 50), [(31, 1), (31, 0)]);
    poll(&mut seat, &routing, 50);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
}

#[test]
fn a_custom_reply_cannot_consume_a_later_frame_the_callback_has_not_seen() {
    for deliver_first_before_later in [false, true] {
        let mut routing = arbitration::routed(vec![recognizer()]);
        let (mut seat, writes) = model();
        assert!(submit(&mut routing, packet(1, 1, 10, &[(30, 1)])).is_empty());
        let first = deliver_first_before_later
            .then(|| routing.recognition_requests(ms(10)).unwrap().pop().unwrap());
        assert!(submit(&mut routing, packet(1, 2, 20, &[(31, 1), (31, 0)])).is_empty());
        let first =
            first.unwrap_or_else(|| routing.recognition_requests(ms(20)).unwrap().remove(0));
        let work = respond(&mut routing, &first, Phase::Discrete, 25);
        assert_eq!(work.len(), 1);
        seat.admit(&routing, work, None, ms(25)).unwrap();
        poll(&mut seat, &routing, 25);
        assert!(key_events(&writes).is_empty());
        assert!(raw(&mut routing, &seat, 1, 34).is_empty());
        assert_eq!(raw(&mut routing, &seat, 1, 35), [(31, 1), (31, 0)]);
        poll(&mut seat, &routing, 35);
        assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    }
}

#[test]
fn rejecting_a_higher_custom_candidate_releases_the_completed_native_alternative() {
    let mut lower = rule(
        "native",
        Action::Keys {
            keys: vec![Key(29)],
        },
    );
    lower.delivery = Delivery::Buffer;
    lower.priority = 0;
    let mut routing = arbitration::routed(vec![recognizer(), lower]);
    let (mut seat, writes) = model();
    assert!(submit(&mut routing, packet(1, 1, 10, &[(30, 1)])).is_empty());
    let first = routing.recognition_requests(ms(10)).unwrap().pop().unwrap();
    let work = respond(&mut routing, &first, Phase::Cancel, 20);
    seat.admit(&routing, work, None, ms(20)).unwrap();
    poll(&mut seat, &routing, 20);
    assert_eq!(key_events(&writes), [(29, 1), (29, 0)]);
    assert!(raw(&mut routing, &seat, 1, 20).is_empty());
}

#[test]
fn a_continuous_buffer_only_consumes_keys_confirmed_by_a_non_cancel_reply() {
    let mut continuous = recognizer();
    let Trigger::Custom { recognizer, .. } = &mut continuous.trigger else {
        unreachable!()
    };
    recognizer.continuous = true;
    let mut routing = arbitration::routed(vec![continuous]);
    let (mut seat, writes) = model();
    submit(&mut routing, packet(1, 1, 10, &[(30, 1)]));
    let first = routing.recognition_requests(ms(10)).unwrap().pop().unwrap();
    let begin = respond(&mut routing, &first, Phase::Begin, 15);
    seat.admit(&routing, begin, None, ms(15)).unwrap();
    poll(&mut seat, &routing, 15);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    submit(&mut routing, packet(1, 2, 20, &[(31, 1)]));
    let second = routing.recognition_requests(ms(20)).unwrap().pop().unwrap();
    let cancelled = respond(&mut routing, &second, Phase::Cancel, 25);
    seat.admit(&routing, cancelled, None, ms(25)).unwrap();
    assert_eq!(raw(&mut routing, &seat, 1, 25), [(31, 1)]);
    poll(&mut seat, &routing, 25);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
}

#[test]
fn a_higher_native_claim_on_unseen_input_does_not_invalidate_an_older_custom_ticket() {
    let mut higher = rule(
        "native",
        Action::Keys {
            keys: vec![Key(29)],
        },
    );
    higher.trigger = Trigger::Key { key: Key(31) };
    higher.delivery = Delivery::Buffer;
    higher.priority = 20;
    let mut routing = arbitration::routed(vec![recognizer(), higher]);
    let (mut seat, writes) = model();
    submit(&mut routing, packet(1, 1, 10, &[(30, 1)]));
    let first = routing.recognition_requests(ms(10)).unwrap().pop().unwrap();
    let native = submit(&mut routing, packet(1, 2, 20, &[(31, 1)]));
    assert_eq!(native.len(), 1);
    seat.admit(&routing, native, None, ms(20)).unwrap();
    poll(&mut seat, &routing, 20);
    assert!(key_events(&writes).is_empty());
    assert!(routing.recognition_requests(ms(21)).unwrap().is_empty());
    let custom = respond(&mut routing, &first, Phase::Discrete, 25);
    assert_eq!(custom.len(), 1);
    seat.admit(&routing, custom, None, ms(25)).unwrap();
    poll(&mut seat, &routing, 25);
    assert_eq!(key_events(&writes), [(29, 1), (29, 0), (42, 1), (42, 0)]);
}

#[test]
fn a_custom_timer_cannot_acquire_input_without_a_validated_frame_reply() {
    let mut routing = arbitration::routed(vec![recognizer()]);
    let (mut seat, writes) = model();
    submit(&mut routing, packet(1, 1, 10, &[(30, 1)]));
    let first = routing.recognition_requests(ms(10)).unwrap().pop().unwrap();
    assert!(respond(&mut routing, &first, Phase::Pending, 15).is_empty());
    submit(&mut routing, packet(1, 2, 20, &[(31, 1), (31, 0)]));
    let tickets = routing.recognition_requests(ms(61)).unwrap();
    let timer = tickets
        .iter()
        .find(|ticket| ticket.cause == crate::recognition::Cause::Timer)
        .unwrap();
    let work = respond(&mut routing, timer, Phase::Discrete, 61);
    assert_eq!(work.len(), 1);
    seat.admit(&routing, work, None, ms(61)).unwrap();
    poll(&mut seat, &routing, 61);
    assert!(key_events(&writes).is_empty());
    assert_eq!(raw(&mut routing, &seat, 1, 71), [(31, 1), (31, 0)]);
    poll(&mut seat, &routing, 71);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
}

#[test]
fn custom_admission_does_not_include_keys_before_its_original_activation_edge() {
    let mut conditional = recognizer();
    conditional.require_keys = vec![Key(42)];
    let mut routing = arbitration::routed(vec![conditional]);
    let (mut seat, writes) = model();
    submit(
        &mut routing,
        packet(1, 1, 10, &[(30, 1), (30, 0), (42, 1), (31, 1)]),
    );
    assert_eq!(raw(&mut routing, &seat, 1, 10), [(30, 1), (30, 0)]);
    let tickets = routing.recognition_requests(ms(10)).unwrap();
    assert_eq!(tickets.len(), 1);
    let ticket = &tickets[0];
    let crate::input_events::Event::Frame {
        changes, sequence, ..
    } = &ticket.input
    else {
        unreachable!()
    };
    assert_eq!(*sequence, 1);
    assert_eq!(
        changes,
        &[
            crate::input::Change::Key {
                code: 42,
                state: crate::input::KeyState::Press
            },
            crate::input::Change::Key {
                code: 31,
                state: crate::input::KeyState::Press
            },
        ]
    );
    let work = respond(&mut routing, ticket, Phase::Discrete, 15);
    seat.admit(&routing, work, None, ms(15)).unwrap();
    poll(&mut seat, &routing, 15);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
}

#[test]
fn begin_only_custom_admission_survives_a_later_modifier_release_in_the_report() {
    let mut conditional = recognizer();
    conditional.require_keys = vec![Key(42)];
    let mut routing = arbitration::routed(vec![conditional]);
    submit(&mut routing, packet(1, 1, 10, &[(42, 1), (30, 1), (42, 0)]));
    let tickets = routing.recognition_requests(ms(10)).unwrap();
    assert_eq!(tickets.len(), 1);
    assert_eq!(
        respond(&mut routing, &tickets[0], Phase::Discrete, 15).len(),
        1
    );
}

#[test]
fn a_transient_same_report_condition_loss_revokes_custom_tickets_on_every_source() {
    for modifier_source in [1, 2] {
        let mut conditional = recognizer();
        conditional.require_keys = vec![Key(42)];
        conditional.keys_throughout = true;
        let mut routing = arbitration::routed(vec![conditional]);
        submit(&mut routing, packet(modifier_source, 1, 5, &[(42, 1)]));
        submit(
            &mut routing,
            packet(1, if modifier_source == 1 { 2 } else { 1 }, 10, &[(30, 1)]),
        );
        let first = routing
            .recognition_requests(ms(10))
            .unwrap()
            .into_iter()
            .find(|ticket| ticket.input.source().device == 1)
            .unwrap();
        submit(
            &mut routing,
            packet(
                modifier_source,
                if modifier_source == 1 { 3 } else { 2 },
                15,
                &[(42, 0), (42, 1)],
            ),
        );
        assert!(
            routing
                .recognition_response(
                    Proposal {
                        fence: first.fence,
                        ticket: first.id,
                        phase: Phase::Discrete,
                        data: serde_json::json!({}),
                    },
                    ms(16)
                )
                .is_err(),
            "modifier source {modifier_source}"
        );
        assert!(routing.take_scoped_dispatches(ms(16)).is_empty());
    }
}
