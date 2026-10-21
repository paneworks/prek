//! The validated phone scrolling profile. Stock IPC exposes no camera offset,
//! so G11 is refused unless the workspace matches a profile in which window
//! goal rectangles determine it: a downward scrolling layout without gaps,
//! borders or workspace rules, whose tiled windows exactly tile their strips.
use super::{Direction, Viewport};
use crate::{
    config::Screen,
    context::{OutputState, Snapshot},
    hyprland::layouts::{self, Workspace},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, io};

/// Stock coordinates are truncated to integers; two truncations bound the
/// difference between an observed and a derived coordinate.
const TOLERANCE: f64 = 2.0;
const MAX_OUTPUTS: usize = 256;

/// Why a workspace is outside the validated profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    Layout,
    Direction,
    Gaps,
    Border,
    WorkspaceRules,
    Window,
    Columns,
    Geometry,
    Unsampled,
}

/// Stock settings that fix the profile, read in one locked batch.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub direction: String,
    pub gaps_in: [i64; 4],
    pub gaps_out: [i64; 4],
    pub border: i64,
    pub workspace_rules: usize,
    pub reserved: Vec<Reserved>,
}

/// An output's reserved `[left, top, right, bottom]` area.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reserved {
    pub output: String,
    pub area: [f64; 4],
}
impl Settings {
    /// Rejects values outside stock bounds and repeated outputs.
    pub fn validate(&self) -> io::Result<()> {
        let text = |value: &str, limit| {
            !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
        };
        let mut outputs = BTreeSet::new();
        if !text(&self.direction, 64)
            || self
                .gaps_in
                .iter()
                .chain(&self.gaps_out)
                .chain([&self.border])
                .any(|value| !(0..=1_000_000).contains(value))
            || self.workspace_rules > 4096
            || self.reserved.len() > MAX_OUTPUTS
            || self.reserved.iter().any(|reserved| {
                !text(&reserved.output, 256)
                    || !outputs.insert(reserved.output.as_str())
                    || reserved
                        .area
                        .iter()
                        .any(|n| !(n.is_finite() && (0.0..=1_000_000.0).contains(n)))
            })
        {
            return Err(invalid("invalid scrolling profile settings"));
        }
        Ok(())
    }
    /// Decodes `getoption` replies for direction, inner and outer gaps and
    /// border size, the `workspacerules` list and the `monitors` list.
    pub fn decode(replies: [&Value; 6]) -> io::Result<Self> {
        let [direction, gaps_in, gaps_out, border, rules, monitors] = replies;
        fn option<'a>(reply: &'a Value, name: &str) -> io::Result<&'a Value> {
            (reply["option"] == name)
                .then_some(reply)
                .ok_or_else(|| invalid("unexpected profile option reply"))
        }
        let gaps = |reply: &Value, name: &str| -> io::Result<[i64; 4]> {
            let text = option(reply, name)?["css"]
                .as_str()
                .or_else(|| reply["custom"].as_str())
                .ok_or_else(|| invalid("invalid gap option"))?;
            let values: Vec<i64> = text
                .split_whitespace()
                .map(str::parse)
                .collect::<Result<_, _>>()
                .map_err(|_| invalid("invalid gap option"))?;
            values.try_into().map_err(|_| invalid("invalid gap option"))
        };
        let reserved = monitors
            .as_array()
            .filter(|monitors| monitors.len() <= MAX_OUTPUTS)
            .ok_or_else(|| invalid("invalid monitor collection"))?
            .iter()
            .map(|monitor| {
                let name = monitor["name"]
                    .as_str()
                    .ok_or_else(|| invalid("invalid monitor name"))?;
                let values = monitor["reserved"]
                    .as_array()
                    .filter(|values| values.len() == 4)
                    .ok_or_else(|| invalid("invalid reserved area"))?;
                let mut area = [0.0; 4];
                for (slot, value) in area.iter_mut().zip(values) {
                    *slot = value
                        .as_f64()
                        .filter(|n| n.is_finite() && (0.0..=1_000_000.0).contains(n))
                        .ok_or_else(|| invalid("invalid reserved area"))?;
                }
                Ok(Reserved {
                    output: name.to_owned(),
                    area,
                })
            })
            .collect::<io::Result<_>>()?;
        let settings = Self {
            direction: option(direction, "scrolling:direction")?["str"]
                .as_str()
                .ok_or_else(|| invalid("invalid scrolling direction"))?
                .to_owned(),
            gaps_in: gaps(gaps_in, "general:gaps_in")?,
            gaps_out: gaps(gaps_out, "general:gaps_out")?,
            border: option(border, "general:border_size")?["int"]
                .as_i64()
                .ok_or_else(|| invalid("invalid border size"))?,
            workspace_rules: rules
                .as_array()
                .ok_or_else(|| invalid("invalid workspace rules"))?
                .len(),
            reserved,
        };
        settings.validate()?;
        Ok(settings)
    }
}

/// Reads the profile settings in one batch bracketed by lock witnesses.
pub(crate) fn sample(
    client: &crate::hyprland::Client,
    until: std::time::Instant,
    inhibited: &mut bool,
) -> io::Result<Settings> {
    let bytes = client.exchange(
        "[[BATCH]]j/locked;j/getoption scrolling:direction;j/getoption general:gaps_in;\
         j/getoption general:gaps_out;j/getoption general:border_size;j/workspacerules;\
         j/monitors;j/locked",
        until,
    )?;
    decode(&bytes, inhibited)
}
fn decode(bytes: &[u8], inhibited: &mut bool) -> io::Result<Settings> {
    let replies = serde_json::Deserializer::from_slice(bytes)
        .into_iter::<Value>()
        .take(9)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;
    let [
        first,
        direction,
        gaps_in,
        gaps_out,
        border,
        rules,
        monitors,
        last,
    ] = replies.as_slice()
    else {
        return Err(invalid("incomplete or trailing profile batch"));
    };
    for witness in [first, last] {
        *inhibited |= crate::hyprland::context::locked(witness)?;
    }
    if *inhibited {
        return Err(invalid("profile batch witnessed compositor lock"));
    }
    Settings::decode([direction, gaps_in, gaps_out, border, rules, monitors])
}

/// The viewport of `workspace`, active on the lit `output` of one desktop
/// sample, from that sample's native graph and attached settings.
pub fn available(
    context: &Snapshot,
    layouts: &layouts::Snapshot,
    output: &str,
    workspace: i64,
) -> Result<Viewport, Refusal> {
    let state = context
        .outputs
        .iter()
        .find(|state| {
            state.geometry.name == output
                && state.screen == Screen::On
                && state.active_workspace == Some(workspace)
        })
        .ok_or(Refusal::Layout)?;
    let native = layouts
        .workspaces
        .iter()
        .find(|native| native.id == workspace && native.output.as_deref() == Some(output))
        .ok_or(Refusal::Layout)?;
    viewport(
        native,
        state,
        layouts.profile.as_ref().ok_or(Refusal::Unsampled)?,
    )
}

/// Derives the native viewport of `workspace` on `output`, or why the
/// profile does not hold.
pub fn viewport(
    workspace: &Workspace,
    output: &OutputState,
    settings: &Settings,
) -> Result<Viewport, Refusal> {
    if workspace.layout != "scrolling"
        || workspace.special
        || !workspace.active
        || workspace.output.as_deref() != Some(output.geometry.name.as_str())
    {
        return Err(Refusal::Layout);
    }
    if settings.direction != "down" {
        return Err(Refusal::Direction);
    }
    if settings.gaps_in != [0; 4] || settings.gaps_out != [0; 4] {
        return Err(Refusal::Gaps);
    }
    if settings.border != 0 {
        return Err(Refusal::Border);
    }
    if settings.workspace_rules != 0 {
        return Err(Refusal::WorkspaceRules);
    }
    if workspace.windows.is_empty()
        || workspace.windows.iter().any(|window| {
            window.hidden
                || window.floating
                || window.grouped
                || window.fullscreen != 0
                || window.column.is_none()
        })
    {
        return Err(Refusal::Window);
    }
    let columns = &workspace.columns;
    let members: usize = columns.iter().map(|column| column.members.len()).sum();
    if members != workspace.windows.len()
        || columns.iter().enumerate().any(|(index, column)| {
            column.index != index
                || column.members.is_empty()
                || !column.width.is_finite()
                || column.width <= 0.0
                || column.members.iter().enumerate().any(|(row, id)| {
                    workspace.windows.iter().all(|window| {
                        window.id != *id
                            || window
                                .column
                                .is_none_or(|p| p.index != index || p.row != row)
                    })
                })
        })
    {
        return Err(Refusal::Columns);
    }
    let reserved = settings
        .reserved
        .iter()
        .find(|reserved| reserved.output == output.geometry.name)
        .map(|reserved| reserved.area)
        .ok_or(Refusal::Geometry)?;
    let rect = output.rect();
    let [left, top, right, bottom] = reserved;
    let (x, y) = (rect.origin.x + left, rect.origin.y + top);
    let (width, height) = (rect.size.x - left - right, rect.size.y - top - bottom);
    if !(width > 0.0 && height > 0.0) {
        return Err(Refusal::Geometry);
    }
    let rect_of = |id: &String| {
        workspace
            .windows
            .iter()
            .find(|window| window.id == *id)
            .map(|window| window.rect)
            .ok_or(Refusal::Columns)
    };
    let offset = y - rect_of(&columns[0].members[0])?[1];
    let near = |a: f64, b: f64| (a - b).abs() <= TOLERANCE;
    let mut start = 0.0;
    for column in columns {
        let (top, extent) = (y + start - offset, height * column.width);
        let mut left = x;
        for id in &column.members {
            let [wx, wy, ww, wh] = rect_of(id)?;
            if !near(wy, top) || !near(wh, extent) || !near(wx, left) {
                return Err(Refusal::Geometry);
            }
            left = wx + ww;
        }
        if !near(left, x + width) {
            return Err(Refusal::Geometry);
        }
        start += extent;
    }
    let content = start;
    let excess = content - height;
    let (low, high) = if excess < 0.0 {
        let center = (excess / 2.0).round();
        (center, center)
    } else {
        (0.0, excess)
    };
    if offset < low - TOLERANCE || offset > high + TOLERANCE {
        return Err(Refusal::Geometry);
    }
    Viewport::new(Direction::Down, height, content, offset.clamp(low, high))
        .map_err(|_| Refusal::Geometry)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
