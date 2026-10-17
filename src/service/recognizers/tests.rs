//! Delivery/cleanup authority models. These do not establish live logind/root acceptance.
use super::*;
use crate::{
    config::{Config, Millis},
    input::{Change, KeyState, Source},
    input_events::Event,
    recognition::{Cause, Definition, InputKind, Phase},
};
fn ms(at: u64) -> Duration {
    Duration::from_millis(at)
}
fn fence() -> Fence {
    Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    }
}
fn ticket(id: u64, candidate: u64) -> Box<Ticket> {
    Box::new(Ticket {
        fence: fence(),
        id,
        candidate,
        revision: 1,
        rule: "hold-rule".into(),
        recognizer: "hold".into(),
        cause: Cause::Frame,
        requested_at: ms(100),
        deadline: ms(200),
        input: Event::Frame {
            source: Source {
                device: 1,
                generation: 1,
                seat: "seat0".into(),
            },
            at: ms(100),
            sequence: id,
            selectors: vec!["keyboard".into()],
            changes: vec![Change::Key {
                code: 30,
                state: KeyState::Press,
            }],
            contacts: vec![],
            contacts_started: vec![],
            contacts_ended: vec![],
            ended_contacts: vec![],
        },
    })
}
fn proposal(ticket: &Ticket) -> Proposal {
    Proposal {
        fence: ticket.fence,
        ticket: ticket.id,
        phase: Phase::Pending,
        data: serde_json::Value::Null,
    }
}
fn ledger() -> Ledger {
    let mut ledger = Ledger::default();
    ledger.request(true).unwrap();
    assert!(ledger.acknowledge_ready(true));
    ledger
}

#[test]
fn composed_source_delivery_requires_every_exact_binding_and_the_current_rule_revision() {
    let source = crate::lua::Definition::compile(
        include_str!("../../../examples/lua/composed_recognizers.lua"),
        "composed.lua",
    )
    .unwrap();
    let config = source.validate().unwrap();
    assert!(bindings_available(Some(&config), &source.recognizers));
    assert!(!bindings_available(Some(&config), &source.recognizers[..1]));
    for name in ["first", "last"] {
        let mut ticket = ticket(1, 1);
        ticket.rule = "composed".into();
        ticket.recognizer = name.into();
        validate_delivery(
            &config,
            &source.recognizers,
            fence(),
            "seat0",
            1,
            &ticket,
            ms(101),
        )
        .unwrap();
        assert!(
            validate_delivery(
                &config,
                &source.recognizers,
                fence(),
                "seat0",
                2,
                &ticket,
                ms(101)
            )
            .is_err()
        );
        let mut changed = source.recognizers.clone();
        changed
            .iter_mut()
            .find(|b| b.definition.name == name)
            .unwrap()
            .definition
            .continuous = true;
        assert!(
            validate_delivery(&config, &changed, fence(), "seat0", 1, &ticket, ms(101)).is_err()
        );
    }
}

#[test]
fn readiness_needs_native_ack_and_retirement_cannot_reactivate_the_same_vm_fence() {
    let mut ledger = Ledger::default();
    assert!(ledger.issue(ticket(1, 1)).is_err());
    ledger.request(false).unwrap();
    ledger.request(true).unwrap();
    assert!(ledger.issue(ticket(1, 1)).is_err());
    assert!(ledger.acknowledge_ready(true));
    ledger.issue(ticket(1, 1)).unwrap();
    ledger.request(false).unwrap();
    assert!(
        !ledger.acknowledge_ready(true),
        "an old native acknowledgement cannot restore a retired helper"
    );
    assert!(ledger.request(true).is_err());
    assert!(ledger.prepare(proposal(&ticket(1, 1)), ms(101)).is_err());
}

#[test]
fn proposals_require_exact_delivery_fence_deadline_and_single_use() {
    let mut ledger = ledger();
    let ticket = ticket(1, 1);
    ledger.issue(ticket.clone()).unwrap();
    assert!(ledger.prepare(proposal(&ticket), ms(101)).is_err());
    ledger.delivered(1).unwrap();
    let mut foreign = proposal(&ticket);
    foreign.fence.session += 1;
    assert!(ledger.prepare(foreign, ms(101)).is_err());
    assert!(ledger.prepare(proposal(&ticket), ms(200)).is_err());
    ledger.prepare(proposal(&ticket), ms(102)).unwrap();
    assert!(ledger.prepare(proposal(&ticket), ms(103)).is_err());
    assert_eq!(ledger.tickets[&1].ticket.deadline, ms(200));
    assert!(ledger.receipt(1).is_err());
    ledger.tickets.get_mut(&1).unwrap().sent = true;
    assert!(ledger.receipt(1).unwrap());
    assert!(ledger.receipt(1).is_err());
    assert!(
        ledger.issue(ticket).is_err(),
        "old ticket identities cannot reopen"
    );
}

#[test]
fn delivering_a_later_ticket_does_not_authorize_an_undelivered_earlier_one() {
    let mut ledger = ledger();
    ledger.issue(ticket(1, 1)).unwrap();
    ledger.issue(ticket(2, 1)).unwrap();
    ledger.delivered(2).unwrap();
    assert!(ledger.prepare(proposal(&ticket(1, 1)), ms(101)).is_err());
    ledger.prepare(proposal(&ticket(2, 1)), ms(101)).unwrap();
}

#[test]
fn cleanup_acknowledges_the_exact_native_identity_and_releases_one_reserved_slot() {
    let mut ledger = ledger();
    let ticket = ticket(1, 1);
    ledger.issue(ticket.clone()).unwrap();
    ledger.delivered(1).unwrap();
    let cancellation = Cancellation::from_ticket(&ticket, Retirement::Completed);
    assert!(
        ledger.cleaned(&cancellation).is_err(),
        "client cannot invent retirement"
    );
    assert!(ledger.cancel(cancellation.clone(), ms(110)).unwrap());
    assert!(ledger.tickets.is_empty());
    assert_eq!(ledger.candidates.len(), 1);
    for field in [
        "candidate",
        "revision",
        "rule",
        "recognizer",
        "source",
        "through",
        "reason",
        "fence",
    ] {
        let mut changed = cancellation.clone();
        match field {
            "candidate" => changed.candidate += 1,
            "revision" => changed.revision += 1,
            "rule" => changed.rule.push('x'),
            "recognizer" => changed.recognizer.push('x'),
            "source" => changed.source.generation += 1,
            "through" => changed.through += 1,
            "reason" => changed.reason = Retirement::Cancelled,
            "fence" => changed.fence.session += 1,
            _ => unreachable!(),
        }
        assert!(ledger.cleaned(&changed).is_err(), "changed {field}");
    }
    ledger.cleaned(&cancellation).unwrap();
    assert!(ledger.candidates.is_empty());
    assert!(ledger.cleaned(&cancellation).is_err());
}

#[test]
fn native_cancellation_before_delivery_requires_no_vm_cleanup_and_purges_proposals() {
    let mut ledger = ledger();
    let ticket = ticket(1, 1);
    ledger.issue(ticket.clone()).unwrap();
    let cancellation = Cancellation::from_ticket(&ticket, Retirement::SourceLost);
    assert!(!ledger.cancel(cancellation.clone(), ms(110)).unwrap());
    assert!(ledger.candidates.is_empty() && ledger.tickets.is_empty());
    assert!(ledger.cleaned(&cancellation).is_err());
    assert!(ledger.prepare(proposal(&ticket), ms(111)).is_err());
}

#[test]
fn cancellation_can_cover_native_tickets_that_were_never_sent_to_the_broker() {
    let mut ledger = ledger();
    let ticket = ticket(1, 1);
    ledger.issue(ticket.clone()).unwrap();
    ledger.delivered(1).unwrap();
    let mut cancellation = Cancellation::from_ticket(&ticket, Retirement::RuleChanged);
    cancellation.through = 7;
    assert!(ledger.cancel(cancellation.clone(), ms(110)).unwrap());
    assert_eq!(
        ledger.candidates[&1].cancellation.as_ref().unwrap().0,
        cancellation
    );
    ledger.cleaned(&cancellation).unwrap();
}

#[test]
fn cancellation_can_overtake_only_a_known_sent_worker_receipt() {
    let mut ledger = ledger();
    let ticket = ticket(1, 1);
    ledger.issue(ticket.clone()).unwrap();
    ledger.delivered(1).unwrap();
    ledger.prepare(proposal(&ticket), ms(101)).unwrap();
    ledger.tickets.get_mut(&1).unwrap().sent = true;
    let cancellation = Cancellation::from_ticket(&ticket, Retirement::Completed);
    ledger.cancel(cancellation.clone(), ms(102)).unwrap();
    ledger.cleaned(&cancellation).unwrap();
    assert_eq!(ledger.retired_replies.len(), 1);
    assert!(ledger.receipt(2).is_err());
    assert!(!ledger.receipt(1).unwrap());
    assert!(ledger.receipt(1).is_err());
    assert!(ledger.retired_replies.is_empty());
}

#[test]
fn native_cancellation_discards_a_known_inflight_client_proposal_once() {
    let mut ledger = ledger();
    let ticket = ticket(1, 1);
    ledger.issue(ticket.clone()).unwrap();
    ledger.delivered(1).unwrap();
    let cancellation = Cancellation::from_ticket(&ticket, Retirement::SourceLost);
    ledger.cancel(cancellation.clone(), ms(102)).unwrap();
    assert_eq!(ledger.retired_proposals.len(), 1);
    let mut foreign = proposal(&ticket);
    foreign.fence.session += 1;
    assert!(ledger.prepare(foreign, ms(103)).is_err());
    ledger.prepare(proposal(&ticket), ms(103)).unwrap();
    assert!(ledger.tickets.is_empty() && ledger.retired_proposals.is_empty());
    assert!(ledger.prepare(proposal(&ticket), ms(104)).is_err());
    ledger.cleaned(&cancellation).unwrap();
}

#[test]
fn cleanup_and_receipt_deadlines_never_wait_indefinitely() {
    let mut ledger = ledger();
    let ticket = ticket(1, 1);
    ledger.issue(ticket.clone()).unwrap();
    ledger.delivered(1).unwrap();
    assert_eq!(ledger.deadline(), Some(ms(450)));
    assert!(!ledger.overdue(ms(449)));
    assert!(ledger.overdue(ms(450)));
    ledger
        .cancel(
            Cancellation::from_ticket(&ticket, Retirement::Expired),
            ms(201),
        )
        .unwrap();
    assert_eq!(ledger.deadline(), Some(ms(1201)));
    assert!(!ledger.overdue(ms(1200)));
    assert!(ledger.overdue(ms(1201)));
}

#[test]
fn undrained_cleanup_and_retired_receipts_reserve_bounded_capacity() {
    let mut ledger = ledger();
    for id in 1..=MAX_PENDING as u64 {
        let ticket = ticket(id, id);
        ledger.issue(ticket.clone()).unwrap();
        ledger.delivered(id).unwrap();
        let cancellation = Cancellation::from_ticket(&ticket, Retirement::Completed);
        ledger.cancel(cancellation, ms(110)).unwrap();
    }
    assert!(ledger.issue(ticket(65, 65)).is_err());
    let cancellation = ledger.candidates[&1]
        .cancellation
        .as_ref()
        .unwrap()
        .0
        .clone();
    ledger.cleaned(&cancellation).unwrap();
    ledger.issue(ticket(65, 65)).unwrap();
    assert_eq!(ledger.candidates.len(), MAX_PENDING);
}

#[test]
fn byte_pressure_cannot_evict_live_tickets() {
    let mut ledger = ledger();
    let mut accepted = 0;
    for id in 1..=MAX_PENDING as u64 {
        let mut ticket = ticket(id, id);
        if let Event::Frame { changes, .. } = &mut ticket.input {
            *changes = vec![Change::Relative { code: 0, value: 1 }; 600];
        }
        match ledger.issue(ticket) {
            Ok(()) => accepted += 1,
            Err(error) => {
                assert!(error.to_string().contains("byte limit"));
                break;
            }
        }
    }
    assert!(accepted > 1 && accepted < MAX_PENDING);
    assert_eq!(ledger.tickets.len(), accepted);
    assert!(ledger.tickets.contains_key(&1));
}

#[test]
fn public_negotiation_requires_source_bindings_policy_and_the_owning_agent() {
    let mut config = Config::preset("desktop").unwrap();
    config.policy.allow_input_events = true;
    let caps = BTreeSet::from([
        Capability::CustomRecognizers,
        Capability::LuaEvents,
        Capability::Observations,
        Capability::Context,
    ]);
    assert!(
        registration_capabilities(None, Some(&config), true, true, true, Role::Agent, &caps)
            .is_ok()
    );
    for missing in [
        Capability::LuaEvents,
        Capability::Context,
        Capability::Observations,
    ] {
        let mut changed = caps.clone();
        changed.remove(&missing);
        assert!(
            registration_capabilities(None, Some(&config), true, true, true, Role::Agent, &changed)
                .is_err()
        );
    }
    assert!(
        registration_capabilities(None, Some(&config), true, false, true, Role::Agent, &caps)
            .is_err()
    );
    assert!(
        registration_capabilities(None, Some(&config), false, true, true, Role::Agent, &caps)
            .is_err()
    );
    assert!(
        registration_capabilities(None, Some(&config), true, true, true, Role::Inspect, &caps)
            .is_err()
    );
    config.policy.allow_input_events = false;
    assert!(
        registration_capabilities(None, Some(&config), true, true, true, Role::Agent, &caps)
            .is_err()
    );
    config.policy.allow_input_events = true;
    config.mode = crate::config::Mode::Route;
    assert!(
        registration_capabilities(
            Some(&config),
            Some(&config),
            true,
            true,
            true,
            Role::Agent,
            &caps
        )
        .is_err()
    );
}

#[test]
fn public_protocol_contains_only_fenced_proposals_and_typed_cleanup() {
    let ticket = ticket(1, 1);
    let cancellation = Cancellation::from_ticket(&ticket, Retirement::Cancelled);
    let request = Request::Recognize {
        fence: fence(),
        proposal: proposal(&ticket),
    };
    request.validate().unwrap();
    assert_eq!(
        serde_json::from_slice::<Request>(&serde_json::to_vec(&request).unwrap()).unwrap(),
        request
    );
    let mut value = serde_json::to_value(&request).unwrap();
    value["proposal"]["source"] = serde_json::json!({"device":2});
    assert!(serde_json::from_value::<Request>(value).is_err());
    let mut bad = proposal(&ticket);
    bad.fence.session += 1;
    assert!(
        Request::Recognize {
            fence: fence(),
            proposal: bad
        }
        .validate()
        .is_err()
    );
    for reply in [
        Reply::Recognition {
            fence: fence(),
            ticket,
        },
        Reply::RecognitionCancelled {
            fence: fence(),
            cancellation: cancellation.clone(),
        },
        Reply::RecognizerReady {
            fence: fence(),
            ready: true,
        },
        Reply::RecognitionResult {
            fence: fence(),
            ticket: 1,
            error: None,
        },
    ] {
        reply.validate().unwrap();
        assert_eq!(
            serde_json::from_slice::<Reply>(&serde_json::to_vec(&reply).unwrap()).unwrap(),
            reply
        );
    }
    Request::RecognitionCancelled {
        fence: fence(),
        cancellation,
    }
    .validate()
    .unwrap();
    let definition = Definition {
        name: "hold".into(),
        selectors: BTreeSet::from(["keyboard".into()]),
        inputs: BTreeSet::from([InputKind::Key]),
        reply_within: Millis(100),
        continuous: false,
    };
    super::tests::ticket(1, 1)
        .validate_for(&definition, ms(101))
        .unwrap();
}

#[test]
fn delivery_requires_the_exact_source_binding_active_rule_and_current_revision() {
    let definition = Definition {
        name: "hold".into(),
        selectors: BTreeSet::from(["keyboard".into()]),
        inputs: BTreeSet::from([InputKind::Key]),
        reply_within: Millis(100),
        continuous: false,
    };
    let mut config = Config::preset("desktop").unwrap();
    config.policy.allow_input_events = true;
    config.rules[0].id = "hold-rule".into();
    config.rules[0].enabled = true;
    config.rules[0].trigger = crate::config::Trigger::Custom {
        recognizer: definition.clone(),
        timeout: Millis(1000),
        wake: None,
    };
    config.validate().unwrap();
    let binding = crate::lua::RecognizerBinding {
        subscription: 1,
        definition,
    };
    assert!(bindings_available(
        Some(&config),
        std::slice::from_ref(&binding)
    ));
    assert!(!bindings_available(Some(&config), &[]));
    let mut changed_binding = binding.clone();
    changed_binding.definition.continuous = true;
    assert!(!bindings_available(
        Some(&config),
        std::slice::from_ref(&changed_binding)
    ));
    let input = ticket(1, 1);
    validate_delivery(
        &config,
        std::slice::from_ref(&binding),
        fence(),
        "seat0",
        1,
        &input,
        ms(101),
    )
    .unwrap();
    assert!(validate_delivery(&config, &[], fence(), "seat0", 1, &input, ms(101)).is_err());
    assert!(
        validate_delivery(
            &config,
            std::slice::from_ref(&changed_binding),
            fence(),
            "seat0",
            1,
            &input,
            ms(101)
        )
        .is_err()
    );
    for field in [
        "fence",
        "seat",
        "revision",
        "rule",
        "selector",
        "undeclared_input",
        "deadline",
    ] {
        let mut changed = input.clone();
        match field {
            "fence" => changed.fence.session += 1,
            "seat" => {
                if let Event::Frame { source, .. } = &mut changed.input {
                    source.seat = "seat1".into();
                }
            }
            "revision" => changed.revision += 1,
            "rule" => changed.rule.push('x'),
            "selector" => {
                if let Event::Frame { selectors, .. } = &mut changed.input {
                    selectors.push("other".into());
                }
            }
            "undeclared_input" => {
                if let Event::Frame { changes, .. } = &mut changed.input {
                    changes.push(Change::Relative { code: 0, value: 1 });
                }
            }
            "deadline" => changed.deadline = ms(201),
            _ => unreachable!(),
        }
        assert!(
            validate_delivery(
                &config,
                std::slice::from_ref(&binding),
                fence(),
                "seat0",
                1,
                &changed,
                ms(101)
            )
            .is_err(),
            "changed {field}"
        );
    }
    config.rules[0].enabled = false;
    assert!(
        validate_delivery(
            &config,
            std::slice::from_ref(&binding),
            fence(),
            "seat0",
            1,
            &input,
            ms(101)
        )
        .is_err()
    );
}

#[test]
fn cancelling_one_candidate_removes_only_its_queued_public_input() {
    let (_parent, child) = Channel::pair().unwrap();
    let mut connection = Connection {
        channel: child,
        opened: ms(0),
        bound: None,
        outgoing: VecDeque::new(),
        bytes: 0,
        administrator: false,
        waiting_reload: false,
    };
    let metadata = crate::recognition::Metadata {
        source: ticket(1, 1).input.source().clone(),
        at: ms(100),
        selectors: vec!["keyboard".into()],
        reason: crate::input_events::ResetReason::Removed,
        metadata: None,
    };
    connection
        .push(Reply::RecognizerMetadata {
            fence: fence(),
            metadata: Box::new(metadata),
        })
        .unwrap();
    connection
        .push(Reply::Recognition {
            fence: fence(),
            ticket: ticket(1, 1),
        })
        .unwrap();
    connection
        .push(Reply::Recognition {
            fence: fence(),
            ticket: ticket(2, 2),
        })
        .unwrap();
    connection
        .push(Reply::RecognizerReady {
            fence: fence(),
            ready: true,
        })
        .unwrap();
    connection.purge_recognitions(Some(1)).unwrap();
    assert_eq!(connection.outgoing.len(), 3);
    assert!(matches!(&connection.outgoing[1], Reply::Recognition { ticket, .. } if ticket.id == 2));
    connection.purge_recognitions(None).unwrap();
    assert_eq!(connection.outgoing.len(), 1);
    assert_eq!(
        connection.bytes,
        connection
            .outgoing
            .iter()
            .map(reply_size)
            .sum::<io::Result<usize>>()
            .unwrap()
    );
}

#[test]
fn metadata_delivery_requires_exact_source_bindings_scope_seat_and_original_age() {
    use crate::{
        input::{Device, EV_KEY},
        input_events::{DeviceInfo, ResetReason},
        recognition::Metadata,
    };
    let mut config = Config::preset("desktop").unwrap();
    config.policy.allow_input_events = true;
    config.devices.get_mut("keyboard").unwrap().keys = vec![crate::config::Key(30)];
    let definition = Definition {
        name: "hold".into(),
        selectors: BTreeSet::from(["keyboard".into()]),
        inputs: BTreeSet::from([InputKind::Key]),
        reply_within: Millis(100),
        continuous: false,
    };
    config.rules[0].trigger = crate::config::Trigger::Custom {
        recognizer: definition.clone(),
        timeout: Millis(1000),
        wake: None,
    };
    let binding = crate::lua::RecognizerBinding {
        subscription: 1,
        definition,
    };
    let source = ticket(1, 1).input.source().clone();
    let device = Device {
        source: source.clone(),
        identity: "model".into(),
        name: "model".into(),
        path: "model".into(),
        physical: String::new(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        axes: BTreeMap::new(),
        capabilities: BTreeMap::from([(EV_KEY, vec![30])]),
        virtual_device: false,
    };
    let metadata = Metadata {
        source,
        at: ms(100),
        selectors: vec!["keyboard".into()],
        reason: ResetReason::Attached,
        metadata: Some(Box::new(DeviceInfo {
            device,
            selectors: vec!["keyboard".into()],
            mapping: None,
        })),
    };
    validate_metadata(
        &config,
        std::slice::from_ref(&binding),
        "seat0",
        &metadata,
        ms(101),
    )
    .unwrap();
    assert!(validate_metadata(&config, &[], "seat0", &metadata, ms(101)).is_err());
    let mut changed = binding.clone();
    changed.definition.continuous = true;
    assert!(validate_metadata(&config, &[changed], "seat0", &metadata, ms(101)).is_err());
    assert!(
        validate_metadata(
            &config,
            std::slice::from_ref(&binding),
            "seat1",
            &metadata,
            ms(101)
        )
        .is_err()
    );
    assert!(
        validate_metadata(
            &config,
            std::slice::from_ref(&binding),
            "seat0",
            &metadata,
            ms(351)
        )
        .is_err()
    );
    let reply = Reply::RecognizerMetadata {
        fence: fence(),
        metadata: Box::new(metadata),
    };
    reply.validate().unwrap();
    assert_eq!(reply.fence(), Some(fence()));
    assert_eq!(
        serde_json::from_slice::<Reply>(&serde_json::to_vec(&reply).unwrap()).unwrap(),
        reply
    );
}
