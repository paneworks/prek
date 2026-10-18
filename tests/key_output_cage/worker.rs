#![cfg(feature = "native-test-harness")]
use super::*;
use prek::keys::producer::{Operation as SoftwareOp, Request as SoftwareRequest};
use prek::{
    config::{Action, Config, Context, Delivery, Key, Millis, Rule, Scope, Selector, Trigger},
    context::{Binding, SessionKind, Snapshot, State},
    input::Device,
    keys::worker::{Remap, Seat, Submission},
    protocol::{Capability, Epoch, Fence},
    replay::Record,
    routing::Routing,
    rules::Event,
    transport::{Consumer, ConsumerEvidence, Descriptor, KernelIdentity, Readiness},
};
use std::collections::{BTreeMap, BTreeSet};

#[path = "worker/custom.rs"]
mod custom;
#[path = "worker/pending.rs"]
mod pending;

fn rule(id: &str, key: u16, action: Action) -> Rule {
    Rule {
        id: id.into(),
        enabled: true,
        priority: 0,
        devices: vec!["keys".into()],
        seats: vec![],
        contexts: vec![Context::Desktop],
        scope: Scope::Everywhere,
        output: None,
        screen: None,
        keyboard_visible: None,
        require_keys: vec![],
        keys_throughout: false,
        layer: None,
        when_mode: None,
        delivery: Delivery::Observe,
        trigger: Trigger::Key { key: Key(key) },
        action,
    }
}
fn ready(device: &Device, at: Duration) -> prek::transport::Ready {
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
    let consumer = Consumer::IsolatedReceiver { pid: 1, start: 1 };
    let mut gate = Readiness::new(descriptor.clone(), consumer.clone(), at).unwrap();
    gate.metadata(&descriptor, at).unwrap();
    gate.consumer(
        &ConsumerEvidence {
            consumer,
            marker: descriptor.marker,
            name: descriptor.name,
            class: descriptor.class,
            seat: device.source.seat.clone(),
            settings: BTreeMap::new(),
            at,
        },
        at,
    )
    .unwrap();
    gate.neutral(true, at).unwrap();
    gate.finish(at).unwrap()
}
struct RoutedKeyboard {
    input_trace: fs::File,
    fence: Fence,
    routing: Routing,
    generated: Seat<Keyboard>,
    raw: Keyboard,
    other_raw: Keyboard,
    raw_sequences: [u64; 2],
    sequence: u64,
    other_sequence: u64,
    revision: u64,
    refreshed: Duration,
    software: Vec<SoftwareLease>,
    retired_first: bool,
}
struct SoftwareLease {
    id: u64,
    sequence: u64,
    renewed: Duration,
    keep: bool,
}
impl RoutedKeyboard {
    fn open(output: &Path, socket: &Path) -> Self {
        let mut config = Config::preset("minimal").unwrap();
        config.mode = prek::config::Mode::Route;
        config.devices.insert(
            "keys".into(),
            Selector {
                keys: vec![Key(183)],
                ..Default::default()
            },
        );
        config.policy.allowed_devices.push("keys".into());
        config.rules = vec![
            rule("one", 183, Action::Remap { key: Key(42) }),
            rule("two", 183, Action::Remap { key: Key(42) }),
            rule(
                "tap",
                184,
                Action::Keys {
                    keys: vec![Key(30)],
                },
            ),
        ];
        config.rules[0].delivery = Delivery::Buffer;
        config.rules[0].priority = 10;
        config.timing.tap_gap = Millis(20);
        for (id, trigger, throughout) in [
            (
                "conditional-hold",
                Trigger::ButtonHold {
                    key: Key(185),
                    after: Millis(20),
                    repeat: None,
                },
                false,
            ),
            (
                "conditional-taps",
                Trigger::ButtonTaps {
                    key: Key(187),
                    count: 1,
                },
                false,
            ),
            (
                "conditional-chord",
                Trigger::Chord {
                    keys: vec![Key(188), Key(189)],
                    ordered: false,
                    timeout: Millis(100),
                },
                true,
            ),
        ] {
            let mut conditional = rule(
                id,
                185,
                Action::Keys {
                    keys: vec![Key(30)],
                },
            );
            conditional.trigger = trigger;
            conditional.require_keys = vec![Key(190)];
            conditional.keys_throughout = throughout;
            config.rules.push(conditional);
        }
        for (id, first, last) in [
            ("ordered-sequence", 191, 192),
            ("settled-sequence", 193, 194),
        ] {
            let mut sequence = rule(
                id,
                first,
                Action::Keys {
                    keys: vec![Key(30)],
                },
            );
            sequence.trigger = Trigger::Sequence {
                steps: vec![
                    if first == 193 {
                        Trigger::ButtonTaps {
                            key: Key(first),
                            count: 1,
                        }
                    } else {
                        Trigger::Key { key: Key(first) }
                    },
                    Trigger::Key { key: Key(last) },
                ],
                timeout: Millis(200),
            };
            config.rules.push(sequence);
        }
        for (id, trigger, priority) in [
            (
                "buffered-tap",
                Trigger::ButtonTaps {
                    key: Key(195),
                    count: 1,
                },
                60,
            ),
            (
                "buffered-hold",
                Trigger::ButtonHold {
                    key: Key(195),
                    after: Millis(80),
                    repeat: None,
                },
                70,
            ),
            (
                "buffered-unmatched",
                Trigger::ButtonTaps {
                    key: Key(31),
                    count: 2,
                },
                50,
            ),
            (
                "buffered-sequence",
                Trigger::Sequence {
                    steps: vec![
                        Trigger::Key { key: Key(197) },
                        Trigger::Key { key: Key(198) },
                    ],
                    timeout: Millis(200),
                },
                40,
            ),
            (
                "buffered-delayed-sequence",
                Trigger::Sequence {
                    steps: vec![
                        Trigger::ButtonTaps {
                            key: Key(199),
                            count: 1,
                        },
                        Trigger::Key { key: Key(200) },
                    ],
                    timeout: Millis(200),
                },
                30,
            ),
        ] {
            let mut native = rule(
                id,
                195,
                Action::Keys {
                    keys: vec![Key(30)],
                },
            );
            native.trigger = trigger;
            native.delivery = Delivery::Buffer;
            native.priority = priority;
            config.rules.push(native);
        }
        let mut later = rule(
            "buffered-later-action",
            201,
            Action::Keys {
                keys: vec![Key(48)],
            },
        );
        later.delivery = Delivery::Buffer;
        later.priority = 20;
        config.rules.push(later);
        let fence = Fence {
            broker: Epoch([1, 2]),
            configuration: 1,
            session: 1,
        };
        let raw = Keyboard::open(output, socket, 900);
        let other_raw = Keyboard::open(output, socket, 901);
        let generated = Seat::new(
            fence,
            Output::new(Keyboard::open(output, socket, 100)).unwrap(),
        )
        .unwrap();
        let mut routing = Routing::with_key_projection(config, fence).unwrap();
        routing
            .authority_mut()
            .bind(
                Binding {
                    seat: "seat0".into(),
                    session: "owned-protocol-model".into(),
                    uid: 1000,
                    compositor_pid: 42,
                    compositor_start: 1,
                    kind: SessionKind::User,
                },
                true,
                false,
            )
            .unwrap();
        let at = monotonic().unwrap();
        routing
            .authority_mut()
            .update(
                Snapshot {
                    generation: 1,
                    revision: 1,
                    captured_at: at,
                    state: State::Desktop,
                    outputs: vec![],
                    windows: vec![],
                    regions: vec![],
                    secure_overlay: false,
                },
                at,
            )
            .unwrap();
        let device = Device {
            source: source(1),
            identity: "protocol-source-model".into(),
            name: "Protocol fixture source".into(),
            path: String::new(),
            physical: String::new(),
            unique: String::new(),
            vendor: 1,
            product: 1,
            properties: vec![],
            capabilities: BTreeMap::from([(
                EV_KEY,
                vec![
                    30, 31, 183, 184, 185, 187, 188, 189, 190, 191, 192, 193, 194, 195, 197, 198,
                    199, 200, 201,
                ],
            )]),
            axes: BTreeMap::new(),
            virtual_device: false,
        };
        routing
            .attach(device.clone(), &ready(&device, at), None, at)
            .unwrap();
        let mut other = device.clone();
        other.source = source(2);
        other.identity.push_str("-other");
        routing
            .attach(other.clone(), &ready(&other, at), None, at)
            .unwrap();
        routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), at);
        Self {
            input_trace: fs::File::create(output.join("key-worker-input.jsonl")).unwrap(),
            fence,
            routing,
            generated,
            raw,
            other_raw,
            raw_sequences: [0, 0],
            sequence: 0,
            other_sequence: 0,
            revision: 1,
            refreshed: at,
            software: Vec::new(),
            retired_first: false,
        }
    }
    fn refresh(&mut self) -> Duration {
        let at = monotonic().unwrap();
        if at.saturating_sub(self.refreshed) >= Duration::from_millis(50) {
            self.revision += 1;
            self.routing
                .authority_mut()
                .update(
                    Snapshot {
                        generation: 1,
                        revision: self.revision,
                        captured_at: at,
                        state: State::Desktop,
                        outputs: vec![],
                        windows: vec![],
                        regions: vec![],
                        secure_overlay: false,
                    },
                    at,
                )
                .unwrap();
            self.routing
                .actions_available(1, BTreeSet::from([Capability::SyntheticInput]), at);
            self.refreshed = at;
        }
        at
    }
    fn feed(&mut self, edges: &[(u16, i32)]) -> (Vec<Remap>, Vec<Event>) {
        self.feed_from(1, edges)
    }
    fn feed_from(&mut self, id: u64, edges: &[(u16, i32)]) -> (Vec<Remap>, Vec<Event>) {
        let at = self.refresh();
        let sequence = if id == 1 {
            self.sequence += 1;
            self.sequence
        } else {
            self.other_sequence += 1;
            self.other_sequence
        };
        let events = edges
            .iter()
            .map(|&(code, value)| RawEvent {
                kind: EV_KEY,
                code,
                value,
            })
            .chain([RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            }])
            .collect();
        let input = Frame {
            source: source(id),
            sequence,
            at,
            events,
        };
        serde_json::to_writer(&mut self.input_trace, &input).unwrap();
        writeln!(self.input_trace).unwrap();
        self.routing
            .push(Record::Frame {
                frame: input.clone(),
            })
            .unwrap();
        let mut handles = Vec::new();
        let mut cancellations = Vec::new();
        let mut works = Vec::new();
        for dispatch in self.routing.take_scoped_dispatches(at) {
            let work = dispatch.into_key_work().ok().unwrap();
            if let Event::Matched {
                source, rule, id, ..
            } = work.event()
            {
                cancellations.push(Event::Cancelled {
                    version: 1,
                    at,
                    source: source.clone(),
                    rule: rule.clone(),
                    id: *id,
                    reason: "owned protocol fixture".into(),
                });
            }
            works.push(work);
        }
        for submission in self
            .generated
            .admit(&self.routing, works, Some(&input), at)
            .unwrap()
        {
            if let Submission::Remap(handle) = submission {
                handles.push(handle);
            }
        }
        (handles, cancellations)
    }
    fn pump(&mut self) {
        for _ in 0..8 {
            self.pump_once();
            if self.generated.settled() {
                self.pump_once();
                break;
            }
        }
    }
    fn pump_once(&mut self) {
        let at = self.refresh();
        self.routing.push(Record::Tick { at }).unwrap();
        let works = self
            .routing
            .take_scoped_dispatches(at)
            .into_iter()
            .map(|dispatch| dispatch.into_key_work().ok().unwrap())
            .collect();
        self.generated
            .admit(&self.routing, works, None, at)
            .unwrap();
        let renew = self
            .software
            .iter()
            .filter(|lease| {
                lease.keep && at.saturating_sub(lease.renewed) >= Duration::from_millis(30)
            })
            .map(|lease| lease.id)
            .collect::<Vec<_>>();
        for id in renew {
            self.software_request(
                id,
                SoftwareOp::Renew {
                    expires: at + Duration::from_millis(120),
                },
                at,
            );
        }
        self.generated.poll(&self.routing, at).unwrap();
        for id in [1, 2] {
            if id == 1 && self.retired_first {
                continue;
            }
            while let Some(scheduled) = self
                .routing
                .due(&source(id), monotonic().unwrap())
                .unwrap()
                .cloned()
            {
                if self.generated.blocks_ordered_span(
                    &self.routing,
                    &source(id),
                    self.routing
                        .input_span(&source(id), scheduled.ticket)
                        .unwrap(),
                ) {
                    break;
                }
                let edges = scheduled
                    .frame
                    .events
                    .iter()
                    .filter(|event| event.kind == EV_KEY)
                    .map(|event| (event.code, event.value))
                    .collect::<Vec<_>>();
                self.raw_sequences[(id - 1) as usize] = scheduled.frame.sequence;
                if id == 1 {
                    self.raw.send(scheduled.frame.sequence, &edges);
                } else {
                    self.other_raw.send(scheduled.frame.sequence, &edges);
                }
                self.routing
                    .delivered(&source(id), scheduled.ticket, monotonic().unwrap())
                    .unwrap();
            }
        }
        self.generated
            .poll(&self.routing, monotonic().unwrap())
            .unwrap();
    }
    fn drain(&mut self) {
        let until = Instant::now() + Duration::from_secs(1);
        loop {
            self.pump();
            if self.generated.settled()
                && [1, 2].into_iter().all(|id| {
                    id == 1 && self.retired_first
                        || self
                            .routing
                            .due(&source(id), monotonic().unwrap())
                            .unwrap()
                            .is_none()
                })
            {
                return;
            }
            assert!(
                Instant::now() < until,
                "scoped key scheduling did not settle"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn advance(&mut self, delay: Duration) {
        let until = Instant::now() + delay;
        while Instant::now() < until {
            self.pump();
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn capture(&mut self, output: &Path, index: usize, text: &str, keys: &[u16]) {
        phase_service(output, index, text, keys, || self.pump());
    }
    fn software_packet(
        &mut self,
        id: u64,
        sequence: u64,
        operation: SoftwareOp,
        at: Duration,
    ) -> bool {
        self.generated
            .fixture_producer_request(
                &self.routing,
                &SoftwareRequest {
                    fence: self.fence,
                    source: source(100),
                    marker: "protocol-actor-model".into(),
                    consumer: Consumer::IsolatedReceiver { pid: 1, start: 1 },
                    producer: Epoch([id, 1]),
                    sequence,
                    operation,
                },
                at,
            )
            .unwrap()
    }
    fn software_open(&mut self, id: u64) {
        let at = self.refresh();
        assert!(self.software_packet(
            id,
            1,
            SoftwareOp::Open {
                expires: at + Duration::from_millis(120)
            },
            at
        ));
        self.software.push(SoftwareLease {
            id,
            sequence: 1,
            renewed: at,
            keep: true,
        });
    }
    fn software_request(&mut self, id: u64, operation: SoftwareOp, at: Duration) {
        let lease = self.software.iter().find(|lease| lease.id == id).unwrap();
        let sequence = lease.sequence + 1;
        let close = matches!(operation, SoftwareOp::Close {});
        let renew = matches!(operation, SoftwareOp::Renew { .. });
        assert!(self.software_packet(id, sequence, operation, at));
        if close {
            self.software.retain(|lease| lease.id != id);
        } else {
            let lease = self
                .software
                .iter_mut()
                .find(|lease| lease.id == id)
                .unwrap();
            lease.sequence = sequence;
            if renew {
                lease.renewed = at;
            }
        }
    }
    fn cancel(&mut self, event: &Event) {
        self.generated
            .cancel(self.fence, event, monotonic().unwrap())
            .unwrap();
    }
}

#[test]
#[ignore = "owned local Cage/nested Hyprland runner only"]
fn local_cage_scoped_key_worker() {
    assert_eq!(std::env::var("PREK_LOCAL_CAGE").as_deref(), Ok("1"));
    let output = PathBuf::from(std::env::var_os("PREK_CAGE_OUTPUT").unwrap());
    let report: Value =
        serde_json::from_slice(&fs::read(output.join("nested.json")).unwrap()).unwrap();
    let socket = Path::new(report["socket"].as_str().unwrap());
    assert_eq!(
        socket,
        Path::new(&std::env::var_os("PREK_CAGE_KEY_SOCKET").unwrap())
    );
    assert!(
        socket.is_absolute()
            && socket.parent() == Some(Path::new(&std::env::var_os("XDG_RUNTIME_DIR").unwrap()))
    );
    let mut keyboard = RoutedKeyboard::open(&output, socket);
    keyboard.feed(&[(183, 1)]);
    keyboard.feed(&[(30, 1), (30, 0)]);
    keyboard.feed(&[(183, 0)]);
    keyboard.drain();
    keyboard.capture(&output, 0, "bAA", &[]);
    let (_, cancellations) = keyboard.feed(&[(183, 1)]);
    assert_eq!(cancellations.len(), 2);
    keyboard.drain();
    keyboard.capture(&output, 1, "bAA", &[42]);
    keyboard.cancel(&cancellations[0]);
    keyboard.drain();
    assert!(keyboard.generated.down(42));
    keyboard.capture(&output, 2, "bAA", &[42]);
    keyboard.feed(&[(183, 0)]);
    keyboard.drain();
    let (_, cancellations) = keyboard.feed(&[(184, 1)]);
    keyboard.pump_once();
    assert!(!keyboard.generated.settled());
    keyboard.cancel(&cancellations[0]);
    keyboard.feed(&[(184, 0)]);
    keyboard.drain();
    keyboard.capture(&output, 3, "bAA", &[]);
    let (old_handles, _) = keyboard.feed(&[(183, 1)]);
    keyboard.drain();
    keyboard
        .routing
        .actions_available(1, BTreeSet::new(), monotonic().unwrap());
    keyboard.routing.actions_available(
        1,
        BTreeSet::from([Capability::SyntheticInput]),
        monotonic().unwrap(),
    );
    keyboard.drain();
    keyboard.feed(&[(183, 0)]);
    keyboard.drain();
    assert!(
        keyboard
            .generated
            .repeat(
                &old_handles,
                &Frame {
                    source: source(1),
                    sequence: keyboard.sequence + 1,
                    at: monotonic().unwrap(),
                    events: vec![RawEvent {
                        kind: EV_KEY,
                        code: 183,
                        value: 2
                    }]
                },
                0,
                monotonic().unwrap()
            )
            .is_err()
    );
    keyboard.capture(&output, 4, "bAA", &[]);
    keyboard.feed(&[(184, 1)]);
    keyboard.feed(&[(184, 0)]);
    keyboard.drain();
    keyboard.capture(&output, 5, "bAAa", &[]);
    keyboard.feed(&[(183, 1), (30, 1), (30, 0), (183, 0), (30, 1), (30, 0)]);
    keyboard.drain();
    keyboard.capture(&output, 6, "bAAaAa", &[]);
    keyboard.feed(&[(183, 1)]);
    keyboard.feed_from(2, &[(30, 1), (30, 0)]);
    keyboard.feed(&[(183, 0)]);
    keyboard.drain();
    keyboard.capture(&output, 7, "bAAaAaA", &[]);
    keyboard.feed(&[(184, 1)]);
    keyboard.pump_once();
    keyboard
        .generated
        .retire_source(&source(1), monotonic().unwrap())
        .unwrap();
    keyboard.routing.recover(&source(1));
    keyboard.retired_first = true;
    for recovery in keyboard.routing.take_recoveries() {
        let edges = recovery
            .neutral_events
            .iter()
            .filter(|event| event.kind == EV_KEY)
            .map(|event| (event.code, event.value))
            .collect::<Vec<_>>();
        keyboard.raw_sequences[0] += 1;
        keyboard.raw.send(keyboard.raw_sequences[0], &edges);
    }
    keyboard.drain();
    keyboard.capture(&output, 8, "bAAaAaA", &[]);
    keyboard.software_open(7);
    keyboard.software_open(8);
    for id in [7, 8] {
        let at = keyboard.refresh();
        keyboard.software_request(id, SoftwareOp::Press { id: 1, key: 42 }, at);
    }
    keyboard.feed_from(2, &[(30, 1), (30, 0)]);
    keyboard.drain();
    keyboard.capture(&output, 9, "bAAaAaAA", &[42]);
    let at = keyboard.refresh();
    keyboard.software_request(7, SoftwareOp::Close {}, at);
    keyboard.feed_from(2, &[(30, 1), (30, 0)]);
    keyboard.drain();
    keyboard.capture(&output, 10, "bAAaAaAAA", &[42]);
    keyboard
        .software
        .iter_mut()
        .find(|lease| lease.id == 8)
        .unwrap()
        .keep = false;
    let until = Instant::now() + Duration::from_secs(2);
    while keyboard.generated.down(42) {
        keyboard.pump();
        assert!(
            Instant::now() < until,
            "software lease did not expire without input"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let at = keyboard.refresh();
    assert!(!keyboard.software_packet(
        8,
        1,
        SoftwareOp::Open {
            expires: at + Duration::from_millis(100)
        },
        at
    ));
    keyboard.software.clear();
    keyboard.feed_from(2, &[(30, 1), (30, 0)]);
    keyboard.drain();
    keyboard.capture(&output, 11, "bAAaAaAAAa", &[]);
    keyboard.feed_from(2, &[(185, 1), (190, 1)]);
    keyboard.advance(Duration::from_millis(60));
    keyboard.feed_from(2, &[(185, 0), (190, 0)]);
    keyboard.drain();
    keyboard.capture(&output, 12, "bAAaAaAAAa", &[]);
    keyboard.feed_from(2, &[(190, 1), (185, 1), (190, 0)]);
    keyboard.advance(Duration::from_millis(60));
    keyboard.feed_from(2, &[(185, 0)]);
    keyboard.drain();
    keyboard.capture(&output, 13, "bAAaAaAAAaa", &[]);
    keyboard.feed_from(2, &[(190, 1), (187, 1), (187, 0), (190, 0)]);
    keyboard.advance(Duration::from_millis(60));
    keyboard.drain();
    keyboard.capture(&output, 14, "bAAaAaAAAaaa", &[]);
    keyboard.feed_from(
        2,
        &[
            (190, 1),
            (188, 1),
            (190, 0),
            (190, 1),
            (189, 1),
            (188, 0),
            (189, 0),
            (190, 0),
        ],
    );
    keyboard.advance(Duration::from_millis(60));
    keyboard.drain();
    keyboard.capture(&output, 15, "bAAaAaAAAaaa", &[]);
    keyboard.feed_from(
        2,
        &[(190, 1), (188, 1), (189, 1), (188, 0), (189, 0), (190, 0)],
    );
    keyboard.drain();
    keyboard.capture(&output, 16, "bAAaAaAAAaaaa", &[]);
    keyboard.feed_from(2, &[(190, 1), (187, 1), (187, 0)]);
    keyboard.feed_from(2, &[(187, 1)]);
    keyboard.advance(Duration::from_millis(620));
    keyboard.drain();
    keyboard.capture(&output, 17, "bAAaAaAAAaaaaa", &[187, 190]);
    keyboard.feed_from(2, &[(187, 0)]);
    keyboard.feed_from(2, &[(187, 1), (187, 0)]);
    keyboard.advance(Duration::from_millis(60));
    keyboard.drain();
    keyboard.capture(&output, 18, "bAAaAaAAAaaaaaa", &[190]);
    keyboard.feed_from(2, &[(187, 1)]);
    keyboard.advance(Duration::from_millis(270));
    keyboard.feed_from(2, &[(187, 0), (190, 0)]);
    keyboard.feed_from(2, &[(187, 1), (187, 0)]);
    keyboard.advance(Duration::from_millis(60));
    keyboard.drain();
    keyboard.capture(&output, 19, "bAAaAaAAAaaaaaa", &[]);
    keyboard.feed_from(2, &[(190, 1), (187, 1), (187, 0), (190, 0)]);
    keyboard.advance(Duration::from_millis(60));
    keyboard.drain();
    keyboard.capture(&output, 20, "bAAaAaAAAaaaaaaa", &[]);
    keyboard.feed_from(
        2,
        &[
            (191, 1),
            (191, 0),
            (192, 1),
            (192, 0),
            (191, 1),
            (191, 0),
            (192, 1),
            (192, 0),
        ],
    );
    keyboard.drain();
    keyboard.capture(&output, 21, "bAAaAaAAAaaaaaaaaa", &[]);
    keyboard.feed_from(2, &[(193, 1), (193, 0), (194, 1), (194, 0)]);
    keyboard.advance(Duration::from_millis(60));
    keyboard.drain();
    keyboard.capture(&output, 22, "bAAaAaAAAaaaaaaaaaa", &[]);
    keyboard.feed_from(2, &[(194, 1), (194, 0), (193, 1), (193, 0)]);
    keyboard.advance(Duration::from_millis(220));
    keyboard.drain();
    keyboard.capture(&output, 23, "bAAaAaAAAaaaaaaaaaa", &[]);
    keyboard.feed_from(2, &[(193, 1), (193, 0)]);
    keyboard.feed_from(2, &[(194, 1), (194, 0)]);
    keyboard.advance(Duration::from_millis(60));
    keyboard.drain();
    keyboard.capture(&output, 24, "bAAaAaAAAaaaaaaaaaaa", &[]);
    keyboard.feed_from(2, &[(195, 1), (195, 0), (31, 1), (31, 0)]);
    keyboard.advance(Duration::from_millis(60));
    keyboard.drain();
    keyboard.capture(&output, 25, "bAAaAaAAAaaaaaaaaaaasa", &[]);
    keyboard.feed_from(2, &[(195, 1)]);
    keyboard.advance(Duration::from_millis(120));
    keyboard.feed_from(2, &[(195, 0)]);
    keyboard.drain();
    keyboard.capture(&output, 26, "bAAaAaAAAaaaaaaaaaaasaa", &[]);
    keyboard.feed_from(
        2,
        &[(197, 1), (197, 0), (198, 1), (198, 0), (31, 1), (31, 0)],
    );
    keyboard.advance(Duration::from_millis(60));
    keyboard.drain();
    keyboard.capture(&output, 27, "bAAaAaAAAaaaaaaaaaaasaaas", &[]);
    keyboard.feed_from(2, &[(199, 1), (199, 0), (200, 1), (200, 0)]);
    keyboard.advance(Duration::from_millis(60));
    keyboard.drain();
    keyboard.capture(&output, 28, "bAAaAaAAAaaaaaaaaaaasaaasa", &[]);
    keyboard.feed_from(2, &[(31, 1)]);
    keyboard.advance(Duration::from_millis(40));
    keyboard.feed_from(2, &[(31, 0)]);
    keyboard.advance(Duration::from_millis(80));
    keyboard.drain();
    keyboard.capture(&output, 29, "bAAaAaAAAaaaaaaaaaaasaaasas", &[]);
    keyboard.feed_from(2, &[(199, 1), (199, 0)]);
    keyboard.feed_from(2, &[(200, 1), (200, 0)]);
    keyboard.feed_from(2, &[(201, 1), (201, 0)]);
    keyboard.advance(Duration::from_millis(60));
    keyboard.drain();
    keyboard.capture(&output, 30, "bAAaAaAAAaaaaaaaaaaasaaasasab", &[]);
    keyboard.generated.retire(monotonic().unwrap()).unwrap();
    for _ in 0..8 {
        if keyboard.generated.drain(monotonic().unwrap()).unwrap() == Progress::Complete {
            break;
        }
    }
    assert!(keyboard.generated.settled());
    let generated = keyboard
        .generated
        .into_output()
        .unwrap_or_else(|_| panic!("retired scheduler"));
    assert!(generated.into_sink().is_ok());
    drop(keyboard.raw);
    drop(keyboard.other_raw);
    let text = custom::exercise(
        &output,
        socket,
        31,
        "bAAaAaAAAaaaaaaaaaaasaaasasab",
        custom::Case::Root,
    );
    let text = custom::exercise(&output, socket, 32, &text, custom::Case::Activation);
    let text = custom::exercise(&output, socket, 33, &text, custom::Case::NativeFirst);
    let text = custom::exercise(&output, socket, 34, &text, custom::Case::CustomFirst);
    pending::exercise(&output, socket, 35, &text);
    phase(&output, 38, &format!("{text}a"), &[]);
    fs::write(output.join("key-output-runtime-result.json"),serde_json::to_vec(&json!({"status":"passed","phases":39,
        "source":"twelve_owned_wayland_virtual_keyboards","production_route_tested":false,"kernel_input_tested":false,
        "unmodified_stock_acceptance":false,"test_backpressure":true,"scoped_worker_tested":true,
        "authority_modeled":true,"transport_readiness_modeled":true,"selective_consumption_tested":true,
        "selective_consumption_scope":"owned_protocol_native_keys","physical_takeover_tested":false,
        "selective_projection_tested":true,"within_frame_order_tested":true,"cross_source_order_tested":true,
        "software_actor_tested":true,"software_lease_tested":true,"software_private_control_tested":false,
        "software_authorization_tested":false,"software_repeat_rendering_tested":false,
        "timed_condition_rendering_tested":true,"chord_condition_rendering_tested":true,
        "tap_tail_lifecycle_rendering_tested":true,"tap_invalid_activation_rendering_tested":true,
        "event_local_sequence_rendering_tested":true,"deferred_key_completion_rendering_tested":true,
        "pending_journal_protocol_tested":true,"pending_fallback_cadence_tested":true,
        "delayed_gesture_arbitration_tested":true,"delayed_gesture_arbitration_scope":"native_key_only",
        "root_custom_key_arbitration_rendering_tested":true,
        "event_local_custom_activation_rendering_tested":true,
        "composed_custom_key_arbitration_rendering_tested":true,
        "mixed_delayed_gesture_arbitration_tested":false})).unwrap()).unwrap();
}
