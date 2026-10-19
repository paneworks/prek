use super::*;
use prek::input::EV_REL;

struct Hybrid {
    routing: Routing,
    generated: Seat<Keyboard>,
    raw: Keyboard,
    inputs: Vec<Frame>,
    delivered: Vec<Value>,
    revision: u64,
    refreshed: Duration,
    dispatches: usize,
}
impl Hybrid {
    fn center(&mut self, output: &Path) {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let at = monotonic().unwrap().as_nanos();
            self.raw
                .child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"c\n")
                .unwrap();
            self.raw.child.stdin.as_mut().unwrap().flush().unwrap();
            self.raw.ack(b"done\n").unwrap();
            std::thread::sleep(Duration::from_millis(5));
            let receipts = fs::read_to_string(output.join("receipts.jsonl")).unwrap();
            if receipts
                .split_inclusive('\n')
                .filter(|line| line.ends_with('\n'))
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .any(|row| {
                    row["event"] == "pointer_motion"
                        && row["at_ns"]
                            .as_u64()
                            .is_some_and(|value| u128::from(value) >= at)
                })
            {
                return;
            }
            assert!(
                Instant::now() < until,
                "missing actual pointer initialization receipt"
            );
        }
    }
    fn pump(&mut self) {
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
        self.routing.push(Record::Tick { at }).unwrap();
        let work: Vec<_> = self
            .routing
            .take_scoped_dispatches(at)
            .into_iter()
            .map(|dispatch| dispatch.into_key_work().ok().unwrap())
            .collect();
        self.dispatches += work.len();
        self.generated.admit(&self.routing, work, None, at).unwrap();
        self.generated.poll(&self.routing, at).unwrap();
        while let Some(scheduled) = self
            .routing
            .due(&source(600), monotonic().unwrap())
            .unwrap()
            .cloned()
        {
            let span = self
                .routing
                .input_span(&source(600), scheduled.ticket)
                .unwrap();
            if self
                .generated
                .blocks_ordered_span(&self.routing, &source(600), span)
            {
                break;
            }
            let sent = monotonic().unwrap();
            self.raw
                .enqueue(&Frame {
                    source: self.raw.source.clone(),
                    ..scheduled.frame.clone()
                })
                .unwrap();
            while self.raw.flush().unwrap() == Progress::Pending {}
            self.routing
                .delivered(&source(600), scheduled.ticket, monotonic().unwrap())
                .unwrap();
            self.delivered
                .push(json!({"frame":scheduled.frame,"due":scheduled.due,"sent":sent}));
        }
        self.generated
            .poll(&self.routing, monotonic().unwrap())
            .unwrap();
    }
    fn feed(&mut self, events: &[(u16, u16, i32)]) {
        let frame = Frame {
            source: source(600),
            sequence: self.inputs.len() as u64 + 1,
            at: monotonic().unwrap(),
            events: events
                .iter()
                .map(|&(kind, code, value)| RawEvent { kind, code, value })
                .chain([RawEvent {
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                }])
                .collect(),
        };
        self.inputs.push(frame.clone());
        self.routing.push(Record::Frame { frame }).unwrap();
        self.pump();
    }
    fn settle(&mut self, count: usize) {
        let until = Instant::now() + Duration::from_secs(2);
        while self.delivered.len() < count || !self.generated.settled() {
            self.pump();
            assert!(Instant::now() < until, "hybrid source failed to settle");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(self.delivered.len(), count);
    }
}

pub(super) fn exercise(output: &Path, socket: &Path, index: usize, text: &str) -> String {
    let mut config = Config::preset("minimal").unwrap();
    config.mode = prek::config::Mode::Route;
    config.devices.insert(
        "keys".into(),
        Selector {
            keys: vec![Key(202)],
            ..Default::default()
        },
    );
    config.policy.allowed_devices.push("keys".into());
    let mut claimed = rule(
        "hybrid-key",
        202,
        Action::Keys {
            keys: vec![Key(30)],
        },
    );
    claimed.delivery = Delivery::Buffer;
    claimed.priority = 10;
    let mut fallback = rule(
        "hybrid-double",
        203,
        Action::Keys {
            keys: vec![Key(48)],
        },
    );
    fallback.delivery = Delivery::Buffer;
    fallback.trigger = Trigger::ButtonTaps {
        key: Key(203),
        count: 2,
    };
    config.rules = vec![claimed, fallback];
    config.timing.tap_gap = Millis(20);
    config.timing.join_window = Millis(1);
    config.timing.decision_window = Millis(200);
    let fence = Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    };
    let at = monotonic().unwrap();
    let device = Device {
        source: source(600),
        identity: "hybrid-protocol-model".into(),
        name: "Hybrid decoded source model".into(),
        path: String::new(),
        physical: String::new(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        capabilities: BTreeMap::from([(EV_KEY, vec![31, 202, 203]), (EV_REL, vec![0])]),
        axes: BTreeMap::new(),
        virtual_device: false,
    };
    let mut routing = worker_routing(config, fence);
    routing
        .authority_mut()
        .bind(
            Binding {
                seat: "seat0".into(),
                session: "owned-hybrid-protocol-model".into(),
                uid: 1000,
                compositor_pid: 42,
                compositor_start: 1,
                kind: SessionKind::User,
            },
            true,
            false,
        )
        .unwrap();
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
    routing
        .attach(device.clone(), &ready(&device, at), None, at)
        .unwrap();
    routing.actions_available(1, BTreeSet::from([Capability::SyntheticInput]), at);
    let mut hybrid = Hybrid {
        routing,
        generated: Seat::new(
            fence,
            Output::new(Keyboard::open(output, socket, 963)).unwrap(),
        )
        .unwrap(),
        raw: Keyboard::open_kind(output, socket, 962, true),
        inputs: vec![],
        delivered: vec![],
        revision: 1,
        refreshed: at,
        dispatches: 0,
    };
    let mut domain_sink = Keyboard::open_kind(output, socket, 964, true);
    hybrid.center(output);
    phase_service(output, index, text, &[], || hybrid.pump());
    hybrid.feed(&[
        (EV_REL, 0, 7),
        (EV_KEY, 202, 1),
        (EV_REL, 0, 9),
        (EV_KEY, 31, 1),
        (EV_KEY, 31, 0),
        (EV_KEY, 202, 0),
    ]);
    hybrid.settle(2);
    assert_eq!(hybrid.dispatches, 2);
    domains::exercise(output, &mut domain_sink);
    let text = format!("{text}as");
    phase_service(output, index + 1, &text, &[], || hybrid.pump());
    hybrid.feed(&[(EV_REL, 0, 7), (EV_KEY, 203, 1)]);
    let until = Instant::now() + Duration::from_millis(80);
    while Instant::now() < until {
        hybrid.pump();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(hybrid.delivered.len(), 2);
    hybrid.feed(&[(EV_REL, 0, 9), (EV_KEY, 203, 0)]);
    hybrid.settle(4);
    assert_eq!(hybrid.dispatches, 2);
    phase_service(output, index + 2, &text, &[], || hybrid.pump());
    hybrid.generated.retire(monotonic().unwrap()).unwrap();
    for _ in 0..16 {
        hybrid.generated.drain(monotonic().unwrap()).unwrap();
    }
    assert!(hybrid.generated.settled());
    assert!(hybrid.routing.take_recoveries().is_empty());
    fs::write(output.join("hybrid-key-runtime.json"), serde_json::to_vec(&json!({
        "status":"passed", "scope":"owned_decoded_keyboard_relative_protocol",
        "inputs":hybrid.inputs, "delivered":hybrid.delivered, "dispatches":hybrid.dispatches,
        "source_and_readiness_modeled":true, "native_hybrid_topology_tested":false,
        "physical_takeover_tested":false, "kernel_input_opened":false, "mt_rendering_tested":false,
        "generated_neutral":true, "slack_ns":20_000_000,
    })).unwrap()).unwrap();
    text
}
