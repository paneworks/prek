use super::*;
use crate::{
    config::{Layer, LayerActivation, Millis},
    input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, Axis, EV_ABS, EV_REL,
    },
};

fn routed(rules: Vec<Rule>, touch: bool, layer: bool) -> Routing {
    let mut config = config(rules);
    config.policy.allow_input_events = true;
    config.timing.join_window = Millis(1);
    config.timing.decision_window = Millis(100);
    if layer {
        config.layers = vec![Layer {
            id: "held-key".into(),
            key: Key(31),
            toggle: true,
            activation: LayerActivation::Physical,
        }];
    }
    let mut routing = Routing::with_key_projection(config, fence()).unwrap();
    context(&mut routing, 0, 1, ContextState::Desktop);
    let mut device = device(1);
    device.capabilities.get_mut(&EV_KEY).unwrap().push(42);
    if touch {
        device.properties = vec![1];
        device.capabilities.get_mut(&EV_KEY).unwrap().push(0x14a);
        let axes = [
            (ABS_MT_SLOT, 1),
            (ABS_MT_POSITION_X, 1000),
            (ABS_MT_POSITION_Y, 1000),
            (ABS_MT_TRACKING_ID, 65535),
        ];
        device
            .capabilities
            .insert(EV_ABS, axes.iter().map(|(code, _)| *code).collect());
        device.axes = axes
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
    } else {
        device.capabilities.insert(EV_REL, vec![0]);
    }
    let mapping = if touch {
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
        let mut snapshot = routing.authority().fresh(ms(0)).unwrap().clone();
        snapshot.revision += 1;
        snapshot.outputs = vec![crate::context::OutputState {
            geometry,
            geometry_generation: mapping.generation,
            screen: crate::config::Screen::On,
        }];
        routing.authority_mut().update(snapshot, ms(0)).unwrap();
        Some(mapping)
    } else {
        None
    };
    routing
        .attach(device.clone(), &ready(&device), mapping, ms(0))
        .unwrap();
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), ms(0));
    routing
}
fn typed() -> Rule {
    let mut rule = rule(
        "hybrid-key",
        Action::Keys {
            keys: vec![Key(42)],
        },
    );
    rule.delivery = Delivery::Buffer;
    rule
}
fn mixed(events: &[(u16, u16, i32)]) -> Frame {
    Frame {
        events: events
            .iter()
            .map(|&(kind, code, value)| RawEvent { kind, code, value })
            .chain([RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            }])
            .collect(),
        ..packet(1, 1, 10, &[])
    }
}
fn raw_frames(routing: &mut Routing, seat: &Seat<TestSink>) -> Vec<Frame> {
    raw_frames_at(routing, seat, 10)
}
fn raw_frames_at(routing: &mut Routing, seat: &Seat<TestSink>, at: u64) -> Vec<Frame> {
    let mut frames = Vec::new();
    while let Some(scheduled) = routing.due(&source(1), ms(at)).unwrap().cloned() {
        let span = routing.input_span(&source(1), scheduled.ticket).unwrap();
        if seat.blocks_ordered_span(routing, &source(1), span) {
            break;
        }
        frames.push(scheduled.frame);
        routing
            .delivered(&source(1), scheduled.ticket, ms(at))
            .unwrap();
    }
    frames
}

#[test]
fn hybrid_key_output_keeps_relative_report_data_after_its_original_key_cut() {
    let mut routing = routed(vec![typed()], false, false);
    let (mut seat, writes) = model();
    let work = submit(
        &mut routing,
        mixed(&[
            (EV_REL, 0, 7),
            (EV_KEY, 30, 1),
            (EV_REL, 0, 9),
            (EV_KEY, 31, 1),
            (EV_KEY, 31, 0),
            (EV_KEY, 30, 0),
        ]),
    );
    assert_eq!(
        work.len(),
        2,
        "events={:?}; recoveries={:?}",
        routing.take_events(),
        routing.take_recoveries()
    );
    seat.admit(&routing, work, None, ms(10)).unwrap();
    poll(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    let raw = raw_frames(&mut routing, &seat);
    assert_eq!(raw.len(), 2);
    assert_eq!(
        raw[0]
            .events
            .iter()
            .filter(|event| event.kind == EV_KEY)
            .map(|event| (event.code, event.value))
            .collect::<Vec<_>>(),
        [(31, 1), (31, 0)]
    );
    assert_eq!(
        raw[1]
            .events
            .iter()
            .filter(|event| event.kind == EV_REL)
            .map(|event| (event.code, event.value))
            .collect::<Vec<_>>(),
        [(0, 7), (0, 9)]
    );
}

#[test]
fn observed_touch_hold_waits_for_the_complete_contact_report() {
    let mut observer = rule(
        "contact",
        Action::Keys {
            keys: vec![Key(29)],
        },
    );
    observer.trigger = Trigger::TouchHold {
        fingers: 1,
        after: Millis(1),
    };
    observer.delivery = Delivery::Observe;
    let mut routing = routed(vec![typed(), observer], true, false);
    let (mut seat, writes) = model();
    let work = submit(
        &mut routing,
        mixed(&[
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 7),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_KEY, 30, 1),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
            (EV_KEY, 0x14a, 1),
        ]),
    );
    assert_eq!(
        work.len(),
        1,
        "events={:?}; recoveries={:?}",
        routing.take_events(),
        routing.take_recoveries()
    );
    seat.admit(&routing, work, None, ms(10)).unwrap();
    poll(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    let observed = arbitration::tick(&mut routing, 11);
    assert_eq!(observed.len(), 1);
    seat.admit(&routing, observed, None, ms(11)).unwrap();
    poll(&mut seat, &routing, 11);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    let raw = raw_frames_at(&mut routing, &seat, 11);
    assert_eq!(raw.len(), 1);
    assert!(
        raw[0]
            .events
            .iter()
            .any(|event| event.kind == EV_KEY && event.code == 0x14a)
    );
    assert_eq!(
        raw[0].events,
        [
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 7),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
            (EV_KEY, 0x14a, 1),
            (EV_SYN, SYN_REPORT, 0),
        ]
        .map(|(kind, code, value)| RawEvent { kind, code, value })
    );
    poll(&mut seat, &routing, 11);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0), (29, 1), (29, 0)]);
}

#[test]
fn a_same_report_key_layer_precedes_the_key_action_and_coherent_touch_tail() {
    let mut rule = typed();
    rule.layer = Some("held-key".into());
    let mut routing = routed(vec![rule], true, true);
    let (mut seat, writes) = model();
    let work = submit(
        &mut routing,
        mixed(&[
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 7),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
            (EV_KEY, 0x14a, 1),
            (EV_KEY, 31, 1),
            (EV_KEY, 31, 0),
            (EV_KEY, 30, 1),
        ]),
    );
    assert_eq!(
        work.len(),
        1,
        "events={:?}; recoveries={:?}",
        routing.take_events(),
        routing.take_recoveries()
    );
    seat.admit(&routing, work, None, ms(10)).unwrap();
    poll(&mut seat, &routing, 10);
    assert!(key_events(&writes).is_empty());
    let prefix = raw_frames(&mut routing, &seat);
    assert_eq!(prefix.len(), 1);
    assert_eq!(
        prefix[0].events,
        [(EV_KEY, 31, 1), (EV_KEY, 31, 0), (EV_SYN, SYN_REPORT, 0)]
            .map(|(kind, code, value)| RawEvent { kind, code, value })
    );
    poll(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    let tail = raw_frames(&mut routing, &seat);
    assert_eq!(tail.len(), 1);
    assert!(tail[0].events.iter().any(|event| event.kind == EV_ABS));
    assert!(tail[0].events.iter().any(|event| event.code == 0x14a));
}

#[test]
fn a_key_sequence_keeps_its_key_condition_on_a_touch_source() {
    let mut rule = typed();
    rule.require_keys = vec![Key(42)];
    rule.trigger = Trigger::Sequence {
        steps: vec![Trigger::Key { key: Key(30) }, Trigger::Key { key: Key(31) }],
        timeout: Millis(200),
    };
    let mut routing = routed(vec![rule], true, false);
    let (mut seat, writes) = model();
    let work = submit(
        &mut routing,
        mixed(&[
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 7),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
            (EV_KEY, 0x14a, 1),
            (EV_KEY, 42, 1),
            (EV_KEY, 30, 1),
            (EV_KEY, 31, 1),
        ]),
    );
    assert_eq!(
        work.len(),
        1,
        "events={:?}; recoveries={:?}",
        routing.take_events(),
        routing.take_recoveries()
    );
    seat.admit(&routing, work, None, ms(10)).unwrap();
    poll(&mut seat, &routing, 10);
    assert!(key_events(&writes).is_empty());
    let prefix = raw_frames(&mut routing, &seat);
    assert_eq!(prefix.len(), 1);
    assert_eq!(
        prefix[0].events,
        [(EV_KEY, 42, 1), (EV_SYN, SYN_REPORT, 0)].map(|(kind, code, value)| RawEvent {
            kind,
            code,
            value
        })
    );
    poll(&mut seat, &routing, 10);
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    let tail = raw_frames(&mut routing, &seat);
    assert_eq!(tail.len(), 1);
    assert!(tail[0].events.iter().any(|event| event.kind == EV_ABS));
}

fn custom_touch_down(routing: &mut Routing) -> crate::recognition::Ticket {
    assert!(
        submit(
            routing,
            mixed(&[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 7),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_ABS, ABS_MT_POSITION_Y, 200),
                (EV_KEY, 0x14a, 1),
                (EV_KEY, 30, 1),
            ])
        )
        .is_empty()
    );
    let mut requests = routing.recognition_requests(ms(10)).unwrap();
    assert_eq!(requests.len(), 1);
    let ticket = requests.pop().unwrap();
    let crate::input_events::Event::Frame {
        changes, contacts, ..
    } = &ticket.input
    else {
        panic!("missing original key frame")
    };
    assert!(contacts.is_empty());
    assert_eq!(
        changes,
        &[crate::input::Change::Key {
            code: 30,
            state: crate::input::KeyState::Press
        }]
    );
    ticket
}

#[test]
fn a_key_only_custom_group_closes_on_key_release_while_touch_stays_held() {
    let mut rule = custom::recognizer();
    let Trigger::Custom { recognizer, .. } = &mut rule.trigger else {
        unreachable!()
    };
    recognizer.continuous = true;
    let mut routing = routed(vec![rule], true, false);
    let ticket = custom_touch_down(&mut routing);
    assert!(submit(&mut routing, packet(1, 2, 20, &[(30, 0)])).is_empty());
    assert!(
        routing
            .recognition_response(
                crate::recognition::Proposal {
                    fence: ticket.fence,
                    ticket: ticket.id,
                    phase: crate::recognition::Phase::Begin,
                    data: serde_json::json!({}),
                },
                ms(25)
            )
            .is_err()
    );
    assert!(routing.take_scoped_dispatches(ms(25)).is_empty());
}

#[test]
fn a_held_contact_does_not_block_new_custom_keys_after_helper_restart() {
    let mut routing = routed(vec![custom::recognizer()], true, false);
    let _ticket = custom_touch_down(&mut routing);
    routing.recognition_ready(false, ms(11)).unwrap();
    assert!(submit(&mut routing, packet(1, 2, 20, &[(30, 0)])).is_empty());
    routing.recognition_ready(true, ms(21)).unwrap();
    assert!(submit(&mut routing, packet(1, 3, 30, &[(31, 1)])).is_empty());
    let requests = routing.recognition_requests(ms(30)).unwrap();
    assert_eq!(requests.len(), 1);
}

#[test]
fn a_completed_custom_key_group_can_restart_without_releasing_unrelated_touch() {
    let mut routing = routed(vec![custom::recognizer()], true, false);
    let ticket = custom_touch_down(&mut routing);
    assert!(submit(&mut routing, packet(1, 2, 20, &[(30, 0)])).is_empty());
    let first = custom::respond(
        &mut routing,
        &ticket,
        crate::recognition::Phase::Discrete,
        25,
    );
    assert_eq!(first.len(), 1);
    assert!(submit(&mut routing, packet(1, 3, 30, &[(31, 1)])).is_empty());
    assert_eq!(routing.recognition_requests(ms(30)).unwrap().len(), 1);
}
