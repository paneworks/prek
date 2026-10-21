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

/// Parents in the stock 0.56.2 animation tree (`AnimationTree.cpp`) on the
/// paths from the nodes read here up to `global`.
const PARENTS: [(&str, &str); 12] = [
    ("windows", "global"),
    ("layers", "global"),
    ("workspaces", "global"),
    ("windowsIn", "windows"),
    ("windowsOut", "windows"),
    ("windowsMove", "windows"),
    ("layersIn", "layers"),
    ("workspacesIn", "workspaces"),
    ("workspacesOut", "workspaces"),
    ("specialWorkspace", "workspaces"),
    ("specialWorkspaceIn", "specialWorkspace"),
    ("specialWorkspaceOut", "specialWorkspace"),
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

/// The listed node whose configuration `name` uses. Stock replies print each
/// node's own values; one never configured inherits its nearest overridden
/// ancestor's. `None` when a node on that path is missing or listed twice.
pub(crate) fn effective<'a>(nodes: &'a [Value], name: &str) -> Option<&'a Value> {
    let mut current = name;
    for _ in 0..=PARENTS.len() {
        let mut matching = nodes.iter().filter(|node| node["name"] == current);
        let node = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        if node["overridden"].as_bool()? {
            return Some(node);
        }
        current = PARENTS
            .iter()
            .find(|(child, _)| *child == current)
            .map(|(_, parent)| *parent)?;
    }
    None
}

/// Whether no window or workspace movement can be animating, so stock goal
/// geometry is the rendered geometry: animations are globally off, or every
/// movement node uses a disabled configuration.
pub(crate) fn still(animations: &Value, option: &Value) -> bool {
    if enabled(option) == Some(false) {
        return true;
    }
    let Some(nodes) = animations.get(0).and_then(Value::as_array) else {
        return false;
    };
    MOVEMENT
        .iter()
        .all(|name| effective(nodes, name).is_some_and(|node| node["enabled"] == false))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// Hyprland 0.56.2's own reply with animations off and `layersIn` set to
    /// speed 3 on the `prekin` bezier, captured on the owned Cage compositor.
    pub(crate) fn stock() -> (Value, Value) {
        let mut reply: Value =
            serde_json::from_str(include_str!("animations/stock-0.56.2.json")).unwrap();
        (reply["animations"].take(), reply["option"].take())
    }
    pub(crate) fn configure(reply: &mut Value, name: &str, enabled: bool) {
        let node = reply[0]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|node| node["name"] == name)
            .unwrap();
        node["overridden"] = json!(true);
        node["enabled"] = json!(enabled);
        node["speed"] = json!(1.0);
        node["bezier"] = json!("default");
    }

    #[test]
    fn stillness_follows_the_inherited_stock_configuration() {
        let on = json!({"option":"animations:enabled","int":1,"set":true});
        let (reply, off) = stock();
        assert_eq!(enabled(&off), Some(false));
        assert!(still(&reply, &off));
        assert!(still(&json!("no such reply"), &off));
        assert!(!still(&reply, &on));
        let mut parents = reply.clone();
        configure(&mut parents, "windows", false);
        assert!(!still(&parents, &on));
        configure(&mut parents, "workspaces", false);
        assert!(still(&parents, &on));
        assert!(still(&parents, &json!("unknown")));
        configure(&mut parents, "specialWorkspaceIn", true);
        assert!(!still(&parents, &on));
        let mut leaves = reply.clone();
        for name in MOVEMENT {
            configure(&mut leaves, name, false);
        }
        assert!(still(&leaves, &on));
        let mut duplicated = parents.clone();
        let node = duplicated[0]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["name"] == "windows")
            .unwrap()
            .clone();
        duplicated[0].as_array_mut().unwrap().push(node);
        assert!(!still(&duplicated, &on));
        let mut orphan = parents;
        orphan[0]
            .as_array_mut()
            .unwrap()
            .retain(|node| node["name"] != "global");
        configure(&mut orphan, "specialWorkspaceIn", false);
        assert!(still(&orphan, &on));
        orphan[0]
            .as_array_mut()
            .unwrap()
            .retain(|node| node["name"] != "workspaces");
        assert!(!still(&orphan, &on));
        assert_eq!(enabled(&json!({"option":"misc:vfr","int":0})), None);
    }

    #[test]
    fn an_unconfigured_node_inherits_its_nearest_overridden_ancestor() {
        let (reply, _) = stock();
        let nodes = reply[0].as_array().unwrap();
        assert_eq!(effective(nodes, "layersIn").unwrap()["bezier"], "prekin");
        let global = effective(nodes, "windowsMove").unwrap();
        assert_eq!(
            (&global["name"], &global["speed"]),
            (&json!("global"), &json!(8.0))
        );
        assert_eq!(
            effective(nodes, "specialWorkspaceOut").unwrap()["name"],
            "global"
        );
        assert!(effective(nodes, "fadeIn").is_none());
        assert!(effective(nodes, "no such node").is_none());
    }
}
