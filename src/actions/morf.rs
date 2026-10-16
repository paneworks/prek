//! User-session submission metadata. Root broker/native checks grant effects;
//! this adapter opens no native socket and never supplies a target over IPC.
use crate::{
    context::{OutputState, SessionKind, State},
    context_transfer::{FRESHNESS, View},
    morf::{ShellPhase, ShellState},
    protocol::Fence,
};
use std::{io, time::Duration};

#[derive(Clone)]
pub(super) struct Target {
    output: OutputState,
    provider: ShellState,
}
pub struct MorfAdapter {
    view: View,
    binding: crate::context::Binding,
}
impl MorfAdapter {
    pub fn new(view: View) -> io::Result<Self> {
        view.validate()?;
        let binding = view
            .context
            .as_ref()
            .ok_or_else(|| io::Error::other("Morf context unavailable"))?
            .binding
            .clone();
        let adapter = Self { view, binding };
        adapter.current(crate::ipc::monotonic()?)?;
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
        if view.validate().is_err()
            || view.fence != self.view.fence
            || view
                .context
                .as_ref()
                .is_some_and(|context| context.binding != self.binding)
            || view.sampled_at > crate::ipc::monotonic()?
        {
            self.view.context = None;
            return Err(io::Error::other("Morf adapter binding/fence changed"));
        }
        self.view = view;
        Ok(())
    }
    fn current(&self, now: Duration) -> io::Result<&crate::worker::ProvidedContext> {
        let context = self
            .view
            .context
            .as_ref()
            .ok_or_else(|| io::Error::other("Morf context unavailable"))?;
        if self.view.state != State::Desktop || context.snapshot.state != State::Desktop
            || context.snapshot.secure_overlay || context.binding.kind != SessionKind::User
            // SAFETY: actual read-only session process credentials.
            || context.binding.uid != unsafe {nix::libc::geteuid()}
            || now < self.view.sampled_at || now-self.view.sampled_at >= FRESHNESS
            || now < context.snapshot.captured_at || now-context.snapshot.captured_at >= FRESHNESS
        {
            return Err(io::Error::other(
                "Morf requires fresh owning desktop metadata",
            ));
        }
        Ok(context)
    }
    pub(super) fn ready(&self, now: Duration) -> bool {
        self.current(now).is_ok_and(|context| {
            self.view.morf.iter().any(|provider| {
                provider.phase == ShellPhase::Configured
                    && context
                        .snapshot
                        .outputs
                        .iter()
                        .any(|output| output.geometry.name == provider.output)
            })
        })
    }
    pub(super) fn target(&self, output: &str, now: Duration) -> io::Result<Target> {
        let context = self.current(now)?;
        let provider = self
            .view
            .morf
            .iter()
            .find(|provider| provider.output == output && provider.phase == ShellPhase::Configured)
            .ok_or_else(|| io::Error::other("Morf provider metadata unavailable"))?;
        let output = context
            .snapshot
            .outputs
            .iter()
            .find(|value| value.geometry.name == output)
            .ok_or_else(|| io::Error::other("Morf frozen output unavailable"))?;
        Ok(Target {
            output: output.clone(),
            provider: provider.clone(),
        })
    }
    pub(super) fn check(&self, target: &Target, now: Duration) -> io::Result<()> {
        let current = self.target(&target.output.geometry.name, now)?;
        if current.output != target.output || current.provider != target.provider {
            return Err(io::Error::other("Morf frozen target/provider changed"));
        }
        Ok(())
    }
}
