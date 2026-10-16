use super::*;

pub(crate) fn sample(at: Duration) -> Context {
    Context {
        version: 1,
        epoch: "a".repeat(32),
        ui_generation: 2,
        revision: 7,
        sampled_ns: at.as_nanos().try_into().unwrap(),
        coverage: Coverage::PrimarySurfaceNamedBounds,
        frame: Some(Frame {
            surface: None,
            keyboard: None,
            width: 1000,
            height: 800,
            scale_120: 120,
            regions: vec![Region {
                node: u64::MAX,
                id: "drawer-keyboard".into(),
                interactive: false,
                semantic: None,
                bounds: Bounds {
                    x: 10.25,
                    y: 600.0,
                    width: 979.5,
                    height: 200.0,
                },
            }],
        }),
    }
}
#[test]
fn semantic_roles_cannot_supply_unverified_bounds_or_keyboard_absence() {
    let at = Duration::from_secs(1);
    let mut context = sample(at);
    context.frame.as_mut().unwrap().regions[0].semantic = Some(Semantic {
        role: SemanticRole::Keyboard,
        layout: Some("dev".into()),
    });
    context.validate(&state(), at, None).unwrap();
    let original = serde_json::to_value(&context).unwrap();
    for bad in [
        serde_json::json!({"role":"keyboard"}),
        serde_json::json!({"role":"keyboard","layout":""}),
        serde_json::json!({"role":"top_left","layout":"dev"}),
        serde_json::json!({"role":"keyboard_hidden"}),
        serde_json::json!({"role":"keyboard","layout":"dev","x":10}),
    ] {
        let mut value = original.clone();
        value["frame"]["regions"][0]["semantic"] = bad;
        assert!(
            serde_json::from_value::<Context>(value)
                .and_then(|context| context
                    .validate(&state(), at, None)
                    .map(|_| context)
                    .map_err(serde_json::Error::io))
                .is_err()
        );
    }
    context.frame.as_mut().unwrap().regions[0].semantic = Some(Semantic {
        role: SemanticRole::TopLeft,
        layout: None,
    });
    context.validate(&state(), at, None).unwrap();
}
fn state() -> ShellState {
    ShellState {
        version: 1,
        role: "shell".into(),
        epoch: "a".repeat(32),
        revision: 2,
        phase: ShellPhase::Configured,
        compositor_pid: 123,
        compositor_start: 456,
        compositor_uid: 1000,
        output: "fixture-output".into(),
    }
}
#[test]
fn registered_keyboard_and_surface_metadata_reject_contradictions_without_claiming_global_absence()
{
    let at = Duration::from_secs(1);
    let mut context = sample(at);
    let frame = context.frame.as_mut().unwrap();
    frame.surface = Some(SurfaceSpace {
        namespace: "shell".into(),
        width: 1000,
        height: 800,
        local_scale: 1.0,
    });
    frame.regions[0].semantic = Some(Semantic {
        role: SemanticRole::Keyboard,
        layout: Some("full".into()),
    });
    frame.keyboard = Some(KeyboardRegistry {
        scope: RegistryScope::RegisteredMorfPrimary,
        registered: 1,
        state: KeyboardState::Visible,
        node: Some(u64::MAX.to_string()),
        layout: Some("full".into()),
    });
    context.validate(&state(), at, None).unwrap();
    for case in 0..6 {
        let mut bad = context.clone();
        let frame = bad.frame.as_mut().unwrap();
        match case {
            0 => frame.keyboard.as_mut().unwrap().state = KeyboardState::Hidden,
            1 => frame.keyboard.as_mut().unwrap().registered = 0,
            2 => frame.keyboard.as_mut().unwrap().node = Some("01".into()),
            3 => frame.keyboard.as_mut().unwrap().layout = Some("dev".into()),
            4 => frame.surface.as_mut().unwrap().local_scale = f64::NAN,
            _ => frame.surface.as_mut().unwrap().width = 1001 + 2,
        }
        assert!(bad.validate(&state(), at, None).is_err());
    }
    let frame = context.frame.as_mut().unwrap();
    frame.regions.clear();
    frame.keyboard.as_mut().unwrap().state = KeyboardState::Hidden;
    context.validate(&state(), at, None).unwrap();
    let frame = context.frame.as_mut().unwrap();
    frame.keyboard = Some(KeyboardRegistry {
        scope: RegistryScope::RegisteredMorfPrimary,
        registered: 2,
        state: KeyboardState::Unknown,
        node: None,
        layout: None,
    });
    context.validate(&state(), at, None).unwrap();
}
#[test]
fn native_bounds_reject_freshness_identity_regression_and_fabricated_geometry() {
    let now = Duration::from_secs(1);
    let original = sample(now);
    original.validate(&state(), now, None).unwrap();
    assert_eq!(
        serde_json::from_slice::<Context>(&serde_json::to_vec(&original).unwrap()).unwrap(),
        original
    );
    for case in 0..13 {
        let mut changed = original.clone();
        match case {
            0 => changed.epoch = "b".repeat(32),
            1 => changed.ui_generation = 0,
            2 => changed.ui_generation = 1,
            3 => changed.revision = 6,
            4 => changed.sampled_ns += 1,
            5 => changed.sampled_ns -= 250_000_000,
            6 => changed.frame.as_mut().unwrap().scale_120 = 0,
            7 => changed.frame.as_mut().unwrap().regions[0].bounds.width = 1001.0,
            8 => changed.frame.as_mut().unwrap().regions[0].bounds.x = -1.0,
            9 => changed.frame.as_mut().unwrap().regions[0].bounds.y = f64::NAN,
            10 => {
                let frame = changed.frame.as_mut().unwrap();
                frame.regions.push(frame.regions[0].clone());
            }
            11 => changed.frame.as_mut().unwrap().regions[0].id = "x".repeat(129),
            _ => changed.frame.as_mut().unwrap().regions[0].bounds.x += 0.1,
        }
        assert!(
            changed.validate(&state(), now, Some(&original)).is_err(),
            "case {case}"
        );
    }
    let mut newer = original.clone();
    newer.revision += 1;
    newer.frame.as_mut().unwrap().regions[0].bounds.x += 0.1;
    newer.validate(&state(), now, Some(&original)).unwrap();
    let mut missing = original.clone();
    missing.frame = None;
    missing.validate(&state(), now, Some(&original)).unwrap();
    assert!(!missing.fresh(now));
    let mut repeated = original.clone();
    let frame = repeated.frame.as_mut().unwrap();
    frame.regions.push(Region {
        node: u64::MAX - 1,
        ..frame.regions[0].clone()
    });
    repeated.validate(&state(), now, None).unwrap();
    let mut value = serde_json::to_value(&original).unwrap();
    value["ui_generation"] = serde_json::json!(2);
    assert!(serde_json::from_value::<Context>(value).is_err());
    let mut value = serde_json::to_value(&original).unwrap();
    value["revision"] = serde_json::json!("07");
    assert!(serde_json::from_value::<Context>(value).is_err());
    let mut value = serde_json::to_value(&original).unwrap();
    value["frame"]["keyboard_hidden"] = serde_json::json!(true);
    assert!(serde_json::from_value::<Context>(value).is_err());
}
#[test]
fn native_context_framing_has_an_explicit_larger_bound_without_changing_control_limits() {
    use std::os::unix::net::UnixStream;
    let (client, mut server) = UnixStream::pair().unwrap();
    client.set_nonblocking(true).unwrap();
    let mut context = sample(Duration::from_secs(1));
    let region = context.frame.as_ref().unwrap().regions[0].clone();
    context.frame.as_mut().unwrap().regions = (0..128)
        .map(|i| Region {
            node: i + 1,
            id: format!("named-{i}"),
            ..region.clone()
        })
        .collect();
    let expected = context.clone();
    let writer = std::thread::spawn(move || {
        let mut request = [0; 2];
        server.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"?\n");
        serde_json::to_writer(&mut server, &context).unwrap();
        server.write_all(b"\n").unwrap();
    });
    let mut transport = Transport {
        stream: client,
        failed: false,
    };
    let bytes = transport
        .exchange_limit(
            b"?\n",
            Instant::now() + Duration::from_secs(1),
            &mut false,
            MAX_BYTES,
        )
        .unwrap();
    assert!(bytes.len() > super::super::MAX_REPLY);
    assert_eq!(serde_json::from_slice::<Context>(&bytes).unwrap(), expected);
    writer.join().unwrap();
    let (client, mut server) = UnixStream::pair().unwrap();
    client.set_nonblocking(true).unwrap();
    let writer = std::thread::spawn(move || {
        let mut request = [0; 2];
        server.read_exact(&mut request).unwrap();
        let _ = server.write_all(&[b'x'; 33]);
    });
    let mut transport = Transport {
        stream: client,
        failed: false,
    };
    assert!(
        transport
            .exchange_limit(
                b"?\n",
                Instant::now() + Duration::from_secs(1),
                &mut false,
                32
            )
            .is_err()
    );
    assert!(transport.failed);
    writer.join().unwrap();
}
