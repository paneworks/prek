use super::*;
use crate::keys::producer::{Operation as Op, Request};

fn request(producer: u64, sequence: u64, operation: Op) -> Request {
    Request {
        fence: fence(),
        source: source(100),
        marker: "model-only".into(),
        consumer: Consumer::IsolatedReceiver { pid: 1, start: 1 },
        producer: Epoch([producer, 1]),
        sequence,
        operation,
    }
}
fn admit(seat: &mut Seat<TestSink>, routing: &Routing, sequence: u64, operation: Op, at: u64) {
    assert!(
        seat.producer_request(routing, &request(7, sequence, operation), ms(at))
            .unwrap()
    );
}
fn opened(expires: u64) -> (Seat<TestSink>, Rc<RefCell<Writes>>, Routing) {
    let (mut seat, writes) = model();
    let routing = projected(vec![]);
    admit(
        &mut seat,
        &routing,
        1,
        Op::Open {
            expires: ms(expires),
        },
        0,
    );
    (seat, writes, routing)
}

#[test]
fn normal_fast_press_repeat_and_release_preserve_their_complete_order() {
    let (mut seat, writes, routing) = opened(200);
    admit(&mut seat, &routing, 2, Op::Press { id: 1, key: 30 }, 10);
    admit(&mut seat, &routing, 3, Op::Repeat { id: 1 }, 11);
    admit(&mut seat, &routing, 4, Op::Release { id: 1 }, 12);
    drain(&mut seat, &routing, 12);
    assert_eq!(key_events(&writes), [(30, 1), (30, 2), (30, 0)]);
    assert_eq!(seat.software_binding_count(), 0);
    assert_eq!(seat.deadline(), Some(ms(200)));
    admit(&mut seat, &routing, 5, Op::Close {}, 13);
    assert_eq!(seat.bytes, 0);
    assert!(seat.software.is_empty());
    assert!(seat.into_output().is_ok());
}

#[test]
fn lease_expiry_releases_held_keys_without_another_frame_or_request() {
    let (mut seat, writes, routing) = opened(20);
    admit(&mut seat, &routing, 2, Op::Press { id: 1, key: 42 }, 10);
    drain(&mut seat, &routing, 10);
    assert_eq!(seat.deadline(), Some(ms(20)));
    drain(&mut seat, &routing, 20);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert!(seat.deadline().is_none());
    assert_eq!(seat.bytes, 0);
    assert!(
        !seat
            .producer_request(
                &routing,
                &request(7, 3, Op::Renew { expires: ms(100) }),
                ms(21)
            )
            .unwrap()
    );
    assert!(
        !seat
            .producer_request(
                &routing,
                &request(7, 1, Op::Open { expires: ms(100) }),
                ms(21)
            )
            .unwrap()
    );
    assert!(
        seat.producer_request(
            &routing,
            &request(8, 1, Op::Open { expires: ms(100) }),
            ms(21)
        )
        .unwrap()
    );
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
}

#[test]
fn invalid_sequences_identities_keys_and_released_handles_do_not_advance_admission() {
    let (mut seat, writes, routing) = opened(200);
    for mut wrong in [
        request(7, 3, Op::Press { id: 1, key: 30 }),
        request(7, 2, Op::Press { id: 0, key: 30 }),
        request(7, 2, Op::Press { id: 1, key: 0 }),
    ] {
        assert!(!seat.producer_request(&routing, &wrong, ms(10)).unwrap());
        wrong.fence.session += 1;
        assert!(!seat.producer_request(&routing, &wrong, ms(10)).unwrap());
    }
    admit(&mut seat, &routing, 2, Op::Press { id: 1, key: 30 }, 10);
    assert!(
        !seat
            .producer_request(
                &routing,
                &request(7, 2, Op::Press { id: 2, key: 31 }),
                ms(10)
            )
            .unwrap()
    );
    admit(&mut seat, &routing, 3, Op::Release { id: 1 }, 10);
    assert!(
        !seat
            .producer_request(&routing, &request(7, 4, Op::Repeat { id: 1 }), ms(10))
            .unwrap()
    );
    drain(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(30, 1), (30, 0)]);
    assert!(
        !seat
            .producer_request(
                &routing,
                &request(7, 4, Op::Press { id: 1, key: 31 }),
                ms(10)
            )
            .unwrap()
    );
    admit(&mut seat, &routing, 4, Op::Press { id: 2, key: 31 }, 10);
    drain(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(30, 1), (30, 0), (31, 1)]);
}

#[test]
fn epoch_revocation_cannot_be_hidden_by_fresh_authority_before_the_next_poll() {
    let (mut seat, writes, mut routing) = opened(200);
    admit(&mut seat, &routing, 2, Op::Press { id: 1, key: 42 }, 10);
    drain(&mut seat, &routing, 10);
    routing.actions_available(1, BTreeSet::new(), ms(20));
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), ms(21));
    assert!(
        !seat
            .producer_request(
                &routing,
                &request(7, 3, Op::Renew { expires: ms(200) }),
                ms(21)
            )
            .unwrap()
    );
    drain(&mut seat, &routing, 21);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert!(seat.output.producer_retired(&[7, 1]));
}

#[test]
fn producer_close_reconciles_partial_presses_and_reclaims_all_charges() {
    for prefix in [0, 1] {
        let (mut seat, writes, routing) = opened(200);
        admit(&mut seat, &routing, 2, Op::Press { id: 1, key: 30 }, 10);
        assert_eq!(seat.poll(&routing, ms(10)).unwrap(), Progress::Pending);
        writes.borrow_mut().steps.push_back(Step::Prefix(prefix));
        assert_eq!(seat.poll(&routing, ms(10)).unwrap(), Progress::Pending);
        admit(&mut seat, &routing, 3, Op::Close {}, 11);
        drain(&mut seat, &routing, 11);
        assert_eq!(
            key_events(&writes),
            if prefix == 0 {
                vec![]
            } else {
                vec![(30, 1), (30, 0)]
            }
        );
        assert_eq!(seat.bytes, 0);
        assert_eq!(seat.software_binding_count(), 0);
        assert!(seat.software.is_empty());
    }
}

#[test]
fn a_full_effect_queue_can_still_close_its_producer_without_emitting_unsent_presses() {
    let (mut seat, writes, routing) = opened(200);
    for id in 1..=MAX_PENDING as u64 {
        admit(&mut seat, &routing, id + 1, Op::Press { id, key: 30 }, 10);
    }
    assert!(
        !seat
            .producer_request(
                &routing,
                &request(7, 66, Op::Press { id: 65, key: 30 }),
                ms(10)
            )
            .unwrap()
    );
    admit(&mut seat, &routing, 66, Op::Close {}, 10);
    drain(&mut seat, &routing, 10);
    assert!(key_events(&writes).is_empty());
    assert_eq!(seat.bytes, 0);
}

#[test]
fn producer_capacity_reclaims_closed_entries_but_never_reuses_a_nonce() {
    let (mut seat, _) = model();
    let routing = projected(vec![]);
    for producer in 1..=64 {
        assert!(
            seat.producer_request(
                &routing,
                &request(producer, 1, Op::Open { expires: ms(200) }),
                ms(0)
            )
            .unwrap()
        );
    }
    assert!(
        !seat
            .producer_request(
                &routing,
                &request(65, 1, Op::Open { expires: ms(200) }),
                ms(0)
            )
            .unwrap()
    );
    assert!(
        seat.producer_request(&routing, &request(1, 2, Op::Close {}), ms(0))
            .unwrap()
    );
    assert!(
        !seat
            .producer_request(
                &routing,
                &request(1, 1, Op::Open { expires: ms(200) }),
                ms(0)
            )
            .unwrap()
    );
    assert!(
        seat.producer_request(
            &routing,
            &request(65, 1, Op::Open { expires: ms(200) }),
            ms(0)
        )
        .unwrap()
    );
    assert_eq!(seat.software.len(), 64);
    seat.retire(ms(0)).unwrap();
    assert_eq!(seat.drain(ms(0)).unwrap(), Progress::Complete);
    assert_eq!(seat.bytes, 0);
}

#[test]
fn software_waits_for_earlier_raw_but_blocks_only_later_key_domain_spans() {
    let (mut seat, writes, mut routing) = opened(200);
    routing
        .push(Record::Frame {
            frame: frame(1, 1, 10, 31, 1),
        })
        .unwrap();
    admit(&mut seat, &routing, 2, Op::Press { id: 1, key: 42 }, 11);
    assert_eq!(seat.poll(&routing, ms(11)).unwrap(), Progress::Pending);
    assert!(key_events(&writes).is_empty());
    routing
        .push(Record::Frame {
            frame: frame(2, 1, 12, 30, 1),
        })
        .unwrap();
    let old = routing
        .due_after_input(&source(1), ms(12))
        .unwrap()
        .cloned()
        .unwrap();
    let old_span = routing.input_span(&source(1), old.ticket).unwrap();
    let new = routing
        .due_after_input(&source(2), ms(12))
        .unwrap()
        .cloned()
        .unwrap();
    let new_span = routing.input_span(&source(2), new.ticket).unwrap();
    assert!(!seat.blocks_ordered_span(&routing, &source(1), old_span));
    assert!(seat.blocks_ordered_span(&routing, &source(2), new_span));
    assert!(!seat.blocks_raw(&source(1), 1));
    assert!(seat.blocks_raw(&source(2), 1));
    routing
        .delivered_after_input(&source(1), old.ticket, ms(12))
        .unwrap();
    drain(&mut seat, &routing, 12);
    assert_eq!(key_events(&writes), [(42, 1)]);
    assert!(!seat.blocks_ordered_span(&routing, &source(2), new_span));
}

#[test]
fn software_leases_cannot_extend_past_the_current_context_approval() {
    let (mut seat, writes) = model();
    let mut routing = projected(vec![]);
    assert!(
        !seat
            .producer_request(
                &routing,
                &request(7, 1, Op::Open { expires: ms(400) }),
                ms(200)
            )
            .unwrap()
    );
    assert!(
        seat.producer_request(
            &routing,
            &request(7, 1, Op::Open { expires: ms(250) }),
            ms(200)
        )
        .unwrap()
    );
    assert!(
        !seat
            .producer_request(
                &routing,
                &request(7, 2, Op::Renew { expires: ms(400) }),
                ms(201)
            )
            .unwrap()
    );
    context(&mut routing, 200, 2, ContextState::Desktop);
    assert!(
        seat.producer_request(
            &routing,
            &request(7, 2, Op::Renew { expires: ms(400) }),
            ms(201)
        )
        .unwrap()
    );
    assert_eq!(seat.deadline(), Some(ms(400)));
    assert!(key_events(&writes).is_empty());
}

#[test]
fn a_lost_desktop_cancels_software_without_waiting_for_its_lease() {
    let (mut seat, writes, mut routing) = opened(200);
    admit(&mut seat, &routing, 2, Op::Press { id: 1, key: 42 }, 10);
    drain(&mut seat, &routing, 10);
    routing.authority_mut().lock_requested();
    assert!(
        !seat
            .producer_request(
                &routing,
                &request(8, 1, Op::Open { expires: ms(100) }),
                ms(20)
            )
            .unwrap()
    );
    drain(&mut seat, &routing, 20);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
}

#[test]
fn software_cleanup_cannot_keep_the_worker_healthy_after_its_drain_budget() {
    let (mut seat, writes, routing) = opened(20);
    admit(&mut seat, &routing, 2, Op::Press { id: 1, key: 42 }, 10);
    drain(&mut seat, &routing, 10);
    writes.borrow_mut().steps.push_back(Step::Prefix(0));
    assert_eq!(seat.poll(&routing, ms(20)).unwrap(), Progress::Pending);
    assert_eq!(seat.deadline(), Some(ms(270)));
    assert!(seat.blocks_raw(&source(1), 2));
    assert_eq!(
        seat.poll(&routing, ms(270)).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert!(seat.failed());
}

#[test]
fn software_preserves_an_independent_buffered_pointer_lane() {
    let (mut seat, writes) = model();
    let mut routing = super::independence::setup(false, false);
    routing
        .push(Record::Frame {
            frame: frame(3, 1, 10, 272, 1),
        })
        .unwrap();
    assert!(routing.take_scoped_dispatches(ms(10)).is_empty());
    admit(&mut seat, &routing, 1, Op::Open { expires: ms(200) }, 10);
    admit(&mut seat, &routing, 2, Op::Press { id: 1, key: 42 }, 11);
    assert_eq!(seat.poll(&routing, ms(11)).unwrap(), Progress::Pending);
    routing
        .push(Record::Frame {
            frame: frame(3, 2, 12, 272, 0),
        })
        .unwrap();
    let raw = routing
        .due_after_input(&source(3), ms(12))
        .unwrap()
        .cloned()
        .unwrap();
    let span = routing.input_span(&source(3), raw.ticket).unwrap();
    assert!(!seat.blocks_ordered_span(&routing, &source(3), span));
    drain(&mut seat, &routing, 12);
    assert_eq!(key_events(&writes), [(42, 1)]);
}

#[test]
fn closing_a_software_provider_does_not_release_a_live_physical_remap() {
    let (mut seat, writes) = model();
    let mut routing = projected(vec![rule("shift", Action::Remap { key: Key(42) })]);
    submit(&mut seat, push(&mut routing, 1, 1, 10, 1), 10);
    ack(&mut routing, 1, 10);
    drain(&mut seat, &routing, 10);
    admit(&mut seat, &routing, 1, Op::Open { expires: ms(200) }, 11);
    admit(&mut seat, &routing, 2, Op::Press { id: 1, key: 42 }, 11);
    drain(&mut seat, &routing, 11);
    admit(&mut seat, &routing, 3, Op::Close {}, 12);
    drain(&mut seat, &routing, 12);
    assert_eq!(key_events(&writes), [(42, 1)]);
    submit(&mut seat, push(&mut routing, 1, 2, 20, 0), 20);
    ack(&mut routing, 1, 20);
    drain(&mut seat, &routing, 20);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
}

#[test]
fn close_can_overtake_unacknowledged_admissions_but_an_old_close_cannot_cancel_a_newer_sequence() {
    let (mut seat, writes, routing) = opened(200);
    admit(&mut seat, &routing, 2, Op::Press { id: 1, key: 42 }, 10);
    drain(&mut seat, &routing, 10);
    admit(&mut seat, &routing, 3, Op::Renew { expires: ms(200) }, 11);
    assert!(
        !seat
            .producer_request(&routing, &request(7, 2, Op::Close {}), ms(11))
            .unwrap()
    );
    assert_eq!(key_events(&writes), [(42, 1)]);
    admit(&mut seat, &routing, 8, Op::Close {}, 12);
    drain(&mut seat, &routing, 12);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert_eq!(seat.bytes, 0);
}
