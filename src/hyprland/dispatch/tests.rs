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
