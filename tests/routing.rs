//! Explicitly isolated kernel gates; never part of ordinary cargo test.
//! Requires root, PREK_ISOLATED_ROUTING=1, a dedicated Linux environment with
//! /dev/uinput, /run/prek/devices and this build's packaged udev import rule.
//! Every opened/grabbed event node belongs to a virtual device created here.

#![cfg(target_os = "linux")]

use evdev::{InputEvent, raw_stream::RawDevice};
use nix::{
    libc,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    time::{ClockId, clock_gettime},
};
use prek::{
    input::{
        ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, Assembly, Axis,
        Device, EV_ABS, EV_KEY, EV_SYN, Frame, FrameAssembler, RawEvent, Source,
    },
    kernel::{Flush, Registry, Virtual},
    router::{Candidate, Limits, Policy, Router},
    transport::{Consumer, ConsumerEvidence, Descriptor, KernelIdentity, Readiness},
};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, OpenOptions},
    io,
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

nix::ioctl_write_ptr!(set_clock, b'E', 0xa0, libc::c_int);

fn capability_bits(raw: &RawDevice, kind: u16) -> Vec<u16> {
    let mut bytes = [0u8; 128];
    let request = nix::request_code_read!(b'E', 0x20 + u32::from(kind), bytes.len());
    // SAFETY: a live owned evdev fd; the kernel receives exactly the writable
    // bounded buffer size encoded in this read-only ioctl.
    assert!(unsafe { libc::ioctl(raw.as_raw_fd(), request, bytes.as_mut_ptr()) } >= 0);
    (0..bytes.len() * 8)
        .filter(|bit| bytes[bit / 8] & (1 << (bit % 8)) != 0)
        .map(|bit| bit as u16)
        .collect()
}

fn now() -> Duration {
    let time = clock_gettime(ClockId::CLOCK_MONOTONIC).unwrap();
    Duration::new(time.tv_sec() as u64, time.tv_nsec() as u32)
}

fn authorized() -> Registry {
    assert_eq!(
        std::env::var("PREK_ISOLATED_ROUTING").as_deref(),
        Ok("1"),
        "set PREK_ISOLATED_ROUTING=1 only in an explicitly authorized isolated environment"
    );
    // SAFETY: read-only current process credential query.
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "isolated kernel tests require administrator access"
    );
    Registry::system()
        .expect("provision the root-owned private device registry and packaged udev rule first")
}

fn fixture(touch: bool) -> Device {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let device = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut fixture = Device {
        identity: format!("test:kernel:{device}"),
        source: Source {
            device,
            generation: now().as_nanos() as u64,
            seat: "seat0".into(),
        },
        name: format!(
            "Prek test {} {device}",
            if touch { "touch" } else { "keyboard" }
        ),
        path: String::new(),
        physical: format!("fixture/{device}"),
        unique: String::new(),
        vendor: 0x1209,
        product: 0xbeef,
        properties: Vec::new(),
        axes: BTreeMap::new(),
        capabilities: BTreeMap::from([(0, vec![0]), (1, vec![30, 125])]),
        virtual_device: false,
    };
    if touch {
        let axis = Axis {
            minimum: 0,
            maximum: 4095,
            fuzz: 0,
            flat: 0,
            resolution: 20,
        };
        fixture.properties = vec![1];
        fixture.capabilities.insert(EV_KEY, vec![0x14a]);
        fixture.capabilities.insert(
            EV_ABS,
            vec![
                ABS_MT_SLOT,
                ABS_MT_POSITION_X,
                ABS_MT_POSITION_Y,
                ABS_MT_TRACKING_ID,
            ],
        );
        fixture.axes = BTreeMap::from([
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
        ]);
    }
    fixture
}

fn descriptor(device: &Device) -> Descriptor {
    Descriptor::mirror(
        device,
        KernelIdentity {
            bus: 3,
            vendor: device.vendor,
            product: device.product,
            version: 1,
        },
        None,
        &BTreeMap::new(),
        BTreeMap::new(),
    )
    .unwrap()
}

struct Receiver {
    raw: RawDevice,
    assembler: FrameAssembler,
    pending: VecDeque<Frame>,
}
impl Receiver {
    fn open(virtual_device: &Virtual, source: &Source) -> Self {
        assert!(virtual_device.isolated());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(nodes) = virtual_device.event_nodes() {
                assert!(nodes.len() <= 1);
                if let Some(path) = nodes.first()
                    && let Ok(file) = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                        .open(path)
                {
                    let number = file.metadata().unwrap().rdev();
                    let properties = fs::read_to_string(format!(
                        "/run/udev/data/c{}:{}",
                        libc::major(number),
                        libc::minor(number)
                    ))
                    .unwrap_or_default();
                    if properties.lines().any(|line| line == "E:PREK_TEST=1")
                        && properties.lines().any(|line| line == "E:PREK_VIRTUAL=1")
                        && properties
                            .lines()
                            .any(|line| line == "E:LIBINPUT_IGNORE_DEVICE=1")
                    {
                        let mut raw = RawDevice::from_fd(file.into()).unwrap();
                        for (key, value) in &virtual_device.descriptor().udev {
                            let expected = format!("E:{key}={value}");
                            assert!(properties.lines().any(|line| line == expected));
                        }
                        assert_eq!(raw.physical_path(), Some(virtual_device.marker()));
                        assert_eq!(raw.name(), Some(virtual_device.descriptor().name.as_str()));
                        let id = raw.input_id();
                        let expected = virtual_device.descriptor().identity;
                        assert_eq!(
                            (id.bus_type().0, id.vendor(), id.product(), id.version()),
                            (
                                expected.bus,
                                expected.vendor,
                                expected.product,
                                expected.version
                            )
                        );
                        assert_eq!(
                            raw.properties()
                                .iter()
                                .map(|value| value.0)
                                .collect::<Vec<_>>(),
                            virtual_device.descriptor().properties
                        );
                        let spec = virtual_device.descriptor();
                        assert_eq!(
                            raw.supported_events()
                                .iter()
                                .map(|kind| kind.0)
                                .collect::<Vec<_>>(),
                            spec.capabilities.keys().copied().collect::<Vec<_>>()
                        );
                        // EVIOCGBIT(0) reports event kinds, not SYN subcodes.
                        for (&kind, codes) in &spec.capabilities {
                            if kind != EV_SYN {
                                assert_eq!(capability_bits(&raw, kind), *codes);
                            }
                        }
                        let axes: BTreeMap<_, _> = raw
                            .get_absinfo()
                            .unwrap()
                            .map(|(code, info)| {
                                (
                                    code.0,
                                    Axis {
                                        minimum: info.minimum(),
                                        maximum: info.maximum(),
                                        resolution: info.resolution(),
                                        fuzz: info.fuzz(),
                                        flat: info.flat(),
                                    },
                                )
                            })
                            .collect();
                        assert_eq!(axes, virtual_device.descriptor().axes);
                        assert!(raw.get_key_state().unwrap().iter().next().is_none());
                        let clock = libc::CLOCK_MONOTONIC;
                        // SAFETY: our owned isolated event fd and initialized c_int.
                        unsafe { set_clock(raw.as_raw_fd(), &clock) }.unwrap();
                        raw.grab()
                            .expect("grab only this owned test node, never production input");
                        return Self {
                            raw,
                            assembler: FrameAssembler::new(source.clone()),
                            pending: VecDeque::new(),
                        };
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "isolated readiness failed: require processed ignore metadata before any test injection"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn next(&mut self, timeout: Duration) -> Option<Frame> {
        if let Some(frame) = self.pending.pop_front() {
            return Some(frame);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let mut fds = [PollFd::new(self.raw.as_fd(), PollFlags::POLLIN)];
            poll(&mut fds, PollTimeout::try_from(remaining).unwrap()).unwrap();
            match self.raw.fetch_events() {
                Ok(events) => {
                    for event in events {
                        let at = event
                            .timestamp()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap();
                        match self.assembler.push(
                            RawEvent {
                                kind: event.event_type().0,
                                code: event.code(),
                                value: event.value(),
                            },
                            at,
                        ) {
                            Assembly::Frame(frame) => self.pending.push_back(frame),
                            Assembly::Lost => panic!("isolated test source lost input"),
                            Assembly::Pending => {}
                        }
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => panic!("isolated receiver failed: {error}"),
            }
            if let Some(frame) = self.pending.pop_front() {
                return Some(frame);
            }
        }
    }
}

fn send(output: &mut Virtual, sequence: u64, events: &[(u16, u16, i32)]) {
    let mut events: Vec<_> = events
        .iter()
        .map(|&(kind, code, value)| RawEvent { kind, code, value })
        .collect();
    events.push(RawEvent {
        kind: EV_SYN,
        code: 0,
        value: 0,
    });
    output
        .enqueue(&Frame {
            source: output.descriptor().source.clone(),
            at: now(),
            sequence,
            events,
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_millis(250);
    while output.flush().unwrap() != Flush::Complete {
        assert!(Instant::now() < deadline, "isolated output stalled");
        thread::sleep(Duration::from_millis(1));
    }
}

fn receiver_identity() -> Consumer {
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
    let fields = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    Consumer::IsolatedReceiver {
        pid: std::process::id(),
        start: fields[19].parse().unwrap(),
    }
}

fn route(device: Device, output: &Virtual) -> Router {
    let time = now();
    let spec = output.descriptor();
    let consumer = receiver_identity();
    let mut gate = Readiness::new(spec.clone(), consumer.clone(), time).unwrap();
    gate.metadata(spec, time).unwrap();
    gate.consumer(
        &ConsumerEvidence {
            consumer,
            marker: spec.marker.clone(),
            name: spec.name.clone(),
            class: spec.class,
            seat: device.source.seat.clone(),
            settings: BTreeMap::new(),
            at: time,
        },
        time,
    )
    .unwrap();
    gate.neutral(true, time).unwrap();
    let ready = gate.finish(time).unwrap();
    Router::new(device, &ready, 1, Limits::default(), time).unwrap()
}

fn deliver(router: &mut Router, output: &mut Virtual, time: Duration) -> bool {
    let Some(frame) = router.due(time).unwrap().cloned() else {
        return false;
    };
    output.enqueue(&frame.frame).unwrap();
    assert_eq!(output.flush().unwrap(), Flush::Complete);
    router.delivered(frame.ticket, now()).unwrap();
    true
}

fn passthrough(original: Device, device: Device, frames: &[Vec<(u16, u16, i32)>]) {
    let registry = authorized();
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&original)).unwrap()).unwrap();
    let mut output =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut input = Receiver::open(&source, &device.source);
    let mut target = Receiver::open(&output, &device.source);
    let mut router = route(device, &output);
    for (index, events) in frames.iter().enumerate() {
        send(&mut source, index as u64 + 1, events);
        let frame = input.next(Duration::from_secs(1)).expect("source event");
        router.push(frame.clone(), None, now()).unwrap();
        assert!(deliver(&mut router, &mut output, now()));
        assert_eq!(
            target.next(Duration::from_secs(1)).unwrap().events,
            frame.events
        );
    }
    assert!(target.next(Duration::from_millis(30)).is_none());
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t46_isolated_mouse_preserves_motion_buttons_and_both_wheel_resolutions() {
    let mouse = || {
        let mut device = fixture(false);
        device.name = "Prek test mouse".into();
        device.capabilities = BTreeMap::from([
            (EV_SYN, vec![0]),
            (EV_KEY, vec![272, 273]),
            (2, vec![0, 1, 6, 8, 11, 12]),
            (4, vec![4]),
        ]);
        device
    };
    passthrough(
        mouse(),
        mouse(),
        &[
            vec![
                (4, 4, 272),
                (EV_KEY, 272, 1),
                (2, 0, 7),
                (2, 1, -4),
                (2, 8, 1),
                (2, 11, 120),
            ],
            vec![(2, 0, -2), (2, 1, 8), (2, 6, -1), (2, 12, -120)],
            vec![(4, 4, 272), (EV_KEY, 272, 0)],
        ],
    );
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t48_isolated_touchpad_preserves_slots_tool_flags_and_button_clicks() {
    let touchpad = || {
        let mut device = fixture(true);
        device.name = "Prek test touchpad".into();
        device.properties = vec![0, 2];
        device
            .capabilities
            .insert(EV_KEY, vec![272, 0x145, 0x14a, 0x14d]);
        device
    };
    passthrough(
        touchpad(),
        touchpad(),
        &[
            vec![
                (EV_KEY, 0x14a, 1),
                (EV_KEY, 0x145, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 42),
                (EV_ABS, ABS_MT_POSITION_X, 100),
                (EV_ABS, ABS_MT_POSITION_Y, 200),
            ],
            vec![
                (EV_KEY, 0x145, 0),
                (EV_KEY, 0x14d, 1),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 43),
                (EV_ABS, ABS_MT_POSITION_X, 300),
                (EV_ABS, ABS_MT_POSITION_Y, 400),
                (EV_KEY, 272, 1),
            ],
            vec![
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_POSITION_X, 110),
                (EV_ABS, ABS_MT_POSITION_Y, 220),
                (EV_KEY, 272, 0),
            ],
            vec![
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                (EV_KEY, 0x14d, 0),
                (EV_KEY, 0x14a, 0),
            ],
        ],
    );
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t55_isolated_keyboard_led_feedback_reaches_source_without_echo() {
    let registry = authorized();
    let keyboard = || {
        let mut device = fixture(false);
        device.capabilities.insert(17, vec![0]);
        device
    };
    let original = keyboard();
    let device = keyboard();
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&original)).unwrap()).unwrap();
    let mut output =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut input = Receiver::open(&source, &original.source);
    let mut target = Receiver::open(&output, &device.source);
    for value in [1, 0] {
        target
            .raw
            .send_events(&[InputEvent::new(17, 0, value)])
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let feedback = loop {
            let events = output.feedback().unwrap();
            if !events.is_empty() {
                break events;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(
            feedback,
            [RawEvent {
                kind: 17,
                code: 0,
                value
            }]
        );
        let events: Vec<_> = feedback
            .iter()
            .map(|event| InputEvent::new(event.kind, event.code, event.value))
            .collect();
        input.raw.send_events(&events).unwrap();
        assert_eq!(
            input
                .raw
                .get_led_state()
                .unwrap()
                .contains(evdev::LedCode(0)),
            value == 1
        );
        assert_eq!(source.feedback().unwrap(), feedback);
        assert!(output.feedback().unwrap().is_empty());
    }
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t46_isolated_keyboard_receives_passthrough_once_and_drop_removes_nodes() {
    let registry = authorized();
    let original = fixture(false);
    let device = fixture(false);
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&original)).unwrap()).unwrap();
    let mut output =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut input = Receiver::open(&source, &device.source);
    let mut target = Receiver::open(&output, &device.source);
    let output_nodes = output.event_nodes().unwrap();
    let mut router = route(device, &output);
    for (sequence, events) in [
        (1, vec![(EV_KEY, 125, 1), (EV_KEY, 30, 1)]),
        (2, vec![(EV_KEY, 30, 2)]),
        (3, vec![(EV_KEY, 30, 0), (EV_KEY, 125, 0)]),
    ] {
        send(&mut source, sequence, &events);
        let frame = input.next(Duration::from_secs(1)).expect("source event");
        router.push(frame.clone(), None, now()).unwrap();
        assert!(deliver(&mut router, &mut output, now()));
        assert_eq!(
            target.next(Duration::from_secs(1)).unwrap().events,
            frame.events
        );
    }
    assert!(target.next(Duration::from_millis(30)).is_none());
    drop(target);
    drop(output);
    let deadline = Instant::now() + Duration::from_secs(2);
    while output_nodes.iter().any(|path| Path::new(path).exists()) {
        assert!(
            Instant::now() < deadline,
            "virtual output leaked after drop"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t50_isolated_claimed_touch_reaches_no_downstream_contact_or_click() {
    let registry = authorized();
    let original = fixture(true);
    let device = fixture(true);
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&original)).unwrap()).unwrap();
    let mut output =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut input = Receiver::open(&source, &device.source);
    let mut target = Receiver::open(&output, &device.source);
    let mut router = route(device, &output);
    send(
        &mut source,
        1,
        &[
            (EV_KEY, 0x14a, 1),
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 42),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
        ],
    );
    let frame = input.next(Duration::from_secs(1)).unwrap();
    router
        .push(
            frame,
            Some(Candidate {
                id: 1,
                authorization: 1,
                policy: Policy::Buffer {
                    join: Duration::ZERO,
                    decision: Duration::from_millis(300),
                },
            }),
            now(),
        )
        .unwrap();
    router.claim(1, 1, now()).unwrap();
    send(
        &mut source,
        2,
        &[(EV_KEY, 0x14a, 0), (EV_ABS, ABS_MT_TRACKING_ID, -1)],
    );
    let frame = input.next(Duration::from_secs(1)).unwrap();
    router.push(frame, None, now()).unwrap();
    assert!(!deliver(&mut router, &mut output, now()));
    assert!(target.next(Duration::from_millis(50)).is_none());
}

#[test]
#[ignore = "requires authorized isolated uinput environment and packaged pre-add udev rule"]
fn t51_isolated_buffered_tap_preserves_delivery_interval_without_bursting() {
    let registry = authorized();
    let original = fixture(true);
    let device = fixture(true);
    let mut source =
        Virtual::create(registry.prepare_isolated(descriptor(&original)).unwrap()).unwrap();
    let mut output =
        Virtual::create(registry.prepare_isolated(descriptor(&device)).unwrap()).unwrap();
    let mut input = Receiver::open(&source, &device.source);
    let mut target = Receiver::open(&output, &device.source);
    let mut router = route(device, &output);
    send(
        &mut source,
        1,
        &[
            (EV_KEY, 0x14a, 1),
            (EV_ABS, ABS_MT_TRACKING_ID, 42),
            (EV_ABS, ABS_MT_POSITION_X, 100),
            (EV_ABS, ABS_MT_POSITION_Y, 200),
        ],
    );
    let first = input.next(Duration::from_secs(1)).unwrap();
    router
        .push(
            first.clone(),
            Some(Candidate {
                id: 1,
                authorization: 1,
                policy: Policy::Buffer {
                    join: Duration::from_millis(120),
                    decision: Duration::from_millis(300),
                },
            }),
            now(),
        )
        .unwrap();
    thread::sleep(Duration::from_millis(40));
    send(
        &mut source,
        2,
        &[(EV_KEY, 0x14a, 0), (EV_ABS, ABS_MT_TRACKING_ID, -1)],
    );
    let last = input.next(Duration::from_secs(1)).unwrap();
    router.push(last.clone(), None, now()).unwrap();
    router.cancel(1, 1, now()).unwrap();
    assert!(deliver(&mut router, &mut output, now()));
    let down = target.next(Duration::from_secs(1)).unwrap();
    assert_eq!(down.events, first.events);
    let deadline = Instant::now() + Duration::from_millis(300);
    while !deliver(&mut router, &mut output, now()) {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let up = target.next(Duration::from_secs(1)).unwrap();
    assert_eq!(up.events, last.events);
    let physical_interval = last.at - first.at;
    let virtual_interval = up.at - down.at;
    assert!(
        virtual_interval >= Duration::from_millis(20),
        "buffered tap was burst into a click"
    );
    assert!(virtual_interval.abs_diff(physical_interval) <= Duration::from_millis(20));
    assert!(target.next(Duration::from_millis(30)).is_none());
}
