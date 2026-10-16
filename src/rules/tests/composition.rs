//! Private coordinator models; public nested-custom admission stays disabled.
#[cfg(target_os = "linux")]
mod helper;
use super::*;
use crate::{
    protocol::{Epoch, Fence},
    recognition::{Definition, InputKind, Phase as CustomPhase, Proposal, Retirement, Ticket},
};

fn custom(name: &str) -> Trigger {
    Trigger::Custom {
        recognizer: Definition {
            name: name.into(),
            selectors: BTreeSet::from(["keyboard".into()]),
            inputs: BTreeSet::from([InputKind::Key]),
            reply_within: Millis(250),
            continuous: false,
        },
        timeout: Millis(1000),
        wake: None,
    }
}
fn model(last: Trigger, chains: usize) -> Engine {
    model_steps(custom("first"), last, chains, false)
}
fn model_steps(first: Trigger, last: Trigger, chains: usize, empty_contexts: bool) -> Engine {
    let mut config = config(custom("direct"));
    config.timing.join_window = Millis(20);
    config.policy.allow_input_events = true;
    config.policy.allowed_devices = config.devices.keys().cloned().collect();
    config.rules.truncate(1);
    let rule = &mut config.rules[0];
    rule.id = "direct".into();
    rule.devices = vec!["keyboard".into()];
    rule.contexts = vec![crate::config::Context::Desktop];
    rule.scope = Scope::Everywhere;
    rule.delivery = crate::config::Delivery::Observe;
    rule.require_keys.clear();
    rule.layer = None;
    rule.keys_throughout = false;
    rule.action = Action::Event {
        name: "fixture".into(),
    };
    config.recognizers = [
        custom("direct"),
        custom("first"),
        custom("last"),
        first.clone(),
        last.clone(),
    ]
    .iter()
    .flat_map(Trigger::recognizers)
    .map(|definition| (definition.name.clone(), definition.clone()))
    .collect::<BTreeMap<_, _>>()
    .into_values()
    .collect();
    let fence = Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    };
    let mut engine = Engine::with_recognition(config.clone(), fence).unwrap();
    for index in 0..chains {
        let mut rule = config.rules[0].clone();
        rule.id = format!("chain-{index}");
        if empty_contexts {
            rule.contexts.clear();
        }
        rule.trigger = Trigger::Sequence {
            steps: vec![first.clone(), last.clone()],
            timeout: Millis(1000),
        };
        engine
            .sequences
            .push(Sequence::new(rule, &config, Some(fence)).unwrap());
    }
    engine.push(Record::Header { version: 1 }).unwrap();
    let mut keyboard = device(1);
    let touch = first.contains_touch() || last.contains_touch();
    if !touch {
        keyboard.axes.clear();
        keyboard.properties.clear();
        keyboard.capabilities =
            BTreeMap::from([(EV_SYN, vec![SYN_REPORT]), (EV_KEY, vec![30, 115, 125])]);
    }
    add(&mut engine, keyboard, &[]);
    engine
        .authority
        .bind(
            Binding {
                seat: "seat0".into(),
                session: "coordinator-model".into(),
                uid: 1000,
                compositor_pid: 123,
                compositor_start: 456,
                kind: SessionKind::User,
            },
            true,
            false,
        )
        .unwrap();
    engine.authority.update(snapshot(1, 1, 0), ms(0)).unwrap();
    if touch {
        let mut mapping = Mapping::from_device(&device(1)).unwrap();
        mapping.generation = 2;
        mapping.output = Some(snapshot(1, 1, 0).outputs[0].geometry.clone());
        engine
            .push(Record::Mapping {
                source: device(1).source,
                at: ms(0),
                mapping,
            })
            .unwrap();
    }
    engine
}

fn touch_custom(name: &str) -> Trigger {
    let mut trigger = custom(name);
    let Trigger::Custom { recognizer, .. } = &mut trigger else {
        unreachable!()
    };
    recognizer.inputs = BTreeSet::from([InputKind::Touch]);
    trigger
}
fn held_touch_model() -> Engine {
    model_steps(
        Trigger::TouchHold {
            fingers: 2,
            after: Millis(100),
        },
        touch_custom("last"),
        1,
        true,
    )
}

#[test]
fn a_stationary_native_hold_hands_off_truthful_retained_touch_without_a_new_frame() {
    let mut engine = held_touch_model();
    touch_start(&mut engine, 200);
    assert!(engine.recognition_requests(ms(0)).unwrap().is_empty());
    let outer = engine.sequences[0].pending["seat0"].id;
    tick(&mut engine, 100);
    let tickets = engine.recognition_requests(ms(100)).unwrap();
    let [ticket] = &tickets[..] else {
        panic!("{tickets:?}; {:?}", engine.events)
    };
    assert_eq!(ticket.cause, crate::recognition::Cause::Timer);
    assert_eq!(ticket.requested_at, ms(100));
    assert_eq!(ticket.input.at(), ms(0));
    assert!(matches!(&ticket.input, crate::input_events::Event::Frame {
        sequence: 1, changes, contacts, contacts_started, contacts_ended, ended_contacts, ..
    } if contacts.len()==2 && contacts.iter().all(|c| !c.fresh)
        && changes.is_empty() && contacts_started.is_empty() && contacts_ended.is_empty() && ended_contacts.is_empty()));
    let Trigger::Custom { recognizer, .. } = &engine.sequences[0].triggers[1] else {
        unreachable!()
    };
    ticket.validate_for(recognizer, ms(100)).unwrap();
    assert_eq!(engine.input_watermark, 1);
    engine
        .recognition_response(reply(ticket, CustomPhase::Discrete), ms(101))
        .unwrap();
    assert!(
        matches!(&matches(&engine.events)[..], [Event::Matched {id,at,evidence:Evidence::Sequence {steps},..}]
        if *id==outer && *at==ms(100) && steps.len()==2)
    );
    assert_eq!(engine.input_watermark, 1);
}

#[test]
fn retained_touch_uses_the_actual_handoff_clock_and_latest_physical_metadata() {
    let mut engine = held_touch_model();
    touch_start(&mut engine, 200);
    frame(&mut engine, device(1).source, 120, 2, &[]);
    let tickets = engine.recognition_requests(ms(120)).unwrap();
    let [ticket] = &tickets[..] else {
        panic!("{tickets:?}; {:?}", engine.events)
    };
    assert_eq!(ticket.cause, crate::recognition::Cause::Timer);
    assert_eq!(ticket.requested_at, ms(120));
    assert!(
        matches!(ticket.input, crate::input_events::Event::Frame { sequence:2,at,.. } if at==ms(120))
    );
    assert_eq!(engine.input_watermark, 2);
}

#[test]
fn asynchronous_touch_handoff_uses_a_new_native_identity_not_a_replayed_frame() {
    let mut engine = model_steps(touch_custom("first"), touch_custom("last"), 1, true);
    touch_start(&mut engine, 200);
    let [first] = &engine.recognition_requests(ms(0)).unwrap()[..] else {
        panic!()
    };
    frame(
        &mut engine,
        device(1).source,
        5,
        2,
        &[(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_POSITION_X, 110)],
    );
    engine
        .recognition_response_after_input(reply(first, CustomPhase::Discrete), ms(5), ms(10))
        .unwrap();
    assert_eq!(engine.input_at(), ms(5));
    assert_eq!(engine.input_watermark, 2);
    let tickets = engine.recognition_requests(ms(10)).unwrap();
    let [last] = &tickets[..] else {
        panic!("{tickets:?}; {:?}", engine.events)
    };
    assert_ne!(last.candidate, first.candidate);
    assert!(last.id > first.id);
    assert_eq!(last.cause, crate::recognition::Cause::Timer);
    assert_eq!(last.requested_at, ms(10));
    assert!(
        matches!(&last.input,crate::input_events::Event::Frame{sequence:2,at,contacts,..}
        if *at==ms(5) && contacts[0].x==110 && contacts.iter().all(|c|!c.fresh))
    );
    engine
        .recognition_response(reply(last, CustomPhase::Discrete), ms(11))
        .unwrap();
    assert_eq!(matches(&engine.events).len(), 1);
    assert!(engine.sequences[0].pending.is_empty());
    assert_eq!(engine.input_watermark, 2);
}

#[test]
fn a_touch_handoff_refuses_changed_contact_membership_even_at_the_hold_deadline() {
    for replacement in [false, true] {
        let mut engine = held_touch_model();
        touch_start(&mut engine, 200);
        let mut transitions = vec![(EV_ABS, ABS_MT_SLOT, 0), (EV_ABS, ABS_MT_TRACKING_ID, -1)];
        if replacement {
            transitions.extend([
                (EV_ABS, ABS_MT_TRACKING_ID, 3),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_ABS, ABS_MT_POSITION_Y, 500),
            ]);
        }
        frame(&mut engine, device(1).source, 100, 2, &transitions);
        assert!(
            engine.recognition_requests(ms(100)).unwrap().is_empty(),
            "replacement={replacement}"
        );
        assert!(
            engine.sequences[0].pending.is_empty(),
            "{:?}",
            engine.events
        );
        assert!(engine.events.iter().any(|event|matches!(event,Event::Cancelled{reason,..} if reason=="touch continuation held state changed")));
    }
}

#[test]
fn a_delayed_custom_prefix_cannot_handoff_to_a_different_held_group() {
    let mut engine = model_steps(touch_custom("first"), touch_custom("last"), 1, true);
    touch_start(&mut engine, 200);
    let first = engine.recognition_requests(ms(0)).unwrap().remove(0);
    frame(
        &mut engine,
        device(1).source,
        1,
        2,
        &[
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_ABS, ABS_MT_SLOT, 1),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
        ],
    );
    touch_start_at(&mut engine, 200, 2, 3);
    engine
        .recognition_response(reply(&first, CustomPhase::Discrete), ms(3))
        .unwrap();
    assert!(engine.recognition_requests(ms(3)).unwrap().is_empty());
    assert!(engine.sequences[0].pending.is_empty());
    assert!(matches(&engine.events).is_empty());
}

#[test]
fn mapping_loss_and_reconciliation_cannot_seed_a_retained_touch_ticket() {
    for reset in 0..3 {
        let mut engine = held_touch_model();
        touch_start(&mut engine, 200);
        match reset {
            0 => frame(
                &mut engine,
                device(1).source,
                50,
                2,
                &[(EV_SYN, crate::input::SYN_DROPPED, 0)],
            ),
            1 => engine
                .push(Record::Reconcile {
                    source: device(1).source,
                    at: ms(50),
                    keys: BTreeSet::new(),
                    contacts: engine.devices[&1].contacts.clone(),
                })
                .unwrap(),
            _ => {
                let mut mapping = engine.devices[&1].mapping.clone().unwrap();
                mapping.generation += 1;
                engine
                    .push(Record::Mapping {
                        source: device(1).source,
                        at: ms(50),
                        mapping,
                    })
                    .unwrap();
            }
        }
        assert!(engine.devices[&1].last_input.is_none());
        tick(&mut engine, 100);
        assert!(
            engine.recognition_requests(ms(100)).unwrap().is_empty(),
            "reset={reset}"
        );
        assert!(engine.sequences[0].pending.is_empty());
    }
}

#[test]
fn retained_touch_handoffs_keep_the_original_frozen_window_when_focus_moves() {
    let mut engine = held_touch_model();
    let action = Action::Hyprland {
        action: crate::config::WindowAction::Close,
    };
    engine.sequences[0].rule.action = action.clone();
    engine.sequences[0].rule.output = Some("phone".into());
    for child in &mut engine.sequences[0].steps {
        child.workers[0].rule.action = action.clone();
        child.workers[0].rule.output = Some("phone".into());
    }
    touch_start(&mut engine, 200);
    let mut changed = snapshot(1, 2, 90);
    changed.windows[0].focused = false;
    changed.windows[1].focused = true;
    engine.authority.update(changed, ms(90)).unwrap();
    tick(&mut engine, 100);
    let tickets = engine.recognition_requests(ms(100)).unwrap();
    let [ticket] = &tickets[..] else {
        panic!("{tickets:?}; {:?}", engine.events)
    };
    let candidate = &engine.sequences[0].steps[1].workers[0].candidates[&device(1).source];
    assert_eq!(candidate.permit.as_ref().unwrap().window().unwrap().id, "a");
    engine
        .recognition_response(reply(ticket, CustomPhase::Discrete), ms(101))
        .unwrap();
    assert!(
        matches!(&matches(&engine.events)[..],[Event::Matched{window:Some(window),..}] if window=="a")
    );
}

#[test]
fn retained_touch_tickets_obey_new_group_clocks_and_original_reply_windows() {
    let mut engine = held_touch_model();
    engine.sequences[0].groups.push(SequenceGroup {
        first: 1,
        end: 2,
        timeout: ms(50),
    });
    touch_start(&mut engine, 200);
    tick(&mut engine, 100);
    assert_eq!(engine.sequences[0].pending["seat0"].groups[&1], ms(100));
    let ticket = engine.recognition_requests(ms(120)).unwrap().remove(0);
    assert_eq!(ticket.requested_at, ms(100));
    assert_eq!(ticket.deadline, ms(150));
    assert!(
        engine
            .recognition_response(reply(&ticket, CustomPhase::Discrete), ms(150))
            .is_err()
    );
    assert!(engine.sequences[0].pending.is_empty());
}

#[test]
fn retained_touch_continuations_share_capacity_and_revoke_with_the_helper() {
    let mut engine = model_steps(
        Trigger::TouchHold {
            fingers: 2,
            after: Millis(100),
        },
        touch_custom("last"),
        65,
        true,
    );
    touch_start(&mut engine, 200);
    tick(&mut engine, 100);
    let tickets = engine.recognition_requests(ms(100)).unwrap();
    assert_eq!(tickets.len(), crate::recognition::MAX_PENDING);
    assert_eq!(
        tickets
            .iter()
            .map(|t| t.candidate)
            .collect::<BTreeSet<_>>()
            .len(),
        tickets.len()
    );
    engine.recognition_ready(false, ms(101)).unwrap();
    assert!(engine.sequences.iter().all(|s| s.pending.is_empty()));
    assert_eq!(engine.take_recognition_cancellations().len(), tickets.len());
    engine.recognition_ready(true, ms(102)).unwrap();
    frame(&mut engine, device(1).source, 103, 2, &[]);
    assert!(engine.recognition_requests(ms(103)).unwrap().is_empty());
}
fn reply(ticket: &Ticket, phase: CustomPhase) -> Proposal {
    Proposal {
        fence: ticket.fence,
        ticket: ticket.id,
        phase,
        data: serde_json::json!({"step":ticket.recognizer}),
    }
}

#[test]
fn one_gate_keeps_sibling_tickets_and_collects_validated_steps_without_input_replay() {
    fn assert_send<T: Send>() {}
    assert_send::<Engine>();
    let mut engine = model(custom("last"), 1);
    key(&mut engine, 1, 0, 1, 30, true);
    assert!(
        !engine.workers[0].candidates.is_empty(),
        "{:?}; {:?}",
        engine.events,
        engine.workers[0].rule
    );
    let tickets = engine.recognition_requests(ms(0)).unwrap();
    assert_eq!(tickets.len(), 2);
    let first = tickets
        .iter()
        .find(|ticket| ticket.recognizer == "first")
        .unwrap();
    let direct = tickets
        .iter()
        .find(|ticket| ticket.recognizer == "direct")
        .unwrap();
    assert_ne!(first.id, direct.id);
    assert_ne!(first.candidate, direct.candidate);
    let outer = engine.sequences[0].pending["seat0"].id;
    assert_ne!(outer, first.candidate);
    assert!(
        engine.sequences[0]
            .steps
            .iter()
            .all(|step| step.custom.is_none())
    );
    engine
        .recognition_response(reply(first, CustomPhase::Discrete), ms(1))
        .unwrap();
    engine
        .recognition_response(reply(direct, CustomPhase::Pending), ms(1))
        .unwrap();
    assert_eq!(engine.sequences[0].pending["seat0"].step, 1);
    assert!(matches(&engine.events).is_empty());
    let cleanup = engine.take_recognition_cancellations();
    assert!(
        matches!(&cleanup[..], [value] if value.candidate == first.candidate && value.reason == Retirement::Completed)
    );
    key(&mut engine, 1, 2, 2, 30, false);
    key(&mut engine, 1, 3, 3, 30, true);
    let tickets = engine.recognition_requests(ms(3)).unwrap();
    let last = tickets
        .iter()
        .find(|ticket| ticket.recognizer == "last")
        .unwrap();
    assert!(last.id > first.id && last.candidate > outer);
    engine
        .recognition_response(reply(last, CustomPhase::Discrete), ms(4))
        .unwrap();
    assert!(
        matches!(&matches(&engine.events)[..], [Event::Matched { id, evidence: Evidence::Sequence { steps }, .. }]
        if *id == outer && steps.len() == 2)
    );
    assert!(
        engine
            .recognition_response(reply(last, CustomPhase::Discrete), ms(4))
            .is_err()
    );
    key(&mut engine, 1, 5, 4, 30, false);
}

#[test]
fn delayed_step_validation_cannot_borrow_an_already_drained_equal_timestamp_press() {
    let mut engine = model(hold(100), 1);
    key(&mut engine, 1, 0, 1, 30, true);
    let ticket = engine
        .recognition_requests(ms(0))
        .unwrap()
        .into_iter()
        .find(|ticket| ticket.recognizer == "first")
        .unwrap();
    key(&mut engine, 1, 0, 2, 115, true);
    engine
        .recognition_response_after_input(reply(&ticket, CustomPhase::Discrete), ms(0), ms(1))
        .unwrap();
    assert_eq!(engine.input_at(), ms(0));
    assert_eq!(engine.input_watermark, 2);
    tick(&mut engine, 100);
    assert!(matches(&engine.events).is_empty());
    key(&mut engine, 1, 101, 3, 115, false);
    key(&mut engine, 1, 102, 4, 115, true);
    tick(&mut engine, 202);
    assert_eq!(matches(&engine.events).len(), 1);
}

#[test]
fn all_private_steps_share_one_candidate_and_offer_budget() {
    let mut engine = model(custom("last"), 64);
    key(&mut engine, 1, 0, 1, 30, true);
    let tickets = engine.recognition_requests(ms(0)).unwrap();
    assert_eq!(tickets.len(), crate::recognition::MAX_PENDING);
    assert_eq!(
        tickets
            .iter()
            .map(|ticket| ticket.id)
            .collect::<BTreeSet<_>>()
            .len(),
        tickets.len()
    );
    assert_eq!(
        tickets
            .iter()
            .map(|ticket| ticket.candidate)
            .collect::<BTreeSet<_>>()
            .len(),
        tickets.len()
    );
    assert_eq!(
        engine
            .sequences
            .iter()
            .filter(|sequence| !sequence.pending.is_empty())
            .count(),
        63
    );
}

#[test]
fn helper_revocation_cancels_parent_steps_and_regrant_requires_neutral_input() {
    let mut engine = model(custom("last"), 1);
    key(&mut engine, 1, 0, 1, 30, true);
    let tickets = engine.recognition_requests(ms(0)).unwrap();
    engine.recognition_ready(false, ms(1)).unwrap();
    assert!(engine.sequences[0].pending.is_empty());
    let cleanup = engine.take_recognition_cancellations();
    assert_eq!(cleanup.len(), tickets.len());
    engine.recognition_ready(true, ms(2)).unwrap();
    frame(&mut engine, device(1).source, 3, 2, &[(EV_KEY, 30, 2)]);
    assert!(engine.recognition_requests(ms(3)).unwrap().is_empty());
    key(&mut engine, 1, 4, 3, 30, false);
    key(&mut engine, 1, 5, 4, 30, true);
    assert_eq!(engine.recognition_requests(ms(5)).unwrap().len(), 2);
}

#[test]
fn a_failed_child_record_returns_the_same_coordinator_to_its_owner() {
    let mut engine = model(custom("last"), 1);
    let mut sequence = engine.sequences.remove(0);
    let mut coordinator = engine.custom.take();
    let context = SequenceContext {
        devices: &engine.devices,
        authority: &engine.authority,
        keys: &engine.conditions,
        generation: engine.generation,
        at: ms(0),
        admitted_at: ms(0),
        input_watermark: engine.input_watermark,
        normalized: None,
    };
    assert!(
        sequence
            .push(
                &Record::Header { version: 1 },
                &context,
                &mut engine.next_id,
                &mut engine.events,
                &mut coordinator
            )
            .is_err()
    );
    assert!(coordinator.is_some());
    assert!(sequence.steps.iter().all(|step| step.custom.is_none()));
}

#[test]
fn a_private_step_refuses_a_missing_coordinator_without_creating_one() {
    let mut engine = model(custom("last"), 1);
    let mut sequence = engine.sequences.remove(0);
    let context = SequenceContext {
        devices: &engine.devices,
        authority: &engine.authority,
        keys: &engine.conditions,
        generation: engine.generation,
        at: ms(0),
        admitted_at: ms(0),
        input_watermark: engine.input_watermark,
        normalized: None,
    };
    let mut absent = None;
    let error = sequence
        .push(
            &Record::Tick { at: ms(0) },
            &context,
            &mut engine.next_id,
            &mut engine.events,
            &mut absent,
        )
        .unwrap_err();
    assert!(error.to_string().contains("coordinator is unavailable"));
    assert!(absent.is_none());
    assert!(engine.custom.is_some());
}

#[test]
fn refusing_an_unselected_source_does_not_cancel_the_owning_sequence() {
    let mut engine = model(custom("last"), 1);
    key(&mut engine, 1, 0, 1, 30, true);
    let ticket = engine
        .recognition_requests(ms(0))
        .unwrap()
        .into_iter()
        .find(|ticket| ticket.recognizer == "first")
        .unwrap();
    let mut other = engine.devices[&1].device.clone();
    other.source.device = 2;
    other.identity = "other-keyboard".into();
    add(&mut engine, other.clone(), &[]);
    frame(&mut engine, other.source, 1, 1, &[(EV_KEY, 30, 1)]);
    let extra = engine.recognition_requests(ms(1)).unwrap();
    assert!(extra.iter().all(|ticket| ticket.recognizer == "direct"));
    tick(&mut engine, 2);
    assert_eq!(engine.sequences[0].pending["seat0"].step, 0);
    engine
        .recognition_response(reply(&ticket, CustomPhase::Discrete), ms(2))
        .unwrap();
    assert_eq!(engine.sequences[0].pending["seat0"].step, 1);
}

#[test]
fn active_private_steps_publish_stationary_wakes_through_the_root_deadline() {
    let mut engine = model(custom("last"), 1);
    let Trigger::Custom { wake, .. } = &mut engine.sequences[0].steps[1].workers[0].rule.trigger
    else {
        unreachable!()
    };
    *wake = Some(Millis(20));
    key(&mut engine, 1, 0, 1, 30, true);
    let first = engine
        .recognition_requests(ms(0))
        .unwrap()
        .into_iter()
        .find(|ticket| ticket.recognizer == "first")
        .unwrap();
    engine
        .recognition_response(reply(&first, CustomPhase::Discrete), ms(1))
        .unwrap();
    key(&mut engine, 1, 2, 2, 30, false);
    key(&mut engine, 1, 3, 3, 30, true);
    let last = engine
        .recognition_requests(ms(3))
        .unwrap()
        .into_iter()
        .find(|ticket| ticket.recognizer == "last")
        .unwrap();
    engine
        .recognition_response(reply(&last, CustomPhase::Pending), ms(4))
        .unwrap();
    assert_eq!(engine.deadline(), Some(ms(23)));
    let timer = engine
        .recognition_requests(ms(23))
        .unwrap()
        .into_iter()
        .find(|ticket| ticket.recognizer == "last")
        .unwrap();
    assert_eq!(timer.candidate, last.candidate);
    assert_eq!(timer.cause, crate::recognition::Cause::Timer);
    assert!(
        matches!(&timer.input, crate::input_events::Event::Frame { sequence: 3, changes, .. } if changes.is_empty())
    );
    engine
        .recognition_response(reply(&timer, CustomPhase::Discrete), ms(24))
        .unwrap();
    assert_eq!(matches(&engine.events).len(), 1);
    assert_eq!(engine.input_watermark, 3);
}

#[test]
fn expired_private_tickets_cancel_the_parent_without_retiring_a_sibling() {
    let mut engine = model(custom("last"), 1);
    let Trigger::Custom { timeout, .. } = &mut engine.sequences[0].steps[0].workers[0].rule.trigger
    else {
        unreachable!()
    };
    *timeout = Millis(50);
    key(&mut engine, 1, 0, 1, 30, true);
    let tickets = engine.recognition_requests(ms(0)).unwrap();
    let first = tickets
        .iter()
        .find(|ticket| ticket.recognizer == "first")
        .unwrap();
    let direct = tickets
        .iter()
        .find(|ticket| ticket.recognizer == "direct")
        .unwrap();
    assert_eq!(first.deadline, ms(50));
    assert!(
        engine
            .recognition_response(reply(first, CustomPhase::Discrete), ms(80))
            .is_err()
    );
    assert!(engine.sequences[0].pending.is_empty());
    assert!(
        matches!(&engine.take_recognition_cancellations()[..], [cancellation]
        if cancellation.candidate == first.candidate && cancellation.reason == Retirement::Expired)
    );
    engine
        .recognition_response(reply(direct, CustomPhase::Pending), ms(80))
        .unwrap();
}

#[test]
fn source_replacement_retires_all_old_step_tickets_and_keeps_new_ids_distinct() {
    let mut engine = model(custom("last"), 1);
    key(&mut engine, 1, 0, 1, 30, true);
    let old = engine.recognition_requests(ms(0)).unwrap();
    let mut device = engine.devices[&1].device.clone();
    engine
        .push(Record::Remove {
            source: device.source.clone(),
            at: ms(1),
        })
        .unwrap();
    assert!(engine.sequences[0].pending.is_empty());
    assert_eq!(engine.take_recognition_cancellations().len(), old.len());
    device.source.generation += 1;
    add(&mut engine, device.clone(), &[]);
    frame(&mut engine, device.source.clone(), 2, 1, &[(EV_KEY, 30, 1)]);
    let fresh = engine.recognition_requests(ms(2)).unwrap();
    assert_eq!(fresh.len(), 2);
    assert!(fresh.iter().all(|ticket| {
        ticket.input.source() == &device.source
            && old
                .iter()
                .all(|old| ticket.id > old.id && ticket.candidate != old.candidate)
    }));
    for ticket in old {
        assert!(
            engine
                .recognition_response(reply(&ticket, CustomPhase::Discrete), ms(2))
                .is_err()
        );
    }
}

#[test]
fn parent_group_deadlines_also_bound_private_ticket_expiry() {
    let mut engine = model(custom("last"), 1);
    engine.sequences[0].groups[0].timeout = ms(50);
    key(&mut engine, 1, 0, 1, 30, true);
    let ticket = engine
        .recognition_requests(ms(0))
        .unwrap()
        .into_iter()
        .find(|ticket| ticket.recognizer == "first")
        .unwrap();
    assert_eq!(ticket.deadline, ms(50));
    assert!(
        engine
            .recognition_response(reply(&ticket, CustomPhase::Discrete), ms(50))
            .is_err()
    );
    assert!(engine.sequences[0].pending.is_empty());
}

#[test]
fn ordinary_fenced_and_unfenced_sequences_keep_identical_external_ids_and_outcomes() {
    let profile = config(Trigger::Sequence {
        steps: vec![Trigger::Key { key: Key(30) }, hold(100)],
        timeout: Millis(1000),
    });
    let mut ordinary = engine(profile.clone());
    let mut coordinated = Engine::with_recognition(
        profile,
        Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 1,
        },
    )
    .unwrap();
    coordinated.push(Record::Header { version: 1 }).unwrap();
    add(&mut coordinated, device(1), &[]);
    for engine in [&mut ordinary, &mut coordinated] {
        frame(
            engine,
            device(1).source,
            0,
            1,
            &[(EV_KEY, 30, 1), (EV_KEY, 115, 1)],
        );
        tick(engine, 100);
    }
    assert!(!matches(&ordinary.events).is_empty());
    assert_eq!(ordinary.events, coordinated.events);
}

#[test]
fn a_native_prefix_still_freezes_desktop_eligibility_for_a_later_custom_step() {
    for active in [true, false] {
        let mut engine = model_steps(Trigger::Key { key: Key(30) }, custom("last"), 1, true);
        engine.authority.active_changed(active);
        key(&mut engine, 1, 0, 1, 30, true);
        if active {
            let pending = &engine.sequences[0].pending["seat0"];
            assert!(pending.first.permit.is_some());
            assert_eq!(pending.first.session_generation, 1);
            assert_eq!(pending.step, 1);
        } else {
            assert!(engine.sequences[0].pending.is_empty());
        }
    }
}

#[test]
fn a_synchronous_prefix_projects_the_same_frame_once_into_its_custom_suffix() {
    let mut engine = model_steps(Trigger::Key { key: Key(30) }, custom("last"), 1, true);
    key(&mut engine, 1, 0, 1, 30, true);
    let outer = engine.sequences[0].pending["seat0"].id;
    let tickets = engine.recognition_requests(ms(0)).unwrap();
    let suffix: Vec<_> = tickets
        .iter()
        .filter(|ticket| ticket.recognizer == "last")
        .collect();
    assert_eq!(suffix.len(), 1);
    assert_eq!(suffix[0].requested_at, ms(0));
    assert!(matches!(
        &suffix[0].input,
        crate::input_events::Event::Frame { sequence: 1, .. }
    ));
    engine
        .recognition_response(reply(suffix[0], CustomPhase::Discrete), ms(1))
        .unwrap();
    assert!(
        matches!(&matches(&engine.events)[..], [Event::Matched { id, evidence: Evidence::Sequence { steps }, .. }]
        if *id == outer && matches!(steps.as_slice(), [Evidence::Key { down: true }, Evidence::Custom { recognizer, .. }] if recognizer == "last"))
    );
    assert_eq!(engine.input_watermark, 1);
    key(&mut engine, 1, 2, 2, 30, false);
    assert_eq!(engine.input_watermark, 2);
}

#[test]
fn asynchronous_completion_cannot_project_a_previous_record_into_a_new_custom_step() {
    let mut engine = model(custom("last"), 1);
    key(&mut engine, 1, 0, 1, 30, true);
    let first = engine
        .recognition_requests(ms(0))
        .unwrap()
        .into_iter()
        .find(|ticket| ticket.recognizer == "first")
        .unwrap();
    engine
        .recognition_response(reply(&first, CustomPhase::Discrete), ms(1))
        .unwrap();
    assert!(engine.recognition_requests(ms(1)).unwrap().is_empty());
    assert!(
        engine.sequences[0].steps[1].workers[0]
            .candidates
            .is_empty()
    );
    assert_eq!(engine.input_watermark, 1);
    key(&mut engine, 1, 2, 2, 30, false);
    key(&mut engine, 1, 3, 3, 30, true);
    assert_eq!(
        engine
            .recognition_requests(ms(3))
            .unwrap()
            .iter()
            .filter(|ticket| ticket.recognizer == "last")
            .count(),
        1
    );
}

#[test]
fn a_same_record_handoff_anchors_nested_group_timeout_before_issuing_a_ticket() {
    let mut engine = model_steps(Trigger::Key { key: Key(30) }, custom("last"), 1, true);
    engine.sequences[0].groups.push(SequenceGroup {
        first: 1,
        end: 2,
        timeout: ms(50),
    });
    key(&mut engine, 1, 0, 1, 30, true);
    let ticket = engine
        .recognition_requests(ms(0))
        .unwrap()
        .into_iter()
        .find(|ticket| ticket.recognizer == "last")
        .unwrap();
    assert_eq!(ticket.deadline, ms(50));
    assert!(
        engine
            .recognition_response(reply(&ticket, CustomPhase::Discrete), ms(50))
            .is_err()
    );
    assert!(engine.sequences[0].pending.is_empty());
}

#[test]
fn input_loss_has_no_normalized_frame_to_activate_a_later_custom_step() {
    let mut engine = model_steps(hold(100), custom("last"), 1, true);
    key(&mut engine, 1, 0, 1, 115, true);
    frame(
        &mut engine,
        device(1).source,
        100,
        2,
        &[(EV_SYN, crate::input::SYN_DROPPED, 0)],
    );
    assert!(engine.recognition_requests(ms(100)).unwrap().is_empty());
    assert!(engine.sequences[0].pending.is_empty());
    assert!(
        engine.sequences[0].steps[1].workers[0]
            .candidates
            .is_empty()
    );
}

#[test]
fn same_record_projection_checks_the_original_target_before_joining_another_source() {
    let mut engine = model_steps(hold(100), custom("last"), 1, true);
    let action = Action::Hyprland {
        action: crate::config::WindowAction::Close,
    };
    engine.sequences[0].rule.action = action.clone();
    engine.sequences[0].rule.output = Some("phone".into());
    for child in &mut engine.sequences[0].steps {
        child.workers[0].rule.action = action.clone();
        child.workers[0].rule.output = Some("phone".into());
    }
    let mut other = engine.devices[&1].device.clone();
    other.source.device = 2;
    other.identity = "handoff-keyboard".into();
    add(&mut engine, other.clone(), &[]);
    key(&mut engine, 1, 0, 1, 115, true);
    assert_eq!(
        engine.sequences[0].pending["seat0"]
            .first
            .permit
            .as_ref()
            .unwrap()
            .window()
            .unwrap()
            .id,
        "a"
    );
    let mut changed = snapshot(1, 2, 90);
    changed.windows[0].focused = false;
    changed.windows[1].focused = true;
    engine.authority.update(changed, ms(90)).unwrap();
    frame(&mut engine, other.source, 110, 1, &[(EV_KEY, 30, 1)]);
    assert!(
        engine.sequences[0].pending.is_empty(),
        "target mismatch was not rejected at admission"
    );
    assert!(
        engine
            .recognition_requests(ms(110))
            .unwrap()
            .iter()
            .all(|ticket| ticket.recognizer == "direct")
    );
    assert!(engine.sequences[0].pending.is_empty());
    assert!(
        engine
            .events
            .iter()
            .any(|event| matches!(event, Event::Cancelled { at, reason, .. }
        if *at == ms(110) && reason == "sequence target changed"))
    );
}

fn handed_off_source() -> (Engine, Source, Ticket, Ticket) {
    let mut engine = model_steps(hold(100), custom("last"), 1, true);
    let mut other = engine.devices[&1].device.clone();
    other.source.device = 2;
    other.identity = "active-suffix".into();
    add(&mut engine, other.clone(), &[]);
    key(&mut engine, 1, 0, 1, 115, true);
    frame(
        &mut engine,
        other.source.clone(),
        110,
        1,
        &[(EV_KEY, 30, 1)],
    );
    let tickets = engine.recognition_requests(ms(110)).unwrap();
    let suffix = tickets
        .iter()
        .find(|ticket| ticket.recognizer == "last")
        .unwrap()
        .clone();
    let sibling = tickets
        .iter()
        .find(|ticket| ticket.recognizer == "direct" && ticket.input.source() == &other.source)
        .unwrap()
        .clone();
    assert!(
        engine.sequences[0].pending["seat0"]
            .sources
            .contains(&device(1).source)
    );
    assert!(
        engine.sequences[0].pending["seat0"]
            .sources
            .contains(&other.source)
    );
    (engine, other.source, suffix, sibling)
}

#[test]
fn retiring_an_admitted_prefix_transport_revokes_the_other_sources_step_ticket() {
    let (mut engine, _, suffix, sibling) = handed_off_source();
    engine.retire_routing_recognition(&device(1).source);
    assert!(engine.sequences[0].pending.is_empty());
    assert!(
        engine
            .take_recognition_cancellations()
            .iter()
            .any(|cancellation| cancellation.candidate == suffix.candidate
                && cancellation.reason == Retirement::SourceLost)
    );
    assert!(
        engine
            .recognition_response(reply(&suffix, CustomPhase::Discrete), ms(111))
            .is_err()
    );
    engine
        .recognition_response(reply(&sibling, CustomPhase::Pending), ms(111))
        .unwrap();
    assert!(matches(&engine.events).is_empty());
}

#[test]
fn reply_validation_rechecks_the_parent_source_independently_of_the_current_step() {
    let (mut engine, other, suffix, sibling) = handed_off_source();
    engine.devices.remove(&device(1).source.device);
    assert!(engine.devices.contains_key(&other.device));
    assert!(
        engine
            .recognition_response(reply(&suffix, CustomPhase::Discrete), ms(111))
            .is_err()
    );
    assert!(engine.sequences[0].pending.is_empty());
    assert!(matches(&engine.events).is_empty());
    engine
        .recognition_response(reply(&sibling, CustomPhase::Pending), ms(111))
        .unwrap();
}
