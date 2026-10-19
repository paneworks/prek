use super::*;
fn fixture(at: Duration) -> (Context, OutputState, Layer) {
    let mut context = super::super::tests::sample(at);
    let frame = context.frame.as_mut().unwrap();
    frame.surface = Some(SurfaceSpace {
        namespace: "shell".into(),
        width: 1500,
        height: 1200,
        local_scale: 1.5,
        mapped_ns: Some(100_000_000),
    });
    let mut output = crate::context_transfer::tests::view(at)
        .context
        .unwrap()
        .snapshot
        .outputs
        .remove(0);
    output.keyboard_known = false;
    output.keyboard = None;
    output.geometry.origin = Point {
        x: -1500.0,
        y: 20.0,
    };
    output.geometry.pixel_width = 1500.0;
    output.geometry.pixel_height = 1200.0;
    let layer = Layer {
        output: output.geometry.name.clone(),
        address: "0x123".into(),
        namespace: "shell".into(),
        pid: Some(4321),
        level: 2,
        rect: output.rect(),
    };
    (context, output, layer)
}
fn inventory(layers: &[Layer], map_in: Option<u64>) -> Inventory {
    Inventory {
        layers: layers.to_vec(),
        map_in: map_in.map(Duration::from_millis),
    }
}
#[test]
fn layout_projection_requires_one_stable_owning_surface_and_keeps_keyboard_unknown() {
    let at = Duration::from_secs(1);
    let (context, output, layer) = fixture(at);
    let layers = inventory(std::slice::from_ref(&layer), None);
    let project = |before: &Inventory, after: &Inventory, pid| {
        LayoutProjection::from_samples(&context, &output, before, after, (pid, 999), (at, at))
            .unwrap()
    };
    let mapped = project(&layers, &layers, 4321).unwrap();
    assert_eq!(
        mapped.regions[0].bounds.unwrap(),
        Rect {
            origin: Point {
                x: -1484.625,
                y: 920.0
            },
            size: Point {
                x: 1469.25,
                y: 300.0
            }
        }
    );
    assert!(!output.keyboard_known);
    assert!(!mapped.settled());
    mapped.validate(&context, &output, at).unwrap();
    let mut extra = layer.clone();
    extra.address = "0x456".into();
    let duplicated = inventory(&[layer.clone(), extra], None);
    assert!(project(&duplicated, &layers, 4321).is_none());
    let mut changed = layer.clone();
    changed.level = 3;
    assert!(project(&layers, &inventory(&[changed], None), 4321).is_none());
    assert!(project(&layers, &layers, 4322).is_none());
    let mut forged = mapped.clone();
    forged.regions[0].bounds.as_mut().unwrap().origin.x += 1.0;
    assert!(forged.validate(&context, &output, at).is_err());
    let mut retired = output.clone();
    retired.geometry_generation += 1;
    assert!(mapped.validate(&context, &retired, at).is_err());
    assert!(
        mapped
            .validate(&context, &output, at + Duration::from_millis(250))
            .is_err()
    );
}
#[test]
fn layout_settles_only_after_its_map_bound_and_an_agreed_map_in_animation() {
    let at = Duration::from_secs(1);
    let (mut context, output, layer) = fixture(at);
    let layer = std::slice::from_ref(&layer);
    let project = |context: &Context, before, after, started| {
        LayoutProjection::from_samples(
            context,
            &output,
            &inventory(layer, before),
            &inventory(layer, after),
            (4321, 999),
            (Duration::from_millis(started), at),
        )
        .unwrap()
        .unwrap()
    };
    let settled = project(&context, Some(500), Some(500), 700);
    assert!(settled.settled());
    assert_eq!(settled.map_in_ns, Some(500_000_000));
    assert_eq!(settled.rendered_from_ns, Some(700_000_000));
    settled.validate(&context, &output, at).unwrap();
    let early = project(&context, Some(500), Some(500), 699);
    assert!(!early.settled());
    assert_eq!(early.rendered_from_ns, Some(700_000_000));
    for (before, after) in [(Some(500), Some(400)), (Some(500), None), (None, None)] {
        let unknown = project(&context, before, after, 900);
        assert!(!unknown.settled());
        assert_eq!((unknown.map_in_ns, unknown.rendered_from_ns), (None, None));
    }
    let mut forged = early.clone();
    forged.basis = Basis::SettledStockLayer;
    assert!(forged.validate(&context, &output, at).is_err());
    let mut forged = settled.clone();
    forged.rendered_from_ns = Some(0);
    assert!(forged.validate(&context, &output, at).is_err());
    assert!(
        LayoutProjection::from_samples(
            &context,
            &output,
            &inventory(layer, None),
            &inventory(layer, None),
            (4321, 999),
            (at + Duration::from_nanos(1), at),
        )
        .is_err()
    );
    let frame = context.frame.as_mut().unwrap();
    frame.surface.as_mut().unwrap().mapped_ns = None;
    let unmapped = project(&context, Some(500), Some(500), 900);
    assert!(!unmapped.settled());
    assert_eq!(unmapped.rendered_from_ns, None);
}
