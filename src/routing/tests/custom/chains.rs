//! Native composed Routing ownership and cancellation models.
use super::*;
#[cfg(target_os = "linux")]
mod helper;
use crate::config::{Key, Rule};

fn custom_step(name: &str, continuous: bool, touch: bool) -> Trigger {
    let mut trigger = profile(Delivery::Buffer, continuous, touch)
        .rules
        .remove(0)
        .trigger;
    if let Trigger::Custom {
        recognizer, wake, ..
    } = &mut trigger
    {
        recognizer.name = name.into();
        *wake = None;
    }
    trigger
}
fn fixture(steps: Vec<Trigger>, delivery: Delivery, touch: bool) -> (Routing, Device, Device) {
    let mut config = profile(delivery, false, touch);
    config.timing.join_window = Millis(20);
    config.timing.decision_window = Millis(300);
    config.devices.get_mut("touch").unwrap().identity = None;
    config.devices.get_mut("touch").unwrap().keys = vec![Key(if touch { 0x14a } else { 30 })];
    config.recognizers = config.rules[0]
        .trigger
        .recognizers()
        .chain(steps.iter().flat_map(Trigger::recognizers))
        .cloned()
        .collect();
    let mut rule: Rule = config.rules[0].clone();
    rule.id = "chain".into();
    if touch && delivery == Delivery::Reserve {
        rule.screen = Some(Screen::Off);
        config.rules[0].screen = Some(Screen::Off);
    }
    rule.trigger = Trigger::Sequence {
        steps,
        timeout: Millis(1000),
    };
    config.rules = vec![rule];
    let mut routing = Routing::with_recognition(config, fence()).unwrap();
    context(&mut routing, 0);
    if touch && delivery == Delivery::Reserve {
        let mut snapshot = routing.authority_mut().fresh(ms(0)).unwrap().clone();
        snapshot.revision += 1;
        snapshot.outputs[0].screen = Screen::Off;
        routing.authority_mut().update(snapshot, ms(0)).unwrap();
    }
    let first = if touch { device() } else { keyboard() };
    let mut second = first.clone();
    second.source.device += 1;
    second.identity.push_str("-second");
    for device in [&first, &second] {
        let mapping = touch.then(|| {
            let mut mapping = Mapping::from_device(device).unwrap();
            mapping.output = Some(output());
            mapping.generation = 2;
            mapping
        });
        routing
            .attach(device.clone(), &ready(device), mapping, ms(0))
            .unwrap();
    }
    (routing, first, second)
}
fn key(routing: &mut Routing, device: &Device, sequence: u64, at: u64, down: bool) {
    routing
        .push(Record::Frame {
            frame: Frame {
                source: device.source.clone(),
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
        .unwrap();
}
fn owner(routing: &Routing) -> u64 {
    let active = routing.engine.route_intents();
    assert_eq!(active.len(), 1);
    active[0].id()
}
fn custom_pair(delivery: Delivery) -> (Routing, Device, Device) {
    fixture(
        vec![
            custom_step("first", false, false),
            custom_step("last", false, false),
        ],
        delivery,
        false,
    )
}
fn prefix(routing: &mut Routing, first: &Device) -> u64 {
    key(routing, first, 1, 0, true);
    let parent = owner(routing);
    let ticket = request(routing, 0);
    assert_eq!(ticket.recognizer, "first");
    assert_ne!(ticket.candidate, parent);
    routing
        .recognition_response(reply(&ticket, Phase::Discrete), ms(1))
        .unwrap();
    assert!(routing.take_dispatches(ms(1)).is_empty());
    key(routing, first, 2, 10, false);
    parent
}

#[test]
fn a_reserved_native_key_hands_its_proven_parent_claim_to_the_exact_custom_leaf() {
    let (mut routing, first, _) = fixture(
        vec![
            Trigger::Key { key: Key(30) },
            custom_step("last", false, false),
        ],
        Delivery::Reserve,
        false,
    );
    key(&mut routing, &first, 1, 0, true);
    let parent = owner(&routing);
    let ticket = request(&mut routing, 0);
    assert_ne!(ticket.candidate, parent);
    assert_eq!(routing.routes[&first.source].winner, Some(parent));
    assert_eq!(routing.custom_chains[&ticket.candidate].owner, parent);
    routing
        .recognition_response(reply(&ticket, Phase::Discrete), ms(1))
        .unwrap();
    let events = routing.take_dispatches(ms(1));
    assert!(
        matches!(&events[..], [Event::Matched { id, evidence: Evidence::Sequence { steps }, .. }] if *id==parent && steps.len()==2)
    );
    assert!(routing.custom_chains.is_empty());
    key(&mut routing, &first, 2, 2, false);
    assert!(routing.due(&first.source, ms(3)).unwrap().is_none());
}

#[test]
fn completed_custom_prefix_and_fresh_suffix_keep_one_buffer_and_original_ticket_deadline() {
    let (mut routing, first, _) = custom_pair(Delivery::Buffer);
    let parent = prefix(&mut routing, &first);
    let token = routing.chains[&parent][&first.source].clone();
    key(&mut routing, &first, 3, 200, true);
    let ticket = request(&mut routing, 200);
    assert_eq!(ticket.recognizer, "last");
    assert_eq!(ticket.deadline, ms(300));
    assert_eq!(
        routing.chains[&parent][&first.source].candidate(),
        token.candidate()
    );
    assert!(routing.chains[&parent][&first.source].segment() > token.segment());
    routing
        .recognition_response(reply(&ticket, Phase::Discrete), ms(201))
        .unwrap();
    let events = routing.take_dispatches(ms(201));
    assert!(matches!(&events[..], [Event::Matched { id, .. }] if *id==parent));
    key(&mut routing, &first, 4, 210, false);
    assert!(routing.due(&first.source, ms(211)).unwrap().is_none());
}

#[test]
fn a_cross_source_suffix_ticket_is_bound_to_every_prefix_transport() {
    let (mut routing, first, second) = custom_pair(Delivery::Buffer);
    let parent = prefix(&mut routing, &first);
    key(&mut routing, &second, 1, 200, true);
    let ticket = request(&mut routing, 200);
    assert_eq!(ticket.deadline, ms(300));
    let binding = &routing.custom_chains[&ticket.candidate];
    assert_eq!(binding.owner, parent);
    assert_eq!(binding.tokens.len(), 2);
    routing
        .recognition_response(reply(&ticket, Phase::Discrete), ms(201))
        .unwrap();
    assert_eq!(routing.take_dispatches(ms(201)).len(), 1);
    assert!(routing.due(&first.source, ms(202)).unwrap().is_none());
    assert!(routing.due(&second.source, ms(202)).unwrap().is_none());
}

#[test]
fn replies_cannot_create_missing_native_leaf_bindings() {
    let (mut routing, first, _) = custom_pair(Delivery::Reserve);
    key(&mut routing, &first, 1, 0, true);
    let ticket = request(&mut routing, 0);
    routing.custom_chains.remove(&ticket.candidate);
    assert!(
        routing
            .recognition_response(reply(&ticket, Phase::Discrete), ms(1))
            .is_err()
    );
    assert!(routing.take_dispatches(ms(1)).is_empty());
    assert!(routing.custom_chains.is_empty());
    assert!(routing.engine.route_intents().is_empty());
}

#[test]
fn reply_validation_rejects_a_changed_parent_or_nonleaf_prefix_binding() {
    for changed_parent in [true, false] {
        let (mut routing, first, second) = custom_pair(Delivery::Buffer);
        prefix(&mut routing, &first);
        key(&mut routing, &second, 1, 200, true);
        let ticket = request(&mut routing, 200);
        let binding = routing.custom_chains.get_mut(&ticket.candidate).unwrap();
        if changed_parent {
            binding.owner += 1;
        } else {
            binding.tokens.remove(&first.source);
        }
        assert!(
            routing
                .recognition_response(reply(&ticket, Phase::Discrete), ms(201))
                .is_err()
        );
        assert!(routing.take_dispatches(ms(201)).is_empty());
        assert!(routing.engine.route_intents().is_empty());
    }
}

#[test]
fn later_ticket_issuance_cannot_rebind_a_changed_native_scope() {
    let (mut routing, first, _) = custom_pair(Delivery::Reserve);
    key(&mut routing, &first, 1, 0, true);
    let ticket = request(&mut routing, 0);
    routing
        .recognition_response(reply(&ticket, Phase::Pending), ms(1))
        .unwrap();
    routing
        .custom_chains
        .get_mut(&ticket.candidate)
        .unwrap()
        .owner += 1;
    key(&mut routing, &first, 2, 2, false);
    assert!(routing.recognition_requests(ms(2)).unwrap().is_empty());
    assert!(routing.take_dispatches(ms(2)).is_empty());
    assert!(routing.engine.route_intents().is_empty());
}

#[test]
fn held_touch_handoffs_share_parent_ownership_without_new_raw_contacts() {
    let (mut routing, _, _) = fixture(
        vec![
            Trigger::TouchHold {
                fingers: 2,
                after: Millis(100),
            },
            custom_step("first", false, true),
            custom_step("last", false, true),
        ],
        Delivery::Reserve,
        true,
    );
    down(&mut routing, 2);
    let parent = owner(&routing);
    let first = request(&mut routing, 100);
    assert_eq!(first.cause, Cause::Timer);
    assert_eq!(first.input.at(), ms(0));
    routing
        .recognition_response(reply(&first, Phase::Discrete), ms(101))
        .unwrap();
    let last = request(&mut routing, 101);
    assert_ne!(last.candidate, first.candidate);
    assert_eq!(last.cause, Cause::Timer);
    assert_eq!(routing.custom_chains[&last.candidate].owner, parent);
    assert_eq!(routing.engine.input_watermark(), 1);
    routing
        .recognition_response(reply(&last, Phase::Discrete), ms(102))
        .unwrap();
    assert!(
        matches!(&routing.take_dispatches(ms(102))[..], [Event::Matched {id, ..}] if *id==parent)
    );
    up(&mut routing, 2, 110, 2);
    assert!(drain(&mut routing, 111).is_empty());
}

#[test]
fn a_claimed_continuous_suffix_outlives_only_the_old_buffer_decision_not_its_parent() {
    let (mut routing, _, _) = fixture(
        vec![
            Trigger::TouchHold {
                fingers: 2,
                after: Millis(100),
            },
            custom_step("last", true, true),
        ],
        Delivery::Buffer,
        true,
    );
    down(&mut routing, 2);
    let parent = owner(&routing);
    let begin = request(&mut routing, 100);
    assert_eq!(begin.deadline, ms(300));
    routing
        .recognition_response(reply(&begin, Phase::Begin), ms(101))
        .unwrap();
    assert!(
        matches!(&routing.take_dispatches(ms(101))[..], [Event::Matched { id, stage: Stage::Begin, .. }] if *id==parent)
    );
    context(&mut routing, 200);
    movement(&mut routing, 2, 310, 1200, 2);
    let update = request(&mut routing, 310);
    assert_eq!(update.candidate, begin.candidate);
    assert_eq!(update.deadline, ms(560));
    routing
        .recognition_response(reply(&update, Phase::Update), ms(311))
        .unwrap();
    assert!(
        matches!(&routing.take_dispatches(ms(311))[..], [Event::Matched { id, ordinal: 2, stage: Stage::Update, .. }] if *id==parent)
    );
    up(&mut routing, 3, 320, 2);
    let end = request(&mut routing, 320);
    routing
        .recognition_response(reply(&end, Phase::End), ms(321))
        .unwrap();
    assert!(
        matches!(&routing.take_dispatches(ms(321))[..], [Event::Matched { id, ordinal: 3, stage: Stage::End, .. }] if *id==parent)
    );
    assert!(drain(&mut routing, 322).is_empty());
}

#[test]
fn expired_suffix_replies_fall_back_at_the_first_buffer_epoch_without_overtaking() {
    let (mut routing, first, _) = custom_pair(Delivery::Buffer);
    prefix(&mut routing, &first);
    key(&mut routing, &first, 3, 200, true);
    let ticket = request(&mut routing, 200);
    assert_eq!(ticket.deadline, ms(300));
    context(&mut routing, 200);
    assert!(
        routing
            .recognition_response_after_input(reply(&ticket, Phase::Discrete), ms(200), ms(300))
            .is_err()
    );
    assert_eq!(routing.engine.input_at(), ms(200));
    assert!(routing.take_dispatches(ms(300)).is_empty());
    for (at, sequence) in [(300, 1), (310, 2), (500, 3)] {
        let scheduled = routing
            .due_after_input(&first.source, ms(at))
            .unwrap()
            .unwrap()
            .clone();
        assert_eq!(scheduled.frame.sequence, sequence);
        routing
            .delivered_after_input(&first.source, scheduled.ticket, ms(at))
            .unwrap();
    }
    assert!(
        routing
            .due_after_input(&first.source, ms(501))
            .unwrap()
            .is_none()
    );
}

#[test]
fn prefix_transport_helper_or_session_lock_invalidates_a_cross_source_reply() {
    for loss in [0, 1, 2] {
        let (mut routing, first, second) = custom_pair(Delivery::Reserve);
        prefix(&mut routing, &first);
        key(&mut routing, &second, 1, 200, true);
        let ticket = request(&mut routing, 200);
        match loss {
            0 => routing.recover(&first.source),
            1 => routing.recognition_ready(false, ms(201)).unwrap(),
            _ => routing.authority_mut().lock_requested(),
        }
        assert!(
            routing
                .recognition_response_after_input(reply(&ticket, Phase::Discrete), ms(200), ms(201))
                .is_err()
        );
        assert!(routing.take_dispatches(ms(201)).is_empty());
        assert!(routing.custom_chains.is_empty());
        assert!(routing.engine.route_intents().is_empty());
    }
}

#[test]
fn inactive_source_input_is_forwarded_and_cannot_become_a_later_prefix_proof() {
    let (mut routing, first, second) = custom_pair(Delivery::Buffer);
    key(&mut routing, &first, 1, 0, true);
    let initial = request(&mut routing, 0);
    key(&mut routing, &second, 1, 2, true);
    assert!(routing.recognition_requests(ms(2)).unwrap().is_empty());
    let ordinary = routing.due(&second.source, ms(2)).unwrap().unwrap().clone();
    assert_eq!(ordinary.frame.sequence, 1);
    routing
        .delivered(&second.source, ordinary.ticket, ms(2))
        .unwrap();
    routing
        .recognition_response(reply(&initial, Phase::Discrete), ms(3))
        .unwrap();
    key(&mut routing, &first, 2, 10, false);
    key(&mut routing, &second, 2, 20, false);
    assert!(routing.recognition_requests(ms(20)).unwrap().is_empty());
    let ordinary = routing
        .due(&second.source, ms(20))
        .unwrap()
        .unwrap()
        .clone();
    assert_eq!(ordinary.frame.sequence, 2);
    routing
        .delivered(&second.source, ordinary.ticket, ms(20))
        .unwrap();
    key(&mut routing, &second, 3, 200, true);
    let last = request(&mut routing, 200);
    routing
        .recognition_response(reply(&last, Phase::Discrete), ms(201))
        .unwrap();
    assert_eq!(routing.take_dispatches(ms(201)).len(), 1);
    assert!(routing.due(&first.source, ms(202)).unwrap().is_none());
    assert!(routing.due(&second.source, ms(202)).unwrap().is_none());
}

#[test]
fn a_new_physical_group_cannot_retarget_an_old_custom_ticket() {
    let (mut routing, first, _) = custom_pair(Delivery::Reserve);
    key(&mut routing, &first, 1, 0, true);
    let parent = owner(&routing);
    let old = request(&mut routing, 0);
    key(&mut routing, &first, 2, 1, false);
    key(&mut routing, &first, 3, 2, true);
    assert!(routing.recognition_requests(ms(2)).unwrap().is_empty());
    assert!(routing.engine.route_intents().is_empty());
    key(&mut routing, &first, 4, 3, false);
    key(&mut routing, &first, 5, 4, true);
    let replacement = request(&mut routing, 4);
    assert_ne!(replacement.candidate, old.candidate);
    assert_ne!(owner(&routing), parent);
    assert!(!routing.custom_chains.contains_key(&old.candidate));
    assert!(
        routing
            .recognition_response(reply(&old, Phase::Discrete), ms(5))
            .is_err()
    );
    assert!(routing.take_dispatches(ms(5)).is_empty());
    assert_eq!(
        routing.custom_chains[&replacement.candidate].owner,
        owner(&routing)
    );
}

#[test]
fn native_scope_storage_refuses_aggregate_overflow_before_inserting_a_leaf() {
    let (mut routing, first, _) = custom_pair(Delivery::Reserve);
    key(&mut routing, &first, 1, 0, true);
    let ticket = request(&mut routing, 0);
    let (leaf, parent) = routing
        .engine
        .recognition_scope(ticket.candidate, ms(0))
        .unwrap();
    let tokens = routing
        .custom_chains
        .remove(&ticket.candidate)
        .unwrap()
        .tokens;
    for id in 10_000..10_000 + chain::MAX_CHAIN_BINDINGS as u64 {
        routing.custom_chains.insert(
            id,
            crate::routing::custom::BoundCustom {
                owner: parent.id(),
                tokens: tokens.clone(),
            },
        );
    }
    let mut scopes = crate::routing::custom::CustomRoutes {
        domains: &routing.key_journal.domains,
        split_inputs: routing.split_inputs,
        routes: &routing.routes,
        chains: &routing.chains,
        groups: &routing.custom_groups,
        won: &routing.won,
        bindings: &mut routing.custom_chains,
        at: ms(0),
        now: ms(0),
    };
    let refused = scopes.ownership(&leaf, &parent, true);
    assert_eq!(refused.ownership, Some(Ownership::Forwarded));
    assert_eq!(refused.winner, None);
    assert_eq!(routing.custom_chains.len(), chain::MAX_CHAIN_BINDINGS);
    assert!(!routing.custom_chains.contains_key(&ticket.candidate));
}

#[test]
fn retained_scope_tokens_cannot_be_substituted_between_native_parents() {
    let (mut routing, first, _) = custom_pair(Delivery::Reserve);
    key(&mut routing, &first, 1, 0, true);
    let old = request(&mut routing, 0);
    let old_tokens = routing.custom_chains[&old.candidate].tokens.clone();
    key(&mut routing, &first, 2, 1, false);
    key(&mut routing, &first, 3, 2, true);
    assert!(routing.recognition_requests(ms(2)).unwrap().is_empty());
    key(&mut routing, &first, 4, 3, false);
    key(&mut routing, &first, 5, 4, true);
    let next = request(&mut routing, 4);
    routing
        .custom_chains
        .get_mut(&next.candidate)
        .unwrap()
        .tokens = old_tokens;
    assert!(
        routing
            .recognition_response(reply(&next, Phase::Discrete), ms(5))
            .is_err()
    );
    assert!(routing.take_dispatches(ms(5)).is_empty());
}
