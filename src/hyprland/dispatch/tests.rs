use super::*;
use crate::{context::Rect, gestures::Point};

fn target(id: &str) -> (Window, OutputState) {
    let rect = Rect {
        origin: Point { x: 20., y: 40. },
        size: Point { x: 300., y: 400. },
    };
    (
        Window {
            id: id.into(),
            output: "panel".into(),
            workspace: 3,
            rect,
            content: rect,
            mapped: true,
            visible: false,
            focused: false,
            z_order: None,
        },
        OutputState {
            geometry: crate::gestures::Output {
                name: "panel".into(),
                pixel_width: 1080.,
                pixel_height: 1920.,
                scale: 2.,
                origin: Point::default(),
                transform: Transform::Normal,
            },
            geometry_generation: 1,
            active_workspace: Some(3),
            screen: Screen::On,
            keyboard_known: false,
            keyboard: None,
        },
    )
}

#[test]
fn requests_accept_only_bounded_literal_targets_and_absolute_workspaces() {
    for dialect in [Dialect::Hyprlang, Dialect::Lua] {
        for id in [
            "active",
            "0x1",
            "0x1:0",
            "0x1:1;exit",
            "0x1:\"",
            "0x1:10000000000000000",
        ] {
            let (window, output) = target(id);
            assert!(dialect.window(&window, &output, None).is_err(), "{id}");
        }
        let (window, output) = target("0x1234:abc");
        for destination in [0, -1, i64::from(i32::MAX) + 1, i64::MAX] {
            assert!(dialect.window(&window, &output, Some(destination)).is_err());
        }
        for output in ["", "panel\"", "panel\\", "panel;exit", "panel\n", "pänel"] {
            assert!(dialect.power(output, Screen::Off).is_err());
        }
        for case in 0..19 {
            let (mut window, mut output) = target("0x1234:abc");
            match case {
                0 => window.mapped = false,
                1 => window.workspace = 0,
                2 => window.workspace = i64::MAX,
                3 => window.output = "other".into(),
                4 => window.rect.origin.x = f64::NAN,
                5 => window.rect.size.y = 0.,
                6 => {
                    output.geometry.name.clear();
                    window.output.clear();
                }
                7 => output.active_workspace = None,
                8 => output.active_workspace = Some(4),
                9 => output.screen = Screen::Off,
                10 => output.geometry_generation = 0,
                11 => output.geometry.pixel_width = f64::INFINITY,
                12 => output.geometry.pixel_height = -1.,
                13 => output.geometry.scale = 0.,
                14 => output.geometry.scale = f64::NAN,
                15 => output.geometry.origin.y = f64::INFINITY,
                16 => output.geometry.scale = f64::from(f32::MAX) * 2.,
                17 => output.geometry.scale = f64::from(f32::MIN_POSITIVE) * 1e-30,
                _ => {
                    output.geometry.name = "n".repeat(257);
                    window.output = output.geometry.name.clone();
                }
            }
            assert!(dialect.window(&window, &output, None).is_err(), "{case}");
        }
    }
    let (window, output) = target("0x1234:abc");
    assert_eq!(
        Dialect::Hyprlang.window(&window, &output, None).unwrap(),
        "/dispatch closewindow address:0x1234"
    );
    assert_eq!(
        Dialect::Hyprlang.window(&window, &output, Some(4)).unwrap(),
        "/dispatch movetoworkspace 4,address:0x1234"
    );
    assert_eq!(
        Dialect::Hyprlang.power("panel", Screen::Off).unwrap(),
        "/dispatch dpms off panel"
    );
    for value in [
        serde_json::Value::Null,
        serde_json::json!(true),
        serde_json::json!("other"),
    ] {
        assert!(Dialect::parse(&value).is_err());
    }
}

const MONITOR: &str = "{name='panel',x=0,y=0,width=1080,height=1920,scale=2,transform=0,dpms_status=true,active_workspace={id=3}}";
fn native_window(stable: &str) -> String {
    format!(
        "{{stable_id=0x{stable},mapped=true,workspace={{id=3}},at={{x=20,y=40}},size={{x=300,y=400}}}}"
    )
}
fn evaluate(request: &str, window: &str, monitor: &str, setup: &str, expected: bool) {
    let expression = request.strip_prefix("/dispatch ").unwrap();
    let script = format!(
        r#"
local factories,mutations=0,0
local monitor={monitor}
local window={window}
if window then window.monitor=monitor end
local function effect()
 factories=factories+1
 return function()mutations=mutations+1 end
end
local hl={{dsp={{window={{}}}}}}
hl.get_window=function(selector)assert(selector=='address:0x1234');return window end
hl.get_monitor=function(selector)assert(selector=='panel');return monitor end
hl.dsp.window.close=function(t)assert(t.window==window);return effect()end
hl.dsp.window.move=function(t)
 assert(t.window==window and t.follow==true and (t.workspace=='2' or t.workspace=='4'))
 return effect()
end
hl.dsp.dpms=function(t)assert(t.monitor==monitor and (t.action=='on' or t.action=='off'));return effect()end
hl.dispatch=function(f)assert(type(f)=='function');f()end
{setup}
local ok=pcall(function()hl.dispatch({expression})end)
assert(ok=={expected})
assert(factories=={count} and mutations=={count})
return require('prek').config.new{{}}
"#,
        count = usize::from(expected)
    );
    crate::lua::Definition::compile(&script, "typed-hyprland-dispatch.lua")
        .unwrap_or_else(|error| panic!("native dispatch setup={setup:?}: {error}"))
        .validate()
        .unwrap();
}

#[test]
fn lua_window_factories_keep_lifetime_and_explicit_follow_targets() {
    for stable in ["abc", "ffffffffffffffff"] {
        let (window, output) = target(&format!("0x1234:{stable}"));
        for destination in [None, Some(2), Some(4)] {
            let request = Dialect::Lua.window(&window, &output, destination).unwrap();
            evaluate(&request, &native_window(stable), MONITOR, "", true);
            evaluate(&request, "nil", MONITOR, "", false);
            evaluate(&request, &native_window("def"), MONITOR, "", false);
        }
    }
}

#[test]
fn native_window_changes_after_sampling_refuse_before_constructing_any_effect() {
    let (window, output) = target("0x1234:abc");
    for destination in [None, Some(2), Some(4)] {
        let request = Dialect::Lua.window(&window, &output, destination).unwrap();
        for setup in [
            "window.mapped=false",
            "window.workspace=nil",
            "window.workspace.id=4",
            "window.monitor=nil",
            "monitor.name='other'",
            "monitor.dpms_status=false",
            "monitor.active_workspace=nil",
            "monitor.active_workspace.id=4",
            "monitor.active_special_workspace={id=-99}",
            "monitor.x=1",
            "monitor.y=-1",
            "monitor.width=1920",
            "monitor.height=1080",
            "monitor.scale=2.5",
            "monitor.transform=1",
            "window.at.x=21",
            "window.at.y=41",
            "window.size.x=301",
            "window.size.y=401",
        ] {
            evaluate(&request, &native_window("abc"), MONITOR, setup, false);
        }
        evaluate(
            &request,
            &native_window("abc"),
            MONITOR,
            "window.focused=false;hl.get_active_window=function()error('focus must not select a target')end",
            true,
        );
    }
}

#[test]
fn native_window_geometry_literals_roundtrip_fractional_negative_and_transformed_targets() {
    for (index, transform) in [
        Transform::Normal,
        Transform::Rotate90,
        Transform::Rotate180,
        Transform::Rotate270,
        Transform::Flipped,
        Transform::Flipped90,
        Transform::Flipped180,
        Transform::Flipped270,
    ]
    .into_iter()
    .enumerate()
    {
        let (mut window, mut output) = target("0x1234:abc");
        window.rect.origin = Point { x: -125., y: 77. };
        output.geometry.origin = Point { x: -400., y: 20. };
        output.geometry.scale = 1.3333334;
        output.geometry.transform = transform;
        let request = Dialect::Lua.window(&window, &output, None).unwrap();
        evaluate(
            &request,
            &native_window("abc"),
            MONITOR,
            &format!(
                "window.at={{x=-125,y=77}};monitor.x=-400;monitor.y=20;monitor.scale=1.3333333730697632;monitor.transform={index}"
            ),
            true,
        );
    }
}

#[test]
fn lua_power_never_constructs_an_all_output_dispatch_for_a_missing_monitor() {
    for screen in [Screen::On, Screen::Off] {
        let request = Dialect::Lua.power("panel", screen).unwrap();
        evaluate(&request, "nil", "{name='panel'}", "", true);
        evaluate(&request, "nil", "nil", "", false);
        evaluate(&request, "nil", "{name='other'}", "", false);
    }
}

#[test]
fn native_output_names_are_exact_byte_literals_not_dispatch_programs() {
    for name in [
        "panel\"",
        "panel\\",
        "panel;exit",
        "panel\n",
        "pänel",
        "\0panel",
        "panel\";error('injected')--",
        "123",
    ] {
        let (mut window, mut output) = target("0x1234:abc");
        window.output = name.into();
        output.geometry.name = name.into();
        let request = Dialect::Lua.window(&window, &output, None).unwrap();
        let bytes = name
            .bytes()
            .map(|byte| byte.to_string())
            .collect::<Vec<_>>()
            .join(",");
        evaluate(
            &request,
            &native_window("abc"),
            MONITOR,
            &format!("monitor.name=string.char({bytes})"),
            true,
        );
    }
}

#[test]
fn native_scale_guards_recover_binary32_identity_without_accepting_neighboring_scales() {
    for native in [1.1f32, 4. / 3., 1.25, 1.7, 2.4, 3.17] {
        let (window, mut output) = target("0x1234:abc");
        output.geometry.scale = native.to_string().parse().unwrap();
        let request = Dialect::Lua.window(&window, &output, None).unwrap();
        evaluate(
            &request,
            &native_window("abc"),
            MONITOR,
            &format!("monitor.scale={:?}", f64::from(native)),
            true,
        );
        for changed in [
            f32::from_bits(native.to_bits() - 1),
            f32::from_bits(native.to_bits() + 1),
        ] {
            evaluate(
                &request,
                &native_window("abc"),
                MONITOR,
                &format!("monitor.scale={:?}", f64::from(changed)),
                false,
            );
        }
    }
}

fn scroll_workspace() -> crate::hyprland::layouts::Workspace {
    use crate::hyprland::layouts::{Column, Position, Window as Native, Workspace};
    let native = |id: &str, y: f64, index: usize| Native {
        id: id.into(),
        rect: [0., y, 540., 480.],
        hidden: false,
        floating: false,
        grouped: false,
        fullscreen: 0,
        column: Some(Position { index, row: 0 }),
    };
    Workspace {
        id: 3,
        output: Some("panel".into()),
        monitor: Some(0),
        active: true,
        special: false,
        layout: "scrolling".into(),
        windows: vec![
            native("0x1234:abc", -100., 0),
            native("0x5678:def", 380., 1),
        ],
        columns: ["0x1234:abc", "0x5678:def"]
            .into_iter()
            .enumerate()
            .map(|(index, id)| Column {
                index,
                width: 0.5,
                members: vec![id.into()],
            })
            .collect(),
    }
}
fn evaluate_scroll(request: &str, setup: &str, expected: bool) {
    let expression = request.strip_prefix("/dispatch ").unwrap();
    let script = format!(
        r#"
local factories,mutations=0,0
local monitor={{name='panel',id=0,focused=true,x=0,y=0,width=1080,height=1920,scale=2,transform=0,dpms_status=true}}
local ws={{id=3,active=true,special=false,tiled_layout='scrolling',monitor=monitor}}
monitor.active_workspace=ws
local columns={{}}
local function window(address,stable,y,index)
 local col=columns[index+1] or {{index=index,width=0.5,windows={{}}}}
 columns[index+1]=col
 local o={{address=address,stable_id=stable,mapped=true,workspace=ws,monitor=monitor,
  at={{x=0,y=y}},size={{x=540,y=480}},hidden=false,floating=false,fullscreen=0,
  layout={{column=col,index_in_column=0}}}}
 col.windows[#col.windows+1]=o
 return o
end
local windows={{window('0x1234',0xabc,-100,0),window('0x5678',0xdef,380,1)}}
local hl={{dsp={{}}}}
hl.get_active_monitor=function()return monitor end
hl.get_workspace=function(id)assert(id==3);return ws end
hl.get_workspace_windows=function(w)assert(w==ws);return windows end
hl.dsp.layout=function(message)
 assert(message=='move -12.5')
 factories=factories+1
 return function()mutations=mutations+1 end
end
hl.dispatch=function(f)assert(type(f)=='function');f()end
{setup}
local ok=pcall(function()hl.dispatch({expression})end)
assert(ok=={expected})
assert(factories=={count} and mutations=={count})
return require('prek').config.new{{}}
"#,
        count = usize::from(expected)
    );
    crate::lua::Definition::compile(&script, "typed-hyprland-scroll.lua")
        .unwrap_or_else(|error| panic!("native scroll setup={setup:?}: {error}"))
        .validate()
        .unwrap();
}

#[test]
fn a_numeric_scroll_moves_only_the_focused_frozen_workspace_layout() {
    let (_, output) = target("0x1234:abc");
    let request = Dialect::Lua
        .scroll(&output, &scroll_workspace(), -12.5)
        .unwrap();
    evaluate_scroll(&request, "", true);
    for setup in [
        "monitor.focused=false",
        "monitor.name='other'",
        "monitor.scale=1",
        "monitor.dpms_status=false",
        "monitor.active_special_workspace={id=-98}",
        "ws.tiled_layout='dwindle'",
        "ws.special=true",
        "windows[1].at.y=-99",
        "windows[1].stable_id=0x1",
        "windows[2]=nil",
        "columns[1].width=0.75",
        "windows[2].floating=true",
    ] {
        evaluate_scroll(&request, setup, false);
    }
}

#[test]
fn numeric_scrolls_require_lua_a_finite_move_and_the_active_output_workspace() {
    let (_, output) = target("0x1234:abc");
    let workspace = scroll_workspace();
    assert!(Dialect::Hyprlang.scroll(&output, &workspace, 10.).is_err());
    for distance in [0., f64::NAN, f64::INFINITY, 2_000_000.] {
        assert!(Dialect::Lua.scroll(&output, &workspace, distance).is_err());
    }
    let mut inactive = workspace.clone();
    inactive.active = false;
    let mut elsewhere = workspace.clone();
    elsewhere.output = Some("other".into());
    let mut selector = workspace.clone();
    selector.windows[0].id = "active".into();
    for workspace in [inactive, elsewhere, selector] {
        assert!(Dialect::Lua.scroll(&output, &workspace, 10.).is_err());
    }
    let mut dark = output.clone();
    dark.screen = Screen::Off;
    assert!(Dialect::Lua.scroll(&dark, &workspace, 10.).is_err());
}
