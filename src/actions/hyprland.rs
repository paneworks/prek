//! Typed, explicitly targeted stock Hyprland window operations.
//! Lua dispatch checks frozen native lifetimes and geometry before mutation.
//! Legacy address selectors retain a read/dispatch race.
use crate::{
    config::{Screen, WindowAction},
    context::{OutputState, SessionKind, Snapshot, State, Window},
    context_transfer::{FRESHNESS, View},
    hyprland::{Client, ContextSampler},
    ipc::monotonic,
    protocol::{Completion, Fence},
};
use std::{
    io,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(super) struct Target {
    window: Window,
    output: OutputState,
    reorder_up: Option<crate::hyprland::reorder::Plan>,
    reorder_down: Option<crate::hyprland::reorder::Plan>,
}
impl Target {
    pub(super) fn reorder(&self, action: WindowAction) -> Option<&crate::hyprland::reorder::Plan> {
        match action {
            WindowAction::ReorderUp => self.reorder_up.as_ref(),
            WindowAction::ReorderDown => self.reorder_down.as_ref(),
            _ => None,
        }
    }
}

pub(super) fn supported(action: WindowAction) -> bool {
    action.supported_session()
}

fn address(id: &str) -> io::Result<&str> {
    crate::config::hyprland_window_address(id)
        .ok_or_else(|| io::Error::other("invalid stock window lifetime identity"))
}

pub struct HyprlandAdapter {
    client: Client,
    dialect: crate::hyprland::dispatch::Dialect,
    sampler: ContextSampler,
    view: View,
}
impl HyprlandAdapter {
    /// The host supplies a broker-approved desktop binding and context.
    pub fn new(client: Client, view: View) -> io::Result<Self> {
        view.validate()?;
        let dialect = client.verify_window_actions()?;
        let adapter = Self {
            client,
            dialect,
            sampler: ContextSampler::default(),
            view,
        };
        adapter.current(monotonic()?)?;
        Ok(adapter)
    }
    pub(super) fn fence(&self) -> Fence {
        self.view.fence
    }
    pub(super) fn seat(&self) -> Option<&str> {
        self.view
            .context
            .as_ref()
            .map(|context| context.binding.seat.as_str())
    }
    pub fn update_context(&mut self, view: View) -> io::Result<()> {
        let valid = view.validate();
        if view.fence != self.view.fence
            || view.context.as_ref().map(|context| &context.binding)
                != self.view.context.as_ref().map(|context| &context.binding)
            || valid.is_err()
            || view.sampled_at > monotonic()?
        {
            self.view.context = None;
            return Err(io::Error::other(
                "Hyprland adapter context binding/fence changed",
            ));
        }
        self.view = view;
        Ok(())
    }
    pub(super) fn ready(&self, now: Duration) -> bool {
        self.current(now).is_ok()
    }
    pub(super) fn reorder_scope(
        &self,
        window: &str,
        now: Duration,
    ) -> Option<crate::protocol::ReorderScope> {
        self.current(now).ok()?;
        self.view.context.as_ref()?.reorder_scope(window)
    }
    fn current(&self, now: Duration) -> io::Result<&Snapshot> {
        let provided = self
            .view
            .context
            .as_ref()
            .ok_or_else(|| io::Error::other("Hyprland context unavailable"))?;
        if self.view.state != State::Desktop || self.sampler.inhibited()
            || provided.snapshot.state != State::Desktop || provided.snapshot.secure_overlay
            || provided.binding.kind != SessionKind::User
            // SAFETY: read-only current process credential query.
            || provided.binding.uid != unsafe {nix::libc::geteuid()}
            || !self.client.bound_to(&provided.binding)
            || now < self.view.sampled_at || now - self.view.sampled_at >= FRESHNESS
            || now < provided.snapshot.captured_at || now - provided.snapshot.captured_at >= FRESHNESS
        {
            return Err(io::Error::other(
                "Hyprland adapter requires fresh owning desktop context",
            ));
        }
        Ok(&provided.snapshot)
    }
    pub(super) fn target(
        &self,
        id: &str,
        output: &str,
        workspace: i64,
        now: Duration,
    ) -> io::Result<Target> {
        address(id)?;
        let snapshot = self.current(now)?;
        let window = snapshot
            .windows
            .iter()
            .find(|window| {
                window.id == id
                    && window.output == output
                    && window.workspace == workspace
                    && window.mapped
            })
            .ok_or_else(|| io::Error::other("frozen window is unavailable"))?;
        let output = snapshot
            .outputs
            .iter()
            .find(|state| state.geometry.name == output && state.screen == Screen::On)
            .ok_or_else(|| io::Error::other("frozen output is unavailable"))?;
        if window.workspace <= 0 || output.active_workspace != Some(window.workspace) {
            return Err(io::Error::other(
                "window must belong to the frozen active numeric workspace",
            ));
        }
        Ok(Target {
            window: window.clone(),
            output: output.clone(),
            reorder_up: self.reorder_plan(id, WindowAction::ReorderUp),
            reorder_down: self.reorder_plan(id, WindowAction::ReorderDown),
        })
    }
    fn reorder_plan(
        &self,
        id: &str,
        action: WindowAction,
    ) -> Option<crate::hyprland::reorder::Plan> {
        if self.dialect != crate::hyprland::dispatch::Dialect::Lua
            || (
                self.client.version().major,
                self.client.version().minor,
                self.client.version().patch,
            ) != (0, 56, 2)
        {
            return None;
        }
        let context = self.view.context.as_ref()?;
        crate::hyprland::reorder::Plan::freeze(
            &context.snapshot,
            context.layouts.as_ref()?,
            id,
            action,
        )
        .ok()
    }
    pub(super) fn execute(
        &mut self,
        action: WindowAction,
        target: &Target,
        expires: Duration,
    ) -> Completion {
        let mut sent = false;
        let result: io::Result<Completion> = (|| {
            let now = monotonic()?;
            let Some(remaining) = expires
                .checked_sub(now)
                .filter(|remaining| !remaining.is_zero())
            else {
                return Ok(Completion::Cancelled);
            };
            if !supported(action) {
                return Ok(Completion::Unavailable);
            }
            let current = self.target(
                &target.window.id,
                &target.window.output,
                target.window.workspace,
                now,
            )?;
            // Focus may change after a gesture begins. It never selects the target.
            if current.output != target.output || !same_window(&current.window, &target.window) {
                return Ok(Completion::Unavailable);
            }
            let until = Instant::now() + remaining.min(Duration::from_millis(200));
            if matches!(action, WindowAction::ReorderUp | WindowAction::ReorderDown) {
                return self.execute_reorder(action, target, expires, until, &mut sent);
            }
            let destination = action.workspace_destination(target.window.workspace);
            if action != WindowAction::Close && destination.is_none() {
                return Ok(Completion::Unavailable);
            }
            // A numeric destination can be bound to another monitor by
            // compositor workspace rules. Until that topology is imported,
            // adjacent moves are available only on a single-output desktop.
            if destination.is_some() && self.current(now)?.outputs.len() != 1 {
                return Ok(Completion::Unavailable);
            }
            let snapshot = self.sampler.sample_until(
                &self.client,
                self.view.fence.session,
                SessionKind::User,
                until,
            )?;
            if destination.is_some() && snapshot.outputs.len() != 1 {
                return Ok(Completion::Unavailable);
            }
            if !snapshot
                .windows
                .iter()
                .any(|window| same_window(window, &target.window))
                || !snapshot
                    .outputs
                    .iter()
                    .any(|output| same_output(output, &target.output))
            {
                return Ok(Completion::Unavailable);
            }
            if self.client.dispatch_dialect_until(until)? != self.dialect {
                return Ok(Completion::Unavailable);
            }
            if monotonic()? >= expires || Instant::now() >= until {
                return Ok(Completion::Cancelled);
            }
            let request = self
                .dialect
                .window(&target.window, &target.output, destination)?;
            // One mutation only; never retry after any request bytes were written.
            let reply = self.client.exchange_effect(&request, until, &mut sent)?;
            if reply != b"ok" && reply != b"ok\n" {
                return Ok(Completion::Uncertain);
            }
            loop {
                let snapshot = self.sampler.sample_until(
                    &self.client,
                    self.view.fence.session,
                    SessionKind::User,
                    until,
                )?;
                if monotonic()? >= expires {
                    return Ok(Completion::Uncertain);
                }
                let window = snapshot
                    .windows
                    .iter()
                    .find(|window| window.id == target.window.id);
                let confirmed = if let Some(destination) = destination {
                    window.is_some_and(|window| {
                        window.workspace == destination
                            && window.output == target.window.output
                            && window.focused
                    }) && snapshot.outputs.iter().any(|output| {
                        output.geometry == target.output.geometry
                            && output.screen == Screen::On
                            && output.active_workspace == Some(destination)
                    })
                } else {
                    window.is_none()
                };
                if confirmed {
                    return Ok(Completion::Applied);
                }
                if Instant::now() >= until {
                    return Ok(Completion::Uncertain);
                }
                std::thread::sleep(
                    Duration::from_millis(1).min(until.saturating_duration_since(Instant::now())),
                );
            }
        })();
        result.unwrap_or(if sent {
            Completion::Uncertain
        } else {
            Completion::Unavailable
        })
    }
    fn execute_reorder(
        &mut self,
        action: WindowAction,
        target: &Target,
        expires: Duration,
        until: Instant,
        sent: &mut bool,
    ) -> io::Result<Completion> {
        let Some(plan) = target.reorder(action) else {
            return Ok(Completion::Unavailable);
        };
        let provided = self.view.context.as_ref().unwrap();
        if !provided
            .layouts
            .as_ref()
            .is_some_and(|layouts| plan.matches(&provided.snapshot, layouts))
        {
            return Ok(Completion::Unavailable);
        }
        let (snapshot, layouts) = self.sampler.sample_with_layouts_until(
            &self.client,
            self.view.fence.session,
            SessionKind::User,
            until,
        )?;
        if !layouts
            .as_ref()
            .is_some_and(|layouts| plan.matches(&snapshot, layouts))
            || self.client.dispatch_dialect_until(until)? != crate::hyprland::dispatch::Dialect::Lua
        {
            return Ok(Completion::Unavailable);
        }
        if monotonic()? >= expires || Instant::now() >= until {
            return Ok(Completion::Cancelled);
        }
        let request = self.dialect.reorder(plan)?;
        let reply = self.client.exchange_effect(&request, until, sent)?;
        if reply != b"ok" && reply != b"ok\n" {
            return Ok(Completion::Uncertain);
        }
        loop {
            let (snapshot, layouts) = self.sampler.sample_with_layouts_until(
                &self.client,
                self.view.fence.session,
                SessionKind::User,
                until,
            )?;
            if monotonic()? >= expires {
                return Ok(Completion::Uncertain);
            }
            if layouts
                .as_ref()
                .is_some_and(|layouts| plan.confirmed(&snapshot, layouts))
            {
                return Ok(Completion::Applied);
            }
            if Instant::now() >= until {
                return Ok(Completion::Uncertain);
            }
            std::thread::sleep(
                Duration::from_millis(1).min(until.saturating_duration_since(Instant::now())),
            );
        }
    }
}
fn same_window(current: &Window, target: &Window) -> bool {
    current.id == target.id
        && current.output == target.output
        && current.workspace == target.workspace
        && current.rect == target.rect
        && current.content == target.content
        && current.mapped
}
fn same_output(current: &OutputState, target: &OutputState) -> bool {
    current.geometry == target.geometry
        && current.active_workspace == target.active_workspace
        && current.screen == target.screen
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lifetime_selectors_are_exact_and_cannot_contain_dispatch_syntax() {
        for id in [
            "",
            "active",
            "0x1",
            "0x:1",
            "0x0:1",
            "0x1:",
            "0x1:0",
            "0x1:1;exit",
            "0x1:1:2",
            "0xfffffffffffffffff:1",
            "0x1:gg",
        ] {
            assert!(address(id).is_err(), "{id}");
        }
        assert_eq!(
            address("0xffffffffffffffff:ffffffffffffffff").unwrap(),
            "0xffffffffffffffff"
        );
        assert!(!supported(WindowAction::ScrollStack));
        assert!(supported(WindowAction::ReorderUp));
        assert!(supported(WindowAction::ReorderDown));
    }
}
