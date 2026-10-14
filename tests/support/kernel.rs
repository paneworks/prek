//! Shared isolated-kernel fixtures. Only tests may construct these devices.
#![allow(dead_code)]

use evdev::raw_stream::RawDevice;
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
    router::{Limits, Router},
    transport::{Consumer, ConsumerEvidence, Descriptor, KernelIdentity, Readiness, Ready},
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

pub fn now() -> Duration {
    let time = clock_gettime(ClockId::CLOCK_MONOTONIC).unwrap();
    Duration::new(time.tv_sec() as u64, time.tv_nsec() as u32)
}

pub fn authorized() -> Registry {
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

pub fn fixture(touch: bool) -> Device {
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

pub fn descriptor(device: &Device) -> Descriptor {
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

pub struct Receiver {
    pub raw: RawDevice,
    assembler: FrameAssembler,
    pending: VecDeque<Frame>,
}
impl Receiver {
    pub fn from_test_node(path: &Path, source: &Source, marker: &str) -> Self {
        assert!(marker.starts_with("prek/test/"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .unwrap();
        let number = file.metadata().unwrap().rdev();
        let properties = fs::read_to_string(format!(
            "/run/udev/data/c{}:{}",
            libc::major(number),
            libc::minor(number)
        ))
        .unwrap();
        for required in [
            "E:PREK_TEST=1",
            "E:PREK_VIRTUAL=1",
            "E:LIBINPUT_IGNORE_DEVICE=1",
        ] {
            assert!(properties.lines().any(|line| line == required));
        }
        let mut raw = RawDevice::from_fd(file.into()).unwrap();
        assert_eq!(raw.physical_path(), Some(marker));
        assert!(raw.name().unwrap().starts_with("Prek virtual "));
        assert!(raw.get_key_state().unwrap().iter().next().is_none());
        let clock = libc::CLOCK_MONOTONIC;
        // SAFETY: verified isolated event fd and initialized clock value.
        unsafe { set_clock(raw.as_raw_fd(), &clock) }.unwrap();
        raw.grab().unwrap();
        Self {
            raw,
            assembler: FrameAssembler::new(source.clone()),
            pending: VecDeque::new(),
        }
    }

    pub fn open(virtual_device: &Virtual, source: &Source) -> Self {
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

    pub fn next(&mut self, timeout: Duration) -> Option<Frame> {
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

pub fn send(output: &mut Virtual, sequence: u64, events: &[(u16, u16, i32)]) {
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

pub fn readiness(device: &Device, output: &Virtual) -> Ready {
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
    gate.finish(time).unwrap()
}
pub fn route(device: Device, output: &Virtual) -> Router {
    let ready = readiness(&device, output);
    Router::new(device, &ready, 1, Limits::default(), now()).unwrap()
}

pub fn deliver(router: &mut Router, output: &mut Virtual, time: Duration) -> bool {
    let Some(frame) = router.due(time).unwrap().cloned() else {
        return false;
    };
    output.enqueue(&frame.frame).unwrap();
    assert_eq!(output.flush().unwrap(), Flush::Complete);
    router.delivered(frame.ticket, now()).unwrap();
    true
}
