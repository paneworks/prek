use super::*;

fn fixtures() -> (Vec<Device>, Vec<Endpoint>) {
    let devices = vec![kernel::fixture(false), kernel::fixture(false)];
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
fn configuration_is_an_exact_two_source_consuming_remap() {
    let (devices, endpoints) = fixtures();
    worker::validate_setup(&devices, &endpoints).unwrap();
    let config = configuration(&devices).unwrap();
    assert_eq!(config.rules.len(), 1);
    assert_eq!(config.rules[0].delivery, Delivery::Buffer);
    assert_eq!(config.rules[0].trigger, Trigger::Key { key: Key(30) });
    assert_eq!(config.rules[0].action, Action::Remap { key: Key(42) });
    assert!(config.devices["source0"].matches(&devices[0]));
    assert!(!config.devices["source0"].matches(&devices[1]));
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
fn fixture_protocol_rejects_extra_fields_and_unknown_messages() {
    for value in [
        serde_json::json!({"fixture":"step","id":1,"frames":1,"extra":true}),
        serde_json::json!({"fixture":"foreign"}),
        serde_json::json!({"fixture":"arm","command":["sh"]}),
    ] {
        assert!(serde_json::from_value::<Request>(value).is_err());
    }
    assert!(
        serde_json::from_value::<Request>(serde_json::json!({"fixture":"step","id":1,"frames":1}))
            .is_ok()
    );
}
