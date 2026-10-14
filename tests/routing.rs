//! Explicitly isolated kernel gates; never part of ordinary cargo test.
//! Requires root, PREK_ISOLATED_ROUTING=1, a dedicated Linux environment with
//! /dev/uinput, /run/prek/devices and this build's packaged udev import rule.
//! Every opened/grabbed event node belongs to a virtual device created here.

#![cfg(target_os = "linux")]

#[path = "support/kernel.rs"]
mod kernel_fixture;
use evdev::InputEvent;
use kernel_fixture::*;
use prek::{
    input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, Device, EV_ABS,
        EV_KEY, EV_SYN, RawEvent,
    },
    kernel::Virtual,
    router::{Candidate, Policy},
};
use std::{
    collections::BTreeMap,
    path::Path,
    thread,
    time::{Duration, Instant},
};

fn configured_touch_delivery(fingers: u16) {
    use prek::{
        config::{
            Action, Config, Context, Delivery, Direction, Distance, Screen, Selector, Trigger,
        },
        context::{Binding, OutputState, SessionKind, Snapshot, State},
        gestures::{Mapping, Output, Point, Transform},
        protocol::Capability,
        replay::Record,
        routing::Routing,
    };
    use std::collections::BTreeSet;
    let registry = authorized();
    let original = fixture(true);
    let device = fixture(true);
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&original)).unwrap()).unwrap();
    let mut output_device =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut input = Receiver::open(&source, &device.source);
    let mut target = Receiver::open(&output_device, &device.source);
    let mut config = Config::preset("desktop").unwrap();
    config.mode = prek::config::Mode::Route;
    config.devices = BTreeMap::from([(
        "touch".into(),
        Selector {
            identity: Some(device.identity.clone()),
            ..Selector::default()
        },
    )]);
    config.policy.allowed_devices = vec!["touch".into()];
    let rule = &mut config.rules[0];
    rule.enabled = true;
    rule.devices = vec!["touch".into()];
    rule.contexts = vec![Context::Desktop];
    rule.delivery = Delivery::Buffer;
    rule.trigger = Trigger::Swipe {
        fingers: 2,
        direction: Direction::Down,
        travel: Distance::Millimetres(40.0),
        fallback_travel: Distance::Normalized(0.2),
        continuous: false,
    };
    rule.action = Action::Event {
        name: "configured-down".into(),
    };
    let mut routing = Routing::new(config).unwrap();
    // Context is a model fixture, not a claim about a real desktop session.
    // The kernel readiness/delivery proof uses this actual isolated receiver.
    let geometry = Output {
        name: "fixture".into(),
        pixel_width: 600.0,
        pixel_height: 1200.0,
        scale: 1.0,
        origin: Point { x: 0.0, y: 0.0 },
        transform: Transform::Normal,
    };
    routing
        .authority_mut()
        .bind(
            Binding {
                seat: "seat0".into(),
                session: "isolated-context".into(),
                uid: 1000,
                compositor_pid: 10,
                compositor_start: 20,
                kind: SessionKind::User,
            },
            true,
            false,
        )
        .unwrap();
    let time = now();
    routing
        .authority_mut()
        .update(
            Snapshot {
                generation: 1,
                revision: 1,
                captured_at: time,
                state: State::Desktop,
                outputs: vec![OutputState {
                    geometry: geometry.clone(),
                    geometry_generation: 2,
                    active_workspace: Some(1),
                    screen: Screen::On,
                    keyboard: None,
                }],
                windows: vec![],
                regions: vec![],
                secure_overlay: false,
            },
            time,
        )
        .unwrap();
    routing.actions_available(1, BTreeSet::from([Capability::Observations]), time);
    let mut mapping = Mapping::from_device(&device).unwrap();
    mapping.output = Some(geometry);
    mapping.generation = 2;
    routing
        .attach(
            device.clone(),
            &readiness(&device, &output_device),
            Some(mapping),
            now(),
        )
        .unwrap();
    let mut down = vec![(EV_KEY, 0x14a, 1)];
    for slot in 0..fingers {
        down.extend([
            (EV_ABS, ABS_MT_SLOT, i32::from(slot)),
            (EV_ABS, ABS_MT_TRACKING_ID, i32::from(slot) + 10),
            (EV_ABS, ABS_MT_POSITION_X, 1000 + i32::from(slot) * 500),
            (EV_ABS, ABS_MT_POSITION_Y, 1000),
        ]);
    }
    send(&mut source, 1, &down);
    let first = input.next(Duration::from_secs(1)).unwrap();
    routing
        .push(Record::Frame {
            frame: first.clone(),
        })
        .unwrap();
    assert!(routing.due(&device.source, now()).unwrap().is_none());
    thread::sleep(if fingers == 2 {
        Duration::from_millis(140)
    } else {
        Duration::from_millis(40)
    });
    if fingers == 2 {
        send(
            &mut source,
            2,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_POSITION_Y, 2000),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_POSITION_Y, 2000),
            ],
        );
        routing
            .push(Record::Frame {
                frame: input.next(Duration::from_secs(1)).unwrap(),
            })
            .unwrap();
    }
    let mut release = vec![(EV_KEY, 0x14a, 0)];
    for slot in 0..fingers {
        release.extend([
            (EV_ABS, ABS_MT_SLOT, i32::from(slot)),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
        ]);
    }
    send(&mut source, 3, &release);
    let last = input.next(Duration::from_secs(1)).unwrap();
    routing
        .push(Record::Frame {
            frame: last.clone(),
        })
        .unwrap();
    if fingers == 2 {
        assert!(routing.due(&device.source, now()).unwrap().is_none());
        assert!(
            target.next(Duration::from_millis(40)).is_none(),
            "consumed contact or click leaked"
        );
        let actions = routing.take_dispatches(now());
        assert_eq!(actions.len(), 1, "{:?}", routing.take_events());
        assert!(
            matches!(&actions[0], prek::rules::Event::Matched { action: Action::Event { name }, .. } if name == "configured-down")
        );
    } else {
        let mut received = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(1);
        while received.len() < 2 {
            let time = now();
            if let Some(scheduled) = routing.due(&device.source, time).unwrap().cloned() {
                output_device.enqueue(&scheduled.frame).unwrap();
                assert_eq!(
                    output_device.flush().unwrap(),
                    prek::kernel::Flush::Complete
                );
                routing
                    .delivered(&device.source, scheduled.ticket, now())
                    .unwrap();
                received.push(target.next(Duration::from_secs(1)).unwrap());
            } else {
                thread::sleep(Duration::from_millis(1));
            }
            assert!(Instant::now() < deadline);
        }
        assert_eq!(received[0].events, first.events);
        assert_eq!(received[1].events, last.events);
        let physical_gap = last.at - first.at;
        let output_gap = received[1].at - received[0].at;
        assert!(
            output_gap.abs_diff(physical_gap) < Duration::from_millis(20),
            "tap cadence changed: {physical_gap:?} -> {output_gap:?}"
        );
        assert!(routing.take_dispatches(now()).is_empty());
        assert!(target.next(Duration::from_millis(30)).is_none());
    }
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t50_configured_swipe_consumes_the_actual_downstream_touch_stream() {
    configured_touch_delivery(2);
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t51_configured_unmatched_tap_reaches_the_receiver_once_with_its_cadence() {
    configured_touch_delivery(1);
}

fn passthrough(original: Device, device: Device, frames: &[Vec<(u16, u16, i32)>]) {
    let registry = authorized();
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&original)).unwrap()).unwrap();
    let mut output =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut input = Receiver::open(&source, &device.source);
    let mut target = Receiver::open(&output, &device.source);
    let mut router = route(device, &output);
    for (index, events) in frames.iter().enumerate() {
        send(&mut source, index as u64 + 1, events);
        let frame = input.next(Duration::from_secs(1)).expect("source event");
        router.push(frame.clone(), None, now()).unwrap();
        assert!(deliver(&mut router, &mut output, now()));
        assert_eq!(
            target.next(Duration::from_secs(1)).unwrap().events,
            frame.events
        );
    }
    assert!(target.next(Duration::from_millis(30)).is_none());
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t46_isolated_mouse_preserves_motion_buttons_and_both_wheel_resolutions() {
    let mouse = || {
        let mut device = fixture(false);
        device.name = "Prek test mouse".into();
        device.capabilities = BTreeMap::from([
            (EV_SYN, vec![0]),
            (EV_KEY, vec![272, 273]),
            (2, vec![0, 1, 6, 8, 11, 12]),
            (4, vec![4]),
        ]);
        device
    };
    passthrough(
        mouse(),
        mouse(),
        &[
            vec![
                (4, 4, 272),
                (EV_KEY, 272, 1),
                (2, 0, 7),
                (2, 1, -4),
                (2, 8, 1),
                (2, 11, 120),
            ],
            vec![(2, 0, -2), (2, 1, 8), (2, 6, -1), (2, 12, -120)],
            vec![(4, 4, 272), (EV_KEY, 272, 0)],
        ],
    );
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t48_isolated_touchpad_preserves_slots_tool_flags_and_button_clicks() {
    let touchpad = || {
        let mut device = fixture(true);
        device.name = "Prek test touchpad".into();
        device.properties = vec![0, 2];
        device
            .capabilities
            .insert(EV_KEY, vec![272, 0x145, 0x14a, 0x14d]);
        device
    };
    passthrough(
        touchpad(),
        touchpad(),
        &[
            vec![
                (EV_KEY, 0x14a, 1),
                (EV_KEY, 0x145, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 42),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_ABS, ABS_MT_POSITION_Y, 200),
            ],
            vec![
                (EV_KEY, 0x145, 0),
                (EV_KEY, 0x14d, 1),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 43),
                (EV_ABS, ABS_MT_POSITION_X, 300),
                (EV_ABS, ABS_MT_POSITION_Y, 400),
                (EV_KEY, 272, 1),
            ],
            vec![
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_POSITION_X, 110),
                (EV_ABS, ABS_MT_POSITION_Y, 220),
                (EV_KEY, 272, 0),
            ],
            vec![
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                (EV_KEY, 0x14d, 0),
                (EV_KEY, 0x14a, 0),
            ],
        ],
    );
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t55_isolated_keyboard_led_feedback_reaches_source_without_echo() {
    let registry = authorized();
    let keyboard = || {
        let mut device = fixture(false);
        device.capabilities.insert(17, vec![0]);
        device
    };
    let original = keyboard();
    let device = keyboard();
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&original)).unwrap()).unwrap();
    let mut output =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut input = Receiver::open(&source, &original.source);
    let mut target = Receiver::open(&output, &device.source);
    for value in [1, 0] {
        target
            .raw
            .send_events(&[InputEvent::new(17, 0, value)])
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let feedback = loop {
            let events = output.feedback().unwrap();
            if !events.is_empty() {
                break events;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(
            feedback,
            [RawEvent {
                kind: 17,
                code: 0,
                value
            }]
        );
        let events: Vec<_> = feedback
            .iter()
            .map(|event| InputEvent::new(event.kind, event.code, event.value))
            .collect();
        input.raw.send_events(&events).unwrap();
        assert_eq!(
            input
                .raw
                .get_led_state()
                .unwrap()
                .contains(evdev::LedCode(0)),
            value == 1
        );
        assert_eq!(source.feedback().unwrap(), feedback);
        assert!(output.feedback().unwrap().is_empty());
    }
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t46_isolated_keyboard_receives_passthrough_once_and_drop_removes_nodes() {
    let registry = authorized();
    let original = fixture(false);
    let device = fixture(false);
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&original)).unwrap()).unwrap();
    let mut output =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut input = Receiver::open(&source, &device.source);
    let mut target = Receiver::open(&output, &device.source);
    let output_nodes = output.event_nodes().unwrap();
    let mut router = route(device, &output);
    for (sequence, events) in [
        (1, vec![(EV_KEY, 125, 1), (EV_KEY, 30, 1)]),
        (2, vec![(EV_KEY, 30, 2)]),
        (3, vec![(EV_KEY, 30, 0), (EV_KEY, 125, 0)]),
    ] {
        send(&mut source, sequence, &events);
        let frame = input.next(Duration::from_secs(1)).expect("source event");
        router.push(frame.clone(), None, now()).unwrap();
        assert!(deliver(&mut router, &mut output, now()));
        assert_eq!(
            target.next(Duration::from_secs(1)).unwrap().events,
            frame.events
        );
    }
    assert!(target.next(Duration::from_millis(30)).is_none());
    drop(target);
    drop(output);
    let deadline = Instant::now() + Duration::from_secs(2);
    while output_nodes.iter().any(|path| Path::new(path).exists()) {
        assert!(
            Instant::now() < deadline,
            "virtual output leaked after drop"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t50_isolated_claimed_touch_reaches_no_downstream_contact_or_click() {
    let registry = authorized();
    let original = fixture(true);
    let device = fixture(true);
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&original)).unwrap()).unwrap();
    let mut output =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut input = Receiver::open(&source, &device.source);
    let mut target = Receiver::open(&output, &device.source);
    let mut router = route(device, &output);
    send(
        &mut source,
        1,
        &[
            (EV_KEY, 0x14a, 1),
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 42),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
        ],
    );
    let frame = input.next(Duration::from_secs(1)).unwrap();
    router
        .push(
            frame,
            Some(Candidate {
                id: 1,
                authorization: 1,
                policy: Policy::Buffer {
                    join: Duration::ZERO,
                    decision: Duration::from_millis(300),
                },
            }),
            now(),
        )
        .unwrap();
    router.claim(1, 1, now()).unwrap();
    send(
        &mut source,
        2,
        &[(EV_KEY, 0x14a, 0), (EV_ABS, ABS_MT_TRACKING_ID, -1)],
    );
    let frame = input.next(Duration::from_secs(1)).unwrap();
    router.push(frame, None, now()).unwrap();
    assert!(!deliver(&mut router, &mut output, now()));
    assert!(target.next(Duration::from_millis(50)).is_none());
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t51_isolated_buffered_tap_preserves_delivery_interval_without_bursting() {
    let registry = authorized();
    let original = fixture(true);
    let device = fixture(true);
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&original)).unwrap()).unwrap();
    let mut output =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut input = Receiver::open(&source, &device.source);
    let mut target = Receiver::open(&output, &device.source);
    let mut router = route(device, &output);
    send(
        &mut source,
        1,
        &[
            (EV_KEY, 0x14a, 1),
            (EV_ABS, ABS_MT_TRACKING_ID, 42),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
        ],
    );
    let first = input.next(Duration::from_secs(1)).unwrap();
    router
        .push(
            first.clone(),
            Some(Candidate {
                id: 1,
                authorization: 1,
                policy: Policy::Buffer {
                    join: Duration::from_millis(120),
                    decision: Duration::from_millis(300),
                },
            }),
            now(),
        )
        .unwrap();
    thread::sleep(Duration::from_millis(40));
    send(
        &mut source,
        2,
        &[(EV_KEY, 0x14a, 0), (EV_ABS, ABS_MT_TRACKING_ID, -1)],
    );
    let last = input.next(Duration::from_secs(1)).unwrap();
    router.push(last.clone(), None, now()).unwrap();
    router.cancel(1, 1, now()).unwrap();
    assert!(deliver(&mut router, &mut output, now()));
    let down = target.next(Duration::from_secs(1)).unwrap();
    assert_eq!(down.events, first.events);
    let deadline = Instant::now() + Duration::from_millis(300);
    while !deliver(&mut router, &mut output, now()) {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let up = target.next(Duration::from_secs(1)).unwrap();
    assert_eq!(up.events, last.events);
    let physical_interval = last.at - first.at;
    let virtual_interval = up.at - down.at;
    assert!(
        virtual_interval >= Duration::from_millis(20),
        "buffered tap was burst into a click"
    );
    assert!(virtual_interval.abs_diff(physical_interval) <= Duration::from_millis(20));
    assert!(target.next(Duration::from_millis(30)).is_none());
}
