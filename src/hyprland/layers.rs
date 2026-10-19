//! Verified stock layer layout inventory. Layout geometry is not the animated
//! rendered box; matching it never establishes presentation or input authority.
use super::*;
use crate::context::{OutputState, Rect};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    pub output: String,
    pub address: String,
    pub namespace: String,
    pub pid: Option<u32>,
    pub level: u8,
    pub rect: Rect,
}
impl Layer {
    pub(crate) fn validate(&self, output: &OutputState) -> io::Result<()> {
        if self.output != output.geometry.name
            || self.level > 3
            || self.pid == Some(0)
            || !self.address.starts_with("0x")
            || self.address.len() < 3
            || self.address.len() > 66
            || !self.address[2..].bytes().all(|b| b.is_ascii_hexdigit())
            || self.namespace.len() > 1024
            || self.namespace.chars().any(char::is_control)
            || !self.rect.valid()
            || !output.rect().contains_rect(self.rect)
        {
            return Err(invalid("invalid owning layer identity/geometry"));
        }
        Ok(())
    }
}
pub(crate) fn parse(value: &Value, outputs: &[OutputState]) -> io::Result<Vec<Layer>> {
    let mut result = Vec::new();
    let mut addresses = std::collections::BTreeSet::new();
    for (output, collection) in value
        .as_object()
        .ok_or_else(|| invalid("invalid layer inventory"))?
    {
        let geometry = outputs
            .iter()
            .find(|o| o.geometry.name == *output)
            .ok_or_else(|| invalid("unknown layer output"))?;
        for (level, items) in collection["levels"]
            .as_object()
            .ok_or_else(|| invalid("invalid layer levels"))?
        {
            let level = level
                .parse::<u8>()
                .ok()
                .filter(|v| *v <= 3)
                .ok_or_else(|| invalid("invalid layer level"))?;
            for item in items
                .as_array()
                .ok_or_else(|| invalid("invalid layer list"))?
            {
                if result.len() >= 4096 {
                    return Err(invalid("layer inventory exceeds limit"));
                }
                let text = |field: &str| {
                    item[field]
                        .as_str()
                        .filter(|s| {
                            (field == "namespace" || !s.is_empty())
                                && s.len() <= 1024
                                && !s.chars().any(char::is_control)
                        })
                        .map(str::to_owned)
                        .ok_or_else(|| invalid("invalid layer identity"))
                };
                let address = text("address")?;
                if !address.starts_with("0x")
                    || address.len() > 66
                    || address[2..].is_empty()
                    || !address[2..].bytes().all(|b| b.is_ascii_hexdigit())
                    || !addresses.insert(address.clone())
                {
                    return Err(invalid("ambiguous layer address"));
                }
                let number = |field: &str| {
                    item[field]
                        .as_f64()
                        .filter(|n| n.is_finite() && n.abs() <= 1_000_000.0)
                        .ok_or_else(|| invalid("invalid layer geometry"))
                };
                let rect = Rect {
                    origin: crate::gestures::Point {
                        x: number("x")?,
                        y: number("y")?,
                    },
                    size: crate::gestures::Point {
                        x: number("w")?,
                        y: number("h")?,
                    },
                };
                if rect.size.x <= 0.0 || rect.size.y <= 0.0 {
                    continue;
                }
                if !rect.valid() || !geometry.rect().contains_rect(rect) {
                    return Err(invalid("layer layout leaves its output"));
                }
                let pid = match item["pid"].as_i64() {
                    Some(-1) => None,
                    Some(pid) if pid > 0 => {
                        Some(u32::try_from(pid).map_err(|_| invalid("invalid layer PID"))?)
                    }
                    _ => return Err(invalid("invalid layer PID")),
                };
                result.push(Layer {
                    output: output.clone(),
                    address,
                    namespace: text("namespace")?,
                    pid,
                    level,
                    rect,
                });
            }
        }
    }
    Ok(result)
}
/// Longest accepted map-in animation; a slower setting yields no bound.
const MAX_MAP_IN: Duration = Duration::from_secs(10);

/// Stock layer inventory with the compositor's bound on a newly mapped
/// layer's position animation, read in the same batch.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Inventory {
    pub layers: Vec<Layer>,
    pub map_in: Option<Duration>,
}
/// The global `animations:enabled` switch from a stock `getoption` reply.
fn animations_enabled(option: &Value) -> Option<bool> {
    if option["option"] != "animations:enabled" {
        return None;
    }
    option["int"]
        .as_i64()
        .map(|value| value != 0)
        .or_else(|| option["bool"].as_bool())
}
/// Effective `layersIn` duration. The tick warps every value while animations
/// or the node are disabled; otherwise `speed` tenths of a second, which only
/// a listed cubic bezier guarantees; springs and unknown curves have none.
pub(crate) fn map_in(value: &Value, enabled: Option<bool>) -> Option<Duration> {
    let [nodes, beziers] = value.as_array()?.as_slice() else {
        return None;
    };
    let mut matching = nodes
        .as_array()?
        .iter()
        .filter(|node| node["name"] == "layersIn");
    let node = matching.next()?;
    if matching.next().is_some() {
        return None;
    }
    if !node["enabled"].as_bool()? || enabled == Some(false) {
        return Some(Duration::ZERO);
    }
    let bezier = node["bezier"].as_str()?;
    let speed = node["speed"]
        .as_f64()
        .filter(|speed| speed.is_finite() && *speed > 0.0)?;
    if !beziers
        .as_array()?
        .iter()
        .any(|curve| curve["name"] == bezier)
    {
        return None;
    }
    // Replies round speed to hundredths.
    let bound = Duration::try_from_secs_f64((speed + 0.005) / 10.0).ok()?;
    (bound <= MAX_MAP_IN).then_some(bound)
}
pub(crate) fn sample(
    client: &Client,
    outputs: &[OutputState],
    until: Instant,
    inhibited: &mut bool,
) -> io::Result<Inventory> {
    let bytes = client.exchange(
        "[[BATCH]]j/locked;j/monitors;j/layers;j/locked;j/animations;j/getoption animations:enabled",
        until,
    )?;
    decode(&bytes, outputs, inhibited)
}
fn decode(bytes: &[u8], outputs: &[OutputState], inhibited: &mut bool) -> io::Result<Inventory> {
    let mut replies = serde_json::Deserializer::from_slice(bytes).into_iter::<Value>();
    let mut next = || {
        replies
            .next()
            .ok_or_else(|| invalid("incomplete layer batch"))?
            .map_err(io::Error::other)
    };
    *inhibited |= super::context::locked(&next()?)?;
    if *inhibited {
        return Err(invalid("layer inventory witnessed compositor lock"));
    }
    let monitors = next()?;
    let layers = next()?;
    *inhibited |= super::context::locked(&next()?)?;
    if *inhibited {
        return Err(invalid("layer inventory lacks unlocked context"));
    }
    let trailing: Vec<_> = replies.by_ref().take(3).collect();
    let map_in = match trailing.as_slice() {
        [Ok(animations), Ok(option)] => map_in(animations, animations_enabled(option)),
        [Ok(animations)] | [Ok(animations), Err(_)] => map_in(animations, None),
        [Ok(_), Ok(_), _] => return Err(invalid("layer inventory has trailing replies")),
        _ => None,
    };
    let topology = super::context::parse_outputs(&monitors)?;
    if topology.len() != outputs.len()
        || outputs.iter().any(|o| {
            !topology
                .iter()
                .any(|t| t.geometry == o.geometry && t.screen == o.screen)
        })
    {
        return Err(invalid("layer inventory output topology changed"));
    }
    Ok(Inventory {
        layers: parse(&layers, outputs)?,
        map_in,
    })
}

#[cfg(test)]
mod tests;
