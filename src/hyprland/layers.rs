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
pub(crate) fn sample(
    client: &Client,
    outputs: &[OutputState],
    until: Instant,
    inhibited: &mut bool,
) -> io::Result<Vec<Layer>> {
    let bytes = client.exchange("[[BATCH]]j/locked;j/monitors;j/layers;j/locked", until)?;
    decode(&bytes, outputs, inhibited)
}
fn decode(bytes: &[u8], outputs: &[OutputState], inhibited: &mut bool) -> io::Result<Vec<Layer>> {
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
    if *inhibited || replies.next().is_some() {
        return Err(invalid("layer inventory lacks unlocked context"));
    }
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
    parse(&layers, outputs)
}

#[cfg(test)]
mod tests;
