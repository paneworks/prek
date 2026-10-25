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
    setup_profile(vec![first, hold(48)], Delivery::Reserve)
}
fn setup_profile(steps: Vec<Trigger>, delivery: Delivery) -> (Routing, Device, Device) {
    setup_mode(steps, delivery, false)
}
fn setup_mode(steps: Vec<Trigger>, delivery: Delivery, scoped: bool) -> (Routing, Device, Device) {
    let mut config = config();
    config.timing.tap_gap = Millis(20);
    let selector = config.devices.get_mut("touch").unwrap();
    selector.identity = None;
    selector.keys = vec![Key(30), Key(48)];
    let rule = &mut config.rules[0];
    rule.contexts = vec![Context::Console];
    rule.scope = Scope::Everywhere;
    rule.delivery = delivery;
    rule.trigger = Trigger::Sequence {
        timeout: Millis(1000),
        steps,
    };
    let mut routing = if scoped {
        Routing::with_input_projection(config, crate::keys::worker::tests::fence()).unwrap()
    } else {
        Routing::new(config).unwrap()
    };
    routing
        .authority_mut()
        .enter_console("seat0".into())
        .unwrap();
    routing.actions_available(1, BTreeSet::from([Capability::Observations]), ms(0));
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

fn pump(routing: &mut Routing, devices: [&Device; 2], from: u64, to: u64) -> Vec<Frame> {
    let mut result = Vec::new();
    for at in from..=to {
        for device in devices {
            result.extend(
                received(routing, device, at)
                    .into_iter()
                    .filter(|frame| frame.source == devices[1].source),
            );
        }
    }
    result
}

#[test]
fn a_future_step_press_is_delivered_but_never_reused_in_either_routing_mode() {
    for scoped in [false, true] {
        let (mut routing, first, second) =
            setup_mode(vec![hold(30), hold(48)], Delivery::Reserve, scoped);
        let devices = [&first, &second];
        key(&mut routing, &first, 1, 0, 30, 1);
        key(&mut routing, &second, 1, 10, 48, 1);
        let mut ordinary = pump(&mut routing, devices, 10, 110);
        routing.push(Record::Tick { at: ms(110) }).unwrap();
        assert!(
            routing.take_dispatches(ms(110)).is_empty(),
            "scoped={scoped}"
        );
        key(&mut routing, &second, 2, 111, 48, 0);
        ordinary.extend(pump(&mut routing, devices, 111, 120));
        key(&mut routing, &second, 3, 120, 48, 1);
        ordinary.extend(pump(&mut routing, devices, 120, 220));
        routing.push(Record::Tick { at: ms(220) }).unwrap();
        assert_eq!(routing.take_dispatches(ms(220)).len(), 1, "scoped={scoped}");
        key(&mut routing, &second, 4, 230, 48, 0);
        key(&mut routing, &first, 2, 231, 30, 0);
        ordinary.extend(pump(&mut routing, devices, 231, 700));
        assert_eq!(
            ordinary
                .iter()
                .map(|frame| frame.sequence)
                .collect::<Vec<_>>(),
            [1, 2],
            "scoped={scoped}"
        );
    }
}
