//! Native coordinator execution with explicit context/readiness models. These
//! fixtures neither open evdev/uinput nor establish kernel routing acceptance.
use super::*;
use crate::{
    config::{Millis, Mode},
    protocol::{Epoch, Fence},
    recognition::{Cause, Definition, InputKind, Phase, Proposal, Retirement, Ticket},
    rules::{Evidence, Stage},
};

fn fence() -> Fence {
    Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    }
}
fn profile(delivery: Delivery, continuous: bool, touch: bool) -> Config {
    let mut profile = config();
    profile.policy.allow_input_events = true;
    profile.timing.join_window = Millis(5);
    profile.timing.decision_window = Millis(40);
    let rule = &mut profile.rules[0];
    rule.id = "custom".into();
    rule.scope = crate::config::Scope::Everywhere;
    rule.delivery = delivery;
    rule.trigger = Trigger::Custom {
        recognizer: Definition {
            name: "custom".into(),
            selectors: BTreeSet::from(["touch".into()]),
            inputs: BTreeSet::from([if touch {
                InputKind::Touch
            } else {
                InputKind::Key
            }]),
            reply_within: Millis(250),
            continuous,
        },
        timeout: Millis(1000),
        wake: Some(Millis(20)),
    };
    profile
}
fn keyboard() -> Device {
    let mut keyboard = device();
    keyboard.axes.clear();
    keyboard.properties.clear();
    keyboard.capabilities = BTreeMap::from([(EV_SYN, vec![SYN_REPORT]), (EV_KEY, vec![30, 48])]);
    keyboard
}
fn custom(profile: Config, touch: bool) -> Routing {
    let mut routing = Routing::with_recognition(profile, fence()).unwrap();
    context(&mut routing, 0);
    let device = if touch { device() } else { keyboard() };
    let mapping = touch.then(|| {
        let mut mapping = Mapping::from_device(&device).unwrap();
        mapping.output = Some(output());
        mapping.generation = 2;
        mapping
    });
    routing
        .attach(device.clone(), &ready(&device), mapping, ms(0))
        .unwrap();
    routing
}
fn reply(ticket: &Ticket, phase: Phase) -> Proposal {
    Proposal {
        fence: ticket.fence,
        ticket: ticket.id,
        phase,
        data: serde_json::json!({"native":"validated"}),
    }
}
fn press(routing: &mut Routing, sequence: u64, at: u64, down: bool) {
    frame(routing, sequence, at, &[(EV_KEY, 30, i32::from(down))]);
}
fn request(routing: &mut Routing, at: u64) -> Ticket {
    let mut tickets = routing.recognition_requests(ms(at)).unwrap();
    assert_eq!(tickets.len(), 1);
    tickets.pop().unwrap()
}

#[test]
fn custom_trigger_requires_coordinator_and_explicit_bounded_policy() {
    let profile = profile(Delivery::Observe, false, false);
    assert!(profile.validate().is_ok());
    assert!(
        Engine::new(profile.clone())
            .err()
            .unwrap()
            .to_string()
            .contains("coordinator")
    );
    for case in [
        "policy",
        "selector",
        "wake",
        "timeout",
        "sequence",
        "duplicate",
        "ungrouped",
    ] {
        let mut bad = profile.clone();
        match case {
            "policy" => bad.policy.allow_input_events = false,
            "selector" => {
                if let Trigger::Custom { recognizer, .. } = &mut bad.rules[0].trigger {
                    recognizer.selectors.insert("other".into());
                }
            }
            "wake" => {
                if let Trigger::Custom { wake, .. } = &mut bad.rules[0].trigger {
                    *wake = Some(Millis(1));
                }
            }
            "timeout" => {
                if let Trigger::Custom { timeout, .. } = &mut bad.rules[0].trigger {
                    *timeout = Millis(10001);
                }
            }
            "sequence" => {
                bad.rules[0].trigger = Trigger::Sequence {
                    steps: vec![
                        bad.rules[0].trigger.clone(),
                        Trigger::Key {
                            key: crate::config::Key(30),
                        },
                    ],
                    timeout: Millis(100),
                };
            }
            "duplicate" => {
                let mut other = bad.rules[0].clone();
                other.id = "other".into();
                other.priority += 1;
                if let Trigger::Custom { recognizer, .. } = &mut other.trigger {
                    recognizer.continuous = true;
                }
                bad.rules.push(other);
            }
            "ungrouped" => {
                bad.rules[0].delivery = Delivery::Buffer;
                if let Trigger::Custom { recognizer, .. } = &mut bad.rules[0].trigger {
                    recognizer.continuous = true;
                    recognizer.inputs = BTreeSet::from([InputKind::Wheel]);
                }
            }
            _ => unreachable!(),
        }
        assert!(bad.validate().is_err(), "{case}");
    }
}

#[test]
fn native_custom_observation_preserves_raw_input_and_emits_typed_evidence() {
    let mut routing = custom(profile(Delivery::Observe, false, false), false);
    press(&mut routing, 1, 0, true);
    let ticket = request(&mut routing, 0);
    routing
        .recognition_response(reply(&ticket, Phase::Discrete), ms(1))
        .unwrap();
    let events = routing.take_dispatches(ms(1));
    assert!(
        matches!(&events[..],[Event::Matched{ordinal:1,stage:Stage::Discrete,evidence:Evidence::Custom{recognizer,data},..}] if recognizer=="custom" && data["native"]=="validated")
    );
    press(&mut routing, 2, 2, false);
    assert_eq!(drain(&mut routing, 3).len(), 2);
    assert_eq!(
        routing.take_recognition_cancellations()[0].reason,
        Retirement::Completed
    );
}

#[test]
fn stationary_native_hold_uses_original_time_and_real_router_ownership() {
    let mut routing = custom(profile(Delivery::Reserve, false, false), false);
    press(&mut routing, 1, 0, true);
    let first = request(&mut routing, 0);
    routing
        .recognition_response(reply(&first, Phase::Pending), ms(1))
        .unwrap();
    assert_eq!(
        routing.routes[&keyboard().source].router.ownership(),
        Some(Ownership::Claimed)
    );
    let timer = request(&mut routing, 20);
    assert_eq!(timer.cause, Cause::Timer);
    assert_eq!(timer.requested_at, ms(20));
    assert_eq!(timer.input.at(), ms(0));
    routing
        .recognition_response(reply(&timer, Phase::Discrete), ms(21))
        .unwrap();
    assert_eq!(routing.take_dispatches(ms(21)).len(), 1);
    press(&mut routing, 2, 25, false);
    assert!(drain(&mut routing, 26).is_empty());
}

#[test]
fn unresponsive_recognizer_cancels_and_drains_an_already_claimed_group() {
    let mut profile = profile(Delivery::Reserve, true, false);
    if let Trigger::Custom { recognizer, .. } = &mut profile.rules[0].trigger {
        recognizer.reply_within = Millis(20);
    }
    let mut routing = custom(profile, false);
    press(&mut routing, 1, 0, true);
    let first = request(&mut routing, 0);
    routing
        .recognition_response(reply(&first, Phase::Begin), ms(1))
        .unwrap();
    routing.take_dispatches(ms(1));
    frame(&mut routing, 2, 5, &[(EV_KEY, 30, 2)]);
    let unanswered = request(&mut routing, 5);
    routing.push(Record::Tick { at: ms(26) }).unwrap();
    assert_eq!(
        routing.routes[&keyboard().source].router.ownership(),
        Some(Ownership::Draining)
    );
    assert!(
        routing
            .recognition_response(reply(&unanswered, Phase::Update), ms(27))
            .is_err()
    );
    assert_eq!(
        routing.take_recognition_cancellations()[0].reason,
        Retirement::Expired
    );
    press(&mut routing, 3, 30, false);
    assert!(drain(&mut routing, 31).is_empty());
    assert_eq!(routing.routes[&keyboard().source].router.ownership(), None);
}

#[test]
fn native_offer_overflow_retires_the_candidate_without_issuing_partial_work() {
    let mut routing = custom(profile(Delivery::Reserve, false, false), false);
    press(&mut routing, 1, 0, true);
    for sequence in 2..=65 {
        frame(&mut routing, sequence, 0, &[(EV_KEY, 30, 2)]);
    }
    assert!(routing.recognition_requests(ms(0)).unwrap().is_empty());
    assert!(routing.take_events().iter().any(
        |event| matches!(event,Event::Cancelled{reason,..} if reason.contains("queue exhausted"))
    ));
    press(&mut routing, 66, 1, false);
    assert!(drain(&mut routing, 2).is_empty());
}

#[test]
fn continuous_custom_end_keeps_its_claim_through_physical_release_only() {
    let mut routing = custom(profile(Delivery::Reserve, true, false), false);
    press(&mut routing, 1, 0, true);
    let first = request(&mut routing, 0);
    routing
        .recognition_response(reply(&first, Phase::Begin), ms(1))
        .unwrap();
    assert_eq!(routing.take_dispatches(ms(1)).len(), 1);
    press(&mut routing, 2, 5, false);
    let source = keyboard().source;
    assert!(routing.routes[&source].router.physical_neutral());
    assert_eq!(
        routing.routes[&source].router.ownership(),
        Some(Ownership::Claimed)
    );
    let end = request(&mut routing, 5);
    routing
        .recognition_response(reply(&end, Phase::End), ms(6))
        .unwrap();
    assert!(matches!(
        &routing.take_dispatches(ms(6))[..],
        [Event::Matched {
            stage: Stage::End,
            ordinal: 2,
            ..
        }]
    ));
    assert_eq!(routing.routes[&source].router.ownership(), None);
    assert!(drain(&mut routing, 6).is_empty());
    press(&mut routing, 3, 7, true);
    let fresh = request(&mut routing, 7);
    assert_ne!(first.candidate, fresh.candidate);
    assert!(
        routing
            .recognition_response(reply(&end, Phase::End), ms(8))
            .is_err()
    );
    assert!(routing.take_dispatches(ms(8)).is_empty());
}

#[test]
fn a_new_physical_group_retires_an_unanswered_custom_end() {
    let mut routing = custom(profile(Delivery::Reserve, true, false), false);
    press(&mut routing, 1, 0, true);
    let first = request(&mut routing, 0);
    routing
        .recognition_response(reply(&first, Phase::Begin), ms(1))
        .unwrap();
    routing.take_dispatches(ms(1));
    press(&mut routing, 2, 5, false);
    let old = request(&mut routing, 5);
    press(&mut routing, 3, 6, true);
    let fresh = request(&mut routing, 6);
    assert_ne!(old.candidate, fresh.candidate);
    assert!(
        routing
            .recognition_response(reply(&old, Phase::End), ms(7))
            .is_err()
    );
    routing
        .recognition_response(reply(&fresh, Phase::Begin), ms(8))
        .unwrap();
    assert!(
        matches!(&routing.take_dispatches(ms(8))[..],[Event::Matched{id,ordinal:1,..}] if *id==fresh.candidate)
    );
    assert!(
        routing
            .take_recognition_cancellations()
            .iter()
            .any(|cancel| cancel.candidate == old.candidate)
    );
}

#[test]
fn unrelated_input_is_not_consumed_by_a_custom_group_waiting_for_end() {
    let profile = profile(Delivery::Reserve, true, false);
    let mut routing = Routing::with_recognition(profile, fence()).unwrap();
    context(&mut routing, 0);
    let mut input = keyboard();
    input.capabilities.insert(crate::input::EV_REL, vec![0]);
    routing
        .attach(input.clone(), &ready(&input), None, ms(0))
        .unwrap();
    press(&mut routing, 1, 0, true);
    let first = request(&mut routing, 0);
    routing
        .recognition_response(reply(&first, Phase::Begin), ms(1))
        .unwrap();
    routing.take_dispatches(ms(1));
    press(&mut routing, 2, 5, false);
    let old = request(&mut routing, 5);
    frame(&mut routing, 3, 6, &[(crate::input::EV_REL, 0, 10)]);
    let delivered = drain(&mut routing, 6);
    assert_eq!(delivered.len(), 1);
    assert!(
        delivered[0]
            .events
            .iter()
            .any(|event| event.kind == crate::input::EV_REL && event.value == 10)
    );
    assert!(
        routing
            .recognition_response(reply(&old, Phase::End), ms(7))
            .is_err()
    );
    assert!(routing.take_dispatches(ms(7)).is_empty());
}

#[test]
fn a_forwarded_stream_cannot_be_claimed_by_a_still_fresh_reply() {
    let mut routing = custom(profile(Delivery::Buffer, false, false), false);
    press(&mut routing, 1, 0, true);
    let ticket = request(&mut routing, 0);
    routing.push(Record::Tick { at: ms(41) }).unwrap();
    assert_eq!(
        routing.routes[&keyboard().source].router.ownership(),
        Some(Ownership::Forwarded)
    );
    assert!(ticket.deadline > ms(42));
    assert!(
        routing
            .recognition_response(reply(&ticket, Phase::Discrete), ms(42))
            .is_err()
    );
    assert_eq!(drain(&mut routing, 42).len(), 1);
    press(&mut routing, 2, 50, false);
    assert_eq!(drain(&mut routing, 91).len(), 1);
    assert!(routing.take_dispatches(ms(91)).is_empty());
}

fn competition(continuous: bool) -> Routing {
    let mut profile = profile(Delivery::Buffer, continuous, false);
    profile.rules[0].id = "high".into();
    profile.rules[0].priority = 10;
    profile.rules[0].action = Action::Event {
        name: "high".into(),
    };
    let mut low = profile.rules[0].clone();
    low.id = "low".into();
    low.priority = 0;
    low.action = Action::Event { name: "low".into() };
    profile.rules.push(low);
    custom(profile, false)
}
fn pair(routing: &mut Routing, at: u64) -> (Ticket, Ticket) {
    let tickets = routing.recognition_requests(ms(at)).unwrap();
    assert_eq!(tickets.len(), 2);
    (
        tickets
            .iter()
            .find(|ticket| ticket.rule == "high")
            .unwrap()
            .clone(),
        tickets
            .iter()
            .find(|ticket| ticket.rule == "low")
            .unwrap()
            .clone(),
    )
}
#[test]
fn lower_priority_results_wait_for_the_existing_higher_priority_candidate() {
    for high_matches in [true, false] {
        let mut routing = competition(false);
        press(&mut routing, 1, 0, true);
        let (high, low) = pair(&mut routing, 0);
        routing
            .recognition_response(reply(&low, Phase::Discrete), ms(1))
            .unwrap();
        assert!(routing.take_dispatches(ms(1)).is_empty());
        assert_eq!(routing.deferred_custom.len(), 1);
        routing
            .recognition_response(
                reply(
                    &high,
                    if high_matches {
                        Phase::Discrete
                    } else {
                        Phase::Cancel
                    },
                ),
                ms(2),
            )
            .unwrap();
        let expected = if high_matches { "high" } else { "low" };
        assert!(
            matches!(&routing.take_dispatches(ms(2))[..],[Event::Matched{rule,..}] if rule==expected)
        );
        assert!(routing.deferred_custom.is_empty());
        press(&mut routing, 2, 5, false);
        assert!(drain(&mut routing, 6).is_empty());
    }
}

#[test]
fn deferred_progress_preserves_begin_latest_update_and_end_order() {
    let mut routing = competition(true);
    press(&mut routing, 1, 0, true);
    let (high, low) = pair(&mut routing, 0);
    routing
        .recognition_response(reply(&high, Phase::Pending), ms(1))
        .unwrap();
    routing
        .recognition_response(reply(&low, Phase::Begin), ms(2))
        .unwrap();
    for (sequence, at) in [(2, 5), (3, 10)] {
        frame(&mut routing, sequence, at, &[(EV_KEY, 30, 2)]);
        let (high, low) = pair(&mut routing, at);
        routing
            .recognition_response(reply(&high, Phase::Pending), ms(at + 1))
            .unwrap();
        routing
            .recognition_response(reply(&low, Phase::Update), ms(at + 2))
            .unwrap();
    }
    assert_eq!(routing.deferred_custom.len(), 2);
    assert!(routing.take_dispatches(ms(12)).is_empty());
    let (high, low) = pair(&mut routing, 20);
    routing
        .recognition_response(reply(&high, Phase::Cancel), ms(21))
        .unwrap();
    assert!(matches!(
        &routing.take_dispatches(ms(21))[..],
        [
            Event::Matched {
                stage: Stage::Begin,
                ordinal: 1,
                ..
            },
            Event::Matched {
                stage: Stage::Update,
                ordinal: 3,
                ..
            }
        ]
    ));
    routing
        .recognition_response(reply(&low, Phase::Update), ms(22))
        .unwrap();
    assert!(matches!(
        &routing.take_dispatches(ms(22))[..],
        [Event::Matched {
            stage: Stage::Update,
            ordinal: 4,
            ..
        }]
    ));
    press(&mut routing, 4, 25, false);
    let end = request(&mut routing, 25);
    routing
        .recognition_response(reply(&end, Phase::End), ms(26))
        .unwrap();
    assert!(matches!(
        &routing.take_dispatches(ms(26))[..],
        [Event::Matched {
            stage: Stage::End,
            ordinal: 5,
            ..
        }]
    ));
    assert!(drain(&mut routing, 26).is_empty());
}

#[test]
fn reply_and_deferred_deadlines_are_not_extended_by_arbitration() {
    let mut routing = competition(false);
    // Leave the higher candidate pending but give both declarations 20 ms.
    // Rebuild through the protected model, rather than changing live state.
    let mut profile = routing.engine.config().clone();
    for rule in &mut profile.rules {
        if let Trigger::Custom { recognizer, .. } = &mut rule.trigger {
            recognizer.reply_within = Millis(20);
        }
    }
    routing = custom(profile, false);
    press(&mut routing, 1, 0, true);
    let (high, low) = pair(&mut routing, 0);
    routing
        .recognition_response(reply(&high, Phase::Pending), ms(1))
        .unwrap();
    routing
        .recognition_response(reply(&low, Phase::Discrete), ms(2))
        .unwrap();
    routing.push(Record::Tick { at: ms(21) }).unwrap();
    assert!(routing.deferred_custom.is_empty());
    assert!(routing.take_dispatches(ms(21)).is_empty());
    let timer = request(&mut routing, 21);
    routing
        .recognition_response(reply(&timer, Phase::Cancel), ms(22))
        .unwrap();
    assert_eq!(drain(&mut routing, 22).len(), 1);
    press(&mut routing, 2, 25, false);
    assert_eq!(drain(&mut routing, 47).len(), 1);
    assert!(
        routing
            .recognition_response(reply(&low, Phase::Discrete), ms(100))
            .is_err()
    );
}

#[test]
fn context_loss_cancels_tickets_before_an_action_can_be_granted() {
    let mut routing = custom(profile(Delivery::Reserve, false, false), false);
    press(&mut routing, 1, 0, true);
    let ticket = request(&mut routing, 0);
    routing.authority_mut().lock_requested();
    routing.push(Record::Tick { at: ms(1) }).unwrap();
    assert!(
        routing
            .recognition_response(reply(&ticket, Phase::Discrete), ms(2))
            .is_err()
    );
    assert!(routing.take_dispatches(ms(2)).is_empty());
    assert_eq!(
        routing.take_recognition_cancellations()[0].reason,
        Retirement::ContextLost
    );
}

#[test]
fn custom_touch_claims_use_the_same_join_and_raw_stream_rules() {
    let mut routing = custom(profile(Delivery::Buffer, false, true), true);
    down(&mut routing, 2);
    let ticket = request(&mut routing, 0);
    routing
        .recognition_response(reply(&ticket, Phase::Discrete), ms(6))
        .unwrap();
    assert_eq!(routing.take_dispatches(ms(6)).len(), 1);
    up(&mut routing, 2, 10, 2);
    assert!(drain(&mut routing, 11).is_empty());
}

#[test]
fn source_reload_retires_old_tickets_and_reconciles_held_input() {
    let mut profile = profile(Delivery::Observe, false, false);
    profile.mode = Mode::Observe;
    let mut engine = Engine::with_recognition(profile.clone(), fence()).unwrap();
    *engine.authority_mut() = routing(config(), true).engine.authority().clone();
    engine.push(Record::Header { version: 1 }).unwrap();
    engine
        .push(Record::Device {
            device: keyboard(),
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    let push = |engine: &mut Engine, sequence, at, down| {
        engine
            .push(Record::Frame {
                frame: Frame {
                    source: keyboard().source,
                    sequence,
                    at: ms(at),
                    events: vec![
                        RawEvent {
                            kind: EV_KEY,
                            code: 30,
                            value: i32::from(down),
                        },
                        RawEvent {
                            kind: EV_SYN,
                            code: SYN_REPORT,
                            value: 0,
                        },
                    ],
                },
            })
            .unwrap()
    };
    push(&mut engine, 1, 0, true);
    let old = engine.recognition_requests(ms(0)).unwrap().pop().unwrap();
    engine.reload(profile, ms(1)).unwrap();
    let cancelled = engine.take_recognition_cancellations();
    assert_eq!(cancelled[0].fence, fence());
    assert_eq!(cancelled[0].reason, Retirement::RuleChanged);
    assert!(
        engine
            .recognition_response(reply(&old, Phase::Discrete), ms(2))
            .is_err()
    );
    assert!(engine.recognition_requests(ms(2)).unwrap().is_empty());
    push(&mut engine, 2, 3, false);
    push(&mut engine, 3, 4, true);
    let fresh = engine.recognition_requests(ms(4)).unwrap().pop().unwrap();
    assert!(fresh.id > old.id && fresh.candidate > old.candidate);
    assert_eq!(fresh.fence.configuration, 2);
    engine
        .recognition_response(reply(&fresh, Phase::Discrete), ms(5))
        .unwrap();
    assert!(engine.take_events().iter().any(|event| matches!(
        event,
        Event::Matched {
            config_generation: 2,
            ..
        }
    )));
}

#[test]
fn rule_transactions_preserve_ticket_identity_and_source_approved_declarations() {
    use crate::rule_transactions::{Change, Transaction};
    let mut profile = profile(Delivery::Observe, false, false);
    profile.mode = Mode::Observe;
    let mut engine = Engine::with_recognition(profile, fence()).unwrap();
    *engine.authority_mut() = routing(config(), true).engine.authority().clone();
    engine.push(Record::Header { version: 1 }).unwrap();
    engine
        .push(Record::Device {
            device: keyboard(),
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    let original = engine.config().rules[0].clone();
    let push = |engine: &mut Engine, sequence, at, down| {
        engine
            .push(Record::Frame {
                frame: Frame {
                    source: keyboard().source,
                    sequence,
                    at: ms(at),
                    events: vec![
                        RawEvent {
                            kind: EV_KEY,
                            code: 30,
                            value: i32::from(down),
                        },
                        RawEvent {
                            kind: EV_SYN,
                            code: SYN_REPORT,
                            value: 0,
                        },
                    ],
                },
            })
            .unwrap()
    };
    push(&mut engine, 1, 0, true);
    let old = engine.recognition_requests(ms(0)).unwrap().pop().unwrap();
    engine
        .transact_rules(
            &Transaction {
                revision: 1,
                changes: vec![Change::Remove {
                    id: original.id.clone(),
                }],
            },
            ms(1),
        )
        .unwrap();
    assert_eq!(
        engine.take_recognition_cancellations()[0].reason,
        Retirement::RuleChanged
    );
    engine
        .transact_rules(
            &Transaction {
                revision: 2,
                changes: vec![Change::Replace {
                    rule: Box::new(original.clone()),
                }],
            },
            ms(2),
        )
        .unwrap();
    assert!(
        engine
            .recognition_response(reply(&old, Phase::Discrete), ms(2))
            .is_err()
    );
    push(&mut engine, 2, 3, false);
    push(&mut engine, 3, 4, true);
    let fresh = engine.recognition_requests(ms(4)).unwrap().pop().unwrap();
    assert_eq!(fresh.fence, old.fence);
    assert_eq!(fresh.revision, 3);
    assert!(fresh.id > old.id);
    let mut unapproved = original;
    unapproved.id = "unapproved".into();
    unapproved.priority += 1;
    if let Trigger::Custom { recognizer, .. } = &mut unapproved.trigger {
        recognizer.name = "other".into();
    }
    assert!(
        engine
            .transact_rules(
                &Transaction {
                    revision: 3,
                    changes: vec![Change::Replace {
                        rule: Box::new(unapproved)
                    }]
                },
                ms(4)
            )
            .is_err()
    );
    assert_eq!(engine.rule_revision(), 3);
    engine
        .recognition_response(reply(&fresh, Phase::Discrete), ms(5))
        .unwrap();
    assert!(
        engine
            .take_events()
            .iter()
            .any(|event| matches!(event, Event::Matched { .. }))
    );
}
