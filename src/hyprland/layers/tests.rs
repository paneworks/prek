use super::*;
#[test]
fn inventories_keep_global_origin_identity_and_known_lock_evidence() {
    let mut output = crate::context_transfer::tests::view(Duration::ZERO)
        .context
        .unwrap()
        .snapshot
        .outputs
        .remove(0);
    output.geometry.origin = crate::gestures::Point {
        x: -1000.0,
        y: 40.0,
    };
    let original = serde_json::json!({"fixture-output":{"levels":{"2":[{"address":"0x123","namespace":"shell","pid":123,"x":-1000,"y":40,"w":1000,"h":800}]}}});
    let layers = parse(&original, std::slice::from_ref(&output)).unwrap();
    assert_eq!(layers[0].rect, output.rect());
    assert_eq!(layers[0].pid, Some(123));
    for case in 0..4 {
        let mut changed = original.clone();
        let item = &mut changed["fixture-output"]["levels"]["2"][0];
        match case {
            0 => item["x"] = serde_json::json!(0),
            1 => item["pid"] = serde_json::json!(0),
            2 => item["address"] = serde_json::json!("not-an-address"),
            _ => item["w"] = serde_json::json!(1_000_001),
        }
        assert!(parse(&changed, std::slice::from_ref(&output)).is_err());
    }
    let mut duplicate = original.clone();
    duplicate["fixture-output"]["levels"]["2"]
        .as_array_mut()
        .unwrap()
        .push(original["fixture-output"]["levels"]["2"][0].clone());
    assert!(parse(&duplicate, std::slice::from_ref(&output)).is_err());
    let mut inhibited = false;
    assert!(decode(b"{\"locked\":true} invalid", &[], &mut inhibited).is_err());
    assert!(inhibited);
    inhibited = false;
    assert!(
        decode(
            b"{\"locked\":false} {} {} {\"locked\":true}",
            &[],
            &mut inhibited
        )
        .is_err()
    );
    assert!(inhibited);
}
#[test]
fn map_in_bounds_only_a_listed_bezier_and_tolerates_a_missing_reply() {
    let reply = |name: &str, speed: f64, bezier: &str| {
        serde_json::json!([
            [
                {"name":"global","overridden":true,"bezier":"default","enabled":true,"speed":10.0,"style":""},
                {"name":name,"overridden":true,"bezier":bezier,"enabled":true,"speed":speed,"style":"slide"}
            ],
            [{"name":"default","X0":0.05,"Y0":0.9,"X1":0.1,"Y1":1.05}]
        ])
    };
    let bound = Some(Duration::from_micros(400_500));
    assert_eq!(map_in(&reply("layersIn", 4.0, "default"), None), bound);
    assert_eq!(
        map_in(&reply("layersIn", 4.0, "default"), Some(true)),
        bound
    );
    for value in [
        reply("layersIn", 4.0, "springy"),
        reply("layersOut", 4.0, "default"),
        reply("layersIn", 0.0, "default"),
        reply("layersIn", 101.0, "default"),
        serde_json::json!([[], []]),
        serde_json::json!({}),
    ] {
        assert_eq!(map_in(&value, None), None);
    }
    let mut duplicate = reply("layersIn", 4.0, "default");
    let node = duplicate[0][1].clone();
    duplicate[0].as_array_mut().unwrap().push(node);
    assert_eq!(map_in(&duplicate, Some(false)), None);
    // The stock tick warps every value while animations or the node are off.
    let mut disabled = reply("layersIn", 4.0, "springy");
    disabled[0][1]["enabled"] = serde_json::json!(false);
    assert_eq!(map_in(&disabled, Some(true)), Some(Duration::ZERO));
    let springy = reply("layersIn", 4.0, "springy");
    assert_eq!(map_in(&springy, Some(false)), Some(Duration::ZERO));
    for (option, enabled) in [
        (
            serde_json::json!({"option":"animations:enabled","int":0,"set":true}),
            Some(false),
        ),
        (
            serde_json::json!({"option":"animations:enabled","bool":true,"set":false}),
            Some(true),
        ),
        (
            serde_json::json!({"option":"misc:vfr","int":0,"set":true}),
            None,
        ),
    ] {
        assert_eq!(super::super::animations::enabled(&option), enabled);
    }
    let monitors = serde_json::json!([]);
    let batch = |tail: &str| {
        format!("{{\"locked\":false}} {monitors} {{}} {{\"locked\":false}} {tail}").into_bytes()
    };
    let mut inhibited = false;
    let animations = reply("layersIn", 2.5, "default").to_string();
    let off = r#"{"option":"animations:enabled","int":0,"set":true}"#;
    for (tail, expected) in [
        (format!("{animations} {off}"), Some(Duration::ZERO)),
        (
            format!("{animations} no such option"),
            Some(Duration::from_micros(250_500)),
        ),
        (animations.clone(), Some(Duration::from_micros(250_500))),
        ("unknown request".into(), None),
        (String::new(), None),
    ] {
        let inventory = decode(&batch(&tail), &[], &mut inhibited).unwrap();
        assert_eq!(inventory.map_in, expected, "{tail}");
        assert!(inventory.layers.is_empty());
    }
    assert!(
        decode(
            &batch(&format!("{animations} {off} {{}}")),
            &[],
            &mut inhibited
        )
        .is_err()
    );
    assert!(!inhibited);
}
#[test]
fn map_in_reads_the_stock_reply_through_inherited_layer_configuration() {
    use crate::hyprland::animations::tests::{configure, stock};
    let (mut reply, off) = stock();
    let on = Some(true);
    assert_eq!(map_in(&reply, on), Some(Duration::from_micros(300_500)));
    assert_eq!(
        map_in(&reply, super::super::animations::enabled(&off)),
        Some(Duration::ZERO)
    );
    let layers_in = reply[0]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["name"] == "layersIn")
        .unwrap();
    layers_in["overridden"] = serde_json::json!(false);
    layers_in["speed"] = serde_json::json!(0.0);
    layers_in["bezier"] = serde_json::json!("");
    assert_eq!(map_in(&reply, on), Some(Duration::from_micros(800_500)));
    configure(&mut reply, "layers", false);
    assert_eq!(map_in(&reply, on), Some(Duration::ZERO));
    configure(&mut reply, "layers", true);
    assert_eq!(map_in(&reply, on), Some(Duration::from_micros(100_500)));
}
