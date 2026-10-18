use super::*;
use crate::input::{ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, Axis};
use crate::router::keyboard::pending::Journal;

fn mixed(sequence: u64, at: u64, events: &[(u16, u16, i32)]) -> Frame {
    Frame {
        at: ms(at),
        ..packet(sequence, events)
    }
}
fn touch() -> Router {
    let mut device = model(Limits::default(), true).device.clone();
    device.properties = vec![1];
    device.capabilities.get_mut(&EV_KEY).unwrap().push(0x14a);
    device.capabilities.insert(
        EV_ABS,
        vec![
            ABS_MT_SLOT,
            ABS_MT_TRACKING_ID,
            ABS_MT_POSITION_X,
            ABS_MT_POSITION_Y,
        ],
    );
    device
        .capabilities
        .get_mut(&EV_ABS)
        .unwrap()
        .sort_unstable();
    device.axes = [
        (ABS_MT_SLOT, 1),
        (ABS_MT_TRACKING_ID, 65535),
        (ABS_MT_POSITION_X, 1000),
        (ABS_MT_POSITION_Y, 1000),
    ]
    .into_iter()
    .map(|(code, maximum)| {
        (
            code,
            Axis {
                minimum: 0,
                maximum,
                resolution: 0,
                fuzz: 0,
                flat: 0,
            },
        )
    })
    .collect();
    prepared(Limits::default(), device)
}

#[test]
fn pending_consumption_keeps_relative_data_in_one_indivisible_report_tail() {
    let mut router = model(Limits::default(), true);
    stage(
        &mut router,
        mixed(
            1,
            1,
            &[
                (EV_REL, 0, 7),
                (EV_KEY, 31, 1),
                (EV_KEY, 30, 1),
                (EV_REL, 0, 9),
                (EV_KEY, 31, 0),
                (EV_KEY, 30, 0),
            ],
        ),
        &[waiting(2, 100)],
    );
    let first = delivered(&mut router, 1);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].frame.events, packet(1, &[(EV_KEY, 31, 1)]).events);
    settle(&mut router, &[decision(1, 2, 30, Some(8))], 50);
    let rest = delivered(&mut router, 50);
    assert_eq!(rest.len(), 2);
    assert_eq!(rest[0].frame.events, packet(1, &[(EV_KEY, 31, 0)]).events);
    assert_eq!(
        rest[1].frame.events,
        packet(1, &[(EV_REL, 0, 7), (EV_REL, 0, 9)]).events
    );
    assert!(router.prefix_delivered(1, true));
}

#[test]
fn untouched_hybrid_reports_retain_every_raw_field_and_original_order() {
    let mut router = model(Limits::default(), true);
    let original = mixed(
        1,
        1,
        &[
            (EV_REL, 0, 7),
            (EV_KEY, 30, 1),
            (EV_REL, 0, -9),
            (EV_KEY, 30, 0),
        ],
    );
    stage(&mut router, original.clone(), &[]);
    let output = delivered(&mut router, 1);
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].frame, original);
}

#[test]
fn the_non_key_tail_cannot_cross_its_original_syn_barrier() {
    let mut router = model(Limits::default(), true);
    let original = mixed(1, 1, &[(EV_REL, 0, 7), (EV_KEY, 30, 1), (EV_KEY, 30, 0)]);
    stage(&mut router, original, &[]);
    router
        .key_journal_barrier(Some(position(1, 3)), Duration::ZERO)
        .unwrap();
    let prefix = delivered(&mut router, 1);
    assert_eq!(prefix.len(), 1);
    assert_eq!(
        prefix[0].frame.events,
        packet(1, &[(EV_KEY, 30, 1), (EV_KEY, 30, 0)]).events
    );
    assert!(router.due(ms(1)).unwrap().is_none());
    assert!(!router.position_prefix_delivered(position(1, 3), true));
    router.key_journal_barrier(None, Duration::ZERO).unwrap();
    let tail = router.due(ms(1)).unwrap().cloned().unwrap();
    let span = router.input_span(tail.ticket).unwrap();
    assert_eq!(span.first, position(1, 3));
    assert_eq!(span.last, position(1, 3));
    assert_eq!(tail.frame.events, packet(1, &[(EV_REL, 0, 7)]).events);
    router.delivered(tail.ticket, ms(1)).unwrap();
    assert!(router.prefix_delivered(1, true));
}

#[test]
fn hybrid_fallback_preserves_whole_reports_and_the_original_motion_cadence() {
    let mut router = model(Limits::default(), true);
    let down = mixed(1, 1, &[(EV_REL, 0, 7), (EV_KEY, 30, 1)]);
    let up = mixed(2, 11, &[(EV_REL, 0, -9), (EV_KEY, 30, 0)]);
    stage(&mut router, down.clone(), &[waiting(1, 100)]);
    stage(&mut router, up.clone(), &[]);
    assert!(router.due(ms(99)).unwrap().is_none());
    let first = delivered(&mut router, 100);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].frame.events, down.events);
    assert!(router.due(ms(109)).unwrap().is_none());
    let last = delivered(&mut router, 110);
    assert_eq!(last.len(), 1);
    assert_eq!(last[0].frame.events, up.events);
}

#[test]
fn mt_slot_coordinates_and_contact_flags_commit_together_after_key_projection() {
    let mut router = touch();
    stage(
        &mut router,
        mixed(
            1,
            1,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 7),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_KEY, 31, 1),
                (EV_KEY, 30, 1),
                (EV_ABS, ABS_MT_POSITION_Y, 200),
                (EV_KEY, 0x14a, 1),
                (EV_KEY, 31, 0),
                (EV_KEY, 30, 0),
            ],
        ),
        &[waiting(4, 100)],
    );
    let prefix = delivered(&mut router, 1);
    assert_eq!(prefix.len(), 1);
    assert_eq!(prefix[0].frame.events, packet(1, &[(EV_KEY, 31, 1)]).events);
    settle(&mut router, &[decision(1, 4, 30, Some(8))], 50);
    let rest = delivered(&mut router, 50);
    assert_eq!(rest.len(), 2);
    assert_eq!(rest[0].frame.events, packet(1, &[(EV_KEY, 31, 0)]).events);
    assert_eq!(
        rest[1].frame.events,
        packet(
            1,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 7),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_ABS, ABS_MT_POSITION_Y, 200),
                (EV_KEY, 0x14a, 1)
            ]
        )
        .events
    );
    stage(
        &mut router,
        mixed(
            2,
            60,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                (EV_KEY, 0x14a, 0),
            ],
        ),
        &[],
    );
    assert!(router.due(ms(108)).unwrap().is_none());
    assert_eq!(delivered(&mut router, 109).len(), 1);
    assert!(router.delivered.neutral());
}

#[test]
fn contact_markers_cannot_be_claimed_as_independent_keyboard_presses() {
    let mut journal = Journal::default();
    let input = mixed(1, 1, &[(EV_KEY, 0x14a, 1)]);
    assert_eq!(
        journal.stage(
            input,
            1,
            &[waiting(0, 100)],
            &BTreeSet::new(),
            Limits::default().offset
        ),
        Err(Fault::UnsupportedProjection)
    );
    assert_eq!(journal.storage(), (0, 0));
}

#[test]
fn both_mt_slots_stay_together_even_when_keys_split_the_original_report() {
    let mut router = touch();
    let data = [
        (EV_ABS, ABS_MT_SLOT, 0),
        (EV_ABS, ABS_MT_TRACKING_ID, 7),
        (EV_ABS, ABS_MT_POSITION_X, 100),
        (EV_KEY, 31, 1),
        (EV_ABS, ABS_MT_POSITION_Y, 200),
        (EV_KEY, 30, 1),
        (EV_ABS, ABS_MT_SLOT, 1),
        (EV_ABS, ABS_MT_TRACKING_ID, 8),
        (EV_KEY, 31, 0),
        (EV_ABS, ABS_MT_POSITION_X, 400),
        (EV_ABS, ABS_MT_POSITION_Y, 500),
        (EV_KEY, 0x14a, 1),
        (EV_KEY, 30, 0),
    ];
    stage(&mut router, mixed(1, 1, &data), &[waiting(5, 100)]);
    let prefix = delivered(&mut router, 1);
    assert_eq!(prefix.len(), 1);
    assert!(
        prefix[0]
            .frame
            .events
            .iter()
            .all(|event| event.kind != EV_ABS)
    );
    settle(&mut router, &[decision(1, 5, 30, Some(8))], 50);
    let rest = delivered(&mut router, 50);
    assert_eq!(rest.len(), 2);
    assert_eq!(rest[0].frame.events, packet(1, &[(EV_KEY, 31, 0)]).events);
    let tail: Vec<_> = data
        .into_iter()
        .filter(|&(kind, code, _)| kind == EV_ABS || code == 0x14a)
        .collect();
    assert_eq!(rest[1].frame.events, packet(1, &tail).events);
    stage(
        &mut router,
        mixed(
            2,
            60,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                (EV_KEY, 0x14a, 0),
            ],
        ),
        &[],
    );
    assert_eq!(delivered(&mut router, 109).len(), 1);
    assert!(router.delivered.neutral());
}

#[test]
fn ambiguous_scan_metadata_never_rebinds_across_a_hybrid_data_tail() {
    let mut journal = Journal::default();
    let input = mixed(
        1,
        1,
        &[(EV_MSC, MSC_SCAN, 30), (EV_REL, 0, 4), (EV_KEY, 30, 1)],
    );
    assert_eq!(
        journal.stage(
            input,
            1,
            &[waiting(2, 100)],
            &BTreeSet::new(),
            Limits::default().offset
        ),
        Err(Fault::UnsupportedProjection)
    );
    assert_eq!(journal.storage(), (0, 0));
}
