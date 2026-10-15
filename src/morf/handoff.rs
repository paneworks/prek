//! Broker-owned recognizer inhibition and confirmed worker-exit cleanup.
//! Quiescence is not physical takeover. Native activation gates the worker's
//! admission separately; the complete kernel cutover gate remains required.
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
    Activating,
    Owned,
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
    dispatches: super::dispatch::Queue,
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
            dispatches: super::dispatch::Queue::default(),
        })
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn owner(&self) -> super::admission::Owner {
        super::admission::Owner {
            output: self.output.clone(),
            epoch: self.capabilities.epoch().into(),
            ui_generation: self.capabilities.ui_generation(),
            ownership_generation: self.generation,
        }
    }
    pub fn admission_until(&self, now: Duration) -> Option<Duration> {
        let until = (self.checked + super::admission::FRESHNESS)
            .min(self.renewed + self.capabilities.lease_limit());
        (self.phase == Phase::Owned && now >= self.checked && now < until).then_some(until)
    }
    /// Initial activation mutates once; later source admissions only sample
    /// existing ownership instead of replaying that operation.
    pub fn activate(&mut self, logind: &Logind) -> io::Result<bool> {
        if !matches!(self.phase, Phase::Quiesced | Phase::Owned) {
            return Ok(false);
        }
        let result = (|| {
            let state = self.client.handoff_state(logind, &self.compositor)?;
            let now = crate::ipc::monotonic()?;
            if self.phase == Phase::Owned {
                self.phase = observed(
                    self.phase,
                    self.generation,
                    self.capabilities.ui_generation(),
                    &state,
                    now,
                )?;
                self.checked = now;
                return Ok(true);
            }
            if state.phase() == native::HandoffPhase::Quiescing {
                self.phase = Phase::Quiescing;
                self.checked = now;
                return Ok(false);
            }
            if observed(
                self.phase,
                self.generation,
                self.capabilities.ui_generation(),
                &state,
                now,
            )? != Phase::Quiesced
            {
                return Ok(false);
            }
            self.phase = Phase::Activating;
            let scope = native::Scope {
                capabilities: &self.capabilities,
                fence: self.control_fence,
            };
            accepted(
                self.client
                    .activate_handoff(scope, &state, logind, &self.compositor),
            )?;
            let state = self.client.handoff_state(logind, &self.compositor)?;
            let completed = crate::ipc::monotonic()?;
            self.phase = observed(
                Phase::Owned,
                self.generation,
                self.capabilities.ui_generation(),
                &state,
                completed,
            )?;
            self.checked = completed;
            Ok(true)
        })();
        if result.is_err() {
            self.phase = Phase::Lost;
            self.dispatches.retire();
        }
        result
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
            self.dispatches.retire();
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
        self.dispatches.retire();
        if matches!(
            self.phase,
            Phase::Quiescing | Phase::Quiesced | Phase::Owned | Phase::Activating
        ) {
            self.phase = Phase::Retired;
        }
    }
    pub fn deadline(&self) -> Option<Duration> {
        matches!(
            self.phase,
            Phase::Quiescing | Phase::Quiesced | Phase::Owned | Phase::ReleaseQueued
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
            if self.phase == Phase::Owned {
                self.poll_control(logind, completed)?;
            }
            Ok(())
        })();
        if result.is_err() {
            self.phase = Phase::Lost;
            self.dispatches.retire();
        }
        result
    }
    /// The trusted broker must separately validate the exact action offer,
    /// frozen target and execution grant. This backend never creates a grant.
    pub fn submit_control(
        &mut self,
        request: &super::controls::Request,
        deadline: Duration,
        logind: &Logind,
    ) -> io::Result<super::dispatch::Ticket> {
        let now = crate::ipc::monotonic()?;
        let until = self
            .admission_until(now)
            .ok_or_else(|| io::Error::other("native action requires fresh owned recognition"))?;
        let ticket =
            self.dispatches
                .reserve(self.fence, self.owner(), request, now, deadline, until)?;
        let mut wire_request = request.clone();
        wire_request.ordinal = self.dispatches.wire_ordinal(&ticket);
        let result = self.client.submit_control(
            native::Scope {
                capabilities: &self.capabilities,
                fence: self.control_fence,
            },
            native::Submission {
                id: ticket.id(),
                deadline,
                request: &wire_request,
            },
            logind,
            &self.compositor,
        );
        self.dispatches.submitted(ticket.id(), &result);
        if result.is_err() || result.as_ref().is_ok_and(control_authority_lost) {
            self.phase = Phase::Lost;
            self.dispatches.retire();
        }
        Ok(ticket)
    }
    fn poll_control(&mut self, logind: &Logind, now: Duration) -> io::Result<()> {
        let Some(id) = self.dispatches.next_receipt(now) else {
            return Ok(());
        };
        let reply = self
            .client
            .control_receipt(
                native::Scope {
                    capabilities: &self.capabilities,
                    fence: self.control_fence,
                },
                id,
                logind,
                &self.compositor,
            )
            .map_err(io::Error::other)?;
        self.dispatches.observed(id, &reply);
        if control_authority_lost(&reply) {
            return Err(io::Error::other("native control authority retired"));
        }
        Ok(())
    }
    pub fn take_control(
        &mut self,
        ticket: &super::dispatch::Ticket,
    ) -> Option<super::dispatch::Outcome> {
        self.dispatches.take(ticket)
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
        self.dispatches.retire();
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
fn control_authority_lost(reply: &native::Reply) -> bool {
    matches!(
        reply.refusal(),
        Some(
            native::Refusal::UiReplaced
                | native::Refusal::ExpiredLease
                | native::Refusal::StaleFence
                | native::Refusal::Retired
                | native::Refusal::HandoffRequired
                | native::Refusal::HandoffUnavailable
                | native::Refusal::StaleOwnership
        )
    )
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
        (Phase::Owned, native::HandoffPhase::Owned) if !state.global_enabled() => Ok(Phase::Owned),
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
