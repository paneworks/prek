//! Native routing models with synthetic device descriptions; no kernel I/O.
use super::*;
mod chains;
use crate::config::{Key, Millis, Scope};

fn hold(key: u16) -> Trigger {
    Trigger::ButtonHold {
        key: Key(key),
        after: Millis(100),
        repeat: None,
    }
}
fn setup(first: Trigger) -> (Routing, Device, Device) {
    setup_with_target(first, false)
}
fn snapshot(at: u64, second: bool) -> Snapshot {
    let geometry = output();
    let rect = crate::context::Rect {
        origin: geometry.origin,
        size: geometry.logical_size(),
    };
    Snapshot {
        generation: 1,
        revision: at + 2,
        captured_at: ms(at),
        state: State::Desktop,
        secure_overlay: false,
        outputs: vec![OutputState {
            geometry,
            geometry_generation: 2,
            active_workspace: Some(1),
            screen: Screen::On,
            keyboard_known: true,
            keyboard: None,
        }],
        windows: ["0x1234:abc", "0x2345:abc"]
            .into_iter()
            .enumerate()
            .map(|(index, id)| crate::context::Window {
                id: id.into(),
                output: "test".into(),
                workspace: 1,
                rect,
                content: rect,
                mapped: true,
                visible: true,
                focused: (index == 1) == second,
                z_order: Some(index as i64),
            })
            .collect(),
        regions: vec![],
    }
}
fn setup_with_target(first: Trigger, targeted: bool) -> (Routing, Device, Device) {
    setup_profile(vec![first, hold(48)], targeted, Delivery::Reserve)
}
fn setup_profile(
    steps: Vec<Trigger>,
    targeted: bool,
    delivery: Delivery,
) -> (Routing, Device, Device) {
    let mut config = config();
    config.timing.tap_gap = Millis(20);
    let selector = config.devices.get_mut("touch").unwrap();
    selector.identity = None;
    selector.keys = vec![Key(30), Key(48)];
    config.policy.allow_hyprland_actions = targeted;
    let rule = &mut config.rules[0];
    rule.contexts = vec![if targeted {
        Context::Desktop
    } else {
        Context::Console
    }];
    rule.scope = Scope::Everywhere;
    rule.delivery = delivery;
    rule.trigger = Trigger::Sequence {
        timeout: Millis(1000),
        steps,
    };
    if targeted {
        rule.output = Some("test".into());
        rule.action = Action::Hyprland {
            action: crate::config::WindowAction::Close,
        };
    }
    let mut routing = Routing::new(config).unwrap();
    if targeted {
        context(&mut routing, 0);
        routing
            .authority_mut()
            .update(snapshot(0, false), ms(0))
            .unwrap();
        routing.actions_available(
            1,
            BTreeSet::from([Capability::HyprlandActions, Capability::Observations]),
            ms(0),
        );
    } else {
        routing
            .authority_mut()
            .enter_console("seat0".into())
            .unwrap();
        routing.actions_available(1, BTreeSet::from([Capability::Observations]), ms(0));
    }
    let mut first = device();
    first.axes.clear();
    first.properties.clear();
    first.capabilities = BTreeMap::from([(EV_SYN, vec![SYN_REPORT]), (EV_KEY, vec![30, 46, 48])]);
    let mut second = first.clone();
    second.source.device += 1;
    second.identity.push_str("-second");
    for device in [&first, &second] {
        routing
            .attach(device.clone(), &ready(device), None, ms(0))
            .unwrap();
    }
    (routing, first, second)
}
fn key(routing: &mut Routing, device: &Device, sequence: u64, at: u64, code: u16, value: i32) {
    routing
        .push(Record::Frame {
            frame: Frame {
                source: device.source.clone(),
                sequence,
                at: ms(at),
                events: vec![
                    RawEvent {
                        kind: EV_KEY,
                        code,
                        value,
                    },
                    RawEvent {
                        kind: EV_SYN,
                        code: SYN_REPORT,
                        value: 0,
                    },
                ],
            },
        })
        .unwrap();
}
fn received(routing: &mut Routing, device: &Device, at: u64) -> Vec<Frame> {
    let mut result = Vec::new();
    while let Some(scheduled) = routing.due(&device.source, ms(at)).unwrap().cloned() {
        result.push(scheduled.frame);
        routing
            .delivered(&device.source, scheduled.ticket, ms(at))
            .unwrap();
    }
    result
}

#[test]
fn active_sequence_step_reserves_its_new_source_before_the_match() {
    let (mut routing, first, second) = setup(Trigger::Key { key: Key(30) });
    key(&mut routing, &first, 1, 0, 30, 1);
    key(&mut routing, &first, 2, 10, 30, 0);
    assert!(received(&mut routing, &first, 10).is_empty());
    key(&mut routing, &second, 1, 20, 48, 1);
    assert!(routing.take_dispatches(ms(20)).is_empty());
    assert!(
        received(&mut routing, &second, 20).is_empty(),
        "active later source escaped before recognition"
    );
    routing.push(Record::Tick { at: ms(120) }).unwrap();
    let events = routing.take_dispatches(ms(120));
    assert!(
        matches!(&events[..], [Event::Matched { source, evidence: crate::rules::Evidence::Sequence { steps }, .. }]
        if source == &first.source && steps.len() == 2)
    );
    key(&mut routing, &second, 2, 130, 48, 0);
    assert!(received(&mut routing, &second, 130).is_empty());
    assert!(routing.take_dispatches(ms(130)).is_empty());
}

#[test]
fn a_future_step_cannot_reserve_or_reuse_input_started_before_activation() {
    let (mut routing, first, second) = setup(hold(30));
    key(&mut routing, &first, 1, 0, 30, 1);
    key(&mut routing, &second, 1, 10, 48, 1);
    let ordinary = received(&mut routing, &second, 10);
    assert_eq!(ordinary.len(), 1);
    assert_eq!(ordinary[0].events[0].value, 1);
    routing.push(Record::Tick { at: ms(110) }).unwrap();
    assert!(routing.take_dispatches(ms(110)).is_empty());
    key(&mut routing, &second, 2, 111, 48, 0);
    assert_eq!(received(&mut routing, &second, 111).len(), 1);
    key(&mut routing, &second, 3, 120, 48, 1);
    assert!(received(&mut routing, &second, 120).is_empty());
    routing.push(Record::Tick { at: ms(220) }).unwrap();
    assert_eq!(routing.take_dispatches(ms(220)).len(), 1);
    key(&mut routing, &second, 4, 230, 48, 0);
    key(&mut routing, &first, 2, 231, 30, 0);
    assert!(received(&mut routing, &second, 231).is_empty());
    assert!(received(&mut routing, &first, 231).is_empty());
}

#[test]
fn a_later_source_with_another_frozen_target_is_rejected_before_reservation() {
    let (mut routing, first, second) = setup_with_target(Trigger::Key { key: Key(30) }, true);
    key(&mut routing, &first, 1, 0, 30, 1);
    key(&mut routing, &first, 2, 10, 30, 0);
    assert!(received(&mut routing, &first, 10).is_empty());
    routing
        .authority_mut()
        .update(snapshot(20, true), ms(20))
        .unwrap();
    key(&mut routing, &second, 1, 21, 48, 1);
    assert_eq!(received(&mut routing, &second, 21).len(), 1);
    assert!(
        routing
            .take_events()
            .iter()
            .any(|event| matches!(event, Event::Cancelled { at, reason, .. }
        if *at == ms(21) && reason == "sequence target changed"))
    );
    key(&mut routing, &second, 2, 22, 48, 0);
    assert_eq!(received(&mut routing, &second, 22).len(), 1);
    routing.push(Record::Tick { at: ms(130) }).unwrap();
    assert!(routing.take_dispatches(ms(130)).is_empty());
}

#[test]
fn losing_an_admitted_later_source_cancels_the_parent_before_completion() {
    let (mut routing, first, second) = setup(Trigger::Key { key: Key(30) });
    key(&mut routing, &first, 1, 0, 30, 1);
    key(&mut routing, &first, 2, 10, 30, 0);
    key(&mut routing, &second, 1, 20, 48, 1);
    assert!(received(&mut routing, &second, 20).is_empty());
    routing
        .push(Record::Remove {
            source: second.source.clone(),
            at: ms(30),
        })
        .unwrap();
    assert!(routing.engine.route_intents().is_empty());
    assert_eq!(routing.take_recoveries().len(), 1);
    routing.push(Record::Tick { at: ms(130) }).unwrap();
    assert!(routing.take_dispatches(ms(130)).is_empty());
    assert!(
        routing
            .take_events()
            .iter()
            .any(|event| matches!(event, Event::Cancelled { .. }))
    );
}
