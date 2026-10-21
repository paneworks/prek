//! Frozen adjacent-column permutations for the native scrolling layout.
use super::{invalid, layouts};
use crate::{
    config::{Scope, WindowAction},
    context::{OutputState, Snapshot, Window},
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt::Write, io};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Plan {
    pub(crate) source: Window,
    pub(crate) output: OutputState,
    workspace: layouts::Workspace,
    neighbor: String,
}
impl Plan {
    pub(crate) fn freeze(
        context: &Snapshot,
        layouts: &layouts::Snapshot,
        id: &str,
        action: WindowAction,
    ) -> io::Result<Self> {
        layouts.validate_context(context)?;
        let up = match action {
            WindowAction::ReorderUp => true,
            WindowAction::ReorderDown => false,
            _ => return Err(invalid("action is not a vertical reorder")),
        };
        let source = context
            .windows
            .iter()
            .find(|window| window.id == id)
            .ok_or_else(|| invalid("reorder source is unavailable"))?;
        if !context.hyprland_window_target_available(
            Scope::Everywhere,
            Some(id),
            Some(&source.output),
            Some(source.workspace),
        ) {
            return Err(invalid(
                "reorder source requires an active desktop workspace",
            ));
        }
        let output = context
            .outputs
            .iter()
            .find(|output| output.geometry.name == source.output)
            .unwrap();
        let workspace = layouts
            .workspaces
            .iter()
            .find(|workspace| workspace.id == source.workspace)
            .ok_or_else(|| invalid("reorder workspace is unavailable"))?;
        if workspace.layout != "scrolling"
            || !workspace.active
            || workspace.special
            || workspace.columns.len() < 2
        {
            return Err(invalid("reorder requires adjacent scrolling columns"));
        }
        let windows: BTreeMap<_, _> = workspace
            .windows
            .iter()
            .map(|window| (window.id.as_str(), window))
            .collect();
        let native = windows[id];
        if !eligible(native) {
            return Err(invalid("reorder source is not an ordinary tiled app"));
        }
        let bands: Vec<_> = workspace
            .columns
            .iter()
            .map(|column| {
                column.members.iter().fold(
                    (f64::INFINITY, f64::NEG_INFINITY),
                    |(start, end), id| {
                        let rect = windows[id.as_str()].rect;
                        (start.min(rect[1]), end.max(rect[1] + rect[3]))
                    },
                )
            })
            .collect();
        let forward = bands.windows(2).all(|pair| pair[0].1 <= pair[1].0);
        let reverse = bands.windows(2).all(|pair| pair[0].0 >= pair[1].1);
        if !forward && !reverse {
            return Err(invalid(
                "scrolling columns have no unambiguous vertical order",
            ));
        }
        let position = native
            .column
            .ok_or_else(|| invalid("reorder source has no column"))?;
        let next = if up == forward {
            position.index.checked_sub(1)
        } else {
            position.index.checked_add(1)
        }
        .and_then(|index| workspace.columns.get(index))
        .ok_or_else(|| invalid("reorder source is at the stack boundary"))?;
        let center = native.rect[0] + native.rect[2] / 2.;
        let mut candidates: Vec<_> = next
            .members
            .iter()
            .filter_map(|id| {
                let window = windows[id.as_str()];
                let overlap = (native.rect[0] + native.rect[2])
                    .min(window.rect[0] + window.rect[2])
                    - native.rect[0].max(window.rect[0]);
                (overlap > 0.).then_some((
                    (window.rect[0] + window.rect[2] / 2. - center).abs(),
                    window,
                ))
            })
            .collect();
        candidates.sort_by(|a, b| a.0.total_cmp(&b.0));
        let Some((distance, neighbor)) = candidates.first() else {
            return Err(invalid("adjacent column has no aligned app"));
        };
        if candidates.get(1).is_some_and(|next| next.0 == *distance) || !eligible(neighbor) {
            return Err(invalid("adjacent app is ambiguous or unavailable"));
        }
        let mut workspace = workspace.clone();
        workspace.windows.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Self {
            source: source.clone(),
            output: output.clone(),
            workspace,
            neighbor: neighbor.id.clone(),
        })
    }
    pub(crate) fn matches(&self, context: &Snapshot, layouts: &layouts::Snapshot) -> bool {
        layouts.validate_context(context).is_ok()
            && context.outputs.iter().any(|output| {
                output.geometry == self.output.geometry
                    && output.screen == self.output.screen
                    && output.active_workspace == self.output.active_workspace
            })
            && layouts
                .workspaces
                .iter()
                .find(|workspace| workspace.id == self.workspace.id)
                .is_some_and(|workspace| {
                    let mut workspace = workspace.clone();
                    workspace.windows.sort_by(|a, b| a.id.cmp(&b.id));
                    workspace == self.workspace
                })
    }
    pub(crate) fn confirmed(&self, context: &Snapshot, layouts: &layouts::Snapshot) -> bool {
        let mut expected = self.clone();
        let first = expected
            .workspace
            .windows
            .iter()
            .position(|window| window.id == self.source.id)
            .unwrap();
        let second = expected
            .workspace
            .windows
            .iter()
            .position(|window| window.id == self.neighbor)
            .unwrap();
        let source = expected.workspace.windows[first].clone();
        let neighbor = expected.workspace.windows[second].clone();
        expected.workspace.windows[first].rect = neighbor.rect;
        expected.workspace.windows[first].column = neighbor.column;
        expected.workspace.windows[second].rect = source.rect;
        expected.workspace.windows[second].column = source.column;
        for column in &mut expected.workspace.columns {
            for member in &mut column.members {
                if *member == self.source.id {
                    *member = self.neighbor.clone();
                } else if *member == self.neighbor {
                    *member = self.source.id.clone();
                }
            }
        }
        let Some(observed) = layouts
            .workspaces
            .iter()
            .find(|workspace| workspace.id == self.workspace.id)
        else {
            return false;
        };
        for window in &mut expected.workspace.windows {
            let Some(observed) = observed
                .windows
                .iter()
                .find(|observed| observed.id == window.id)
            else {
                return false;
            };
            window.rect = observed.rect;
        }
        expected.matches(context, layouts)
    }
    pub(super) fn native_guard(&self) -> String {
        let (neighbor, stable) = self.neighbor.split_once(':').unwrap();
        format!(
            "{}local v=native['{neighbor}'];assert(v and v.stable_id==0x{stable},'Prek reorder neighbor retired')\n",
            workspace_guard(&self.workspace, "reorder")
        )
    }
}
/// Lua asserting that `workspace` still has exactly these mapped windows,
/// goal rectangles, app states, column order, widths and lifetimes. It
/// leaves the observed windows in `native`, keyed by address.
pub(super) fn workspace_guard(workspace: &layouts::Workspace, purpose: &str) -> String {
    let mut expected = String::new();
    for window in &workspace.windows {
        let (address, stable) = window.id.split_once(':').unwrap();
        let [x, y, width, height] = window.rect;
        let position = window
            .column
            .map_or_else(|| "false".into(), |p| format!("{{{}, {}}}", p.index, p.row));
        write!(expected,"{{address='{address}',stable=0x{stable},rect={{{x:?},{y:?},{width:?},{height:?}}},hidden={},floating={},grouped={},fullscreen={},column={position}}},",window.hidden,window.floating,window.grouped,window.fullscreen).unwrap();
    }
    let mut columns = String::new();
    for column in &workspace.columns {
        write!(
            columns,
            "{{index={},width={:?},members={{",
            column.index, column.width
        )
        .unwrap();
        for id in &column.members {
            let (address, stable) = id.split_once(':').unwrap();
            write!(columns, "{{address='{address}',stable=0x{stable}}},").unwrap();
        }
        columns.push_str("}},");
    }
    let id = workspace.id;
    let monitor = workspace.monitor.unwrap();
    format!(
        r#"
local ws=hl.get_workspace({id})
assert(ws and ws.active and not ws.special and ws.tiled_layout=='scrolling' and ws.monitor and ws.monitor.id=={monitor},'Prek {purpose} workspace changed')
local expected={{{expected}}}
local current=hl.get_workspace_windows(ws)
assert(#current==#expected,'Prek {purpose} membership changed')
local native={{}}
for _,o in ipairs(current) do native[o.address]=o end
for _,e in ipairs(expected) do
 local o=native[e.address]
 assert(o and o.stable_id==e.stable and o.mapped and o.workspace and o.workspace.id=={id} and o.monitor and o.monitor.id=={monitor},'Prek {purpose} lifetime changed')
 local p,s=o.at,o.size
 assert(p.x==e.rect[1] and p.y==e.rect[2] and s.x==e.rect[3] and s.y==e.rect[4],'Prek {purpose} geometry changed')
 assert(o.hidden==e.hidden and o.floating==e.floating and (o.group~=nil)==e.grouped and o.fullscreen==e.fullscreen,'Prek {purpose} app state changed')
 local layout=o.layout;local col=layout and layout.column
 if e.column then assert(col and col.index==e.column[1] and layout.index_in_column==e.column[2],'Prek {purpose} position changed')
 else assert(not col,'Prek {purpose} tiling changed') end
end
local columns={{{columns}}}
for _,e in ipairs(columns) do
 local col=native[e.members[1].address].layout.column
 assert(col.index==e.index and col.width==e.width and #col.windows==#e.members,'Prek {purpose} column changed')
 for i,member in ipairs(e.members) do
  local o=col.windows[i]
  assert(o and o.address==member.address and o.stable_id==member.stable,'Prek {purpose} column lifetime changed')
 end
end
"#
    )
}
fn eligible(window: &layouts::Window) -> bool {
    !window.hidden
        && !window.floating
        && !window.grouped
        && window.fullscreen == 0
        && window.column.is_some()
}
pub(crate) fn scope(
    context: &Snapshot,
    layouts: &layouts::Snapshot,
    id: &str,
) -> Option<crate::protocol::ReorderScope> {
    if layouts.continuity == 0 {
        return None;
    }
    let plan = Plan::freeze(context, layouts, id, WindowAction::ReorderUp)
        .or_else(|_| Plan::freeze(context, layouts, id, WindowAction::ReorderDown))
        .ok()?;
    let window = &plan.source;
    let value = serde_json::to_value((
        context.generation,
        layouts.continuity,
        &window.id,
        &window.output,
        window.workspace,
        window.rect,
        window.content,
        window.mapped,
        window.visible,
        window.z_order,
        &plan.output,
        &plan.workspace,
    ))
    .ok()?;
    Some(crate::protocol::ReorderScope {
        fingerprint: canonical_hash(b"prek.native.reorder.v1\0", value)?,
    })
}
/// The native proof of a reorder window or a scrolled workspace.
pub(crate) fn native_scope(
    context: &Snapshot,
    layouts: &layouts::Snapshot,
    target: &crate::routing::native::Target,
) -> Option<crate::protocol::ReorderScope> {
    match target {
        crate::routing::native::Target::Window(window) => scope(context, layouts, window),
        crate::routing::native::Target::Workspace(output, workspace) => {
            super::scrolling::profile::scope(context, layouts, output, *workspace)
        }
    }
}
pub(super) fn continuity_signature(
    context: &Snapshot,
    layouts: &layouts::Snapshot,
) -> Option<[u8; 32]> {
    let mut outputs = context.outputs.clone();
    outputs.sort_by(|a, b| a.geometry.name.cmp(&b.geometry.name));
    let mut windows = context.windows.clone();
    windows.sort_by(|a, b| a.id.cmp(&b.id));
    for window in &mut windows {
        window.focused = false;
    }
    let mut workspaces = layouts.workspaces.clone();
    workspaces.sort_by_key(|workspace| workspace.id);
    for workspace in &mut workspaces {
        workspace.windows.sort_by(|a, b| a.id.cmp(&b.id));
    }
    let value = serde_json::to_value((context.generation, outputs, windows, workspaces)).ok()?;
    canonical_hash(b"prek.native.continuity.v1\0", value)
}
pub(super) fn canonical_hash(domain: &[u8], mut value: serde_json::Value) -> Option<[u8; 32]> {
    fn normalize(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Number(number) if number.as_f64() == Some(0.) => {
                *value = serde_json::json!(0)
            }
            serde_json::Value::Array(values) => values.iter_mut().for_each(normalize),
            serde_json::Value::Object(values) => values.values_mut().for_each(normalize),
            _ => {}
        }
    }
    normalize(&mut value);
    let bytes = serde_json::to_vec(&value).ok()?;
    if bytes.len() > 1024 * 1024 {
        return None;
    }
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes);
    Some(digest.finalize().into())
}
pub(crate) fn available(
    context: &Snapshot,
    layouts: Option<&layouts::Snapshot>,
    action: &crate::config::Action,
    scope: Scope,
    window: Option<&str>,
    output: Option<&str>,
    workspace: Option<i64>,
) -> bool {
    match action {
        crate::config::Action::Hyprland {
            action: action @ (WindowAction::ReorderUp | WindowAction::ReorderDown),
        } => {
            action.supported_session()
                && context.hyprland_window_target_available(scope, window, output, workspace)
                && window.zip(layouts).is_some_and(|(window, layouts)| {
                    Plan::freeze(context, layouts, window, *action).is_ok()
                })
        }
        crate::config::Action::Hyprland {
            action: action @ WindowAction::ScrollStack,
        } => {
            action.supported_session()
                && matches!(scope, Scope::SideLeft | Scope::SideRight)
                && window.is_none()
                && context.state == crate::context::State::Desktop
                && !context.secure_overlay
                && output.zip(workspace).zip(layouts).is_some_and(
                    |((output, workspace), layouts)| {
                        super::scrolling::profile::available(context, layouts, output, workspace)
                            .is_ok()
                    },
                )
        }
        _ => context.hyprland_target_available(action, scope, window, output, workspace),
    }
}

#[cfg(test)]
pub(crate) mod tests;
