use super::*;

fn fixtures() -> (Vec<Device>, Vec<Endpoint>) {
    let mut devices = vec![kernel::fixture(false), kernel::fixture(false)];
    for device in &mut devices {
        device.capabilities.insert(EV_KEY, vec![30, 31, 32]);
    }
    devices[0].capabilities.insert(EV_REL, vec![0]);
    let endpoints = devices
        .iter()
        .enumerate()
        .map(|(index, device)| Endpoint {
            path: PathBuf::from(format!("/dev/input/event{}", index + 100)),
            descriptor: {
                let mut producer = device.clone();
                producer.source.device += 1 << 32;
                kernel::descriptor(&producer)
                    .for_isolated()
                    .unwrap()
                    .bind_publication(Epoch([1, index as u64 + 1]))
                    .unwrap()
            },
        })
        .collect();
    (devices, endpoints)
}

#[test]
fn configuration_is_a_two_source_hybrid_remap_with_bounded_tap_fallback() {
    let (devices, endpoints) = fixtures();
    worker::validate_setup(&devices, &endpoints).unwrap();
    let config = configuration(&devices).unwrap();
    assert_eq!(config.rules.len(), 2);
    assert_eq!(config.rules[0].delivery, Delivery::Buffer);
    assert_eq!(config.rules[0].trigger, Trigger::Key { key: Key(30) });
    assert_eq!(config.rules[0].action, Action::Remap { key: Key(42) });
    assert!(config.devices["source0"].matches(&devices[0]));
    assert!(!config.devices["source0"].matches(&devices[1]));
    assert_eq!(
        config.rules[1].trigger,
        Trigger::ButtonTaps {
            key: Key(32),
            count: 2
        }
    );
    assert_eq!(config.timing.tap_gap, Millis(20));
    assert_eq!(config.timing.decision_window, Millis(200));
    assert!(config.rules[0].priority > config.rules[1].priority);
}

#[test]
fn hybrid_publication_serializes_one_full_capability_counterpart() {
    let (devices, _) = fixtures();
    let descriptor = kernel::descriptor(&devices[0])
        .for_isolated()
        .unwrap()
        .bind_publication(Epoch([3, 4]))
        .unwrap();
    let endpoint = Endpoint {
        path: "/dev/input/event101".into(),
        descriptor,
    };
    endpoint.validate().unwrap();
    assert_eq!(endpoint.descriptor.fixture_source(), Some(&devices[0]));
    let value = serde_json::to_value(endpoint).unwrap();
    assert_eq!(value["descriptor"]["class"], "mouse");
    assert_eq!(
        value["descriptor"]["capabilities"],
        serde_json::json!({"0":[0],"1":[30,31,32],"2":[0]})
    );
    assert_eq!(
        value["descriptor"]["source"],
        serde_json::to_value(&devices[0].source).unwrap()
    );
}

#[test]
fn setup_rejects_missing_duplicate_and_changed_source_bindings() {
    let (devices, endpoints) = fixtures();
    assert!(worker::validate_setup(&devices[..1], &endpoints[..1]).is_err());
    let mut changed = devices.clone();
    changed[1].source.device = changed[0].source.device;
    assert!(worker::validate_setup(&changed, &endpoints).is_err());
    let mut changed = endpoints.clone();
    changed.swap(0, 1);
    assert!(worker::validate_setup(&devices, &changed).is_err());
    let mut changed = devices.clone();
    changed[0].name.push('x');
    assert!(worker::validate_setup(&changed, &endpoints).is_err());
}

#[test]
fn endpoint_validation_does_not_accept_paths_or_nonisolated_roles() {
    let (devices, endpoints) = fixtures();
    for path in [
        "/dev/input/event",
        "/dev/input/eventx",
        "/dev/input/../event1",
        "/tmp/event1",
        "event1",
        "/dev/input/event1/extra",
    ] {
        let mut endpoint = endpoints[0].clone();
        endpoint.path = PathBuf::from(path);
        assert!(endpoint.validate().is_err(), "{path}");
    }
    let mut endpoint = endpoints[0].clone();
    endpoint.descriptor = kernel::descriptor(&devices[0]);
    assert!(endpoint.validate().is_err());
    let mut endpoint = endpoints[0].clone();
    endpoint.descriptor =
        Descriptor::generated_keyboard(devices[0].source.clone(), BTreeMap::new())
            .unwrap()
            .for_isolated()
            .unwrap()
            .bind_publication(Epoch([3, 4]))
            .unwrap();
    assert!(worker::validate_setup(&devices, &[endpoint, endpoints[1].clone()]).is_err());
}

#[test]
fn producer_namespace_cannot_alias_an_enrolled_source_or_another_endpoint() {
    let (devices, endpoints) = fixtures();
    let mut changed = endpoints.clone();
    changed[1].path = changed[0].path.clone();
    assert!(worker::validate_setup(&devices, &changed).is_err());
    let mut changed = endpoints.clone();
    let mut producer = devices[0].clone();
    producer.source = devices[1].source.clone();
    changed[0].descriptor = kernel::descriptor(&producer)
        .for_isolated()
        .unwrap()
        .bind_publication(Epoch([3, 4]))
        .unwrap();
    assert!(worker::validate_setup(&devices, &changed).is_err());
    let mut changed = devices.clone();
    changed[1].identity = changed[0].identity.clone();
    assert!(configuration(&changed).is_err());
}

#[test]
fn modeled_subscription_does_not_accept_a_compositor_consumer() {
    let fence = Fence {
        broker: Epoch([1, 2]),
        configuration: 1,
        session: 1,
    };
    assert!(
        subscription(
            fence,
            &Consumer::Hyprland {
                pid: 1,
                start: 1,
                instance: "not-isolated".into()
            },
            1,
            true
        )
        .is_err()
    );
    let isolated = Consumer::IsolatedReceiver {
        pid: std::process::id(),
        start: 1,
    };
    let active = subscription(fence, &isolated, 1, true).unwrap();
    let revoked = subscription(fence, &isolated, 2, false).unwrap();
    assert!(active.capabilities.contains(&Capability::SyntheticInput));
    assert!(!revoked.capabilities.contains(&Capability::SyntheticInput));
    assert_eq!(active.fence, revoked.fence);
}

#[test]
fn a_finished_worker_channel_is_read_to_its_end_and_then_stays_closed() {
    let (mut channel, mut worker) = Channel::pair().unwrap();
    let message = Message::Configuration {
        generation: 1,
        error: None,
    };
    assert!(worker.send(&message, monotonic().unwrap()).unwrap());
    drop(worker);
    let mut closed = false;
    assert!(
        parent::receive(&mut channel, true, &mut closed)
            .unwrap()
            .is_some()
    );
    for _ in 0..2 {
        assert!(
            parent::receive(&mut channel, true, &mut closed)
                .unwrap()
                .is_none()
        );
        assert!(closed);
    }
    let (mut channel, worker) = Channel::pair().unwrap();
    drop(worker);
    let mut closed = false;
    assert!(parent::receive(&mut channel, false, &mut closed).is_err());
    assert!(!closed);
}

#[test]
fn fixture_protocol_rejects_extra_fields_and_unknown_messages() {
    for value in [
        serde_json::json!({"fixture":"step","id":1,"frames":1,"extra":true}),
        serde_json::json!({"fixture":"foreign"}),
        serde_json::json!({"fixture":"arm","command":["sh"]}),
        serde_json::json!({"fixture":"idle","id":1,"until":999}),
    ] {
        assert!(serde_json::from_value::<Request>(value).is_err());
    }
    assert!(
        serde_json::from_value::<Request>(serde_json::json!({"fixture":"step","id":1,"frames":1}))
            .is_ok()
    );
}
