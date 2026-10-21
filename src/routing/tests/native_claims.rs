use super::*;
fn configured() -> (
    Routing,
    Device,
    Snapshot,
    crate::hyprland::layouts::Snapshot,
) {
    let mut device = device();
    device.axes.clear();
    device.properties.clear();
    device.capabilities = BTreeMap::from([(EV_SYN, vec![SYN_REPORT]), (EV_KEY, vec![30])]);
    let mut cfg = config();
    cfg.policy.allow_hyprland_actions = true;
    let rule = &mut cfg.rules[0];
    rule.trigger = Trigger::Key {
        key: crate::config::Key(30),
    };
    rule.delivery = Delivery::Reserve;
    rule.output = Some("panel".into());
    rule.action = Action::Hyprland {
        action: crate::config::WindowAction::ReorderUp,
    };
    let mut routing = Routing::new(cfg).unwrap();
    context(&mut routing, 0);
    let (mut core, mut native) = crate::hyprland::reorder::tests::fixture(true);
    core.generation = routing.authority().generation();
    core.revision = 2;
    core.captured_at = ms(0);
    core.windows[1].focused = true;
    native.generation = core.generation;
    native.revision = 2;
    native.captured_at = ms(0);
    routing
        .authority_mut()
        .update_native(core.clone(), Some(native.clone()), ms(0))
        .unwrap();
    routing
        .attach(device.clone(), &ready(&device), None, ms(0))
        .unwrap();
    refresh(&mut routing);
    (routing, device, core, native)
}
fn refresh(routing: &mut Routing) {
    let generation = routing.authority().generation();
    routing.actions_available(
        generation,
        BTreeSet::from([Capability::HyprlandActions]),
        ms(0),
    );
}
fn edge(routing: &mut Routing, device: &Device, sequence: u64, value: i32) {
    routing
        .push(Record::Frame {
            frame: Frame {
                source: device.source.clone(),
                at: ms(sequence - 1),
                sequence,
                events: vec![
                    RawEvent {
                        kind: EV_KEY,
                        code: 30,
                        value,
                    },
                    RawEvent {
                        kind: EV_SYN,
                        code: SYN_REPORT,
                        value: 0,
                    },
                ],
            },
        })
        .unwrap();
}
#[test]
fn native_claim_is_frozen_before_dispatch_and_cannot_resurrect_after_neighbor_or_provider_loss() {
    for case in [
        "refresh",
        "neighbor",
        "loss-restored",
        "capability-restored",
    ] {
        let (mut routing, device, mut core, mut native) = configured();
        let original = routing.authority().reorder_scope("0x2:2", ms(0)).unwrap();
        edge(&mut routing, &device, 1, 1);
        assert!(drain(&mut routing, 0).is_empty());
        if case == "loss-restored" {
            core.revision += 1;
            routing.authority_mut().update(core.clone(), ms(0)).unwrap();
            refresh(&mut routing);
        }
        if case == "capability-restored" {
            routing.restrict_actions(&BTreeSet::new());
            refresh(&mut routing);
        }
        if case == "neighbor" {
            core.windows[0].id = "0x9:9".into();
            native.workspaces[0].windows[0].id = "0x9:9".into();
            native.workspaces[0].columns[0].members[0] = "0x9:9".into();
        }
        core.revision += 1;
        native.revision = core.revision;
        routing
            .authority_mut()
            .update_native(core.clone(), Some(native), ms(0))
            .unwrap();
        refresh(&mut routing);
        let dispatches = routing.take_scoped_dispatches(ms(0));
        if case == "refresh" {
            assert_eq!(dispatches.len(), 1);
            assert_eq!(dispatches[0].reorder_scope, Some(original.clone()));
        } else {
            assert!(dispatches.is_empty(), "{case} retargeted an owned gesture");
            if case != "neighbor" {
                assert_eq!(
                    routing.authority().reorder_scope("0x2:2", ms(0)),
                    Some(original.clone())
                );
            }
        }
        edge(&mut routing, &device, 2, 0);
        assert!(drain(&mut routing, 1).is_empty());
        let release = routing.take_scoped_dispatches(ms(1));
        if case == "refresh" {
            assert_eq!(release.len(), 1);
            assert_eq!(release[0].reorder_scope, Some(original));
            assert!(matches!(
                release[0].event,
                Event::Matched {
                    stage: crate::rules::Stage::End,
                    ..
                }
            ));
        } else {
            assert!(release.is_empty(), "{case} restored a retired release");
        }
    }
}

/// A G11-style continuous one-finger swipe on a left side strip, over a
/// validated phone workspace of three half-height strips scrolled by 300.
fn side_strip(width: f64) -> (Routing, Option<crate::protocol::ReorderScope>) {
    use crate::{
        context::{Rect, Region, Window},
        hyprland::{
            layouts,
            scrolling::profile::{Reserved, Settings},
        },
    };
    let mut cfg = config();
    cfg.policy.allow_hyprland_actions = true;
    let rule = &mut cfg.rules[0];
    rule.scope = crate::config::Scope::SideLeft;
    rule.delivery = Delivery::Reserve;
    rule.trigger = Trigger::Swipe {
        fingers: 1,
        direction: Direction::Vertical,
        travel: crate::config::Distance::Millimetres(12.0),
        fallback_travel: crate::config::Distance::Normalized(0.04),
        continuous: true,
    };
    rule.action = Action::Hyprland {
        action: crate::config::WindowAction::ScrollStack,
    };
    let mut routing = Routing::new(cfg).unwrap();
    context(&mut routing, 0);
    let id = |i: usize| format!("0x{:x}:{:x}", i + 1, i + 1);
    let rect = |i: usize| Rect {
        origin: Point {
            x: 0.,
            y: 600. * i as f64 - 300.,
        },
        size: Point { x: 600., y: 600. },
    };
    let windows: Vec<_> = (0..3)
        .map(|i| Window {
            id: id(i),
            output: "test".into(),
            workspace: 1,
            rect: rect(i),
            content: rect(i),
            mapped: true,
            visible: true,
            focused: i == 1,
            z_order: None,
        })
        .collect();
    let core = Snapshot {
        generation: routing.authority().generation(),
        revision: 2,
        captured_at: ms(0),
        state: State::Desktop,
        outputs: vec![OutputState {
            geometry: output(),
            geometry_generation: 2,
            active_workspace: Some(1),
            screen: Screen::On,
            keyboard_known: true,
            keyboard: None,
        }],
        windows,
        regions: vec![Region {
            id: "strip".into(),
            scope: Some(crate::config::Scope::SideLeft),
            output: "test".into(),
            rect: Rect {
                origin: Point { x: 0., y: 0. },
                size: Point { x: 30., y: 1200. },
            },
            interactive: false,
        }],
        secure_overlay: false,
    };
    let native = layouts::Snapshot {
        generation: core.generation,
        revision: 2,
        continuity: 2,
        captured_at: ms(0),
        workspaces: vec![layouts::Workspace {
            id: 1,
            output: Some("test".into()),
            monitor: Some(0),
            active: true,
            special: false,
            layout: "scrolling".into(),
            windows: (0..3)
                .map(|i| layouts::Window {
                    id: id(i),
                    rect: [0., 600. * i as f64 - 300., 600., 600.],
                    hidden: false,
                    floating: false,
                    grouped: false,
                    fullscreen: 0,
                    column: Some(layouts::Position { index: i, row: 0 }),
                })
                .collect(),
            columns: (0..3)
                .map(|i| layouts::Column {
                    index: i,
                    width,
                    members: vec![id(i)],
                })
                .collect(),
        }],
        profile: Some(Settings {
            direction: "down".into(),
            gaps_in: [0; 4],
            gaps_out: [0; 4],
            border: 0,
            focus_fit: 1,
            workspace_rules: 0,
            reserved: vec![Reserved {
                output: "test".into(),
                area: [0.; 4],
            }],
        }),
    };
    routing
        .authority_mut()
        .update_native(core, Some(native), ms(0))
        .unwrap();
    let device = device();
    let mut mapping = Mapping::from_device(&device).unwrap();
    mapping.output = Some(output());
    mapping.generation = 2;
    routing
        .attach(device.clone(), &ready(&device), Some(mapping), ms(0))
        .unwrap();
    refresh(&mut routing);
    let scope = routing.authority().native_scope(
        &crate::routing::native::Target::Workspace("test".into(), 1),
        ms(0),
    );
    (routing, scope)
}
fn touch(routing: &mut Routing, sequence: u64, at: u64, x: i32, y: i32) {
    frame(
        routing,
        sequence,
        at,
        &[
            (EV_KEY, 0x14a, 1),
            (EV_ABS, ABS_MT_SLOT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, 10),
            (EV_ABS, ABS_MT_POSITION_X, x),
            (EV_ABS, ABS_MT_POSITION_Y, y),
        ],
    );
}

#[test]
fn a_side_strip_swipe_freezes_the_scrolled_workspace_proof_for_every_stage() {
    let (mut routing, scope) = side_strip(0.5);
    let scope = scope.unwrap();
    touch(&mut routing, 1, 0, 40, 1000);
    movement(&mut routing, 2, 40, 1300, 1);
    movement(&mut routing, 3, 80, 1600, 1);
    up(&mut routing, 4, 120, 1);
    assert!(drain(&mut routing, 120).is_empty());
    let dispatches = routing.take_scoped_dispatches(ms(120));
    assert!(dispatches.len() >= 2, "{}", dispatches.len());
    for dispatch in &dispatches {
        assert_eq!(dispatch.reorder_scope.as_ref(), Some(&scope));
        assert!(matches!(
            &dispatch.event,
            Event::Matched {
                action: Action::Hyprland {
                    action: crate::config::WindowAction::ScrollStack
                },
                window: None,
                output: Some(output),
                workspace: Some(1),
                ..
            } if output == "test"
        ));
    }
    assert!(matches!(
        dispatches.first().unwrap().event,
        Event::Matched {
            stage: crate::rules::Stage::Begin,
            ..
        }
    ));
    assert!(matches!(
        dispatches.last().unwrap().event,
        Event::Matched {
            stage: crate::rules::Stage::End,
            ..
        }
    ));
}

#[test]
fn a_swipe_outside_the_strip_or_off_the_profile_claims_nothing_native() {
    for case in ["outside", "wide"] {
        let (mut routing, scope) = side_strip(if case == "wide" { 0.75 } else { 0.5 });
        assert_eq!(scope.is_none(), case == "wide");
        let x = if case == "outside" { 2000 } else { 40 };
        touch(&mut routing, 1, 0, x, 1000);
        let mut delivered = drain(&mut routing, 0).len();
        routing.push(Record::Tick { at: ms(130) }).unwrap();
        delivered += drain(&mut routing, 130).len();
        movement(&mut routing, 2, 140, 1300, 1);
        delivered += drain(&mut routing, 140).len();
        up(&mut routing, 3, 180, 1);
        delivered += drain(&mut routing, 180).len();
        assert_eq!(delivered, 3, "{case}");
        assert!(routing.take_scoped_dispatches(ms(180)).is_empty(), "{case}");
    }
}
