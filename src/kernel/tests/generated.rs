use super::*;
use crate::input::EV_KEY;
use std::time::Duration;

fn generated() -> Descriptor {
    Descriptor::generated_keyboard(descriptor().source, BTreeMap::new()).unwrap()
}

#[test]
fn generated_and_physical_publications_have_independent_marker_namespaces() {
    let physical = descriptor();
    let generated = generated();
    assert_eq!(physical.source, generated.source);
    assert_ne!(
        filename(&physical.marker).unwrap(),
        filename(&generated.marker).unwrap()
    );
    let marker = crate::transport::Marker::parse(&generated.marker).unwrap();
    assert!(marker.generated && marker.publication.is_none());
    for marker in [
        "prek/keys/0000000000000001/0000000000000001",
        "prek/test/keys/0000000000000001/0000000000000001",
        "prek/keys/0000000000000001/0000000000000001/11111111111111111111111111111111",
        "prek/test/keys/0000000000000001/0000000000000001/11111111111111111111111111111111",
    ] {
        assert!(crate::transport::Marker::parse(marker).unwrap().generated);
    }
    for marker in [
        "prek/keys/../0000000000000001",
        "prek/keys/0000000000000001/0000000000000001/",
        "prek/keys/test/0000000000000001/0000000000000001",
    ] {
        assert!(filename(marker).is_err());
    }
}

#[test]
fn generated_publication_cannot_use_a_physical_publication_entry_point() {
    let temporary = Temporary::new();
    let registry = temporary.registry();
    assert_eq!(
        registry.prepare(generated()).err().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        registry.prepare_isolated(generated()).err().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        registry
            .prepare_generated_isolated(descriptor())
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(fs::read_dir(&temporary.path).unwrap().count(), 0);
}

#[test]
fn generated_isolated_publication_preserves_preadd_role_and_test_exclusion() {
    let temporary = Temporary::new();
    let registry = temporary.registry();
    let publication = registry.prepare_generated_isolated(generated()).unwrap();
    let descriptor = publication.descriptor();
    descriptor.validate().unwrap();
    assert!(descriptor.is_generated_keyboard() && descriptor.is_isolated());
    assert!(descriptor.marker.starts_with("prek/test/keys/"));
    let metadata = registry.directory.read(&descriptor.marker).unwrap();
    for key in [
        "PREK_VIRTUAL",
        "PREK_TEST",
        "PREK_GENERATED_KEYBOARD",
        "LIBINPUT_IGNORE_DEVICE",
    ] {
        assert_eq!(metadata.properties[key], "1");
    }
    metadata.validate(&descriptor.marker).unwrap();
    publication.verify().unwrap();
}

#[test]
fn generated_metadata_role_cannot_be_removed_forged_or_attached_to_a_physical_marker() {
    let temporary = Temporary::new();
    let registry = temporary.registry();
    let publication = registry.prepare_generated_isolated(generated()).unwrap();
    for value in [None, Some("0"), Some("keyboard")] {
        let mut metadata = publication.metadata.clone();
        metadata.properties.remove("PREK_GENERATED_KEYBOARD");
        if let Some(value) = value {
            metadata
                .properties
                .insert("PREK_GENERATED_KEYBOARD".into(), value.into());
        }
        assert!(metadata.validate(&metadata.marker).is_err());
    }
    let mut metadata = publication.metadata.clone();
    metadata.marker = metadata.marker.replace("/keys/", "/");
    metadata
        .properties
        .insert("PREK_PUBLICATION".into(), metadata.marker.clone());
    assert!(metadata.validate(&metadata.marker).is_err());
}

#[test]
fn generated_publication_drop_cannot_remove_a_same_source_physical_counterpart() {
    let temporary = Temporary::new();
    let registry = temporary.registry();
    let physical = registry.prepare_isolated(descriptor()).unwrap();
    let generated = registry.prepare_generated_isolated(generated()).unwrap();
    let marker = generated.descriptor().marker.clone();
    drop(generated);
    assert!(registry.directory.read(&marker).is_err());
    physical.verify().unwrap();
    drop(physical);
}

#[test]
fn generated_key_sink_refuses_every_physical_adapter_operation_without_a_device() {
    use crate::keys::output::Sink;
    let temporary = Temporary::new();
    let registry = temporary.registry();
    let mut original = crate::transport::tests::touch();
    original.identity = "test:keyboard".into();
    original.name = "Prek test keyboard".into();
    original.physical = "fixture/keyboard".into();
    original.path.clear();
    original.axes.clear();
    original.properties.clear();
    original.capabilities = BTreeMap::from([(EV_SYN, vec![SYN_REPORT]), (EV_KEY, vec![30, 42])]);
    let descriptor = Descriptor::mirror(
        &original,
        descriptor().identity,
        None,
        &BTreeMap::new(),
        BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(descriptor.class, crate::transport::Class::Keyboard);
    let publication = registry.prepare_isolated(descriptor).unwrap();
    let path = temporary.path.join("ordinary-file");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    let mut virtual_device = Virtual {
        file,
        publication,
        pending: None,
        stopped: false,
        created: false,
        sequence: None,
    };
    assert!(!Sink::supports_key(&virtual_device, 30));
    let frame = Frame {
        source: original.source,
        at: Duration::ZERO,
        sequence: 1,
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
    };
    assert_eq!(
        Sink::enqueue(&mut virtual_device, &frame)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        Sink::flush(&mut virtual_device).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        Sink::abort(&mut virtual_device).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(virtual_device.pending.is_none());
    assert_eq!(fs::metadata(path).unwrap().len(), 0);
}
