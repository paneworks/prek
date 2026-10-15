//! Trusted broker entry points, never public session requests.
use super::*;
use crate::morf::handoff::{Phase, Session};

impl Broker {
    /// Trusted host dispatch of an already offered/granted action. Public
    /// session packets cannot supply a native payload or create these grants.
    /// MorfActions negotiation remains gated pending rendered context/cutover.
    pub fn submit_morf_control(
        &mut self,
        fence: Fence,
        id: u64,
        ordinal: u64,
        request: &crate::morf::controls::Request,
    ) -> io::Result<crate::morf::dispatch::Ticket> {
        // SAFETY: actual process credentials, not caller-supplied metadata.
        if unsafe { libc::geteuid() } != 0 || self.routing.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "native control requires the root routing broker",
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
                    && bound.context.fresh(now)
                    && bound.context.state == State::Desktop
                    && now >= bound.renewed
                    && now - bound.renewed < protocol::MAX_PACKET_AGE
            })
            .ok_or_else(|| protocol::invalid("native control lacks its owning desktop"))?;
        bound.registration.check(&self.logind, &bound.client)?;
        let offer = bound
            .actions
            .pending
            .get(&(id, ordinal))
            .ok_or_else(|| protocol::invalid("native control lacks its broker offer"))?;
        if !bound.capabilities.contains(&Capability::MorfActions)
            || !bound.available_actions.contains(&Capability::MorfActions)
            || !offer.capabilities_available(&bound.capabilities, &bound.available_actions)
            || offer.source.seat != bound.seat
        {
            return Err(protocol::invalid("native action capability unavailable"));
        }
        let snapshot = &bound
            .context
            .value
            .as_ref()
            .ok_or_else(|| protocol::invalid("native control context missing"))?
            .snapshot;
        let deadline = offer
            .morf_control(request, snapshot, now)?
            .min(bound.context.checked + crate::context::SNAPSHOT_MAX_AGE)
            .min(snapshot.captured_at + crate::context::SNAPSHOT_MAX_AGE);
        let session = self
            .morf_handoffs
            .iter_mut()
            .find(|session| {
                session.fence == fence
                    && session.seat == bound.seat
                    && session.output == request.output
            })
            .ok_or_else(|| protocol::invalid("native control has no owning recognizer"))?;
        let deadline = deadline.min(
            session
                .admission_until(monotonic()?)
                .ok_or_else(|| protocol::invalid("native recognition lease unavailable"))?,
        );
        session.submit_control(request, deadline, Some(&offer.source), &self.logind)
    }
    pub fn take_morf_control(
        &mut self,
        ticket: &crate::morf::dispatch::Ticket,
    ) -> Option<crate::morf::dispatch::Outcome> {
        self.morf_handoffs
            .iter_mut()
            .find(|session| session.fence == ticket.fence() && session.output == ticket.output())?
            .take_control(ticket)
    }
    pub fn cancel_morf_control(&mut self, ticket: &crate::morf::dispatch::Ticket) -> bool {
        self.morf_handoffs
            .iter_mut()
            .find(|session| session.fence == ticket.fence() && session.output == ticket.output())
            .is_some_and(|session| session.cancel_control(ticket))
    }
    pub fn morf_control_cancellation(
        &self,
        ticket: &crate::morf::dispatch::Ticket,
    ) -> Option<crate::morf::dispatch::Cancellation> {
        self.morf_handoffs
            .iter()
            .find(|session| session.fence == ticket.fence() && session.output == ticket.output())?
            .control_cancellation(ticket)
    }
    pub fn morf_revocation(
        &self,
        fence: Fence,
        output: &str,
    ) -> Option<crate::morf::dispatch::Cancellation> {
        self.morf_handoffs
            .iter()
            .find(|session| session.fence == fence && session.output == output)?
            .revocation()
    }
    pub(super) fn cancel_morf_source(
        &mut self,
        fence: Fence,
        source: &crate::input::Source,
        id: Option<u64>,
    ) {
        for session in self
            .morf_handoffs
            .iter_mut()
            .filter(|session| session.fence == fence && session.seat == source.seat)
        {
            session.cancel_source(source, id);
        }
    }
    pub(super) fn morf_requirements(
        &self,
        fence: Fence,
        seat: &str,
    ) -> Vec<crate::morf::admission::Owner> {
        let mut owners = self
            .morf_handoffs
            .iter()
            .filter(|session| session.fence == fence && session.seat == seat)
            .map(Session::owner)
            .collect::<Vec<_>>();
        owners.sort_by(|a, b| a.output.cmp(&b.output));
        owners
    }
    pub(super) fn morf_lease(&self, fence: Fence, seat: &str, now: Duration) -> Option<Duration> {
        let mut until = None;
        for session in self
            .morf_handoffs
            .iter()
            .filter(|session| session.fence == fence && session.seat == seat)
        {
            let value = session.admission_until(now)?;
            until = Some(until.map_or(value, |old: Duration| old.min(value)));
        }
        until
    }
    /// Private worker request after metadata, consumer and neutral preflight.
    /// No public client-supplied readiness enters this path.
    pub fn activate_morf_handoff(
        &mut self,
        channel: &Channel,
        activation: &crate::morf::admission::Activation,
    ) -> io::Result<Option<crate::configuration::Command>> {
        let now = monotonic()?;
        if !self
            .input_releases
            .as_ref()
            .is_some_and(|ledger| ledger.matches_worker(channel, activation.worker))
        {
            return Err(protocol::invalid(
                "native admission came from another worker",
            ));
        }
        if !activation.fresh(now)
            || !self.input_releases.as_ref().is_some_and(|ledger| {
                ledger.prepared_by(
                    channel,
                    activation.worker,
                    activation.fence,
                    &activation.source,
                )
            })
            || self.morf_requirements(activation.fence, &activation.source.seat)
                != activation.owners
        {
            return Ok(None);
        }
        let Some(bound) = self
            .connections
            .iter_mut()
            .filter_map(|connection| connection.bound.as_mut())
            .find(|bound| {
                bound.role == Role::Agent
                    && bound.fence == activation.fence
                    && bound.seat == activation.source.seat
                    && bound.context.fresh(now)
                    && bound.context.state == State::Desktop
                    && bound
                        .context
                        .value
                        .as_ref()
                        .is_some_and(|context| !context.snapshot.secure_overlay)
            })
        else {
            return Ok(None);
        };
        bound.registration.check(&self.logind, &bound.client)?;
        let context_until = (bound.context.checked + crate::context::SNAPSHOT_MAX_AGE).min(
            bound.context.value.as_ref().unwrap().snapshot.captured_at
                + crate::context::SNAPSHOT_MAX_AGE,
        );
        for session in self.morf_handoffs.iter_mut().filter(|session| {
            session.fence == activation.fence && session.seat == activation.source.seat
        }) {
            if !activation.fresh(monotonic()?) || !session.activate(&self.logind).unwrap_or(false) {
                return Ok(None);
            }
        }
        let completed = monotonic()?;
        if !activation.fresh(completed) || completed >= context_until {
            return Ok(None);
        }
        let Some(until) = self.morf_lease(activation.fence, &activation.source.seat, completed)
        else {
            return Ok(None);
        };
        let expires = until
            .min(context_until)
            .min(activation.observed_at + crate::morf::admission::FRESHNESS);
        if expires <= completed {
            return Ok(None);
        }
        Ok(Some(crate::configuration::Command::MorfActivated {
            grant: crate::morf::admission::Grant {
                activation: activation.clone(),
                expires,
            },
        }))
    }
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
