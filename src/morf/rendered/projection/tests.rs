use super::*;
#[test]
fn layout_projection_requires_one_stable_owning_surface_and_keeps_keyboard_unknown() {
    let at = Duration::from_secs(1);
    let mut context = super::super::tests::sample(at);
    let frame = context.frame.as_mut().unwrap();
    frame.surface = Some(SurfaceSpace {
        namespace: "shell".into(),
        width: 1500,
        height: 1200,
        local_scale: 1.5,
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
    let layers = [layer.clone()];
    let mapped =
        LayoutProjection::from_samples(&context, &output, &layers, &layers, (4321, 999), at)
            .unwrap()
            .unwrap();
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
    mapped.validate(&context, &output, at).unwrap();
    let mut duplicated = layers.to_vec();
    let mut extra = layer.clone();
    extra.address = "0x456".into();
    duplicated.push(extra);
    assert!(
        LayoutProjection::from_samples(&context, &output, &duplicated, &layers, (4321, 999), at)
            .unwrap()
            .is_none()
    );
    let mut changed = layer.clone();
    changed.level = 3;
    assert!(
        LayoutProjection::from_samples(&context, &output, &layers, &[changed], (4321, 999), at)
            .unwrap()
            .is_none()
    );
    assert!(
        LayoutProjection::from_samples(&context, &output, &layers, &layers, (4322, 999), at)
            .unwrap()
            .is_none()
    );
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
