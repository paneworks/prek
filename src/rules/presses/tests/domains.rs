use super::*;
use crate::{
    config::Millis,
    input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, Axis, EV_ABS,
        EV_KEY, EV_REL, EV_SW, EV_SYN, Frame, RawEvent, SYN_REPORT,
    },
    recognition::{Definition, InputKind, Phase, Proposal, Ticket},
};

pub(super) fn ms(at: u64) -> Duration {
    Duration::from_millis(at)
}

pub(super) fn hybrid(inputs: &[InputKind], continuous: bool) -> Engine {
    hybrid_budget(inputs, continuous, MAX_BYTES)
}
pub(super) fn hybrid_budget(inputs: &[InputKind], continuous: bool, limit: usize) -> Engine {
    let mut engine = configured(
        Trigger::Custom {
            recognizer: Definition {
                name: "domains".into(),
                selectors: BTreeSet::from(["keys".into()]),
                inputs: inputs.iter().copied().collect(),
                reply_within: Millis(100),
                continuous,
            },
            timeout: Millis(200),
            wake: Some(Millis(50)),
        },
        limit,
    );
    let mut device = engine.devices[&1].device.clone();
    engine
        .push(Record::Remove {
            source: device.source.clone(),
            at: ms(0),
        })
        .unwrap();
    device.source.generation += 1;
    device.capabilities.insert(EV_KEY, vec![30, 31, 42, 0x14a]);
    device
        .capabilities
        .insert(EV_REL, vec![0, 1, 2, 6, 8, 11, 12]);
    device.capabilities.insert(EV_SW, vec![0]);
    device.axes = [
        (0, 1000),
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
    device
        .capabilities
        .insert(EV_ABS, device.axes.keys().copied().collect());
    let geometry = crate::gestures::Output {
        name: "hybrid".into(),
        pixel_width: 1000.0,
        pixel_height: 1000.0,
        scale: 1.0,
        origin: crate::gestures::Point { x: 0.0, y: 0.0 },
        transform: crate::gestures::Transform::Normal,
    };
    let mut mapping = crate::gestures::Mapping::from_device(&device).unwrap();
    mapping.generation = 2;
    mapping.output = Some(geometry.clone());
    let mut snapshot = engine.authority.fresh(ms(0)).unwrap().clone();
    snapshot.revision += 1;
    snapshot.outputs = vec![crate::context::OutputState {
        geometry,
        geometry_generation: mapping.generation,
        active_workspace: Some(1),
        screen: crate::config::Screen::On,
        keyboard_known: true,
        keyboard: None,
    }];
    engine.authority.update(snapshot, ms(0)).unwrap();
    let source = device.source.clone();
    engine
        .push(Record::Device {
            device,
            keys: BTreeSet::new(),
            contacts: Vec::new(),
        })
        .unwrap();
    engine
        .push(Record::Mapping {
            source,
            at: ms(0),
            mapping,
        })
        .unwrap();
    engine
}

pub(super) fn raw(engine: &mut Engine, sequence: u64, at: u64, events: &[(u16, u16, i32)]) {
    engine
        .push(Record::Frame {
            frame: Frame {
                source: engine.devices[&1].device.source.clone(),
                sequence,
                at: ms(at),
                events: events
                    .iter()
                    .map(|&(kind, code, value)| RawEvent { kind, code, value })
                    .chain([RawEvent {
                        kind: EV_SYN,
                        code: SYN_REPORT,
                        value: 0,
                    }])
                    .collect(),
            },
        })
        .unwrap();
}

fn down(engine: &mut Engine) {
    raw(
        engine,
        1,
        10,
        &[
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 7),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
            (EV_KEY, 0x14a, 1),
            (EV_KEY, 30, 1),
            (EV_REL, 0, 4),
            (EV_REL, 8, 1),
            (EV_ABS, 0, 20),
            (EV_SW, 0, 1),
        ],
    );
}

pub(super) fn request(engine: &mut Engine, at: u64) -> Ticket {
    let mut requests = engine.recognition_requests(ms(at)).unwrap();
    assert_eq!(requests.len(), 1, "{:?}", engine.take_events());
    requests.pop().unwrap()
}

fn begin(engine: &mut Engine, ticket: &Ticket, at: u64) -> io::Result<()> {
    engine.recognition_response(
        Proposal {
            fence: ticket.fence,
            ticket: ticket.id,
            phase: Phase::Begin,
            data: serde_json::json!({}),
        },
        ms(at),
    )
}

#[test]
fn non_touch_custom_input_families_do_not_inherit_contact_evidence() {
    for inputs in [
        vec![InputKind::Key, InputKind::Relative],
        vec![InputKind::Key, InputKind::Wheel],
        vec![InputKind::Relative],
        vec![InputKind::Wheel],
        vec![InputKind::Absolute],
        vec![InputKind::Switch],
    ] {
        let mut engine = hybrid(&inputs, false);
        down(&mut engine);
        let ticket = request(&mut engine, 10);
        let candidate = &engine.workers[0].candidates[ticket.input.source()];
        assert!(candidate.contacts.is_empty(), "{inputs:?}");
        assert_eq!(candidate.peak_contacts, 0, "{inputs:?}");
        let Recognizer::Custom { closed, .. } = &engine.workers[0].recognizer else {
            unreachable!()
        };
        assert_eq!(
            closed.contains(ticket.input.source()),
            !inputs.contains(&InputKind::Key),
            "{inputs:?}"
        );
    }
}

#[test]
fn a_key_relative_group_closes_when_its_key_releases_not_when_touch_releases() {
    let mut engine = hybrid(&[InputKind::Key, InputKind::Relative], true);
    down(&mut engine);
    let ticket = request(&mut engine, 10);
    raw(&mut engine, 2, 20, &[(EV_KEY, 30, 0)]);
    assert!(begin(&mut engine, &ticket, 21).is_err());
    assert!(engine.take_route_outcomes().is_empty());
}

#[test]
fn a_touch_group_closes_when_its_contacts_release_not_when_an_unrelated_key_releases() {
    let mut engine = hybrid(&[InputKind::Touch], true);
    down(&mut engine);
    let ticket = request(&mut engine, 10);
    raw(
        &mut engine,
        2,
        20,
        &[(EV_ABS, ABS_MT_TRACKING_ID, -1), (EV_KEY, 0x14a, 0)],
    );
    assert!(begin(&mut engine, &ticket, 21).is_err());
    assert!(engine.take_route_outcomes().is_empty());
}

#[test]
fn mixed_key_touch_groups_remain_active_until_both_declared_families_are_neutral() {
    for release_key in [false, true] {
        let mut engine = hybrid(&[InputKind::Key, InputKind::Touch], true);
        down(&mut engine);
        let ticket = request(&mut engine, 10);
        if release_key {
            raw(&mut engine, 2, 20, &[(EV_KEY, 30, 0)]);
        } else {
            raw(
                &mut engine,
                2,
                20,
                &[(EV_ABS, ABS_MT_TRACKING_ID, -1), (EV_KEY, 0x14a, 0)],
            );
        }
        begin(&mut engine, &ticket, 21).unwrap();
    }
}

#[test]
fn a_restarted_key_relative_recognizer_does_not_wait_for_undeclared_touch() {
    let mut engine = hybrid(&[InputKind::Key, InputKind::Relative], true);
    down(&mut engine);
    let original = request(&mut engine, 10);
    engine.recognition_ready(false, ms(11)).unwrap();
    raw(&mut engine, 2, 20, &[(EV_KEY, 30, 0)]);
    engine.recognition_ready(true, ms(21)).unwrap();
    raw(&mut engine, 3, 30, &[(EV_KEY, 31, 1)]);
    assert_ne!(request(&mut engine, 30).candidate, original.candidate);
}

#[test]
fn a_restarted_touch_recognizer_does_not_wait_for_an_undeclared_key() {
    let mut engine = hybrid(&[InputKind::Touch], true);
    down(&mut engine);
    let original = request(&mut engine, 10);
    engine.recognition_ready(false, ms(11)).unwrap();
    raw(
        &mut engine,
        2,
        20,
        &[(EV_ABS, ABS_MT_TRACKING_ID, -1), (EV_KEY, 0x14a, 0)],
    );
    engine.recognition_ready(true, ms(21)).unwrap();
    raw(
        &mut engine,
        3,
        30,
        &[
            (EV_ABS, ABS_MT_TRACKING_ID, 8),
            (EV_ABS, ABS_MT_POSITION_X, 200),
            (EV_ABS, ABS_MT_POSITION_Y, 300),
            (EV_KEY, 0x14a, 1),
        ],
    );
    assert_ne!(request(&mut engine, 30).candidate, original.candidate);
}

#[test]
fn later_contacts_do_not_join_a_key_relative_candidates_frozen_scope() {
    let mut engine = hybrid(&[InputKind::Key, InputKind::Relative], true);
    raw(&mut engine, 1, 10, &[(EV_KEY, 30, 1), (EV_REL, 0, 4)]);
    let original = request(&mut engine, 10);
    raw(
        &mut engine,
        2,
        20,
        &[
            (EV_ABS, ABS_MT_TRACKING_ID, 7),
            (EV_ABS, ABS_MT_POSITION_X, 200),
            (EV_ABS, ABS_MT_POSITION_Y, 300),
            (EV_KEY, 0x14a, 1),
            (EV_REL, 0, 8),
        ],
    );
    let current = &engine.workers[0].candidates[original.input.source()];
    assert_eq!(current.id, original.candidate);
    assert!(current.contacts.is_empty());
    assert_eq!(current.peak_contacts, 0);
}

#[test]
fn explicit_key_touch_progress_is_rejected_after_both_domains_release() {
    let mut engine = hybrid(&[InputKind::Key, InputKind::Touch], true);
    down(&mut engine);
    let ticket = request(&mut engine, 10);
    raw(
        &mut engine,
        2,
        20,
        &[
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_KEY, 0x14a, 0),
            (EV_KEY, 30, 0),
        ],
    );
    assert!(begin(&mut engine, &ticket, 21).is_err());
}
