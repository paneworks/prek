use super::*;
use crate::{
    config::Screen,
    context::{Rect, State},
    gestures::{Output, Point, Transform},
};
use std::time::Duration;

pub(crate) fn fixture(down: bool) -> (Snapshot, layouts::Snapshot) {
    let windows: Vec<_> = (0..3)
        .map(|index| {
            let rect = Rect {
                origin: Point {
                    x: 0.,
                    y: 20. + f64::from(if down { index } else { 2 - index }) * 100.,
                },
                size: Point { x: 100., y: 80. },
            };
            Window {
                id: format!("0x{:x}:{:x}", index + 1, index + 1),
                output: "panel".into(),
                workspace: 1,
                rect,
                content: rect,
                mapped: true,
                visible: true,
                focused: false,
                z_order: None,
            }
        })
        .collect();
    let output = OutputState {
        geometry: Output {
            name: "panel".into(),
            pixel_width: 1000.,
            pixel_height: 800.,
            origin: Point::default(),
            scale: 1.,
            transform: Transform::Normal,
        },
        geometry_generation: 2,
        active_workspace: Some(1),
        screen: Screen::On,
        keyboard_known: true,
        keyboard: None,
    };
    let native = layouts::Snapshot {
        generation: 1,
        revision: 1,
        continuity: 1,
        captured_at: Duration::from_secs(1),
        workspaces: vec![layouts::Workspace {
            id: 1,
            output: Some("panel".into()),
            monitor: Some(0),
            active: true,
            special: false,
            layout: "scrolling".into(),
            windows: windows
                .iter()
                .enumerate()
                .map(|(index, w)| layouts::Window {
                    id: w.id.clone(),
                    rect: [
                        w.rect.origin.x,
                        w.rect.origin.y,
                        w.rect.size.x,
                        w.rect.size.y,
                    ],
                    hidden: false,
                    floating: false,
                    grouped: false,
                    fullscreen: 0,
                    column: Some(layouts::Position { index, row: 0 }),
                })
                .collect(),
            columns: windows
                .iter()
                .enumerate()
                .map(|(index, w)| layouts::Column {
                    index,
                    width: 0.5,
                    members: vec![w.id.clone()],
                })
                .collect(),
        }],
    };
    let core = Snapshot {
        generation: 1,
        revision: 1,
        captured_at: native.captured_at,
        state: State::Desktop,
        outputs: vec![output],
        windows,
        regions: vec![],
        secure_overlay: false,
    };
    native.validate_context(&core).unwrap();
    (core, native)
}
fn permute(core: &mut Snapshot, native: &mut layouts::Snapshot, a: usize, b: usize) {
    let first = core.windows[a].rect;
    core.windows[a].rect = core.windows[b].rect;
    core.windows[a].content = core.windows[b].content;
    core.windows[b].rect = first;
    core.windows[b].content = first;
    let ws = &mut native.workspaces[0];
    let first = ws.windows[a].clone();
    let second = ws.windows[b].clone();
    ws.windows[a].rect = second.rect;
    ws.windows[a].column = second.column;
    ws.windows[b].rect = first.rect;
    ws.windows[b].column = first.column;
    for col in &mut ws.columns {
        for id in &mut col.members {
            if *id == first.id {
                *id = second.id.clone();
            } else if *id == second.id {
                *id = first.id.clone();
            }
        }
    }
    native.validate_context(core).unwrap();
}
#[test]
fn vertical_reorder_freezes_exact_adjacent_pair_in_both_native_directions() {
    for down in [true, false] {
        for action in [WindowAction::ReorderUp, WindowAction::ReorderDown] {
            let (mut core, mut native) = fixture(down);
            let plan = Plan::freeze(&core, &native, "0x2:2", action).unwrap();
            let neighbor = if (action == WindowAction::ReorderUp) == down {
                0
            } else {
                2
            };
            assert_eq!(plan.neighbor, core.windows[neighbor].id);
            assert!(plan.matches(&core, &native));
            assert!(!plan.confirmed(&core, &native));
            core.windows[0].focused = true;
            assert!(plan.matches(&core, &native));
            native.workspaces[0].windows.reverse();
            assert!(plan.matches(&core, &native));
            native.workspaces[0].windows.reverse();
            permute(&mut core, &mut native, 1, neighbor);
            assert!(!plan.matches(&core, &native));
            assert!(plan.confirmed(&core, &native));
            assert!(core.windows.iter().all(|window| window.workspace == 1));
        }
    }
}
#[test]
fn reorder_boundaries_horizontal_or_missing_graph_and_unsafe_apps_refuse() {
    for down in [true, false] {
        let (core, native) = fixture(down);
        let top = if down { "0x1:1" } else { "0x3:3" };
        let bottom = if down { "0x3:3" } else { "0x1:1" };
        assert!(Plan::freeze(&core, &native, top, WindowAction::ReorderUp).is_err());
        assert!(Plan::freeze(&core, &native, bottom, WindowAction::ReorderDown).is_err());
    }
    for case in 0..12 {
        let (mut core, mut native) = fixture(true);
        match case {
            0 => native.revision += 1,
            1 => core.secure_overlay = true,
            2 => core.outputs[0].screen = Screen::Off,
            3 => core.outputs[0].active_workspace = Some(2),
            4 => native.workspaces[0].windows[1].hidden = true,
            5 => native.workspaces[0].windows[0].grouped = true,
            6 => native.workspaces[0].windows[0].fullscreen = 1,
            7 => native.workspaces[0].windows[1].floating = true,
            8 => native.workspaces.clear(),
            9 => {
                for (i, window) in native.workspaces[0].windows.iter_mut().enumerate() {
                    window.rect[0] = i as f64 * 100.;
                    window.rect[1] = 0.;
                    core.windows[i].rect.origin = Point {
                        x: window.rect[0],
                        y: 0.,
                    };
                    core.windows[i].content = core.windows[i].rect;
                }
            }
            10 => {
                native.workspaces[0].columns.clear();
                native.workspaces[0].layout = "dwindle".into();
                for window in &mut native.workspaces[0].windows {
                    window.column = None;
                }
            }
            _ => native.workspaces[0].windows[0].id = "0x1:99".into(),
        }
        assert!(
            Plan::freeze(&core, &native, "0x2:2", WindowAction::ReorderUp).is_err(),
            "{case}"
        );
    }
}
#[test]
fn multiple_adjacent_members_require_unique_horizontal_alignment() {
    let (mut core, mut native) = fixture(true);
    let mut extra = core.windows[0].clone();
    extra.id = "0x4:4".into();
    extra.rect.origin.x = 100.;
    extra.content = extra.rect;
    core.windows.push(extra);
    let ws = &mut native.workspaces[0];
    let mut extra = ws.windows[0].clone();
    extra.id = "0x4:4".into();
    extra.rect[0] = 100.;
    extra.column.as_mut().unwrap().row = 1;
    ws.windows.push(extra);
    ws.columns[0].members.push("0x4:4".into());
    for (x, expected) in [(20., true), (50., false), (300., false)] {
        core.windows[1].rect.origin.x = x;
        core.windows[1].content = core.windows[1].rect;
        native.workspaces[0].windows[1].rect[0] = x;
        native.validate_context(&core).unwrap();
        let plan = Plan::freeze(&core, &native, "0x2:2", WindowAction::ReorderUp);
        if expected {
            assert_eq!(plan.unwrap().neighbor, "0x1:1");
        } else {
            assert!(plan.is_err());
        }
    }
}
#[test]
fn unrelated_topology_geometry_or_lifetime_change_cannot_confirm_a_swap() {
    let (core, native) = fixture(true);
    let plan = Plan::freeze(&core, &native, "0x2:2", WindowAction::ReorderUp).unwrap();
    for case in 0..6 {
        let (mut core, mut native) = (core.clone(), native.clone());
        permute(&mut core, &mut native, 0, 1);
        match case {
            0 => native.workspaces[0].windows[2].rect[0] += 1.,
            1 => native.workspaces[0].columns[2].width = 0.75,
            2 => native.workspaces[0].windows[2].hidden = true,
            3 => core.outputs[0].geometry.origin.x += 1.,
            4 => native.workspaces[0].monitor = Some(2),
            _ => permute(&mut core, &mut native, 1, 2),
        }
        assert!(!plan.confirmed(&core, &native), "{case}");
    }
}

fn evaluate(setup: &str, expected: bool) {
    let (core, native) = fixture(true);
    let plan = Plan::freeze(&core, &native, "0x2:2", WindowAction::ReorderUp).unwrap();
    assert!(
        super::super::dispatch::Dialect::Hyprlang
            .reorder(&plan)
            .is_err()
    );
    let request = super::super::dispatch::Dialect::Lua.reorder(&plan).unwrap();
    let expression = request.strip_prefix("/dispatch ").unwrap();
    let script = format!(
        r#"
local factories,effects=0,0
local m={{id=0,name='panel',x=0,y=0,width=1000,height=800,scale=1,transform=0,dpms_status=true,active_workspace={{id=1}}}}
local ws={{id=1,monitor=m,active=true,special=false,tiled_layout='scrolling'}}
local windows={{}}
for index=1,3 do
 local o={{address='0x'..index,stable_id=index,mapped=true,monitor=m,workspace=ws,at={{x=0,y=20+(index-1)*100}},size={{x=100,y=80}},hidden=false,floating=false,fullscreen=0}}
 o.layout={{column={{index=index-1,width=0.5,windows={{o}}}},index_in_column=0}}
 windows[index]=o
end
local hl={{dsp={{window={{}}}}}}
hl.get_window=function(selector)for _,o in ipairs(windows)do if 'address:'..o.address==selector then return o end end end
hl.get_workspace=function(id)assert(id==1);return ws end
hl.get_workspace_windows=function(w)assert(w==ws);return windows end
hl.dsp.window.swap=function(t)
 assert(t.window==windows[2] and t.target==windows[1]);factories=factories+1
 return function()effects=effects+1 end
end
{setup}
local ok=pcall(function()local dispatch={expression};assert(type(dispatch)=='function');dispatch()end)
assert(ok=={expected} and factories=={count} and effects=={count})
return require('prek').config.new{{}}
"#,
        count = usize::from(expected)
    );
    crate::lua::Definition::compile(&script, "native-reorder-factory.lua")
        .unwrap_or_else(|error| panic!("setup={setup:?}: {error}"));
}
#[test]
fn native_reorder_factory_checks_whole_graph_before_constructing_one_explicit_effect() {
    evaluate("", true);
    for setup in [
        "windows[1].stable_id=99",
        "windows[2].stable_id=99",
        "windows[3].stable_id=99",
        "windows[1].at.y=21",
        "windows[3].size.y=81",
        "windows[1].hidden=true",
        "windows[1].group={}",
        "windows[2].fullscreen=1",
        "windows[3].floating=true",
        "windows[1].layout.column.index=2",
        "windows[3].layout.index_in_column=1",
        "windows[3].layout.column.width=0.75",
        "windows[3].layout.column.windows={windows[2]}",
        "windows[3].mapped=false",
        "windows[3].workspace={id=2}",
        "windows[3].monitor={id=3}",
        "windows[3]=windows[1]",
        "windows[4]=windows[1]",
        "windows[3]=nil",
        "ws.tiled_layout='dwindle'",
        "ws.active=false",
        "ws.special=true",
        "m.id=2",
    ] {
        evaluate(setup, false);
    }
}

#[test]
fn native_authority_is_bound_to_one_context_sample_and_does_not_survive_replacement() {
    let (mut core, mut native) = fixture(true);
    let binding = crate::context::Binding {
        seat: "seat0".into(),
        session: "model".into(),
        uid: 1000,
        compositor_pid: 1,
        compositor_start: 1,
        kind: crate::context::SessionKind::User,
    };
    let at = core.captured_at;
    let mut authority = crate::context::Authority::recorded(binding, core.clone(), at).unwrap();
    let action = crate::config::Action::Hyprland {
        action: WindowAction::ReorderUp,
    };
    let eligible = |authority: &crate::context::Authority| {
        authority.hyprland_target_available(
            &action,
            Scope::AppContent,
            Some("0x2:2"),
            Some("panel"),
            Some(1),
            at,
        )
    };
    assert!(!eligible(&authority));
    core.revision += 1;
    native.revision += 1;
    authority
        .update_native(core.clone(), Some(native.clone()), at)
        .unwrap();
    assert!(eligible(&authority));
    core.revision += 1;
    assert!(
        authority
            .update_native(core.clone(), Some(native.clone()), at)
            .is_err()
    );
    assert!(eligible(&authority));
    authority.update(core.clone(), at).unwrap();
    assert!(!eligible(&authority));
    core.revision += 1;
    native.revision = core.revision;
    authority
        .update_native(core.clone(), Some(native), at)
        .unwrap();
    assert!(eligible(&authority));
    authority.lock_requested();
    assert!(!eligible(&authority));
}

#[test]
fn graph_confirmation_accepts_native_camera_repositioning_but_not_another_permutation() {
    let (mut core, mut native) = fixture(true);
    let plan = Plan::freeze(&core, &native, "0x2:2", WindowAction::ReorderUp).unwrap();
    permute(&mut core, &mut native, 0, 1);
    for (window, layout) in core
        .windows
        .iter_mut()
        .zip(&mut native.workspaces[0].windows)
    {
        window.rect.origin.y -= 30.;
        window.content = window.rect;
        layout.rect[1] -= 30.;
    }
    assert!(plan.confirmed(&core, &native));
    permute(&mut core, &mut native, 1, 2);
    assert!(!plan.confirmed(&core, &native));
}

#[test]
fn claim_fingerprint_preserves_refresh_focus_order_and_signed_zero_but_binds_graph_lifetimes() {
    let (core, native) = fixture(true);
    let original = scope(&core, &native, "0x2:2").unwrap();
    let mut retired = native.clone();
    retired.continuity = 0;
    assert!(scope(&core, &retired, "0x2:2").is_none());
    retired.revision += 1;
    retired.continuity = retired.revision;
    let mut restored = core.clone();
    restored.revision = retired.revision;
    assert_ne!(scope(&restored, &retired, "0x2:2").unwrap(), original);
    for case in 0..6 {
        let (mut core, mut native) = (core.clone(), native.clone());
        match case {
            0 => {
                core.revision += 1;
                native.revision += 1;
                core.captured_at += Duration::from_millis(10);
                native.captured_at = core.captured_at;
            }
            1 => core.windows[1].focused = true,
            2 => native.workspaces[0].windows.reverse(),
            3 => {
                core.windows[1].rect.origin.x = -0.;
                core.windows[1].content.origin.x = -0.;
                native.workspaces[0].windows[1].rect[0] = -0.;
            }
            4 => core.outputs[0].geometry.origin.x = -0.,
            _ => native.workspaces.push(layouts::Workspace {
                id: 2,
                output: None,
                monitor: None,
                active: false,
                special: false,
                layout: "dwindle".into(),
                windows: vec![],
                columns: vec![],
            }),
        }
        assert_eq!(
            scope(&core, &native, "0x2:2").as_ref(),
            Some(&original),
            "{case}"
        );
    }
    for case in 0..7 {
        let (mut core, mut native) = (core.clone(), native.clone());
        match case {
            0 => {
                core.windows[0].id = "0x9:9".into();
                native.workspaces[0].windows[0].id = "0x9:9".into();
                native.workspaces[0].columns[0].members[0] = "0x9:9".into();
            }
            1 => native.workspaces[0].columns[2].width = 0.75,
            2 => {
                core.windows[2].rect.origin.x += 1.;
                core.windows[2].content = core.windows[2].rect;
                native.workspaces[0].windows[2].rect[0] += 1.;
            }
            3 => core.outputs[0].geometry_generation += 1,
            4 => native.workspaces[0].monitor = Some(7),
            5 => {
                core.generation += 1;
                native.generation += 1;
            }
            _ => native.workspaces[0].windows[2].grouped = true,
        }
        assert_ne!(
            scope(&core, &native, "0x2:2").as_ref(),
            Some(&original),
            "{case}"
        );
    }
    let encoded = serde_json::to_vec(&original).unwrap();
    assert!(encoded.len() < 192);
    assert_eq!(
        serde_json::from_slice::<crate::protocol::ReorderScope>(&encoded).unwrap(),
        original
    );
}
