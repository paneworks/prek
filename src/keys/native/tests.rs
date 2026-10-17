use super::*;
use crate::{
    input::{EV_KEY, EV_SYN, SYN_REPORT},
    transport::{Class, KernelIdentity, Repeat},
};

#[path = "../../../tests/support/kernel.rs"]
mod kernel;

fn source() -> Source {
    Source {
        device: 0,
        generation: 1,
        seat: "seat0".into(),
    }
}
fn descriptor() -> Descriptor {
    Descriptor::generated_keyboard(
        source(),
        BTreeMap::from([("kb_layout".into(), "us".into())]),
    )
    .unwrap()
}
fn consumer() -> Consumer {
    Consumer::Hyprland {
        pid: 123,
        start: 456,
        instance: "owned-model".into(),
    }
}
fn evidence(descriptor: &Descriptor, at: Duration) -> ConsumerEvidence {
    ConsumerEvidence {
        consumer: consumer(),
        marker: descriptor.marker.clone(),
        name: descriptor.name.clone(),
        class: descriptor.class,
        seat: descriptor.source.seat.clone(),
        settings: descriptor.consumer_settings.clone(),
        at,
    }
}

#[test]
fn generated_descriptor_is_not_a_physical_counterpart_and_has_only_keyboard_keys() {
    let descriptor = descriptor();
    assert!(descriptor.is_generated_keyboard());
    assert_eq!(descriptor.source, source());
    assert_eq!(descriptor.class, Class::Keyboard);
    assert_eq!(
        descriptor.capabilities.keys().copied().collect::<Vec<_>>(),
        vec![EV_SYN, EV_KEY]
    );
    assert_eq!(descriptor.capabilities[&EV_SYN], [SYN_REPORT]);
    for key in [
        1, 30, 42, 113, 125, 126, 255, 0x160, 0x21f, 0x224, 0x2bf, 0x2e8, 0x2ff,
    ] {
        assert!(descriptor.capabilities[&EV_KEY].contains(&key));
    }
    for key in (0x100..=0x15f)
        .chain(0x220..=0x223)
        .chain(0x2c0..=0x2e7)
        .chain([0])
    {
        assert!(!descriptor.capabilities[&EV_KEY].contains(&key));
    }
    assert!(descriptor.axes.is_empty() && descriptor.properties.is_empty());
    assert!(descriptor.repeat.is_none() && !descriptor.feedback_required());
    assert_eq!(descriptor.udev["PREK_GENERATED_KEYBOARD"], "1");
    assert_eq!(descriptor.udev["ID_SEAT"], "seat0");
    assert!(
        crate::transport::Marker::parse(&descriptor.marker)
            .unwrap()
            .generated
    );
    let copy: Descriptor =
        serde_json::from_slice(&serde_json::to_vec(&descriptor).unwrap()).unwrap();
    assert_eq!(descriptor, copy);
    copy.validate().unwrap();
}

#[test]
fn generated_descriptor_revalidation_rejects_changed_native_shape_or_role() {
    let mutations: Vec<fn(&mut Descriptor)> = vec![
        |d| d.source.generation = 0,
        |d| d.source.seat = "seat1".into(),
        |d| d.name.push('x'),
        |d| d.marker = d.marker.replace("/keys/", "/"),
        |d| {
            d.identity = KernelIdentity {
                bus: 3,
                ..d.identity
            }
        },
        |d| d.class = Class::Buttons,
        |d| {
            d.capabilities.insert(2, vec![0]);
        },
        |d| {
            d.capabilities.get_mut(&EV_KEY).unwrap().push(0x100);
        },
        |d| {
            d.capabilities.remove(&EV_SYN);
        },
        |d| {
            d.repeat = Some(Repeat {
                delay_ms: 250,
                period_ms: 30,
            })
        },
        |d| d.properties.push(1),
        |d| {
            d.udev.remove("PREK_GENERATED_KEYBOARD");
        },
        |d| {
            d.udev.insert("ID_INPUT_MOUSE".into(), "1".into());
        },
        |d| {
            d.consumer_settings
                .insert("kb_layout".into(), "us\n".into());
        },
    ];
    for edit in mutations {
        let mut changed = descriptor();
        edit(&mut changed);
        assert!(
            changed.validate().is_err(),
            "accepted changed descriptor: {changed:?}"
        );
    }
    for seat in ["".into(), "s".repeat(129), "seat\n".into()] {
        assert!(
            Descriptor::generated_keyboard(Source { seat, ..source() }, BTreeMap::new()).is_err()
        );
    }
    let mut value = serde_json::to_value(descriptor()).unwrap();
    value["original"] = serde_json::Value::Null;
    assert!(serde_json::from_value::<Descriptor>(value).is_err());
}

#[test]
fn generated_readiness_requires_both_verified_metadata_neutrality_and_current_consumer() {
    let descriptor = descriptor();
    let mut admission = Admission::new(descriptor.clone(), consumer(), Duration::ZERO).unwrap();
    assert!(
        admission
            .poll(&descriptor, true, Duration::ZERO)
            .unwrap()
            .is_none()
    );
    admission
        .observe(&evidence(&descriptor, Duration::ZERO), Duration::ZERO)
        .unwrap();
    assert!(
        admission
            .poll(&descriptor, false, Duration::ZERO)
            .unwrap()
            .is_none()
    );
    let ready = admission
        .poll(&descriptor, true, Duration::ZERO)
        .unwrap()
        .unwrap();
    assert!(ready.matches_generated(&descriptor) && ready.valid_at(Duration::ZERO));
    assert!(!ready.matches(&crate::transport::tests::touch()));
    let physical = kernel::descriptor(&kernel::fixture(false));
    assert!(!ready.matches_generated(&physical));
    assert!(Admission::new(physical, consumer(), Duration::ZERO).is_err());
    assert!(admission.poll(&descriptor, true, Duration::ZERO).is_err());
}

#[test]
fn generated_consumer_expiry_can_renew_before_the_readiness_deadline() {
    let descriptor = descriptor();
    let mut admission = Admission::new(descriptor.clone(), consumer(), Duration::ZERO).unwrap();
    admission
        .observe(&evidence(&descriptor, Duration::ZERO), Duration::ZERO)
        .unwrap();
    let now = Duration::from_millis(251);
    assert!(admission.poll(&descriptor, true, now).unwrap().is_none());
    admission.observe(&evidence(&descriptor, now), now).unwrap();
    assert!(admission.poll(&descriptor, true, now).unwrap().is_some());
}

#[test]
fn generated_readiness_rejects_wrong_or_future_consumer_evidence() {
    let edits: Vec<fn(&mut ConsumerEvidence)> = vec![
        |e| e.marker.push('0'),
        |e| e.name.push('x'),
        |e| e.seat = "seat1".into(),
        |e| e.class = Class::Mouse,
        |e| e.settings.clear(),
        |e| e.at = Duration::from_millis(1),
        |e| {
            e.consumer = Consumer::Hyprland {
                pid: 124,
                start: 456,
                instance: "owned-model".into(),
            }
        },
    ];
    for edit in edits {
        let descriptor = descriptor();
        let mut admission = Admission::new(descriptor.clone(), consumer(), Duration::ZERO).unwrap();
        let mut observed = evidence(&descriptor, Duration::ZERO);
        edit(&mut observed);
        assert!(admission.observe(&observed, Duration::ZERO).is_err());
        assert!(admission.poll(&descriptor, true, Duration::ZERO).is_err());
    }
}

#[test]
fn generated_readiness_cannot_survive_deadline_or_backwards_time_or_changed_publication() {
    for at in [Duration::from_secs(5), Duration::from_millis(999)] {
        let descriptor = descriptor();
        let mut admission =
            Admission::new(descriptor.clone(), consumer(), Duration::from_secs(1)).unwrap();
        admission
            .observe(
                &evidence(&descriptor, Duration::from_secs(1)),
                Duration::from_secs(1),
            )
            .unwrap();
        let at = if at == Duration::from_secs(5) {
            at + Duration::from_secs(1)
        } else {
            at
        };
        assert!(admission.poll(&descriptor, true, at).is_err());
    }
    assert!(Admission::new(descriptor(), consumer(), Duration::MAX).is_err());
    let descriptor = descriptor();
    let mut admission = Admission::new(descriptor.clone(), consumer(), Duration::ZERO).unwrap();
    admission
        .observe(&evidence(&descriptor, Duration::ZERO), Duration::ZERO)
        .unwrap();
    let mut changed = descriptor.clone();
    changed.marker.push('0');
    assert!(admission.poll(&changed, true, Duration::ZERO).is_err());
}

#[test]
#[ignore = "user-launched root runner with owned isolated uinput receiver only"]
fn isolated_generated_keyboard_executes_owned_keys_and_exact_cancellation() {
    use crate::keys::{
        Owner, SourceKey,
        output::{Claim, Progress},
    };
    use std::{fs, thread, time::Instant};
    let registry = kernel::authorized();
    let source = Source {
        generation: kernel::now().as_nanos().try_into().unwrap(),
        ..source()
    };
    let spec = Descriptor::generated_keyboard(source.clone(), BTreeMap::new()).unwrap();
    let consumer = kernel::receiver_identity();
    let publication = registry.prepare_generated_isolated(spec).unwrap();
    let mut prepared = Prepared::create(publication, consumer.clone(), kernel::now()).unwrap();
    assert!(prepared.activate().unwrap().is_none());
    let mut receiver = kernel::Receiver::open(prepared.device.as_ref().unwrap(), &source);
    let nodes = prepared.device.as_ref().unwrap().event_nodes().unwrap();
    assert_eq!(nodes.len(), 1);
    let sysfs = fs::canonicalize(
        std::path::Path::new("/sys/class/input")
            .join(nodes[0].file_name().unwrap())
            .join("device"),
    )
    .unwrap();
    assert!(sysfs.starts_with("/sys/devices/virtual/input"));
    let now = kernel::now();
    let descriptor = prepared.descriptor().clone();
    prepared
        .observe(&ConsumerEvidence {
            consumer,
            marker: descriptor.marker.clone(),
            name: descriptor.name.clone(),
            class: Class::Keyboard,
            seat: source.seat.clone(),
            settings: BTreeMap::new(),
            at: now,
        })
        .unwrap();
    let mut output = prepared.activate().unwrap().unwrap();
    assert!(prepared.activate().is_err());
    fn drain(output: &mut Output<Virtual>) {
        let deadline = Instant::now() + Duration::from_millis(250);
        while output.flush().unwrap() != Progress::Complete {
            assert!(
                Instant::now() < deadline,
                "owned native key write did not drain"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn frame(receiver: &mut kernel::Receiver, expected: &[(u16, i32)]) {
        let frame = receiver
            .next(Duration::from_millis(250))
            .expect("missing generated keyboard frame");
        assert_eq!(
            frame.events.last(),
            Some(&crate::input::RawEvent {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0
            })
        );
        assert_eq!(
            frame.events[..frame.events.len() - 1]
                .iter()
                .map(|event| {
                    assert_eq!(event.kind, EV_KEY);
                    (event.code, event.value)
                })
                .collect::<Vec<_>>(),
            expected
        );
    }
    fn physical(serial: u64, device: u64) -> Claim {
        let source = Source {
            device,
            generation: 1,
            seat: "seat0".into(),
        };
        Claim {
            owner: Owner::Physical(SourceKey {
                source: source.clone(),
                code: 30,
            }),
            origin: Some(source),
            configuration: 1,
            serial,
        }
    }
    fn synthetic(serial: u64) -> Claim {
        Claim {
            owner: Owner::Synthetic {
                seat: "seat0".into(),
                generation: 1,
                id: serial,
            },
            origin: None,
            configuration: 1,
            serial,
        }
    }
    let a = physical(1, 1);
    let b = physical(2, 2);
    assert!(output.hold(a.clone(), &[0x110], kernel::now()).is_err());
    output.hold(a.clone(), &[42], kernel::now()).unwrap();
    drain(&mut output);
    frame(&mut receiver, &[(42, 1)]);
    assert_eq!(receiver.keys(), [42]);
    assert_eq!(
        output.hold(b.clone(), &[42], kernel::now()).unwrap(),
        Progress::Complete
    );
    assert!(receiver.next(Duration::from_millis(20)).is_none());
    output.tap(synthetic(3), &[42, 30], kernel::now()).unwrap();
    drain(&mut output);
    frame(&mut receiver, &[(30, 1)]);
    frame(&mut receiver, &[(30, 0)]);
    assert_eq!(
        output.release(&a, kernel::now()).unwrap(),
        Progress::Complete
    );
    output.repeat(&b, kernel::now()).unwrap();
    drain(&mut output);
    frame(&mut receiver, &[(42, 2)]);
    assert_eq!(receiver.keys(), [42]);
    output.cancel(&b, kernel::now()).unwrap();
    drain(&mut output);
    frame(&mut receiver, &[(42, 0)]);
    let cancelled = synthetic(4);
    output
        .tap(cancelled.clone(), &[42, 30], kernel::now())
        .unwrap();
    output.cancel(&cancelled, kernel::now()).unwrap();
    drain(&mut output);
    assert!(receiver.next(Duration::from_millis(20)).is_none());
    assert!(receiver.keys().is_empty());
    output.hold(synthetic(5), &[42], kernel::now()).unwrap();
    drain(&mut output);
    frame(&mut receiver, &[(42, 1)]);
    let task = synthetic(6);
    output.tap(task.clone(), &[42, 30], kernel::now()).unwrap();
    output.cancel(&task, kernel::now()).unwrap();
    drain(&mut output);
    assert!(receiver.next(Duration::from_millis(20)).is_none());
    assert_eq!(receiver.keys(), [42]);
    output.cancel_all(kernel::now()).unwrap();
    drain(&mut output);
    frame(&mut receiver, &[(42, 0)]);
    let old = physical(7, 1);
    output.hold(old.clone(), &[42], kernel::now()).unwrap();
    drain(&mut output);
    frame(&mut receiver, &[(42, 1)]);
    output.cancel(&old, kernel::now()).unwrap();
    drain(&mut output);
    frame(&mut receiver, &[(42, 0)]);
    output.hold(physical(8, 1), &[42], kernel::now()).unwrap();
    drain(&mut output);
    frame(&mut receiver, &[(42, 1)]);
    assert_eq!(
        output.release(&old, kernel::now()).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        output.repeat(&old, kernel::now()).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        output.cancel(&old, kernel::now()).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(receiver.next(Duration::from_millis(20)).is_none());
    assert_eq!(receiver.keys(), [42]);
    output.cancel_all(kernel::now()).unwrap();
    drain(&mut output);
    frame(&mut receiver, &[(42, 0)]);
    assert!(receiver.keys().is_empty());
    output.retire(kernel::now()).unwrap();
    drain(&mut output);
    let device = match output.into_sink() {
        Ok(device) => device,
        Err(_) => panic!("native keyboard retained local ownership"),
    };
    assert!(device.generated_neutral().unwrap());
    drop(receiver);
    drop(device);
    let deadline = Instant::now() + Duration::from_secs(5);
    while sysfs.exists() {
        assert!(
            Instant::now() < deadline,
            "owned generated keyboard was not removed"
        );
        thread::sleep(Duration::from_millis(10));
    }
    println!(
        "\nPREK_GENERATED_KEY_OUTPUT_RESULT={}",
        serde_json::json!({
            "version": 1, "status": "passed", "uid": 0, "physical_devices_opened": 0,
            "native_preparation_tested": true, "generated_role_metadata_verified": true,
            "native_key_output_tested": true, "native_cancel_before_write_tested": true,
            "native_repeat_event_tested": true, "native_partial_write_tested": false,
            "broker_authorization_tested": false, "compositor_delivery_tested": false,
            "worker_binding_tested": false, "owned_device_removed": true,
        })
    );
}
