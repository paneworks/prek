//! Complete native plans through the sole worker's private channel and rebuilds.
use super::*;
mod helper;

fn replace(fixture: &mut Fixture, config: &Config, generation: u64) -> Vec<Message> {
    let mut transfer =
        crate::configuration::Transfer::new(toml::to_string(config).unwrap(), generation).unwrap();
    loop {
        let finished = transfer
            .send(&mut fixture.parent, monotonic().unwrap())
            .unwrap();
        fixture.stream.service().unwrap();
        if finished {
            break;
        }
    }
    fixture.drain()
}
fn fresh_source(fixture: &mut Fixture, generation: u64) -> Device {
    fixture
        .stream
        .push(Record::Remove {
            source: device().source,
            at: monotonic().unwrap(),
        })
        .unwrap();
    let mut fresh = device();
    fresh.source.generation = generation;
    fixture
        .stream
        .push(Record::Device {
            device: fresh.clone(),
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    fixture.attach_device(&fresh);
    let messages = fixture.drain();
    fixture.initial.extend(messages);
    fresh
}

fn custom(name: &str) -> Trigger {
    let mut trigger = profile().rules.remove(0).trigger;
    if let Trigger::Custom { recognizer, .. } = &mut trigger {
        recognizer.name = name.into();
    }
    trigger
}
fn config(delivery: Delivery) -> Config {
    let mut config = profile();
    config.timing.decision_window = Millis(300);
    let rule = &mut config.rules[0];
    rule.id = "chain".into();
    rule.contexts = vec![crate::config::Context::Desktop];
    rule.delivery = delivery;
    rule.trigger = Trigger::Sequence {
        steps: vec![custom("first"), custom("last")],
        timeout: Millis(1000),
    };
    config.validate().unwrap();
    config
}
fn tickets(messages: &[Message]) -> Vec<Ticket> {
    messages
        .iter()
        .filter_map(|message| {
            if let Message::Outcome {
                report: Report::Recognition { ticket, .. },
            } = message
            {
                Some(ticket.as_ref().clone())
            } else {
                None
            }
        })
        .collect()
}
fn one(fixture: &mut Fixture, name: &str) -> Ticket {
    fixture.tick();
    let messages = fixture.drain();
    let mut tickets = tickets(&messages);
    assert_eq!(tickets.len(), 1, "{messages:?}");
    let ticket = tickets.pop().unwrap();
    assert_eq!(ticket.recognizer, name);
    ticket
}
fn complete(fixture: &mut Fixture, ticket: &Ticket) -> Vec<Message> {
    fixture.reply(ticket);
    assert_eq!(fixture.stream.pending.len(), 1);
    assert!(fixture.drain().is_empty());
    fixture.tick();
    let messages = fixture.drain();
    assert!(
        messages.iter().any(
            |m| matches!(m,Message::RecognitionResult {ticket:id,error:None,..} if *id==ticket.id)
        ),
        "{messages:?}"
    );
    messages
}
fn dispatches(messages: &[Message]) -> Vec<&crate::rules::Event> {
    messages
        .iter()
        .filter_map(|m| {
            if let Message::Outcome {
                report: Report::Dispatch { event, .. },
            } = m
            {
                Some(event.as_ref())
            } else {
                None
            }
        })
        .collect()
}
fn key(fixture: &mut Fixture, source: &Source, sequence: u64, down: bool) {
    fixture
        .stream
        .push(Record::Frame {
            frame: Frame {
                source: source.clone(),
                sequence,
                at: monotonic().unwrap(),
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

#[test]
fn composed_callbacks_cross_private_commands_with_one_outer_id_and_two_leaf_scopes() {
    for delivery in [Delivery::Observe, Delivery::Buffer, Delivery::Reserve] {
        let mut fixture = Fixture::new(config(delivery), owner());
        fixture.key(1, true);
        let first = one(&mut fixture, "first");
        let messages = complete(&mut fixture, &first);
        assert!(dispatches(&messages).is_empty());
        fixture.key(2, false);
        fixture.key(3, true);
        let last = one(&mut fixture, "last");
        assert_ne!(first.candidate, last.candidate);
        let messages = complete(&mut fixture, &last);
        assert!(
            matches!(&dispatches(&messages)[..],[crate::rules::Event::Matched{id,evidence:crate::rules::Evidence::Sequence{steps},..}]
            if *id!=first.candidate && *id!=last.candidate && steps.len()==2),
            "{messages:?}"
        );
    }
}

#[test]
fn nested_steps_keep_one_outer_result_after_private_worker_compilation() {
    for delivery in [Delivery::Observe, Delivery::Buffer, Delivery::Reserve] {
        let mut profile = config(delivery);
        profile.rules[0].trigger = Trigger::Sequence {
            steps: vec![
                custom("first"),
                Trigger::Sequence {
                    steps: vec![custom("last"), custom("tail")],
                    timeout: Millis(1000),
                },
            ],
            timeout: Millis(1000),
        };
        let mut fixture = Fixture::new(profile, owner());
        let mut leaves = BTreeSet::new();
        for (index, name) in ["first", "last", "tail"].into_iter().enumerate() {
            if index > 0 {
                fixture.key(index as u64 * 2, false);
            }
            fixture.key(index as u64 * 2 + 1, true);
            let ticket = one(&mut fixture, name);
            assert!(leaves.insert(ticket.candidate));
            let messages = complete(&mut fixture, &ticket);
            let dispatched = dispatches(&messages);
            if index < 2 {
                assert!(dispatched.is_empty());
            } else {
                assert!(
                    matches!(&dispatched[..], [crate::rules::Event::Matched { id, evidence: crate::rules::Evidence::Sequence { steps }, .. }]
                    if !leaves.contains(id) && steps.len()==3
                    && steps.iter().zip(["first", "last", "tail"]).all(|(step, expected)|
                        matches!(step, crate::rules::Evidence::Custom { recognizer, .. } if recognizer==expected))),
                    "{messages:?}"
                );
            }
        }
    }
}

#[test]
fn a_cross_source_prefix_is_part_of_the_last_ticket_and_retirement_cancels_it() {
    let mut fixture = Fixture::new(config(Delivery::Reserve), owner());
    fixture.key(1, true);
    let first = one(&mut fixture, "first");
    complete(&mut fixture, &first);
    fixture.key(2, false);
    let mut second = device();
    second.source.device += 1;
    fixture
        .stream
        .push(Record::Device {
            device: second.clone(),
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    fixture.attach_device(&second);
    fixture.drain();
    key(&mut fixture, &second.source, 1, true);
    let last = one(&mut fixture, "last");
    assert_eq!(last.input.source(), &second.source);
    fixture
        .stream
        .push(Record::Remove {
            source: device().source,
            at: monotonic().unwrap(),
        })
        .unwrap();
    fixture.drain();
    fixture.reply(&last);
    fixture.tick();
    let messages = fixture.drain();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::RecognitionResult { error: Some(_), .. }))
    );
    assert!(dispatches(&messages).is_empty());
}

#[test]
fn rejected_new_callback_declarations_preserve_the_current_graph_and_ticket() {
    let mut fixture = Fixture::new(config(Delivery::Reserve), owner());
    fixture.key(1, true);
    let first = one(&mut fixture, "first");
    let mut rule = fixture.stream.config.rules[0].clone();
    rule.trigger = Trigger::Sequence {
        steps: vec![custom("first"), custom("unapproved")],
        timeout: Millis(1000),
    };
    fixture.rules(
        1,
        vec![Change::Replace {
            rule: Box::new(rule),
        }],
    );
    fixture.tick();
    let messages = fixture.drain();
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::RulesChanged {
                revision: 1,
                error: Some(_),
                ..
            }
        )),
        "{messages:?}"
    );
    assert!(
        fixture.stream.seats["seat0"]
            .routing
            .has_source(&device().source)
    );
    complete(&mut fixture, &first);
    fixture.key(2, false);
    fixture.key(3, true);
    let last = one(&mut fixture, "last");
    assert_eq!(dispatches(&complete(&mut fixture, &last)).len(), 1);
}

#[test]
fn disabling_a_composed_rule_retires_every_step_and_requires_fresh_transport() {
    let mut fixture = Fixture::new(config(Delivery::Reserve), owner());
    fixture.key(1, true);
    let first = one(&mut fixture, "first");
    fixture.rules(
        1,
        vec![Change::Enabled {
            id: "chain".into(),
            enabled: false,
        }],
    );
    fixture.tick();
    let messages = fixture.drain();
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::RulesChanged {
                revision: 2,
                error: None,
                ..
            }
        )),
        "{messages:?}"
    );
    assert!(
        !fixture.stream.seats["seat0"]
            .routing
            .has_source(&device().source)
    );
    assert!(messages.iter().any(|m|matches!(m,Message::Outcome{report:Report::RecognitionCancelled{cancellation,..}}if cancellation.candidate==first.candidate)));
    fixture.reply(&first);
    fixture.tick();
    assert!(dispatches(&fixture.drain()).is_empty());
}

#[test]
fn helper_capability_loss_cancels_both_parent_and_current_step_before_regrant() {
    let mut fixture = Fixture::new(config(Delivery::Reserve), owner());
    fixture.key(1, true);
    let first = one(&mut fixture, "first");
    let mut revoked = fixture.owner.clone();
    revoked.capabilities.remove(&Capability::CustomRecognizers);
    fixture.subscribe(revoked, 2);
    let messages = fixture.drain();
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::Outcome {
                report: Report::RecognitionCancelled { .. }
            }
        )),
        "{messages:?}"
    );
    fixture.reply(&first);
    fixture.tick();
    assert!(dispatches(&fixture.drain()).is_empty());
    fixture.subscribe(fixture.owner.clone(), 3);
    fixture.tick();
    assert!(tickets(&fixture.drain()).is_empty());
    fixture.key(2, false);
    fixture.tick();
    assert!(tickets(&fixture.drain()).is_empty());
    fixture.key(3, true);
    let next = one(&mut fixture, "first");
    assert!(next.candidate > first.candidate);
}

#[test]
fn a_complete_private_configuration_reload_rebuilds_the_plan_and_rejects_old_fences() {
    let mut fixture = Fixture::new(config(Delivery::Reserve), owner());
    fixture.key(1, true);
    let old = one(&mut fixture, "first");
    fixture.reply(&old);
    assert_eq!(fixture.stream.pending.len(), 1);
    let next = fixture.stream.config.clone();
    let messages = replace(&mut fixture, &next, 2);
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::Configuration {
                generation: 2,
                error: None
            }
        )),
        "{messages:?}"
    );
    assert_eq!(fixture.stream.generation, 2);
    assert!(fixture.stream.seats.is_empty());
    assert!(fixture.stream.pending.is_empty());
    assert_eq!(fixture.stream.pending_bytes, 0);
    fixture.owner.fence.configuration = 2;
    fixture.owner.context.as_mut().unwrap().snapshot.captured_at = monotonic().unwrap();
    fixture.subscribe(fixture.owner.clone(), 2);
    let fresh = fresh_source(&mut fixture, device().source.generation + 1);
    fixture.reply(&old);
    fixture.tick();
    let messages = fixture.drain();
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::RecognitionResult { error: Some(_), .. })),
        "{messages:?}"
    );
    assert!(dispatches(&messages).is_empty());
    key(&mut fixture, &fresh.source, 1, true);
    let first = one(&mut fixture, "first");
    assert_eq!(first.fence.configuration, 2);
    assert_eq!(first.input.source(), &fresh.source);
    complete(&mut fixture, &first);
    key(&mut fixture, &fresh.source, 2, false);
    key(&mut fixture, &fresh.source, 3, true);
    let last = one(&mut fixture, "last");
    assert_eq!(dispatches(&complete(&mut fixture, &last)).len(), 1);
}

#[test]
fn rejected_private_reload_preserves_the_live_ticket_transport_and_configuration() {
    for bad_timing in [false, true] {
        let mut fixture = Fixture::new(config(Delivery::Buffer), owner());
        fixture.key(1, true);
        let first = one(&mut fixture, "first");
        let mut invalid = fixture.stream.config.clone();
        if bad_timing {
            invalid.timing.join_window = invalid.timing.decision_window;
        } else {
            invalid.policy.allow_input_events = false;
        }
        let messages = replace(&mut fixture, &invalid, 2);
        assert!(
            messages.iter().any(|m| matches!(
                m,
                Message::Configuration {
                    generation: 2,
                    error: Some(_)
                }
            )),
            "{messages:?}"
        );
        assert_eq!(fixture.stream.generation, 1);
        assert!(
            fixture.stream.seats["seat0"]
                .routing
                .has_source(&device().source)
        );
        complete(&mut fixture, &first);
        fixture.key(2, false);
        fixture.key(3, true);
        let last = one(&mut fixture, "last");
        assert_eq!(dispatches(&complete(&mut fixture, &last)).len(), 1);
    }
}

#[test]
fn removed_composed_rules_can_be_restored_only_with_new_transport_and_approved_steps() {
    let mut fixture = Fixture::new(config(Delivery::Reserve), owner());
    let rule = fixture.stream.config.rules[0].clone();
    fixture.key(1, true);
    let old = one(&mut fixture, "first");
    fixture.rules(
        1,
        vec![Change::Remove {
            id: rule.id.clone(),
        }],
    );
    fixture.tick();
    assert!(fixture.drain().iter().any(|m| matches!(
        m,
        Message::RulesChanged {
            revision: 2,
            error: None,
            ..
        }
    )));
    fixture.rules(
        2,
        vec![Change::Replace {
            rule: Box::new(rule),
        }],
    );
    fixture.tick();
    let messages = fixture.drain();
    assert!(
        messages.iter().any(|m| matches!(
            m,
            Message::RulesChanged {
                revision: 3,
                error: None,
                ..
            }
        )),
        "{messages:?}"
    );
    assert!(
        !fixture.stream.seats["seat0"]
            .routing
            .has_source(&device().source)
    );
    let fresh = fresh_source(&mut fixture, device().source.generation + 1);
    fixture.reply(&old);
    fixture.tick();
    assert!(dispatches(&fixture.drain()).is_empty());
    key(&mut fixture, &fresh.source, 1, true);
    let first = one(&mut fixture, "first");
    assert!(first.revision > old.revision);
    complete(&mut fixture, &first);
    key(&mut fixture, &fresh.source, 2, false);
    key(&mut fixture, &fresh.source, 3, true);
    let last = one(&mut fixture, "last");
    assert_eq!(dispatches(&complete(&mut fixture, &last)).len(), 1);
}
