use super::*;
use crate::config::Millis;

fn pointer() -> Device {
    let mut pointer = device(3);
    pointer.identity = "independent-pointer".into();
    pointer.capabilities =
        BTreeMap::from([(EV_KEY, vec![272]), (crate::input::EV_REL, vec![0, 1])]);
    pointer
}
fn profile(required: bool, slow_keyboard: bool) -> Config {
    let mut shift = rule("shift", Action::Remap { key: Key(42) });
    shift.delivery = Delivery::Buffer;
    let mut hold = rule(
        "pointer-hold",
        Action::Event {
            name: "held".into(),
        },
    );
    hold.devices = vec!["pointer".into()];
    hold.priority = 10;
    hold.delivery = Delivery::Buffer;
    hold.trigger = Trigger::ButtonHold {
        key: Key(272),
        after: Millis(100),
        repeat: None,
    };
    if required {
        hold.require_keys = vec![Key(31)];
    }
    let mut config = config(vec![shift, hold]);
    config.devices.insert(
        "pointer".into(),
        Selector {
            identity: Some(pointer().identity),
            ..Default::default()
        },
    );
    config.policy.allowed_devices.push("pointer".into());
    if slow_keyboard {
        let mut slow = rule(
            "slow",
            Action::Event {
                name: "slow".into(),
            },
        );
        slow.devices = vec!["slow".into()];
        slow.priority = 20;
        slow.delivery = Delivery::Buffer;
        slow.trigger = Trigger::ButtonHold {
            key: Key(31),
            after: Millis(100),
            repeat: None,
        };
        config.devices.insert(
            "slow".into(),
            Selector {
                identity: Some(device(2).identity),
                ..Default::default()
            },
        );
        config.policy.allowed_devices.push("slow".into());
        config.rules.push(slow);
    }
    config
}
fn setup(required: bool, slow_keyboard: bool) -> Routing {
    configured(profile(required, slow_keyboard))
}
fn configured(config: Config) -> Routing {
    let mut routing = Routing::with_key_projection(config, fence()).unwrap();
    context(&mut routing, 0, 1, ContextState::Desktop);
    for device in [device(1), device(2), pointer()] {
        routing
            .attach(device.clone(), &ready(&device), None, ms(0))
            .unwrap();
    }
    routing.actions_available(
        1,
        BTreeSet::from([Capability::SyntheticInput, Capability::Observations]),
        ms(0),
    );
    routing
}
#[test]
fn an_unrelated_pending_pointer_does_not_stall_a_generated_keyboard_press() {
    let mut routing = setup(false, false);
    let (mut seat, writes) = model();
    routing
        .push(Record::Frame {
            frame: frame(3, 1, 10, 272, 1),
        })
        .unwrap();
    assert!(routing.due(&source(3), ms(10)).unwrap().is_none());
    let work = push(&mut routing, 1, 1, 20, 1).pop().unwrap();
    seat.enqueue(work, ms(20)).unwrap();
    for _ in 0..8 {
        seat.poll(&routing, ms(20)).unwrap();
    }
    assert_eq!(key_events(&writes), [(42, 1)]);
    assert!(routing.due(&source(3), ms(20)).unwrap().is_none());
}

#[test]
fn an_explicit_key_condition_keeps_the_pointer_in_the_keyboard_ordering_domain() {
    let mut routing = setup(true, false);
    let (mut seat, writes) = model();
    routing
        .push(Record::Frame {
            frame: frame(1, 1, 5, 31, 1),
        })
        .unwrap();
    while let Some(raw) = routing.due(&source(1), ms(5)).unwrap().cloned() {
        routing.delivered(&source(1), raw.ticket, ms(5)).unwrap();
    }
    routing
        .push(Record::Frame {
            frame: frame(3, 1, 10, 272, 1),
        })
        .unwrap();
    assert!(routing.due(&source(3), ms(10)).unwrap().is_none());
    let work = push(&mut routing, 1, 2, 20, 1).pop().unwrap();
    seat.enqueue(work, ms(20)).unwrap();
    for _ in 0..8 {
        seat.poll(&routing, ms(20)).unwrap();
    }
    assert!(key_events(&writes).is_empty());
}

#[test]
fn an_earlier_pending_legacy_keyboard_remains_an_ordering_dependency() {
    let mut routing = setup(false, true);
    let (mut seat, writes) = model();
    routing
        .push(Record::Frame {
            frame: frame(2, 1, 10, 31, 1),
        })
        .unwrap();
    assert!(routing.due(&source(2), ms(10)).unwrap().is_none());
    let work = push(&mut routing, 1, 1, 20, 1).pop().unwrap();
    seat.enqueue(work, ms(20)).unwrap();
    for _ in 0..8 {
        seat.poll(&routing, ms(20)).unwrap();
    }
    assert!(key_events(&writes).is_empty());
}

#[test]
fn a_blocked_generated_write_does_not_hold_an_unrelated_pointer_report() {
    let mut routing = setup(false, false);
    let (mut seat, writes) = model();
    writes.borrow_mut().steps = (0..32).map(|_| Step::Prefix(0)).collect();
    let work = push(&mut routing, 1, 1, 20, 1).pop().unwrap();
    seat.enqueue(work, ms(20)).unwrap();
    for _ in 0..8 {
        seat.poll(&routing, ms(20)).unwrap();
    }
    for (sequence, at, value) in [(1, 21, 1), (2, 22, 0)] {
        routing
            .push(Record::Frame {
                frame: frame(3, sequence, at, 272, value),
            })
            .unwrap();
    }
    let mut events = Vec::new();
    while let Some(raw) = routing.due(&source(3), ms(40)).unwrap().cloned() {
        let span = routing.input_span(&source(3), raw.ticket).unwrap();
        assert!(!seat.blocks_ordered_span(&routing, &source(3), span));
        events.extend(
            raw.frame
                .events
                .iter()
                .filter(|event| event.kind == EV_KEY)
                .map(|event| (event.code, event.value)),
        );
        routing.delivered(&source(3), raw.ticket, ms(40)).unwrap();
    }
    assert_eq!(events, [(272, 1), (272, 0)]);
    assert!(key_events(&writes).is_empty());
    assert!(seat.flight.is_some());
}

#[test]
fn a_key_dependent_pointer_report_stays_behind_the_pending_generated_write() {
    let mut routing = setup(true, false);
    let (mut seat, writes) = model();
    routing
        .push(Record::Frame {
            frame: frame(1, 1, 5, 31, 1),
        })
        .unwrap();
    ack(&mut routing, 1, 5);
    writes.borrow_mut().steps = (0..32).map(|_| Step::Prefix(0)).collect();
    let work = push(&mut routing, 1, 2, 20, 1).pop().unwrap();
    seat.enqueue(work, ms(20)).unwrap();
    for _ in 0..8 {
        seat.poll(&routing, ms(20)).unwrap();
    }
    for (sequence, at, value) in [(1, 21, 1), (2, 22, 0)] {
        routing
            .push(Record::Frame {
                frame: frame(3, sequence, at, 272, value),
            })
            .unwrap();
    }
    let raw = routing.due(&source(3), ms(40)).unwrap().unwrap().clone();
    let span = routing.input_span(&source(3), raw.ticket).unwrap();
    assert!(seat.blocks_ordered_span(&routing, &source(3), span));
    assert!(key_events(&writes).is_empty());
}

#[test]
fn configured_domains_cover_effects_layers_compounds_and_inactive_rules() {
    use crate::config::{EffectKind, Layer, LayerActivation, NamedMode};
    for case in 0..11 {
        let mut config = profile(false, false);
        let hold = &mut config.rules[1];
        let expected = match case {
            0 => false,
            1 => {
                hold.require_keys = vec![Key(31)];
                true
            }
            2 => {
                hold.layer = Some("armed".into());
                config.layers.push(Layer {
                    id: "armed".into(),
                    key: Key(31),
                    toggle: false,
                    activation: LayerActivation::Physical,
                });
                true
            }
            3 => {
                hold.action = Action::Keys {
                    keys: vec![Key(42)],
                };
                true
            }
            4 => {
                hold.action = Action::Lua {
                    handler: "keys".into(),
                    requires: BTreeSet::from([EffectKind::SyntheticInput]),
                };
                true
            }
            5 => {
                hold.devices.push("keys".into());
                hold.trigger = Trigger::Sequence {
                    steps: vec![hold.trigger.clone(), Trigger::Key { key: Key(30) }],
                    timeout: Millis(500),
                };
                true
            }
            6 => {
                hold.devices.push("keys".into());
                hold.trigger = Trigger::Chord {
                    keys: vec![Key(272), Key(31)],
                    ordered: false,
                    timeout: Millis(100),
                };
                true
            }
            7 => {
                config.policy.allow_input_events = true;
                hold.devices.push("keys".into());
                hold.trigger = Trigger::Custom {
                    recognizer: crate::recognition::Definition {
                        name: "group".into(),
                        selectors: BTreeSet::from(["keys".into(), "pointer".into()]),
                        inputs: BTreeSet::from([crate::recognition::InputKind::Key]),
                        reply_within: Millis(250),
                        continuous: false,
                    },
                    timeout: Millis(1000),
                    wake: None,
                };
                true
            }
            8 => {
                hold.require_keys = vec![Key(31)];
                hold.enabled = false;
                false
            }
            9 => {
                hold.require_keys = vec![Key(31)];
                hold.when_mode = Some("unused".into());
                config.modes.push(NamedMode {
                    id: "unused".into(),
                    active: false,
                });
                false
            }
            10 => {
                hold.devices.push("keys".into());
                false
            }
            _ => unreachable!(),
        };
        let routing = configured(config);
        assert_eq!(
            routing.key_orders_source(&source(3)),
            expected,
            "case={case}"
        );
        assert!(routing.key_orders_source(&source(1)));
        assert!(routing.key_orders_source(&source(2)));
    }
}

#[test]
fn attaching_a_condition_provider_rebuilds_transitive_source_dependencies() {
    let mut config = profile(false, false);
    config.rules[1].require_keys = vec![Key(273)];
    let mut right = pointer();
    right.source = source(4);
    right.identity = "right-pointer".into();
    right.capabilities.get_mut(&EV_KEY).unwrap()[0] = 273;
    config.devices.insert(
        "right".into(),
        Selector {
            identity: Some(right.identity.clone()),
            ..Default::default()
        },
    );
    config.policy.allowed_devices.push("right".into());
    let mut rule = config.rules[1].clone();
    rule.id = "right".into();
    rule.priority = 20;
    rule.devices = vec!["right".into()];
    rule.require_keys = vec![Key(31)];
    rule.trigger = Trigger::ButtonHold {
        key: Key(273),
        after: Millis(100),
        repeat: None,
    };
    config.rules.push(rule);
    let mut routing = configured(config);
    assert!(!routing.key_orders_source(&source(3)));
    routing
        .attach(right.clone(), &ready(&right), None, ms(0))
        .unwrap();
    assert!(routing.key_orders_source(&source(3)));
    assert!(routing.key_orders_source(&source(4)));
    routing.recover(&source(4));
    assert!(!routing.key_orders_source(&source(3)));
    assert!(routing.key_orders_source(&source(4)));
}

#[test]
fn neutral_cleanup_still_fences_an_independent_pointer() {
    let mut routing = setup(false, false);
    let (mut seat, writes) = model();
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    seat.enqueue(work, ms(10)).unwrap();
    for _ in 0..8 {
        seat.poll(&routing, ms(10)).unwrap();
    }
    assert_eq!(key_events(&writes), [(42, 1)]);
    for (sequence, at, value) in [(1, 21, 1), (2, 22, 0)] {
        routing
            .push(Record::Frame {
                frame: frame(3, sequence, at, 272, value),
            })
            .unwrap();
    }
    let raw = routing.due(&source(3), ms(40)).unwrap().unwrap().clone();
    let span = routing.input_span(&source(3), raw.ticket).unwrap();
    writes.borrow_mut().steps = (0..8).map(|_| Step::Prefix(0)).collect();
    seat.retire(ms(40)).unwrap();
    assert!(seat.blocks_ordered_span(&routing, &source(3), span));
    writes.borrow_mut().steps.clear();
    for _ in 0..8 {
        seat.drain(ms(40)).unwrap();
    }
    assert_eq!(key_events(&writes), [(42, 1), (42, 0)]);
    assert!(!seat.blocks_ordered_span(&routing, &source(3), span));
}

#[test]
fn another_seat_cannot_join_a_condition_or_hold_the_keyboard_prefix() {
    let mut config = profile(true, false);
    config.policy.allowed_seats.push("seat1".into());
    let mut own = device(1);
    own.capabilities
        .get_mut(&EV_KEY)
        .unwrap()
        .retain(|code| *code != 31);
    let mut foreign = device(4);
    foreign.source.seat = "seat1".into();
    foreign
        .capabilities
        .get_mut(&EV_KEY)
        .unwrap()
        .retain(|code| *code == 31);
    config.devices.insert(
        "foreign".into(),
        Selector {
            identity: Some(foreign.identity.clone()),
            ..Default::default()
        },
    );
    config.policy.allowed_devices.push("foreign".into());
    let mut routing = Routing::with_key_projection(config, fence()).unwrap();
    context(&mut routing, 0, 1, ContextState::Desktop);
    for device in [own, pointer(), foreign.clone()] {
        routing
            .attach(device.clone(), &ready(&device), None, ms(0))
            .unwrap();
    }
    routing.actions_available(
        1,
        BTreeSet::from([Capability::SyntheticInput, Capability::Observations]),
        ms(0),
    );
    assert!(!routing.key_orders_source(&source(3)));
    let mut raw = frame(4, 1, 10, 31, 1);
    raw.source = foreign.source.clone();
    routing.push(Record::Frame { frame: raw }).unwrap();
    assert!(routing.due(&foreign.source, ms(10)).unwrap().is_some());
    let (mut seat, writes) = model();
    let work = push(&mut routing, 1, 1, 20, 1).pop().unwrap();
    seat.enqueue(work, ms(20)).unwrap();
    for _ in 0..8 {
        seat.poll(&routing, ms(20)).unwrap();
    }
    assert_eq!(key_events(&writes), [(42, 1)]);
    assert!(routing.due(&foreign.source, ms(20)).unwrap().is_some());
    writes.borrow_mut().steps = (0..32).map(|_| Step::Prefix(0)).collect();
    let end = push(&mut routing, 1, 2, 21, 0).pop().unwrap();
    seat.enqueue(end, ms(21)).unwrap();
    for _ in 0..8 {
        seat.poll(&routing, ms(21)).unwrap();
    }
    let mut release = frame(4, 2, 22, 31, 0);
    release.source = foreign.source.clone();
    routing.push(Record::Frame { frame: release }).unwrap();
    while let Some(raw) = routing.due(&foreign.source, ms(22)).unwrap().cloned() {
        let span = routing.input_span(&foreign.source, raw.ticket).unwrap();
        assert!(!seat.blocks_ordered_span(&routing, &foreign.source, span));
        routing
            .delivered(&foreign.source, raw.ticket, ms(22))
            .unwrap();
    }
    assert_eq!(key_events(&writes), [(42, 1)]);
}

#[test]
fn rule_replacement_rebuilds_domains_only_after_retiring_old_routes_and_work() {
    let mut routing = setup(true, false);
    let work = push(&mut routing, 1, 1, 10, 1).pop().unwrap();
    assert!(routing.key_orders_source(&source(3)));
    let replacement = profile(false, false).rules.remove(1);
    routing
        .transact_rules(
            &crate::rule_transactions::Transaction {
                revision: routing.rule_revision(),
                changes: vec![crate::rule_transactions::Change::Replace {
                    rule: Box::new(replacement),
                }],
            },
            ms(20),
        )
        .unwrap();
    assert!(!work.available(&routing, ms(20)));
    assert!(routing.key_orders_source(&source(3)));
    for mut device in [device(1), device(2), pointer()] {
        device.source.generation += 1;
        routing
            .attach(device.clone(), &ready(&device), None, ms(20))
            .unwrap();
    }
    let mut fresh = source(3);
    fresh.generation += 1;
    assert!(!routing.key_orders_source(&fresh));
    assert!(!work.available(&routing, ms(20)));
}

#[test]
fn pending_touch_without_a_key_dependency_does_not_hold_keyboard_output() {
    use crate::input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, EV_ABS,
    };
    let mut touch = crate::transport::tests::touch();
    touch.source = source(4);
    touch.identity = "ordering-touch".into();
    let mut shift = rule("shift", Action::Remap { key: Key(42) });
    shift.delivery = Delivery::Buffer;
    let mut hold = rule(
        "touch",
        Action::Event {
            name: "touch".into(),
        },
    );
    hold.devices = vec!["touch".into()];
    hold.priority = 10;
    hold.delivery = Delivery::Buffer;
    hold.trigger = Trigger::TouchHold {
        fingers: 1,
        after: Millis(100),
    };
    let mut config = config(vec![shift, hold]);
    config.devices.insert(
        "touch".into(),
        Selector {
            identity: Some(touch.identity.clone()),
            ..Default::default()
        },
    );
    config.policy.allowed_devices.push("touch".into());
    let mut routing = Routing::with_key_projection(config, fence()).unwrap();
    context(&mut routing, 0, 1, ContextState::Desktop);
    let output = crate::gestures::Output {
        name: "test".into(),
        pixel_width: 600.0,
        pixel_height: 1200.0,
        scale: 1.0,
        origin: crate::gestures::Point { x: 0.0, y: 0.0 },
        transform: crate::gestures::Transform::Normal,
    };
    routing
        .authority_mut()
        .update(
            Snapshot {
                generation: 1,
                revision: 2,
                captured_at: ms(0),
                state: ContextState::Desktop,
                outputs: vec![crate::context::OutputState {
                    geometry: output.clone(),
                    geometry_generation: 2,
                    active_workspace: Some(1),
                    screen: crate::config::Screen::On,
                    keyboard_known: true,
                    keyboard: None,
                }],
                windows: vec![],
                regions: vec![],
                secure_overlay: false,
            },
            ms(0),
        )
        .unwrap();
    for device in [device(1), touch.clone()] {
        let mapping = (device.source == touch.source).then(|| {
            let mut mapping = crate::gestures::Mapping::from_device(&device).unwrap();
            mapping.output = Some(output.clone());
            mapping.generation = 2;
            mapping
        });
        routing
            .attach(device.clone(), &ready(&device), mapping, ms(0))
            .unwrap();
    }
    routing.actions_available(
        1,
        BTreeSet::from([Capability::SyntheticInput, Capability::Observations]),
        ms(0),
    );
    let mut down = frame(4, 1, 10, 0, 0);
    down.events = [
        (EV_ABS, ABS_MT_SLOT, 0),
        (EV_ABS, ABS_MT_TRACKING_ID, 1),
        (EV_ABS, ABS_MT_POSITION_X, 100),
        (EV_ABS, ABS_MT_POSITION_Y, 100),
        (EV_KEY, 330, 1),
        (EV_SYN, SYN_REPORT, 0),
    ]
    .into_iter()
    .map(|(kind, code, value)| RawEvent { kind, code, value })
    .collect();
    routing.push(Record::Frame { frame: down }).unwrap();
    assert!(routing.due(&source(4), ms(10)).unwrap().is_none());
    assert!(!routing.key_orders_source(&source(4)));
    let (mut seat, writes) = model();
    let work = push(&mut routing, 1, 1, 20, 1).pop().unwrap();
    seat.enqueue(work, ms(20)).unwrap();
    for _ in 0..8 {
        seat.poll(&routing, ms(20)).unwrap();
    }
    assert_eq!(key_events(&writes), [(42, 1)]);
    assert!(routing.due(&source(4), ms(20)).unwrap().is_none());
}
