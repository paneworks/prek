use super::*;

fn repeat_at(frame: Option<u64>) -> Operation {
    Operation::Repeat {
        members: vec![],
        inputs: BTreeMap::new(),
        emitted: BTreeSet::new(),
        position: frame.map(InputPosition::after_frame),
        inclusive: false,
    }
}

#[test]
fn unexposed_work_sorts_stably_without_crossing_a_legacy_fence() {
    let (mut seat, _) = model();
    for (frame, at) in [
        (Some(30), 10),
        (None, 11),
        (Some(20), 12),
        (Some(10), 13),
        (Some(20), 14),
    ] {
        seat.queue(repeat_at(frame), 1, ms(at));
    }
    assert_eq!(
        seat.pending
            .iter()
            .map(|pending| (
                pending.position.map(|position| position.frame),
                pending.received
            ))
            .collect::<Vec<_>>(),
        [
            (Some(30), ms(10)),
            (None, ms(11)),
            (Some(10), ms(13)),
            (Some(20), ms(12)),
            (Some(20), ms(14))
        ]
    );
    assert_eq!(seat.bytes, 5);
}

#[test]
fn reordered_pending_work_cannot_hide_an_older_arrival_deadline() {
    let mut routing = routing(vec![]);
    let (mut seat, writes) = model();
    seat.queue(repeat_at(Some(2)), 1, ms(20));
    seat.queue(repeat_at(Some(1)), 1, ms(31));
    assert_eq!(seat.pending.front().unwrap().received, ms(31));
    assert_eq!(seat.deadline(), Some(ms(1020)));
    for (revision, at) in [(2, 200), (3, 400), (4, 600), (5, 800), (6, 900)] {
        context(&mut routing, at, revision, ContextState::Desktop);
    }
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), ms(900));
    assert_eq!(
        seat.poll(&routing, ms(1020)).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert!(seat.failed());
    assert!(writes.borrow().frames.is_empty());
    assert_eq!(seat.bytes, 2);
}

#[test]
fn queue_rejection_does_not_partially_accept_the_next_action() {
    let mut routing = routing(vec![rule(
        "tap",
        Action::Keys {
            keys: vec![Key(30)],
        },
    )]);
    let (mut seat, writes) = model();
    for index in 0..MAX_PENDING as u64 {
        submit(&mut seat, push(&mut routing, 1, index * 2 + 1, 10, 1), 10);
        ack(&mut routing, 1, 10);
        submit(&mut seat, push(&mut routing, 1, index * 2 + 2, 10, 0), 10);
        ack(&mut routing, 1, 10);
    }
    let bytes = seat.bytes;
    let work = push(&mut routing, 1, MAX_PENDING as u64 * 2 + 1, 10, 1)
        .pop()
        .unwrap();
    assert_eq!(
        seat.enqueue(work, ms(10)).err().unwrap().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(seat.pending.len(), MAX_PENDING);
    assert_eq!(seat.bytes, bytes);
    seat.retire(ms(11)).unwrap();
    assert_eq!(seat.drain(ms(11)).unwrap(), Progress::Complete);
    assert_eq!(seat.bytes, 0);
    assert!(writes.borrow().frames.is_empty());
}

#[test]
fn completed_actions_reclaim_scope_bytes_and_bindings() {
    let mut routing = routing(vec![rule(
        "tap",
        Action::Keys {
            keys: vec![Key(30)],
        },
    )]);
    let (mut seat, writes) = model();
    for index in 0..1000 {
        submit(&mut seat, push(&mut routing, 1, index * 2 + 1, 10, 1), 10);
        ack(&mut routing, 1, 10);
        drain(&mut seat, &routing, 10);
        submit(&mut seat, push(&mut routing, 1, index * 2 + 2, 10, 0), 10);
        ack(&mut routing, 1, 10);
        assert_eq!(seat.bytes, 0);
        assert!(seat.pending.is_empty());
        assert!(seat.bindings.is_empty());
        assert!(seat.flight.is_none());
    }
    assert_eq!(seat.serial, 1000);
    assert_eq!(key_events(&writes).len(), 2000);
}

#[test]
fn queued_repeat_continuations_keep_their_original_age_and_byte_charge() {
    let mut routing = routing(vec![
        rule("one", Action::Remap { key: Key(42) }),
        rule("two", Action::Remap { key: Key(29) }),
    ]);
    let (mut seat, _) = model();
    let handles = submit(&mut seat, push(&mut routing, 1, 1, 10, 1), 10);
    ack(&mut routing, 1, 10);
    drain(&mut seat, &routing, 10);
    push(&mut routing, 1, 2, 20, 2);
    ack(&mut routing, 1, 20);
    seat.repeat(&handles, &frame(1, 2, 20, 30, 2), 0, ms(20))
        .unwrap();
    let bytes = seat.bytes;
    seat.poll(&routing, ms(20)).unwrap();
    assert_eq!(seat.bytes, bytes);
    assert_eq!(seat.pending.front().unwrap().received, ms(20));
    seat.poll(&routing, ms(21)).unwrap();
    assert_eq!(seat.bytes, bytes);
    assert_eq!(seat.pending.front().unwrap().received, ms(20));
    drain(&mut seat, &routing, 22);
    assert!(seat.bytes < bytes);
    submit(&mut seat, push(&mut routing, 1, 3, 30, 0), 30);
    ack(&mut routing, 1, 30);
    drain(&mut seat, &routing, 30);
    assert_eq!(seat.bytes, 0);
}
