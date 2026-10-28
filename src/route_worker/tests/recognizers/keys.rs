use super::*;
use crate::keys::output::Sink;
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt};

mod producers;

#[test]
fn generated_incarnations_have_distinct_consumer_names() {
    let mut fixture = Fixture::new(profile(), super::owner());
    let one =
        Descriptor::generated_keyboard(fixture.stream.next_key_source("seat0").unwrap()).unwrap();
    let two =
        Descriptor::generated_keyboard(fixture.stream.next_key_source("seat1").unwrap()).unwrap();
    assert_ne!(one.name, two.name);
    fixture.stream.key_generation = u64::MAX;
    assert!(fixture.stream.next_key_source("seat0").is_err());
}

#[test]
fn a_fatal_generated_cleanup_error_stays_latched() {
    let mut fixture = Fixture::new(profile(), super::owner());
    fixture.stream.key_failure = Some(io::Error::new(
        io::ErrorKind::TimedOut,
        "modeled cleanup failure",
    ));
    for _ in 0..2 {
        assert_eq!(
            fixture
                .stream
                .refresh_keys(monotonic().unwrap())
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }
}

#[test]
fn the_native_adapter_fixture_rejects_a_socket_instead_of_a_regular_file() {
    let fixture = Fixture::new(profile(), super::owner());
    let descriptor = Descriptor::generated_keyboard(Source {
        device: 0,
        generation: 1,
        seat: "seat0".into(),
    })
    .unwrap();
    let publication = fixture
        .stream
        .registry
        .prepare_generated_isolated(descriptor)
        .unwrap();
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let file = std::fs::File::from(std::os::fd::OwnedFd::from(socket));
    assert!(Virtual::ordinary_file_fixture(publication, file).is_err());
    assert_eq!(fs::read_dir(&fixture.directory).unwrap().count(), 0);
}

fn profile() -> Config {
    let mut config = super::profile();
    let rule = &mut config.rules[0];
    rule.id = "shift".into();
    rule.trigger = Trigger::Key {
        key: crate::config::Key(30),
    };
    rule.action = Action::Remap {
        key: crate::config::Key(42),
    };
    config
}
fn owner() -> Subscription {
    let mut owner = super::owner();
    owner.capabilities.insert(Capability::SyntheticInput);
    owner
}
fn slot(fixture: &Fixture) -> (crate::route_worker::keys::Slot, Descriptor, PathBuf) {
    let descriptor = Descriptor::generated_keyboard(Source {
        device: 0,
        generation: 1,
        seat: "seat0".into(),
    })
    .unwrap();
    let publication = fixture
        .stream
        .registry
        .prepare_generated_isolated(descriptor)
        .unwrap();
    let path = fixture.directory.join("generated-bytes");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    let device = Virtual::ordinary_file_fixture(publication, file).unwrap();
    let descriptor = device.descriptor().clone();
    assert!(Sink::supports_key(&device, 42));
    (
        crate::route_worker::keys::Slot::ordinary_file_fixture(
            fixture.owner.fence,
            fixture.owner.consumer.clone().unwrap(),
            device,
            monotonic().unwrap(),
        )
        .unwrap(),
        descriptor,
        path,
    )
}
fn ack(fixture: &mut Fixture) {
    let routing = &mut fixture.stream.seats.get_mut("seat0").unwrap().routing;
    while let Some(scheduled) = routing.due(&device().source, monotonic().unwrap()).unwrap() {
        let ticket = scheduled.ticket;
        routing
            .delivered(&device().source, ticket, monotonic().unwrap())
            .unwrap();
    }
}
fn events(path: &std::path::Path) -> Vec<(u16, i32)> {
    let bytes = fs::read(path).unwrap();
    assert_eq!(
        bytes.len() % std::mem::size_of::<nix::libc::input_event>(),
        0
    );
    bytes
        .chunks_exact(std::mem::size_of::<nix::libc::input_event>())
        .filter_map(|bytes| {
            let kind = u16::from_ne_bytes(
                bytes[std::mem::offset_of!(nix::libc::input_event, type_)..][..2]
                    .try_into()
                    .unwrap(),
            );
            let code = u16::from_ne_bytes(
                bytes[std::mem::offset_of!(nix::libc::input_event, code)..][..2]
                    .try_into()
                    .unwrap(),
            );
            let value = i32::from_ne_bytes(
                bytes[std::mem::offset_of!(nix::libc::input_event, value)..][..4]
                    .try_into()
                    .unwrap(),
            );
            (kind == EV_KEY).then_some((code, value))
        })
        .collect()
}

fn physical_file(fixture: &mut Fixture, original: &Device, name: &str) -> PathBuf {
    let descriptor = Descriptor::mirror(
        original,
        KernelIdentity {
            bus: 3,
            vendor: 1,
            product: 1,
            version: 1,
        },
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    let publication = fixture
        .stream
        .registry
        .prepare_isolated(descriptor)
        .unwrap();
    let path = fixture.directory.join(name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    let virtual_device = Virtual::ordinary_file_fixture(publication, file).unwrap();
    let now = monotonic().unwrap();
    fixture.stream.outputs.insert(
        original.source.clone(),
        Output {
            fence: fixture.owner.fence,
            virtual_device,
            state_probe: None,
            feedback: None,
            readiness: None,
            consumer: None,
            offered: now,
            started: now,
            in_flight: None,
            queued: None,
            sequence: 0,
        },
    );
    path
}

fn raw_frame(device: &Device, sequence: u64, data: &[(u16, i32)]) -> Frame {
    let mut events: Vec<_> = data
        .iter()
        .map(|&(code, value)| RawEvent {
            kind: EV_KEY,
            code,
            value,
        })
        .collect();
    events.push(RawEvent {
        kind: EV_SYN,
        code: SYN_REPORT,
        value: 0,
    });
    Frame {
        source: device.source.clone(),
        at: monotonic().unwrap(),
        sequence,
        events,
    }
}

#[test]
fn sole_worker_key_output_waits_only_for_the_explicitly_related_pointer() {
    for required in [false, true] {
        let mut pointer = device();
        pointer.source.device += 1;
        pointer.identity = "test:independent-pointer".into();
        pointer.capabilities = BTreeMap::from([
            (EV_SYN, vec![SYN_REPORT]),
            (EV_KEY, vec![272]),
            (crate::input::EV_REL, vec![0, 1]),
        ]);
        let mut config = profile();
        config.rules[0].delivery = Delivery::Buffer;
        config.devices.insert(
            "pointer".into(),
            Selector {
                identity: Some(pointer.identity.clone()),
                ..Default::default()
            },
        );
        config.policy.allowed_devices.push("pointer".into());
        let mut hold = config.rules[0].clone();
        hold.id = "pointer".into();
        hold.priority = 10;
        hold.devices = vec!["pointer".into()];
        hold.trigger = Trigger::ButtonHold {
            key: crate::config::Key(272),
            after: Millis(100),
            repeat: None,
        };
        hold.action = Action::Event {
            name: "pointer".into(),
        };
        if required {
            hold.require_keys = vec![crate::config::Key(48)];
        }
        config.rules.push(hold);
        let mut fixture = Fixture::new(config, owner());
        fixture
            .stream
            .push(Record::Device {
                device: pointer.clone(),
                keys: BTreeSet::new(),
                contacts: vec![],
            })
            .unwrap();
        fixture.attach_device(&pointer);
        let (slot, _, generated) = slot(&fixture);
        fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
        fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
        physical_file(&mut fixture, &device(), "keyboard-ordering");
        let raw = physical_file(&mut fixture, &pointer, "pointer-ordering");
        if required {
            fixture
                .stream
                .push(Record::Frame {
                    frame: raw_frame(&device(), 1, &[(48, 1)]),
                })
                .unwrap();
            for _ in 0..4 {
                fixture.stream.flush().unwrap();
            }
        }
        fixture
            .stream
            .push(Record::Frame {
                frame: raw_frame(&pointer, 1, &[(272, 1)]),
            })
            .unwrap();
        assert!(events(&raw).is_empty());
        fixture
            .stream
            .push(Record::Frame {
                frame: raw_frame(&device(), if required { 2 } else { 1 }, &[(30, 1)]),
            })
            .unwrap();
        for _ in 0..4 {
            fixture.stream.flush().unwrap();
        }
        assert_eq!(
            events(&generated),
            if required { vec![] } else { vec![(42, 1)] },
            "required={required}"
        );
        assert!(events(&raw).is_empty());
    }
}

#[test]
fn sole_worker_interleaves_mixed_input_and_generated_reports_on_owned_regular_files() {
    let mut config = profile();
    config.rules[0].delivery = crate::config::Delivery::Buffer;
    let mut fixture = Fixture::new(config, owner());
    let (slot, _, generated) = slot(&fixture);
    fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    let physical = physical_file(&mut fixture, &device(), "physical-ordered");
    fixture
        .stream
        .push(Record::Frame {
            frame: raw_frame(&device(), 1, &[(30, 1), (30, 2), (48, 1), (30, 0), (48, 0)]),
        })
        .unwrap();
    for _ in 0..12 {
        fixture.stream.flush().unwrap();
        let keys = events(&generated);
        let raw = events(&physical);
        if raw.contains(&(48, 1)) {
            assert!(keys.starts_with(&[(42, 1), (42, 2)]));
        }
        if keys.contains(&(42, 0)) {
            assert!(raw.starts_with(&[(48, 1)]));
        }
        if raw.contains(&(48, 0)) {
            assert_eq!(keys, [(42, 1), (42, 2), (42, 0)]);
        }
    }
    assert_eq!(events(&generated), [(42, 1), (42, 2), (42, 0)]);
    assert_eq!(events(&physical), [(48, 1), (48, 0)]);
}

#[test]
fn a_forwarded_frame_records_its_delay_for_the_taken_over_device() {
    let mut fixture = Fixture::new(profile(), owner());
    let physical = physical_file(&mut fixture, &device(), "physical-delays");
    let source = device().source;
    fixture.stream.forwarding.insert(
        source.clone(),
        crate::route_worker::latency::Forwarding::new("Delays".into(), Duration::ZERO),
    );
    fixture
        .stream
        .push(Record::Frame {
            frame: raw_frame(&device(), 1, &[(48, 1)]),
        })
        .unwrap();
    for _ in 0..4 {
        fixture.stream.flush().unwrap();
    }
    assert_eq!(events(&physical), [(48, 1)]);
    let summary = fixture.stream.forwarding[&source].summary();
    assert!(
        summary.starts_with("routing worker: Delays forwarded 1 frames; Prek overhead median ≤ "),
        "{summary}"
    );
    fixture.stream.outputs.clear();
    fixture.stream.settle_forwarding();
    assert!(fixture.stream.forwarding.is_empty());
}

#[test]
fn sole_worker_pending_modifier_fences_an_unrelated_physical_source_file() {
    let mut config = profile();
    config.rules[0].delivery = crate::config::Delivery::Buffer;
    config.devices.get_mut("keyboard").unwrap().identity = None;
    config.devices.get_mut("keyboard").unwrap().keys = vec![crate::config::Key(30)];
    let mut fixture = Fixture::new(config, owner());
    let (slot, _, generated) = slot(&fixture);
    fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    let mut second = device();
    second.source.device += 1;
    second.identity.push_str("-second");
    fixture
        .stream
        .push(Record::Device {
            device: second.clone(),
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    fixture.attach_device(&second);
    let physical = physical_file(&mut fixture, &second, "physical-second");
    fixture
        .stream
        .push(Record::Frame {
            frame: raw_frame(&device(), 1, &[(30, 1)]),
        })
        .unwrap();
    fixture
        .stream
        .push(Record::Frame {
            frame: raw_frame(&second, 1, &[(48, 1), (48, 0)]),
        })
        .unwrap();
    for _ in 0..8 {
        fixture.stream.flush().unwrap();
        if !events(&physical).is_empty() {
            assert_eq!(events(&generated), [(42, 1)]);
        }
    }
    assert_eq!(events(&physical), [(48, 1), (48, 0)]);
}

#[test]
fn blocked_raw_reports_wait_for_progress_or_the_write_budget_not_an_expired_due_time() {
    let mut config = profile();
    config.rules[0].delivery = crate::config::Delivery::Buffer;
    let mut fixture = Fixture::new(config, owner());
    let (slot, _, _) = slot(&fixture);
    fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    physical_file(&mut fixture, &device(), "physical-deadline");
    let input = raw_frame(&device(), 1, &[(30, 1), (48, 1), (48, 0)]);
    let at = input.at;
    fixture.stream.push(Record::Frame { frame: input }).unwrap();
    assert!(fixture.stream.deadline().unwrap() > at);
}

#[test]
fn a_generated_keyboard_ready_before_any_source_keeps_its_key_lane() {
    let mut owner = owner();
    owner.capabilities = BTreeSet::from([
        Capability::Context,
        Capability::Observations,
        Capability::SyntheticInput,
    ]);
    let mut fixture = Fixture::subscribed(profile(), owner);
    let (slot, _, _) = slot(&fixture);
    fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
    for _ in 0..2 {
        fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    }
    let now = monotonic().unwrap();
    let seat = &fixture.stream.seats["seat0"];
    assert!(seat.keys.as_ref().unwrap().available(now));
    assert!(seat.routing.key_lane(fixture.owner.fence, now));
}

#[test]
fn subscription_permission_is_not_execution_availability_without_a_ready_output() {
    let mut fixture = Fixture::new(profile(), owner());
    let seat = &fixture.stream.seats["seat0"];
    assert!(
        seat.subscription
            .capabilities
            .contains(&Capability::SyntheticInput)
    );
    assert!(
        !seat
            .routing
            .key_lane(fixture.owner.fence, monotonic().unwrap())
    );
    assert!(seat.keys.is_none());
    assert!(fixture.stream.service_owned(None).is_err());
    assert_eq!(fs::read_dir(&fixture.directory).unwrap().count(), 0);
}

#[test]
fn sole_worker_collect_executes_original_key_work_through_the_native_adapter_on_a_private_file() {
    let mut fixture = Fixture::new(profile(), owner());
    let (slot, _descriptor, path) = slot(&fixture);
    fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    fixture.key(1, true);
    ack(&mut fixture);
    for _ in 0..3 {
        fixture.stream.flush().unwrap();
    }
    assert_eq!(events(&path), [(42, 1)]);
    fixture.key(2, false);
    ack(&mut fixture);
    for _ in 0..3 {
        fixture.stream.flush().unwrap();
    }
    assert_eq!(events(&path), [(42, 1), (42, 0)]);
    assert!(!fixture.drain().iter().any(|message| matches!(message,Message::Outcome { report: Report::Dispatch { event,.. } } if matches!(&**event,crate::rules::Event::Matched { action: Action::Remap { .. },.. }))));
}

#[test]
fn private_publication_reply_is_demultiplexed_without_physical_source_aliases() {
    let mut fixture = Fixture::new(profile(), owner());
    let (slot, descriptor, _path) = slot(&fixture);
    fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
    let evidence = ConsumerEvidence {
        consumer: fixture.owner.consumer.clone().unwrap(),
        marker: descriptor.marker.clone(),
        name: descriptor.name.clone(),
        class: descriptor.class,
        seat: "seat0".into(),
        at: monotonic().unwrap(),
    };
    let source = descriptor.source.clone();
    assert!(
        fixture
            .parent
            .send(
                &Command::Transport {
                    fence: fixture.owner.fence,
                    source: source.clone(),
                    marker: "old-publication".into(),
                    evidence,
                },
                monotonic().unwrap()
            )
            .unwrap()
    );
    fixture.stream.service().unwrap();
    assert!(
        fixture.stream.seats["seat0"]
            .keys
            .as_ref()
            .unwrap()
            .available(monotonic().unwrap())
    );
}

#[test]
fn a_late_generated_reply_cannot_mutate_a_physical_output_with_equal_source_numbers() {
    let mut fixture = Fixture::new(profile(), super::owner());
    let mut original = crate::transport::tests::touch();
    original.identity = "test:role-alias".into();
    original.name = "Prek test role alias".into();
    original.physical = "fixture/role-alias".into();
    original.path.clear();
    original.source = Source {
        device: 0,
        generation: 1,
        seat: "seat0".into(),
    };
    let descriptor = Descriptor::mirror(
        &original,
        KernelIdentity {
            bus: 3,
            vendor: 1,
            product: 2,
            version: 1,
        },
        None,
        &BTreeMap::new(),
    )
    .unwrap();
    let publication = fixture
        .stream
        .registry
        .prepare_isolated(descriptor)
        .unwrap();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(fixture.directory.join("physical-bytes"))
        .unwrap();
    let virtual_device = Virtual::ordinary_file_fixture(publication, file).unwrap();
    let physical = virtual_device.descriptor().clone();
    let source = physical.source.clone();
    let now = monotonic().unwrap();
    fixture.stream.outputs.insert(
        source.clone(),
        Output {
            fence: fixture.owner.fence,
            virtual_device,
            state_probe: None,
            feedback: None,
            readiness: None,
            consumer: None,
            offered: now,
            started: now,
            in_flight: None,
            queued: None,
            sequence: 0,
        },
    );
    let generated = Descriptor::generated_keyboard(source.clone())
        .unwrap()
        .for_isolated()
        .unwrap()
        .bind_publication(Epoch([7, 8]))
        .unwrap();
    for descriptor in [&generated, &physical] {
        let evidence = ConsumerEvidence {
            consumer: fixture.owner.consumer.clone().unwrap(),
            marker: descriptor.marker.clone(),
            name: descriptor.name.clone(),
            class: descriptor.class,
            seat: "seat0".into(),
            at: monotonic().unwrap(),
        };
        assert!(
            fixture
                .parent
                .send(
                    &Command::Transport {
                        fence: fixture.owner.fence,
                        source: source.clone(),
                        marker: descriptor.marker.clone(),
                        evidence,
                    },
                    monotonic().unwrap()
                )
                .unwrap()
        );
        fixture.stream.service().unwrap();
        assert_eq!(
            fixture.stream.outputs[&source].consumer.is_some(),
            !descriptor.is_generated_keyboard()
        );
    }
}

#[test]
fn teardown_drains_generated_releases_before_dropping_its_publication() {
    let mut fixture = Fixture::new(profile(), owner());
    let (slot, descriptor, path) = slot(&fixture);
    let raw = physical_file(&mut fixture, &device(), "raw-bytes");
    fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    fixture.key(1, true);
    for _ in 0..3 {
        fixture.stream.flush().unwrap();
    }
    fixture
        .stream
        .push(Record::Frame {
            frame: raw_frame(&device(), 2, &[(48, 1)]),
        })
        .unwrap();
    for _ in 0..3 {
        fixture.stream.flush().unwrap();
    }
    assert_eq!(events(&raw), [(30, 1), (48, 1)]);
    fixture.stream.teardown("seat0", monotonic().unwrap());
    assert!(fixture.stream.seats.is_empty());
    assert_eq!(fixture.stream.closing_keys.len(), 1);
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    assert_eq!(fixture.stream.closing_keys.len(), 1);
    assert_eq!(fixture.stream.closing_outputs.len(), 1);
    assert!(fixture.stream.key_drain_pending("seat0"));
    assert!(fixture.stream.reader_released(&device().source).is_err());
    assert_eq!(events(&path), [(42, 1), (42, 0)]);
    assert_eq!(events(&raw), [(30, 1), (48, 1)]);
    let mut requests: Vec<_> = fixture
        .drain()
        .into_iter()
        .filter_map(|message| match message {
            Message::KeyDrain { request } => Some(request),
            _ => None,
        })
        .collect();
    assert_eq!(requests.len(), 1);
    let request = requests.pop().unwrap();
    assert_eq!(request.marker, descriptor.marker);
    let mut wrong = request.clone();
    wrong.nonce.0[0] ^= 1;
    fixture.send(Command::KeyDrained {
        request: wrong,
        at: monotonic().unwrap(),
    });
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    assert_eq!(fixture.stream.closing_keys.len(), 1);
    assert_eq!(events(&raw), [(30, 1), (48, 1)]);
    fixture.send(Command::KeyDrained {
        request: request.clone(),
        at: monotonic().unwrap(),
    });
    assert!(fixture.stream.key_drain_pending("seat0"));
    assert!(fixture.stream.reader_released(&device().source).is_err());
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    assert!(fixture.stream.closing_keys.is_empty());
    assert!(fixture.stream.closing_outputs.is_empty());
    assert!(!fixture.stream.key_drain_pending("seat0"));
    assert_eq!(events(&raw), [(30, 1), (48, 1), (30, 0), (48, 0)]);
    fixture.send(Command::KeyDrained {
        request,
        at: monotonic().unwrap(),
    });
    assert!(
        fs::read_dir(&fixture.directory)
            .unwrap()
            .all(|entry| matches!(
                entry.unwrap().file_name().to_str(),
                Some("generated-bytes" | "raw-bytes")
            ))
    );
    assert!(descriptor.is_generated_keyboard());
}

#[test]
fn input_held_through_a_revoked_generated_drain_is_delivered_after_it() {
    for queued in [false, true] {
        let mut fixture = Fixture::new(profile(), owner());
        let (slot, _, generated) = slot(&fixture);
        let raw = physical_file(&mut fixture, &device(), "raw-revoked");
        fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
        fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
        fixture.key(1, true);
        for _ in 0..3 {
            fixture.stream.flush().unwrap();
        }
        assert_eq!(events(&generated), [(42, 1)]);
        let held = raw_frame(&device(), 2, &[(30, 0), (48, 1), (48, 0)]);
        let mut revoked = fixture.owner.clone();
        revoked.capabilities.remove(&Capability::SyntheticInput);
        fixture.subscribe(revoked, 2);
        fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
        assert!(fixture.stream.key_drain_pending("seat0"));
        let mut held = Some(held);
        if queued {
            let frame = held.take().unwrap();
            fixture.stream.push(Record::Frame { frame }).unwrap();
        }
        let request = fixture
            .drain()
            .into_iter()
            .find_map(|message| match message {
                Message::KeyDrain { request } => Some(request),
                _ => None,
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(40));
        assert!(!events(&raw).ends_with(&[(48, 1), (48, 0)]));
        fixture.send(Command::KeyDrained {
            request,
            at: monotonic().unwrap(),
        });
        fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
        assert!(!fixture.stream.key_drain_pending("seat0"));
        if let Some(frame) = held {
            fixture.stream.push(Record::Frame { frame }).unwrap();
        }
        for _ in 0..3 {
            fixture.stream.flush().unwrap();
        }
        assert_eq!(events(&generated), [(42, 1), (42, 0)]);
        assert!(events(&raw).ends_with(&[(48, 1), (48, 0)]));
    }
}

#[test]
fn input_read_late_is_delivered_after_its_read_in_order() {
    let mut fixture = Fixture::new(profile(), super::owner());
    let raw = physical_file(&mut fixture, &device(), "raw-late");
    let first = raw_frame(&device(), 1, &[(48, 1)]);
    std::thread::sleep(Duration::from_millis(5));
    let second = raw_frame(&device(), 2, &[(48, 0)]);
    std::thread::sleep(Duration::from_millis(40));
    fixture.stream.push(Record::Frame { frame: first }).unwrap();
    fixture
        .stream
        .push(Record::Frame { frame: second })
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_millis(200);
    while events(&raw).len() < 2 && std::time::Instant::now() < deadline {
        fixture.stream.flush().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(events(&raw), [(48, 1), (48, 0)]);
}

#[test]
fn runtime_route_replacement_accepts_the_fresh_reader_incarnation() {
    let mut fixture = Fixture::new(profile(), owner());
    let (slot, _descriptor, path) = slot(&fixture);
    fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    fixture.key(1, true);
    ack(&mut fixture);
    for _ in 0..3 {
        fixture.stream.flush().unwrap();
    }
    for enabled in [false, true] {
        let routing = &mut fixture.stream.seats.get_mut("seat0").unwrap().routing;
        routing
            .transact_rules(
                &Transaction {
                    revision: routing.rule_revision(),
                    changes: vec![Change::Enabled {
                        id: "shift".into(),
                        enabled,
                    }],
                },
                monotonic().unwrap(),
            )
            .unwrap();
        fixture
            .stream
            .collect("seat0", monotonic().unwrap(), None)
            .unwrap();
        for _ in 0..3 {
            fixture.stream.flush().unwrap();
        }
    }
    fixture.key(2, false);
    let old = device();
    fixture
        .stream
        .push(Record::Remove {
            source: old.source.clone(),
            at: monotonic().unwrap(),
        })
        .unwrap();
    let mut fresh = old;
    fresh.source.generation += 1;
    fixture
        .stream
        .push(Record::Device {
            device: fresh.clone(),
            keys: BTreeSet::new(),
            contacts: vec![],
        })
        .unwrap();
    fixture.attach_device(&fresh);
    fixture
        .stream
        .push(Record::Frame {
            frame: Frame {
                source: fresh.source.clone(),
                sequence: 1,
                at: monotonic().unwrap(),
                events: vec![
                    RawEvent {
                        kind: EV_KEY,
                        code: 30,
                        value: 1,
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
    let routing = &mut fixture.stream.seats.get_mut("seat0").unwrap().routing;
    while let Some(scheduled) = routing.due(&fresh.source, monotonic().unwrap()).unwrap() {
        let ticket = scheduled.ticket;
        routing
            .delivered(&fresh.source, ticket, monotonic().unwrap())
            .unwrap();
    }
    for _ in 0..3 {
        fixture.stream.flush().unwrap();
    }
    assert_eq!(events(&path), [(42, 1), (42, 0), (42, 1)]);
}

#[test]
fn shutdown_services_consumer_drain_without_reactivating_subscriptions() {
    let mut fixture = Fixture::new(profile(), owner());
    let (slot, descriptor, path) = slot(&fixture);
    let raw = physical_file(&mut fixture, &device(), "raw-bytes");
    fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    fixture.key(1, true);
    for _ in 0..3 {
        fixture.stream.flush().unwrap();
    }
    let (replacement, _peer) = Channel::pair().unwrap();
    let mut peer = std::mem::replace(&mut fixture.parent, replacement);
    let owner = fixture.owner.clone();
    let expected = descriptor.marker;
    let receiver = std::thread::spawn(move || {
        let until = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(packet) = peer.receive::<Message>().unwrap()
                && let Message::KeyDrain { request } = packet.message
            {
                assert_eq!(request.marker, expected);
                assert_eq!(events(&path), [(42, 1), (42, 0)]);
                assert_eq!(events(&raw), [(30, 1)]);
                assert!(
                    peer.send(
                        &Command::Subscriptions {
                            update: Subscriptions {
                                revision: 999,
                                seats: vec![owner]
                            }
                        },
                        monotonic().unwrap()
                    )
                    .unwrap()
                );
                let mut wrong = request.clone();
                wrong.nonce.0[0] ^= 1;
                assert!(
                    peer.send(
                        &Command::KeyDrained {
                            request: wrong,
                            at: monotonic().unwrap()
                        },
                        monotonic().unwrap()
                    )
                    .unwrap()
                );
                assert!(
                    peer.send(
                        &Command::KeyDrained {
                            request,
                            at: monotonic().unwrap()
                        },
                        monotonic().unwrap()
                    )
                    .unwrap()
                );
                return;
            }
            assert!(
                std::time::Instant::now() < until,
                "shutdown did not request consumer drain"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    fixture.stream.shutdown();
    receiver.join().unwrap();
    assert!(fixture.stream.seats.is_empty());
    assert!(fixture.stream.closing_keys.is_empty());
    assert!(fixture.stream.closing_outputs.is_empty());
    assert!(!fixture.stream.key_drain_pending("seat0"));
    assert!(fs::read_dir(&fixture.directory).unwrap().all(|entry| {
        matches!(
            entry.unwrap().file_name().to_str(),
            Some("generated-bytes" | "raw-bytes")
        )
    }));
}

#[test]
fn shutdown_without_a_receipt_is_bounded_and_destroys_all_outputs() {
    let mut fixture = Fixture::new(profile(), owner());
    let (slot, _, path) = slot(&fixture);
    physical_file(&mut fixture, &device(), "raw-bytes");
    fixture.stream.seats.get_mut("seat0").unwrap().keys = Some(slot);
    fixture.stream.refresh_keys(monotonic().unwrap()).unwrap();
    fixture.key(1, true);
    for _ in 0..3 {
        fixture.stream.flush().unwrap();
    }
    let started = std::time::Instant::now();
    fixture.stream.shutdown();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(fixture.stream.closing_keys.is_empty());
    assert!(fixture.stream.closing_outputs.is_empty());
    assert!(!fixture.stream.key_drain_pending("seat0"));
    assert_eq!(events(&path), [(42, 1), (42, 0)]);
    assert!(fs::read_dir(&fixture.directory).unwrap().all(|entry| {
        matches!(
            entry.unwrap().file_name().to_str(),
            Some("generated-bytes" | "raw-bytes")
        )
    }));
}
