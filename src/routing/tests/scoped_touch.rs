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
