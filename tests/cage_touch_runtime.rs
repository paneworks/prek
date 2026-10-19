//! Rendered local touch delivery; source, desktop and readiness are explicit models.
#![cfg(target_os = "linux")]
#[path = "cage_touch_runtime/bridge.rs"]
mod bridge;
use prek::{
    config::Screen,
    context::{Binding, OutputState, SessionKind, Snapshot, State},
    context_transfer::View,
    gestures::{Mapping, Output, Point, Transform},
    input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, Axis, Device,
        EV_ABS, EV_KEY, EV_SYN, Frame, RawEvent, Source,
    },
    ipc::{Channel, monotonic},
    lua::{Definition, EventKind, RuleProposal, RuleReceipt, session::Session},
    protocol::{Capability, Epoch, Fence},
    replay::Record,
    routing::Routing,
    subscription_transfer::Sender,
    transport::{Consumer, ConsumerEvidence, Descriptor, KernelIdentity, Readiness},
    worker::{Message, ProvidedContext, Report, Stream, Subscription, Subscriptions},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::MetadataExt, net::UnixDatagram},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

fn device() -> Device {
    let axis = Axis {
        minimum: 0,
        maximum: 1000,
        resolution: 10,
        fuzz: 0,
        flat: 0,
    };
    Device {
        identity: "test:local-touch".into(),
        source: Source {
            device: 1,
            generation: 1,
            seat: "seat0".into(),
        },
        name: "Prek local touch".into(),
        path: String::new(),
        physical: "fixture/local-touch".into(),
        unique: String::new(),
        vendor: 0x1209,
        product: 0xbeef,
        properties: vec![1],
        virtual_device: false,
        capabilities: BTreeMap::from([
            (EV_SYN, vec![0]),
            (EV_KEY, vec![0x14a]),
            (
                EV_ABS,
                vec![
                    ABS_MT_SLOT,
                    ABS_MT_POSITION_X,
                    ABS_MT_POSITION_Y,
                    ABS_MT_TRACKING_ID,
                ],
            ),
        ]),
        axes: BTreeMap::from([
            (
                ABS_MT_SLOT,
                Axis {
                    maximum: 9,
                    resolution: 0,
                    ..axis
                },
            ),
            (ABS_MT_POSITION_X, axis),
            (ABS_MT_POSITION_Y, axis),
            (
                ABS_MT_TRACKING_ID,
                Axis {
                    maximum: 65535,
                    resolution: 0,
                    ..axis
                },
            ),
        ]),
    }
}
fn down(fingers: u16) -> Vec<RawEvent> {
    let mut events = vec![raw(EV_KEY, 0x14a, 1)];
    for slot in 0..fingers {
        events.extend([
            raw(EV_ABS, ABS_MT_SLOT, i32::from(slot)),
            raw(EV_ABS, ABS_MT_TRACKING_ID, i32::from(slot) + 7),
            raw(EV_ABS, ABS_MT_POSITION_X, if slot == 0 { 250 } else { 750 }),
            raw(EV_ABS, ABS_MT_POSITION_Y, 250),
        ]);
    }
    events
}
fn motion(fingers: u16, y: i32) -> Vec<RawEvent> {
    (0..fingers)
        .flat_map(|slot| {
            [
                raw(EV_ABS, ABS_MT_SLOT, i32::from(slot)),
                raw(EV_ABS, ABS_MT_POSITION_Y, y),
            ]
        })
        .collect()
}
fn up(fingers: u16) -> Vec<RawEvent> {
    let mut events = vec![raw(EV_KEY, 0x14a, 0)];
    for slot in 0..fingers {
        events.extend([
            raw(EV_ABS, ABS_MT_SLOT, i32::from(slot)),
            raw(EV_ABS, ABS_MT_TRACKING_ID, -1),
        ]);
    }
    events
}
fn raw(kind: u16, code: u16, value: i32) -> RawEvent {
    RawEvent { kind, code, value }
}
fn scoped() -> bool {
    std::env::var("PREK_CAGE_TOUCH_ROUTING").as_deref() == Ok("scoped")
}
fn frame(device: &Device, sequence: u64, at: Duration, events: &[RawEvent]) -> Frame {
    let mut events = events.to_vec();
    events.push(raw(EV_SYN, 0, 0));
    Frame {
        source: device.source.clone(),
        sequence,
        at,
        events,
    }
}

struct Fixture {
    routing: Routing,
    observer: Stream,
    parent: Channel,
    sender: Sender,
    session: Session,
    input_session: Session,
    device: Device,
    binding: Binding,
    fence: Fence,
    geometry: Output,
    revision: u64,
    sequence: u64,
    socket: UnixDatagram,
    bridge: bridge::Bridge,
    output: PathBuf,
    logs: fs::File,
    input_logs: fs::File,
    forwarded: Vec<Frame>,
    inputs: Vec<Frame>,
    dispatches: Vec<prek::rules::Event>,
    proposals: Vec<RuleProposal>,
}
impl Fixture {
    fn new(output: PathBuf, app: &Path, socket: UnixDatagram) -> Self {
        let definition = Definition::compile(
            include_str!("../examples/lua/touch_routing.lua"),
            "touch_routing.lua",
        )
        .unwrap();
        let config = definition.validate().unwrap();
        let (parent, child) = Channel::pair().unwrap();
        let observations = Definition::compile(
            include_str!("../examples/lua/touch_observations.lua"),
            "touch_observations.lua",
        )
        .unwrap();
        let mut observer = Stream::new(observations.validate().unwrap(), child).unwrap();
        observer.push(Record::Header { version: 1 }).unwrap();
        let fence = Fence {
            broker: Epoch([11, 12]),
            configuration: 1,
            session: 1,
        };
        let binding = Binding {
            seat: "seat0".into(),
            session: "local-cage-touch-model".into(),
            uid: fs::metadata("/proc/self").unwrap().uid(),
            compositor_pid: std::env::var("PREK_CAGE_TOUCH_PID")
                .unwrap()
                .parse()
                .unwrap(),
            compositor_start: 1,
            kind: SessionKind::User,
        };
        let rows = rows(&output);
        let configured = rows
            .iter()
            .find(|row| row["event"] == "configured")
            .unwrap();
        let geometry = Output {
            name: "HEADLESS-1".into(),
            pixel_width: configured["width"].as_f64().unwrap(),
            pixel_height: configured["height"].as_f64().unwrap(),
            scale: 1.0,
            origin: Point { x: 0.0, y: 0.0 },
            transform: Transform::Normal,
        };
        let mut routing = if scoped() {
            Routing::with_input_projection(config, fence).unwrap()
        } else {
            Routing::new(config).unwrap()
        };
        routing
            .authority_mut()
            .bind(binding.clone(), true, false)
            .unwrap();
        let session =
            Session::spawn_with_rule_transactions(app, fence, "seat0", definition).unwrap();
        let input_session = Session::spawn(app, fence, "seat0", observations).unwrap();
        let device = device();
        let mut fixture = Self {
            routing,
            observer,
            parent,
            sender: Sender::default(),
            session,
            input_session,
            bridge: bridge::Bridge::new(&device, monotonic().unwrap()),
            device,
            binding,
            fence,
            geometry,
            revision: 0,
            sequence: 0,
            socket,
            logs: fs::File::create(output.join("touch-runtime.jsonl")).unwrap(),
            input_logs: fs::File::create(output.join("touch-input.jsonl")).unwrap(),
            output,
            forwarded: vec![],
            inputs: vec![],
            dispatches: vec![],
            proposals: vec![],
        };
        fixture.settle();
        fixture.refresh();
        fixture.attach();
        fixture
    }
    fn settle(&mut self) {
        let deadline = monotonic().unwrap() + Duration::from_secs(3);
        loop {
            for log in self.session.step(monotonic().unwrap()).unwrap() {
                serde_json::to_writer(&mut self.logs, &log).unwrap();
                writeln!(self.logs).unwrap();
            }
            self.proposals.extend(self.session.take_rule_proposals());
            for log in self.input_session.step(monotonic().unwrap()).unwrap() {
                serde_json::to_writer(&mut self.input_logs, &log).unwrap();
                writeln!(self.input_logs).unwrap();
            }
            assert!(self.input_session.take_rule_proposals().is_empty());
            if (self.session.finished() || !self.proposals.is_empty())
                && self.input_session.finished()
            {
                break;
            }
            assert!(monotonic().unwrap() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn refresh(&mut self) {
        self.revision += 1;
        let now = monotonic().unwrap();
        let snapshot = Snapshot {
            generation: 1,
            revision: self.revision,
            captured_at: now,
            state: State::Desktop,
            secure_overlay: false,
            outputs: vec![OutputState {
                geometry: self.geometry.clone(),
                geometry_generation: 2,
                active_workspace: Some(1),
                screen: Screen::On,
                keyboard_known: true,
                keyboard: None,
            }],
            windows: vec![],
            regions: vec![],
        };
        self.routing
            .authority_mut()
            .update(snapshot.clone(), now)
            .unwrap();
        self.routing
            .actions_available(1, BTreeSet::from([Capability::Observations]), now);
        let context = Some(Box::new(ProvidedContext {
            binding: self.binding.clone(),
            snapshot,
            layouts: None,
        }));
        self.session
            .update_context(View {
                fence: self.fence,
                sampled_at: now,
                state: State::Desktop,
                context: context.clone(),
                error: None,
                morf: vec![],
                morf_rendered: vec![],
            })
            .unwrap();
        self.input_session
            .update_context(View {
                fence: self.fence,
                sampled_at: now,
                state: State::Desktop,
                context: context.clone(),
                error: None,
                morf: vec![],
                morf_rendered: vec![],
            })
            .unwrap();
        self.session
            .update_capabilities(
                BTreeSet::from([
                    Capability::Context,
                    Capability::Observations,
                    Capability::LuaEvents,
                    Capability::RuntimeRules,
                ]),
                BTreeSet::new(),
            )
            .unwrap();
        self.input_session
            .update_capabilities(
                BTreeSet::from([
                    Capability::Context,
                    Capability::Observations,
                    Capability::LuaEvents,
                    Capability::InputEvents,
                ]),
                BTreeSet::new(),
            )
            .unwrap();
        self.sender
            .replace(
                Subscriptions {
                    revision: self.revision,
                    seats: vec![Subscription {
                        seat: "seat0".into(),
                        fence: self.fence,
                        context,
                        capabilities: BTreeSet::from([
                            Capability::Context,
                            Capability::Observations,
                            Capability::LuaEvents,
                            Capability::InputEvents,
                        ]),
                        consumer: None,
                    }],
                },
                now,
            )
            .unwrap();
        while self.sender.pending() {
            self.sender
                .send(&mut self.parent, monotonic().unwrap())
                .unwrap();
            self.observer.service().unwrap();
        }
        self.settle();
        self.observations();
    }
    fn attach(&mut self) {
        let descriptor = Descriptor::mirror(
            &self.device,
            KernelIdentity {
                bus: 3,
                vendor: self.device.vendor,
                product: self.device.product,
                version: 1,
            },
            None,
            &BTreeMap::new(),
            BTreeMap::new(),
        )
        .unwrap();
        let now = monotonic().unwrap();
        let consumer = Consumer::IsolatedReceiver {
            pid: self.binding.compositor_pid,
            start: 1,
        };
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
        let mut mapping = Mapping::from_device(&self.device).unwrap();
        mapping.output = Some(self.geometry.clone());
        mapping.generation = 2;
        self.routing
            .attach(
                self.device.clone(),
                &gate.finish(now).unwrap(),
                Some(mapping.clone()),
                now,
            )
            .unwrap();
        self.observer
            .push(Record::Device {
                device: self.device.clone(),
                keys: BTreeSet::new(),
                contacts: vec![],
            })
            .unwrap();
        self.observer
            .push(Record::Mapping {
                source: self.device.source.clone(),
                at: now,
                mapping,
            })
            .unwrap();
        self.observations();
    }
    fn observations(&mut self) {
        self.observer.flush().unwrap();
        while let Some(packet) = self.parent.receive::<Message>().unwrap() {
            if let Message::Outcome {
                report:
                    Report::Input {
                        subscription,
                        input,
                    },
            } = packet.message
            {
                input.validate().unwrap();
                self.input_session
                    .notify(
                        EventKind::Input,
                        serde_json::json!({"fence":subscription.fence,"input":input}),
                    )
                    .unwrap();
                self.settle();
            }
        }
    }
    fn push(&mut self, events: &[RawEvent]) {
        self.refresh();
        self.sequence += 1;
        let frame = frame(&self.device, self.sequence, monotonic().unwrap(), events);
        self.inputs.push(frame.clone());
        self.routing
            .push(Record::Frame {
                frame: frame.clone(),
            })
            .unwrap();
        self.observer.push(Record::Frame { frame }).unwrap();
        self.observations();
        self.drain();
    }
    fn send(&self, packet: bridge::Packet) {
        let deadline = monotonic().unwrap() + Duration::from_secs(3);
        loop {
            match self.socket.send(&packet.bytes()) {
                Ok(32) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                other => panic!("touch send failed: {other:?}"),
            }
            assert!(monotonic().unwrap() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        let mut ack = [0; 5];
        loop {
            match self.socket.recv(&mut ack) {
                Ok(4) => {
                    assert_eq!(&ack[..4], packet.operation.to_ne_bytes());
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                other => panic!("touch acknowledgement failed: {other:?}"),
            }
            assert!(monotonic().unwrap() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn drain(&mut self) {
        while let Some(scheduled) = self
            .routing
            .due(&self.device.source, monotonic().unwrap())
            .unwrap()
            .cloned()
        {
            for packet in self.bridge.encode(scheduled.frame.clone()) {
                self.send(packet);
            }
            self.forwarded.push(scheduled.frame);
            self.routing
                .delivered(&self.device.source, scheduled.ticket, monotonic().unwrap())
                .unwrap();
        }
        for event in self.routing.take_dispatches(monotonic().unwrap()) {
            self.session
                .notify(
                    EventKind::Matched,
                    serde_json::json!({"fence":self.fence,"event":event}),
                )
                .unwrap();
            self.dispatches.push(event);
            self.settle();
        }
    }
    fn wait(&mut self, duration: Duration) {
        let until = monotonic().unwrap() + duration;
        while monotonic().unwrap() < until {
            self.refresh();
            self.routing
                .push(Record::Tick {
                    at: monotonic().unwrap(),
                })
                .unwrap();
            self.drain();
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn wait_app(&mut self, contacts: usize, frames: u64) {
        let deadline = monotonic().unwrap() + Duration::from_secs(3);
        loop {
            let rows = rows(&self.output);
            if let Some(receipt) = rows.iter().rev().find(|row| row["event"] == "touch_frame")
                && receipt["frames"] == frames
                && receipt["contacts"].as_array().unwrap().len() == contacts
                && rows.iter().any(|row| {
                    row["event"] == "rendered"
                        && row["drawn_sequence"].as_u64().unwrap_or(0)
                            >= receipt["sequence"].as_u64().unwrap()
                })
            {
                return;
            }
            assert!(
                monotonic().unwrap() < deadline,
                "touch app did not render expected contacts/frame count"
            );
            self.wait(Duration::from_millis(5));
        }
    }
    fn capture(&mut self, name: &str) {
        let mut capture = Command::new(std::env::var_os("PREK_CAGE_TOUCH_GRIM").unwrap())
            .env(
                "WAYLAND_DISPLAY",
                std::env::var_os("PREK_CAGE_TOUCH_SOCKET").unwrap(),
            )
            .arg(self.output.join(format!("touch-{name}.png")))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let until = monotonic().unwrap() + Duration::from_secs(5);
        while capture.try_wait().unwrap().is_none() {
            if monotonic().unwrap() >= until {
                capture.kill().unwrap();
                capture.wait().unwrap();
                panic!("owned screencopy timed out")
            }
            self.wait(Duration::from_millis(5));
        }
        assert!(capture.wait().unwrap().success());
    }
}
fn rows(output: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(output.join("receipts.jsonl"))
        .unwrap()
        .split_inclusive('\n')
        .filter(|line| line.ends_with('\n'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn touch_lua_example_compiles_to_continuous_buffered_rule() {
    let definition = Definition::compile(
        include_str!("../examples/lua/touch_routing.lua"),
        "touch_routing.lua",
    )
    .unwrap();
    let config = definition.validate().unwrap();
    assert_eq!(config.rules.len(), 1);
    assert_eq!(config.rules[0].id, "touch-pan");
    assert_eq!(config.rules[0].delivery, prek::config::Delivery::Buffer);
    assert!(matches!(
        config.rules[0].trigger,
        prek::config::Trigger::Swipe {
            fingers: 2,
            continuous: true,
            ..
        }
    ));
}

#[test]
fn touch_observer_example_has_no_routing_or_mutation_permission() {
    let definition = Definition::compile(
        include_str!("../examples/lua/touch_observations.lua"),
        "touch_observations.lua",
    )
    .unwrap();
    let config = definition.validate().unwrap();
    assert_eq!(config.mode, prek::config::Mode::Observe);
    assert!(config.policy.allow_input_events);
    assert!(!config.policy.allow_runtime_rules);
    assert!(config.rules.is_empty());
}

#[test]
#[ignore = "owned local Cage touch runner only; no kernel input access"]
fn local_cage_lua_touch_routing() {
    assert_eq!(std::env::var("PREK_LOCAL_CAGE_TOUCH").as_deref(), Ok("1"));
    let raw: i32 = std::env::var("PREK_CAGE_TOUCH_CONTROL_FD")
        .unwrap()
        .parse()
        .unwrap();
    assert!(raw >= 3);
    // SAFETY: this fixture receives its exclusively inherited socket descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut kind = 0;
    let mut length = std::mem::size_of_val(&kind) as nix::libc::socklen_t;
    // SAFETY: the retained socket and scalar output match the supplied size.
    assert_eq!(
        unsafe {
            nix::libc::getsockopt(
                fd.as_raw_fd(),
                nix::libc::SOL_SOCKET,
                nix::libc::SO_TYPE,
                (&mut kind as *mut i32).cast(),
                &mut length,
            )
        },
        0
    );
    assert_eq!(kind, nix::libc::SOCK_SEQPACKET);
    nix::fcntl::fcntl(
        &fd,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )
    .unwrap();
    let output = PathBuf::from(std::env::var_os("PREK_CAGE_OUTPUT").unwrap());
    let app = PathBuf::from(std::env::var_os("PREK_LOCAL_CAGE_APP").unwrap());
    let mut fixture = Fixture::new(output.clone(), &app, UnixDatagram::from(fd));
    let tap_start = fixture.inputs.len();
    fixture.push(&down(1));
    fixture.wait(Duration::from_millis(28));
    fixture.push(&up(1));
    fixture.wait_app(0, 6);
    assert_eq!(fixture.forwarded.len(), 2);
    let input_interval = fixture.inputs[tap_start + 1].at - fixture.inputs[tap_start].at;
    assert_eq!(
        fixture.forwarded[1].at - fixture.forwarded[0].at,
        input_interval
    );
    let initial_touch = rows(&output)
        .iter()
        .filter(|row| row["event"].as_str().unwrap().starts_with("touch_"))
        .count();
    fixture.push(&down(2));
    fixture.wait(Duration::from_millis(12));
    fixture.push(&motion(2, 500));
    fixture.wait(Duration::from_millis(12));
    fixture.push(&motion(2, 700));
    fixture.wait(Duration::from_millis(12));
    fixture.push(&up(2));
    assert_eq!(
        fixture.forwarded.len(),
        2,
        "claimed pan leaked raw contacts"
    );
    assert_eq!(
        fixture
            .dispatches
            .iter()
            .filter_map(
                |event| if let prek::rules::Event::Matched { stage, .. } = event {
                    Some(*stage)
                } else {
                    None
                }
            )
            .collect::<Vec<_>>(),
        vec![
            prek::rules::Stage::Begin,
            prek::rules::Stage::Update,
            prek::rules::Stage::End
        ]
    );
    assert_eq!(
        rows(&output)
            .iter()
            .filter(|row| row["event"].as_str().unwrap().starts_with("touch_"))
            .count(),
        initial_touch
    );
    assert_eq!(fixture.proposals.len(), 1);
    let proposal = fixture.proposals.remove(0);
    let revision = fixture
        .routing
        .transact_rules(&proposal.transaction, monotonic().unwrap())
        .unwrap();
    assert_eq!(revision, 2);
    fixture
        .session
        .acknowledge_rules(
            fixture.fence,
            RuleReceipt {
                id: proposal.id,
                revision,
                error: None,
            },
        )
        .unwrap();
    fixture.settle();
    let recoveries = fixture.routing.take_recoveries();
    assert_eq!(recoveries.len(), 1);
    assert!(recoveries[0].neutral_events.is_empty());
    fixture
        .observer
        .push(Record::Remove {
            source: fixture.device.source.clone(),
            at: monotonic().unwrap(),
        })
        .unwrap();
    fixture.observations();
    fixture.device.source.generation += 1;
    fixture.sequence = 0;
    let epoch = fixture.bridge.epoch();
    fixture.bridge = bridge::Bridge::new(&fixture.device, epoch);
    fixture.attach();
    fixture.push(&down(2));
    fixture.wait_app(2, 7);
    fixture.capture("restored-down");
    fixture.wait(Duration::from_millis(12));
    fixture.push(&motion(2, 500));
    fixture.wait_app(2, 8);
    fixture.capture("restored-motion");
    fixture.wait(Duration::from_millis(12));
    fixture.push(&motion(2, 700));
    fixture.wait_app(2, 9);
    fixture.wait(Duration::from_millis(12));
    fixture.push(&up(2));
    fixture.wait_app(0, 10);
    fixture.capture("neutral");
    assert_eq!(fixture.forwarded.len(), 6);
    assert!(fixture.bridge.neutral());
    assert_eq!(fixture.dispatches.len(), 3);
    assert!(fixture.proposals.is_empty());
    fixture.session.stop().unwrap();
    fixture.input_session.stop().unwrap();
    fixture.settle();
    fixture.logs.sync_all().unwrap();
    fixture.input_logs.sync_all().unwrap();
    let log = fs::read_to_string(output.join("touch-runtime.jsonl")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("touch-phase"))
            .count(),
        3
    );
    assert!(log.contains("touch-rule-disabled"));
    fs::write(output.join("touch-routing-receipts.json"),serde_json::to_vec_pretty(&serde_json::json!({
            "version":1,"status":"passed","routing":if scoped() {"scoped"} else {"coarse"},"model_source_and_readiness":true,"kernel_input_opened":false,"physical_takeover_tested":false,"production_broker_tested":false,
        "consumed_pan_frames":4,"consumed_pan_forwarded":0,"unmatched_tap_frames":2,"tap_interval":input_interval,
        "restored_pan_frames":4,"runtime_rule_revision":revision,"source_generations":[1,2],"dispatches":fixture.dispatches,
        "inputs":fixture.inputs,"forwarded":fixture.forwarded,
    })).unwrap()).unwrap();
}
