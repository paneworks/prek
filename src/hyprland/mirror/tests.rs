use super::*;

fn settings(values: &[(&str, &str)]) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(option, value)| (option.to_string(), value.to_string()))
        .collect()
}
fn lua_text(text: &str) -> String {
    text.bytes().map(|byte| format!("\\{byte:03}")).collect()
}

#[test]
fn hyprlang_writes_one_keyword_batch_and_lua_one_refreshing_device_call() {
    let declared = settings(&[("kb_layout", "de"), ("repeat_rate", "35")]);
    assert_eq!(
        request(dispatch::Dialect::Hyprlang, "prek-virtual-1-kbd", &declared).unwrap(),
        "[[BATCH]]keyword device[prek-virtual-1-kbd]:kb_layout de;\
         keyword device[prek-virtual-1-kbd]:repeat_rate 35"
    );
    let declared = settings(&[
        ("kb_layout", "de"),
        ("natural_scroll", "1"),
        ("repeat_rate", "35"),
        ("sensitivity", "-0.5"),
        ("tap-to-click", "false"),
    ]);
    assert_eq!(
        request(dispatch::Dialect::Lua, "prek-virtual-1-kbd", &declared).unwrap(),
        format!(
            "r/eval hl.device({{name=\"{}\",kb_layout=\"{}\",natural_scroll=true,\
             repeat_rate=35,sensitivity=-0.5,tap_to_click=false}})",
            lua_text("prek-virtual-1-kbd"),
            lua_text("de")
        )
    );
}

#[test]
fn unknown_options_mistyped_values_and_unsafe_names_are_refused() {
    for (dialect, mirror, declared) in [
        (
            dispatch::Dialect::Hyprlang,
            "kbd",
            settings(&[("enabled", "0")]),
        ),
        (
            dispatch::Dialect::Hyprlang,
            "kbd",
            settings(&[("kb_layout", "de;exec")]),
        ),
        (
            dispatch::Dialect::Hyprlang,
            "kbd]",
            settings(&[("kb_layout", "de")]),
        ),
        (
            dispatch::Dialect::Lua,
            "kbd",
            settings(&[("repeat_rate", "fast")]),
        ),
        (
            dispatch::Dialect::Lua,
            "kbd",
            settings(&[("natural_scroll", "maybe")]),
        ),
        (
            dispatch::Dialect::Lua,
            "kbd",
            settings(&[("sensitivity", "inf")]),
        ),
    ] {
        assert!(
            request(dialect, mirror, &declared).is_err(),
            "{mirror} {declared:?}"
        );
    }
    let name = "prek \"quoted\" ]name";
    assert_eq!(
        request(
            dispatch::Dialect::Lua,
            name,
            &settings(&[("kb_layout", "us")])
        )
        .unwrap(),
        format!(
            "r/eval hl.device({{name=\"{}\",kb_layout=\"{}\"}})",
            lua_text(name),
            lua_text("us")
        )
    );
}

#[test]
fn visible_name_specific_settings_are_xkb_differences_and_explicit_scroll_factors() {
    let globals: Vec<Value> = ["evdev", "pc105", "us", "", ""]
        .iter()
        .map(|value| serde_json::json!({"str": value, "set": false}))
        .collect();
    let keyboard = serde_json::json!({"name":"kbd","rules":"evdev","model":"pc105",
        "layout":"de","variant":"","options":"caps:escape"});
    assert_eq!(
        visible(Class::Keyboard, &keyboard, &globals).unwrap(),
        settings(&[("kb_layout", "de"), ("kb_options", "caps:escape")])
    );
    let keyboard = serde_json::json!({"name":"kbd","rules":"evdev","model":"pc105",
        "layout":"us","variant":"","options":""});
    assert!(
        visible(Class::Keyboard, &keyboard, &globals)
            .unwrap()
            .is_empty()
    );
    let mouse = |factor: f64| serde_json::json!({"name":"mouse","scrollFactor":factor});
    assert!(
        visible(Class::Mouse, &mouse(-1.0), &globals)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        visible(Class::Touchpad, &mouse(1.5), &globals).unwrap(),
        settings(&[("scroll_factor", "1.5")])
    );
    let unsafe_keyboard = serde_json::json!({"name":"kbd","rules":"evdev","model":"pc105",
        "layout":"us","variant":"","options":"grp:a;exec"});
    assert!(visible(Class::Keyboard, &unsafe_keyboard, &globals).is_err());
}

fn keyboard(declared: &[(&str, &str)]) -> Descriptor {
    let mut device = crate::transport::tests::touch();
    device.properties.clear();
    device.axes.clear();
    device.capabilities = BTreeMap::from([(0, vec![0]), (1, vec![30])]);
    Descriptor::mirror(
        &device,
        crate::transport::KernelIdentity {
            bus: 3,
            vendor: 1,
            product: 2,
            version: 1,
        },
        None,
        &BTreeMap::new(),
        settings(declared),
    )
    .unwrap()
}

#[test]
fn evidence_verifies_visible_declarations_and_mirror_parity_with_its_original() {
    let entry = |layout: &str, options: &str| {
        serde_json::json!({"name":"kbd","rules":"evdev","model":"pc105",
            "layout":layout,"variant":"","options":options})
    };
    let declared = keyboard(&[("kb_layout", "de"), ("repeat_rate", "35")]);
    assert_eq!(
        verified_settings(&entry("de", ""), None, &declared).unwrap(),
        settings(&[("kb_layout", "de"), ("repeat_rate", "35")])
    );
    assert!(verified_settings(&entry("us", ""), None, &declared).is_err());
    let original = entry("us", "caps:escape");
    assert!(verified_settings(&entry("de", "caps:escape"), Some(&original), &declared).is_ok());
    assert!(verified_settings(&entry("de", ""), Some(&original), &declared).is_err());
    let plain = keyboard(&[]);
    assert!(verified_settings(&entry("us", "caps:escape"), Some(&original), &plain).is_ok());
    assert!(verified_settings(&entry("de", "caps:escape"), Some(&original), &plain).is_err());
}

#[test]
fn a_reload_is_reported_once_even_when_its_line_arrives_in_pieces() {
    let (stream, mut compositor) = UnixStream::pair().unwrap();
    stream.set_nonblocking(true).unwrap();
    let mut events = Events {
        stream,
        line: Vec::new(),
    };
    assert!(!events.reloaded().unwrap());
    compositor
        .write_all(b"workspace>>2\nactivewindow>>kitty,shell\nconfigrel")
        .unwrap();
    assert!(!events.reloaded().unwrap());
    compositor.write_all(b"oaded>>\nworkspace>>3\n").unwrap();
    assert!(events.reloaded().unwrap());
    assert!(!events.reloaded().unwrap());
    let long = format!("windowtitle>>{}\n", "x".repeat(4096));
    compositor.write_all(long.as_bytes()).unwrap();
    assert!(!events.reloaded().unwrap());
    drop(compositor);
    assert!(events.reloaded().is_err());
}
