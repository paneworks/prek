use super::*;
use prek::{
    config::{Action, Config, Context, Delivery, Key, Rule, Scope, Selector, Trigger},
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
            capabilities: BTreeMap::from([(EV_KEY, vec![30, 183, 184])]),
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
        let at = self.refresh();
        self.generated.poll(&self.routing, at).unwrap();
        for id in [1, 2] {
            while let Some(scheduled) = self
                .routing
                .due(&source(id), monotonic().unwrap())
                .unwrap()
                .cloned()
            {
                if self.generated.blocks_raw_span(
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
                && self
                    .routing
                    .due(&source(1), monotonic().unwrap())
                    .unwrap()
                    .is_none()
                && self
                    .routing
                    .due(&source(2), monotonic().unwrap())
                    .unwrap()
                    .is_none()
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
    fn capture(&mut self, output: &Path, index: usize, text: &str, keys: &[u16]) {
        phase_service(output, index, text, keys, || self.pump());
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
    keyboard.pump();
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
    keyboard.pump();
    keyboard
        .generated
        .retire_source(&source(1), monotonic().unwrap())
        .unwrap();
    keyboard.routing.recover(&source(1));
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
    phase(&output, 8, "bAAaAaA", &[]);
    fs::write(output.join("key-output-runtime-result.json"),serde_json::to_vec(&json!({"status":"passed","phases":9,
        "source":"three_owned_wayland_virtual_keyboards","production_route_tested":false,"kernel_input_tested":false,
        "unmodified_stock_acceptance":false,"test_backpressure":true,"scoped_worker_tested":true,
        "authority_modeled":true,"transport_readiness_modeled":true,"selective_consumption_tested":false,
        "selective_projection_tested":true,"within_frame_order_tested":true,"cross_source_order_tested":true})).unwrap()).unwrap();
}
