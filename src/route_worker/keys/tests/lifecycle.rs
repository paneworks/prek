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
fn consumer_loss_abandons_unwritten_chord_tails_and_retires_the_output() {
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
    slot.advance(ms(251)).unwrap();
    assert_eq!(
        slot.take_notice(),
        Some("generated keyboard lost its consumer lease; retiring it")
    );
    assert!(!slot.available(ms(251)));
    assert!(slot.retiring());
    state.borrow_mut().blocked = false;
    slot.advance(ms(252)).unwrap();
    assert!(!slot.closed());
    assert_eq!(keys(&state), [(42, 1), (42, 0)]);
    assert!(slot.offer(ms(253)).is_none());
    let request = slot.drain_request().unwrap();
    assert!(slot.acknowledge_drain(&request, ms(253), ms(253)));
    assert!(slot.closed());
    assert_eq!(state.borrow().dropped, 1);
}

#[test]
fn retirement_retains_the_writer_after_local_release_until_consumer_acknowledgement() {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    submit(&mut slot, &mut routing, 1, 10, 1);
    drain(&mut slot, &mut routing, 10);
    slot.retire(ms(20)).unwrap();
    drop(routing);
    slot.advance(ms(21)).unwrap();
    assert!(!slot.closed());
    assert_eq!(keys(&state), [(42, 1), (42, 0)]);
    assert_eq!(state.borrow().dropped, 0);
    assert_eq!(slot.deadline(), Some(ms(270)));
    let request = slot.drain_request().unwrap();
    assert!(slot.drain_request().is_none());
    assert!(slot.acknowledge_drain(&request, ms(22), ms(22)));
    assert!(slot.closed());
    assert_eq!(state.borrow().dropped, 1);
    assert!(!slot.acknowledge_drain(&request, ms(22), ms(22)));
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
fn mixed_repeat_entry_requires_the_original_coordinator_frame() {
    let (mut slot, state) = model();
    activate(&mut slot, &state, 0);
    let mut routing =
        crate::keys::worker::tests::projected(vec![rule("shift", Action::Remap { key: Key(42) })]);
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
        slot.admit(&routing, vec![], Some(&pulse), ms(20))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(keys(&state), [(42, 1)]);
}
