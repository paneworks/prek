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
