use super::*;
use crate::config::Millis;

fn prepared(config: Config, active: bool, scoped: bool) -> Routing {
    if !scoped {
        return routing(config, active);
    }
    let mut routing =
        Routing::with_input_projection(config, crate::keys::worker::tests::fence()).unwrap();
    if active {
        context(&mut routing, 0);
    }
    let device = device();
    let mut mapping = Mapping::from_device(&device).unwrap();
    mapping.output = Some(output());
    mapping.generation = 2;
    routing
        .attach(device.clone(), &ready(&device), Some(mapping), ms(0))
        .unwrap();
    routing
}

fn contact(routing: &mut Routing, sequence: u64, at: u64, fingers: usize) {
    let mut events = vec![(EV_KEY, 0x14a, 1)];
    for slot in 0..fingers {
        events.extend([
            (EV_ABS, ABS_MT_SLOT, slot as i32),
            (EV_ABS, ABS_MT_TRACKING_ID, slot as i32 + 10),
            (EV_ABS, ABS_MT_POSITION_X, 1000 + slot as i32 * 500),
            (EV_ABS, ABS_MT_POSITION_Y, 1000),
        ]);
    }
    frame(routing, sequence, at, &events);
}

#[test]
fn scoped_touch_native_swipe_claims_motion_before_completion_without_replay() {
    for scoped in [false, true] {
        for complete in [false, true] {
            let mut routing = prepared(config(), true, scoped);
            contact(&mut routing, 1, 0, 2);
            movement(&mut routing, 2, 140, 1500, 2);
            assert!(drain(&mut routing, 140).is_empty());
            assert!(routing.take_dispatches(ms(140)).is_empty());
            if complete {
                movement(&mut routing, 3, 180, 2000, 2);
            }
            up(&mut routing, if complete { 4 } else { 3 }, 200, 2);
            assert!(
                drain(&mut routing, 200).is_empty(),
                "scoped={scoped}, complete={complete}"
            );
            assert_eq!(
                routing.take_dispatches(ms(200)).len(),
                usize::from(complete)
            );
        }
    }
}

#[test]
fn scoped_touch_join_wakes_from_the_actual_contact_start_and_keeps_fallback_cadence() {
    for scoped in [false, true] {
        let mut routing = prepared(config(), true, scoped);
        contact(&mut routing, 1, 20, 1);
        assert_eq!(routing.deadline(), Some(ms(140)), "scoped={scoped}");
        routing.push(Record::Tick { at: ms(140) }).unwrap();
        let down = drain(&mut routing, 140);
        assert_eq!(down.len(), 1, "scoped={scoped}");
        assert_eq!(down[0].at, ms(20));
        assert!(routing.deadline().is_none_or(|at| at > ms(140)));
        up(&mut routing, 2, 170, 1);
        assert!(drain(&mut routing, 170).is_empty());
        context(&mut routing, 240);
        routing.push(Record::Tick { at: ms(290) }).unwrap();
        let release = drain(&mut routing, 290);
        assert_eq!(release.len(), 1);
        assert_eq!(release[0].at, ms(170));
        assert!(routing.take_dispatches(ms(290)).is_empty());
    }
}

#[test]
fn scoped_touch_late_join_never_reclaims_an_already_forwarded_contact() {
    for scoped in [false, true] {
        let mut routing = prepared(config(), true, scoped);
        contact(&mut routing, 1, 20, 1);
        routing.push(Record::Tick { at: ms(140) }).unwrap();
        assert_eq!(drain(&mut routing, 140).len(), 1);
        frame(
            &mut routing,
            2,
            150,
            &[
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 11),
                (EV_ABS, ABS_MT_POSITION_X, 1500),
                (EV_ABS, ABS_MT_POSITION_Y, 1000),
            ],
        );
        movement(&mut routing, 3, 170, 2000, 2);
        up(&mut routing, 4, 190, 2);
        context(&mut routing, 200);
        assert_eq!(drain(&mut routing, 270).len(), 1);
        assert_eq!(drain(&mut routing, 290).len(), 1);
        assert_eq!(drain(&mut routing, 310).len(), 1);
        assert!(routing.take_dispatches(ms(310)).is_empty());
    }
}

#[test]
fn scoped_touch_multi_finger_tap_claims_at_join_and_drains_a_later_non_tap() {
    for scoped in [false, true] {
        let mut config = config();
        config.rules[0].trigger = Trigger::TouchTaps {
            fingers: 2,
            count: 1,
        };
        let mut routing = prepared(config, true, scoped);
        contact(&mut routing, 1, 20, 2);
        routing.push(Record::Tick { at: ms(140) }).unwrap();
        assert!(drain(&mut routing, 140).is_empty());
        assert!(routing.take_dispatches(ms(140)).is_empty());
        let router = &routing.routes[&device().source].router;
        if scoped {
            let root = router
                .domain_scope(crate::input::TailDomain::Touch)
                .unwrap();
            assert!(matches!(
                router.domain_disposition(root),
                Some(crate::router::KeyDisposition::Consumed { .. })
            ));
        } else {
            assert_eq!(router.ownership(), Some(Ownership::Claimed));
        }
        movement(&mut routing, 2, 160, 2000, 2);
        up(&mut routing, 3, 180, 2);
        assert!(drain(&mut routing, 180).is_empty(), "scoped={scoped}");
        assert!(routing.take_dispatches(ms(180)).is_empty());
        context(&mut routing, 240);
        routing.push(Record::Tick { at: ms(300) }).unwrap();
        assert!(drain(&mut routing, 300).is_empty());
    }
}

#[test]
fn scoped_touch_finger_competition_and_native_cancellation_emit_at_most_one_action() {
    for scoped in [false, true] {
        let mut config = config();
        config.rules[0].priority = 10;
        let mut three = config.rules[0].clone();
        three.id = "three".into();
        three.priority = 100;
        let Trigger::Swipe { fingers, .. } = &mut three.trigger else {
            unreachable!()
        };
        *fingers = 3;
        three.action = Action::Event {
            name: "three-down".into(),
        };
        config.rules.push(three);
        let mut routing = prepared(config, true, scoped);
        contact(&mut routing, 1, 0, 3);
        movement(&mut routing, 2, 140, 2000, 3);
        up(&mut routing, 3, 180, 3);
        assert!(drain(&mut routing, 180).is_empty());
        let events = routing.take_dispatches(ms(180));
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], Event::Matched { action: Action::Event { name }, .. } if name == "three-down")
        );
    }
}

#[test]
fn scoped_touch_native_claim_outranks_an_unclaimed_higher_priority_candidate() {
    for scoped in [false, true] {
        for taps in [false, true] {
            let mut config = config();
            let mut rival = config.rules[0].clone();
            rival.id = "rival".into();
            rival.priority = 100;
            rival.trigger = if taps {
                Trigger::TouchTaps {
                    fingers: 2,
                    count: 1,
                }
            } else {
                let mut trigger = rival.trigger.clone();
                let Trigger::Swipe { direction, .. } = &mut trigger else {
                    unreachable!()
                };
                *direction = crate::config::Direction::Up;
                trigger
            };
            rival.action = Action::Event {
                name: "rival".into(),
            };
            config.rules.push(rival);
            let mut routing = prepared(config, true, scoped);
            contact(&mut routing, 1, 0, 2);
            movement(&mut routing, 2, 140, 1500, 2);
            assert!(
                drain(&mut routing, 140).is_empty(),
                "scoped={scoped}, taps={taps}"
            );
            movement(&mut routing, 3, 160, 2000, 2);
            up(&mut routing, 4, 180, 2);
            assert!(
                drain(&mut routing, 180).is_empty(),
                "scoped={scoped}, taps={taps}"
            );
            let events = routing.take_dispatches(ms(180));
            assert_eq!(events.len(), 1, "scoped={scoped}, taps={taps}");
            assert!(
                matches!(&events[0], Event::Matched { action: Action::Event { name }, .. } if name == "two-down")
            );
        }
    }
}

#[test]
fn scoped_touch_contact_tap_rival_without_motion_keeps_priority() {
    for scoped in [false, true] {
        let mut config = config();
        let mut rival = config.rules[0].clone();
        rival.id = "rival".into();
        rival.priority = 100;
        rival.trigger = Trigger::TouchTaps {
            fingers: 2,
            count: 1,
        };
        rival.action = Action::Event {
            name: "rival".into(),
        };
        config.rules.push(rival);
        let mut routing = prepared(config, true, scoped);
        contact(&mut routing, 1, 0, 2);
        routing.push(Record::Tick { at: ms(120) }).unwrap();
        up(&mut routing, 2, 150, 2);
        assert!(drain(&mut routing, 150).is_empty(), "scoped={scoped}");
        context(&mut routing, 240);
        routing.push(Record::Tick { at: ms(450) }).unwrap();
        assert!(drain(&mut routing, 450).is_empty(), "scoped={scoped}");
        let events = routing.take_dispatches(ms(450));
        assert_eq!(events.len(), 1, "scoped={scoped}");
        assert!(
            matches!(&events[0], Event::Matched { action: Action::Event { name }, .. } if name == "rival")
        );
    }
}

#[test]
fn scoped_touch_unknown_context_is_ordinary_and_lost_context_revokes_queued_actions() {
    for scoped in [false, true] {
        for active in [false, true] {
            let mut routing = prepared(config(), active, scoped);
            contact(&mut routing, 1, 0, 2);
            if !active {
                assert_eq!(drain(&mut routing, 0).len(), 1);
            }
            movement(&mut routing, 2, 140, 2000, 2);
            if !active {
                assert_eq!(drain(&mut routing, 140).len(), 1);
            }
            up(&mut routing, 3, 180, 2);
            if !active {
                assert_eq!(drain(&mut routing, 180).len(), 1);
            }
            routing.authority_mut().active_changed(false);
            assert!(routing.take_dispatches(ms(180)).is_empty());
            routing.push(Record::Tick { at: ms(181) }).unwrap();
            if active {
                assert_eq!(routing.take_recoveries().len(), 1);
            }
        }
    }
}

#[test]
fn scoped_touch_join_is_shorter_than_the_decision_budget() {
    let mut config = config();
    config.timing.join_window = Millis(120);
    config.timing.decision_window = Millis(120);
    assert!(Routing::with_input_projection(config, crate::keys::worker::tests::fence()).is_err());
}

#[test]
fn scoped_touch_custom_begin_before_join_waits_and_advertises_the_original_join_cutpoint() {
    for scoped in [false, true] {
        let mut config = config();
        config.policy.allow_input_events = true;
        config.timing.join_window = Millis(30);
        config.timing.decision_window = Millis(100);
        config.rules[0].trigger = Trigger::Custom {
            recognizer: crate::recognition::Definition {
                name: "early-touch".into(),
                selectors: BTreeSet::from(["touch".into()]),
                inputs: BTreeSet::from([crate::recognition::InputKind::Touch]),
                reply_within: Millis(100),
                continuous: true,
            },
            timeout: Millis(1000),
            wake: None,
        };
        let mut routing = if scoped {
            prepared(config, true, true)
        } else {
            let mut routing =
                Routing::with_recognition(config, crate::keys::worker::tests::fence()).unwrap();
            context(&mut routing, 0);
            let mut mapping = Mapping::from_device(&device()).unwrap();
            mapping.output = Some(output());
            mapping.generation = 2;
            routing
                .attach(device(), &ready(&device()), Some(mapping), ms(0))
                .unwrap();
            routing
        };
        contact(&mut routing, 1, 0, 2);
        let ticket = routing.recognition_requests(ms(1)).unwrap().pop().unwrap();
        routing
            .recognition_response(
                crate::recognition::Proposal {
                    fence: ticket.fence,
                    ticket: ticket.id,
                    phase: crate::recognition::Phase::Begin,
                    data: serde_json::json!({}),
                },
                ms(10),
            )
            .unwrap();
        assert!(
            routing.take_dispatches(ms(10)).is_empty(),
            "scoped={scoped}"
        );
        assert_eq!(routing.deadline(), Some(ms(30)), "scoped={scoped}");
        routing.push(Record::Tick { at: ms(30) }).unwrap();
        assert_eq!(routing.take_dispatches(ms(30)).len(), 1);
        assert!(drain(&mut routing, 30).is_empty());
        assert!(routing.deadline().is_none_or(|at| at > ms(30)));
    }
}

fn attach_device(config: Config, scoped: bool, device: Device) -> Routing {
    let mut routing = if scoped {
        Routing::with_input_projection(config, crate::keys::worker::tests::fence()).unwrap()
    } else {
        Routing::new(config).unwrap()
    };
    context(&mut routing, 0);
    let mut mapping = Mapping::from_device(&device).unwrap();
    mapping.output = Some(output());
    mapping.generation = 2;
    routing
        .attach(device.clone(), &ready(&device), Some(mapping), ms(0))
        .unwrap();
    routing
}

fn coupled_device() -> Device {
    let mut device = device();
    let codes = device.capabilities.get_mut(&EV_ABS).unwrap();
    codes.extend([0, 1]);
    codes.sort_unstable();
    for code in [0, 1] {
        device.axes.insert(code, device.axes[&ABS_MT_POSITION_X]);
    }
    assert!(crate::input::coupled_absolute(&device));
    device
}

#[test]
fn scoped_touch_consumes_coupled_scalar_emulation_with_its_contacts() {
    for scoped in [false, true] {
        for complete in [false, true] {
            let mut routing = attach_device(config(), scoped, coupled_device());
            let mut down = vec![(EV_KEY, 0x14a, 1)];
            for slot in 0..2 {
                down.extend([
                    (EV_ABS, ABS_MT_SLOT, slot),
                    (EV_ABS, ABS_MT_TRACKING_ID, slot + 10),
                    (EV_ABS, ABS_MT_POSITION_X, 1000 + slot * 500),
                    (EV_ABS, ABS_MT_POSITION_Y, 1000),
                ]);
            }
            down.extend([(EV_ABS, 0, 1000), (EV_ABS, 1, 1000)]);
            frame(&mut routing, 1, 0, &down);
            let mut sequence = 2;
            for (at, y) in [(140, 1500), (180, 2000)]
                .into_iter()
                .take(1 + usize::from(complete))
            {
                frame(
                    &mut routing,
                    sequence,
                    at,
                    &[
                        (EV_ABS, ABS_MT_SLOT, 0),
                        (EV_ABS, ABS_MT_POSITION_Y, y),
                        (EV_ABS, ABS_MT_SLOT, 1),
                        (EV_ABS, ABS_MT_POSITION_Y, y),
                        (EV_ABS, 1, y),
                    ],
                );
                sequence += 1;
                assert!(drain(&mut routing, at).is_empty(), "scoped={scoped}");
            }
            context(&mut routing, 240);
            frame(&mut routing, sequence, 320, &[(EV_ABS, 1, 2100)]);
            up(&mut routing, sequence + 1, 330, 2);
            assert_eq!(
                routing.take_dispatches(ms(330)).len(),
                usize::from(complete),
                "scoped={scoped}, complete={complete}"
            );
            assert!(
                drain(&mut routing, 470).is_empty(),
                "scoped={scoped}, complete={complete}"
            );
        }
    }
}

#[test]
fn scoped_touch_unclaimed_cohort_keeps_coupled_scalar_emulation_ordinary() {
    for scoped in [false, true] {
        let mut config = config();
        config.rules[0].contexts = vec![crate::config::Context::Locked];
        let mut routing = attach_device(config, scoped, coupled_device());
        frame(
            &mut routing,
            1,
            0,
            &[
                (EV_KEY, 0x14a, 1),
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 10),
                (EV_ABS, ABS_MT_POSITION_X, 1000),
                (EV_ABS, ABS_MT_POSITION_Y, 1000),
                (EV_ABS, 0, 1000),
                (EV_ABS, 1, 1000),
            ],
        );
        assert_eq!(drain(&mut routing, 0).len(), 1, "scoped={scoped}");
        frame(&mut routing, 2, 10, &[(EV_ABS, 1, 1100)]);
        let scalar = drain(&mut routing, 10);
        assert_eq!(scalar.len(), 1, "scoped={scoped}");
        assert!(scalar[0].events.contains(&RawEvent {
            kind: EV_ABS,
            code: 1,
            value: 1100
        }));
    }
}

#[test]
fn scoped_contact_samples_cannot_authorize_an_unrelated_relative_claim() {
    use crate::recognition::InputKind;
    for contacts in [false, true] {
        let mut config = config();
        config.policy.allow_input_events = true;
        let mut inputs = BTreeSet::from([InputKind::Relative]);
        if contacts {
            inputs.insert(InputKind::Absolute);
        }
        config.rules[0].trigger = Trigger::Custom {
            recognizer: crate::recognition::Definition {
                name: "pointer".into(),
                selectors: BTreeSet::from(["touch".into()]),
                inputs,
                reply_within: Millis(100),
                continuous: false,
            },
            timeout: Millis(1000),
            wake: None,
        };
        let mut device = device();
        device.capabilities.insert(crate::input::EV_REL, vec![0, 1]);
        let mut routing = attach_device(config, true, device);
        frame(
            &mut routing,
            1,
            0,
            &[
                (EV_KEY, 0x14a, 1),
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 10),
                (EV_ABS, ABS_MT_POSITION_X, 1000),
                (EV_ABS, ABS_MT_POSITION_Y, 1000),
                (crate::input::EV_REL, 0, 5),
            ],
        );
        let ordinary = drain(&mut routing, 0);
        if contacts {
            assert_eq!(ordinary.len(), 1);
            assert!(ordinary[0].events.contains(&RawEvent {
                kind: crate::input::EV_REL,
                code: 0,
                value: 5
            }));
        } else {
            assert!(ordinary.is_empty(), "a declared relative report waits");
        }
    }
}

#[test]
fn scoped_touch_continuous_pan_keeps_ownership_past_the_decision_window() {
    for scoped in [false, true] {
        let mut config = config();
        let Trigger::Swipe { continuous, .. } = &mut config.rules[0].trigger else {
            unreachable!()
        };
        *continuous = true;
        let mut routing = prepared(config, true, scoped);
        contact(&mut routing, 1, 0, 2);
        let mut updates = Vec::new();
        for (sequence, at, y) in [
            (2, 140, 1500),
            (3, 200, 2000),
            (4, 260, 2300),
            (5, 340, 2600),
            (6, 400, 2900),
        ] {
            if at == 260 {
                context(&mut routing, 240);
            }
            movement(&mut routing, sequence, at, y, 2);
            assert!(
                drain(&mut routing, at).is_empty(),
                "scoped={scoped}, at={at}"
            );
            updates.push(routing.take_dispatches(ms(at)).len());
        }
        up(&mut routing, 7, 420, 2);
        assert!(drain(&mut routing, 420).is_empty(), "scoped={scoped}");
        assert!(
            updates[2..].iter().all(|count| *count > 0),
            "scoped={scoped}, updates={updates:?}"
        );
    }
}

fn touch_sequence(steps: Vec<Trigger>, scoped: bool) -> Routing {
    let mut config = config();
    config.timing.tap_gap = Millis(20);
    config.rules[0].scope = crate::config::Scope::Everywhere;
    config.rules[0].trigger = Trigger::Sequence {
        steps,
        timeout: Millis(1000),
    };
    let mut device = device();
    let keys = device.capabilities.entry(EV_KEY).or_default();
    keys.push(48);
    keys.sort_unstable();
    keys.dedup();
    attach_device(config, scoped, device)
}

#[test]
fn scoped_touch_sequence_waits_for_the_join_window_of_its_latest_group() {
    for scoped in [false, true] {
        let mut routing = touch_sequence(
            vec![
                Trigger::TouchTaps {
                    fingers: 2,
                    count: 1,
                },
                Trigger::ButtonHold {
                    key: crate::config::Key(48),
                    after: Millis(100),
                    repeat: None,
                },
            ],
            scoped,
        );
        contact(&mut routing, 1, 0, 2);
        up(&mut routing, 2, 10, 2);
        routing.push(Record::Tick { at: ms(30) }).unwrap();
        frame(&mut routing, 3, 40, &[(EV_KEY, 48, 1)]);
        routing.push(Record::Tick { at: ms(140) }).unwrap();
        assert!(
            routing.take_dispatches(ms(140)).is_empty(),
            "scoped={scoped}"
        );
        if scoped {
            assert_eq!(routing.deadline(), Some(ms(160)));
        }
        routing.push(Record::Tick { at: ms(160) }).unwrap();
        assert_eq!(routing.take_dispatches(ms(160)).len(), 1, "scoped={scoped}");
    }
}

#[test]
fn scoped_touch_continuous_sequence_dispatches_every_stage_without_leaking_contacts() {
    for scoped in [false, true] {
        let mut routing = touch_sequence(
            vec![
                Trigger::TouchTaps {
                    fingers: 2,
                    count: 1,
                },
                Trigger::Swipe {
                    fingers: 2,
                    direction: crate::config::Direction::Down,
                    travel: crate::config::Distance::Millimetres(12.0),
                    fallback_travel: crate::config::Distance::Normalized(0.04),
                    continuous: true,
                },
            ],
            scoped,
        );
        contact(&mut routing, 1, 0, 2);
        up(&mut routing, 2, 10, 2);
        routing.push(Record::Tick { at: ms(30) }).unwrap();
        frame(
            &mut routing,
            3,
            40,
            &[
                (EV_KEY, 0x14a, 1),
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 30),
                (EV_ABS, ABS_MT_POSITION_X, 1000),
                (EV_ABS, ABS_MT_POSITION_Y, 1000),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 31),
                (EV_ABS, ABS_MT_POSITION_X, 1500),
                (EV_ABS, ABS_MT_POSITION_Y, 1000),
            ],
        );
        movement(&mut routing, 4, 180, 3000, 2);
        movement(&mut routing, 5, 190, 3500, 2);
        up(&mut routing, 6, 200, 2);
        let events = routing.take_dispatches(ms(210));
        assert_eq!(
            events
                .iter()
                .map(|event| match event {
                    Event::Matched { stage, .. } => Some(*stage),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [
                crate::rules::Stage::Begin,
                crate::rules::Stage::Update,
                crate::rules::Stage::End
            ]
            .map(Some),
            "scoped={scoped}"
        );
        assert!(drain(&mut routing, 210).is_empty(), "scoped={scoped}");
    }
}

#[test]
fn scoped_touch_reload_while_held_releases_only_what_the_consumer_saw() {
    for scoped in [false, true] {
        for case in ["ordinary", "claimed", "completed"] {
            let mut routing = prepared(config(), true, scoped);
            if case == "ordinary" {
                contact(&mut routing, 1, 20, 1);
                routing.push(Record::Tick { at: ms(140) }).unwrap();
                assert_eq!(drain(&mut routing, 140).len(), 1, "scoped={scoped}");
            } else {
                contact(&mut routing, 1, 0, 2);
                movement(&mut routing, 2, 140, 1500, 2);
                if case == "completed" {
                    movement(&mut routing, 3, 145, 2000, 2);
                    up(&mut routing, 4, 148, 2);
                }
                assert!(drain(&mut routing, 148).is_empty(), "scoped={scoped}");
                assert_eq!(
                    routing.dispatches.len(),
                    usize::from(case == "completed"),
                    "scoped={scoped}, {case}"
                );
            }
            routing.reload(config(), ms(150)).unwrap();
            let recoveries = routing.take_recoveries();
            assert_eq!(recoveries.len(), 1, "scoped={scoped}, {case}");
            let releases = &recoveries[0].neutral_events;
            if case == "ordinary" {
                assert!(
                    releases.iter().any(|event| event.kind == EV_ABS
                        && event.code == ABS_MT_TRACKING_ID
                        && event.value == -1),
                    "scoped={scoped}: {releases:?}"
                );
                assert!(
                    releases.iter().any(|event| event.kind == EV_KEY
                        && event.code == 0x14a
                        && event.value == 0),
                    "scoped={scoped}: {releases:?}"
                );
            } else {
                assert!(releases.is_empty(), "scoped={scoped}, {case}: {releases:?}");
            }
            assert!(
                routing.take_dispatches(ms(150)).is_empty(),
                "scoped={scoped}, {case}"
            );
            assert!(routing.due(&device().source, ms(150)).is_err());
        }
    }
}

#[test]
fn scoped_touch_device_loss_and_lock_mid_gesture_release_only_what_the_consumer_saw() {
    for scoped in [false, true] {
        for trigger in ["remove", "lock"] {
            for claimed in [false, true] {
                let label = format!("scoped={scoped}, {trigger}, claimed={claimed}");
                let mut routing = prepared(config(), true, scoped);
                if claimed {
                    contact(&mut routing, 1, 0, 2);
                    movement(&mut routing, 2, 140, 1500, 2);
                    assert!(drain(&mut routing, 140).is_empty(), "{label}");
                } else {
                    contact(&mut routing, 1, 20, 1);
                    routing.push(Record::Tick { at: ms(140) }).unwrap();
                    assert_eq!(drain(&mut routing, 140).len(), 1, "{label}");
                }
                if trigger == "remove" {
                    routing
                        .push(Record::Remove {
                            source: device().source,
                            at: ms(150),
                        })
                        .unwrap();
                } else {
                    routing.authority_mut().lock_requested();
                    routing.push(Record::Tick { at: ms(150) }).unwrap();
                }
                let recoveries = routing.take_recoveries();
                assert_eq!(recoveries.len(), 1, "{label}");
                let releases = &recoveries[0].neutral_events;
                assert_eq!(
                    releases.iter().any(|event| event.kind == EV_ABS
                        && event.code == ABS_MT_TRACKING_ID
                        && event.value == -1),
                    !claimed,
                    "{label}: {releases:?}"
                );
                assert!(routing.take_dispatches(ms(150)).is_empty(), "{label}");
                assert!(routing.due(&device().source, ms(150)).is_err(), "{label}");
            }
        }
    }
}

#[test]
fn an_edge_region_consumes_a_swipe_from_its_strip_and_passes_one_from_mid_screen() {
    for scoped in [false, true] {
        for (start, consumed) in [(4080, true), (2000, false)] {
            let mut config = config();
            let rule = &mut config.rules[0];
            rule.trigger = Trigger::Swipe {
                fingers: 1,
                direction: Direction::Up,
                travel: Distance::Millimetres(40.0),
                fallback_travel: Distance::Normalized(0.2),
                continuous: false,
            };
            rule.region = Some(crate::config::Region {
                edge: crate::config::Edge::Bottom,
                depth: Distance::Pixels(20.0),
                from: 0.0,
                to: 1.0,
            });
            let mut routing = prepared(config, true, scoped);
            let label = format!("scoped={scoped}, start={start}");
            frame(
                &mut routing,
                1,
                0,
                &[
                    (EV_KEY, 0x14a, 1),
                    (EV_ABS, ABS_MT_SLOT, 0),
                    (EV_ABS, ABS_MT_TRACKING_ID, 10),
                    (EV_ABS, ABS_MT_POSITION_X, 1000),
                    (EV_ABS, ABS_MT_POSITION_Y, start),
                ],
            );
            let mut forwarded = drain(&mut routing, 0);
            assert_eq!(forwarded.len(), usize::from(!consumed), "{label}");
            routing.push(Record::Tick { at: ms(120) }).unwrap();
            forwarded.extend(drain(&mut routing, 120));
            movement(&mut routing, 2, 140, start - 1100, 1);
            forwarded.extend(drain(&mut routing, 140));
            up(&mut routing, 3, 200, 1);
            forwarded.extend(drain(&mut routing, 200));
            let dispatches = routing.take_dispatches(ms(200));
            if consumed {
                assert!(forwarded.is_empty(), "{label}: {forwarded:?}");
                assert_eq!(dispatches.len(), 1, "{label}");
            } else {
                assert_eq!(forwarded.len(), 3, "{label}");
                assert!(dispatches.is_empty(), "{label}");
            }
        }
    }
}
