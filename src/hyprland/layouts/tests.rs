use super::*;
use serde_json::{Value, json};

fn document() -> Value {
    json!({"schema":1,"workspaces":[{
        "id":1,"output":"panel","monitor":0,"active":true,"special":false,
        "layout":"scrolling","windows":[{
            "id":"0x1234:ffffffffffffffff","rect":[0,20,500,700],"hidden":false,
            "floating":false,"grouped":false,"fullscreen":0,"column":{"index":0,"row":0}
        }],"columns":[{"index":0,"width":0.5,"members":["0x1234:ffffffffffffffff"]}]
    }]})
}
fn decode(value: &Value) -> io::Result<Vec<Workspace>> {
    parse(&serde_json::to_vec(value).unwrap())
}

#[test]
fn strict_layout_graph_retains_native_membership_and_lifetime() {
    let workspaces = decode(&document()).unwrap();
    assert_eq!(
        workspaces[0].columns[0].members[0],
        "0x1234:ffffffffffffffff"
    );
    assert_eq!(
        workspaces[0].windows[0].column,
        Some(Position { index: 0, row: 0 })
    );
    for case in 0..18 {
        let mut changed = document();
        match case {
            0 => changed["schema"] = json!(2),
            1 => changed["extra"] = json!(true),
            2 => changed["workspaces"][0]["id"] = json!(0),
            3 => changed["workspaces"][0]["output"] = Value::Null,
            4 => changed["workspaces"][0]["monitor"] = json!(-1),
            5 => changed["workspaces"][0]["layout"] = json!("dwindle"),
            6 => changed["workspaces"][0]["windows"][0]["id"] = json!("active"),
            7 => changed["workspaces"][0]["windows"][0]["rect"][2] = json!(0),
            8 => changed["workspaces"][0]["windows"][0]["floating"] = json!(true),
            9 => changed["workspaces"][0]["windows"][0]["column"]["row"] = json!(1),
            10 => changed["workspaces"][0]["columns"][0]["index"] = json!(1),
            11 => changed["workspaces"][0]["columns"][0]["members"] = json!([]),
            12 => changed["workspaces"][0]["columns"][0]["width"] = json!(0),
            13 => changed["workspaces"][0]["columns"][0]["width"] = json!(17),
            14 => changed["workspaces"][0]["columns"][0]["members"]
                .as_array_mut()
                .unwrap()
                .push(json!("0x1234:ffffffffffffffff")),
            15 => changed["workspaces"][0]["windows"]
                .as_array_mut()
                .unwrap()
                .push(document()["workspaces"][0]["windows"][0].clone()),
            16 => changed["workspaces"][0]["windows"][0]["fullscreen"] = json!(4),
            _ => changed["workspaces"][0]["output"] = json!("bad\nname"),
        }
        assert!(decode(&changed).is_err(), "case {case}");
    }
}

#[test]
fn native_inventory_bounds_cover_cross_workspace_identity_and_active_scope() {
    let original = document();
    for case in 0..3 {
        let mut changed = original.clone();
        let mut next = original["workspaces"][0].clone();
        match case {
            0 => {}
            1 => next["id"] = json!(2),
            _ => {
                next["id"] = json!(2);
                next["active"] = json!(false);
            }
        }
        changed["workspaces"].as_array_mut().unwrap().push(next);
        assert!(decode(&changed).is_err());
    }
    let mut too_many = json!({"schema":1,"workspaces":[]});
    for id in 1..=65 {
        too_many["workspaces"].as_array_mut().unwrap().push(json!({
            "id":id,"output":null,"monitor":null,"active":false,"special":false,
            "layout":"dwindle","windows":[],"columns":[]
        }));
    }
    assert!(decode(&too_many).is_err());
    too_many["workspaces"].as_array_mut().unwrap().pop();
    assert_eq!(decode(&too_many).unwrap().len(), 64);
    let mut many_windows = document();
    let workspace = &mut many_windows["workspaces"][0];
    workspace["layout"] = json!("dwindle");
    workspace["columns"] = json!([]);
    let mut window = workspace["windows"][0].clone();
    window["column"] = Value::Null;
    workspace["windows"] = json!([]);
    for index in 1..=512 {
        window["id"] = json!(format!("0x{index:x}:{index:x}"));
        workspace["windows"]
            .as_array_mut()
            .unwrap()
            .push(window.clone());
    }
    assert!(decode(&many_windows).is_ok());
    let mut next = many_windows["workspaces"][0].clone();
    next["id"] = json!(2);
    next["active"] = json!(false);
    window["id"] = json!("0x9999:9999");
    next["windows"] = json!([window]);
    many_windows["workspaces"]
        .as_array_mut()
        .unwrap()
        .push(next);
    assert!(decode(&many_windows).is_err());
    assert!(parse(b"ok").is_err());
    assert!(parse(b"error: fixture").is_err());
    assert!(parse(b"{\"schema\":1,\"workspaces\":[]} extra").is_err());
}

#[test]
fn native_lifetimes_and_addresses_are_independently_unique() {
    for id in ["0x1234:abc", "0x01234:abc", "0x2345:ffffffffffffffff"] {
        let mut changed = document();
        let workspace = &mut changed["workspaces"][0];
        workspace["layout"] = json!("dwindle");
        workspace["columns"] = json!([]);
        workspace["windows"][0]["column"] = Value::Null;
        let mut next = workspace["windows"][0].clone();
        next["id"] = json!(id);
        workspace["windows"].as_array_mut().unwrap().push(next);
        assert!(decode(&changed).is_err());
    }
}

fn evaluate(setup: &str) -> Value {
    let expression = QUERY.strip_prefix("/repl ").unwrap();
    let source = format!(
        r#"
local one={{address='0x1234',stable_id=0xffffffffffffffff,at={{x=0,y=20}},size={{x=500,y=700}},hidden=false,floating=false,fullscreen=0}}
local two={{address='0x2345',stable_id=0xabc,at={{x=0,y=740}},size={{x=500,y=700}},hidden=false,floating=false,fullscreen=0}}
one.layout={{column={{index=0,width=0.5,windows={{one}}}},index_in_column=0}}
two.layout={{column={{index=1,width=0.5,windows={{two}}}},index_in_column=0}}
local ws={{id=1,monitor={{name='panel',id=0}},active=true,special=false,tiled_layout='scrolling'}}
local reads=0
local hl={{get_workspaces=function()reads=reads+1;return {{ws}} end,
get_workspace_windows=function(w)assert(w==ws);reads=reads+1;return {{two,one}} end}}
{setup}
local value={expression}
assert(reads==2)
local p=require('prek')
local cfg=p.config.preset('desktop')
cfg.rules[1].action={{kind='command',argv={{'capture',value}}}}
return cfg
"#
    );
    let config = crate::lua::Definition::compile(&source, "native-layout-query.lua")
        .unwrap()
        .validate()
        .unwrap();
    let crate::config::Action::Command { argv } = &config.rules[0].action else {
        panic!()
    };
    serde_json::from_str(&argv[1]).unwrap()
}

#[test]
fn actual_luna_query_returns_json_and_sorts_native_columns_without_mutators() {
    let value = evaluate("");
    let workspaces = decode(&value).unwrap();
    assert_eq!(workspaces[0].windows[0].id, "0x2345:abc");
    assert_eq!(workspaces[0].columns[0].index, 0);
    assert_eq!(workspaces[0].columns[1].index, 1);
    assert_eq!(
        workspaces[0].columns[0].members[0],
        "0x1234:ffffffffffffffff"
    );
    let value = evaluate(
        "ws.monitor=nil;ws.active=false;ws.tiled_layout='dwindle';one.layout=nil;two.layout=nil",
    );
    assert!(decode(&value).unwrap()[0].columns.is_empty());
}

#[test]
fn closed_query_quotes_control_bytes_unicode_quotes_and_backslashes_as_json() {
    let value = evaluate("ws.monitor.name='panel\\\"\\\\\\n\\0é'");
    assert_eq!(value["workspaces"][0]["output"], "panel\"\\\n\0é");
    assert!(decode(&value).is_err());
}
