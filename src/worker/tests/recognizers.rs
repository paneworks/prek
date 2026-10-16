//! Modeled source/context authority; no device nodes or privileged operations.
use super::*;
use crate::{
    protocol::Capability,
    recognition::{Definition, InputKind, Phase, Proposal, Ticket},
};

fn profile() -> Config {
    let mut config = config();
    config.policy.allow_input_events = true;
    config.rules[0].scope = crate::config::Scope::Everywhere;
    config.rules[0].trigger = Trigger::Custom {
        recognizer: Definition {
            name: "hold".into(),
            selectors: BTreeSet::from(["keyboard".into()]),
            inputs: BTreeSet::from([InputKind::Key]),
            reply_within: Millis(100),
            continuous: false,
        },
        timeout: Millis(1000),
        wake: Some(Millis(20)),
    };
    config
}
#[test]
fn startup_preflight_validates_declarations_without_inventing_a_coordinator_fence() {
    let mut config = profile();
    assert!(Engine::new(config.clone()).is_err());
    super::super::preflight(&config).unwrap();
    config.policy.allow_input_events = false;
    assert!(super::super::preflight(&config).is_err());
}
fn owner(at: u64) -> Subscription {
    let mut owner = input_subscription("seat0", at);
    owner.capabilities.remove(&Capability::InputEvents);
    owner.capabilities.insert(Capability::CustomRecognizers);
    owner
}
fn tickets(registry: &Registry) -> Vec<Ticket> {
    registry
        .reports
        .iter()
        .filter_map(|(report, _)| match report {
            Report::Recognition { ticket, .. } => Some(ticket.as_ref().clone()),
            _ => None,
        })
        .collect()
}
fn reply(ticket: &Ticket, phase: Phase) -> Proposal {
    Proposal {
        fence: ticket.fence,
        ticket: ticket.id,
        phase,
        data: serde_json::json!({"model": true}),
    }
}
fn clear(registry: &mut Registry) {
    registry.reports.clear();
    registry.bytes = 0;
}
fn metadata(registry: &Registry) -> Vec<crate::recognition::Metadata> {
    registry
        .reports
        .iter()
        .filter_map(|(report, _)| match report {
            Report::RecognizerMetadata { metadata, .. } => Some(metadata.as_ref().clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn custom_metadata_projects_only_declared_selectors_without_held_state() {
    let mut config = profile();
    config
        .devices
        .insert("broader".into(), config.devices["keyboard"].clone());
    config.policy.allowed_devices.push("broader".into());
    let mut registry = registry(config);
    key(&mut registry, source(1, "seat0"), 90, 1, true, 90);
    update(&mut registry, 1, vec![owner(100)], 100);
    let info = metadata(&registry).pop().unwrap();
    assert_eq!(info.selectors, ["keyboard"]);
    assert_eq!(info.metadata.as_ref().unwrap().selectors, ["keyboard"]);
    assert_eq!(info.source, source(1, "seat0"));
    assert!(inputs(&registry).is_empty());
    let wire = serde_json::to_value(&info).unwrap();
    assert!(wire.get("keys").is_none() && wire.get("contacts").is_none());
    clear(&mut registry);
    key(&mut registry, source(1, "seat0"), 101, 2, false, 101);
    tick(&mut registry, 102);
    assert!(metadata(&registry).is_empty());
}

#[test]
fn custom_catalog_reconciles_replacements_and_exact_removals_before_tickets() {
    let mut registry = registry(profile());
    update(&mut registry, 1, vec![owner(100)], 100);
    let mut device = metadata(&registry)[0]
        .metadata
        .as_ref()
        .unwrap()
        .device
        .clone();
    clear(&mut registry);
    registry
        .push(
            Record::Reconcile {
                source: device.source.clone(),
                at: ms(101),
                keys: BTreeSet::from([30]),
                contacts: vec![],
            },
            ms(101),
        )
        .unwrap();
    assert_eq!(
        metadata(&registry)[0].reason,
        crate::input_events::ResetReason::Reconciled
    );
    clear(&mut registry);
    device.source.generation = 2;
    registry
        .push(
            Record::Device {
                device: device.clone(),
                keys: BTreeSet::new(),
                contacts: vec![],
            },
            ms(102),
        )
        .unwrap();
    let updates = metadata(&registry);
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0].source.generation, 1);
    assert_eq!(updates[0].reason, crate::input_events::ResetReason::Removed);
    assert!(updates[0].metadata.is_none());
    assert_eq!(updates[1].source.generation, 2);
    assert_eq!(
        updates[1].reason,
        crate::input_events::ResetReason::Attached
    );
    clear(&mut registry);
    registry
        .push(
            Record::Remove {
                source: device.source,
                at: ms(103),
            },
            ms(103),
        )
        .unwrap();
    assert_eq!(
        metadata(&registry)[0].reason,
        crate::input_events::ResetReason::Removed
    );
    assert!(inputs(&registry).is_empty());
}

#[test]
fn custom_metadata_requires_every_capability_and_does_not_duplicate_ordinary_resets() {
    for denied in [
        Capability::CustomRecognizers,
        Capability::Context,
        Capability::Observations,
        Capability::LuaEvents,
    ] {
        let mut registry = registry(profile());
        let mut subscription = owner(100);
        subscription.capabilities.remove(&denied);
        update(&mut registry, 1, vec![subscription], 100);
        assert!(metadata(&registry).is_empty());
    }
    let mut registry = registry(profile());
    let mut subscription = owner(100);
    subscription.capabilities.insert(Capability::InputEvents);
    update(&mut registry, 1, vec![subscription], 100);
    assert!(!inputs(&registry).is_empty());
    assert!(metadata(&registry).is_empty());
}

#[test]
fn excessive_custom_metadata_ends_only_its_registration() {
    let mut registry = registry(profile());
    let mut other = owner(100);
    other.seat = "seat1".into();
    other.context.as_mut().unwrap().binding.seat = "seat1".into();
    update(&mut registry, 1, vec![owner(100), other], 100);
    let mut info = metadata(&registry)
        .into_iter()
        .find(|info| info.source.seat == "seat0")
        .unwrap();
    info.metadata.as_mut().unwrap().device.capabilities =
        (0..32).map(|kind| (kind, (0..4096).collect())).collect();
    clear(&mut registry);
    registry
        .queue(
            Report::RecognizerMetadata {
                subscription: owner(100).identity(),
                metadata: Box::new(info),
            },
            ms(101),
        )
        .unwrap();
    assert!(!registry.observers.contains_key("seat0"));
    assert!(registry.observers.contains_key("seat1"));
    assert!(matches!(&registry.reports[0].0, Report::Ended { .. }));
}
fn issued() -> (Registry, Ticket) {
    let mut registry = registry(profile());
    update(&mut registry, 1, vec![owner(100)], 100);
    key(&mut registry, source(1, "seat0"), 101, 1, true, 102);
    tick(&mut registry, 103);
    let ticket = tickets(&registry).pop().unwrap();
    (registry, ticket)
}

#[test]
fn explicit_private_readiness_precedes_tickets_and_does_not_enable_broad_input() {
    let mut registry = registry(profile());
    update(
        &mut registry,
        1,
        vec![input_subscription("seat0", 100)],
        100,
    );
    key(&mut registry, source(1, "seat0"), 101, 1, true, 102);
    tick(&mut registry, 103);
    assert!(
        tickets(&registry).is_empty(),
        "ordinary InputEvents is not recognizer readiness"
    );
    key(&mut registry, source(1, "seat0"), 104, 2, false, 104);
    clear(&mut registry);
    let mut owner = owner(105);
    owner.context.as_mut().unwrap().snapshot.revision = 2;
    update(&mut registry, 2, vec![owner], 105);
    key(&mut registry, source(1, "seat0"), 106, 3, true, 109);
    assert!(
        tickets(&registry).is_empty(),
        "input must be drained before ticket publication"
    );
    tick(&mut registry, 110);
    let reports: Vec<_> = registry.reports.iter().map(|(report, _)| report).collect();
    assert!(matches!(
        reports[0],
        Report::RecognizerReady { ready: true, .. }
    ));
    assert_eq!(tickets(&registry).len(), 1);
    assert!(inputs(&registry).is_empty());
    assert_eq!(tickets(&registry)[0].requested_at, ms(106));
    assert_eq!(tickets(&registry)[0].deadline, ms(206));
}

#[test]
fn revocation_purges_unsent_tickets_and_reenable_suppresses_held_input() {
    let (mut registry, ticket) = issued();
    let mut denied = owner(104);
    denied.capabilities.remove(&Capability::CustomRecognizers);
    denied.context.as_mut().unwrap().snapshot.revision = 2;
    update(&mut registry, 2, vec![denied], 104);
    assert!(tickets(&registry).is_empty());
    assert!(metadata(&registry).is_empty());
    assert!(
        registry
            .reports
            .iter()
            .any(|(r, _)| matches!(r, Report::RecognizerReady { ready: false, .. }))
    );
    assert!(registry.reports.iter().any(|(r, _)| matches!(r, Report::RecognitionCancelled { cancellation, .. } if cancellation.candidate == ticket.candidate && cancellation.through == ticket.id)));
    assert!(
        registry
            .recognize(
                "seat0",
                ticket.fence,
                reply(&ticket, Phase::Discrete),
                ms(105),
                ms(105)
            )
            .is_err()
    );
    let mut enabled = owner(106);
    enabled.context.as_mut().unwrap().snapshot.revision = 3;
    update(&mut registry, 3, vec![enabled], 106);
    assert_eq!(metadata(&registry).len(), 1);
    clear(&mut registry);
    tick(&mut registry, 130);
    assert!(tickets(&registry).is_empty());
    key(&mut registry, source(1, "seat0"), 131, 2, false, 131);
    key(&mut registry, source(1, "seat0"), 132, 3, true, 133);
    tick(&mut registry, 134);
    assert!(tickets(&registry)[0].candidate > ticket.candidate);
}

#[test]
fn replies_bind_seat_fence_single_use_and_native_outcome() {
    let (mut registry, ticket) = issued();
    assert!(
        registry
            .recognize(
                "seat1",
                ticket.fence,
                reply(&ticket, Phase::Discrete),
                ms(104),
                ms(104)
            )
            .is_err()
    );
    let mut foreign = ticket.fence;
    foreign.session += 1;
    assert!(
        registry
            .recognize(
                "seat0",
                foreign,
                reply(&ticket, Phase::Discrete),
                ms(104),
                ms(104)
            )
            .is_err()
    );
    registry
        .recognize(
            "seat0",
            ticket.fence,
            reply(&ticket, Phase::Discrete),
            ms(104),
            ms(107),
        )
        .unwrap();
    assert!(
        matches!(outcomes(&registry).as_slice(), [Event::Matched { evidence: Evidence::Custom { data, .. }, ordinal: 1, .. }] if data["model"] == true)
    );
    assert!(
        tickets(&registry).is_empty(),
        "completion cancels the retained ticket"
    );
    assert!(
        registry
            .recognize(
                "seat0",
                ticket.fence,
                reply(&ticket, Phase::Discrete),
                ms(108),
                ms(108)
            )
            .is_err()
    );
    assert_eq!(outcomes(&registry).len(), 1);
}

#[test]
fn wall_clock_expiry_cannot_be_hidden_by_an_earlier_input_watermark() {
    let (mut registry, ticket) = issued();
    assert!(
        registry
            .recognize(
                "seat0",
                ticket.fence,
                reply(&ticket, Phase::Discrete),
                ms(104),
                ms(202)
            )
            .is_err()
    );
    assert!(
        !outcomes(&registry)
            .iter()
            .any(|event| matches!(event, Event::Matched { .. }))
    );
    assert!(tickets(&registry).is_empty());
    assert!(
        registry
            .reports
            .iter()
            .any(|(r, _)| matches!(r, Report::RecognitionCancelled { .. }))
    );
}

#[test]
fn callback_validation_does_not_move_input_timers_past_the_readers_watermark() {
    let (mut registry, ticket) = issued();
    registry
        .recognize(
            "seat0",
            ticket.fence,
            reply(&ticket, Phase::Pending),
            ms(104),
            ms(107),
        )
        .unwrap();
    clear(&mut registry);
    key(&mut registry, source(1, "seat0"), 105, 2, false, 108);
    tick(&mut registry, 106);
    assert_eq!(
        tickets(&registry).len(),
        1,
        "later input must not be treated as a clock regression"
    );
    assert_eq!(tickets(&registry)[0].candidate, ticket.candidate);
    assert!(outcomes(&registry).is_empty());
}

#[test]
fn replacement_requires_a_new_fence_and_does_not_restore_old_helper_readiness() {
    let (mut registry, old) = issued();
    registry.install(profile(), 2, ms(104)).unwrap();
    assert!(registry.observers.is_empty());
    assert!(registry.reports.is_empty());
    assert!(
        registry
            .recognize(
                "seat0",
                old.fence,
                reply(&old, Phase::Discrete),
                ms(105),
                ms(105)
            )
            .is_err()
    );
    let mut next = owner(106);
    next.fence.configuration = 2;
    next.capabilities.remove(&Capability::CustomRecognizers);
    update(&mut registry, 2, vec![next.clone()], 106);
    key(&mut registry, source(1, "seat0"), 107, 2, false, 107);
    key(&mut registry, source(1, "seat0"), 108, 3, true, 108);
    tick(&mut registry, 109);
    assert!(tickets(&registry).is_empty());
    next.capabilities.insert(Capability::CustomRecognizers);
    next.context.as_mut().unwrap().snapshot.revision += 1;
    next.context.as_mut().unwrap().snapshot.captured_at = ms(110);
    update(&mut registry, 3, vec![next], 110);
    key(&mut registry, source(1, "seat0"), 111, 4, false, 111);
    key(&mut registry, source(1, "seat0"), 112, 5, true, 112);
    tick(&mut registry, 113);
    assert_eq!(tickets(&registry)[0].fence.configuration, 2);
}

#[test]
fn stationary_wakes_keep_original_frame_identity_and_deadlines() {
    let (mut registry, ticket) = issued();
    registry
        .recognize(
            "seat0",
            ticket.fence,
            reply(&ticket, Phase::Pending),
            ms(104),
            ms(104),
        )
        .unwrap();
    clear(&mut registry);
    tick(&mut registry, 121);
    let wake = tickets(&registry).pop().unwrap();
    assert_eq!(wake.cause, crate::recognition::Cause::Timer);
    assert_eq!(wake.input.at(), ticket.input.at());
    assert_eq!(wake.candidate, ticket.candidate);
    assert_eq!(wake.requested_at, ms(121));
    assert_eq!(wake.deadline, ms(221));
    assert!(
        matches!(wake.input, crate::input_events::Event::Frame { changes, .. } if changes.is_empty())
    );
}

#[test]
fn source_loss_and_rule_edits_retire_queued_custom_work() {
    for source_loss in [false, true] {
        let (mut registry, ticket) = issued();
        if source_loss {
            registry
                .push(
                    Record::Remove {
                        source: ticket.input.source().clone(),
                        at: ms(104),
                    },
                    ms(104),
                )
                .unwrap();
        } else {
            let id = registry.config.rules[0].id.clone();
            registry
                .transact_rules(
                    "seat0",
                    ticket.fence,
                    &crate::rule_transactions::Transaction {
                        revision: 1,
                        changes: vec![crate::rule_transactions::Change::Enabled {
                            id,
                            enabled: false,
                        }],
                    },
                    ms(104),
                )
                .unwrap();
            assert_eq!(registry.observers["seat0"].recognizer_ready, Some(true));
        }
        assert!(tickets(&registry).is_empty());
        assert!(registry.reports.iter().any(|(r, _)| matches!(r, Report::RecognitionCancelled { cancellation, .. } if cancellation.candidate == ticket.candidate)));
        assert!(
            registry
                .recognize(
                    "seat0",
                    ticket.fence,
                    reply(&ticket, Phase::Discrete),
                    ms(105),
                    ms(105)
                )
                .is_err()
        );
    }
}

#[test]
fn context_loss_discards_all_custom_work_and_requires_a_new_registration() {
    let (mut registry, ticket) = issued();
    tick(&mut registry, 351);
    assert!(tickets(&registry).is_empty());
    assert!(registry.observers.is_empty());
    assert!(
        registry
            .reports
            .iter()
            .any(|(r, _)| matches!(r, Report::ContextLost { .. }))
    );
    assert!(
        registry
            .recognize(
                "seat0",
                ticket.fence,
                reply(&ticket, Phase::Discrete),
                ms(352),
                ms(352)
            )
            .is_err()
    );
}

#[test]
fn private_replies_wait_for_a_drained_input_watermark_and_return_exact_receipts() {
    let (mut parent, child) = Channel::pair().unwrap();
    let mut stream = Stream::new(profile(), child).unwrap();
    stream.registry = registry(profile());
    let at = crate::ipc::monotonic().unwrap();
    let mut subscription = owner(100);
    subscription.context.as_mut().unwrap().snapshot.captured_at = at;
    stream
        .registry
        .update(
            Subscriptions {
                revision: 1,
                seats: vec![subscription.clone()],
            },
            at,
        )
        .unwrap();
    let press_at = crate::ipc::monotonic().unwrap();
    stream
        .registry
        .push(
            Record::Frame {
                frame: Frame {
                    source: source(1, "seat0"),
                    at: press_at,
                    sequence: 1,
                    events: vec![
                        RawEvent {
                            kind: EV_KEY,
                            code: 30,
                            value: 1,
                        },
                        RawEvent {
                            kind: EV_SYN,
                            code: SYN_REPORT,
                            value: 0,
                        },
                    ],
                },
            },
            press_at,
        )
        .unwrap();
    stream
        .push(Record::Tick {
            at: crate::ipc::monotonic().unwrap(),
        })
        .unwrap();
    stream.flush().unwrap();
    let mut ticket = None;
    while let Some(packet) = parent.receive::<Message>().unwrap() {
        if let Message::Outcome {
            report: Report::Recognition { ticket: value, .. },
        } = packet.message
        {
            ticket = Some(value);
        }
    }
    let ticket = ticket.unwrap();
    parent
        .send(
            &Command::Recognition {
                seat: subscription.seat.clone(),
                fence: subscription.fence,
                proposal: reply(&ticket, Phase::Discrete),
            },
            crate::ipc::monotonic().unwrap(),
        )
        .unwrap();
    stream.service().unwrap();
    assert_eq!(stream.recognitions.len(), 1);
    assert!(outcomes(&stream.registry).is_empty());
    assert!(stream.deadline().unwrap() <= crate::ipc::monotonic().unwrap());
    stream
        .push(Record::Tick {
            at: crate::ipc::monotonic().unwrap(),
        })
        .unwrap();
    assert!(stream.recognitions.is_empty());
    stream.flush().unwrap();
    assert!(
        matches!(parent.receive::<Message>().unwrap().unwrap().message, Message::RecognitionResult { ticket: id, fence, error: None, .. } if id == ticket.id && fence == ticket.fence)
    );
    let mut matched = 0;
    while let Some(packet) = parent.receive::<Message>().unwrap() {
        if matches!(packet.message, Message::Outcome { report: Report::Observation { event, .. } } if matches!(*event, Event::Matched { .. }))
        {
            matched += 1;
        }
    }
    assert_eq!(matched, 1);
}
