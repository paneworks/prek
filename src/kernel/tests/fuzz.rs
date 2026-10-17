use super::*;
use crate::input::{ABS_MT_POSITION_X, ABS_MT_SLOT, ABS_MT_TRACKING_ID, EV_ABS};
use std::{
    thread,
    time::{Duration, Instant},
};

nix::ioctl_read_buf!(get_slots, b'E', 0x0a, libc::c_int);

fn filtered(raw: i32, old: i32, fuzz: i32) -> i32 {
    assert!((-4096..=4096).contains(&raw));
    assert!((-4096..=4096).contains(&old));
    assert!((0..=64).contains(&fuzz));
    let distance = (raw - old).abs();
    if fuzz == 0 || distance >= fuzz * 2 {
        raw
    } else if distance < fuzz / 2 {
        old
    } else if distance < fuzz {
        (old * 3 + raw) / 4
    } else {
        (old + raw) / 2
    }
}

#[test]
fn observed_coordinates_are_not_raw_driver_coordinates() {
    let first = filtered(16, 0, 16);
    let second = filtered(24, first, 16);
    assert_eq!([first, second], [8, 16]);
    assert_eq!(filtered(first, 0, 16), 2);
    assert_eq!(filtered(-16, 0, 16), -8);
    assert_eq!(filtered(-24, -8, 16), -16);
}

#[test]
fn a_valid_source_snapshot_can_be_unreachable_in_one_mirrored_slot_write() {
    let source = filtered(24, filtered(16, 0, 16), 16);
    assert_eq!(source, 16);
    for raw in -4096..=4096 {
        assert_ne!(filtered(raw, 0, 16), source);
    }
    assert_eq!(filtered(31, 0, 16), 15);
    assert_eq!(filtered(32, 0, 16), 32);
    assert_eq!(filtered(source, 8, 16), 10);
}

#[test]
fn old_slot_values_and_signed_truncation_cannot_be_ignored_by_an_inverse() {
    assert_eq!(filtered(24, 8, 16), 16);
    assert_eq!(filtered(24, 0, 16), 12);
    assert_eq!(filtered(-9, 0, 16), -2);
    assert_eq!(filtered(9, 0, 16), 2);
    assert_eq!(filtered(-7, 0, 16), 0);
    assert_eq!(filtered(7, 0, 16), 0);
}

#[test]
fn clearing_fuzz_changes_the_declared_device_instead_of_preserving_parity() {
    let build = |fuzz| {
        let mut device = crate::transport::tests::touch();
        for axis in device.axes.values_mut() {
            axis.fuzz = 0;
        }
        device.axes.get_mut(&ABS_MT_POSITION_X).unwrap().fuzz = fuzz;
        Descriptor::mirror(
            &device,
            descriptor().identity,
            None,
            &BTreeMap::new(),
            BTreeMap::new(),
        )
        .unwrap()
    };
    let spec = build(16);
    assert_eq!(
        supported_backend(&spec).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    let changed = build(0);
    assert_ne!(spec.axes, changed.axes);
    assert!(supported_backend(&changed).is_ok());
    assert_eq!(filtered(16, 0, 0), 16);
    assert_ne!(filtered(16, 0, 16), 16);
}

struct Experiment {
    output: Virtual,
    reader: RawDevice,
}

impl Experiment {
    fn create(registry: &Registry, id: u64, fuzz: i32) -> Self {
        assert_eq!(std::env::var("PREK_ISOLATED_ROUTING").as_deref(), Ok("1"));
        // SAFETY: read-only query of the test process's credentials.
        assert_eq!(unsafe { libc::geteuid() }, 0);
        assert!(matches!(fuzz, 0 | 16));
        let mut device = crate::transport::tests::touch();
        device.identity = format!("test:fuzz-{id}");
        device.name = format!("Prek test fuzz {id}");
        device.physical = format!("fixture/fuzz-{id}");
        device.path.clear();
        device.source.device = id;
        for axis in device.axes.values_mut() {
            axis.fuzz = 0;
        }
        device.axes.get_mut(&ABS_MT_SLOT).unwrap().maximum = 1;
        device.axes.get_mut(&ABS_MT_POSITION_X).unwrap().fuzz = fuzz;
        let spec = Descriptor::mirror(
            &device,
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
        .unwrap();
        let publication = registry.prepare_isolated(spec).unwrap();
        let output = if fuzz == 0 {
            Virtual::create(publication).unwrap()
        } else {
            Self::filtered_output(publication)
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let reader = loop {
            assert!(
                Instant::now() < deadline,
                "fuzz fixture metadata never completed"
            );
            if output.processed_metadata().unwrap() {
                let nodes = output.event_nodes().unwrap();
                assert_eq!(nodes.len(), 1);
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
                    .open(&nodes[0])
                    .unwrap();
                let receipt = crate::linux::DevicePublication::read(file.as_fd()).unwrap();
                assert!(processed_publication(&receipt, output.descriptor()).unwrap());
                assert!(receipt.current(file.as_fd(), &nodes[0]));
                let mut reader = RawDevice::from_fd(file.into()).unwrap();
                assert_eq!(reader.name(), Some(output.descriptor().name.as_str()));
                assert_eq!(reader.physical_path(), Some(output.marker()));
                assert_eq!(output.event_nodes().unwrap(), nodes);
                output.publication.verify().unwrap();
                reader.grab().unwrap();
                break reader;
            }
            thread::sleep(Duration::from_millis(5));
        };
        let result = Self { output, reader };
        assert_eq!(
            result.reader.get_abs_state().unwrap()[usize::from(ABS_MT_POSITION_X)].fuzz,
            fuzz
        );
        assert_eq!(result.slots(ABS_MT_POSITION_X), [0, 0]);
        result.neutral();
        result
    }

    fn filtered_output(publication: Publication) -> Virtual {
        publication.verify().unwrap();
        assert!(publication.descriptor.is_isolated());
        assert_eq!(
            supported_backend(&publication.descriptor)
                .unwrap_err()
                .kind(),
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
        assert_eq!(
            spec.capabilities.keys().copied().collect::<Vec<_>>(),
            [0, 1, 3]
        );
        let fd = output.file.as_raw_fd();
        let marker = CString::new(spec.marker.clone()).unwrap();
        // SAFETY: the setup structure contains only integer fields.
        let mut setup: libc::uinput_setup = unsafe { std::mem::zeroed() };
        setup.id = libc::input_id {
            bustype: spec.identity.bus,
            vendor: spec.identity.vendor,
            product: spec.identity.product,
            version: spec.identity.version,
        };
        assert!(spec.name.len() < setup.name.len());
        for (slot, byte) in setup.name.iter_mut().zip(spec.name.bytes()) {
            *slot = byte as libc::c_char;
        }
        // SAFETY: validated isolated test capabilities on this owned uinput fd.
        unsafe {
            for &kind in spec.capabilities.keys() {
                event_bit(fd, kind.into()).unwrap();
            }
            for &key in &spec.capabilities[&1] {
                key_bit(fd, key.into()).unwrap();
            }
            for &property in &spec.properties {
                property_bit(fd, property.into()).unwrap();
            }
            for (&code, axis) in &spec.axes {
                absolute_bit(fd, code.into()).unwrap();
                let mut value: libc::uinput_abs_setup = std::mem::zeroed();
                value.code = code;
                value.absinfo.minimum = axis.minimum;
                value.absinfo.maximum = axis.maximum;
                value.absinfo.fuzz = axis.fuzz;
                value.absinfo.flat = axis.flat;
                value.absinfo.resolution = axis.resolution;
                absolute_setup(fd, &value).unwrap();
            }
            physical_path(fd, marker.as_ptr()).unwrap();
            device_setup(fd, &setup).unwrap();
            create_device(fd).unwrap();
        }
        output.created = true;
        output
    }

    fn slots(&self, code: u16) -> [i32; 2] {
        let mut values = [i32::from(code), 0, 0];
        // SAFETY: one axis code followed by the two declared slot values.
        unsafe { get_slots(self.reader.as_raw_fd(), &mut values) }.unwrap();
        [values[1], values[2]]
    }

    fn neutral(&self) {
        assert_eq!(self.slots(ABS_MT_TRACKING_ID), [-1, -1]);
        assert!(self.reader.get_key_state().unwrap().iter().next().is_none());
    }

    fn write(&mut self, raw: i32, expected: i32) -> i32 {
        self.output
            .file
            .write_all(
                &encode_events(&[
                    RawEvent {
                        kind: EV_ABS,
                        code: ABS_MT_POSITION_X,
                        value: raw,
                    },
                    RawEvent {
                        kind: EV_SYN,
                        code: SYN_REPORT,
                        value: 0,
                    },
                ])
                .unwrap(),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut received = Vec::new();
        for _ in 0..512 {
            match self.reader.fetch_events() {
                Ok(events) => received.extend(events.map(|event| RawEvent {
                    kind: event.event_type().0,
                    code: event.code(),
                    value: event.value(),
                })),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => panic!("fuzz fixture read failed: {error}"),
            }
            assert!(received.len() <= 32 && Instant::now() < deadline);
            if received
                .last()
                .is_some_and(|event| event.kind == EV_SYN && event.code == SYN_REPORT)
            {
                assert_eq!(
                    received
                        .iter()
                        .filter(|event| event.kind == EV_ABS)
                        .map(|event| (event.code, event.value))
                        .collect::<Vec<_>>(),
                    [(ABS_MT_POSITION_X, expected)]
                );
                assert_eq!(received.len(), 2);
                assert_eq!(self.slots(ABS_MT_POSITION_X), [expected, 0]);
                self.neutral();
                return expected;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("fuzz fixture receive budget exhausted");
    }
}

#[test]
#[ignore = "requires user-launched isolated root routing runner"]
fn isolated_uinput_fuzz_refiltering_and_slot_restore_are_not_transparent() {
    assert_eq!(std::env::var("PREK_ISOLATED_ROUTING").as_deref(), Ok("1"));
    // SAFETY: read-only query of the test process's credentials.
    assert_eq!(unsafe { libc::geteuid() }, 0);
    let registry = Registry::system().unwrap();
    let mut source = Experiment::create(&registry, 1, 16);
    let first = source.write(16, 8);
    let second = source.write(24, 16);
    let mut mirror = Experiment::create(&registry, 2, 16);
    let twice = mirror.write(first, 2);
    let mut restore = Experiment::create(&registry, 3, 16);
    let restored = restore.write(second, 8);
    let lower = Experiment::create(&registry, 4, 16).write(31, 15);
    let upper = Experiment::create(&registry, 5, 16).write(32, 32);
    let unchanged = Experiment::create(&registry, 6, 0).write(second, 16);
    println!(
        "\nPREK_UINPUT_FUZZ_RESULT={}",
        serde_json::json!({
            "version":1, "fuzz":16, "source_raw":[16,24], "source_observed":[first,second],
            "mirror_raw":first, "mirror_observed":twice, "restore_raw":second,
            "restore_observed":restored, "gap_raw":[31,32], "gap_observed":[lower,upper],
            "zero_fuzz_observed":unchanged, "inactive_slot_preserved":true, "contacts_neutral":true,
            "physical_devices_opened":0, "fuzz_passthrough_tested":false
        })
    );
}
