//! Trusted broker entry points, never public session requests.
use super::*;
use crate::morf::handoff::{Phase, Session};

impl Broker {
    /// Inhibit a configured native Morf recognizer before physical takeover.
    /// This does not grant transport readiness or enable Morf action dispatch.
    /// The CLI keeps routing disabled pending the kernel cutover gate.
    pub fn begin_morf_handoff(&mut self, fence: Fence, output: &str) -> io::Result<()> {
        // SAFETY: process credentials, not client-supplied authority.
        if unsafe { libc::geteuid() } != 0 || self.routing.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recognizer handoff requires the root routing broker",
            ));
        }
        if self.morf_handoffs.len() >= 8
            || self
                .morf_handoffs
                .iter()
                .any(|session| session.fence == fence && session.output == output)
        {
            return Err(protocol::invalid(
                "handoff already exists or capacity is exhausted",
            ));
        }
        let now = monotonic()?;
        let bound = self
            .connections
            .iter_mut()
            .filter_map(|connection| connection.bound.as_mut())
            .find(|bound| {
                bound.fence == fence
                    && bound.role == Role::Agent
                    && !bound.transport_started
                    && bound.context.fresh(now)
                    && bound.context.state == State::Desktop
                    && bound.context.value.as_ref().is_some_and(|context| {
                        !context.snapshot.secure_overlay
                            && context
                                .snapshot
                                .outputs
                                .iter()
                                .any(|state| state.geometry.name == output)
                    })
            })
            .ok_or_else(|| {
                protocol::invalid("handoff lacks a fresh bound desktop before transport")
            })?;
        bound.registration.check(&self.logind, &bound.client)?;
        let index = bound
            .native_shells
            .iter()
            .position(|client| {
                client
                    .last_state()
                    .is_some_and(|state| state.output == output)
            })
            .ok_or_else(|| protocol::invalid("configured native shell is unavailable"))?;
        let ledger = self
            .input_releases
            .as_mut()
            .ok_or_else(|| protocol::invalid("input worker is not bound"))?;
        let compositor = bound.client.retain_binding()?;
        let client = bound.native_shells.remove(index);
        let session = Session::prepare(
            fence,
            bound.seat.clone(),
            ledger.worker(),
            client,
            compositor,
            &self.logind,
        )?;
        if !ledger.watching(fence, &bound.seat) {
            ledger.watch(fence, &bound.seat)?;
        }
        // Retain ownership before the first mutating handoff request. A lost
        // reply or agent connection must not discard the cleanup obligation.
        self.morf_handoffs.push(session);
        self.morf_handoffs
            .last_mut()
            .unwrap()
            .start(&self.logind, monotonic()?)
    }

    pub fn morf_handoff_status(&self, fence: Fence, output: &str) -> Option<(Phase, u64)> {
        self.morf_handoffs
            .iter()
            .find(|session| session.fence == fence && session.output == output)
            .map(|session| (session.phase(), session.generation()))
    }

    pub(super) fn poll_morf_handoffs(&mut self, now: Duration) {
        // One bounded native exchange sequence per service step. Never renew
        // an orphaned agent/configuration scope merely because Morf is alive.
        for session in &mut self.morf_handoffs {
            if !self
                .connections
                .iter()
                .filter_map(|connection| connection.bound.as_ref())
                .any(|bound| {
                    bound.fence == session.fence
                        && bound.seat == session.seat
                        && bound.context.fresh(now)
                        && bound.context.state == State::Desktop
                        && bound.context.value.as_ref().is_some_and(|context| {
                            !context.snapshot.secure_overlay
                                && context
                                    .snapshot
                                    .outputs
                                    .iter()
                                    .any(|output| output.geometry.name == session.output)
                        })
                        && now >= bound.renewed
                        && now - bound.renewed < protocol::MAX_PACKET_AGE
                })
            {
                session.retire();
            }
        }
        if let Some(session) = self
            .morf_handoffs
            .iter_mut()
            .filter(|session| session.deadline().is_some_and(|deadline| now >= deadline))
            .min_by_key(|session| session.deadline())
        {
            let _ = session.poll(&self.logind, now);
        }
    }

    pub(super) fn release_morf_after_worker_exit(&mut self) -> io::Result<()> {
        let Some(ledger) = self.input_releases.as_mut() else {
            return Ok(());
        };
        let scopes = self
            .morf_handoffs
            .iter()
            .map(|session| {
                (
                    session.fence.broker.0,
                    session.fence.configuration,
                    session.fence.session,
                    session.seat.clone(),
                )
            })
            .collect::<BTreeSet<_>>();
        let mut failure = None;
        for (broker, configuration, session, seat) in scopes {
            let fence = Fence {
                broker: Epoch(broker),
                configuration,
                session,
            };
            let Some(proof) = ledger.take(fence, &seat) else {
                continue;
            };
            // Old fences can already be retired by lease expiry. A new
            // cleanup-only scope may assert the old proof, never revive it.
            self.session = self
                .session
                .checked_add(1)
                .ok_or_else(|| protocol::invalid("cleanup session generation exhausted"))?;
            let cleanup = Fence {
                broker: self.epoch,
                configuration: self.configuration,
                session: self.session,
            };
            for handoff in self
                .morf_handoffs
                .iter_mut()
                .filter(|handoff| handoff.fence == fence && handoff.seat == seat)
            {
                if let Err(error) = handoff.release(&proof, cleanup, &self.logind, monotonic()?) {
                    failure = Some(error);
                }
            }
        }
        // Failure preserves inhibition, and is reported to the supervisor's
        // caller instead of claiming that normal recognition was restored.
        failure.map_or(Ok(()), Err)
    }
}
