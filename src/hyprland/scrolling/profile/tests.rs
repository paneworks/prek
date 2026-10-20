use super::*;
use crate::{
    config::Screen,
    gestures::{Output, Point, Transform},
    hyprland::layouts::{Column, Position, Window},
};
use serde_json::json;

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
        geometry_generation: 2,
        active_workspace: Some(1),
        screen: Screen::On,
        keyboard_known: false,
        keyboard: None,
    }
}
fn settings() -> Settings {
    Settings {
        direction: "down".into(),
        gaps_in: [0; 4],
        gaps_out: [0; 4],
        border: 0,
        workspace_rules: 0,
        reserved: vec![("phone".into(), [0.0, 40.0, 0.0, 0.0])],
    }
}
fn window(id: &str, rect: [f64; 4], index: usize, row: usize) -> Window {
    Window {
        id: id.into(),
        rect,
        hidden: false,
        floating: false,
        grouped: false,
        fullscreen: 0,
        column: Some(Position { index, row }),
    }
}
/// Three half-height strips under a 40 px reserved bar, scrolled by 240;
/// the middle strip holds two side-by-side windows.
fn workspace(offset: f64) -> Workspace {
    let top = |strip: f64| 40.0 + strip * 480.0 - offset;
    Workspace {
        id: 1,
        output: Some("phone".into()),
        monitor: Some(0),
        active: true,
        special: false,
        layout: "scrolling".into(),
        windows: vec![
            window("a", [0.0, top(0.0), 500.0, 480.0], 0, 0),
            window("b", [0.0, top(1.0), 250.0, 480.0], 1, 0),
            window("c", [250.0, top(1.0), 250.0, 480.0], 1, 1),
            window("d", [0.0, top(2.0), 500.0, 480.0], 2, 0),
        ],
        columns: vec![
            Column {
                index: 0,
                width: 0.5,
                members: vec!["a".into()],
            },
            Column {
                index: 1,
                width: 0.5,
                members: vec!["b".into(), "c".into()],
            },
            Column {
                index: 2,
                width: 0.5,
                members: vec!["d".into()],
            },
        ],
    }
}

#[test]
fn a_tiled_downward_profile_yields_the_native_camera_and_content_bounds() {
    let viewport = viewport(&workspace(240.0), &phone(), &settings()).unwrap();
    assert_eq!(viewport.bounds(), (0.0, 480.0));
    assert_eq!(viewport.offset(), 240.0);
    for offset in [0.0, 480.0] {
        assert_eq!(
            viewport_of(workspace(offset)).unwrap().offset(),
            offset,
            "{offset}"
        );
    }
    let mut truncated = workspace(240.0);
    truncated.windows[2].rect[1] += 1.9;
    truncated.windows[3].rect[3] -= 1.9;
    assert_eq!(viewport_of(truncated).unwrap().offset(), 240.0);
    let mut short = workspace(-240.0);
    short.windows.truncate(1);
    short.columns.truncate(1);
    let centered = viewport_of(short).unwrap();
    assert_eq!(
        (centered.bounds(), centered.offset()),
        ((-240.0, -240.0), -240.0)
    );
}
fn viewport_of(workspace: Workspace) -> Result<Viewport, Refusal> {
    viewport(&workspace, &phone(), &settings())
}
#[test]
fn every_departure_from_the_profile_is_refused_with_its_reason() {
    let refused = |change: &dyn Fn(&mut Workspace, &mut Settings), reason| {
        let (mut workspace, mut settings) = (workspace(240.0), settings());
        change(&mut workspace, &mut settings);
        assert_eq!(viewport(&workspace, &phone(), &settings), Err(reason));
    };
    refused(&|w, _| w.layout = "dwindle".into(), Refusal::Layout);
    refused(&|w, _| w.active = false, Refusal::Layout);
    refused(&|w, _| w.output = Some("other".into()), Refusal::Layout);
    refused(&|_, s| s.direction = "up".into(), Refusal::Direction);
    refused(&|_, s| s.gaps_in = [0, 0, 0, 5], Refusal::Gaps);
    refused(&|_, s| s.gaps_out = [5; 4], Refusal::Gaps);
    refused(&|_, s| s.border = 2, Refusal::Border);
    refused(&|_, s| s.workspace_rules = 1, Refusal::WorkspaceRules);
    refused(&|w, _| w.windows[1].floating = true, Refusal::Window);
    refused(&|w, _| w.windows[1].fullscreen = 1, Refusal::Window);
    refused(&|w, _| w.windows[0].column = None, Refusal::Window);
    refused(&|w, _| w.columns[2].index = 3, Refusal::Columns);
    refused(&|w, _| w.columns[1].members.swap(0, 1), Refusal::Columns);
    refused(
        &|w, _| w.columns[1].members.pop().map(drop).unwrap(),
        Refusal::Columns,
    );
    refused(&|_, s| s.reserved.clear(), Refusal::Geometry);
    refused(&|w, _| w.windows[3].rect[1] += 3.0, Refusal::Geometry);
    refused(&|w, _| w.windows[2].rect[2] = 240.0, Refusal::Geometry);
    refused(&|w, _| w.columns[2].width = 0.25, Refusal::Geometry);
    let beyond = workspace(490.0);
    assert_eq!(viewport_of(beyond), Err(Refusal::Geometry));
}
#[test]
fn settings_decode_stock_option_rule_and_monitor_replies() {
    let direction = json!({"option":"scrolling:direction","str":"down","set":true});
    let gaps_in = json!({"option":"general:gaps_in","css":"0 0 0 0","set":true});
    let gaps_out = json!({"option":"general:gaps_out","custom":"1 2 3 4","set":true});
    let border = json!({"option":"general:border_size","int":0,"set":false});
    let rules = json!([]);
    let monitors = json!([{"name":"phone","reserved":[0,40,0,12]}]);
    let decoded =
        Settings::decode([&direction, &gaps_in, &gaps_out, &border, &rules, &monitors]).unwrap();
    assert_eq!(decoded.direction, "down");
    assert_eq!(decoded.gaps_out, [1, 2, 3, 4]);
    assert_eq!(decoded.reserved, [("phone".into(), [0.0, 40.0, 0.0, 12.0])]);
    for bad in [
        json!({"option":"general:gaps_out","css":"0 0 0","set":true}),
        json!({"option":"general:gaps_in","css":"0 0 0 0","set":true}),
        json!("no such option"),
    ] {
        assert!(
            Settings::decode([&direction, &gaps_in, &bad, &border, &rules, &monitors]).is_err()
        );
    }
    let reserved = json!([{"name":"phone","reserved":[0,40,0]}]);
    assert!(
        Settings::decode([&direction, &gaps_in, &gaps_out, &border, &rules, &reserved]).is_err()
    );
}
#[test]
fn the_profile_batch_is_bracketed_by_lock_witnesses_and_exact() {
    let unlocked = r#"{"locked":false}"#;
    let body = [
        r#"{"option":"scrolling:direction","str":"down","set":true}"#,
        r#"{"option":"general:gaps_in","css":"0 0 0 0","set":true}"#,
        r#"{"option":"general:gaps_out","css":"0 0 0 0","set":true}"#,
        r#"{"option":"general:border_size","int":0,"set":true}"#,
        "[]",
        r#"[{"name":"phone","reserved":[0,40,0,0]}]"#,
    ]
    .join(" ");
    let batch = |first: &str, last: &str, tail: &str| format!("{first} {body} {last}{tail}");
    let mut inhibited = false;
    let decoded = decode(batch(unlocked, unlocked, "").as_bytes(), &mut inhibited).unwrap();
    assert_eq!(decoded, settings());
    assert!(decode(batch(unlocked, unlocked, " {}").as_bytes(), &mut inhibited).is_err());
    assert!(decode(batch(unlocked, "", "").as_bytes(), &mut inhibited).is_err());
    assert!(!inhibited);
    let locked = r#"{"locked":true}"#;
    assert!(decode(batch(unlocked, locked, "").as_bytes(), &mut inhibited).is_err());
    assert!(inhibited);
}
