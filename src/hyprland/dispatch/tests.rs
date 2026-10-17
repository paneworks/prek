use super::*;

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
            assert!(dialect.window(id, None).is_err(), "{id}");
        }
        for destination in [0, -1, i64::from(i32::MAX) + 1, i64::MAX] {
            assert!(dialect.window("0x1234:abc", Some(destination)).is_err());
        }
        for output in ["", "panel\"", "panel\\", "panel;exit", "panel\n", "pänel"] {
            assert!(dialect.power(output, Screen::Off).is_err());
        }
    }
    assert_eq!(
        Dialect::Hyprlang.window("0x1234:abc", None).unwrap(),
        "/dispatch closewindow address:0x1234"
    );
    assert_eq!(
        Dialect::Hyprlang.window("0x1234:abc", Some(4)).unwrap(),
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

fn evaluate(request: &str, window: &str, monitor: &str, expected: bool) {
    let expression = request.strip_prefix("/dispatch ").unwrap();
    let script = format!(
        r#"
local factories,mutations=0,0
local window={window}
local monitor={monitor}
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
local ok=pcall(function()hl.dispatch({expression})end)
assert(ok=={expected})
assert(factories=={count} and mutations=={count})
return require('prek').config.new{{}}
"#,
        count = usize::from(expected)
    );
    crate::lua::Definition::compile(&script, "typed-hyprland-dispatch.lua")
        .unwrap()
        .validate()
        .unwrap();
}

#[test]
fn lua_window_factories_keep_lifetime_and_explicit_follow_targets() {
    for stable in ["abc", "ffffffffffffffff"] {
        for destination in [None, Some(2), Some(4)] {
            let request = Dialect::Lua
                .window(&format!("0x1234:{stable}"), destination)
                .unwrap();
            evaluate(&request, &format!("{{stable_id=0x{stable}}}"), "nil", true);
            evaluate(&request, "nil", "nil", false);
            evaluate(&request, "{stable_id=0xdef}", "nil", false);
        }
    }
}

#[test]
fn lua_power_never_constructs_an_all_output_dispatch_for_a_missing_monitor() {
    for screen in [Screen::On, Screen::Off] {
        let request = Dialect::Lua.power("panel", screen).unwrap();
        evaluate(&request, "nil", "{name='panel'}", true);
        evaluate(&request, "nil", "nil", false);
        evaluate(&request, "nil", "{name='other'}", false);
    }
}
