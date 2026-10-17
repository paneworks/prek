//! Typed output-scoped DPMS, called only by the granted session executor.
//! No lock/unlock, suspend, toggle or ambient compositor discovery is exposed.
use crate::{
    config::{PowerAction, Screen},
    context::{OutputState, SessionKind, State},
    context_transfer::{FRESHNESS, View},
    hyprland::{Client, ContextSampler},
    ipc::monotonic,
    protocol::{Completion, Fence},
};
use std::{
    io,
    time::{Duration, Instant},
};

pub struct PowerAdapter {
    client: Client,
    dialect: crate::hyprland::dispatch::Dialect,
    sampler: ContextSampler,
    view: View,
}
impl PowerAdapter {
    /// The host supplies the broker-approved binding and context, never Lua.
    pub fn new(client: Client, view: View) -> io::Result<Self> {
        view.validate()?;
        let dialect = client.dispatch_dialect_until(Instant::now() + Duration::from_millis(200))?;
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
                "power adapter context binding/fence changed",
            ));
        }
        self.view = view;
        Ok(())
    }
    pub(super) fn ready(&self, now: Duration) -> bool {
        self.current(now).is_ok()
    }
    fn current(&self, now: Duration) -> io::Result<&crate::context::Snapshot> {
        let provided = self
            .view
            .context
            .as_ref()
            .ok_or_else(|| io::Error::other("power context unavailable"))?;
        if self.view.state != State::Desktop
            || self.sampler.inhibited()
            || provided.snapshot.state != State::Desktop
            || provided.snapshot.secure_overlay
            || provided.binding.kind != SessionKind::User
            // SAFETY: read-only current process credential query.
            || provided.binding.uid != unsafe {nix::libc::geteuid()}
            || !self.client.bound_to(&provided.binding)
            || now < self.view.sampled_at
            || now - self.view.sampled_at >= FRESHNESS
            || now < provided.snapshot.captured_at
            || now - provided.snapshot.captured_at >= FRESHNESS
        {
            return Err(io::Error::other(
                "power adapter requires fresh owning desktop context",
            ));
        }
        Ok(&provided.snapshot)
    }
    pub(super) fn target(&self, name: &str, now: Duration) -> io::Result<OutputState> {
        if !crate::config::valid_power_output(name) {
            return Err(io::Error::other(
                "power output requires an exact safe connector name",
            ));
        }
        self.current(now)?
            .outputs
            .iter()
            .find(|output| output.geometry.name == name)
            .cloned()
            .ok_or_else(|| io::Error::other("power target is unavailable"))
    }
    pub(super) fn execute(
        &mut self,
        action: PowerAction,
        target: &OutputState,
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
            let screen = match action {
                PowerAction::Wake => Screen::On,
                PowerAction::Blank => Screen::Off,
                PowerAction::LockAndBlank => return Ok(Completion::Unavailable),
            };
            if self.target(&target.geometry.name, now)? != *target {
                return Ok(Completion::Unavailable);
            }
            let until = Instant::now() + remaining.min(Duration::from_millis(200));
            let snapshot = self.sampler.sample_until(
                &self.client,
                self.view.fence.session,
                SessionKind::User,
                until,
            )?;
            if !snapshot
                .outputs
                .iter()
                .any(|output| output.geometry == target.geometry && output.screen == target.screen)
            {
                return Ok(Completion::Unavailable);
            }
            // Sampling may consume the grant. Never start a late mutation.
            if self.client.dispatch_dialect_until(until)? != self.dialect {
                return Ok(Completion::Unavailable);
            }
            if monotonic()? >= expires || Instant::now() >= until {
                return Ok(Completion::Cancelled);
            }
            let request = self.dialect.power(&target.geometry.name, screen)?;
            let reply = self.client.exchange_effect(&request, until, &mut sent);
            let Ok(reply) = reply else {
                return Ok(if sent {
                    Completion::Uncertain
                } else {
                    Completion::Unavailable
                });
            };
            if reply != b"ok" && reply != b"ok\n" {
                return Ok(Completion::Uncertain);
            }
            // Success needs an observed DPMS state, not just dispatch acceptance.
            loop {
                let snapshot = match self.sampler.sample_until(
                    &self.client,
                    self.view.fence.session,
                    SessionKind::User,
                    until,
                ) {
                    Ok(snapshot) => snapshot,
                    Err(_) => return Ok(Completion::Uncertain),
                };
                if monotonic()? >= expires {
                    return Ok(Completion::Uncertain);
                }
                let Some(output) = snapshot
                    .outputs
                    .iter()
                    .find(|output| output.geometry == target.geometry)
                else {
                    return Ok(Completion::Uncertain);
                };
                if output.screen == screen {
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
}
