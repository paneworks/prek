use super::hybrid::{mixed, touch};
use super::*;
use crate::{
    input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, AbsoluteSlot,
        AbsoluteState, Axis, Normalizer, TailDomain,
    },
    router::{DomainDecision, DomainRoot, PendingDomain},
};

fn prepared_domains(limits: Limits) -> Router {
    let mut device = touch().device;
    device
        .capabilities
        .insert(EV_REL, vec![0, 1, 2, 6, 8, 11, 12]);
    device.capabilities.insert(EV_SW, vec![0]);
    device.capabilities.insert(4, vec![4, 5]);
    device.capabilities.get_mut(&EV_ABS).unwrap().extend([0, 1]);
    device
        .capabilities
        .get_mut(&EV_ABS)
        .unwrap()
        .sort_unstable();
    for code in [0, 1] {
        device.axes.insert(
            code,
            Axis {
                minimum: 0,
                maximum: 1000,
                resolution: 0,
                fuzz: 0,
                flat: 0,
            },
        );
    }
    prepared(limits, device)
}
fn admission(domain: TailDomain, deadline: u64) -> PendingDomain {
    PendingDomain {
        authorization: 1,
        domain,
        deadline: ms(deadline),
        continuation: None,
    }
}
fn retain(
    router: &mut Router,
    frame: Frame,
    keys: &[PendingKey],
    pending: &[PendingDomain],
) -> Vec<DomainRoot> {
    let at = frame.at;
    let watermark = frame.sequence;
    router
        .stage_domain_frame(frame, watermark, keys, pending, &BTreeSet::new(), at)
        .unwrap()
}
fn choose(scope: DomainRoot, owner: Option<u64>) -> DomainDecision {
    DomainDecision {
        authorization: 1,
        scope,
        owner,
    }
}
fn settle(router: &mut Router, keys: &[KeyDecision], domains: &[DomainDecision], at: u64) {
    router.decide_domain_scopes(keys, domains, ms(at)).unwrap();
    router.advance(ms(at)).unwrap();
}
fn delivered_clocked(router: &mut Router, input_at: u64, now: u64) -> Vec<Scheduled> {
    router.advance(ms(input_at)).unwrap();
    let mut output = Vec::new();
    while let Some(frame) = router.due_after_input(ms(now)).unwrap().cloned() {
        router.delivered_after_input(frame.ticket, ms(now)).unwrap();
        output.push(frame);
    }
    output
}
fn contacts() -> Vec<(u16, u16, i32)> {
    vec![
        (EV_ABS, ABS_MT_SLOT, 0),
        (EV_ABS, ABS_MT_TRACKING_ID, 7),
        (EV_ABS, ABS_MT_POSITION_X, 100),
        (EV_ABS, ABS_MT_POSITION_Y, 200),
        (EV_KEY, 0x14a, 1),
    ]
}

#[test]
fn a_relative_claim_leaves_wheel_touch_absolute_switch_and_opaque_data_ordinary() {
    let mut router = prepared_domains(Limits::default());
    let mut data = vec![
        (EV_REL, 0, 4),
        (EV_KEY, 31, 1),
        (EV_REL, 1, 8),
        (EV_REL, 8, 1),
        (EV_REL, 11, 120),
        (EV_REL, 6, 1),
        (EV_REL, 12, 120),
        (EV_ABS, 0, 20),
        (EV_SW, 0, 1),
        (4, 5, 123),
    ];
    data.extend(contacts());
    let original = mixed(1, 1, &data);
    let roots = retain(
        &mut router,
        original.clone(),
        &[],
        &[admission(TailDomain::Relative, 100)],
    );
    assert_eq!(roots.len(), 7);
    let root = router.domain_scope(TailDomain::Relative).unwrap();
    assert_eq!(root.position, position(1, data.len()));
    assert!(roots.contains(&root));
    assert_eq!(edges(&delivered(&mut router, 1)[0].frame), [(31, 1)]);
    settle(&mut router, &[], &[choose(root, Some(9))], 50);
    let output = delivered(&mut router, 50);
    assert_eq!(output.len(), 1);
    let expected: Vec<_> = original
        .events
        .iter()
        .filter(|event| {
            !(event.kind == EV_REL && matches!(event.code, 0 | 1)
                || event.kind == EV_KEY && event.code == 31)
        })
        .copied()
        .collect();
    assert_eq!(output[0].frame.events, expected);
    assert_eq!(output[0].due, ms(50));
    assert!(router.domain_disposition(root).is_none());
    assert!(router.prefix_delivered(1, true));
}

#[test]
fn independent_wheel_axes_keep_their_legacy_and_high_resolution_companions_together() {
    let mut router = prepared_domains(Limits::default());
    let frame = mixed(
        1,
        1,
        &[
            (EV_REL, 8, 1),
            (EV_REL, 0, 4),
            (EV_REL, 11, 120),
            (EV_REL, 6, -1),
            (EV_REL, 12, -120),
            (EV_REL, 1, 8),
            (EV_REL, 2, 3),
        ],
    );
    retain(
        &mut router,
        frame,
        &[],
        &[
            admission(TailDomain::Wheel, 100),
            admission(TailDomain::Relative, 100),
        ],
    );
    let vertical = router.domain_scope(TailDomain::Wheel).unwrap();
    let xy = router.domain_scope(TailDomain::Relative).unwrap();
    assert_eq!(vertical.position, xy.position);
    settle(
        &mut router,
        &[],
        &[choose(vertical, Some(8)), choose(xy, Some(9))],
        50,
    );
    let output = delivered(&mut router, 50);
    assert_eq!(output.len(), 1);
    assert_eq!(
        output[0].frame.events,
        mixed(1, 1, &[(EV_REL, 6, -1), (EV_REL, 12, -120), (EV_REL, 2, 3)]).events
    );
}

#[test]
fn all_key_and_domain_choices_are_validated_before_any_ownership_changes() {
    let mut router = prepared_domains(Limits::default());
    retain(
        &mut router,
        mixed(1, 1, &[(EV_KEY, 30, 1), (EV_REL, 0, 4), (EV_REL, 8, 1)]),
        &[waiting(0, 100)],
        &[
            admission(TailDomain::Relative, 100),
            admission(TailDomain::Wheel, 100),
        ],
    );
    let relative = router.domain_scope(TailDomain::Relative).unwrap();
    let wheel = router.domain_scope(TailDomain::Wheel).unwrap();
    let key = decision(1, 0, 30, Some(8));
    let mut wrong = wheel;
    wrong.position.event -= 1;
    for invalid in [
        vec![choose(relative, Some(8)), choose(wrong, Some(8))],
        vec![choose(relative, Some(8)), choose(relative, Some(8))],
        vec![choose(relative, Some(8)), choose(wheel, Some(0))],
    ] {
        assert!(
            router
                .decide_domain_scopes(&[key], &invalid, ms(50))
                .is_err()
        );
        assert_eq!(
            router.key_disposition(30, position(1, 0)),
            Some(KeyDisposition::Pending { deadline: ms(100) })
        );
        assert_eq!(
            router.domain_disposition(relative),
            Some(KeyDisposition::Pending { deadline: ms(100) })
        );
        assert_eq!(
            router.domain_disposition(wheel),
            Some(KeyDisposition::Pending { deadline: ms(100) })
        );
    }
    settle(
        &mut router,
        &[key],
        &[choose(relative, Some(8)), choose(wheel, Some(9))],
        50,
    );
    assert!(delivered(&mut router, 50).is_empty());
}

#[test]
fn an_unmatched_multi_domain_report_falls_back_byte_exact_with_original_cadence() {
    let mut router = prepared_domains(Limits::default());
    let first = mixed(
        1,
        1,
        &[
            (EV_REL, 8, 1),
            (EV_KEY, 30, 1),
            (EV_REL, 0, 4),
            (EV_KEY, 30, 0),
        ],
    );
    let second = mixed(2, 11, &[(EV_REL, 0, 8), (EV_REL, 8, -1)]);
    retain(
        &mut router,
        first.clone(),
        &[waiting(1, 100)],
        &[
            admission(TailDomain::Relative, 100),
            admission(TailDomain::Wheel, 100),
        ],
    );
    retain(&mut router, second.clone(), &[], &[]);
    assert!(router.due(ms(99)).unwrap().is_none());
    let output = delivered(&mut router, 100);
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].frame.events, first.events);
    assert_eq!(output[0].due, ms(100));
    let output = delivered(&mut router, 110);
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].frame.events, second.events);
    assert_eq!(output[0].due, ms(110));
}

#[test]
fn pending_domains_share_the_original_syn_barrier_but_keep_independent_deadlines() {
    let mut router = prepared_domains(Limits::default());
    retain(
        &mut router,
        mixed(1, 1, &[(EV_KEY, 31, 1), (EV_REL, 0, 4), (EV_REL, 8, 1)]),
        &[],
        &[
            admission(TailDomain::Relative, 100),
            admission(TailDomain::Wheel, 200),
        ],
    );
    let relative = router.domain_scope(TailDomain::Relative).unwrap();
    let wheel = router.domain_scope(TailDomain::Wheel).unwrap();
    let prefix = delivered(&mut router, 1);
    assert_eq!(prefix.len(), 1);
    assert_eq!(edges(&prefix[0].frame), [(31, 1)]);
    assert!(router.position_prefix_delivered(relative.position, false));
    assert!(!router.position_prefix_delivered(relative.position, true));
    settle(&mut router, &[], &[choose(relative, Some(9))], 50);
    assert_eq!(
        router.domain_disposition(wheel),
        Some(KeyDisposition::Pending { deadline: ms(200) })
    );
    assert!(delivered(&mut router, 100).is_empty());
    let output = delivered(&mut router, 200);
    assert_eq!(output.len(), 1);
    assert_eq!(
        output[0].frame.events,
        mixed(1, 1, &[(EV_REL, 8, 1)]).events
    );
    assert_eq!(output[0].due, ms(200));
}

#[test]
fn consumed_contact_state_drains_without_consuming_keys_motion_or_wheel() {
    let mut router = prepared_domains(Limits::default());
    let mut first = contacts();
    first.extend([(EV_KEY, 31, 1), (EV_REL, 0, 4), (EV_REL, 8, 1)]);
    retain(
        &mut router,
        mixed(1, 1, &first),
        &[],
        &[admission(TailDomain::Touch, 100)],
    );
    let root = router.domain_scope(TailDomain::Touch).unwrap();
    assert_eq!(edges(&delivered(&mut router, 1)[0].frame), [(31, 1)]);
    settle(&mut router, &[], &[choose(root, Some(9))], 50);
    assert_eq!(
        delivered(&mut router, 50)[0].frame.events,
        mixed(1, 1, &[(EV_REL, 0, 4), (EV_REL, 8, 1)]).events
    );
    retain(
        &mut router,
        mixed(
            2,
            60,
            &[
                (EV_ABS, ABS_MT_POSITION_Y, 250),
                (EV_REL, 1, 8),
                (EV_KEY, 31, 0),
            ],
        ),
        &[],
        &[],
    );
    let output = delivered_clocked(&mut router, 60, 109);
    assert_eq!(output.len(), 2);
    assert!(output.iter().all(|frame| frame.due == ms(109)));
    assert_eq!(edges(&output[0].frame), [(31, 0)]);
    assert_eq!(
        output[1].frame.events,
        mixed(2, 60, &[(EV_REL, 1, 8)]).events
    );
    assert_eq!(router.domain_scope(TailDomain::Touch), Some(root));
    let bindings = retain(&mut router, mixed(3, 70, &[]), &[], &[]);
    assert!(bindings.is_empty());
    assert!(delivered_clocked(&mut router, 70, 119).is_empty());
    retain(
        &mut router,
        mixed(
            4,
            80,
            &[(EV_ABS, ABS_MT_TRACKING_ID, -1), (EV_KEY, 0x14a, 0)],
        ),
        &[],
        &[],
    );
    assert!(delivered_clocked(&mut router, 80, 129).is_empty());
    assert!(router.domain_scope(TailDomain::Touch).is_none());
    retain(
        &mut router,
        mixed(
            5,
            140,
            &[(EV_ABS, ABS_MT_TRACKING_ID, 8), (EV_KEY, 0x14a, 1)],
        ),
        &[],
        &[],
    );
    let ordinary = delivered(&mut router, 140);
    assert_eq!(ordinary.len(), 1);
    let normalized = Normalizer::new(&router.device)
        .process(ordinary[0].frame.clone())
        .unwrap();
    assert_eq!(
        (normalized.contacts[0].x, normalized.contacts[0].y),
        (100, 250)
    );
}

#[test]
fn scalar_absolute_repair_is_private_until_an_ordinary_scalar_domain_is_addressed() {
    let mut router = prepared_domains(Limits::default());
    let initial = AbsoluteState {
        values: BTreeMap::from([(0, 10), (1, 20), (ABS_MT_SLOT, 0)]),
        slots: BTreeMap::from([(
            0,
            AbsoluteSlot {
                values: BTreeMap::from([(ABS_MT_POSITION_X, 100), (ABS_MT_POSITION_Y, 200)]),
            },
        )]),
    };
    router.seed_absolute(&initial).unwrap();
    let mut receiver = Normalizer::new(&router.device);
    receiver.seed_absolute(&router.device, initial).unwrap();
    retain(
        &mut router,
        mixed(1, 1, &[(EV_ABS, 0, 500)]),
        &[],
        &[admission(TailDomain::Absolute, 100)],
    );
    let root = router.domain_scope(TailDomain::Absolute).unwrap();
    settle(&mut router, &[], &[choose(root, Some(9))], 50);
    assert!(delivered(&mut router, 50).is_empty());
    retain(&mut router, mixed(2, 60, &contacts()), &[], &[]);
    let output = delivered(&mut router, 60);
    assert_eq!(output.len(), 1);
    assert!(
        !output[0]
            .frame
            .events
            .iter()
            .any(|event| event.kind == EV_ABS && matches!(event.code, 0 | 1))
    );
    assert_eq!(
        receiver
            .process(output[0].frame.clone())
            .unwrap()
            .contacts
            .len(),
        1
    );
    retain(&mut router, mixed(3, 70, &[(EV_ABS, 1, 30)]), &[], &[]);
    let output = delivered(&mut router, 70);
    assert_eq!(output.len(), 1);
    assert_eq!(
        output[0].frame.events,
        mixed(3, 70, &[(EV_ABS, 0, 500), (EV_ABS, 1, 30)]).events
    );
    assert_eq!(
        receiver
            .process(output[0].frame.clone())
            .unwrap()
            .contacts
            .len(),
        1
    );
}

#[test]
fn ordinary_scalar_updates_do_not_reveal_owned_contact_coordinates_or_tracking_ids() {
    let mut router = prepared_domains(Limits::default());
    let mut first = contacts();
    first.push((EV_ABS, 0, 500));
    retain(
        &mut router,
        mixed(1, 1, &first),
        &[],
        &[admission(TailDomain::Touch, 100)],
    );
    let root = router.domain_scope(TailDomain::Touch).unwrap();
    settle(&mut router, &[], &[choose(root, Some(9))], 50);
    let output = delivered(&mut router, 50);
    assert_eq!(output.len(), 1);
    assert_eq!(
        output[0].frame.events,
        mixed(1, 1, &[(EV_ABS, 0, 500)]).events
    );
    retain(
        &mut router,
        mixed(2, 60, &[(EV_ABS, ABS_MT_POSITION_X, 600), (EV_ABS, 1, 30)]),
        &[],
        &[],
    );
    let output = delivered_clocked(&mut router, 60, 109);
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].due, ms(109));
    assert_eq!(
        output[0].frame.events,
        mixed(2, 60, &[(EV_ABS, 1, 30)]).events
    );
    retain(
        &mut router,
        mixed(
            3,
            70,
            &[(EV_ABS, ABS_MT_TRACKING_ID, -1), (EV_KEY, 0x14a, 0)],
        ),
        &[],
        &[],
    );
    assert!(delivered_clocked(&mut router, 70, 119).is_empty());
    retain(
        &mut router,
        mixed(
            4,
            130,
            &[(EV_ABS, ABS_MT_TRACKING_ID, 8), (EV_KEY, 0x14a, 1)],
        ),
        &[],
        &[],
    );
    let output = delivered(&mut router, 130);
    let normalized = Normalizer::new(&router.device)
        .process(output[0].frame.clone())
        .unwrap();
    assert_eq!(
        (normalized.contacts[0].x, normalized.contacts[0].y),
        (600, 200)
    );
}

#[test]
fn continuation_is_bound_to_the_exact_domain_as_well_as_position_and_deadline() {
    let mut router = prepared_domains(Limits::default());
    retain(
        &mut router,
        mixed(1, 1, &[(EV_REL, 0, 4), (EV_REL, 8, 1)]),
        &[],
        &[
            admission(TailDomain::Relative, 100),
            admission(TailDomain::Wheel, 100),
        ],
    );
    let relative = router.domain_scope(TailDomain::Relative).unwrap();
    let wheel = router.domain_scope(TailDomain::Wheel).unwrap();
    assert_eq!(relative.position, wheel.position);
    let mut continue_relative = admission(TailDomain::Relative, 100);
    continue_relative.continuation = Some(relative);
    let next = retain(
        &mut router,
        mixed(2, 11, &[(EV_REL, 0, 8), (EV_REL, 8, 1)]),
        &[],
        &[continue_relative],
    );
    assert!(next.contains(&relative));
    assert_eq!(router.domain_scope(TailDomain::Relative), Some(relative));
    assert_eq!(
        router.domain_disposition(wheel),
        Some(KeyDisposition::Ordinary)
    );
    settle(&mut router, &[], &[choose(relative, Some(9))], 50);
    let first = delivered(&mut router, 50);
    assert_eq!(first[0].frame.events, mixed(1, 1, &[(EV_REL, 8, 1)]).events);
    let second = delivered(&mut router, 60);
    assert_eq!(
        second[0].frame.events,
        mixed(2, 11, &[(EV_REL, 8, 1)]).events
    );

    for wrong in [
        PendingDomain {
            continuation: Some(relative),
            ..admission(TailDomain::Wheel, 100)
        },
        PendingDomain {
            continuation: Some(relative),
            ..admission(TailDomain::Relative, 101)
        },
    ] {
        let mut router = prepared_domains(Limits::default());
        retain(
            &mut router,
            mixed(1, 1, &[(EV_REL, 0, 4), (EV_REL, 8, 1)]),
            &[],
            &[
                admission(TailDomain::Relative, 100),
                admission(TailDomain::Wheel, 100),
            ],
        );
        assert!(
            router
                .stage_domain_frame(
                    mixed(2, 11, &[(EV_REL, 0, 8), (EV_REL, 8, 1)]),
                    2,
                    &[],
                    &[wrong],
                    &BTreeSet::new(),
                    ms(11)
                )
                .is_err()
        );
        assert_eq!(router.recover().unwrap().neutral_events, Vec::new());
    }
}

#[test]
fn ordinary_contact_and_non_key_domains_cannot_be_reclaimed_after_passthrough() {
    let mut router = prepared_domains(Limits::default());
    retain(&mut router, mixed(1, 1, &contacts()), &[], &[]);
    delivered(&mut router, 1);
    let root = router.domain_scope(TailDomain::Touch).unwrap();
    assert_eq!(
        router.domain_disposition(root),
        Some(KeyDisposition::Ordinary)
    );
    assert!(
        router
            .decide_domain_scopes(&[], &[choose(root, Some(9))], ms(2))
            .is_err()
    );
    assert_eq!(
        router.domain_disposition(root),
        Some(KeyDisposition::Ordinary)
    );
    retain(
        &mut router,
        mixed(2, 11, &[(EV_ABS, ABS_MT_POSITION_Y, 250)]),
        &[],
        &[],
    );
    assert_eq!(delivered(&mut router, 11).len(), 1);
}

#[test]
fn switch_and_opaque_ownership_are_refused_without_rejecting_other_domains_on_the_device() {
    for domain in [TailDomain::Switch, TailDomain::Opaque] {
        let mut router = prepared_domains(Limits::default());
        let frame = mixed(1, 1, &[(EV_REL, 0, 4), (EV_SW, 0, 1), (4, 5, 123)]);
        assert_eq!(
            router.stage_domain_frame(
                frame,
                1,
                &[],
                &[admission(domain, 100)],
                &BTreeSet::new(),
                ms(1)
            ),
            Err(Fault::UnsupportedProjection)
        );
        assert!(router.recover().unwrap().neutral_events.is_empty());
    }
    let mut router = prepared_domains(Limits::default());
    retain(
        &mut router,
        mixed(1, 1, &[(EV_REL, 0, 4), (EV_SW, 0, 1), (4, 5, 123)]),
        &[],
        &[admission(TailDomain::Relative, 100)],
    );
    let root = router.domain_scope(TailDomain::Relative).unwrap();
    settle(&mut router, &[], &[choose(root, Some(9))], 50);
    assert_eq!(
        delivered(&mut router, 50)[0].frame.events,
        mixed(1, 1, &[(EV_SW, 0, 1), (4, 5, 123)]).events
    );
}

#[test]
fn an_ordinary_switch_interval_keeps_its_cadence_when_another_domain_is_consumed() {
    let mut router = prepared_domains(Limits::default());
    retain(
        &mut router,
        mixed(1, 1, &[(EV_REL, 0, 4), (EV_SW, 0, 1)]),
        &[],
        &[admission(TailDomain::Relative, 100)],
    );
    let root = router.domain_scope(TailDomain::Relative).unwrap();
    settle(&mut router, &[], &[choose(root, Some(9))], 50);
    let on = delivered(&mut router, 50);
    assert_eq!(on.len(), 1);
    assert_eq!(on[0].frame.events, mixed(1, 1, &[(EV_SW, 0, 1)]).events);
    assert_eq!(on[0].due, ms(50));
    retain(&mut router, mixed(2, 60, &[(EV_SW, 0, 0)]), &[], &[]);
    assert!(delivered(&mut router, 60).is_empty());
    let off = delivered_clocked(&mut router, 60, 109);
    assert_eq!(off.len(), 1);
    assert_eq!(off[0].frame.events, mixed(2, 60, &[(EV_SW, 0, 0)]).events);
    assert_eq!(off[0].due - on[0].due, ms(59));
}

#[test]
fn metadata_capacity_is_charged_before_committing_pending_key_or_domain_state() {
    for enough in [false, true] {
        let mut router = prepared_domains(Limits {
            events: if enough { 35 } else { 34 },
            ..Limits::default()
        });
        let result = router.stage_domain_frame(
            mixed(1, 1, &[(EV_KEY, 30, 1), (EV_REL, 0, 4)]),
            1,
            &[waiting(0, 100)],
            &[admission(TailDomain::Relative, 100)],
            &BTreeSet::new(),
            ms(1),
        );
        if enough {
            assert!(result.is_ok());
            assert_eq!(router.storage(), (1, 35));
            assert_eq!(
                router.domain_disposition(result.unwrap()[0]),
                Some(KeyDisposition::Pending { deadline: ms(100) })
            );
        } else {
            assert_eq!(result, Err(Fault::Overflow));
            assert!(router.recover().unwrap().neutral_events.is_empty());
            assert!(router.pending_key_position().is_none());
        }
    }
}

#[test]
fn a_source_cannot_switch_whole_tail_and_independent_domain_interpretations_mid_stream() {
    for start_split in [false, true] {
        let mut router = prepared_domains(Limits::default());
        let first = mixed(1, 1, &[(EV_REL, 0, 4)]);
        if start_split {
            retain(&mut router, first, &[], &[]);
        } else {
            router
                .stage_owned_frame(first, 1, &[], None, &BTreeSet::new(), ms(1))
                .unwrap();
        }
        delivered(&mut router, 1);
        let next = mixed(2, 11, &[(EV_REL, 0, 8)]);
        let result = if start_split {
            router.stage_owned_frame(next, 2, &[], None, &BTreeSet::new(), ms(11))
        } else {
            router
                .stage_domain_frame(next, 2, &[], &[], &BTreeSet::new(), ms(11))
                .map(|_| ())
        };
        assert_eq!(result, Err(Fault::UnsupportedProjection));
    }
}

#[test]
fn expired_wall_time_cannot_hide_behind_a_drained_input_clock() {
    let mut router = prepared_domains(Limits::default());
    retain(
        &mut router,
        mixed(1, 1, &[(EV_KEY, 30, 1), (EV_REL, 0, 4)]),
        &[waiting(0, 100)],
        &[admission(TailDomain::Relative, 100)],
    );
    let root = router.domain_scope(TailDomain::Relative).unwrap();
    let key = decision(1, 0, 30, Some(9));
    assert_eq!(
        router.decide_domains_clocked(&[key], &[choose(root, Some(9))], ms(1), ms(101)),
        Err(Fault::TooLate)
    );
    assert_eq!(
        router.key_disposition(30, position(1, 0)),
        Some(KeyDisposition::Pending { deadline: ms(100) })
    );
    assert_eq!(
        router.domain_disposition(root),
        Some(KeyDisposition::Pending { deadline: ms(100) })
    );
    router
        .decide_domains_clocked(
            &[KeyDecision { owner: None, ..key }],
            &[choose(root, None)],
            ms(1),
            ms(101),
        )
        .unwrap();
    retain(
        &mut router,
        mixed(2, 11, &[(EV_KEY, 30, 0), (EV_REL, 0, 8)]),
        &[],
        &[],
    );
    assert_eq!(delivered(&mut router, 100)[0].due, ms(100));
    assert_eq!(delivered(&mut router, 110)[0].due, ms(110));
}

#[test]
fn ordinary_domain_roots_are_retired_after_delivery_in_long_source_lifetimes() {
    let mut router = prepared_domains(Limits {
        events: 35,
        ..Limits::default()
    });
    for sequence in 1..=1000 {
        let roots = retain(
            &mut router,
            mixed(sequence, sequence, &[(EV_REL, 0, 4)]),
            &[],
            &[],
        );
        assert_eq!(roots.len(), 1);
        assert_eq!(delivered(&mut router, sequence).len(), 1);
        assert_eq!(router.storage(), (0, 0));
        assert!(router.domain_disposition(roots[0]).is_none());
    }
}

#[test]
fn the_key_scan_companion_is_not_misclassified_as_owned_or_ordinary_tail_data() {
    let mut router = prepared_domains(Limits::default());
    let frame = mixed(
        1,
        1,
        &[(4, 4, 123), (EV_KEY, 30, 1), (EV_REL, 0, 4), (EV_REL, 8, 1)],
    );
    retain(
        &mut router,
        frame,
        &[waiting(1, 100)],
        &[admission(TailDomain::Relative, 100)],
    );
    let relative = router.domain_scope(TailDomain::Relative).unwrap();
    settle(
        &mut router,
        &[decision(1, 1, 30, Some(9))],
        &[choose(relative, Some(9))],
        50,
    );
    assert_eq!(
        delivered(&mut router, 50)[0].frame.events,
        mixed(1, 1, &[(EV_REL, 8, 1)]).events
    );
}

#[test]
fn source_authorization_clock_and_declaration_bounds_are_checked_before_reinterpretation() {
    for foreign_authorization in [false, true] {
        let mut router = prepared_domains(Limits::default());
        retain(
            &mut router,
            mixed(1, 1, &[(EV_REL, 0, 4)]),
            &[],
            &[admission(TailDomain::Relative, 100)],
        );
        let root = router.domain_scope(TailDomain::Relative).unwrap();
        let mut stale = mixed(2, 11, &[(EV_REL, 0, 8)]);
        stale.source.generation += 1;
        assert_eq!(
            router.stage_domain_frame(stale, 2, &[], &[], &BTreeSet::new(), ms(11)),
            Err(Fault::StaleSource)
        );
        assert_eq!(router.domain_scope(TailDomain::Relative), Some(root));
        let mut decision = choose(root, Some(9));
        if foreign_authorization {
            decision.authorization += 1;
        }
        let error = router.decide_domain_scopes(
            &[],
            &[decision],
            ms(if foreign_authorization { 50 } else { 0 }),
        );
        assert_eq!(
            error,
            Err(if foreign_authorization {
                Fault::Unauthorized
            } else {
                Fault::BackwardTime
            })
        );
        assert!(router.recover().unwrap().neutral_events.is_empty());
    }
    let mut router = prepared_domains(Limits::default());
    assert_eq!(
        router.stage_domain_frame(
            mixed(1, 1, &[(EV_REL, 0, 4)]),
            1,
            &[],
            &[admission(TailDomain::Relative, 100); 9],
            &BTreeSet::new(),
            ms(1)
        ),
        Err(Fault::InvalidCandidate)
    );
    assert!(router.recover().unwrap().neutral_events.is_empty());
}

fn ordered(router: &mut Router, frame: Frame, pending: &[PendingDomain]) -> Vec<DomainRoot> {
    let at = frame.at;
    let watermark = frame.sequence;
    router
        .stage_ordered_domain_frame(frame, watermark, &[], pending, &BTreeSet::new(), at)
        .unwrap()
}

#[test]
fn coupled_scalar_absolute_shares_its_contact_cohort_fate_in_ordered_admission() {
    for consume in [false, true] {
        let mut router = prepared_domains(Limits::default());
        let mut first = contacts();
        first.push((EV_ABS, 0, 100));
        let roots = ordered(
            &mut router,
            mixed(1, 1, &first),
            &[
                admission(TailDomain::Touch, 100),
                admission(TailDomain::Absolute, 100),
            ],
        );
        let touch = router.domain_scope(TailDomain::Touch).unwrap();
        let scalar = roots
            .iter()
            .copied()
            .find(|root| root.domain == TailDomain::Absolute)
            .unwrap();
        assert_eq!(
            router.domain_disposition(scalar),
            Some(KeyDisposition::Pending { deadline: ms(100) })
        );
        assert!(delivered(&mut router, 1).is_empty());
        settle(&mut router, &[], &[choose(scalar, Some(9))], 20);
        assert_eq!(
            router.domain_disposition(scalar),
            Some(KeyDisposition::Pending { deadline: ms(100) }),
            "scalar data cannot be owned apart from its contacts"
        );
        assert!(delivered(&mut router, 20).is_empty());
        router
            .decide_domain_scopes(&[], &[choose(touch, consume.then_some(9))], ms(50))
            .unwrap();
        assert_eq!(
            router.domain_disposition(scalar),
            router.domain_disposition(touch)
        );
        router.advance(ms(50)).unwrap();
        let output = delivered(&mut router, 50);
        if consume {
            assert!(output.is_empty());
        } else {
            assert_eq!(output.len(), 1);
            assert_eq!(output[0].frame.events, mixed(1, 1, &first).events);
        }
        let late = ordered(&mut router, mixed(2, 60, &[(EV_ABS, 1, 300)]), &[]);
        let late = late
            .iter()
            .copied()
            .find(|root| root.domain == TailDomain::Absolute)
            .unwrap();
        assert_eq!(
            router.domain_disposition(late),
            router.domain_disposition(touch)
        );
        assert_eq!(
            delivered_clocked(&mut router, 60, 109).len(),
            usize::from(!consume)
        );
        ordered(
            &mut router,
            mixed(
                3,
                70,
                &[
                    (EV_ABS, ABS_MT_TRACKING_ID, -1),
                    (EV_KEY, 0x14a, 0),
                    (EV_ABS, 0, 110),
                ],
            ),
            &[],
        );
        assert_eq!(
            delivered_clocked(&mut router, 70, 119).len(),
            usize::from(!consume)
        );
        assert!(router.domain_scope(TailDomain::Touch).is_none());
        ordered(&mut router, mixed(4, 130, &[(EV_ABS, 0, 150)]), &[]);
        let output = delivered(&mut router, 130);
        assert_eq!(output.len(), 1, "consume={consume}");
        assert!(output[0].frame.events.contains(&RawEvent {
            kind: EV_ABS,
            code: 0,
            value: 150
        }));
    }
}

#[test]
fn uncoupled_transport_staging_keeps_scalar_and_contact_fates_independent() {
    let mut router = prepared_domains(Limits::default());
    let mut first = contacts();
    first.push((EV_ABS, 0, 100));
    let roots = retain(
        &mut router,
        mixed(1, 1, &first),
        &[],
        &[
            admission(TailDomain::Touch, 100),
            admission(TailDomain::Absolute, 100),
        ],
    );
    let scalar = roots
        .iter()
        .copied()
        .find(|root| root.domain == TailDomain::Absolute)
        .unwrap();
    settle(&mut router, &[], &[choose(scalar, Some(9))], 20);
    assert_eq!(
        router.domain_disposition(scalar),
        Some(KeyDisposition::Consumed { owner: 9 })
    );
    let touch = router.domain_scope(TailDomain::Touch).unwrap();
    assert!(matches!(
        router.domain_disposition(touch),
        Some(KeyDisposition::Pending { .. })
    ));
}
