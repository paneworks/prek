//! Bounded multi-group ownership, FIFO fallback and native receipt checks.
use super::*;

fn held(admission: ChainAdmission) -> ChainToken {
    let ChainAdmission::Held(token) = admission else {
        panic!("chain forwarded")
    };
    token
}
fn first(router: &mut Router) -> ChainToken {
    let token = held(
        router
            .start_chain(down(1, 0, 0, 10), candidate(1), ms(0))
            .unwrap(),
    );
    router.push(up(2, 10, 0), None, ms(10)).unwrap();
    token
}
fn second(router: &mut Router, token: &ChainToken) -> ChainToken {
    let token = held(
        router
            .continue_chain(down(3, 50, 0, 20), token, ms(50))
            .unwrap(),
    );
    router.push(up(4, 60, 0), None, ms(60)).unwrap();
    token
}

#[test]
fn all_segments_are_claimed_once_and_the_latest_token_seals_a_compact_receipt() {
    let mut router = router(Limits::default());
    let one = first(&mut router);
    let two = second(&mut router, &one);
    let three = held(
        router
            .continue_chain(down(5, 100, 0, 30), &two, ms(100))
            .unwrap(),
    );
    router.push(up(6, 110, 0), None, ms(110)).unwrap();
    assert!(one.segment() < two.segment() && two.segment() < three.segment());
    assert_eq!(one.candidate(), three.candidate());
    assert_eq!(three.source(), &router.device.source);
    for token in [&one, &two, &three] {
        assert_eq!(
            router.chain_ownership(token, ms(110)),
            Some(Ownership::Pending)
        );
    }
    assert_eq!(router.seal_chain(&three, ms(110)), Err(Fault::TooLate));
    router.claim_finished(1, 1, ms(110)).unwrap();
    assert!(router.queue.is_empty());
    for token in [&one, &two, &three] {
        assert_eq!(
            router.chain_ownership(token, ms(110)),
            Some(Ownership::Claimed)
        );
    }
    assert_eq!(router.seal_chain(&one, ms(110)), Err(Fault::WrongTicket));
    let receipt = router.seal_chain(&three, ms(110)).unwrap();
    assert_eq!(receipt.source(), three.source());
    assert_eq!(receipt.candidate(), 1);
    assert_eq!(receipt.segments(), 3);
    router.release_completion();
    assert!(router.ownership().is_none());
    assert!(router.chain_receipt_valid(&receipt));
    assert!(router.chain_ownership(&three, ms(110)).is_none());
    assert_eq!(
        router.continue_chain(down(7, 120, 0, 40), &three, ms(120)),
        Err(Fault::TooLate)
    );
    assert_eq!(router.last_sequence, Some(6));
}

#[test]
fn rejecting_a_chain_replays_all_frames_once_with_one_fixed_delivery_offset() {
    let mut router = router(Limits::default());
    let one = first(&mut router);
    let _two = second(&mut router, &one);
    router.cancel(1, 1, ms(140)).unwrap();
    assert_eq!(router.delivery_offset(), ms(140));
    for (at, sequence, physical) in [(140, 1, 0), (150, 2, 10), (190, 3, 50), (200, 4, 60)] {
        let entry = router.due(ms(at)).unwrap().unwrap().clone();
        assert_eq!(entry.frame.sequence, sequence);
        assert_eq!(entry.frame.at, ms(physical));
        router.delivered(entry.ticket, ms(at)).unwrap();
        assert!(router.due(ms(at)).unwrap().is_none());
    }
    assert_eq!(router.delivery_offset(), ms(0));
    assert!(router.delivered.neutral());
    assert_eq!(router.claim(1, 1, ms(200)), Err(Fault::WrongTicket));
}

#[test]
fn clocked_rejection_anchors_delivery_at_original_deadline_without_advancing_unread_input() {
    let mut router = router(Limits::default());
    let token = first(&mut router);
    second(&mut router, &token);
    router.cancel_after_input(1, 1, ms(60), ms(300)).unwrap();
    assert_eq!(router.last_at, ms(60));
    assert_eq!(router.delivery_offset(), ms(300));
    router.push(down(5, 70, 0, 30), None, ms(70)).unwrap();
    for (at, sequence) in [(300, 1), (310, 2), (350, 3), (360, 4), (370, 5)] {
        let scheduled = router.due_after_input(ms(at)).unwrap().unwrap().clone();
        assert_eq!(scheduled.frame.sequence, sequence);
        router
            .delivered_after_input(scheduled.ticket, ms(at))
            .unwrap();
    }
    assert_eq!(router.last_at, ms(70));
}

#[test]
fn clocked_rejection_before_decision_uses_wall_time_and_after_slack_requires_recovery() {
    for now in [200, 310, 350] {
        let mut router = router(Limits::default());
        let token = first(&mut router);
        second(&mut router, &token);
        let result = router.cancel_after_input(1, 1, ms(60), ms(now));
        assert_eq!(router.last_at, ms(60));
        if now != 350 {
            result.unwrap();
            assert_eq!(router.delivery_offset(), ms(now.min(300)));
            assert_eq!(
                router
                    .due_after_input(ms(now))
                    .unwrap()
                    .unwrap()
                    .frame
                    .sequence,
                1
            );
        } else {
            assert_eq!(result, Err(Fault::MissedDelivery));
            assert!(router.recover().is_some());
        }
    }
}

#[test]
fn continuing_at_the_original_decision_boundary_forwards_instead_of_renewing() {
    let mut router = router(Limits::default());
    let one = first(&mut router);
    let two = second(&mut router, &one);
    assert_eq!(router.next_deadline(), Some(ms(300)));
    assert_eq!(
        router.chain_ownership(&two, ms(299)),
        Some(Ownership::Pending)
    );
    assert!(router.chain_ownership(&two, ms(300)).is_none());
    assert_eq!(router.ownership(), Some(Ownership::Pending));
    let admission = router
        .continue_chain(down(5, 300, 0, 30), &two, ms(300))
        .unwrap();
    assert_eq!(admission, ChainAdmission::Forwarded);
    assert_eq!(router.ownership(), Some(Ownership::Forwarded));
    assert_eq!(router.delivery_offset(), ms(300));
    assert_eq!(router.claim(1, 1, ms(300)), Err(Fault::WrongTicket));
    for (at, sequence) in [(300, 1), (310, 2), (350, 3), (360, 4), (600, 5)] {
        assert_eq!(deliver(&mut router, at).unwrap().sequence, sequence);
    }
}

#[test]
fn the_latest_held_group_gets_its_own_join_window_but_not_a_new_decision() {
    let mut router = router(Limits::default());
    let one = first(&mut router);
    let two = held(
        router
            .continue_chain(down(3, 150, 0, 20), &one, ms(150))
            .unwrap(),
    );
    assert_eq!(router.next_deadline(), Some(ms(300)));
    assert_eq!(router.claimable(1, 1, ms(160)), Err(Fault::TooLate));
    assert_eq!(router.claim(1, 1, ms(160)), Err(Fault::TooLate));
    router.claimable(1, 1, ms(270)).unwrap();
    router.claim(1, 1, ms(270)).unwrap();
    assert_eq!(
        router.chain_ownership(&two, ms(270)),
        Some(Ownership::Claimed)
    );
    router.push(up(4, 280, 0), None, ms(280)).unwrap();
    assert!(router.queue.is_empty());
}

#[test]
fn reserve_extends_across_long_neutral_gaps_and_cancellation_drains_without_replay() {
    let mut router = router(Limits::default());
    let token = held(
        router
            .start_chain(
                down(1, 0, 0, 10),
                Candidate {
                    policy: Policy::Reserve,
                    ..candidate(1)
                },
                ms(0),
            )
            .unwrap(),
    );
    router.push(up(2, 10, 0), None, ms(10)).unwrap();
    let token = held(
        router
            .continue_chain(down(3, 500, 0, 20), &token, ms(500))
            .unwrap(),
    );
    assert_eq!(
        router.chain_ownership(&token, ms(500)),
        Some(Ownership::Claimed)
    );
    router.cancel(1, 1, ms(510)).unwrap();
    assert_eq!(router.ownership(), Some(Ownership::Draining));
    router.push(down(4, 520, 1, 21), None, ms(520)).unwrap();
    router.push(up(5, 530, 0), None, ms(530)).unwrap();
    assert_eq!(router.ownership(), Some(Ownership::Draining));
    router.push(up(6, 540, 1), None, ms(540)).unwrap();
    assert!(router.ownership().is_none());
    assert!(router.queue.is_empty());
    assert!(router.chain_ownership(&token, ms(540)).is_none());
    let ordinary = down(7, 550, 0, 30);
    router.push(ordinary.clone(), None, ms(550)).unwrap();
    assert_eq!(deliver(&mut router, 550), Some(ordinary));
}

#[test]
fn foreign_authorization_source_and_consumed_predecessors_are_rejected_atomically() {
    let mut router = router(Limits::default());
    let one = first(&mut router);
    let two = second(&mut router, &one);
    assert_eq!(
        router.continue_chain(down(5, 280, 0, 30), &one, ms(280)),
        Err(Fault::WrongTicket)
    );
    let mut other = super::router(Limits::default());
    other.authorization = 2;
    let foreign = held(
        other
            .start_chain(
                down(1, 0, 0, 10),
                Candidate {
                    authorization: 2,
                    ..candidate(1)
                },
                ms(0),
            )
            .unwrap(),
    );
    assert_eq!(
        router.continue_chain(down(5, 280, 0, 30), &foreign, ms(280)),
        Err(Fault::Unauthorized)
    );
    let mut stale = super::router(Limits::default());
    stale.device.source.generation += 1;
    let mut start = down(1, 0, 0, 10);
    start.source = stale.device.source.clone();
    let stale = held(stale.start_chain(start, candidate(1), ms(0)).unwrap());
    assert_eq!(
        router.continue_chain(down(5, 280, 0, 30), &stale, ms(280)),
        Err(Fault::StaleSource)
    );
    assert_eq!(router.last_at, ms(60));
    assert_eq!(router.last_sequence, Some(4));
    assert_eq!(router.next_deadline(), Some(ms(300)));
    assert!(router.queue.is_empty());
    assert_eq!(
        router.chain_ownership(&two, ms(60)),
        Some(Ownership::Pending)
    );
    let next = held(
        router
            .continue_chain(down(5, 100, 0, 30), &two, ms(100))
            .unwrap(),
    );
    assert!(next.segment() > two.segment());
}

#[test]
fn ordinary_input_without_a_continuation_token_ends_the_lease_and_is_not_stolen() {
    let mut router = router(Limits::default());
    let token = first(&mut router);
    router.push(down(3, 50, 0, 20), None, ms(50)).unwrap();
    assert_eq!(deliver(&mut router, 50).unwrap().sequence, 1);
    router.push(up(4, 60, 0), None, ms(60)).unwrap();
    assert!(router.chain_ownership(&token, ms(60)).is_none());
    assert_eq!(router.delivery_offset(), ms(50));
    for (at, sequence) in [(60, 2), (100, 3), (110, 4)] {
        assert_eq!(deliver(&mut router, at).unwrap().sequence, sequence);
    }
    assert!(router.queue.is_empty());
}

#[test]
fn claiming_a_later_chain_preserves_an_older_queued_application_release() {
    let mut router = router(Limits::default());
    router
        .push(down(1, 0, 0, 10), Some(candidate(1)), ms(0))
        .unwrap();
    router.push(up(2, 40, 0), None, ms(40)).unwrap();
    router.cancel(1, 1, ms(40)).unwrap();
    assert_eq!(deliver(&mut router, 40).unwrap().sequence, 1);
    let token = held(
        router
            .start_chain(down(3, 50, 0, 20), candidate(2), ms(50))
            .unwrap(),
    );
    router.push(up(4, 60, 0), None, ms(60)).unwrap();
    let token = held(
        router
            .continue_chain(down(5, 70, 0, 30), &token, ms(70))
            .unwrap(),
    );
    router.push(up(6, 75, 0), None, ms(75)).unwrap();
    assert_eq!(router.next_deadline(), Some(ms(80)));
    assert_eq!(router.chain_deadline(&token), Some(ms(350)));
    router.claim_finished(2, 1, ms(80)).unwrap();
    let receipt = router.seal_chain(&token, ms(80)).unwrap();
    assert_eq!(receipt.segments(), 2);
    assert_eq!(deliver(&mut router, 80).unwrap().sequence, 2);
    router.release_completion();
    assert_eq!(router.delivery_offset(), ms(0));
    assert!(router.delivered.neutral());
    router.push(down(7, 90, 0, 40), None, ms(90)).unwrap();
    assert_eq!(deliver(&mut router, 90).unwrap().sequence, 7);
}

#[test]
fn pending_frame_and_reserved_segment_budgets_cover_the_whole_chain() {
    let mut pending = router(Limits {
        frames: 3,
        ..Limits::default()
    });
    let token = first(&mut pending);
    held(
        pending
            .continue_chain(down(3, 20, 0, 20), &token, ms(20))
            .unwrap(),
    );
    assert_eq!(
        pending.push(up(4, 30, 0), None, ms(30)),
        Err(Fault::Overflow)
    );
    assert_eq!(pending.queued_frames(), 0);
    assert_eq!(pending.claim(1, 1, ms(30)), Err(Fault::Stopped));
    assert!(pending.recover().unwrap().destroy_virtual_and_release_grab);
    let mut reserved = router(Limits {
        frames: 3,
        ..Limits::default()
    });
    let mut token = held(
        reserved
            .start_chain(
                down(1, 0, 0, 10),
                Candidate {
                    policy: Policy::Reserve,
                    ..candidate(1)
                },
                ms(0),
            )
            .unwrap(),
    );
    reserved.push(up(2, 1, 0), None, ms(1)).unwrap();
    for (sequence, at) in [(3, 10), (5, 20)] {
        token = held(
            reserved
                .continue_chain(down(sequence, at, 0, sequence as i32), &token, ms(at))
                .unwrap(),
        );
        reserved
            .push(up(sequence + 1, at + 1, 0), None, ms(at + 1))
            .unwrap();
    }
    assert_eq!(
        reserved.continue_chain(down(7, 30, 0, 30), &token, ms(30)),
        Err(Fault::Overflow)
    );
    assert_eq!(reserved.claim(1, 1, ms(30)), Err(Fault::Stopped));
    assert!(reserved.recover().is_some());
}

#[test]
fn event_limits_count_all_segments_and_overflow_requires_teardown() {
    let mut router = router(Limits {
        events: 12,
        ..Limits::default()
    });
    let token = first(&mut router);
    assert_eq!(
        router.continue_chain(down(3, 20, 0, 20), &token, ms(20)),
        Err(Fault::Overflow)
    );
    assert!(router.stopped);
    let recovery = router.recover().unwrap();
    assert!(recovery.destroy_virtual_and_release_grab);
    assert!(recovery.neutral_events.is_empty());
}

#[test]
fn a_claim_receipt_survives_later_groups_but_not_transport_loss_or_reauthorization() {
    let mut router = router(Limits::default());
    let token = first(&mut router);
    router.claim_finished(1, 1, ms(10)).unwrap();
    let receipt = router.seal_chain(&token, ms(10)).unwrap();
    router.release_completion();
    router.push(down(3, 20, 0, 20), None, ms(20)).unwrap();
    assert!(router.chain_receipt_valid(&receipt));
    let mut foreign = super::router(Limits::default());
    foreign.authorization = 2;
    assert!(!foreign.chain_receipt_valid(&receipt));
    foreign.authorization = 1;
    foreign.device.source.generation += 1;
    assert!(!foreign.chain_receipt_valid(&receipt));
    router.revoke();
    assert!(!router.chain_receipt_valid(&receipt));
    router.recover().unwrap();
    assert!(!router.chain_receipt_valid(&receipt));
}

#[test]
fn consumed_segments_preserve_latent_absolute_state_for_the_next_ordinary_group() {
    let mut router = router(Limits::default());
    let token = held(
        router
            .start_chain(
                down(1, 0, 0, 10),
                Candidate {
                    policy: Policy::Reserve,
                    ..candidate(1)
                },
                ms(0),
            )
            .unwrap(),
    );
    router.push(up(2, 10, 0), None, ms(10)).unwrap();
    let changed = frame(
        3,
        20,
        &[
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 20),
            (EV_ABS, ABS_MT_POSITION_X, 900),
            (EV_ABS, ABS_MT_POSITION_Y, 500),
        ],
    );
    held(router.continue_chain(changed, &token, ms(20)).unwrap());
    router.push(up(4, 30, 0), None, ms(30)).unwrap();
    router.release_completion();
    let next = frame(
        5,
        40,
        &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, 30)],
    );
    router.push(next, None, ms(40)).unwrap();
    let output = deliver(&mut router, 40).unwrap();
    assert_eq!(output.sequence, 5);
    for (code, value) in [(ABS_MT_POSITION_X, 900), (ABS_MT_POSITION_Y, 500)] {
        assert!(
            output
                .events
                .iter()
                .any(|event| event.kind == EV_ABS && event.code == code && event.value == value)
        );
    }
}

#[test]
fn empty_or_overlapping_continuations_cannot_advance_or_consume_the_native_token() {
    let mut router = router(Limits::default());
    let token = held(
        router
            .start_chain(down(1, 0, 0, 10), candidate(1), ms(0))
            .unwrap(),
    );
    assert_eq!(
        router.continue_chain(down(2, 20, 1, 11), &token, ms(20)),
        Err(Fault::InvalidCandidate)
    );
    assert_eq!(router.last_at, ms(0));
    router.push(up(2, 10, 0), None, ms(10)).unwrap();
    assert_eq!(
        router.continue_chain(frame(3, 280, &[]), &token, ms(280)),
        Err(Fault::InvalidCandidate)
    );
    assert_eq!(router.last_at, ms(10));
    assert_eq!(router.last_sequence, Some(2));
    assert!(
        router
            .start_chain(down(3, 280, 0, 20), candidate(2), ms(280))
            .is_err()
    );
    assert_eq!(router.last_at, ms(10));
    held(
        router
            .continue_chain(down(3, 20, 0, 20), &token, ms(20))
            .unwrap(),
    );
}

#[test]
fn chain_start_requires_consuming_policy_and_counter_overflow_stops_the_transport() {
    let mut router = router(Limits::default());
    assert_eq!(
        router.start_chain(
            down(1, 0, 0, 10),
            Candidate {
                policy: Policy::Observe,
                ..candidate(1)
            },
            ms(0)
        ),
        Err(Fault::InvalidCandidate)
    );
    assert_eq!(router.last_sequence, None);
    router.next_segment = u64::MAX;
    assert_eq!(
        router.start_chain(down(1, 0, 0, 10), candidate(1), ms(0)),
        Err(Fault::Overflow)
    );
    assert!(router.stopped);
    assert!(router.recover().is_some());
}

#[test]
fn keyboard_chains_keep_all_down_up_pairs_under_the_same_decision() {
    let mut device = crate::transport::tests::touch();
    device.capabilities.entry(EV_KEY).or_default().push(30);
    device
        .capabilities
        .get_mut(&EV_KEY)
        .unwrap()
        .sort_unstable();
    device.capabilities.get_mut(&EV_KEY).unwrap().dedup();
    let mut router = router_for(device, Limits::default());
    let token = held(
        router
            .start_chain(frame(1, 0, &[(EV_KEY, 30, 1)]), candidate(1), ms(0))
            .unwrap(),
    );
    router
        .push(frame(2, 10, &[(EV_KEY, 30, 0)]), None, ms(10))
        .unwrap();
    let token = held(
        router
            .continue_chain(frame(3, 50, &[(EV_KEY, 30, 1)]), &token, ms(50))
            .unwrap(),
    );
    router
        .push(frame(4, 60, &[(EV_KEY, 30, 0)]), None, ms(60))
        .unwrap();
    router.claim_finished(1, 1, ms(60)).unwrap();
    assert_eq!(router.seal_chain(&token, ms(60)).unwrap().segments(), 2);
    assert!(router.queue.is_empty());
}

#[test]
fn variable_length_chains_preserve_every_raw_frame_or_consume_the_entire_prefix() {
    for trial in 0..128 {
        let mut router = router(Limits::default());
        let count = 2 + trial % 7;
        let mut token = None;
        let mut original = Vec::new();
        for segment in 0..count {
            let at = segment * 20;
            let sequence = segment * 2 + 1;
            let begin = down(sequence, at, (segment % 2) as i32, (segment + 10) as i32);
            let admission = match &token {
                None => router.start_chain(begin.clone(), candidate(1), ms(at)),
                Some(token) => router.continue_chain(begin.clone(), token, ms(at)),
            }
            .unwrap();
            token = Some(held(admission));
            original.push(begin);
            let end = up(sequence + 1, at + 10, (segment % 2) as i32);
            router.push(end.clone(), None, ms(at + 10)).unwrap();
            original.push(end);
        }
        let at = count * 20 + 10;
        if trial % 2 == 0 {
            router.claim_finished(1, 1, ms(at)).unwrap();
            assert_eq!(
                router
                    .seal_chain(token.as_ref().unwrap(), ms(at))
                    .unwrap()
                    .segments(),
                count
            );
            assert!(deliver(&mut router, at).is_none());
            router.release_completion();
        } else {
            router.cancel(1, 1, ms(at)).unwrap();
            let mut forwarded = Vec::new();
            for expected in &original {
                forwarded.push(deliver(&mut router, at + expected.at.as_millis() as u64).unwrap());
            }
            assert_eq!(forwarded, original);
            assert!(router.delivered.neutral());
        }
        assert!(router.queue.is_empty());
        assert!(router.ownership().is_none());
        assert_eq!(router.delivery_offset(), ms(0));
    }
}
