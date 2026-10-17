use super::*;
use crate::input::EV_KEY;
use std::{
    thread,
    time::{Duration, Instant},
};

nix::ioctl_read!(get_repeat, b'E', 3, [u32; 2]);
nix::ioctl_write_ptr!(set_repeat, b'E', 3, [u32; 2]);

struct Experiment {
    output: Virtual,
    reader: RawDevice,
}

impl Experiment {
    fn create(publication: Publication) -> Self {
        publication.verify().unwrap();
        let spec = &publication.descriptor;
        assert!(spec.is_isolated());
        assert_eq!(
            spec.capabilities,
            BTreeMap::from([(0, vec![0]), (1, vec![30]), (20, vec![0, 1])])
        );
        assert!(spec.axes.is_empty() && spec.properties.is_empty());
        assert_eq!(
            supported_backend(spec).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open("/dev/uinput")
            .unwrap();
        let mut output = Virtual {
            file,
            publication,
            pending: None,
            stopped: false,
            created: false,
            sequence: None,
        };
        let spec = output.descriptor();
        // SAFETY: uinput_setup contains only integer fields and arrays.
        let mut setup: libc::uinput_setup = unsafe { std::mem::zeroed() };
        setup.id = libc::input_id {
            bustype: spec.identity.bus,
            vendor: spec.identity.vendor,
            product: spec.identity.product,
            version: spec.identity.version,
        };
        for (slot, byte) in setup.name.iter_mut().zip(spec.name.bytes()) {
            *slot = byte as libc::c_char;
        }
        let marker = CString::new(spec.marker.clone()).unwrap();
        let fd = output.file.as_raw_fd();
        // SAFETY: this test owns the uinput fd and its validated isolated publication.
        unsafe {
            for kind in [0, 1, 20] {
                event_bit(fd, kind).unwrap();
            }
            key_bit(fd, 30).unwrap();
            physical_path(fd, marker.as_ptr()).unwrap();
            device_setup(fd, &setup).unwrap();
            create_device(fd).unwrap();
        }
        output.created = true;
        let deadline = Instant::now() + Duration::from_secs(5);
        let reader = loop {
            assert!(
                Instant::now() < deadline,
                "repeat fixture metadata never completed"
            );
            if let Ok(nodes) = output.event_nodes() {
                assert!(nodes.len() <= 1);
                if let Some(path) = nodes.first() {
                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
                        .open(path);
                    if let Ok(file) = file {
                        match crate::linux::DevicePublication::read(file.as_fd()) {
                            Ok(receipt)
                                if processed_publication(&receipt, output.descriptor())
                                    .unwrap() =>
                            {
                                let mut raw = RawDevice::from_fd(file.into()).unwrap();
                                assert_eq!(raw.name(), Some(output.descriptor().name.as_str()));
                                assert_eq!(raw.physical_path(), Some(output.marker()));
                                let id = raw.input_id();
                                let expected = output.descriptor().identity;
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
                                    raw.supported_events()
                                        .iter()
                                        .map(|kind| kind.0)
                                        .collect::<Vec<_>>(),
                                    [0, 1, 20]
                                );
                                assert_eq!(
                                    raw.supported_keys()
                                        .unwrap()
                                        .iter()
                                        .map(|key| key.0)
                                        .collect::<Vec<_>>(),
                                    [30]
                                );
                                assert!(raw.properties().iter().next().is_none());
                                assert!(raw.get_key_state().unwrap().iter().next().is_none());
                                if receipt.current(raw.as_fd(), path)
                                    && output.event_nodes().unwrap() == nodes
                                {
                                    output.publication.verify().unwrap();
                                    raw.grab().unwrap();
                                    break raw;
                                }
                            }
                            Ok(_) => {}
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => panic!("repeat fixture publication failed: {error}"),
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(5));
        };
        Self { output, reader }
    }

    fn write(&mut self, events: &[RawEvent]) {
        self.output
            .file
            .write_all(&encode_events(events).unwrap())
            .unwrap();
    }

    fn key(&mut self, value: i32) {
        self.write(&[
            RawEvent {
                kind: EV_KEY,
                code: 30,
                value,
            },
            RawEvent {
                kind: EV_SYN,
                code: 0,
                value: 0,
            },
        ]);
    }

    fn collect(&mut self, duration: Duration) -> Vec<RawEvent> {
        let deadline = Instant::now() + duration;
        let mut received = Vec::new();
        for _ in 0..512 {
            match self.reader.fetch_events() {
                Ok(events) => {
                    for event in events {
                        assert!(
                            received.len() < 256,
                            "repeat fixture receive budget exceeded"
                        );
                        assert!(
                            !(event.event_type().0 == EV_SYN
                                && event.code() == crate::input::SYN_DROPPED)
                        );
                        received.push(RawEvent {
                            kind: event.event_type().0,
                            code: event.code(),
                            value: event.value(),
                        });
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => panic!("repeat fixture read failed: {error}"),
            }
            if Instant::now() >= deadline {
                return received;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("repeat fixture syscall budget exceeded");
    }

    fn settings(&self) -> [u32; 2] {
        let mut values = [0; 2];
        // SAFETY: read-only repeat ioctl on this test's retained evdev client.
        unsafe { get_repeat(self.reader.as_raw_fd(), &mut values) }.unwrap();
        values
    }

    fn feedback(&mut self) -> Vec<RawEvent> {
        let mut received = Vec::new();
        for _ in 0..64 {
            let mut bytes = [0; EVENT_SIZE * 32];
            match self.output.file.read(&mut bytes) {
                Ok(0) => panic!("repeat fixture producer closed"),
                Ok(length) => received.extend(decode_events(&bytes[..length]).unwrap()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return received,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => panic!("repeat fixture feedback failed: {error}"),
            }
            assert!(received.len() <= 64);
        }
        panic!("repeat fixture feedback budget exceeded");
    }

    fn configure(&mut self, values: [u32; 2]) -> Vec<RawEvent> {
        // SAFETY: settings affect only this test's verified, exclusively grabbed keyboard.
        unsafe { set_repeat(self.reader.as_raw_fd(), &values) }.unwrap();
        assert_eq!(self.settings(), values);
        self.write(&[RawEvent {
            kind: EV_SYN,
            code: 0,
            value: 0,
        }]);
        assert!(
            self.collect(Duration::from_millis(10))
                .iter()
                .all(|event| event.kind != EV_KEY)
        );
        self.feedback()
    }
}

#[test]
#[ignore = "requires user-launched isolated root routing runner"]
fn isolated_uinput_repeat_timer_and_consumer_settings_are_not_transparent() {
    assert_eq!(std::env::var("PREK_ISOLATED_ROUTING").as_deref(), Ok("1"));
    // SAFETY: read-only query of this test process's credentials.
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let registry = Registry::system().unwrap();
    let mut device = crate::transport::tests::touch();
    device.identity = "test:repeat".into();
    device.name = "Prek test repeat".into();
    device.physical = "fixture/repeat".into();
    device.path.clear();
    device.axes.clear();
    device.properties.clear();
    device.capabilities = BTreeMap::from([(0, vec![0]), (1, vec![30]), (20, vec![0, 1])]);
    let spec = Descriptor::mirror(
        &device,
        KernelIdentity {
            bus: 3,
            vendor: device.vendor,
            product: device.product,
            version: 1,
        },
        Some(crate::transport::Repeat {
            delay_ms: 80,
            period_ms: 20,
        }),
        &BTreeMap::new(),
        BTreeMap::new(),
    )
    .unwrap();
    let mut test = Experiment::create(registry.prepare_isolated(spec).unwrap());
    let initial = test.settings();
    println!("\nrepeat fixture: registration settings {initial:?}");
    assert_eq!(
        initial,
        [250, 33],
        "uinput registration no longer has the expected kernel defaults"
    );
    test.feedback();
    assert_eq!(
        test.configure([80, 0]),
        [
            RawEvent {
                kind: 20,
                code: 0,
                value: 80
            },
            RawEvent {
                kind: 20,
                code: 1,
                value: 0
            }
        ]
    );
    test.key(1);
    test.key(2);
    let keys = |events: Vec<RawEvent>| {
        events
            .into_iter()
            .filter(|event| event.kind == EV_KEY)
            .map(|event| (event.code, event.value))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        keys(test.collect(Duration::from_millis(160))),
        [(30, 1), (30, 2)]
    );
    println!("repeat fixture: disabled timer preserved exactly one emitted repeat");
    assert!(test.feedback().is_empty());
    test.key(0);
    assert_eq!(keys(test.collect(Duration::from_millis(10))), [(30, 0)]);

    let requests = test.configure([60, 20]);
    assert_eq!(
        requests,
        [
            RawEvent {
                kind: 20,
                code: 0,
                value: 60
            },
            RawEvent {
                kind: 20,
                code: 1,
                value: 20
            }
        ]
    );
    test.key(1);
    test.key(2);
    let received = keys(test.collect(Duration::from_millis(240)));
    assert_eq!(received.first(), Some(&(30, 1)));
    assert!(received[1..].iter().all(|event| *event == (30, 2)));
    let repeats = received.len() - 1;
    println!("repeat fixture: re-enabled timer observed {repeats} repeats after one emission");
    assert!(
        repeats > 1,
        "consumer restored nonzero settings but no extra timer repeat arrived"
    );
    assert!(test.feedback().is_empty());
    test.key(0);
    let release = keys(test.collect(Duration::from_millis(20)));
    assert_eq!(release.last(), Some(&(30, 0)));
    assert!(
        release[..release.len() - 1]
            .iter()
            .all(|event| *event == (30, 2))
    );
    assert!(test.reader.get_key_state().unwrap().iter().next().is_none());
    println!(
        "\nPREK_UINPUT_REPEAT_RESULT={}",
        serde_json::json!({
            "version": 1, "initial": initial, "disabled_forwarded": 1, "disabled_observed": 1,
            "enabled_forwarded": 1, "enabled_observed": repeats, "consumer_feedback_codes": [0, 1],
            "keys_neutral": true, "physical_devices_opened": 0, "repeat_passthrough_tested": false
        })
    );
}
