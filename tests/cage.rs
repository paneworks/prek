//! Actual Wayland delivery gates in the disposable Cage VM. Context is an
//! explicit model fixture; this does not authenticate a production broker.
#![cfg(target_os = "linux")]

#[path = "support/kernel.rs"]
mod kernel_fixture;

use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, InputEvent, InputId, KeyCode, PropType,
    UinputAbsSetup, raw_stream::RawDevice, uinput::VirtualDevice,
};
use nix::libc;
use prek::{
    config::{Config, Screen},
    context::{Binding, OutputState, SessionKind, Snapshot, State},
    gestures::{Mapping, Output, Point, Transform},
    input::{Assembly, Device, EV_ABS, EV_KEY, Frame, FrameAssembler, RawEvent},
    kernel::{Flush, Registry, Virtual},
    protocol::Capability,
    replay::Record,
    routing::Routing,
    transport::{Consumer, ConsumerEvidence, Readiness, Ready},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{self, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

const CONTROL: &str = "/run/prek-cage";
nix::ioctl_write_ptr!(set_clock, b'E', 0xa0, libc::c_int);
nix::ioctl_read_buf!(get_mt_slots, b'E', 0x0a, libc::c_int);

fn witness(name: &str, content: impl AsRef<[u8]>) {
    let temporary = format!("{CONTROL}/{name}.new");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .unwrap();
    file.write_all(content.as_ref()).unwrap();
    fs::rename(temporary, format!("{CONTROL}/{name}")).unwrap();
}

fn consumer(pid: u32) -> Consumer {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    assert!(
        status
            .lines()
            .any(|line| line == "Uid:\t1000\t1000\t1000\t1000")
    );
    assert_eq!(
        fs::read_link(format!("/proc/{pid}/exe")).unwrap(),
        fs::canonicalize(std::env::var("PREK_CAGE_EXE").unwrap()).unwrap()
    );
    let start = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap();
    Consumer::IsolatedReceiver {
        pid,
        start: start.parse().unwrap(),
    }
}

fn opened_by(pid: u32, path: &Path) -> bool {
    fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| fs::read_link(entry.path()).is_ok_and(|target| target == path))
}

fn wait_open(pid: u32, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !opened_by(pid, path) {
        assert!(
            Instant::now() < deadline,
            "Cage did not open {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

struct Input {
    device: Device,
    // Destruction order on an ordinary unwind: output precedes original grab.
    output: Option<Virtual>,
    delivered: u64,
    absolute: Option<prek::input::AbsoluteState>,
    raw: RawDevice,
    assembler: FrameAssembler,
    emitter: VirtualDevice,
    path: PathBuf,
}

impl Input {
    fn new(touch: bool, pid: u32) -> Self {
        let mut device = kernel_fixture::fixture(touch);
        if touch {
            let axis = device.axes[&53];
            device.axes.insert(0, axis);
            device.axes.insert(1, axis);
            device.capabilities.get_mut(&EV_ABS).unwrap().extend([0, 1]);
            device
                .capabilities
                .get_mut(&EV_ABS)
                .unwrap()
                .sort_unstable();
        } else {
            device.capabilities.insert(EV_KEY, vec![30, 42, 48, 125]);
        }
        let keys =
            AttributeSet::from_iter(device.capabilities[&EV_KEY].iter().copied().map(KeyCode));
        let mut builder = VirtualDevice::builder()
            .unwrap()
            .name(&device.name)
            .input_id(InputId::new(
                BusType::BUS_USB,
                device.vendor,
                device.product,
                1,
            ))
            .with_keys(&keys)
            .unwrap();
        if touch {
            builder = builder
                .with_properties(&AttributeSet::from_iter([PropType::DIRECT]))
                .unwrap();
            for (&code, axis) in &device.axes {
                builder = builder
                    .with_absolute_axis(&UinputAbsSetup::new(
                        AbsoluteAxisCode(code),
                        AbsInfo::new(
                            0,
                            axis.minimum,
                            axis.maximum,
                            axis.fuzz,
                            axis.flat,
                            axis.resolution,
                        ),
                    ))
                    .unwrap();
            }
        }
        let mut emitter = builder.build().unwrap();
        let path = emitter
            .enumerate_dev_nodes_blocking()
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        wait_open(pid, &path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .unwrap();
        let raw = RawDevice::from_fd(file.into()).unwrap();
        let clock = libc::CLOCK_MONOTONIC;
        // SAFETY: owned fixture evdev fd, valid EVIOCSCLOCKID argument.
        unsafe { set_clock(raw.as_raw_fd(), &clock) }.unwrap();
        Self {
            assembler: FrameAssembler::new(device.source.clone()),
            device,
            output: None,
            delivered: 0,
            absolute: None,
            raw,
            emitter,
            path,
        }
    }

    fn emit(&mut self, events: &[(u16, u16, i32)]) {
        self.emitter
            .emit(
                &events
                    .iter()
                    .map(|&(kind, code, value)| InputEvent::new(kind, code, value))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
    }

    fn drain(&mut self, routing: &mut Option<Routing>) {
        match self.raw.fetch_events() {
            Ok(events) => {
                for event in events {
                    let at = event.timestamp().duration_since(UNIX_EPOCH).unwrap();
                    match self.assembler.push(
                        RawEvent {
                            kind: event.event_type().0,
                            code: event.code(),
                            value: event.value(),
                        },
                        at,
                    ) {
                        Assembly::Frame(frame) => {
                            if let Some(routing) = routing {
                                routing.push(Record::Frame { frame }).unwrap();
                            }
                        }
                        Assembly::Lost => panic!("owned Cage source lost an input frame"),
                        Assembly::Pending => {}
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => panic!("Cage source failed: {error}"),
        }
    }

    fn neutral(&self) -> bool {
        if self.raw.get_key_state().unwrap().iter().next().is_some() {
            return false;
        }
        let Some(capacity) = self.device.touch_capacity() else {
            return true;
        };
        let mut values = vec![0; capacity + 1];
        values[0] = 57; // ABS_MT_TRACKING_ID, followed by every Type B slot.
        // SAFETY: this fixture's owned evdev fd and exactly the bounded i32
        // buffer encoded in EVIOCGMTSLOTS, including its requested axis.
        unsafe { get_mt_slots(self.raw.as_raw_fd(), &mut values) }.unwrap();
        values[1..].iter().all(|&id| id == -1)
    }

    fn prepare(&mut self, registry: &Registry, pid: u32) -> Ready {
        assert!(
            self.neutral(),
            "the controlled fixture must be neutral at handoff"
        );
        let absolute = prek::linux::absolute_state(&self.raw, &self.device).unwrap();
        let mut output = Virtual::create(
            registry
                .prepare_isolated(kernel_fixture::descriptor(&self.device))
                .unwrap(),
        )
        .unwrap();
        output
            .initialize_absolute(&absolute, kernel_fixture::now())
            .unwrap();
        let deadline = Instant::now() + Duration::from_millis(250);
        while output.flush().unwrap() != Flush::Complete {
            assert!(Instant::now() < deadline, "initial absolute state stalled");
            thread::sleep(Duration::from_millis(1));
        }
        self.absolute = Some(absolute);
        let deadline = Instant::now() + Duration::from_secs(3);
        let node = loop {
            if let Some(node) = output.event_nodes().unwrap().into_iter().next()
                && opened_by(pid, &node)
            {
                break node;
            }
            assert!(
                Instant::now() < deadline,
                "Cage did not accept the owned virtual output"
            );
            thread::sleep(Duration::from_millis(10));
        };
        let properties = fs::read_to_string(format!(
            "/run/udev/data/c{}:{}",
            {
                use std::os::unix::fs::MetadataExt;
                libc::major(fs::metadata(&node).unwrap().rdev())
            },
            {
                use std::os::unix::fs::MetadataExt;
                libc::minor(fs::metadata(&node).unwrap().rdev())
            }
        ))
        .unwrap();
        for required in [
            "E:PREK_TEST=1",
            "E:PREK_VIRTUAL=1",
            "E:LIBINPUT_IGNORE_DEVICE=0",
        ] {
            assert!(
                properties.lines().any(|line| line == required),
                "{properties}"
            );
        }
        let time = kernel_fixture::now();
        let identity = consumer(pid);
        let spec = output.descriptor();
        let mut gate = Readiness::new(spec.clone(), identity.clone(), time).unwrap();
        gate.metadata(spec, time).unwrap();
        gate.consumer(
            &ConsumerEvidence {
                consumer: identity,
                marker: spec.marker.clone(),
                name: spec.name.clone(),
                class: spec.class,
                seat: self.device.source.seat.clone(),
                settings: BTreeMap::new(),
                at: time,
            },
            time,
        )
        .unwrap();
        gate.neutral(self.neutral(), time).unwrap();
        let ready = gate.finish(time).unwrap();
        // Emission is controlled by this same thread. No concurrent-input
        // handoff claim is made here; production's race gate remains pending.
        self.raw.grab().unwrap();
        assert!(self.neutral());
        witness(
            if self.device.axes.is_empty() {
                "keyboard-output"
            } else {
                "touch-output"
            },
            node.as_os_str().as_encoded_bytes(),
        );
        self.output = Some(output);
        ready
    }

    fn deliver(&mut self, routing: &mut Routing) {
        while let Some(frame) = routing
            .due(&self.device.source, kernel_fixture::now())
            .unwrap()
            .cloned()
        {
            let output = self.output.as_mut().unwrap();
            output.enqueue(&frame.frame).unwrap();
            let deadline = Instant::now() + Duration::from_millis(250);
            while output.flush().unwrap() != Flush::Complete {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
            routing
                .delivered(&self.device.source, frame.ticket, kernel_fixture::now())
                .unwrap();
            self.delivered = frame.frame.sequence;
        }
    }
}

fn context(routing: &mut Routing, geometry: &Output, revision: u64) {
    let time = kernel_fixture::now();
    routing
        .authority_mut()
        .update(
            Snapshot {
                generation: 1,
                revision,
                captured_at: time,
                state: State::Desktop,
                outputs: vec![OutputState {
                    geometry: geometry.clone(),
                    geometry_generation: 2,
                    active_workspace: Some(1),
                    screen: Screen::On,
                    keyboard: None,
                }],
                windows: vec![],
                regions: vec![],
                secure_overlay: false,
            },
            time,
        )
        .unwrap();
    routing.actions_available(1, BTreeSet::from([Capability::Observations]), time);
}

fn touch(input: &mut Input, command: &str) {
    let fingers = if command.starts_with("two-") { 2 } else { 1 };
    let mut events = Vec::new();
    if command.ends_with("down") {
        events.extend([(EV_KEY, 0x14a, 1), (EV_ABS, 0, 1000), (EV_ABS, 1, 1000)]);
        for slot in 0..fingers {
            events.extend([
                (EV_ABS, 47, slot),
                (EV_ABS, 57, slot + 10),
                (EV_ABS, 53, 1000 + slot * 500),
                (EV_ABS, 54, 1000),
            ]);
        }
    } else if command.ends_with("move") {
        let y = if command.contains("far") { 3000 } else { 2200 };
        events.push((EV_ABS, 1, y));
        for slot in 0..fingers {
            events.extend([(EV_ABS, 47, slot), (EV_ABS, 54, y)]);
        }
    } else if command.ends_with("up") {
        for slot in 0..fingers {
            events.extend([(EV_ABS, 47, slot), (EV_ABS, 57, -1)]);
        }
        events.push((EV_KEY, 0x14a, 0));
    } else {
        panic!("unknown touch command {command}");
    }
    input.emit(&events);
}

#[test]
#[ignore = "requires an explicitly isolated Cage VM"]
fn cage_controlled_input_fixture() {
    assert_eq!(std::env::var("PREK_CAGE_VM").as_deref(), Ok("1"));
    let registry = kernel_fixture::authorized();
    let pid: u32 = fs::read_to_string(format!("{CONTROL}/cage-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let Consumer::IsolatedReceiver { start, .. } = consumer(pid) else {
        unreachable!()
    };
    let mut keyboard = Input::new(false, pid);
    let mut touchscreen = Input::new(true, pid);
    witness("keyboard", keyboard.path.as_os_str().as_encoded_bytes());
    witness("touch", touchscreen.path.as_os_str().as_encoded_bytes());
    let geometry: Vec<u32> = fs::read_to_string(format!("{CONTROL}/geometry"))
        .unwrap()
        .split_whitespace()
        .map(|part| part.parse().unwrap())
        .collect();
    assert_eq!(geometry.len(), 2);
    let geometry = Output {
        name: "Cage fixture".into(),
        pixel_width: f64::from(geometry[0]),
        pixel_height: f64::from(geometry[1]),
        scale: 1.0,
        origin: Point { x: 0.0, y: 0.0 },
        transform: Transform::Normal,
    };
    let mut routing: Option<Routing> = None;
    let mut revision = 0;
    let mut previous = String::new();
    let mut dispatch = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(format!("{CONTROL}/dispatch.jsonl"))
        .unwrap();
    witness("ready", b"ready");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(
            Instant::now() < deadline,
            "independent fixture deadline expired"
        );
        // Input is checked against the previous context receipt, whose
        // acknowledgement predates these events. Refresh before the next tick.
        keyboard.drain(&mut routing);
        touchscreen.drain(&mut routing);
        if let Some(routing) = &mut routing {
            revision += 1;
            context(routing, &geometry, revision);
        }
        if let Some(routing) = &mut routing {
            routing
                .push(Record::Tick {
                    at: kernel_fixture::now(),
                })
                .unwrap();
            keyboard.deliver(routing);
            touchscreen.deliver(routing);
            for event in routing.take_dispatches(kernel_fixture::now()) {
                serde_json::to_writer(&mut dispatch, &event).unwrap();
                writeln!(dispatch).unwrap();
            }
            dispatch.flush().unwrap();
            let _ = routing.take_events();
            assert!(
                routing.take_recoveries().is_empty(),
                "unexpected route retirement"
            );
        }
        if let Ok(message) = fs::read_to_string(format!("{CONTROL}/emit"))
            && message != previous
        {
            let (_, command) = message.trim().split_once(' ').unwrap();
            match command {
                "a-down" => keyboard.emit(&[(EV_KEY, 30, 1)]),
                "a-up" => keyboard.emit(&[(EV_KEY, 30, 0)]),
                "b-down" => keyboard.emit(&[(EV_KEY, 48, 1)]),
                "b-up" => keyboard.emit(&[(EV_KEY, 48, 0)]),
                "shift-down" => keyboard.emit(&[(EV_KEY, 42, 1)]),
                "shift-up" => keyboard.emit(&[(EV_KEY, 42, 0)]),
                "meta-down" => keyboard.emit(&[(EV_KEY, 125, 1)]),
                "meta-up" => keyboard.emit(&[(EV_KEY, 125, 0)]),
                "tail-down" => touchscreen.emit(&[
                    (EV_KEY, 0x14a, 1),
                    (EV_ABS, 0, 1500),
                    (EV_ABS, 1, 3000),
                    (EV_ABS, 47, 1),
                    (EV_ABS, 57, 11),
                    (EV_ABS, 53, 1500),
                    (EV_ABS, 54, 3000),
                ]),
                "head-down" => touchscreen.emit(&[
                    (EV_ABS, 47, 0),
                    (EV_ABS, 57, 10),
                    (EV_ABS, 53, 1000),
                    (EV_ABS, 54, 3000),
                ]),
                command if command.starts_with("one-") || command.starts_with("two-") => {
                    touch(&mut touchscreen, command)
                }
                "route" => {
                    assert!(routing.is_none());
                    // This explicitly isolated fixture loads the real Luna configuration;
                    // handlers/adapter effects are outside this transport gate.
                    let config = Config::parse_named(
                        &fs::read_to_string(format!("{CONTROL}/config.lua")).unwrap(),
                        "cage.lua",
                    )
                    .unwrap();
                    let mut next = Routing::new(config).unwrap();
                    next.authority_mut()
                        .bind(
                            Binding {
                                seat: "seat0".into(),
                                session: "cage-model".into(),
                                uid: 1000,
                                compositor_pid: pid,
                                compositor_start: start,
                                kind: SessionKind::User,
                            },
                            true,
                            false,
                        )
                        .unwrap();
                    let key_ready = keyboard.prepare(&registry, pid);
                    let touch_ready = touchscreen.prepare(&registry, pid);
                    revision += 1;
                    context(&mut next, &geometry, revision);
                    next.attach_with_state(
                        keyboard.device.clone(),
                        &key_ready,
                        None,
                        keyboard.absolute.clone(),
                        kernel_fixture::now(),
                    )
                    .unwrap();
                    let mut mapping = Mapping::from_device(&touchscreen.device).unwrap();
                    mapping.output = Some(geometry.clone());
                    mapping.generation = 2;
                    next.attach_with_state(
                        touchscreen.device.clone(),
                        &touch_ready,
                        Some(mapping),
                        touchscreen.absolute.clone(),
                        kernel_fixture::now(),
                    )
                    .unwrap();
                    routing = Some(next);
                }
                "stop" => {
                    if let Some(routing) = &mut routing {
                        routing.recover(&keyboard.device.source);
                        routing.recover(&touchscreen.device.source);
                        for recovery in routing.take_recoveries() {
                            let input = if recovery.source == keyboard.device.source {
                                &mut keyboard
                            } else {
                                &mut touchscreen
                            };
                            if !recovery.neutral_events.is_empty()
                                && let Some(output) = &mut input.output
                            {
                                let frame = Frame {
                                    source: recovery.source,
                                    sequence: input.delivered + 1,
                                    at: kernel_fixture::now(),
                                    events: recovery.neutral_events,
                                };
                                output.enqueue(&frame).unwrap();
                                assert_eq!(output.flush().unwrap(), Flush::Complete);
                            }
                        }
                    }
                    drop(keyboard.output.take());
                    drop(touchscreen.output.take());
                    if routing.is_some() {
                        keyboard.raw.ungrab().unwrap();
                        touchscreen.raw.ungrab().unwrap();
                    }
                    witness("emitted", &message);
                    break;
                }
                _ => panic!("unknown Cage command {command}"),
            }
            witness("emitted", &message);
            previous = message;
        }
        thread::sleep(Duration::from_millis(5));
    }
    // Original nodes disappear after all owned reader/emitter fds are gone.
    let paths = [keyboard.path.clone(), touchscreen.path.clone()];
    drop(keyboard);
    drop(touchscreen);
    witness("finished", serde_json::to_vec(&paths).unwrap());
}
