use super::*;
use crate::input::{Device, EV_KEY};

fn fixture() -> (Config, Metadata, BTreeSet<String>) {
    let mut config = Config::preset("desktop").unwrap();
    config.policy.allow_input_events = true;
    config.devices.get_mut("keyboard").unwrap().keys = vec![crate::config::Key(30)];
    let source = Source {
        device: u64::MAX,
        generation: u64::MAX,
        seat: "seat0".into(),
    };
    let device = Device {
        source: source.clone(),
        identity: "fixture".into(),
        name: "fixture".into(),
        path: "fixture".into(),
        physical: String::new(),
        unique: String::new(),
        vendor: 1,
        product: 1,
        properties: vec![],
        capabilities: BTreeMap::from([(EV_KEY, vec![30])]),
        axes: BTreeMap::new(),
        virtual_device: false,
    };
    let scope = BTreeSet::from(["keyboard".into()]);
    let input = Event::reset(
        &config,
        &device,
        None,
        Duration::from_millis(100),
        ResetReason::Attached,
        &BTreeSet::from([30]),
        &[],
    )
    .unwrap();
    (
        config,
        Metadata::project(input, &scope).unwrap().unwrap(),
        scope,
    )
}

#[test]
fn metadata_wire_is_strict_state_free_and_preserves_full_width_sources() {
    let (_, metadata, _) = fixture();
    metadata.validate().unwrap();
    let wire = serde_json::to_value(&metadata).unwrap();
    assert!(wire.get("keys").is_none() && wire.get("contacts").is_none());
    let decoded: Metadata = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(metadata, decoded);
    assert_eq!(decoded.source.device, u64::MAX);
    for field in [
        "keys", "contacts", "changes", "ticket", "target", "action", "fence",
    ] {
        let mut forged = wire.clone();
        forged[field] = serde_json::json!([]);
        assert!(
            serde_json::from_value::<Metadata>(forged).is_err(),
            "{field}"
        );
    }
}

#[test]
fn metadata_rejects_inconsistent_missing_or_unsorted_identity_and_info() {
    let (_, metadata, _) = fixture();
    for change in [
        "source",
        "selectors",
        "removed",
        "missing",
        "virtual",
        "duplicate",
        "unbounded",
    ] {
        let mut changed = metadata.clone();
        match change {
            "source" => changed.source.generation -= 1,
            "selectors" => changed.selectors.push("z".into()),
            "removed" => changed.reason = ResetReason::Removed,
            "missing" => changed.metadata = None,
            "virtual" => changed.metadata.as_mut().unwrap().device.virtual_device = true,
            "duplicate" => {
                changed.selectors.push("keyboard".into());
                changed.metadata.as_mut().unwrap().selectors = changed.selectors.clone();
            }
            "unbounded" => {
                changed.metadata.as_mut().unwrap().device.capabilities =
                    (0..32).map(|kind| (kind, (0..4096).collect())).collect()
            }
            _ => unreachable!(),
        }
        assert!(changed.validate().is_err(), "{change}");
    }
}

#[test]
fn metadata_requires_protected_scope_matching_device_and_original_age() {
    let (config, metadata, scope) = fixture();
    metadata
        .validate_for(&config, &scope, Duration::from_millis(350))
        .unwrap();
    assert!(
        metadata
            .validate_for(&config, &scope, Duration::from_millis(99))
            .is_err()
    );
    assert!(
        metadata
            .validate_for(&config, &scope, Duration::from_millis(351))
            .is_err()
    );
    assert!(
        metadata
            .validate_for(&config, &BTreeSet::new(), Duration::from_millis(100))
            .is_err()
    );
    for change in ["policy", "allowlist", "seat", "selector"] {
        let mut changed = config.clone();
        match change {
            "policy" => changed.policy.allow_input_events = false,
            "allowlist" => changed.policy.allowed_devices.clear(),
            "seat" => changed.policy.allowed_seats.clear(),
            "selector" => {
                changed.devices.get_mut("keyboard").unwrap().keys = vec![crate::config::Key(42)]
            }
            _ => unreachable!(),
        }
        assert!(
            metadata
                .validate_for(&changed, &scope, Duration::from_millis(100))
                .is_err(),
            "{change}"
        );
    }
}

#[test]
fn metadata_projection_does_not_disclose_unrelated_sources_or_accept_frames() {
    let (_, metadata, _) = fixture();
    let input = Event::Reset {
        source: metadata.source.clone(),
        at: metadata.at,
        selectors: metadata.selectors,
        reason: metadata.reason,
        keys: BTreeSet::from([30]),
        contacts: vec![],
        metadata: metadata.metadata,
    };
    assert!(
        Metadata::project(input, &BTreeSet::from(["unrelated".into()]))
            .unwrap()
            .is_none()
    );
    let frame = Event::Frame {
        source: metadata.source,
        at: metadata.at,
        sequence: 1,
        selectors: vec!["keyboard".into()],
        changes: vec![],
        contacts: vec![],
        contacts_started: vec![],
        contacts_ended: vec![],
        ended_contacts: vec![],
    };
    assert!(Metadata::project(frame, &BTreeSet::from(["keyboard".into()])).is_err());
}
