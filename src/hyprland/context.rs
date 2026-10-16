//! Stock IPC geometry and direct lock inhibition. Lock-provider transitions
//! remain separate from compositor membership and geometry sampling.

use super::{Client, REQUEST_TIME, invalid};
use crate::{
    config::Screen,
    context::{OutputState, Rect, Region, SessionKind, Snapshot, State, Window},
    gestures::{Output, Point, Transform},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct ContextSampler {
    revision: u64,
    outputs: BTreeMap<String, (Output, u64)>,
    active_outputs: BTreeSet<String>,
    inhibited: bool,
}
impl ContextSampler {
    /// Broker calls this only after accepting the bound native provider's
    /// completed unlock and independently verifying compositor unlock.
    pub(crate) fn provider_unlock_completed(&mut self) {
        self.inhibited = false;
    }
    /// Used by observation-only agents too: authentication input must never be
    /// exposed merely because desktop context was not requested.
    pub fn check_lock(&mut self, client: &Client) -> io::Result<bool> {
        self.inhibited |= locked(&client.query_until("locked", Instant::now() + REQUEST_TIME)?)?;
        Ok(self.inhibited)
    }
    pub fn inhibited(&self) -> bool {
        self.inhibited
    }
    /// One shared request budget brackets geometry with the verified
    /// compositor's lock state. A witnessed lock latches inhibition until a
    /// bound provider can establish its completed unlock transition.
    pub fn sample(
        &mut self,
        client: &Client,
        generation: u64,
        kind: SessionKind,
    ) -> io::Result<Snapshot> {
        self.sample_until(client, generation, kind, Instant::now() + REQUEST_TIME)
    }
    pub(crate) fn sample_until(
        &mut self,
        client: &Client,
        generation: u64,
        kind: SessionKind,
        until: Instant,
    ) -> io::Result<Snapshot> {
        if generation == 0 {
            return Err(invalid("context requires a positive session generation"));
        }
        let captured_at = crate::ipc::monotonic()?;
        if self.inhibited {
            return Err(invalid("a completed lock-provider transition is required"));
        }
        let bytes = client.query_context(until)?;
        let mut replies = serde_json::Deserializer::from_slice(&bytes).into_iter::<Value>();
        // Decode the leading lock witness first, retaining inhibition even if
        // later geometry replies are malformed or incomplete.
        if locked(&next_reply(&mut replies)?)? {
            self.inhibited = true;
            return Err(invalid("compositor lock inhibits desktop context"));
        }
        let monitors = next_reply(&mut replies)?;
        let clients = next_reply(&mut replies)?;
        let layers = next_reply(&mut replies)?;
        let active = next_reply(&mut replies)?;
        let final_monitors = next_reply(&mut replies)?;
        if locked(&next_reply(&mut replies)?)? {
            self.inhibited = true;
            return Err(invalid("compositor locked during context sampling"));
        }
        if replies.next().is_some() {
            return Err(invalid("unexpected extra context batch reply"));
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("context revision exhausted"))?;
        let mut snapshot = parse(
            &monitors,
            &clients,
            &layers,
            &active,
            generation,
            revision,
            captured_at,
        )?;
        let final_outputs = parse(
            &final_monitors,
            &Value::Array(vec![]),
            &Value::Object(Default::default()),
            &Value::Object(Default::default()),
            generation,
            revision,
            captured_at,
        )?
        .outputs;
        if snapshot.outputs != final_outputs {
            return Err(invalid("output topology changed during context sampling"));
        }
        // Keep tombstones so removal/reappearance cannot reuse an old mapping
        // generation. Bound this history independently of the live outputs.
        let mut outputs = self.outputs.clone();
        for output in &mut snapshot.outputs {
            let geometry_generation = match self.outputs.get(&output.geometry.name) {
                Some((geometry, generation))
                    if geometry == &output.geometry
                        && self.active_outputs.contains(&output.geometry.name) =>
                {
                    *generation
                }
                Some((_, generation)) => generation
                    .checked_add(1)
                    .ok_or_else(|| invalid("output geometry generation exhausted"))?,
                None => 2,
            };
            output.geometry_generation = geometry_generation;
            outputs.insert(
                output.geometry.name.clone(),
                (output.geometry.clone(), geometry_generation),
            );
        }
        if outputs.len() > 4096 {
            return Err(invalid("output identity history exhausted"));
        }
        snapshot.state = match kind {
            SessionKind::User => State::Desktop,
            SessionKind::Greeter => State::Greeter,
        };
        snapshot.validate().map_err(io::Error::other)?;
        self.active_outputs = snapshot
            .outputs
            .iter()
            .map(|output| output.geometry.name.clone())
            .collect();
        self.outputs = outputs;
        self.revision = snapshot.revision;
        Ok(snapshot)
    }
}
fn next_reply(replies: &mut impl Iterator<Item = serde_json::Result<Value>>) -> io::Result<Value> {
    replies
        .next()
        .ok_or_else(|| invalid("incomplete context batch reply"))?
        .map_err(io::Error::other)
}
fn locked(value: &Value) -> io::Result<bool> {
    value
        .get("locked")
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid("compositor lock state is unavailable"))
}
fn text(value: &Value, key: &str) -> io::Result<String> {
    let text = value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("missing context string"))?;
    if text.is_empty() || text.len() > 256 || text.chars().any(char::is_control) {
        return Err(invalid("invalid context string"));
    }
    Ok(text.into())
}
fn number(value: &Value, key: &str) -> io::Result<f64> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite())
        .ok_or_else(|| invalid("missing context number"))
}
fn integer(value: &Value, key: &str) -> io::Result<i64> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid("missing context integer"))
}
fn flag(value: &Value, key: &str) -> io::Result<bool> {
    value
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid(&format!("missing context boolean: {key}")))
}
fn point(value: &Value, key: &str) -> io::Result<Point> {
    let values = value
        .get(key)
        .and_then(Value::as_array)
        .filter(|values| values.len() == 2)
        .ok_or_else(|| invalid("missing context coordinate pair"))?;
    Ok(Point {
        x: values[0]
            .as_f64()
            .ok_or_else(|| invalid("invalid x coordinate"))?,
        y: values[1]
            .as_f64()
            .ok_or_else(|| invalid("invalid y coordinate"))?,
    })
}
fn transform(value: i64) -> io::Result<Transform> {
    Ok(match value {
        0 => Transform::Normal,
        1 => Transform::Rotate90,
        2 => Transform::Rotate180,
        3 => Transform::Rotate270,
        4 => Transform::Flipped,
        5 => Transform::Flipped90,
        6 => Transform::Flipped180,
        7 => Transform::Flipped270,
        _ => return Err(invalid("unsupported output transform")),
    })
}
fn parse(
    monitors: &Value,
    clients: &Value,
    layers: &Value,
    active: &Value,
    generation: u64,
    revision: u64,
    captured_at: Duration,
) -> io::Result<Snapshot> {
    let monitors = monitors
        .as_array()
        .filter(|values| values.len() <= 256)
        .ok_or_else(|| invalid("invalid monitor collection"))?;
    let mut outputs = Vec::new();
    let mut by_id = BTreeMap::new();
    for monitor in monitors {
        if flag(monitor, "disabled")? {
            continue;
        }
        let name = text(monitor, "name")?;
        let workspace = integer(
            monitor
                .get("activeWorkspace")
                .ok_or_else(|| invalid("missing active workspace"))?,
            "id",
        )?;
        if by_id
            .insert(integer(monitor, "id")?, name.clone())
            .is_some()
        {
            return Err(invalid("duplicate output identity"));
        }
        outputs.push(OutputState {
            geometry: Output {
                name,
                pixel_width: number(monitor, "width")?,
                pixel_height: number(monitor, "height")?,
                scale: number(monitor, "scale")?,
                origin: Point {
                    x: number(monitor, "x")?,
                    y: number(monitor, "y")?,
                },
                transform: transform(integer(monitor, "transform")?)?,
            },
            geometry_generation: 2,
            active_workspace: Some(workspace),
            screen: if flag(monitor, "dpmsStatus")? {
                Screen::On
            } else {
                Screen::Off
            },
            keyboard_known: false,
            keyboard: None,
        });
    }
    let clients = clients
        .as_array()
        .filter(|values| values.len() <= 4096)
        .ok_or_else(|| invalid("invalid client collection"))?;
    let mut windows = Vec::new();
    let active_id = if active.as_object().is_some_and(|object| object.is_empty()) {
        None
    } else {
        Some(window_id(active)?)
    };
    for client in clients {
        if !flag(client, "mapped")? {
            continue;
        }
        let output = by_id
            .get(&integer(client, "monitor")?)
            .ok_or_else(|| invalid("client has no known output"))?
            .clone();
        let workspace = integer(
            client
                .get("workspace")
                .ok_or_else(|| invalid("missing client workspace"))?,
            "id",
        )?;
        let rect = Rect {
            origin: point(client, "at")?,
            size: point(client, "size")?,
        };
        let id = window_id(client)?;
        let is_focused = active_id.as_ref() == Some(&id);
        windows.push(Window {
            id,
            output,
            workspace,
            rect,
            content: rect,
            mapped: true,
            // Stock IPC reports animation goals, not current rendered bounds.
            // Retain that geometry for diagnostics, but never use it as proof
            // of an eligible app-content target without a rendering provider.
            visible: false,
            focused: is_focused,
            z_order: None,
        });
    }
    let layers = layers
        .as_object()
        .ok_or_else(|| invalid("invalid layer collection"))?;
    let mut regions = Vec::new();
    for (output, layer) in layers {
        if !outputs.iter().any(|item| &item.geometry.name == output) {
            return Err(invalid("layer has no known output"));
        }
        let levels = layer
            .get("levels")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("invalid layer levels"))?;
        for (level, surfaces) in levels {
            let level: u8 = level
                .parse()
                .ok()
                .filter(|level| *level <= 3)
                .ok_or_else(|| invalid("invalid layer level"))?;
            let surfaces = surfaces
                .as_array()
                .ok_or_else(|| invalid("invalid layer surfaces"))?;
            for surface in surfaces {
                if regions.len() >= 4096 {
                    return Err(invalid("too many layer surfaces"));
                }
                if level == 0 {
                    continue;
                }
                let rect = Rect {
                    origin: Point {
                        x: number(surface, "x")?,
                        y: number(surface, "y")?,
                    },
                    size: Point {
                        x: number(surface, "w")?,
                        y: number(surface, "h")?,
                    },
                };
                if rect.size.x <= 0.0 || rect.size.y <= 0.0 {
                    continue;
                }
                regions.push(Region {
                    id: format!("layer:{}:{}", output, text(surface, "address")?),
                    scope: None,
                    output: output.clone(),
                    rect,
                    interactive: true,
                });
            }
        }
    }
    let snapshot = Snapshot {
        generation,
        revision,
        captured_at,
        state: State::Desktop,
        outputs,
        windows,
        regions,
        secure_overlay: false,
    };
    snapshot.validate().map_err(io::Error::other)?;
    Ok(snapshot)
}
fn window_id(value: &Value) -> io::Result<String> {
    let address = text(value, "address")?;
    let stable = text(value, "stableId")?;
    if !address.starts_with("0x")
        || address.len() <= 2
        || address.len() > 18
        || !address[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        || stable.len() > 16
        || !stable.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid("invalid window lifetime identity"));
    }
    Ok(format!("{address}:{stable}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn monitor() -> Value {
        json!({"id": 1, "name": "panel", "disabled": false, "width": 1080,
            "height": 1920, "scale": 2.0, "x": 20, "y": 40, "transform": 0,
            "activeWorkspace": {"id": 3}, "dpmsStatus": true})
    }
    fn window() -> Value {
        json!({"address": "0x1234", "stableId": "abc", "monitor": 1,
            "workspace": {"id": 3}, "mapped": true, "at": [20, 40],
            "size": [300, 500], "focusHistoryID": 0, "title": "private text"})
    }
    fn snapshot(
        monitors: &Value,
        clients: &Value,
        layers: &Value,
        active: &Value,
    ) -> io::Result<Snapshot> {
        parse(monitors, clients, layers, active, 7, 1, Duration::ZERO)
    }
    #[test]
    fn transforms_preserve_pixel_dimensions_and_logical_origin() {
        for value in 0..8 {
            let mut monitor = monitor();
            monitor["transform"] = json!(value);
            let result = snapshot(&json!([monitor]), &json!([]), &json!({}), &json!({})).unwrap();
            let output = &result.outputs[0];
            assert_eq!(output.geometry.transform, transform(value).unwrap());
            assert_eq!(output.geometry.pixel_width, 1080.0);
            assert_eq!(output.geometry.scale, 2.0);
            assert_eq!(output.rect().origin, Point { x: 20.0, y: 40.0 });
            assert_eq!(output.active_workspace, Some(3));
        }
        assert!(transform(8).is_err());
    }
    #[test]
    fn focus_uses_current_lifetime_and_animation_goals_never_authorize_app_targets() {
        let monitors = json!([monitor()]);
        let clients = json!([window()]);
        let mut active = window();
        active["stableId"] = json!("abd");
        let result = snapshot(&monitors, &clients, &json!({}), &active).unwrap();
        assert!(!result.windows[0].focused);
        assert!(!result.windows[0].visible);
        let result = snapshot(&monitors, &clients, &json!({}), &window()).unwrap();
        assert!(result.windows[0].focused);
        assert!(!result.windows[0].visible);
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("private text")
        );
    }
    #[test]
    fn layers_reserve_interactive_space_without_inventing_shell_regions() {
        let layers = json!({"panel": {"levels": {"2": [{"address": "0x99",
            "x": 20, "y": 40, "w": 540, "h": 30}]}}});
        let result = snapshot(&json!([monitor()]), &json!([]), &layers, &json!({})).unwrap();
        assert_eq!(result.regions.len(), 1);
        assert!(result.regions[0].interactive);
        assert_eq!(result.regions[0].scope, None);
        assert!(!result.outputs[0].keyboard_known);
        assert!(result.outputs[0].keyboard.is_none());
        let mut outside = layers;
        outside["panel"]["levels"]["2"][0]["x"] = json!(0);
        assert!(snapshot(&json!([monitor()]), &json!([]), &outside, &json!({})).is_err());
    }
    #[test]
    fn malformed_or_ambiguous_geometry_and_empty_lifetime_ids_are_rejected() {
        assert!(
            snapshot(
                &json!([monitor(), monitor()]),
                &json!([]),
                &json!({}),
                &json!({})
            )
            .is_err()
        );
        assert!(
            snapshot(
                &json!([monitor()]),
                &json!([window(), window()]),
                &json!({}),
                &json!({})
            )
            .is_err()
        );
        for address in ["0x", "0xzz", "1234"] {
            let mut invalid = window();
            invalid["address"] = json!(address);
            assert!(window_id(&invalid).is_err());
        }
        let mut invalid = monitor();
        invalid["scale"] = json!(0);
        assert!(snapshot(&json!([invalid]), &json!([]), &json!({}), &json!({})).is_err());
        assert!(locked(&json!({})).is_err());
        assert!(locked(&json!({"locked": "false"})).is_err());
    }
}
