use super::*;
use crate::router::{KeyDecision, KeyDisposition, PendingKey};

mod hybrid;
mod tails;

fn position(frame: u64, event: usize) -> InputPosition {
    InputPosition { frame, event }
}
fn waiting(event: usize, deadline: u64) -> PendingKey {
    PendingKey {
        authorization: 1,
        event,
        deadline: ms(deadline),
    }
}
fn decision(frame: u64, event: usize, code: u16, owner: Option<u64>) -> KeyDecision {
    KeyDecision {
        authorization: 1,
        code,
        position: position(frame, event),
        owner,
    }
}
fn frame(sequence: u64, at: u64, data: &[(u16, i32)]) -> Frame {
    Frame {
        at: ms(at),
        ..keys(sequence, data)
    }
}
fn stage(router: &mut Router, frame: Frame, pending: &[PendingKey]) {
    let at = frame.at;
    let watermark = frame.sequence;
    router
        .stage_key_frame(frame, watermark, pending, &BTreeSet::new(), at)
        .unwrap();
}
fn settle(router: &mut Router, decisions: &[KeyDecision], at: u64) {
    router.decide_key_presses(decisions, ms(at)).unwrap();
    router.advance(ms(at)).unwrap();
}

#[test]
fn fallback_preserves_down_up_cadence_and_fifo_without_neutral_epoch_reset() {
    let mut router = model(Limits::default(), false);
    stage(&mut router, frame(1, 1, &[(30, 1)]), &[waiting(0, 100)]);
    stage(&mut router, frame(2, 11, &[(30, 0)]), &[]);
    stage(&mut router, frame(3, 20, &[(31, 1), (31, 0)]), &[]);
    assert!(router.due(ms(99)).unwrap().is_none());
    assert!(!router.prefix_delivered(1, true));
    assert!(!router.position_prefix_delivered(position(1, 0), true));
    let first = delivered(&mut router, 100);
    assert_eq!(first.len(), 1);
    assert_eq!(edges(&first[0].frame), [(30, 1)]);
    assert_eq!(first[0].due, ms(100));
    assert!(router.due(ms(109)).unwrap().is_none());
    assert_eq!(edges(&delivered(&mut router, 110)[0].frame), [(30, 0)]);
    assert!(router.due(ms(118)).unwrap().is_none());
    assert_eq!(
        edges(&delivered(&mut router, 119)[0].frame),
        [(31, 1), (31, 0)]
    );
    assert!(router.prefix_delivered(3, true));
    assert_eq!(router.offset, Duration::ZERO);
}

#[test]
fn delayed_consumption_holds_the_following_ordinary_prefix_until_resolution() {
    let mut router = model(Limits::default(), false);
    stage(&mut router, frame(1, 1, &[(30, 1)]), &[waiting(0, 100)]);
    stage(&mut router, frame(2, 10, &[(31, 1)]), &[]);
    stage(&mut router, frame(3, 20, &[(31, 0), (30, 0)]), &[]);
    settle(&mut router, &[decision(1, 0, 30, Some(8))], 50);
    assert_eq!(
        router.key_disposition(30, position(1, 0)),
        Some(KeyDisposition::Consumed { owner: 8 })
    );
    assert_eq!(edges(&delivered(&mut router, 50)[0].frame), [(31, 1)]);
    assert!(router.due(ms(59)).unwrap().is_none());
    assert_eq!(edges(&delivered(&mut router, 60)[0].frame), [(31, 0)]);
}

#[test]
fn same_report_prefix_is_released_before_pending_suffix_and_still_requires_ack() {
    let mut router = model(Limits::default(), false);
    stage(
        &mut router,
        frame(1, 1, &[(31, 1), (30, 1), (31, 0), (30, 0)]),
        &[waiting(1, 100)],
    );
    let first = router.due(ms(1)).unwrap().unwrap().clone();
    assert_eq!(edges(&first.frame), [(31, 1)]);
    assert_eq!(router.pending_key_position(), Some(position(1, 1)));
    assert!(!router.position_prefix_delivered(position(1, 0), true));
    router.delivered(first.ticket, ms(1)).unwrap();
    assert!(router.position_prefix_delivered(position(1, 1), false));
    assert!(!router.position_prefix_delivered(position(1, 1), true));
    settle(&mut router, &[decision(1, 1, 30, Some(9))], 50);
    assert_eq!(edges(&delivered(&mut router, 50)[0].frame), [(31, 0)]);
    assert!(router.position_prefix_delivered(InputPosition::after_frame(1), true));
}

#[test]
fn released_old_press_and_fresh_repress_have_distinct_ownership() {
    let mut router = model(Limits::default(), false);
    stage(
        &mut router,
        frame(1, 1, &[(30, 1), (30, 0), (30, 1)]),
        &[waiting(0, 100), waiting(2, 100)],
    );
    settle(
        &mut router,
        &[decision(1, 0, 30, Some(2)), decision(1, 2, 30, None)],
        40,
    );
    assert_eq!(edges(&delivered(&mut router, 40)[0].frame), [(30, 1)]);
    assert_eq!(
        router.key_disposition(30, position(1, 2)),
        Some(KeyDisposition::Ordinary)
    );
    assert_eq!(
        router.decide_key_presses(&[decision(1, 0, 30, Some(3))], ms(40)),
        Err(Fault::TooLate)
    );
    stage(&mut router, frame(2, 41, &[(30, 0)]), &[]);
    assert_eq!(edges(&delivered(&mut router, 80)[0].frame), [(30, 0)]);
}

#[test]
fn compound_decisions_are_atomic_and_cannot_steal_ordinary_members() {
    let mut router = model(Limits::default(), false);
    stage(
        &mut router,
        frame(1, 1, &[(30, 1), (31, 1)]),
        &[waiting(0, 100)],
    );
    assert_eq!(
        router.decide_key_presses(
            &[decision(1, 0, 30, Some(5)), decision(1, 1, 31, Some(5))],
            ms(20)
        ),
        Err(Fault::TooLate)
    );
    assert_eq!(
        router.key_disposition(30, position(1, 0)),
        Some(KeyDisposition::Pending { deadline: ms(100) })
    );
    assert!(!router.stopped);
    settle(&mut router, &[decision(1, 0, 30, None)], 20);
    assert_eq!(
        edges(&delivered(&mut router, 20)[0].frame),
        [(30, 1), (31, 1)]
    );
}

#[test]
fn settled_ordinary_and_consumed_choices_are_irreversible() {
    let mut router = model(Limits::default(), false);
    stage(
        &mut router,
        frame(1, 1, &[(30, 1), (31, 1)]),
        &[waiting(0, 100), waiting(1, 100)],
    );
    router
        .decide_key_presses(
            &[decision(1, 0, 30, None), decision(1, 1, 31, Some(5))],
            ms(20),
        )
        .unwrap();
    assert_eq!(
        router.decide_key_presses(&[decision(1, 0, 30, Some(5))], ms(20)),
        Err(Fault::TooLate)
    );
    assert_eq!(
        router.decide_key_presses(&[decision(1, 1, 31, Some(6))], ms(20)),
        Err(Fault::TooLate)
    );
    assert_eq!(
        router.decide_key_presses(&[decision(1, 1, 31, None)], ms(20)),
        Err(Fault::TooLate)
    );
    router
        .decide_key_presses(&[decision(1, 1, 31, Some(5))], ms(20))
        .unwrap();
    assert_eq!(edges(&delivered(&mut router, 20)[0].frame), [(30, 1)]);
}

#[test]
fn same_deadline_decision_must_precede_expiry_and_late_claim_is_rejected() {
    let mut router = model(Limits::default(), false);
    stage(&mut router, frame(1, 1, &[(30, 1)]), &[waiting(0, 100)]);
    settle(&mut router, &[decision(1, 0, 30, Some(1))], 100);
    assert!(router.due(ms(100)).unwrap().is_none());
    let mut expired = model(Limits::default(), false);
    stage(&mut expired, frame(1, 1, &[(30, 1)]), &[waiting(0, 100)]);
    expired.advance(ms(100)).unwrap();
    assert_eq!(
        expired.decide_key_presses(&[decision(1, 0, 30, Some(1))], ms(100)),
        Err(Fault::TooLate)
    );
    let mut late = model(Limits::default(), false);
    stage(&mut late, frame(1, 1, &[(30, 1)]), &[waiting(0, 100)]);
    assert_eq!(
        late.decide_key_presses(&[decision(1, 0, 30, Some(1))], ms(101)),
        Err(Fault::TooLate)
    );
}

#[test]
fn overdue_poll_cannot_hide_an_earlier_expiry_behind_a_later_one() {
    let mut router = model(Limits::default(), false);
    stage(&mut router, frame(1, 1, &[(30, 1)]), &[waiting(0, 10)]);
    stage(&mut router, frame(2, 2, &[(31, 1)]), &[waiting(0, 100)]);
    assert_eq!(router.due(ms(100)), Err(Fault::MissedDelivery));
    assert_eq!(router.queue.front().unwrap().due, ms(10));
}

#[test]
fn overdue_fallback_uses_deadline_not_poll_time_and_enforces_scheduler_slack() {
    for (at, missed) in [(120, false), (121, true)] {
        let mut router = model(Limits::default(), false);
        stage(&mut router, frame(1, 1, &[(30, 1)]), &[waiting(0, 100)]);
        let result = router.due(ms(at));
        if missed {
            assert_eq!(result, Err(Fault::MissedDelivery));
        } else {
            assert_eq!(result.unwrap().unwrap().due, ms(100));
        }
    }
}

#[test]
fn ordinary_prefix_before_a_late_decision_is_not_retimed_to_hide_missed_delivery() {
    let mut router = model(Limits::default(), false);
    stage(
        &mut router,
        frame(1, 1, &[(31, 1), (30, 1)]),
        &[waiting(1, 100)],
    );
    router
        .decide_key_presses(&[decision(1, 1, 30, Some(1))], ms(50))
        .unwrap();
    assert_eq!(router.due(ms(50)), Err(Fault::MissedDelivery));
    assert_eq!(router.queue.front().unwrap().due, ms(1));
}

#[test]
fn consumed_repeat_duplicate_edges_and_scan_metadata_are_drained() {
    let mut router = model(Limits::default(), false);
    stage(&mut router, frame(1, 1, &[(30, 1)]), &[waiting(0, 100)]);
    settle(&mut router, &[decision(1, 0, 30, Some(5))], 1);
    let projected = router
        .push_decided_key_frame(
            packet(
                2,
                &[
                    (EV_MSC, MSC_SCAN, 3),
                    (EV_KEY, 30, 1),
                    (EV_KEY, 30, 2),
                    (EV_KEY, 30, 0),
                    (EV_KEY, 30, 0),
                ],
            ),
            2,
            &[],
            &BTreeSet::new(),
            ms(2),
        )
        .unwrap();
    assert_eq!(
        projected
            .consumed
            .iter()
            .map(|edge| edge.state)
            .collect::<Vec<_>>(),
        [KeyState::Repeat, KeyState::Release]
    );
    assert!(projected.tickets.is_empty());
    let fresh = router
        .push_decided_key_frame(keys(3, &[(30, 1)]), 3, &[], &BTreeSet::new(), ms(3))
        .unwrap();
    assert_eq!(fresh.tickets.len(), 1);
}

#[test]
fn scan_code_waits_with_its_key_and_unrelated_metadata_is_preserved() {
    let mut router = model(Limits::default(), false);
    router
        .stage_key_frame(
            packet(
                1,
                &[
                    (EV_MSC, MSC_SCAN, 80),
                    (EV_KEY, 30, 1),
                    (EV_MSC, MSC_SCAN, 81),
                    (EV_KEY, 31, 1),
                ],
            ),
            1,
            &[waiting(1, 100)],
            &BTreeSet::new(),
            ms(1),
        )
        .unwrap();
    assert!(router.due(ms(1)).unwrap().is_none());
    settle(&mut router, &[decision(1, 1, 30, Some(7))], 30);
    assert_eq!(
        delivered(&mut router, 30)[0].frame.events,
        packet(1, &[(EV_MSC, MSC_SCAN, 81), (EV_KEY, 31, 1)]).events
    );
    let mut ordinary = model(Limits::default(), false);
    let original = packet(1, &[(EV_MSC, 5, 19), (EV_KEY, 30, 1)]);
    stage(&mut ordinary, original.clone(), &[]);
    assert_eq!(delivered(&mut ordinary, 1)[0].frame, original);
}

#[test]
fn recovery_discards_undelivered_input_and_only_releases_acknowledged_prefixes() {
    let mut router = model(Limits::default(), false);
    stage(
        &mut router,
        frame(1, 1, &[(31, 1), (30, 1)]),
        &[waiting(1, 100)],
    );
    delivered(&mut router, 1);
    stage(&mut router, frame(2, 2, &[(32, 1)]), &[]);
    let recovery = router.recover().unwrap();
    assert_eq!(
        recovery
            .neutral_events
            .iter()
            .filter(|event| event.kind == EV_KEY)
            .map(|event| (event.code, event.value))
            .collect::<Vec<_>>(),
        [(31, 0)]
    );
    assert!(router.pending_keyboard.is_none());
    assert!(router.queue.is_empty());
    assert!(router.recover().is_none());
    assert_eq!(router.due(ms(100)), Err(Fault::Stopped));
}

#[test]
fn capacity_counts_waiting_raw_frames_and_scheduled_fragments_together() {
    for limits in [
        Limits {
            frames: 2,
            ..Limits::default()
        },
        Limits {
            events: 5,
            ..Limits::default()
        },
    ] {
        let mut router = model(limits, false);
        stage(
            &mut router,
            frame(1, 1, &[(31, 1), (30, 1)]),
            &[waiting(1, 100)],
        );
        router.advance(ms(1)).unwrap();
        assert_eq!(
            router.stage_key_frame(frame(2, 2, &[(31, 0)]), 2, &[], &BTreeSet::new(), ms(2)),
            Err(Fault::Overflow)
        );
    }
}

#[test]
fn invalid_authorization_stops_without_stealing_a_pending_press() {
    let mut router = model(Limits::default(), false);
    stage(&mut router, frame(1, 1, &[(30, 1)]), &[waiting(0, 100)]);
    let bad = KeyDecision {
        authorization: 2,
        ..decision(1, 0, 30, Some(1))
    };
    assert_eq!(
        router.decide_key_presses(&[bad], ms(10)),
        Err(Fault::Unauthorized)
    );
    assert!(router.recover().unwrap().neutral_events.is_empty());
}

#[test]
fn stale_source_does_not_stop_the_current_pending_source() {
    let mut router = model(Limits::default(), false);
    stage(&mut router, frame(1, 1, &[(30, 1)]), &[waiting(0, 100)]);
    let mut stale = frame(2, 2, &[(30, 0)]);
    stale.source.generation = 2;
    assert_eq!(
        router.stage_key_frame(stale, 2, &[], &BTreeSet::new(), ms(2)),
        Err(Fault::StaleSource)
    );
    assert!(!router.stopped);
    settle(&mut router, &[decision(1, 0, 30, Some(1))], 10);
}

#[test]
fn decisions_reject_duplicates_wrong_codes_zero_owners_and_backward_time_atomically() {
    let mut router = model(Limits::default(), false);
    stage(&mut router, frame(1, 1, &[(30, 1)]), &[waiting(0, 100)]);
    for decisions in [
        vec![decision(1, 0, 30, Some(1)); 2],
        vec![decision(1, 0, 31, Some(1))],
        vec![decision(1, 0, 30, Some(0))],
    ] {
        assert_eq!(
            router.decide_key_presses(&decisions, ms(10)),
            Err(Fault::InvalidCandidate)
        );
        assert_eq!(
            router.key_disposition(30, position(1, 0)),
            Some(KeyDisposition::Pending { deadline: ms(100) })
        );
    }
    router
        .decide_key_presses(&[decision(1, 0, 30, Some(1))], ms(20))
        .unwrap();
    assert_eq!(
        router.decide_key_presses(&[decision(1, 0, 30, Some(1))], ms(19)),
        Err(Fault::BackwardTime)
    );
}

#[test]
fn immediate_adapter_preserves_owner_replay_rejection_and_original_spans() {
    let mut router = model(Limits::default(), false);
    let projected = router
        .push_decided_key_frame(
            keys(1, &[(31, 1), (30, 1), (31, 0), (30, 0)]),
            5,
            &[claim(1, 1)],
            &BTreeSet::new(),
            ms(1),
        )
        .unwrap();
    assert_eq!(
        projected
            .consumed
            .iter()
            .map(|edge| edge.position)
            .collect::<Vec<_>>(),
        [position(5, 1), position(5, 3)]
    );
    assert_eq!(projected.tickets.len(), 2);
    assert_eq!(
        router.input_span(projected.tickets[1]).unwrap().first,
        position(5, 2)
    );
    delivered(&mut router, 1);
    assert_eq!(
        router.push_decided_key_frame(
            keys(2, &[(30, 1)]),
            6,
            &[claim(1, 0)],
            &BTreeSet::new(),
            ms(2)
        ),
        Err(Fault::InvalidCandidate)
    );
}

#[test]
fn ordinary_held_down_cannot_be_reenrolled_as_pending_by_duplicate_down() {
    let mut router = model(Limits::default(), false);
    stage(&mut router, keys(1, &[(30, 1)]), &[]);
    delivered(&mut router, 1);
    assert_eq!(
        router.stage_key_frame(
            keys(2, &[(30, 1)]),
            2,
            &[waiting(0, 100)],
            &BTreeSet::new(),
            ms(2)
        ),
        Err(Fault::TooLate)
    );
}

#[test]
fn journal_and_legacy_modes_cannot_mix_but_hybrid_key_reports_are_admitted() {
    let mut router = model(Limits::default(), false);
    stage(&mut router, keys(1, &[(30, 1)]), &[waiting(0, 100)]);
    assert_eq!(
        router.push_key_frame(keys(2, &[(31, 1)]), 2, &[], &BTreeSet::new(), ms(2)),
        Err(Fault::TooLate)
    );
    let mut hybrid = model(Limits::default(), true);
    hybrid
        .stage_key_frame(keys(1, &[(30, 1)]), 1, &[], &BTreeSet::new(), ms(1))
        .unwrap();
    assert_eq!(edges(&delivered(&mut hybrid, 1)[0].frame), [(30, 1)]);
}

#[test]
fn decision_window_is_bounded_and_unknown_metadata_cannot_cross_a_pending_cut() {
    for (frame, pending) in [
        (keys(1, &[(30, 1)]), waiting(0, 302)),
        (
            packet(1, &[(EV_MSC, 5, 1), (EV_KEY, 30, 1)]),
            waiting(1, 100),
        ),
    ] {
        let mut router = model(Limits::default(), false);
        assert!(
            router
                .stage_key_frame(frame, 1, &[pending], &BTreeSet::new(), ms(1))
                .is_err()
        );
        assert!(router.physical.neutral());
        assert_eq!(router.storage(), (0, 0));
    }
}

#[test]
fn successive_waits_use_maximum_offset_and_neutral_completion_resets_it() {
    let mut router = model(Limits::default(), false);
    stage(&mut router, frame(1, 1, &[(30, 1)]), &[waiting(0, 100)]);
    assert_eq!(router.next_deadline(), Some(ms(100)));
    delivered(&mut router, 100);
    assert_eq!(router.delivery_offset(), ms(99));
    stage(&mut router, frame(2, 101, &[(31, 1)]), &[waiting(0, 200)]);
    delivered(&mut router, 200);
    assert_eq!(router.delivery_offset(), ms(99));
    stage(&mut router, frame(3, 201, &[(30, 0), (31, 0)]), &[]);
    delivered(&mut router, 300);
    assert_eq!(router.delivery_offset(), Duration::ZERO);
    stage(&mut router, frame(4, 301, &[(32, 1), (32, 0)]), &[]);
    assert_eq!(delivered(&mut router, 301)[0].due, ms(301));
}

#[test]
fn orphan_release_does_not_invent_an_ownable_press() {
    let mut router = model(Limits::default(), false);
    stage(&mut router, keys(1, &[(30, 0)]), &[]);
    assert_eq!(router.key_disposition(30, position(1, 0)), None);
    assert_eq!(
        router.decide_key_presses(&[decision(1, 0, 30, Some(1))], ms(1)),
        Err(Fault::TooLate)
    );
    assert_eq!(edges(&delivered(&mut router, 1)[0].frame), [(30, 0)]);
}

#[test]
fn empty_projection_at_maximum_ticket_does_not_overflow() {
    let mut router = model(Limits::default(), false);
    router.next_ticket = u64::MAX;
    let projected = router
        .push_decided_key_frame(
            keys(1, &[(30, 1), (30, 0)]),
            1,
            &[claim(1, 0)],
            &BTreeSet::new(),
            ms(1),
        )
        .unwrap();
    assert!(projected.tickets.is_empty());
    assert_eq!(projected.consumed.len(), 2);
}

#[test]
fn unchanged_ordinary_reports_staged_together_keep_their_original_clocks() {
    let mut router = model(Limits::default(), false);
    stage(&mut router, keys(1, &[(30, 1)]), &[]);
    stage(&mut router, keys(2, &[(30, 0)]), &[]);
    let packets = delivered(&mut router, 2);
    assert_eq!(
        packets.iter().map(|packet| packet.due).collect::<Vec<_>>(),
        [ms(1), ms(2)]
    );
    assert_eq!(router.delivery_offset(), Duration::ZERO);
}

#[test]
fn consumed_tombstones_and_queued_prefixes_do_not_retain_old_incarnations() {
    let mut router = model(Limits::default(), false);
    for n in 1..=100 {
        router
            .push_decided_key_frame(
                keys(n, &[(30, 1), (30, 0)]),
                n,
                &[claim(n, 0)],
                &BTreeSet::new(),
                ms(n),
            )
            .unwrap();
        let ledger = router.pending_keyboard.as_ref().unwrap();
        assert_eq!(ledger.storage(), (0, 0));
        assert!(format!("{ledger:?}").contains("presses: 1"));
    }
    assert!(router.recover().unwrap().neutral_events.is_empty());
}

#[test]
fn immediate_adapter_matches_direct_ledger_for_consumed_duplicate_edges() {
    let mut old = model(Limits::default(), false);
    let mut new = model(Limits::default(), false);
    for (frame, claims, cuts) in [
        (keys(1, &[(30, 1)]), vec![claim(1, 0)], BTreeSet::new()),
        (
            keys(2, &[(31, 1), (30, 1), (31, 0)]),
            vec![],
            BTreeSet::new(),
        ),
        (
            packet(
                3,
                &[
                    (EV_KEY, 31, 1),
                    (EV_MSC, MSC_SCAN, 1),
                    (EV_KEY, 30, 1),
                    (EV_KEY, 31, 0),
                ],
            ),
            vec![],
            BTreeSet::new(),
        ),
        (
            keys(4, &[(31, 1), (30, 2), (31, 0), (30, 0), (30, 0)]),
            vec![],
            BTreeSet::new(),
        ),
        (
            keys(5, &[(31, 1), (30, 0), (31, 0)]),
            vec![],
            BTreeSet::from([1]),
        ),
        (keys(6, &[(30, 1), (30, 0)]), vec![], BTreeSet::from([0])),
    ] {
        let at = frame.at;
        let watermark = frame.sequence * 2;
        let expected = old
            .push_key_frame(frame.clone(), watermark, &claims, &cuts, at)
            .unwrap();
        let actual = new
            .push_decided_key_frame(frame.clone(), watermark, &claims, &cuts, at)
            .unwrap();
        assert_eq!(
            actual, expected,
            "projection for original sequence {}",
            frame.sequence
        );
        for ticket in expected.tickets {
            assert_eq!(new.input_span(ticket), old.input_span(ticket));
        }
        assert_eq!(
            delivered(&mut new, frame.sequence),
            delivered(&mut old, frame.sequence)
        );
    }
}

#[test]
fn later_fallback_in_the_same_report_cannot_hide_an_earlier_missed_deadline() {
    let mut router = model(Limits::default(), false);
    stage(
        &mut router,
        frame(1, 1, &[(30, 1), (31, 1)]),
        &[waiting(0, 10), waiting(1, 100)],
    );
    assert_eq!(router.due(ms(100)), Err(Fault::MissedDelivery));
    assert_eq!(router.queue.front().unwrap().due, ms(10));
}

#[test]
fn ordinary_fallback_cannot_retime_an_earlier_ordinary_same_report_prefix() {
    let mut router = model(Limits::default(), false);
    stage(
        &mut router,
        frame(1, 1, &[(31, 1), (30, 1)]),
        &[waiting(1, 100)],
    );
    router
        .decide_key_presses(&[decision(1, 1, 30, None)], ms(50))
        .unwrap();
    assert_eq!(router.due(ms(50)), Err(Fault::MissedDelivery));
    assert_eq!(router.queue.front().unwrap().due, ms(1));
}

#[test]
fn independent_deadlines_split_at_scan_boundaries_and_keep_original_positions() {
    let mut router = model(Limits::default(), false);
    router
        .stage_key_frame(
            packet(
                1,
                &[
                    (EV_MSC, MSC_SCAN, 30),
                    (EV_KEY, 30, 1),
                    (EV_MSC, MSC_SCAN, 31),
                    (EV_KEY, 31, 1),
                ],
            ),
            7,
            &[waiting(1, 10), waiting(3, 100)],
            &BTreeSet::new(),
            ms(1),
        )
        .unwrap();
    let first = router.due(ms(10)).unwrap().unwrap().clone();
    assert_eq!(
        first.frame.events,
        packet(1, &[(EV_MSC, MSC_SCAN, 30), (EV_KEY, 30, 1)]).events
    );
    assert_eq!(
        router.input_span(first.ticket).unwrap().first,
        position(7, 0)
    );
    router.delivered(first.ticket, ms(10)).unwrap();
    let second = router.due(ms(100)).unwrap().unwrap().clone();
    assert_eq!(
        second.frame.events,
        packet(1, &[(EV_MSC, MSC_SCAN, 31), (EV_KEY, 31, 1)]).events
    );
    assert_eq!(
        router.input_span(second.ticket).unwrap().first,
        position(7, 2)
    );
    assert_eq!(second.due, ms(100));
}
