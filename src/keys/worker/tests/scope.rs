use super::*;

#[test]
fn observer_waits_for_its_raw_frame_and_keeps_later_frames_behind_its_write() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, writes) = model();
    submit(&mut seat, push(&mut routing, 1, 1, 10, 1), 10);
    assert!(!seat.blocks_raw(&source(1), 1));
    assert!(seat.blocks_raw(&source(1), 2));
    assert!(!seat.blocks_raw(&source(2), 2));
    assert_eq!(seat.poll(&routing, ms(10)).unwrap(), Progress::Pending);
    assert!(writes.borrow().frames.is_empty());
    let scheduled = routing.due(&source(1), ms(10)).unwrap().unwrap().clone();
    assert!(
        !writes
            .borrow()
            .frames
            .iter()
            .any(|frame| frame.sequence > 0)
    );
    assert_eq!(seat.poll(&routing, ms(10)).unwrap(), Progress::Pending);
    routing
        .delivered(&source(1), scheduled.ticket, ms(10))
        .unwrap();
    assert_eq!(seat.poll(&routing, ms(10)).unwrap(), Progress::Pending);
    assert!(seat.blocks_raw(&source(1), 2));
    drain(&mut seat, &routing, 11);
    assert!(!seat.blocks_raw(&source(1), 2));
}

#[test]
fn capability_revocation_cancels_old_work_even_after_same_grant_renewal() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, writes) = model();
    submit(&mut seat, push(&mut routing, 1, 1, 10, 1), 10);
    ack(&mut routing, 1, 10);
    seat.poll(&routing, ms(10)).unwrap();
    routing.restrict_actions(&BTreeSet::new());
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), ms(11));
    drain(&mut seat, &routing, 11);
    assert!(key_events(&writes).is_empty());
    assert!(seat.bindings.is_empty());
    assert_eq!(seat.bytes, 0);
}

#[test]
fn timely_capability_renewal_keeps_a_live_remap_held() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, writes) = model();
    submit(&mut seat, push(&mut routing, 1, 1, 10, 1), 10);
    ack(&mut routing, 1, 10);
    drain(&mut seat, &routing, 10);
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), ms(20));
    drain(&mut seat, &routing, 20);
    assert!(seat.down(42));
    assert_eq!(key_events(&writes), [(42, 1)]);
}

#[test]
fn context_loss_abandons_unwritten_chord_keys_and_only_compensates_the_prefix() {
    let mut routing = routing(vec![rule(
        "chord",
        Action::Keys {
            keys: vec![Key(42), Key(30)],
        },
    )]);
    let (mut seat, writes) = model();
    submit(&mut seat, push(&mut routing, 1, 1, 10, 1), 10);
    ack(&mut routing, 1, 10);
    seat.poll(&routing, ms(10)).unwrap();
    writes.borrow_mut().steps.push_back(Step::Prefix(1));
    seat.poll(&routing, ms(11)).unwrap();
    routing.authority_mut().lock_requested();
    drain(&mut seat, &routing, 12);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert_eq!(seat.bytes, 0);
}

#[test]
fn pause_revokes_old_tokens_and_requires_explicit_fresh_resume() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, _) = model();
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    seat.pause(&mut routing, ms(10)).unwrap();
    assert!(seat.enqueue(work, ms(10)).is_err());
    assert!(seat.resume(&routing, ms(11)).is_err());
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), ms(12));
    seat.resume(&routing, ms(12)).unwrap();
    assert!(!seat.paused());
}

#[test]
fn another_coordinator_cannot_submit_or_cancel_work() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (seat, _) = model();
    let mut other = fence();
    other.session += 1;
    let output = seat.into_output().unwrap_or_else(|_| panic!("idle output"));
    let mut seat = Seat::new(other, output).unwrap();
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    let cancellation = cancelled(&work);
    assert!(seat.enqueue(work, ms(10)).is_err());
    assert!(seat.cancel(fence(), &cancellation, ms(10)).is_err());
    assert!(seat.bindings.is_empty());
}

#[test]
fn pause_rejects_another_coordinator_before_revoking_either_lane() {
    let mut different = fence();
    different.session += 1;
    let mut routing = with_fence(
        vec![rule("shift", Action::Remap { key: Key(42) })],
        Some(different),
    );
    let (mut seat, _) = model();
    assert!(seat.pause(&mut routing, ms(10)).is_err());
    assert!(!seat.paused());
    assert!(routing.key_lane(different, ms(10)));
}
