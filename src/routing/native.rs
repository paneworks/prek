use super::*;
use crate::{
    config::{Action, WindowAction},
    protocol::ReorderScope,
};
const MAX_CLAIMS: usize = 4096;
pub(super) struct Claim {
    source: Source,
    target: Target,
    scope: Option<ReorderScope>,
}
/// What a native proof is bound to: a window, or an output's workspace.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Target {
    Window(String),
    Workspace(String, i64),
}
pub(crate) fn required(action: &Action) -> bool {
    matches!(
        action,
        Action::Hyprland {
            action: WindowAction::ReorderUp | WindowAction::ReorderDown | WindowAction::ScrollStack
        }
    )
}
/// The native target of a frozen window, output and workspace selection.
pub(crate) fn target(
    action: &Action,
    window: Option<&str>,
    output: Option<&str>,
    workspace: Option<i64>,
) -> Option<Target> {
    match action {
        Action::Hyprland {
            action: WindowAction::ScrollStack,
        } => output
            .zip(workspace)
            .filter(|_| window.is_none())
            .map(|(output, workspace)| Target::Workspace(output.into(), workspace)),
        _ => window
            .filter(|_| action.requires_hyprland())
            .map(|window| Target::Window(window.into())),
    }
}
pub(crate) fn event_target(event: &Event) -> Option<Target> {
    match event {
        Event::Matched {
            action,
            window,
            output,
            workspace,
            ..
        } => target(action, window.as_deref(), output.as_deref(), *workspace),
        _ => None,
    }
}
impl Routing {
    fn native_capable(&self, at: Duration) -> bool {
        self.capability_generation == self.authority_generation
            && at < self.capability_expires
            && self.capabilities.contains(&Capability::HyprlandActions)
    }
    pub(super) fn retire_native(&mut self, at: Duration) {
        let capable = self.native_capable(at);
        let authority = self.engine.authority();
        let mut cache: BTreeMap<Target, Option<ReorderScope>> = BTreeMap::new();
        let mut current = |target: Target| {
            cache
                .entry(target)
                .or_insert_with_key(|target| {
                    if capable {
                        authority.native_scope(target, at)
                    } else {
                        None
                    }
                })
                .clone()
        };
        for claim in self.native_claims.values_mut() {
            if claim.scope.is_some() && claim.scope != current(claim.target.clone()) {
                claim.scope = None;
            }
        }
        for dispatch in &mut self.dispatches {
            if dispatch.reorder_scope.is_some()
                && dispatch.reorder_scope
                    != dispatch
                        .outcome
                        .intent
                        .native_target()
                        .and_then(&mut current)
            {
                dispatch.reorder_scope = None;
            }
        }
    }
    pub(super) fn capture_native<'a>(
        &mut self,
        intents: impl Iterator<Item = &'a RouteIntent>,
        at: Duration,
    ) {
        self.retire_native(at);
        let capable = self.native_capable(at);
        let mut keep = BTreeSet::new();
        let mut pending = Vec::new();
        for intent in intents {
            let Some(target) = intent.native_target() else {
                continue;
            };
            if self.native_claims.contains_key(&intent.id()) {
                keep.insert(intent.id());
            } else if pending.len() < MAX_CLAIMS {
                pending.push((intent, target));
            }
        }
        keep.extend(
            self.won
                .keys()
                .copied()
                .filter(|id| self.native_claims.contains_key(id)),
        );
        keep.extend(
            self.dispatches
                .iter()
                .map(|dispatch| dispatch.outcome.intent.id())
                .filter(|id| self.native_claims.contains_key(id)),
        );
        keep.extend(
            self.deferred_custom
                .iter()
                .map(|outcome| outcome.intent.id())
                .filter(|id| self.native_claims.contains_key(id)),
        );
        self.native_claims.retain(|id, _| keep.contains(id));
        for (intent, target) in pending {
            if self.native_claims.len() < MAX_CLAIMS
                && !self.native_claims.contains_key(&intent.id())
            {
                let scope = capable
                    .then(|| self.engine.authority().native_scope(&target, at))
                    .flatten();
                self.native_claims.insert(
                    intent.id(),
                    Claim {
                        source: intent.source().clone(),
                        target,
                        scope,
                    },
                );
            }
        }
    }
    pub(super) fn native_scope(&self, intent: &RouteIntent) -> Option<ReorderScope> {
        let claim = self.native_claims.get(&intent.id())?;
        (claim.source == *intent.source() && Some(&claim.target) == intent.native_target().as_ref())
            .then(|| claim.scope.clone())
            .flatten()
    }
    pub(super) fn remove_native(&mut self, source: &Source) {
        self.native_claims
            .retain(|_, claim| &claim.source != source);
    }
    pub(super) fn clear_native(&mut self) {
        for claim in self.native_claims.values_mut() {
            claim.scope = None;
        }
        for dispatch in &mut self.dispatches {
            dispatch.reorder_scope = None;
        }
    }
}
