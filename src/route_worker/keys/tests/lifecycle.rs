use super::*;

#[test]
fn a_fresh_sample_after_expiry_cannot_resurrect_the_old_output() {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let fresh = evidence(&slot, 251);
    let source = slot.source().clone();
    let marker = slot.descriptor.marker.clone();
    assert!(
        slot.observe(fence(), &source, &marker, &fresh, ms(251))
            .is_err()
    );
    assert!(!slot.available(ms(251)));
    assert!(slot.retiring());
}

#[test]
fn consumer_loss_abandons_unwritten_chord_tails_then_requires_worker_recovery() {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let mut routing = routing(vec![rule(
        "chord",
        Action::Keys {
            keys: vec![Key(42), Key(30)],
        },
    )]);
    submit(&mut slot, &mut routing, 1, 10, 1);
    slot.poll(&mut routing, ms(10)).unwrap();
    state.borrow_mut().blocked = true;
    state.borrow_mut().prefix = 1;
    slot.poll(&mut routing, ms(11)).unwrap();
    assert_eq!(keys(&state), [(42, 1)]);
    assert_eq!(
        slot.advance(ms(251)).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert!(!slot.available(ms(251)));
    assert!(slot.retiring());
    state.borrow_mut().blocked = false;
    slot.advance(ms(252)).unwrap();
    assert!(slot.closed());
    assert_eq!(keys(&state), [(42, 1), (42, 0)]);
    assert!(slot.offer(ms(253)).is_none());
    assert_eq!(state.borrow().dropped, 1);
}

#[test]
fn retirement_drains_a_held_release_without_requiring_the_old_routing() {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    submit(&mut slot, &mut routing, 1, 10, 1);
    drain(&mut slot, &mut routing, 10);
    slot.retire(ms(20)).unwrap();
    drop(routing);
    slot.advance(ms(21)).unwrap();
    assert!(slot.closed());
    assert_eq!(keys(&state), [(42, 1), (42, 0)]);
    assert_eq!(state.borrow().dropped, 1);
}

#[test]
fn drain_deadline_fails_instead_of_holding_the_worker_healthy_forever() {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    submit(&mut slot, &mut routing, 1, 10, 1);
    drain(&mut slot, &mut routing, 10);
    state.borrow_mut().blocked = true;
    slot.retire(ms(20)).unwrap();
    slot.advance(ms(21)).unwrap();
    assert_eq!(slot.deadline(), Some(ms(270)));
    assert_eq!(
        slot.advance(ms(270)).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert!(!slot.available(ms(270)));
    assert!(!slot.closed());
}

#[test]
fn consumer_revocation_masks_key_authority_before_any_pending_write() {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    submit(&mut slot, &mut routing, 1, 10, 1);
    slot.poll(&mut routing, ms(10)).unwrap();
    slot.retire(ms(11)).unwrap();
    slot.poll(&mut routing, ms(11)).unwrap();
    assert!(!routing.key_lane(fence(), ms(11)));
    assert!(keys(&state).is_empty());
}

#[test]
fn mixed_repeat_frames_are_refused_until_the_position_projector_exists() {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    submit(&mut slot, &mut routing, 1, 10, 1);
    drain(&mut slot, &mut routing, 10);
    let mut pulse = frame(1, 2, 20, 30, 2);
    pulse.events.insert(
        1,
        RawEvent {
            kind: EV_KEY,
            code: 31,
            value: 1,
        },
    );
    assert_eq!(
        slot.repeat(&pulse, ms(20)).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(keys(&state), [(42, 1)]);
}
