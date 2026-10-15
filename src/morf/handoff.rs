//! Broker-owned recognizer inhibition and confirmed worker-exit cleanup.
//! Quiescence is not physical takeover: transport admission stays inhibited
//! until the separate kernel cutover and activation path is implemented.
use super::{ShellClient, native};
use crate::{
    hyprland, morf::ownership::ReleasedInputs, protocol::Fence, session::Logind,
    supervisor::WorkerIdentity,
};
use std::{io, time::Duration};

#[cfg(test)]
mod tests;

const REFRESH: Duration = Duration::from_millis(100);
const RENEW: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Quiescing,
    Quiesced,
    Retired,
    ReleaseQueued,
    ReleaseFailed,
    Restored,
    Lost,
}

/// The binding persists independently of the agent connection. Dropping it
/// never requests release; connection loss alone cannot enable Morf globals.
pub(crate) struct Session {
    pub fence: Fence,
    pub seat: String,
    pub output: String,
    control_fence: Fence,
    worker: WorkerIdentity,
    generation: u64,
    phase: Phase,
    client: ShellClient,
    compositor: hyprland::Client,
    capabilities: native::Capabilities,
    checked: Duration,
    renewed: Duration,
}
impl Session {
    pub fn prepare(
        fence: Fence,
        seat: String,
        worker: WorkerIdentity,
        mut client: ShellClient,
        compositor: hyprland::Client,
        logind: &Logind,
    ) -> io::Result<Self> {
        fence.validate()?;
        let binding = client
            .last_state()
            .ok_or_else(|| io::Error::other("missing shell binding"))?;
        if binding.phase != super::ShellPhase::Configured {
            return Err(io::Error::other("native shell is not configured"));
        }
        let output = binding.output.clone();
        let capabilities = client.control_capabilities(logind, &compositor)?;
        let state = client.handoff_state(logind, &compositor)?;
        if state.phase() != native::HandoffPhase::Native
            || state.ui_generation() != capabilities.ui_generation()
            || !state.global_enabled()
        {
            return Err(io::Error::other(
                "native recognizer already inhibited; previous ownership must be resolved",
            ));
        }
        let generation = state
            .ownership_generation()
            .checked_add(1)
            .ok_or_else(|| io::Error::other("ownership generation exhausted"))?;
        Ok(Self {
            fence,
            seat,
            output,
            control_fence: fence,
            worker,
            generation,
            phase: Phase::Quiescing,
            client,
            compositor,
            capabilities,
            checked: Duration::ZERO,
            renewed: Duration::ZERO,
        })
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn start(&mut self, logind: &Logind, now: Duration) -> io::Result<()> {
        let result = (|| {
            self.renew(logind, now)?;
            let scope = native::Scope {
                capabilities: &self.capabilities,
                fence: self.fence,
            };
            accepted(
                self.client
                    .quiesce_handoff(scope, self.generation, logind, &self.compositor),
            )
        })();
        if result.is_err() {
            self.phase = Phase::Lost;
        }
        result
    }
    fn renew(&mut self, logind: &Logind, now: Duration) -> io::Result<()> {
        let scope = native::Scope {
            capabilities: &self.capabilities,
            fence: self.control_fence,
        };
        accepted(self.client.lease_control(
            scope,
            now + self.capabilities.lease_limit(),
            logind,
            &self.compositor,
        ))?;
        self.renewed = now;
        Ok(())
    }
    pub fn retire(&mut self) {
        if matches!(self.phase, Phase::Quiescing | Phase::Quiesced) {
            self.phase = Phase::Retired;
        }
    }
    pub fn deadline(&self) -> Option<Duration> {
        matches!(
            self.phase,
            Phase::Quiescing | Phase::Quiesced | Phase::ReleaseQueued
        )
        .then_some(self.checked + REFRESH)
    }
    pub fn poll(&mut self, logind: &Logind, now: Duration) -> io::Result<()> {
        if self.deadline().is_none_or(|deadline| now < deadline) {
            return Ok(());
        }
        let result = (|| {
            if now < self.checked || now < self.renewed {
                return Err(io::Error::other("handoff clock moved backward"));
            }
            if self.phase != Phase::ReleaseQueued && now - self.renewed >= RENEW {
                self.renew(logind, now)?;
            }
            let state = self.client.handoff_state(logind, &self.compositor)?;
            let completed = crate::ipc::monotonic()?;
            self.phase = observed(
                self.phase,
                self.generation,
                self.capabilities.ui_generation(),
                &state,
                completed,
            )?;
            self.checked = completed;
            Ok(())
        })();
        if result.is_err() {
            self.phase = Phase::Lost;
        }
        result
    }
    /// Only an actual reaped worker proves recognition has stopped as well as
    /// descriptor closure. Per-source closure alone cannot assert drain here.
    pub fn release(
        &mut self,
        proof: &ReleasedInputs,
        cleanup: Fence,
        logind: &Logind,
        now: Duration,
    ) -> io::Result<()> {
        validate_release(self.fence, &self.seat, self.worker, self.phase, proof)?;
        validate_cleanup(self.fence, cleanup)?;
        // Commit before writing: an uncertain mutation is never retried.
        self.phase = Phase::ReleaseFailed;
        self.control_fence = cleanup;
        self.renew(logind, now)?;
        let scope = native::Scope {
            capabilities: &self.capabilities,
            fence: self.control_fence,
        };
        accepted(self.client.confirm_handoff_release(
            scope,
            self.generation,
            logind,
            &self.compositor,
        ))?;
        self.phase = Phase::ReleaseQueued;
        self.checked = now;
        Ok(())
    }
}
fn validate_cleanup(original: Fence, cleanup: Fence) -> io::Result<()> {
    cleanup.validate()?;
    if cleanup.broker != original.broker
        || cleanup.configuration < original.configuration
        || cleanup.session <= original.session
    {
        return Err(io::Error::other(
            "cleanup requires a fresh broker-owned fence",
        ));
    }
    Ok(())
}
fn accepted(result: Result<native::Reply, native::Error>) -> io::Result<()> {
    match result {
        Ok(reply) if reply.status() == native::Status::Accepted => Ok(()),
        Ok(reply) => Err(io::Error::other(format!(
            "native handoff refused: {:?}",
            reply.refusal()
        ))),
        Err(error) => Err(io::Error::other(error)),
    }
}
fn validate_release(
    fence: Fence,
    seat: &str,
    worker: WorkerIdentity,
    phase: Phase,
    proof: &ReleasedInputs,
) -> io::Result<()> {
    if matches!(
        phase,
        Phase::ReleaseQueued | Phase::ReleaseFailed | Phase::Restored
    ) || proof.fence() != fence
        || proof.seat() != seat
        || proof.worker() != worker
        || !proof.worker_exited()
    {
        return Err(io::Error::other(
            "handoff release needs exact reaped-worker evidence and an unsubmitted release",
        ));
    }
    Ok(())
}
fn observed(
    phase: Phase,
    generation: u64,
    ui: u64,
    state: &native::HandoffState,
    now: Duration,
) -> io::Result<Phase> {
    if state.ownership_generation() != generation || state.ui_generation() != ui {
        return Err(io::Error::other(
            "native ownership or UI generation changed",
        ));
    }
    match (phase, state.phase()) {
        (Phase::Quiescing | Phase::Quiesced, native::HandoffPhase::Quiescing) => {
            Ok(Phase::Quiescing)
        }
        (Phase::Quiescing | Phase::Quiesced, native::HandoffPhase::Quiesced)
            if state.ready_to_activate(now) =>
        {
            Ok(Phase::Quiesced)
        }
        (Phase::ReleaseQueued, native::HandoffPhase::Releasing) => Ok(Phase::ReleaseQueued),
        (Phase::ReleaseQueued, native::HandoffPhase::Native) if state.global_enabled() => {
            Ok(Phase::Restored)
        }
        _ => Err(io::Error::other(
            "native handoff lost or unexpected ownership phase",
        )),
    }
}
