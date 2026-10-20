//! Stock animation configuration as geometry evidence. The stock animation
//! tick warps every value while the global switch or the value's node is off.
use super::*;

/// Nodes whose values move or reveal windows and workspaces.
const MOVEMENT: [&str; 10] = [
    "windows",
    "windowsIn",
    "windowsOut",
    "windowsMove",
    "workspaces",
    "workspacesIn",
    "workspacesOut",
    "specialWorkspace",
    "specialWorkspaceIn",
    "specialWorkspaceOut",
];

/// The global `animations:enabled` switch from a stock `getoption` reply.
pub(crate) fn enabled(option: &Value) -> Option<bool> {
    if option["option"] != "animations:enabled" {
        return None;
    }
    option["int"]
        .as_i64()
        .map(|value| value != 0)
        .or_else(|| option["bool"].as_bool())
}

/// Whether no window or workspace movement can be animating, so stock goal
/// geometry is the rendered geometry: animations are globally off, or every
/// movement node appears exactly once and is disabled.
pub(crate) fn still(animations: &Value, option: &Value) -> bool {
    if enabled(option) == Some(false) {
        return true;
    }
    let Some(nodes) = animations.get(0).and_then(Value::as_array) else {
        return false;
    };
    MOVEMENT.iter().all(|name| {
        let mut matching = nodes.iter().filter(|node| node["name"] == *name);
        matches!(
            (matching.next(), matching.next()),
            (Some(node), None) if node["enabled"] == false
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree(disabled: &[&str]) -> Value {
        let nodes: Vec<_> = MOVEMENT
            .iter()
            .chain(&["layersIn", "fadeIn"])
            .map(|name| {
                json!({"name":name,"overridden":false,"bezier":"default",
                "enabled":!disabled.contains(name),"speed":4.0,"style":""})
            })
            .collect();
        json!([nodes, [{"name":"default","X0":0.0,"Y0":0.0,"X1":1.0,"Y1":1.0}]])
    }

    #[test]
    fn only_a_disabled_global_switch_or_every_movement_node_proves_stillness() {
        let on = json!({"option":"animations:enabled","int":1,"set":true});
        let off = json!({"option":"animations:enabled","bool":false,"set":true});
        assert!(still(&tree(&[]), &off));
        assert!(still(&json!("no such reply"), &off));
        assert!(!still(&tree(&[]), &on));
        assert!(still(&tree(&MOVEMENT), &on));
        assert!(still(&tree(&MOVEMENT), &json!("unknown")));
        for missing in MOVEMENT {
            let partial: Vec<_> = MOVEMENT.into_iter().filter(|n| *n != missing).collect();
            assert!(!still(&tree(&partial), &on), "{missing}");
        }
        let mut duplicated = tree(&MOVEMENT);
        let node = duplicated[0][0].clone();
        duplicated[0].as_array_mut().unwrap().push(node);
        assert!(!still(&duplicated, &on));
        assert_eq!(enabled(&json!({"option":"misc:vfr","int":0})), None);
    }
}
