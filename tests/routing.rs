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
