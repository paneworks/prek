use super::*;
use crate::transport::{Consumer, ConsumerEvidence, Descriptor, KernelIdentity, Readiness};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}
fn source() -> Source {
    Source {
        device: 1,
        generation: 1,
        seat: "seat0".into(),
    }
}
fn model(limits: Limits, hybrid: bool) -> Router {
    let mut capabilities =
        BTreeMap::from([(EV_KEY, vec![30, 31, 32, 42]), (EV_MSC, vec![MSC_SCAN, 5])]);
    if hybrid {
        capabilities.insert(EV_REL, vec![0]);
    }
    let device = Device {
        source: source(),
        identity: "projection-model".into(),
        name: "Keyboard projection".into(),
        path: String::new(),
        physical: String::new(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        capabilities,
        axes: BTreeMap::new(),
        virtual_device: false,
    };
    let descriptor = Descriptor::mirror(
        &device,
        KernelIdentity {
            bus: 3,
            vendor: 1,
            product: 1,
            version: 1,
        },
        None,
        &BTreeMap::new(),
        BTreeMap::new(),
    )
    .unwrap();
    let consumer = Consumer::IsolatedReceiver { pid: 1, start: 1 };
    let mut gate = Readiness::new(descriptor.clone(), consumer.clone(), ms(0)).unwrap();
    gate.metadata(&descriptor, ms(0)).unwrap();
    gate.consumer(
        &ConsumerEvidence {
            consumer,
            marker: descriptor.marker,
            name: descriptor.name,
            class: descriptor.class,
            seat: "seat0".into(),
            settings: BTreeMap::new(),
            at: ms(0),
        },
        ms(0),
    )
    .unwrap();
    gate.neutral(true, ms(0)).unwrap();
    Router::new(device, &gate.finish(ms(0)).unwrap(), 1, limits, ms(0)).unwrap()
}
fn packet(sequence: u64, data: &[(u16, u16, i32)]) -> Frame {
    let mut events: Vec<_> = data
        .iter()
        .map(|&(kind, code, value)| RawEvent { kind, code, value })
        .collect();
    events.push(RawEvent {
        kind: EV_SYN,
        code: SYN_REPORT,
        value: 0,
    });
    Frame {
        source: source(),
        sequence,
        at: ms(sequence),
        events,
    }
}
fn keys(sequence: u64, data: &[(u16, i32)]) -> Frame {
    packet(
        sequence,
        &data
            .iter()
            .map(|&(code, value)| (EV_KEY, code, value))
            .collect::<Vec<_>>(),
    )
}
fn claim(id: u64, event: usize) -> KeyClaim {
    KeyClaim {
        id,
        event,
        authorization: 1,
    }
}
fn push(router: &mut Router, frame: Frame, claims: &[KeyClaim], cuts: &[usize]) -> KeyProjection {
    let sequence = frame.sequence;
    router
        .push_key_frame(
            frame,
            sequence,
            claims,
            &cuts.iter().copied().collect(),
            ms(sequence),
        )
        .unwrap()
}
fn delivered(router: &mut Router, at: u64) -> Vec<Scheduled> {
    let mut frames = Vec::new();
    while let Some(frame) = router.due(ms(at)).unwrap().cloned() {
        router.delivered(frame.ticket, ms(at)).unwrap();
        frames.push(frame);
    }
    frames
}
fn edges(frame: &Frame) -> Vec<(u16, i32)> {
    frame
        .events
        .iter()
        .filter(|event| event.kind == EV_KEY)
        .map(|event| (event.code, event.value))
        .collect()
}

#[test]
fn held_consumption_preserves_unrelated_typing_and_drains_repeat_up_after_revocation() {
    let mut router = model(Limits::default(), false);
    let down = push(&mut router, keys(1, &[(30, 1)]), &[claim(1, 0)], &[]);
    assert!(down.tickets.is_empty());
    assert_eq!(down.consumed[0].owner, 1);
    let ordinary = keys(2, &[(31, 1), (31, 0)]);
    push(&mut router, ordinary.clone(), &[], &[]);
    assert_eq!(delivered(&mut router, 2)[0].frame, ordinary);
    let drain = push(&mut router, keys(3, &[(30, 2), (30, 0), (30, 0)]), &[], &[]);
    assert_eq!(
        drain
            .consumed
            .iter()
            .map(|edge| edge.state)
            .collect::<Vec<_>>(),
        [KeyState::Repeat, KeyState::Release]
    );
    assert!(drain.tickets.is_empty());
    assert!(
        push(&mut router, keys(4, &[(30, 0)]), &[], &[])
            .tickets
            .is_empty()
    );
    let fresh = keys(5, &[(30, 1), (30, 0)]);
    push(&mut router, fresh.clone(), &[], &[]);
    assert_eq!(delivered(&mut router, 5)[0].frame, fresh);
}

#[test]
fn same_report_cuts_emit_complete_ordered_packets_with_original_spans() {
    let mut router = model(Limits::default(), false);
    let result = push(
        &mut router,
        keys(1, &[(31, 1), (30, 1), (31, 0), (30, 0), (32, 1), (32, 0)]),
        &[claim(1, 1)],
        &[],
    );
    assert_eq!(result.tickets.len(), 3);
    assert_eq!(
        result
            .consumed
            .iter()
            .map(|edge| edge.position.event)
            .collect::<Vec<_>>(),
        [1, 3]
    );
    let spans = result
        .tickets
        .iter()
        .map(|&ticket| router.input_span(ticket).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        spans
            .iter()
            .map(|span| (span.sequence, span.first.event, span.last.event))
            .collect::<Vec<_>>(),
        [(1, 0, 0), (1, 2, 2), (1, 4, 5)]
    );
    let packets = delivered(&mut router, 1);
    assert_eq!(
        packets
            .iter()
            .map(|packet| packet.frame.sequence)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        packets
            .iter()
            .map(|packet| edges(&packet.frame))
            .collect::<Vec<_>>(),
        [vec![(31, 1)], vec![(31, 0)], vec![(32, 1), (32, 0)]]
    );
    for packet in packets {
        assert_eq!(
            packet.frame.events.last(),
            Some(&RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0
            })
        );
        assert_eq!(
            packet
                .frame
                .events
                .iter()
                .filter(|event| event.kind == EV_SYN)
                .count(),
            1
        );
        assert!(router.input_span(packet.ticket).is_none());
    }
}

#[test]
fn observer_cuts_follow_the_key_and_acknowledged_prefixes_are_positioned() {
    let mut router = model(Limits::default(), false);
    let result = push(&mut router, keys(1, &[(30, 1), (31, 1)]), &[], &[0]);
    let position = InputPosition { frame: 1, event: 0 };
    assert!(router.position_prefix_delivered(position, false));
    assert!(!router.position_prefix_delivered(position, true));
    let first = router.due(ms(1)).unwrap().unwrap().clone();
    assert_eq!(edges(&first.frame), [(30, 1)]);
    router.delivered(first.ticket, ms(1)).unwrap();
    assert!(router.position_prefix_delivered(position, true));
    assert!(!router.position_prefix_delivered(InputPosition::after_frame(1), false));
    assert_eq!(result.tickets.len(), 2);
    delivered(&mut router, 1);
    assert!(router.position_prefix_delivered(InputPosition::after_frame(1), true));
}

#[test]
fn a_passed_down_cannot_be_stolen_by_a_duplicate_press_claim() {
    let mut router = model(Limits::default(), false);
    push(&mut router, keys(1, &[(30, 1)]), &[], &[]);
    delivered(&mut router, 1);
    assert_eq!(
        router.push_key_frame(
            keys(2, &[(30, 1)]),
            2,
            &[claim(1, 0)],
            &BTreeSet::new(),
            ms(2)
        ),
        Err(Fault::TooLate)
    );
    assert_eq!(router.keyboard.as_ref().unwrap().last_claim, 0);
    assert_eq!(
        router
            .recover()
            .unwrap()
            .neutral_events
            .iter()
            .filter(|event| event.kind == EV_KEY)
            .map(|event| event.code)
            .collect::<Vec<_>>(),
        [30]
    );
}

#[test]
fn duplicate_owned_edges_do_not_invent_actions_or_leak_orphan_releases() {
    let mut router = model(Limits::default(), false);
    let result = push(
        &mut router,
        keys(1, &[(30, 1), (30, 1), (30, 2), (30, 0), (30, 0)]),
        &[claim(1, 0)],
        &[],
    );
    assert_eq!(
        result
            .consumed
            .iter()
            .map(|edge| (edge.position.event, edge.state))
            .collect::<Vec<_>>(),
        [
            (0, KeyState::Press),
            (2, KeyState::Repeat),
            (3, KeyState::Release)
        ]
    );
    assert!(result.tickets.is_empty());
    assert!(
        push(&mut router, keys(2, &[(30, 0)]), &[], &[])
            .tickets
            .is_empty()
    );
    let result = push(
        &mut router,
        keys(3, &[(30, 1), (30, 0)]),
        &[claim(2, 0)],
        &[],
    );
    assert!(result.consumed.iter().all(|edge| edge.owner == 2));
}

#[test]
fn separate_pressed_keys_keep_separate_owners() {
    let mut router = model(Limits::default(), false);
    let result = push(
        &mut router,
        keys(
            1,
            &[
                (30, 1),
                (31, 1),
                (30, 0),
                (32, 1),
                (31, 2),
                (31, 0),
                (32, 0),
            ],
        ),
        &[claim(4, 0), claim(7, 1)],
        &[],
    );
    assert_eq!(
        result
            .consumed
            .iter()
            .map(|edge| edge.owner)
            .collect::<Vec<_>>(),
        [4, 7, 4, 7, 7]
    );
    assert_eq!(
        delivered(&mut router, 1)
            .iter()
            .flat_map(|packet| edges(&packet.frame))
            .collect::<Vec<_>>(),
        [(32, 1), (32, 0)]
    );
}

#[test]
fn scan_metadata_is_dropped_only_with_its_adjacent_consumed_key() {
    let mut router = model(Limits::default(), false);
    let result = push(
        &mut router,
        packet(
            1,
            &[
                (EV_MSC, MSC_SCAN, 90),
                (EV_KEY, 30, 1),
                (EV_MSC, MSC_SCAN, 91),
                (EV_KEY, 31, 1),
                (EV_MSC, MSC_SCAN, 90),
                (EV_KEY, 30, 0),
                (EV_MSC, MSC_SCAN, 91),
                (EV_KEY, 31, 0),
            ],
        ),
        &[claim(1, 1)],
        &[],
    );
    assert_eq!(
        result
            .consumed
            .iter()
            .map(|edge| edge.position.event)
            .collect::<Vec<_>>(),
        [1, 5]
    );
    let frames = delivered(&mut router, 1);
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames[0].frame.events,
        packet(1, &[(EV_MSC, MSC_SCAN, 91), (EV_KEY, 31, 1)]).events
    );
    assert_eq!(
        frames[1].frame.events,
        packet(1, &[(EV_MSC, MSC_SCAN, 91), (EV_KEY, 31, 0)]).events
    );
}

#[test]
fn reports_without_effects_keep_unknown_metadata_and_hybrid_data_intact() {
    for frame in [
        packet(
            1,
            &[
                (EV_MSC, MSC_SCAN, 1),
                (EV_MSC, MSC_SCAN, 2),
                (EV_KEY, 30, 1),
            ],
        ),
        packet(1, &[(EV_REL, 0, 12), (EV_MSC, 5, 91), (EV_KEY, 30, 1)]),
    ] {
        let mut router = model(Limits::default(), true);
        push(&mut router, frame.clone(), &[], &[]);
        assert_eq!(delivered(&mut router, 1)[0].frame, frame);
    }
}

#[test]
fn ambiguous_metadata_and_hybrid_cuts_fail_before_any_state_or_queue_mutation() {
    for (frame, event) in [
        (
            packet(
                1,
                &[
                    (EV_MSC, MSC_SCAN, 1),
                    (EV_MSC, MSC_SCAN, 2),
                    (EV_KEY, 30, 1),
                ],
            ),
            2,
        ),
        (packet(1, &[(EV_REL, 0, 12), (EV_KEY, 30, 1)]), 1),
    ] {
        let mut router = model(Limits::default(), true);
        assert_eq!(
            router.push_key_frame(frame, 1, &[claim(1, event)], &BTreeSet::new(), ms(1)),
            Err(Fault::UnsupportedProjection)
        );
        assert!(router.physical.neutral());
        assert!(router.keyboard.is_none());
        assert!(router.queue.is_empty());
        assert_eq!(router.next_ticket, 0);
    }
}

#[test]
fn projection_capacity_preflight_includes_inserted_report_boundaries() {
    for limits in [
        Limits {
            frames: 1,
            ..Limits::default()
        },
        Limits {
            events: 5,
            ..Limits::default()
        },
    ] {
        let mut router = model(limits, false);
        assert_eq!(
            router.push_key_frame(
                keys(1, &[(31, 1), (30, 1), (31, 0), (30, 0), (32, 1), (32, 0)]),
                1,
                &[claim(1, 1)],
                &BTreeSet::new(),
                ms(1)
            ),
            Err(Fault::Overflow)
        );
        assert!(router.queue.is_empty());
        assert!(router.physical.neutral());
        assert!(router.keyboard.is_none());
        assert_eq!(router.next_ticket, 0);
    }
}

#[test]
fn original_input_sequences_do_not_alias_fragment_output_sequences() {
    let mut router = model(Limits::default(), false);
    push(
        &mut router,
        keys(1, &[(31, 1), (30, 1), (31, 0), (30, 0)]),
        &[claim(1, 1)],
        &[],
    );
    let first = router.due(ms(1)).unwrap().unwrap().clone();
    router.delivered(first.ticket, ms(1)).unwrap();
    assert!(!router.prefix_delivered(1, true));
    let second = router.due(ms(1)).unwrap().unwrap().clone();
    assert_eq!(second.frame.sequence, 2);
    router.delivered(second.ticket, ms(1)).unwrap();
    assert!(router.prefix_delivered(1, true));
    push(&mut router, keys(2, &[(32, 1)]), &[], &[]);
    let next = router.due(ms(2)).unwrap().unwrap().clone();
    assert_eq!(next.frame.sequence, 3);
    assert_eq!(router.input_span(next.ticket).unwrap().sequence, 2);
    assert!(!router.prefix_delivered(2, true));
}

#[test]
fn recovery_releases_only_delivered_keys_and_retires_position_receipts() {
    let mut router = model(Limits::default(), false);
    push(
        &mut router,
        keys(1, &[(30, 1), (31, 1)]),
        &[claim(1, 0)],
        &[],
    );
    delivered(&mut router, 1);
    let pending = push(&mut router, keys(2, &[(32, 1)]), &[], &[]);
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
    assert!(router.input_span(pending.tickets[0]).is_none());
    assert!(!router.position_prefix_delivered(InputPosition::after_frame(2), true));
    assert!(router.recover().is_none());
}

#[test]
fn legacy_and_selective_ownership_modes_cannot_be_mixed() {
    let mut legacy = model(Limits::default(), false);
    legacy.push(keys(1, &[(30, 1)]), None, ms(1)).unwrap();
    assert_eq!(
        legacy.push_key_frame(keys(2, &[(30, 0)]), 2, &[], &BTreeSet::new(), ms(2)),
        Err(Fault::TooLate)
    );
    let mut selective = model(Limits::default(), false);
    push(&mut selective, keys(1, &[(30, 1)]), &[], &[]);
    assert_eq!(
        selective.push(keys(2, &[(30, 0)]), None, ms(2)),
        Err(Fault::InvalidCandidate)
    );
}

#[test]
fn stale_sources_and_invalid_claims_cannot_change_the_current_ledger() {
    let mut router = model(Limits::default(), false);
    let mut stale = keys(1, &[(30, 1)]);
    stale.source.generation = 2;
    assert_eq!(
        router.push_key_frame(stale, 1, &[], &BTreeSet::new(), ms(1)),
        Err(Fault::StaleSource)
    );
    assert!(!router.stopped);
    let bad = KeyClaim {
        authorization: 2,
        ..claim(1, 0)
    };
    assert_eq!(
        router.push_key_frame(keys(1, &[(30, 1)]), 1, &[bad], &BTreeSet::new(), ms(1)),
        Err(Fault::Unauthorized)
    );
    assert!(router.keyboard.is_none());
    assert!(router.physical.neutral());
}

#[test]
fn duplicate_claims_invalid_cuts_and_reused_owner_ids_are_rejected_atomically() {
    for (claims, cuts) in [
        (vec![claim(1, 0), claim(2, 0)], BTreeSet::new()),
        (vec![claim(1, 0)], BTreeSet::from([1])),
        (vec![claim(2, 0), claim(1, 1)], BTreeSet::new()),
    ] {
        let mut router = model(Limits::default(), false);
        let frame = if cuts.is_empty() {
            keys(1, &[(30, 1), (31, 1)])
        } else {
            keys(1, &[(30, 1)])
        };
        assert_eq!(
            router.push_key_frame(frame, 1, &claims, &cuts, ms(1)),
            Err(Fault::InvalidCandidate)
        );
        assert!(router.keyboard.is_none());
        assert!(router.queue.is_empty());
        assert!(router.physical.neutral());
    }
    let mut router = model(Limits::default(), false);
    push(
        &mut router,
        keys(1, &[(30, 1), (30, 0)]),
        &[claim(5, 0)],
        &[],
    );
    assert_eq!(
        router.push_key_frame(
            keys(2, &[(30, 1)]),
            2,
            &[claim(5, 0)],
            &BTreeSet::new(),
            ms(2)
        ),
        Err(Fault::InvalidCandidate)
    );
}

#[test]
fn watermark_and_output_sequence_exhaustion_never_wrap() {
    let mut router = model(Limits::default(), false);
    push(&mut router, keys(1, &[(30, 1)]), &[claim(1, 0)], &[]);
    assert_eq!(
        router.push_key_frame(keys(2, &[(30, 0)]), 1, &[], &BTreeSet::new(), ms(2)),
        Err(Fault::BackwardTime)
    );
    assert!(router.physical.keys.contains(&30));
    let mut router = model(Limits::default(), false);
    router.keyboard = Some(Ledger {
        output_sequence: u64::MAX,
        ..Default::default()
    });
    assert_eq!(
        router.push_key_frame(keys(1, &[(30, 1)]), 1, &[], &BTreeSet::new(), ms(1)),
        Err(Fault::Overflow)
    );
    assert_eq!(router.next_ticket, 0);
    assert!(router.queue.is_empty());
    assert!(router.physical.neutral());
}

#[test]
fn fragment_receipts_survive_backpressure_and_ignore_wrong_acknowledgements() {
    let mut router = model(Limits::default(), false);
    let result = push(
        &mut router,
        keys(1, &[(31, 1), (30, 1), (31, 0), (30, 0)]),
        &[claim(1, 1)],
        &[],
    );
    let first = router.due(ms(1)).unwrap().unwrap().clone();
    let span = router.input_span(first.ticket).unwrap();
    assert_eq!(router.due(ms(1)).unwrap(), Some(&first));
    assert_eq!(
        router.delivered(result.tickets[1], ms(1)),
        Err(Fault::WrongTicket)
    );
    assert_eq!(router.input_span(first.ticket), Some(span));
    assert!(!router.position_prefix_delivered(InputPosition { frame: 1, event: 1 }, false));
    router.delivered(first.ticket, ms(1)).unwrap();
    assert!(router.position_prefix_delivered(InputPosition { frame: 1, event: 1 }, false));
    assert_eq!(
        router.due(ms(1)).unwrap().unwrap().ticket,
        result.tickets[1]
    );
}

#[test]
fn sequence_exhaustion_inside_a_multifragment_plan_does_not_queue_a_prefix() {
    let mut router = model(Limits::default(), false);
    router.keyboard = Some(Ledger {
        output_sequence: u64::MAX - 1,
        ..Default::default()
    });
    assert_eq!(
        router.push_key_frame(
            keys(1, &[(31, 1), (30, 1), (31, 0), (30, 0)]),
            1,
            &[claim(1, 1)],
            &BTreeSet::new(),
            ms(1)
        ),
        Err(Fault::Overflow)
    );
    assert!(router.queue.is_empty());
    assert!(router.physical.neutral());
    assert_eq!(router.next_ticket, 0);
    assert_eq!(
        router.keyboard.as_ref().unwrap().output_sequence,
        u64::MAX - 1
    );
}

#[test]
fn claim_and_cut_counts_are_bounded_before_planning() {
    for excessive_claims in [false, true] {
        let mut router = model(Limits::default(), false);
        let claims = if excessive_claims {
            (0..5000)
                .map(|event| claim(event as u64 + 1, event))
                .collect()
        } else {
            vec![]
        };
        let cuts = if excessive_claims {
            BTreeSet::new()
        } else {
            (0..5000).collect()
        };
        assert_eq!(
            router.push_key_frame(keys(1, &[(30, 1)]), 1, &claims, &cuts, ms(1)),
            Err(Fault::InvalidCandidate)
        );
        assert!(router.queue.is_empty());
        assert!(router.keyboard.is_none());
        assert!(router.physical.neutral());
    }
}

#[test]
fn a_stale_legacy_call_cannot_stop_a_current_selective_source() {
    let mut router = model(Limits::default(), false);
    push(&mut router, keys(1, &[(30, 1)]), &[claim(1, 0)], &[]);
    let mut old = keys(2, &[(30, 0)]);
    old.source.generation = 0;
    assert_eq!(router.push(old, None, ms(2)), Err(Fault::StaleSource));
    assert!(!router.stopped);
    assert_eq!(
        push(&mut router, keys(2, &[(30, 0)]), &[], &[])
            .consumed
            .len(),
        1
    );
}

#[test]
fn draining_duplicate_edges_does_not_split_unrelated_typing_without_an_effect() {
    for (duplicate, explicit) in [(0, false), (1, false), (0, true), (1, true)] {
        let mut router = model(Limits::default(), false);
        let mut first = vec![(30, 1)];
        if duplicate == 0 {
            first.push((30, 0));
        }
        push(&mut router, keys(1, &first), &[claim(1, 0)], &[]);
        let result = push(
            &mut router,
            keys(2, &[(31, 1), (30, duplicate), (31, 0)]),
            &[],
            if explicit { &[1] } else { &[] },
        );
        assert!(result.consumed.is_empty());
        let frames = delivered(&mut router, 2);
        assert_eq!(frames.len(), if explicit { 2 } else { 1 });
        assert_eq!(
            frames
                .iter()
                .flat_map(|frame| edges(&frame.frame))
                .collect::<Vec<_>>(),
            [(31, 1), (31, 0)]
        );
        if !explicit {
            assert_eq!(frames[0].frame, keys(2, &[(31, 1), (31, 0)]));
        }
    }
}
