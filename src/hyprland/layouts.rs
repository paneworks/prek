//! Native mapped-window layout metadata from a closed, read-only Lua REPL query.
//! Goal rectangles are not rendered hit tests, camera bounds or execution grants.
use super::{Client, dispatch::Dialect, invalid};
use crate::{config::hyprland_window_address, context::Rect, gestures::Point};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io, time::Duration, time::Instant};

pub(super) const QUERY: &str = include_str!("layouts/query.lua");
const MAX_WORKSPACES: usize = 64;
const MAX_WINDOWS: usize = 512;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub generation: u64,
    pub revision: u64,
    pub captured_at: Duration,
    pub workspaces: Vec<Workspace>,
}
impl Snapshot {
    /// Validate the graph against the same conservatively stamped desktop sample.
    pub fn validate_context(&self, context: &crate::context::Snapshot) -> io::Result<()> {
        context.validate().map_err(io::Error::other)?;
        if self.generation == 0
            || self.generation != context.generation
            || self.revision == 0
            || self.revision != context.revision
            || self.captured_at != context.captured_at
            || context.state != crate::context::State::Desktop
            || context.secure_overlay
        {
            return Err(invalid(
                "native layouts differ from desktop sample identity",
            ));
        }
        validate_workspaces(&self.workspaces)?;
        let mut count = 0;
        for workspace in &self.workspaces {
            if let Some(name) = &workspace.output {
                let output = context
                    .outputs
                    .iter()
                    .find(|output| &output.geometry.name == name)
                    .ok_or_else(|| invalid("native layout output is unavailable"))?;
                if !workspace.special
                    && workspace.active != (output.active_workspace == Some(workspace.id))
                {
                    return Err(invalid(
                        "native active workspace differs from desktop sample",
                    ));
                }
            }
            for window in &workspace.windows {
                count += 1;
                let matched = context.windows.iter().any(|candidate| {
                    candidate.mapped
                        && candidate.id == window.id
                        && Some(&candidate.output) == workspace.output.as_ref()
                        && candidate.workspace == workspace.id
                        && window.rect
                            == [
                                candidate.rect.origin.x,
                                candidate.rect.origin.y,
                                candidate.rect.size.x,
                                candidate.rect.size.y,
                            ]
                });
                if !matched {
                    return Err(invalid("native window layout differs from desktop sample"));
                }
            }
        }
        for output in &context.outputs {
            if let Some(id) = output.active_workspace.filter(|id| *id > 0)
                && !self.workspaces.iter().any(|workspace| {
                    workspace.output.as_ref() == Some(&output.geometry.name)
                        && workspace.id == id
                        && workspace.active
                        && !workspace.special
                })
            {
                return Err(invalid("native layouts omit an active desktop workspace"));
            }
        }
        if count
            != context
                .windows
                .iter()
                .filter(|window| window.mapped)
                .count()
        {
            return Err(invalid("native layouts omit mapped desktop windows"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub id: i64,
    pub output: Option<String>,
    pub monitor: Option<i64>,
    pub active: bool,
    pub special: bool,
    pub layout: String,
    pub windows: Vec<Window>,
    pub columns: Vec<Column>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub id: String,
    pub rect: [f64; 4],
    pub hidden: bool,
    pub floating: bool,
    pub grouped: bool,
    pub fullscreen: u8,
    pub column: Option<Position>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub index: usize,
    pub row: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub index: usize,
    pub width: f64,
    pub members: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema: u32,
    workspaces: Vec<Workspace>,
}

fn text(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

fn parse(bytes: &[u8]) -> io::Result<Vec<Workspace>> {
    let document: Document = serde_json::from_slice(bytes).map_err(io::Error::other)?;
    if document.schema != 1 || document.workspaces.len() > MAX_WORKSPACES {
        return Err(invalid("unsupported or oversized native layout document"));
    }
    validate_workspaces(&document.workspaces)?;
    Ok(document.workspaces)
}

fn validate_workspaces(workspace_list: &[Workspace]) -> io::Result<()> {
    if workspace_list.len() > MAX_WORKSPACES {
        return Err(invalid("oversized native workspace inventory"));
    }
    let mut workspaces = BTreeSet::new();
    let mut windows = BTreeSet::new();
    let mut addresses = BTreeSet::new();
    let mut lifetimes = BTreeSet::new();
    let mut active = BTreeSet::new();
    for workspace in workspace_list {
        if workspace.id == 0
            || i32::try_from(workspace.id).is_err()
            || !workspaces.insert(workspace.id)
            || !text(&workspace.layout, 64)
            || workspace
                .output
                .as_ref()
                .is_some_and(|name| !text(name, 256))
            || workspace.output.is_some() != workspace.monitor.is_some()
            || workspace.monitor.is_some_and(|id| id < 0)
            || workspace.active && workspace.output.is_none()
            || workspace.active && !active.insert((&workspace.output, workspace.special))
            || workspace.windows.len() > MAX_WINDOWS
            || workspace.columns.len() > MAX_WINDOWS
            || workspace.layout != "scrolling" && !workspace.columns.is_empty()
        {
            return Err(invalid("invalid native workspace layout"));
        }
        let mut members = BTreeSet::new();
        for (index, column) in workspace.columns.iter().enumerate() {
            if column.index != index
                || !column.width.is_finite()
                || column.width <= 0.
                || column.width > 16.
                || column.members.is_empty()
                || column.members.len() > MAX_WINDOWS
            {
                return Err(invalid("invalid native scrolling column"));
            }
            for (row, member) in column.members.iter().enumerate() {
                if !members.insert(member)
                    || !workspace.windows.iter().any(|window| {
                        window.id == *member && window.column == Some(Position { index, row })
                    })
                {
                    return Err(invalid("inconsistent native column membership"));
                }
            }
        }
        for window in &workspace.windows {
            let [x, y, width, height] = window.rect;
            let address = hyprland_window_address(&window.id)
                .and_then(|address| u64::from_str_radix(&address[2..], 16).ok());
            let lifetime = window
                .id
                .split_once(':')
                .and_then(|(_, stable)| u64::from_str_radix(stable, 16).ok());
            if address.is_none()
                || lifetime.is_none()
                || !addresses.insert(address)
                || !lifetimes.insert(lifetime)
                || !windows.insert(&window.id)
                || windows.len() > MAX_WINDOWS
                || !(Rect {
                    origin: Point { x, y },
                    size: Point {
                        x: width,
                        y: height,
                    },
                })
                .valid()
                || window.fullscreen > 3
                || window.column.is_some() && (window.floating || workspace.layout != "scrolling")
                || workspace.layout == "scrolling"
                    && !window.floating
                    && !window.grouped
                    && window.column.is_none()
                || window.column.is_some() != members.contains(&window.id)
            {
                return Err(invalid("invalid native mapped-window layout"));
            }
        }
    }
    Ok(())
}

pub(super) fn sample(
    client: &Client,
    until: Instant,
    inhibited: &mut bool,
) -> io::Result<Vec<Workspace>> {
    if *inhibited {
        return Err(invalid("a completed lock-provider transition is required"));
    }
    if (
        client.version.major,
        client.version.minor,
        client.version.patch,
    ) != (0, 56, 2)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native layout queries require verified Hyprland 0.56.2 Lua",
        ));
    }
    *inhibited |= super::context::locked(&client.query_until("locked", until)?)?;
    if *inhibited {
        return Err(invalid("compositor lock inhibits native layout context"));
    }
    if client.dispatch_dialect_until(until)? != Dialect::Lua {
        return Err(invalid(
            "native layout queries require the Lua config provider",
        ));
    }
    let bytes = client.exchange(QUERY, until);
    let provider = client.dispatch_dialect_until(until);
    *inhibited |= super::context::locked(&client.query_until("locked", until)?)?;
    if *inhibited {
        return Err(invalid("compositor locked during native layout sampling"));
    }
    if provider? != Dialect::Lua {
        return Err(invalid("native layout config provider changed"));
    }
    parse(&bytes?)
}

#[cfg(test)]
mod tests;
