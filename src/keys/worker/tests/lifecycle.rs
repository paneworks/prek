use super::*;

#[test]
fn two_rules_on_one_physical_key_have_independent_owners() {
    let mut routing = routing(vec![
        rule("one", Action::Remap { key: Key(42) }),
        rule("two", Action::Remap { key: Key(42) }),
    ]);
    let (mut seat, writes) = model();
    let work = push(&mut routing, 1, 1, 10, 1);
    assert_eq!(work.len(), 2);
    let first_cancel = cancelled(&work[0]);
    submit(&mut seat, work, 10);
    ack(&mut routing, 1, 10);
    drain(&mut seat, &routing, 10);
    assert_eq!(seat.bindings.len(), 2);
    assert_eq!(key_events(&writes), [(42, 1)]);
    seat.cancel(fence(), &first_cancel, ms(11)).unwrap();
    drain(&mut seat, &routing, 11);
    assert!(seat.down(42));
    submit(&mut seat, push(&mut routing, 1, 2, 20, 0), 20);
    ack(&mut routing, 1, 20);
    drain(&mut seat, &routing, 20);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert_eq!(seat.bytes, 0);
}

#[test]
fn each_input_repeat_emits_one_repeat_per_distinct_target_before_the_release() {
    let mut routing = routing(vec![
        rule("one", Action::Remap { key: Key(42) }),
        rule("two", Action::Remap { key: Key(42) }),
        rule("three", Action::Remap { key: Key(29) }),
    ]);
    let (mut seat, writes) = model();
    let handles = submit(&mut seat, push(&mut routing, 1, 1, 10, 1), 10);
    ack(&mut routing, 1, 10);
    drain(&mut seat, &routing, 10);
    let pulse = frame(1, 2, 20, 30, 2);
    assert!(push(&mut routing, 1, 2, 20, 2).is_empty());
    seat.repeat(&handles, &pulse, 0, ms(20)).unwrap();
    assert!(seat.repeat(&handles, &pulse, 0, ms(20)).is_err());
    submit(&mut seat, push(&mut routing, 1, 3, 30, 0), 30);
    ack(&mut routing, 1, 30);
    drain(&mut seat, &routing, 30);
    let events = key_events(&writes);
    assert_eq!(&events[..4], [(42, 1), (29, 1), (42, 2), (29, 2)]);
    let mut releases = events[4..].to_vec();
    releases.sort();
    assert_eq!(releases, [(29, 0), (42, 0)]);
    assert!(
        seat.repeat(&handles, &frame(1, 4, 40, 30, 2), 0, ms(40))
            .is_err()
    );
    assert_eq!(seat.bytes, 0);
    assert!(seat.repeats.is_empty());
}

#[test]
fn cancelling_a_partially_written_chord_never_completes_its_unwritten_tail() {
    let mut routing = routing(vec![rule(
        "chord",
        Action::Keys {
            keys: vec![Key(42), Key(30)],
        },
    )]);
    let (mut seat, writes) = model();
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    let cancellation = cancelled(&work);
    seat.enqueue(work, ms(10)).unwrap();
    ack(&mut routing, 1, 10);
    seat.poll(&routing, ms(10)).unwrap();
    writes.borrow_mut().steps.push_back(Step::Prefix(1));
    seat.poll(&routing, ms(11)).unwrap();
    assert_eq!(key_events(&writes), [(42, 1)]);
    seat.cancel(fence(), &cancellation, ms(12)).unwrap();
    drain(&mut seat, &routing, 12);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert_eq!(seat.bytes, 0);
}

#[test]
fn source_retirement_preserves_other_sources_and_rejects_old_incarnations() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, writes) = model();
    submit(&mut seat, push(&mut routing, 1, 1, 10, 1), 10);
    ack(&mut routing, 1, 10);
    drain(&mut seat, &routing, 10);
    submit(&mut seat, push(&mut routing, 2, 1, 20, 1), 20);
    ack(&mut routing, 2, 20);
    drain(&mut seat, &routing, 20);
    seat.retire_source(&source(1), ms(21)).unwrap();
    drain(&mut seat, &routing, 21);
    assert!(seat.down(42));
    let stale = push(&mut routing, 1, 2, 22, 0).pop().unwrap();
    assert_eq!(
        seat.enqueue(stale, ms(22)).err().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
    submit(&mut seat, push(&mut routing, 2, 2, 30, 0), 30);
    ack(&mut routing, 2, 30);
    drain(&mut seat, &routing, 30);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert_eq!(seat.bytes, 0);
}

#[test]
fn unknown_cancellation_does_not_shorten_an_unrelated_write_deadline() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, _) = model();
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    let mut cancellation = cancelled(&work);
    if let Event::Cancelled { id, .. } = &mut cancellation {
        *id += 1;
    }
    seat.enqueue(work, ms(10)).unwrap();
    ack(&mut routing, 1, 10);
    seat.poll(&routing, ms(10)).unwrap();
    let original = seat.deadline();
    seat.cancel(fence(), &cancellation, ms(11)).unwrap();
    assert_eq!(seat.deadline(), original);
    assert!(seat.draining.is_none());
}

#[test]
fn io_failure_is_terminal_and_retains_the_exact_uncertain_frame() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, writes) = model();
    submit(&mut seat, push(&mut routing, 1, 1, 10, 1), 10);
    ack(&mut routing, 1, 10);
    seat.poll(&routing, ms(10)).unwrap();
    writes
        .borrow_mut()
        .steps
        .push_back(Step::Error(io::ErrorKind::BrokenPipe));
    assert_eq!(
        seat.poll(&routing, ms(11)).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert!(seat.failed());
    assert_eq!(seat.uncertain_frame(), writes.borrow().frames.first());
    assert!(seat.poll(&routing, ms(12)).is_err());
    assert!(seat.into_output().is_err());
}

#[test]
fn stalled_cleanup_expires_without_silent_reentry() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, writes) = model();
    submit(&mut seat, push(&mut routing, 1, 1, 10, 1), 10);
    ack(&mut routing, 1, 10);
    drain(&mut seat, &routing, 10);
    seat.retire(ms(20)).unwrap();
    writes.borrow_mut().steps.push_back(Step::Prefix(0));
    assert_eq!(seat.drain(ms(21)).unwrap(), Progress::Pending);
    assert_eq!(
        seat.drain(ms(270)).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert!(seat.failed());
    assert!(seat.down(42));
    assert!(seat.resume(&routing, ms(271)).is_err());
}

#[test]
fn actor_reuse_preserves_submission_and_frame_sequences() {
    let mut routing = routing(vec![rule(
        "chord",
        Action::Keys {
            keys: vec![Key(42), Key(30)],
        },
    )]);
    let (mut seat, writes) = model();
    submit(&mut seat, push(&mut routing, 1, 1, 10, 1), 10);
    ack(&mut routing, 1, 10);
    drain(&mut seat, &routing, 10);
    let serial = seat.serial;
    let sequence = writes.borrow().frames.last().unwrap().sequence;
    let mut seat = Seat::new(
        fence(),
        seat.into_output().unwrap_or_else(|_| panic!("idle output")),
    )
    .unwrap();
    assert_eq!(seat.serial, serial);
    assert_eq!(seat.at, ms(10));
    push(&mut routing, 1, 2, 20, 0);
    ack(&mut routing, 1, 20);
    submit(&mut seat, push(&mut routing, 1, 3, 30, 1), 30);
    ack(&mut routing, 1, 30);
    drain(&mut seat, &routing, 30);
    assert!(seat.serial > serial);
    assert!(
        writes
            .borrow()
            .frames
            .iter()
            .skip_while(|frame| frame.sequence <= sequence)
            .count()
            >= 2
    );
    assert_eq!(seat.bytes, 0);
}

#[test]
fn cleanup_advances_the_actor_clock_even_without_matching_work() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, _) = model();
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    let mut cancellation = cancelled(&work);
    if let Event::Cancelled { id, .. } = &mut cancellation {
        *id += 1;
    }
    seat.cancel(fence(), &cancellation, ms(20)).unwrap();
    assert_eq!(seat.at, ms(20));
    assert!(seat.enqueue(work, ms(10)).is_err());
}

#[test]
fn timed_out_work_can_only_drain_releases_with_a_new_cleanup_deadline() {
    let mut routing = routing(vec![rule("shift", Action::Remap { key: Key(42) })]);
    let (mut seat, writes) = model();
    submit(&mut seat, push(&mut routing, 1, 1, 10, 1), 10);
    ack(&mut routing, 1, 10);
    drain(&mut seat, &routing, 10);
    submit(&mut seat, push(&mut routing, 1, 2, 20, 0), 20);
    ack(&mut routing, 1, 20);
    seat.poll(&routing, ms(20)).unwrap();
    for (revision, at) in [(2, 200), (3, 400), (4, 600), (5, 800), (6, 900)] {
        context(&mut routing, at, revision, ContextState::Desktop);
    }
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), ms(900));
    assert_eq!(
        seat.poll(&routing, ms(1020)).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    seat.retire(ms(1021)).unwrap();
    assert_eq!(seat.deadline(), Some(ms(1271)));
    assert!(
        seat.enqueue(push(&mut routing, 1, 3, 1021, 1).pop().unwrap(), ms(1021))
            .is_err()
    );
    assert_eq!(seat.drain(ms(1022)).unwrap(), Progress::Complete);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert!(seat.failed());
}
