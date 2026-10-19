//! Settled Morf frames as routing context. A settled shell layer's committed
//! input region replaces its coarse layer box, its semantic nodes become
//! scopes, and its registered keyboard becomes known on an exclusive output.
use super::*;
use crate::{
    config::Scope,
    context::{Keyboard, Region as ContextRegion, Snapshot},
};

fn scope(role: SemanticRole) -> Option<Scope> {
    Some(match role {
        SemanticRole::ClockBackground => Scope::ClockBackground,
        SemanticRole::ContentBottom => Scope::ContentBottom,
        SemanticRole::PhysicalBottom => Scope::PhysicalBottom,
        SemanticRole::TopLeft => Scope::TopLeft,
        SemanticRole::TopRight => Scope::TopRight,
        SemanticRole::SideLeft => Scope::SideLeft,
        SemanticRole::SideRight => Scope::SideRight,
        SemanticRole::Keyboard | SemanticRole::SecureOverlay | SemanticRole::LocalControl => {
            return None;
        }
    })
}

/// Refines `snapshot` with every fresh view. An unsettled, undescribed or
/// inconsistent view leaves the coarse layer box in place; a visible secure
/// overlay restricts the snapshot either way.
pub(crate) fn apply(snapshot: &mut Snapshot, views: &[OutputContext], now: Duration) {
    for view in views {
        let Some(frame) = view
            .context
            .frame
            .as_ref()
            .filter(|_| view.context.fresh(now))
        else {
            continue;
        };
        if frame.regions.iter().any(|region| {
            region
                .semantic
                .as_ref()
                .is_some_and(|semantic| semantic.role == SemanticRole::SecureOverlay)
        }) {
            snapshot.secure_overlay = true;
        }
        let mut next = snapshot.clone();
        if refine(&mut next, view, frame, now).is_some() && next.validate().is_ok() {
            *snapshot = next;
        }
    }
}

fn refine(
    snapshot: &mut Snapshot,
    view: &OutputContext,
    frame: &Frame,
    now: Duration,
) -> Option<()> {
    let layout = view.layout.as_ref().filter(|layout| layout.settled())?;
    let input = layout.input.as_ref()?;
    let index = snapshot
        .outputs
        .iter()
        .position(|output| output.geometry.name == view.output)?;
    layout
        .validate(&view.context, &snapshot.outputs[index], now)
        .ok()?;
    let coarse = format!("layer:{}:{}", view.output, layout.surface.address);
    let position = snapshot
        .regions
        .iter()
        .position(|region| region.id == coarse)?;
    snapshot.regions.remove(position);
    let epoch = &view.context.epoch;
    let region = |id: String, scope, rect, interactive| ContextRegion {
        id,
        scope,
        output: view.output.clone(),
        rect,
        interactive,
    };
    for (index, rect) in input.iter().enumerate() {
        let id = format!("morf:{epoch}:input:{index}");
        snapshot.regions.push(region(id, None, *rect, true));
    }
    for projected in &layout.regions {
        let (Some(rect), Some(semantic)) = (projected.bounds, &projected.semantic) else {
            continue;
        };
        let id = format!("morf:{epoch}:node:{}", projected.node);
        if semantic.role == SemanticRole::LocalControl {
            snapshot.regions.push(region(id, None, rect, true));
        } else if let Some(scope) = scope(semantic.role) {
            snapshot
                .regions
                .push(region(id, Some(scope), rect, projected.interactive));
        }
    }
    let output = &mut snapshot.outputs[index];
    if let Some(registry) = frame.keyboard.as_ref().filter(|_| layout.exclusive)
        && !output.keyboard_known
    {
        match registry.state {
            KeyboardState::Hidden => {
                output.keyboard_known = true;
                output.keyboard = None;
            }
            KeyboardState::Visible => {
                let node = registry.node.as_deref()?.parse::<u64>().ok()?;
                let rect = layout
                    .regions
                    .iter()
                    .find(|region| region.node == node)?
                    .bounds?;
                output.keyboard_known = true;
                output.keyboard = Some(Keyboard {
                    rect,
                    layout: registry.layout.clone()?,
                });
            }
            KeyboardState::Unknown => {}
        }
    }
    Some(())
}

#[cfg(test)]
pub(crate) mod tests;
