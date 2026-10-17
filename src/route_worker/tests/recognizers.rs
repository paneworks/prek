//! Private-channel fixtures model native readiness without opening device nodes.
mod actual;
mod composed;
mod keys;
mod native_reorder;
mod subscriptions;
use super::*;
use crate::{
    config::{Action, Delivery, Millis, Scope, Selector, Trigger},
    context::{Binding, SessionKind, Snapshot, State},
    input::{EV_KEY, EV_SYN, RawEvent, SYN_REPORT},
    protocol::{Capability, Epoch},
    recognition::{Definition, InputKind, Phase, Proposal, Ticket},
    rule_transactions::{Change, Transaction},
    transport::{Consumer, ConsumerEvidence},
    worker::ProvidedContext,
};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

fn profile() -> Config {
    let mut config = Config::preset("desktop").unwrap();
    config.mode = Mode::Route;
    config.policy.allow_input_events = true;
    config.policy.allow_runtime_rules = true;
    config.policy.allowed_devices = vec!["keyboard".into()];
    config.devices = BTreeMap::from([(
        "keyboard".into(),
        Selector {
            identity: Some("bridge-model".into()),
            ..Selector::default()
        },
    )]);
    config.rules.truncate(1);
    let rule = &mut config.rules[0];
    rule.enabled = true;
    rule.devices = vec!["keyboard".into()];
    rule.scope = Scope::Everywhere;
    rule.delivery = Delivery::Observe;
    rule.action = Action::Event {
        name: "bridge-match".into(),
    };
    rule.trigger = Trigger::Custom {
        recognizer: Definition {
            name: "bridge".into(),
            selectors: BTreeSet::from(["keyboard".into()]),
            inputs: BTreeSet::from([InputKind::Key]),
            reply_within: Millis(250),
            continuous: false,
        },
        timeout: Millis(1000),
        wake: None,
    };
    config
}
fn device() -> Device {
    Device {
        source: Source {
            device: (1 << 54) + 7,
            generation: (1 << 54) + 3,
            seat: "seat0".into(),
        },
        identity: "bridge-model".into(),
        name: "Prek test bridge model".into(),
        path: String::new(),
        physical: "fixture/bridge".into(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        axes: BTreeMap::new(),
        capabilities: BTreeMap::from([(EV_SYN, vec![SYN_REPORT]), (EV_KEY, vec![30, 48])]),
        virtual_device: false,
    }
}
fn owner() -> Subscription {
    Subscription {
        seat: "seat0".into(),
        fence: Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 1,
        },
        context: Some(Box::new(ProvidedContext {
            layouts: None,
            binding: Binding {
                seat: "seat0".into(),
                session: "model".into(),
                uid: 1000,
                compositor_pid: 42,
                compositor_start: 123,
                kind: SessionKind::User,
            },
            snapshot: Snapshot {
                generation: 1,
                revision: 1,
                captured_at: monotonic().unwrap(),
                state: State::Desktop,
                outputs: vec![],
                windows: vec![],
                regions: vec![],
                secure_overlay: false,
            },
        })),
        capabilities: BTreeSet::from([
            Capability::Context,
            Capability::Observations,
            Capability::LuaEvents,
            Capability::CustomRecognizers,
            Capability::RuntimeRules,
        ]),
        consumer: Some(Consumer::IsolatedReceiver { pid: 1, start: 1 }),
    }
}
struct Fixture {
    stream: Stream,
    parent: Channel,
    directory: PathBuf,
    owner: Subscription,
    initial: Vec<Message>,
}
impl Fixture {
    fn new(config: Config, owner: Subscription) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "prek-route-bridge-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let registry = Registry::fixture(&directory).unwrap();
        let (parent, channel) = Channel::pair().unwrap();
        let mut stream = Stream::with_registry(config, channel, registry).unwrap();
        stream
            .push(Record::Device {
                device: device(),
                keys: BTreeSet::new(),
                contacts: vec![],
            })
            .unwrap();
        let mut fixture = Self {
            stream,
            parent,
            directory,
            owner,
            initial: vec![],
        };
        fixture.subscribe(fixture.owner.clone(), 1);
        fixture.attach();
        fixture.initial = fixture.drain();
        fixture
    }
    fn attach(&mut self) {
        self.attach_device(&device());
    }
    fn attach_device(&mut self, device: &Device) {
        let now = monotonic().unwrap();
        let descriptor = Descriptor::mirror(
            device,
            KernelIdentity {
                bus: 3,
                vendor: 1,
                product: 1,
                version: 1,
            },
            None,
            &BTreeMap::new(),
            BTreeMap::new(),
        )
        .unwrap();
        let consumer = self.owner.consumer.clone().unwrap();
        let mut gate = Readiness::new(descriptor.clone(), consumer.clone(), now).unwrap();
        gate.metadata(&descriptor, now).unwrap();
        gate.consumer(
            &ConsumerEvidence {
                consumer,
                marker: descriptor.marker,
                name: descriptor.name,
                class: descriptor.class,
                seat: "seat0".into(),
                settings: BTreeMap::new(),
                at: now,
            },
            now,
        )
        .unwrap();
        gate.neutral(true, now).unwrap();
        self.stream
            .seats
            .get_mut("seat0")
            .unwrap()
            .routing
            .attach(device.clone(), &gate.finish(now).unwrap(), None, now)
            .unwrap();
        self.stream.grabbed.insert(device.source.clone());
        self.stream
            .queue(Message::Outcome {
                report: Report::Ready {
                    subscription: self.owner.identity(),
                },
            })
            .unwrap();
        let input = self.stream.seats["seat0"]
            .routing
            .input_states(now)
            .into_iter()
            .find(|input| input.source() == &device.source);
        if let Some(input) = input {
            self.stream.report_input("seat0", input, now).unwrap();
        }
    }
    fn send(&mut self, command: Command) {
        assert!(self.parent.send(&command, monotonic().unwrap()).unwrap());
        self.stream.service().unwrap();
    }
    fn subscribe(&mut self, owner: Subscription, revision: u64) {
        let mut sender = crate::subscription_transfer::Sender::default();
        sender
            .replace(
                Subscriptions {
                    revision,
                    seats: vec![owner],
                },
                monotonic().unwrap(),
            )
            .unwrap();
        for _ in 0..2048 {
            sender.send(&mut self.parent, monotonic().unwrap()).unwrap();
            self.stream.service().unwrap();
            if !sender.pending() {
                return;
            }
        }
        panic!("private subscription transfer did not drain");
    }
    fn drain(&mut self) -> Vec<Message> {
        let mut messages = vec![];
        loop {
            self.stream.flush().unwrap();
            while let Some(packet) = self.parent.receive::<Message>().unwrap() {
                messages.push(packet.message);
            }
            if self.stream.messages.messages.is_empty() {
                return messages;
            }
        }
    }
    fn key(&mut self, sequence: u64, down: bool) {
        self.stream
            .push(Record::Frame {
                frame: Frame {
                    source: device().source,
                    at: monotonic().unwrap(),
                    sequence,
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
    fn tick(&mut self) {
        self.stream
            .push(Record::Tick {
                at: monotonic().unwrap(),
            })
            .unwrap();
    }
    fn ticket(&mut self) -> Ticket {
        self.key(1, true);
        self.tick();
        self.drain()
            .into_iter()
            .find_map(|message| match message {
                Message::Outcome {
                    report: Report::Recognition { ticket, .. },
                } => Some(*ticket),
                _ => None,
            })
            .expect("native custom ticket")
    }
    fn reply(&mut self, ticket: &Ticket) {
        self.send(Command::Recognition {
            seat: "seat0".into(),
            fence: ticket.fence,
            proposal: Proposal {
                fence: ticket.fence,
                ticket: ticket.id,
                phase: Phase::Discrete,
                data: serde_json::json!({"model":true}),
            },
        });
    }
    fn rules(&mut self, id: u64, changes: Vec<Change>) {
        self.send(Command::Rules {
            seat: "seat0".into(),
            fence: self.owner.fence,
            id,
            transaction: Transaction {
                revision: self.stream.seats["seat0"].routing.rule_revision(),
                changes,
            },
        });
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.directory).unwrap();
    }
}
fn has_ticket(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        matches!(
            message,
            Message::Outcome {
                report: Report::Recognition { .. }
            }
        )
    })
}

#[test]
fn callbacks_cross_the_private_channel_only_after_the_drained_watermark() {
    let mut fixture = Fixture::new(profile(), owner());
    let ticket = fixture.ticket();
    fixture.reply(&ticket);
    assert_eq!(fixture.stream.pending.len(), 1);
    assert!(fixture.drain().is_empty());
    let arrival = fixture.stream.pending[0].received_at;
    fixture
        .stream
        .push(Record::Tick {
            at: arrival - Duration::from_nanos(1),
        })
        .unwrap();
    assert_eq!(fixture.stream.pending.len(), 1);
    fixture.tick();
    assert!(fixture.stream.pending.is_empty());
    let messages = fixture.drain();
    assert!(
        matches!(messages[0], Message::RecognitionResult { ticket: id, error: None, .. } if id == ticket.id)
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::Dispatch { .. }
        }
    )));
}

#[test]
fn consuming_callbacks_use_native_ownership_and_never_forward_claimed_keys() {
    let mut config = profile();
    config.rules[0].delivery = Delivery::Reserve;
    config.timing.join_window = Millis(1);
    config.timing.decision_window = Millis(40);
    let mut fixture = Fixture::new(config, owner());
    let ticket = fixture.ticket();
    std::thread::sleep(Duration::from_millis(2));
    fixture.reply(&ticket);
    fixture.tick();
    let messages = fixture.drain();
    assert!(matches!(
        messages[0],
        Message::RecognitionResult { error: None, .. }
    ));
    assert!(messages.iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::Dispatch { .. }
        }
    )));
    assert!(
        fixture
            .stream
            .seats
            .get_mut("seat0")
            .unwrap()
            .routing
            .due_after_input(&device().source, monotonic().unwrap())
            .unwrap()
            .is_none()
    );
    fixture.key(2, false);
    fixture.tick();
    assert!(
        fixture
            .stream
            .seats
            .get_mut("seat0")
            .unwrap()
            .routing
            .due_after_input(&device().source, monotonic().unwrap())
            .unwrap()
            .is_none()
    );
}

#[test]
fn wrong_seat_and_fence_cannot_consume_another_owners_ticket() {
    let mut fixture = Fixture::new(profile(), owner());
    let ticket = fixture.ticket();
    let mut other = ticket.fence;
    other.session += 1;
    for (seat, fence) in [("seat1", ticket.fence), ("seat0", other)] {
        fixture.send(Command::Recognition {
            seat: seat.into(),
            fence,
            proposal: Proposal {
                fence,
                ticket: ticket.id,
                phase: Phase::Discrete,
                data: serde_json::Value::Null,
            },
        });
    }
    fixture.tick();
    let messages = fixture.drain();
    assert_eq!(
        messages
            .iter()
            .filter(|message| matches!(message, Message::RecognitionResult { error: Some(_), .. }))
            .count(),
        2
    );
    assert!(!messages.iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::Dispatch { .. }
        }
    )));
    fixture.reply(&ticket);
    fixture.tick();
    assert!(
        fixture
            .drain()
            .iter()
            .any(|message| matches!(message, Message::RecognitionResult { error: None, .. }))
    );
}

#[test]
fn protected_catalog_has_exact_source_identity_and_no_ordinary_input() {
    let fixture = Fixture::new(profile(), owner());
    assert!(matches!(
        fixture.initial[0],
        Message::Configuration {
            generation: 1,
            error: None
        }
    ));
    let metadata: Vec<_> = fixture
        .initial
        .iter()
        .filter_map(|message| match message {
            Message::Outcome {
                report: Report::RecognizerMetadata { metadata, .. },
            } => Some(metadata),
            _ => None,
        })
        .collect();
    assert!(!metadata.is_empty());
    assert!(
        metadata.iter().all(
            |metadata| metadata.source == device().source && metadata.selectors == ["keyboard"]
        )
    );
    let wire = serde_json::to_value(metadata.last().unwrap()).unwrap();
    assert!(wire.get("keys").is_none() && wire.get("contacts").is_none());
    assert!(!fixture.initial.iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::Input { .. }
        }
    )));
    let ready = fixture
        .initial
        .iter()
        .position(|message| {
            matches!(
                message,
                Message::Outcome {
                    report: Report::Ready { .. }
                }
            )
        })
        .unwrap();
    assert!(fixture.initial[ready + 1..].iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::RecognizerMetadata { .. }
        }
    )));
}

#[test]
fn input_events_capability_does_not_bypass_the_observe_only_contract() {
    let mut subscription = owner();
    subscription.capabilities.insert(Capability::InputEvents);
    let mut fixture = Fixture::new(profile(), subscription);
    assert!(fixture.initial.iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::RecognizerMetadata { .. }
        }
    )));
    fixture.key(1, true);
    let messages = fixture.drain();
    assert!(!messages.iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::RecognizerMetadata { .. }
        }
    )));
    assert!(!messages.iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::Input { .. }
        }
    )));
}

#[test]
fn malformed_proposals_and_pending_limits_are_rejected_before_mutation() {
    let mut fixture = Fixture::new(profile(), owner());
    let ticket = fixture.ticket();
    let command = |ticket_id, data| Command::Recognition {
        seat: "seat0".into(),
        fence: ticket.fence,
        proposal: Proposal {
            fence: ticket.fence,
            ticket: ticket_id,
            phase: Phase::Pending,
            data,
        },
    };
    let now = monotonic().unwrap();
    assert!(
        fixture
            .stream
            .enqueue(command(0, serde_json::Value::Null), now)
            .is_err()
    );
    assert!(
        fixture
            .stream
            .enqueue(
                command(
                    ticket.id,
                    serde_json::json!({"large":"x".repeat(crate::recognition::MAX_DATA_BYTES)})
                ),
                now
            )
            .is_err()
    );
    assert!(fixture.stream.pending.is_empty());
    assert_eq!(fixture.stream.pending_bytes, 0);
    for _ in 0..crate::recognition::MAX_PENDING {
        fixture
            .stream
            .enqueue(command(ticket.id, serde_json::Value::Null), now)
            .unwrap();
    }
    let bytes = fixture.stream.pending_bytes;
    assert!(
        fixture
            .stream
            .enqueue(command(ticket.id, serde_json::Value::Null), now)
            .is_err()
    );
    assert_eq!(
        fixture.stream.pending.len(),
        crate::recognition::MAX_PENDING
    );
    assert_eq!(fixture.stream.pending_bytes, bytes);
    fixture.send(Command::Subscriptions {
        update: Subscriptions {
            revision: 2,
            seats: vec![],
        },
    });
    assert!(fixture.stream.pending.is_empty());
    assert_eq!(fixture.stream.pending_bytes, 0);
}

#[test]
fn source_loss_before_a_queued_reply_cancels_without_dispatch() {
    let mut fixture = Fixture::new(profile(), owner());
    let ticket = fixture.ticket();
    fixture.reply(&ticket);
    fixture
        .stream
        .push(Record::Remove {
            source: device().source,
            at: monotonic().unwrap(),
        })
        .unwrap();
    fixture.tick();
    let messages = fixture.drain();
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, Message::RecognitionResult { error: Some(_), .. }))
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::RecognitionCancelled { .. }
        }
    )));
    assert!(!has_ticket(&messages));
    assert!(!messages.iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::Dispatch { .. }
        }
    )));
}

#[test]
fn revoked_helpers_cancel_queued_tickets_and_require_new_neutral_input() {
    let mut fixture = Fixture::new(profile(), owner());
    fixture.key(1, true);
    fixture.tick();
    assert!(
        fixture
            .stream
            .messages
            .messages
            .iter()
            .any(|(message, _)| matches!(
                message,
                Message::Outcome {
                    report: Report::Recognition { .. }
                }
            ))
    );
    let mut revoked = fixture.owner.clone();
    revoked.capabilities.remove(&Capability::CustomRecognizers);
    fixture.subscribe(revoked, 2);
    let messages = fixture.drain();
    assert!(!has_ticket(&messages));
    assert!(messages.iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::RecognitionCancelled { .. }
        }
    )));
    fixture.subscribe(fixture.owner.clone(), 3);
    fixture.tick();
    assert!(!has_ticket(&fixture.drain()));
    fixture.key(2, false);
    fixture.tick();
    assert!(!has_ticket(&fixture.drain()));
    fixture.key(3, true);
    fixture.tick();
    assert!(has_ticket(&fixture.drain()));
}

#[test]
fn rule_receipts_follow_drain_and_retire_old_tickets_and_transport() {
    let mut fixture = Fixture::new(profile(), owner());
    let ticket = fixture.ticket();
    let id = fixture.stream.config.rules[0].id.clone();
    fixture.rules(1, vec![Change::Enabled { id, enabled: false }]);
    assert_eq!(fixture.stream.seats["seat0"].routing.rule_revision(), 1);
    assert!(fixture.drain().is_empty());
    fixture.tick();
    let messages = fixture.drain();
    assert!(matches!(
        messages[0],
        Message::RulesChanged {
            id: 1,
            revision: 2,
            error: None,
            ..
        }
    ));
    assert!(
        !fixture.stream.seats["seat0"]
            .routing
            .has_source(&device().source)
    );
    assert!(messages.iter().any(|message| matches!(message,
        Message::Outcome { report: Report::SourceInvalidated { source, .. } } if source == &device().source)));
    fixture.reply(&ticket);
    fixture.tick();
    assert!(
        fixture
            .drain()
            .iter()
            .any(|message| matches!(message, Message::RecognitionResult { error: Some(_), .. }))
    );
}

#[test]
fn unauthorized_or_expired_rule_commands_do_not_commit() {
    for expired in [false, true] {
        let mut subscription = owner();
        if !expired {
            subscription.capabilities.remove(&Capability::RuntimeRules);
        }
        let mut fixture = Fixture::new(profile(), subscription);
        let id = fixture.stream.config.rules[0].id.clone();
        fixture.rules(1, vec![Change::Enabled { id, enabled: false }]);
        let arrival = fixture.stream.pending[0].received_at;
        let now = if expired {
            arrival + crate::recognition::MAX_REPLY
        } else {
            monotonic().unwrap()
        };
        fixture.stream.apply_pending(arrival, now).unwrap();
        assert_eq!(fixture.stream.seats["seat0"].routing.rule_revision(), 1);
        assert!(fixture.drain().iter().any(|message| matches!(
            message,
            Message::RulesChanged {
                revision: 1,
                error: Some(_),
                ..
            }
        )));
    }
}

#[test]
fn retired_transport_cannot_offer_new_callbacks_or_accept_old_replies() {
    let mut fixture = Fixture::new(profile(), owner());
    let ticket = fixture.ticket();
    fixture
        .stream
        .seats
        .get_mut("seat0")
        .unwrap()
        .routing
        .recover(&device().source);
    fixture
        .stream
        .collect("seat0", monotonic().unwrap(), None)
        .unwrap();
    fixture.key(2, false);
    fixture.key(3, true);
    fixture.tick();
    assert!(!has_ticket(&fixture.drain()));
    fixture.reply(&ticket);
    fixture.tick();
    let messages = fixture.drain();
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, Message::RecognitionResult { error: Some(_), .. }))
    );
    assert!(!messages.iter().any(|message| matches!(
        message,
        Message::Outcome {
            report: Report::Dispatch { .. }
        }
    )));
}
