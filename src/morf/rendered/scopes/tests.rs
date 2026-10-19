use super::*;
use crate::{
    config::Screen,
    context::{OutputState, Rect, State},
    gestures::{Output, Point, Transform},
    hyprland::layers::{Inventory, Layer},
};

pub(crate) const COARSE: &str = "layer:phone:0x123";

fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
    Rect {
        origin: Point { x, y },
        size: Point {
            x: width,
            y: height,
        },
    }
}
fn named(node: u64, id: &str, b: [f64; 4], role: SemanticRole, interactive: bool) -> Region {
    Region {
        node,
        id: id.into(),
        bounds: Bounds {
            x: b[0],
            y: b[1],
            width: b[2],
            height: b[3],
        },
        interactive,
        semantic: Some(Semantic {
            role,
            layout: (role == SemanticRole::Keyboard).then(|| "full".into()),
        }),
    }
}
fn phone() -> OutputState {
    OutputState {
        geometry: Output {
            name: "phone".into(),
            pixel_width: 1000.0,
            pixel_height: 2000.0,
            scale: 2.0,
            origin: Point::default(),
            transform: Transform::Normal,
        },
        geometry_generation: 1,
        active_workspace: Some(1),
        screen: Screen::On,
        keyboard_known: false,
        keyboard: None,
    }
}
/// The stock snapshot of a phone whose only layer is a full-output shell.
pub(crate) fn coarse(output: OutputState, at: Duration) -> Snapshot {
    Snapshot {
        generation: 1,
        revision: 1,
        captured_at: at,
        state: State::Desktop,
        outputs: vec![output],
        windows: Vec::new(),
        regions: vec![ContextRegion {
            id: COARSE.into(),
            scope: None,
            output: "phone".into(),
            rect: rect(0.0, 0.0, 500.0, 1000.0),
            interactive: true,
        }],
        secure_overlay: false,
    }
}
/// A full-output Morf shell on the phone fixture sampled at `at`, with a left
/// strip, a physical bottom, one local control taking input and, when
/// visible, its registered keyboard.
pub(crate) fn shell(output: &OutputState, at: Duration, keyboard: KeyboardState) -> OutputContext {
    let mut regions = vec![
        named(
            1,
            "edge-left",
            [0.0, 30.0, 8.0, 740.0],
            SemanticRole::SideLeft,
            false,
        ),
        named(
            2,
            "edge-bottom",
            [0.0, 970.0, 500.0, 30.0],
            SemanticRole::PhysicalBottom,
            false,
        ),
        named(
            3,
            "drawer",
            [100.0, 100.0, 50.0, 50.0],
            SemanticRole::LocalControl,
            true,
        ),
    ];
    if keyboard == KeyboardState::Visible {
        regions.push(named(
            4,
            "keyboard",
            [0.0, 800.0, 500.0, 200.0],
            SemanticRole::Keyboard,
            true,
        ));
    }
    let known = keyboard != KeyboardState::Unknown;
    let context = Context {
        version: 1,
        epoch: "a".repeat(32),
        ui_generation: 2,
        revision: 7,
        sampled_ns: at.as_nanos().try_into().unwrap(),
        coverage: Coverage::PrimarySurfaceNamedBounds,
        frame: Some(Frame {
            width: 500,
            height: 1000,
            scale_120: 240,
            regions,
            surface: Some(SurfaceSpace {
                namespace: "shell".into(),
                width: 500,
                height: 1000,
                local_scale: 1.0,
                mapped_ns: Some(1),
            }),
            keyboard: Some(KeyboardRegistry {
                scope: RegistryScope::RegisteredMorfPrimary,
                registered: 1,
                state: keyboard,
                node: known.then(|| "4".into()),
                layout: known.then(|| "full".into()),
            }),
            input: Some(vec![InputRect {
                x: 100,
                y: 100,
                width: 50,
                height: 50,
            }]),
        }),
    };
    let inventory = Inventory {
        layers: vec![Layer {
            output: "phone".into(),
            address: "0x123".into(),
            namespace: "shell".into(),
            pid: Some(4321),
            level: 2,
            rect: rect(0.0, 0.0, 500.0, 1000.0),
        }],
        map_in: Some(Duration::ZERO),
    };
    let layout = LayoutProjection::from_samples(
        &context,
        output,
        &inventory,
        &inventory,
        (4321, 999),
        (at, at),
    )
    .unwrap();
    OutputContext {
        output: "phone".into(),
        context,
        layout,
    }
}

#[test]
fn a_settled_shell_replaces_its_coarse_layer_with_input_and_semantic_scopes() {
    let at = Duration::from_secs(1);
    let mut snapshot = coarse(phone(), at);
    let view = shell(&snapshot.outputs[0], at, KeyboardState::Hidden);
    assert!(view.layout.as_ref().unwrap().settled());
    apply(&mut snapshot, std::slice::from_ref(&view), at);
    snapshot.validate().unwrap();
    let ids: Vec<_> = snapshot.regions.iter().map(|r| r.id.as_str()).collect();
    let epoch = "a".repeat(32);
    assert_eq!(
        ids,
        [
            format!("morf:{epoch}:input:0"),
            format!("morf:{epoch}:node:1"),
            format!("morf:{epoch}:node:2"),
            format!("morf:{epoch}:node:3"),
        ]
    );
    let scopes: Vec<_> = snapshot
        .regions
        .iter()
        .map(|r| (r.scope, r.interactive, r.rect))
        .collect();
    assert_eq!(
        scopes,
        [
            (None, true, rect(100.0, 100.0, 50.0, 50.0)),
            (Some(Scope::SideLeft), false, rect(0.0, 30.0, 8.0, 740.0)),
            (
                Some(Scope::PhysicalBottom),
                false,
                rect(0.0, 970.0, 500.0, 30.0)
            ),
            (None, true, rect(100.0, 100.0, 50.0, 50.0)),
        ]
    );
    assert!(snapshot.outputs[0].keyboard_known);
    assert_eq!(snapshot.outputs[0].keyboard, None);
    assert!(!snapshot.secure_overlay);
}
#[test]
fn a_visible_registered_keyboard_is_known_only_on_an_exclusive_output() {
    let at = Duration::from_secs(1);
    let mut snapshot = coarse(phone(), at);
    let view = shell(&snapshot.outputs[0], at, KeyboardState::Visible);
    apply(&mut snapshot, std::slice::from_ref(&view), at);
    let output = &snapshot.outputs[0];
    assert!(output.keyboard_known);
    assert_eq!(
        output.keyboard,
        Some(Keyboard {
            rect: rect(0.0, 800.0, 500.0, 200.0),
            layout: "full".into(),
        })
    );
    let mut shared = view.clone();
    shared.layout.as_mut().unwrap().exclusive = false;
    let mut snapshot = coarse(phone(), at);
    apply(&mut snapshot, &[shared], at);
    assert!(!snapshot.outputs[0].keyboard_known);
    assert_eq!(snapshot.outputs[0].keyboard, None);
    assert!(snapshot.regions.iter().all(|r| r.id != COARSE));
    let mut snapshot = coarse(phone(), at);
    let unknown = shell(&snapshot.outputs[0], at, KeyboardState::Unknown);
    apply(&mut snapshot, &[unknown], at);
    assert!(!snapshot.outputs[0].keyboard_known);
}
#[test]
fn unsettled_undescribed_stale_or_unmatched_views_keep_the_coarse_layer() {
    let at = Duration::from_secs(1);
    let original = coarse(phone(), at);
    let view = shell(&original.outputs[0], at, KeyboardState::Hidden);
    let mut cases = Vec::new();
    let mut unsettled = view.clone();
    let layout = unsettled.layout.as_mut().unwrap();
    layout.basis = super::super::projection::Basis::StockLayerLayout;
    cases.push((unsettled, at));
    let mut undescribed = view.clone();
    undescribed.layout.as_mut().unwrap().input = None;
    cases.push((undescribed, at));
    cases.push((view.clone(), at + Duration::from_millis(250)));
    let mut retargeted = view.clone();
    retargeted.layout.as_mut().unwrap().surface.address = "0x456".into();
    cases.push((retargeted, at));
    let mut foreign = view.clone();
    foreign.output = "other".into();
    cases.push((foreign, at));
    for (case, now) in cases {
        let mut snapshot = original.clone();
        apply(&mut snapshot, &[case], now);
        assert_eq!(snapshot, original);
    }
}
#[test]
fn a_visible_secure_overlay_restricts_even_an_unsettled_frame() {
    let at = Duration::from_secs(1);
    let original = coarse(phone(), at);
    let mut view = shell(&original.outputs[0], at, KeyboardState::Hidden);
    view.layout = None;
    view.context.frame.as_mut().unwrap().regions.push(named(
        5,
        "auth",
        [0.0, 0.0, 500.0, 400.0],
        SemanticRole::SecureOverlay,
        true,
    ));
    let mut snapshot = original.clone();
    apply(&mut snapshot, &[view], at);
    assert!(snapshot.secure_overlay);
    assert_eq!(snapshot.regions, original.regions);
}
