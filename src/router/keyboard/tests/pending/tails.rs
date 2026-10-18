use super::hybrid::{mixed, touch};
use super::*;
use crate::{
    input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, AbsoluteSlot,
        AbsoluteState, Normalizer,
    },
    router::{PendingTail, TailDecision, TailDisposition},
};

fn down(sequence: u64, at: u64) -> Frame {
    mixed(
        sequence,
        at,
        &[
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 7),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
            (EV_KEY, 0x14a, 1),
        ],
    )
}
fn up(sequence: u64, at: u64) -> Frame {
    mixed(
        sequence,
        at,
        &[(EV_ABS, ABS_MT_TRACKING_ID, -1), (EV_KEY, 0x14a, 0)],
    )
}
fn retain(router: &mut Router, frame: Frame, keys: &[PendingKey], deadline: Option<u64>) {
    let at = frame.at;
    let watermark = frame.sequence;
    router
        .stage_owned_frame(
            frame,
            watermark,
            keys,
            deadline.map(|deadline| PendingTail {
                authorization: 1,
                deadline: ms(deadline),
                continuation: None,
            }),
            &BTreeSet::new(),
            at,
        )
        .unwrap();
}
fn tail(root: InputPosition, owner: Option<u64>) -> TailDecision {
    TailDecision {
        authorization: 1,
        position: root,
        owner,
    }
}
fn settle_tail(router: &mut Router, root: InputPosition, owner: Option<u64>, at: u64) {
    router
        .decide_input_scopes(&[], &[tail(root, owner)], ms(at))
        .unwrap();
    router.advance(ms(at)).unwrap();
}

#[test]
fn consumption_drains_the_whole_contact_cohort_and_repairs_the_next_delta_group() {
    let mut router = touch();
    let mut first = down(1, 1);
    first.events.insert(
        0,
        RawEvent {
            kind: EV_KEY,
            code: 31,
            value: 1,
        },
    );
    retain(&mut router, first, &[], Some(100));
    let root = router.tail_scope().unwrap();
    assert_eq!(root, position(1, 6));
    assert_eq!(edges(&delivered(&mut router, 1)[0].frame), [(31, 1)]);
    retain(
        &mut router,
        mixed(2, 11, &[(EV_KEY, 31, 0), (EV_ABS, ABS_MT_POSITION_Y, 250)]),
        &[],
        None,
    );
    assert!(router.due(ms(11)).unwrap().is_none());
    settle_tail(&mut router, root, Some(9), 50);
    let raw = delivered(&mut router, 50);
    assert_eq!(raw.len(), 1);
    assert_eq!(edges(&raw[0].frame), [(31, 0)]);
    assert_eq!(raw[0].due, ms(50));
    assert_eq!(
        router.tail_disposition(root),
        Some(TailDisposition::Consumed { owner: 9 })
    );
    retain(&mut router, up(3, 60), &[], None);
    assert!(delivered(&mut router, 60).is_empty());
    assert!(router.tail_scope().is_none());
    retain(
        &mut router,
        mixed(
            4,
            70,
            &[(EV_ABS, ABS_MT_TRACKING_ID, 8), (EV_KEY, 0x14a, 1)],
        ),
        &[],
        None,
    );
    let ordinary = delivered(&mut router, 70);
    assert_eq!(ordinary.len(), 1);
    assert_eq!(ordinary[0].due, ms(70));
    let normalized = Normalizer::new(&router.device)
        .process(ordinary[0].frame.clone())
        .unwrap();
    assert_eq!(normalized.contacts.len(), 1);
    assert_eq!(
        (normalized.contacts[0].x, normalized.contacts[0].y),
        (100, 250)
    );
}

#[test]
fn an_owned_cohort_suppresses_empty_reports_without_consuming_independent_key_atoms() {
    let mut router = touch();
    retain(&mut router, down(1, 1), &[], Some(100));
    let root = router.tail_scope().unwrap();
    retain(&mut router, mixed(2, 11, &[]), &[], None);
    retain(&mut router, mixed(3, 21, &[(EV_KEY, 31, 1)]), &[], None);
    retain(&mut router, mixed(4, 31, &[(EV_KEY, 31, 0)]), &[], None);
    settle_tail(&mut router, root, Some(9), 50);
    let first = delivered(&mut router, 50);
    assert_eq!(first.len(), 1);
    assert_eq!(edges(&first[0].frame), [(31, 1)]);
    assert_eq!(first[0].due, ms(50));
    let last = delivered(&mut router, 60);
    assert_eq!(last.len(), 1);
    assert_eq!(edges(&last[0].frame), [(31, 0)]);
    assert_eq!(last[0].due, ms(60));
    retain(&mut router, up(5, 70), &[], None);
    assert!(delivered(&mut router, 70).is_empty());
}

#[test]
fn a_new_physical_group_cannot_inherit_an_unanswered_older_tail_claim() {
    let mut router = touch();
    retain(&mut router, down(1, 1), &[], Some(100));
    let original = router.tail_scope().unwrap();
    retain(&mut router, up(2, 11), &[], None);
    retain(&mut router, down(3, 21), &[], Some(100));
    let current = router.tail_scope().unwrap();
    assert_ne!(current, original);
    assert_eq!(
        router.tail_disposition(original),
        Some(TailDisposition::Ordinary)
    );
    assert_eq!(
        router.decide_input_scopes(&[], &[tail(original, Some(9))], ms(22)),
        Err(Fault::TooLate)
    );
    settle_tail(&mut router, current, Some(10), 22);
    let first = delivered(&mut router, 22);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].due, ms(21));
    assert_eq!(first[0].frame.events, down(1, 1).events);
    let last = delivered(&mut router, 31);
    assert_eq!(last.len(), 1);
    assert_eq!(last[0].frame.events, up(2, 11).events);
}

#[test]
fn separate_contact_groups_require_explicit_bounded_continuation_to_share_a_root() {
    let mut router = touch();
    retain(&mut router, down(1, 1), &[], Some(100));
    let root = router.tail_scope().unwrap();
    retain(&mut router, up(2, 11), &[], None);
    router
        .stage_owned_frame(
            down(3, 21),
            3,
            &[],
            Some(PendingTail {
                authorization: 1,
                deadline: ms(100),
                continuation: Some(root),
            }),
            &BTreeSet::new(),
            ms(21),
        )
        .unwrap();
    assert_eq!(router.tail_scope(), Some(root));
    retain(&mut router, up(4, 31), &[], None);
    settle_tail(&mut router, root, Some(9), 50);
    assert!(delivered(&mut router, 50).is_empty());
    assert!(router.tail_scope().is_none());
    assert!(router.physical_neutral());
}

#[test]
fn continuation_cannot_change_the_original_deadline_or_rebind_a_different_root() {
    for (wanted, deadline) in [(position(99, 0), 100), (position(1, 5), 200)] {
        let mut router = touch();
        retain(&mut router, down(1, 1), &[], Some(100));
        retain(&mut router, up(2, 11), &[], None);
        assert_eq!(
            router.stage_owned_frame(
                down(3, 21),
                3,
                &[],
                Some(PendingTail {
                    authorization: 1,
                    deadline: ms(deadline),
                    continuation: Some(wanted),
                }),
                &BTreeSet::new(),
                ms(21)
            ),
            Err(Fault::TooLate)
        );
        assert_eq!(router.last_sequence, Some(2));
        assert!(router.recover().unwrap().neutral_events.is_empty());
    }
}

#[test]
fn complete_tail_fallback_is_byte_exact_and_preserves_inter_report_cadence() {
    let mut router = touch();
    let original = [
        down(1, 1),
        mixed(2, 11, &[(EV_ABS, ABS_MT_POSITION_Y, 250)]),
        up(3, 21),
    ];
    for (index, frame) in original.iter().enumerate() {
        retain(&mut router, frame.clone(), &[], (index == 0).then_some(100));
    }
    assert!(router.due(ms(99)).unwrap().is_none());
    for (index, at) in [100, 110, 120].into_iter().enumerate() {
        let frames = delivered(&mut router, at);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].due, ms(at));
        assert_eq!(frames[0].frame.events, original[index].events);
    }
    assert_eq!(router.delivery_offset(), Duration::ZERO);
    assert!(router.tail_scope().is_none());
}

#[test]
fn invalid_tail_decisions_do_not_partially_consume_valid_key_scopes() {
    let mut router = touch();
    let mut frame = down(1, 1);
    frame.events.insert(
        0,
        RawEvent {
            kind: EV_KEY,
            code: 30,
            value: 1,
        },
    );
    retain(&mut router, frame, &[waiting(0, 100)], Some(100));
    let root = router.tail_scope().unwrap();
    assert_eq!(
        router.decide_input_scopes(
            &[decision(1, 0, 30, Some(9))],
            &[tail(position(1, 1), Some(9))],
            ms(50)
        ),
        Err(Fault::TooLate)
    );
    assert_eq!(
        router.key_disposition(30, position(1, 0)),
        Some(KeyDisposition::Pending { deadline: ms(100) })
    );
    assert_eq!(
        router.tail_disposition(root),
        Some(TailDisposition::Pending { deadline: ms(100) })
    );
    router
        .decide_input_scopes(
            &[decision(1, 0, 30, Some(9))],
            &[tail(root, Some(9))],
            ms(50),
        )
        .unwrap();
    assert!(delivered(&mut router, 50).is_empty());
    let mut last = up(2, 60);
    last.events.insert(
        0,
        RawEvent {
            kind: EV_KEY,
            code: 30,
            value: 0,
        },
    );
    retain(&mut router, last, &[], None);
    assert!(delivered(&mut router, 60).is_empty());
    assert!(router.physical_neutral());
}

#[test]
fn ordinary_contact_state_cannot_be_reclaimed_before_the_group_is_neutral() {
    let mut router = touch();
    let first = down(1, 1);
    retain(&mut router, first.clone(), &[], None);
    let root = router.tail_scope().unwrap();
    assert_eq!(delivered(&mut router, 1)[0].frame.events, first.events);
    assert_eq!(
        router.decide_input_scopes(&[], &[tail(root, Some(9))], ms(10)),
        Err(Fault::TooLate)
    );
    let last = up(2, 11);
    retain(&mut router, last.clone(), &[], None);
    assert_eq!(delivered(&mut router, 11)[0].frame.events, last.events);
    assert!(router.tail_scope().is_none());
}

#[test]
fn tail_deadlines_are_immutable_and_late_wall_time_cannot_hide_behind_input_time() {
    let mut router = touch();
    retain(&mut router, down(1, 1), &[], Some(100));
    let root = router.tail_scope().unwrap();
    assert_eq!(
        router.validate_input_decisions(&[], &[tail(root, Some(9))], ms(2), ms(101)),
        Err(Fault::TooLate)
    );
    assert_eq!(
        router.validate_input_decisions(&[], &[tail(root, Some(9))], ms(2), ms(1)),
        Err(Fault::BackwardTime)
    );
    let result = router.stage_owned_frame(
        mixed(2, 11, &[(EV_ABS, ABS_MT_POSITION_Y, 250)]),
        2,
        &[],
        Some(PendingTail {
            authorization: 1,
            deadline: ms(200),
            continuation: None,
        }),
        &BTreeSet::new(),
        ms(11),
    );
    assert_eq!(result, Err(Fault::TooLate));
    assert_eq!(router.last_sequence, Some(1));
}

#[test]
fn duplicate_zero_owner_and_foreign_authorization_decisions_are_refused() {
    let mut router = touch();
    retain(&mut router, down(1, 1), &[], Some(100));
    let root = router.tail_scope().unwrap();
    for decisions in [
        vec![tail(root, Some(0))],
        vec![tail(root, None), tail(root, Some(9))],
    ] {
        assert_eq!(
            router.decide_input_scopes(&[], &decisions, ms(10)),
            Err(Fault::InvalidCandidate)
        );
        assert_eq!(
            router.tail_disposition(root),
            Some(TailDisposition::Pending { deadline: ms(100) })
        );
    }
    let mut foreign = tail(root, Some(9));
    foreign.authorization += 1;
    assert_eq!(
        router.decide_input_scopes(&[], &[foreign], ms(10)),
        Err(Fault::Unauthorized)
    );
    let recovery = router.recover().unwrap();
    assert!(recovery.neutral_events.is_empty());
    assert!(recovery.destroy_virtual_and_release_grab);
    assert!(router.recover().is_none());
}

#[test]
fn retained_absolute_versions_are_isolated_and_charged_before_allocation() {
    let mut router = touch();
    let initial = AbsoluteState {
        values: BTreeMap::from([(ABS_MT_SLOT, 0)]),
        slots: BTreeMap::from([(
            0,
            AbsoluteSlot {
                values: BTreeMap::from([(ABS_MT_POSITION_X, 10), (ABS_MT_POSITION_Y, 20)]),
            },
        )]),
    };
    router.seed_absolute(&initial).unwrap();
    retain(
        &mut router,
        mixed(1, 1, &[(EV_ABS, ABS_MT_TRACKING_ID, 7), (EV_KEY, 0x14a, 1)]),
        &[],
        Some(100),
    );
    let root = router.tail_scope().unwrap();
    retain(
        &mut router,
        mixed(2, 11, &[(EV_ABS, ABS_MT_POSITION_Y, 250)]),
        &[],
        None,
    );
    assert_eq!(
        router.physical_absolute.slots[&0].values[&ABS_MT_POSITION_Y],
        250
    );
    settle_tail(&mut router, root, None, 50);
    let mut receiver = Normalizer::new(&router.device);
    receiver.seed_absolute(&router.device, initial).unwrap();
    let first = receiver
        .process(delivered(&mut router, 50).remove(0).frame)
        .unwrap();
    assert_eq!((first.contacts[0].x, first.contacts[0].y), (10, 20));
    let later = receiver
        .process(delivered(&mut router, 60).remove(0).frame)
        .unwrap();
    assert_eq!((later.contacts[0].x, later.contacts[0].y), (10, 250));
    let device = router.device.clone();
    let mut bounded = prepared(
        Limits {
            events: 8,
            ..Limits::default()
        },
        device,
    );
    bounded
        .seed_absolute(&AbsoluteState {
            values: BTreeMap::from([(ABS_MT_SLOT, 0)]),
            slots: BTreeMap::from([(
                0,
                AbsoluteSlot {
                    values: BTreeMap::from([(ABS_MT_POSITION_X, 10), (ABS_MT_POSITION_Y, 20)]),
                },
            )]),
        })
        .unwrap();
    assert_eq!(
        bounded.stage_owned_frame(
            down(1, 1),
            1,
            &[],
            Some(PendingTail {
                authorization: 1,
                deadline: ms(100),
                continuation: None,
            }),
            &BTreeSet::new(),
            ms(1)
        ),
        Err(Fault::Overflow)
    );
    assert!(bounded.pending_keyboard.is_none());
    assert_eq!(Arc::strong_count(&bounded.physical_absolute), 1);
}

#[test]
fn consumed_slot_cursor_and_axes_are_restored_only_for_the_next_used_slot() {
    let mut router = touch();
    let initial = AbsoluteState {
        values: BTreeMap::from([(ABS_MT_SLOT, 0)]),
        slots: BTreeMap::from([
            (
                0,
                AbsoluteSlot {
                    values: BTreeMap::from([(ABS_MT_POSITION_X, 10), (ABS_MT_POSITION_Y, 20)]),
                },
            ),
            (
                1,
                AbsoluteSlot {
                    values: BTreeMap::from([(ABS_MT_POSITION_X, 30), (ABS_MT_POSITION_Y, 40)]),
                },
            ),
        ]),
    };
    router.seed_absolute(&initial).unwrap();
    let mut first = down(1, 1);
    first.events[0].value = 1;
    retain(&mut router, first, &[], Some(100));
    let root = router.tail_scope().unwrap();
    settle_tail(&mut router, root, Some(9), 50);
    retain(&mut router, up(2, 60), &[], None);
    assert!(delivered(&mut router, 60).is_empty());
    retain(
        &mut router,
        mixed(
            3,
            70,
            &[(EV_ABS, ABS_MT_TRACKING_ID, 8), (EV_KEY, 0x14a, 1)],
        ),
        &[],
        None,
    );
    let mut receiver = Normalizer::new(&router.device);
    receiver.seed_absolute(&router.device, initial).unwrap();
    let resumed = receiver
        .process(delivered(&mut router, 70).remove(0).frame)
        .unwrap();
    assert_eq!(resumed.contacts[0].id.slot, 1);
    assert_eq!((resumed.contacts[0].x, resumed.contacts[0].y), (100, 200));
    retain(
        &mut router,
        mixed(
            4,
            80,
            &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, 9)],
        ),
        &[],
        None,
    );
    let joined = receiver
        .process(delivered(&mut router, 80).remove(0).frame)
        .unwrap();
    assert_eq!(joined.contacts.len(), 2);
    let untouched = joined
        .contacts
        .iter()
        .find(|contact| contact.id.slot == 0)
        .unwrap();
    assert_eq!((untouched.x, untouched.y), (10, 20));
}

#[test]
fn unrelated_held_keys_do_not_extend_a_finished_tail_scope() {
    let mut router = touch();
    let mut first = down(1, 1);
    first.events.insert(
        0,
        RawEvent {
            kind: EV_KEY,
            code: 30,
            value: 1,
        },
    );
    retain(&mut router, first, &[], Some(100));
    let original = router.tail_scope().unwrap();
    assert_eq!(edges(&delivered(&mut router, 1)[0].frame), [(30, 1)]);
    settle_tail(&mut router, original, Some(9), 50);
    retain(&mut router, up(2, 60), &[], None);
    assert!(delivered(&mut router, 60).is_empty());
    assert!(router.tail_scope().is_none());
    assert!(!router.physical_neutral());
    retain(&mut router, mixed(3, 70, &[(EV_REL, 0, 8)]), &[], Some(100));
    assert_ne!(router.tail_scope().unwrap(), original);
}

#[test]
fn wheel_companions_are_consumed_or_forwarded_together_without_tail_reuse() {
    let mut device = model(Limits::default(), true).device;
    device.capabilities.insert(EV_REL, vec![0, 8, 11]);
    let mut router = prepared(Limits::default(), device);
    retain(
        &mut router,
        mixed(1, 1, &[(EV_REL, 8, 1), (EV_REL, 11, 120)]),
        &[],
        Some(100),
    );
    let root = router.tail_scope().unwrap();
    settle_tail(&mut router, root, Some(9), 50);
    assert!(delivered(&mut router, 50).is_empty());
    assert!(router.tail_scope().is_none());
    let ordinary = mixed(2, 60, &[(EV_REL, 8, -1), (EV_REL, 11, -120)]);
    retain(&mut router, ordinary.clone(), &[], None);
    assert_eq!(delivered(&mut router, 60)[0].frame.events, ordinary.events);
}

#[test]
fn a_tail_cannot_cross_an_original_position_barrier_or_release_without_ack() {
    let mut router = touch();
    let mut first = down(1, 1);
    first.events.insert(
        0,
        RawEvent {
            kind: EV_KEY,
            code: 31,
            value: 1,
        },
    );
    retain(&mut router, first, &[], Some(100));
    let root = router.tail_scope().unwrap();
    let prefix = router.due(ms(1)).unwrap().unwrap().clone();
    assert!(!router.position_prefix_delivered(root, false));
    router.delivered(prefix.ticket, ms(1)).unwrap();
    assert!(router.position_prefix_delivered(root, false));
    router
        .key_journal_barrier(Some(root), Duration::ZERO)
        .unwrap();
    settle_tail(&mut router, root, None, 50);
    assert!(router.due(ms(50)).unwrap().is_none());
    router.key_journal_barrier(None, Duration::ZERO).unwrap();
    let tail = router.due(ms(50)).unwrap().unwrap().clone();
    let span = router.input_span(tail.ticket).unwrap();
    assert_eq!((span.first, span.last), (root, root));
    assert!(!router.position_prefix_delivered(root, true));
    router.delivered(tail.ticket, ms(50)).unwrap();
    assert!(router.position_prefix_delivered(root, true));
}

#[test]
fn empty_cohort_admission_and_unproven_switch_ownership_never_suppress_input() {
    let mut router = model(Limits::default(), false);
    assert_eq!(
        router.stage_owned_frame(
            frame(1, 1, &[(30, 1)]),
            1,
            &[],
            Some(PendingTail {
                authorization: 1,
                deadline: ms(100),
                continuation: None,
            }),
            &BTreeSet::new(),
            ms(1)
        ),
        Err(Fault::InvalidCandidate)
    );
    let mut device = model(Limits::default(), false).device;
    device.capabilities = BTreeMap::from([(EV_SW, vec![0])]);
    let original = mixed(1, 1, &[(EV_SW, 0, 1)]);
    let mut guarded = prepared(Limits::default(), device.clone());
    assert_eq!(
        guarded.stage_owned_frame(
            original.clone(),
            1,
            &[],
            Some(PendingTail {
                authorization: 1,
                deadline: ms(100),
                continuation: None,
            }),
            &BTreeSet::new(),
            ms(1)
        ),
        Err(Fault::UnsupportedProjection)
    );
    let mut ordinary = prepared(Limits::default(), device);
    retain(&mut ordinary, original.clone(), &[], None);
    assert_eq!(delivered(&mut ordinary, 1)[0].frame.events, original.events);
}
