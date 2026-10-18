use super::*;
use crate::{
    config::Millis,
    recognition::{Definition, InputKind, Phase, Proposal, Ticket},
};

fn recognizer() -> Rule {
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
fn respond(routing: &mut Routing, ticket: &Ticket, phase: Phase, at: u64) -> Vec<KeyWork> {
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
