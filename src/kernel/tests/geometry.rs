use super::*;
use crate::input::ABS_MT_POSITION_X;

fn model(slots: u16, edit: impl FnOnce(&mut Device)) -> Descriptor {
    let mut device = crate::transport::tests::touch();
    device.identity = "test:geometry".into();
    device.name = "Prek test geometry".into();
    device.physical = "fixture/geometry".into();
    device.path.clear();
    for axis in device.axes.values_mut() {
        axis.fuzz = 0;
    }
    device.axes.get_mut(&ABS_MT_SLOT).unwrap().maximum = i32::from(slots) - 1;
    edit(&mut device);
    assert_eq!(device.touch_capacity(), Some(usize::from(slots)));
    let spec = Descriptor::mirror(&device, descriptor().identity, None, &BTreeMap::new()).unwrap();
    spec.validate().unwrap();
    spec
}

#[test]
fn native_slot_limits_do_not_shrink_the_general_descriptor_model() {
    for slots in [1, 2, 99, 100, 101, 256] {
        let spec = model(slots, |_| {});
        let encoded = serde_json::to_vec(&spec).unwrap();
        let decoded: Descriptor = serde_json::from_slice(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded, spec);
        if slots <= 100 {
            supported_backend(&spec).unwrap();
        } else {
            let error = supported_backend(&spec).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(error.to_string().contains("100 Type B slots"));
        }
        assert_eq!(serde_json::to_vec(&spec).unwrap(), encoded);
    }
}

#[test]
fn native_flat_limits_preserve_wide_and_signed_coordinate_ranges() {
    for (minimum, maximum, flat, accepted) in [
        (0, 100, 100, true),
        (0, 100, 101, false),
        (-100, -20, 80, true),
        (-100, -20, 81, false),
        (0, 0, 0, true),
        (0, 0, 1, false),
        (5, 5, 0, true),
        (5, 5, 1, false),
        (i32::MIN, i32::MAX, i32::MAX, true),
        (i32::MIN, -1, i32::MAX, true),
        (i32::MIN, -2, i32::MAX, false),
        (i32::MAX - 1, i32::MAX, 1, true),
        (i32::MAX - 1, i32::MAX, 2, false),
        (i32::MIN, i32::MIN + 1, 1, true),
        (i32::MIN, i32::MIN + 1, 2, false),
    ] {
        let spec = model(2, |device| {
            let axis = device.axes.get_mut(&ABS_MT_POSITION_X).unwrap();
            axis.minimum = minimum;
            axis.maximum = maximum;
            axis.flat = flat;
        });
        let result = supported_backend(&spec);
        assert_eq!(
            result.is_ok(),
            accepted,
            "{minimum}..{maximum}, flat={flat}"
        );
        if let Err(error) = result {
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(error.to_string().contains("axis 53 flat"));
        }
    }
}

#[test]
fn tracking_id_ranges_cannot_be_silently_normalized() {
    for (minimum, maximum) in [(1, 65535), (-1, 65535), (0, 65534), (0, 65536)] {
        let spec = model(2, |device| {
            let axis = device.axes.get_mut(&ABS_MT_TRACKING_ID).unwrap();
            axis.minimum = minimum;
            axis.maximum = maximum;
        });
        let error = supported_backend(&spec).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("slot/tracking-ID metadata"));
    }
}

#[test]
fn slot_and_tracking_flat_are_fixed_but_resolution_is_preserved() {
    for code in [ABS_MT_SLOT, ABS_MT_TRACKING_ID] {
        let spec = model(2, |device| device.axes.get_mut(&code).unwrap().flat = 1);
        assert_eq!(
            supported_backend(&spec).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }
    let spec = model(100, |device| {
        device.axes.get_mut(&ABS_MT_SLOT).unwrap().resolution = 7;
        device.axes.get_mut(&ABS_MT_TRACKING_ID).unwrap().resolution = 55;
    });
    supported_backend(&spec).unwrap();
    assert_eq!(spec.axes[&ABS_MT_SLOT].resolution, 7);
    assert_eq!(spec.axes[&ABS_MT_TRACKING_ID].resolution, 55);
}

#[test]
fn owned_publication_preflight_never_leaves_metadata_or_normalizes_geometry() {
    let temporary = Temporary::new();
    let registry = temporary.registry();
    let owner: WorkerIdentity = serde_json::from_value(serde_json::json!({
        "pid": std::process::id(), "birth": 1, "epoch": 1
    }))
    .unwrap();
    let names = || {
        fs::read_dir(&temporary.path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>()
    };
    let original = names();
    for spec in [
        model(101, |_| {}),
        model(2, |device| {
            device.axes.get_mut(&ABS_MT_POSITION_X).unwrap().flat = 4096;
        }),
    ] {
        assert_eq!(
            registry
                .publish(spec, false, Some(owner))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(names(), original);
    }
    let spec = model(100, |_| {});
    let publication = registry.publish(spec.clone(), false, Some(owner)).unwrap();
    assert_eq!(publication.metadata.owner, Some(owner));
    assert_eq!(publication.descriptor.axes, spec.axes);
    publication.verify().unwrap();
}

#[test]
fn native_creation_refuses_geometry_before_publication_verification_or_device_open() {
    let temporary = Temporary::new();
    let registry = temporary.registry();
    for spec in [
        model(101, |_| {}),
        model(2, |device| {
            device.axes.get_mut(&ABS_MT_POSITION_X).unwrap().flat = 4096;
        }),
    ] {
        let publication = registry.prepare_isolated(spec).unwrap();
        fs::remove_file(temporary.path.join(publication.name.to_str().unwrap())).unwrap();
        assert_eq!(
            publication.verify().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            Virtual::create(publication).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert!(fs::read_dir(&temporary.path).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("prek-")
        }));
    }
}
