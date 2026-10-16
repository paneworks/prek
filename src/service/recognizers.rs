//! Broker-owned delivery, proposal and cleanup correlation. No routing claims.
use super::*;
use crate::recognition::{Cancellation, MAX_BYTES, MAX_PENDING, Proposal, Retirement, Ticket};

const RECEIPT_GRACE: Duration = Duration::from_millis(250);
struct Pending {
    ticket: Box<Ticket>,
    proposal: Option<Proposal>,
    sent: bool,
    delivered: bool,
}
struct Candidate {
    scope: Cancellation,
    delivered: u64,
    cancellation: Option<(Cancellation, Duration)>,
}
#[derive(Default)]
pub(super) struct Ledger {
    pub requested: bool,
    pub ready: bool,
    retired: bool,
    last: u64,
    candidates: BTreeMap<u64, Candidate>,
    tickets: BTreeMap<u64, Pending>,
    retired_replies: BTreeMap<u64, Duration>,
    retired_proposals: BTreeMap<u64, (Fence, u64)>,
}
impl Ledger {
    fn bytes(&self) -> io::Result<usize> {
        self.tickets.values().try_fold(0usize, |bytes, pending| {
            Ok(bytes
                + crate::recognition::encoded_size(
                    &pending.ticket,
                    crate::recognition::MAX_FRAME_BYTES,
                )?
                + pending
                    .proposal
                    .as_ref()
                    .map(|proposal| {
                        crate::recognition::encoded_size(
                            proposal,
                            crate::recognition::MAX_DATA_BYTES + 512,
                        )
                    })
                    .transpose()?
                    .unwrap_or(0))
        })
    }
    fn request(&mut self, ready: bool) -> io::Result<()> {
        if ready && self.retired {
            return Err(protocol::invalid(
                "recognizer VM retirement needs a new session fence",
            ));
        }
        if !ready {
            self.retired |= self.requested;
            self.ready = false;
            for pending in self.tickets.values_mut() {
                if !pending.sent {
                    pending.proposal = None;
                }
            }
        }
        self.requested = ready;
        Ok(())
    }
    fn acknowledge_ready(&mut self, ready: bool) -> bool {
        self.ready = ready && self.requested && !self.retired;
        self.ready
    }
    fn issue(&mut self, ticket: Box<Ticket>) -> io::Result<()> {
        if !self.ready
            || !self.requested
            || ticket.id <= self.last
            || self.tickets.len() + self.retired_replies.len() + self.retired_proposals.len()
                >= MAX_PENDING
        {
            return Err(protocol::invalid(
                "inactive, repeated or excessive recognizer ticket",
            ));
        }
        let bytes = self.bytes()?;
        if bytes + crate::recognition::encoded_size(&ticket, crate::recognition::MAX_FRAME_BYTES)?
            > MAX_BYTES
        {
            return Err(protocol::invalid("recognizer ticket byte limit exceeded"));
        }
        let scope = Cancellation::from_ticket(&ticket, Retirement::Cancelled);
        if let Some(candidate) = self.candidates.get_mut(&ticket.candidate) {
            if !candidate.scope.same_candidate(&scope) || candidate.cancellation.is_some() {
                return Err(protocol::invalid(
                    "recognizer candidate changed without cleanup",
                ));
            }
            candidate.scope = scope;
        } else {
            if self.candidates.len() >= MAX_PENDING {
                return Err(protocol::invalid("recognizer cleanup capacity exhausted"));
            }
            self.candidates.insert(
                ticket.candidate,
                Candidate {
                    scope,
                    delivered: 0,
                    cancellation: None,
                },
            );
        }
        self.last = ticket.id;
        self.tickets.insert(
            ticket.id,
            Pending {
                ticket,
                proposal: None,
                sent: false,
                delivered: false,
            },
        );
        Ok(())
    }
    pub(super) fn delivered(&mut self, id: u64) -> io::Result<()> {
        let pending = self
            .tickets
            .get_mut(&id)
            .ok_or_else(|| protocol::invalid("retired recognizer ticket was transmitted"))?;
        self.candidates
            .get_mut(&pending.ticket.candidate)
            .unwrap()
            .delivered = id;
        pending.delivered = true;
        Ok(())
    }
    fn prepare(&mut self, proposal: Proposal, now: Duration) -> io::Result<()> {
        if !self.requested || !self.ready {
            return Err(protocol::invalid("recognizer helper is unavailable"));
        }
        crate::recognition::validate_data(&proposal.data)?;
        if self
            .retired_proposals
            .get(&proposal.ticket)
            .is_some_and(|(fence, _)| *fence == proposal.fence)
        {
            self.retired_proposals.remove(&proposal.ticket);
            return Ok(());
        }
        if self.bytes()?
            + crate::recognition::encoded_size(&proposal, crate::recognition::MAX_DATA_BYTES + 512)?
            > MAX_BYTES
        {
            return Err(protocol::invalid(
                "recognizer proposal byte budget exhausted",
            ));
        }
        let pending = self
            .tickets
            .get_mut(&proposal.ticket)
            .ok_or_else(|| protocol::invalid("unknown recognizer proposal ticket"))?;
        let candidate = &self.candidates[&pending.ticket.candidate];
        if pending.ticket.fence != proposal.fence
            || !pending.delivered
            || candidate.cancellation.is_some()
            || now >= pending.ticket.deadline
            || pending.proposal.is_some()
            || pending.sent
        {
            return Err(protocol::invalid(
                "expired, undelivered or repeated recognizer proposal",
            ));
        }
        pending.proposal = Some(proposal);
        Ok(())
    }
    fn cancel(&mut self, cancellation: Cancellation, now: Duration) -> io::Result<bool> {
        cancellation.validate()?;
        let Some(candidate) = self.candidates.get_mut(&cancellation.candidate) else {
            return Ok(false);
        };
        if !candidate.scope.same_candidate(&cancellation)
            || cancellation.through < candidate.scope.through
            || candidate.cancellation.is_some()
        {
            return Err(protocol::invalid(
                "recognizer cancellation changed its issued identity",
            ));
        }
        self.tickets.retain(|id, pending| {
            if pending.ticket.candidate != cancellation.candidate {
                return true;
            }
            if pending.sent {
                self.retired_replies.insert(*id, now + RECEIPT_GRACE);
            } else if pending.delivered && pending.proposal.is_none() {
                self.retired_proposals
                    .insert(*id, (pending.ticket.fence, cancellation.candidate));
            }
            false
        });
        if candidate.delivered == 0 {
            self.candidates.remove(&cancellation.candidate);
            return Ok(false);
        }
        candidate.cancellation = Some((cancellation, now + protocol::MAX_PACKET_AGE));
        Ok(true)
    }
    pub(super) fn cleaned(&mut self, cancellation: &Cancellation) -> io::Result<()> {
        let candidate = self
            .candidates
            .get(&cancellation.candidate)
            .ok_or_else(|| protocol::invalid("unknown recognizer cleanup acknowledgement"))?;
        if candidate
            .cancellation
            .as_ref()
            .is_none_or(|(expected, _)| expected != cancellation)
        {
            return Err(protocol::invalid(
                "recognizer cleanup acknowledgement differs",
            ));
        }
        self.candidates.remove(&cancellation.candidate);
        self.retired_proposals
            .retain(|_, (_, candidate)| *candidate != cancellation.candidate);
        Ok(())
    }
    fn overdue(&self, now: Duration) -> bool {
        self.tickets
            .values()
            .any(|pending| now >= pending.ticket.deadline.saturating_add(RECEIPT_GRACE))
            || self.retired_replies.values().any(|expires| now >= *expires)
            || self.candidates.values().any(|candidate| {
                candidate
                    .cancellation
                    .as_ref()
                    .is_some_and(|(_, expires)| now >= *expires)
            })
    }
    pub(super) fn deadline(&self) -> Option<Duration> {
        self.tickets
            .values()
            .map(|pending| pending.ticket.deadline.saturating_add(RECEIPT_GRACE))
            .chain(self.retired_replies.values().copied())
            .chain(self.candidates.values().filter_map(|candidate| {
                candidate.cancellation.as_ref().map(|(_, expires)| *expires)
            }))
            .min()
    }
    fn receipt(&mut self, ticket: u64) -> io::Result<bool> {
        let Some(pending) = self.tickets.get(&ticket) else {
            if self.retired_replies.remove(&ticket).is_some() {
                return Ok(false);
            }
            return Err(protocol::invalid("unknown recognizer worker receipt"));
        };
        if !pending.sent {
            return Err(protocol::invalid("unsolicited recognizer worker receipt"));
        }
        self.tickets.remove(&ticket);
        Ok(true)
    }
}

pub(super) fn permitted(
    config: Option<&crate::config::Config>,
    bound: &Bound,
    now: Duration,
) -> bool {
    bound.role == Role::Agent
        && bound.observing
        && bound.context.fresh(now)
        && config.is_some_and(|config| {
            crate::recognition::permitted_subscription(
                config,
                &bound.capabilities,
                bound.context.value.as_deref(),
                &bound.seat,
                bound.fence.session,
                now,
            )
        })
}
fn bindings_available(
    config: Option<&crate::config::Config>,
    bindings: &[crate::lua::RecognizerBinding],
) -> bool {
    !bindings.is_empty()
        && config.is_some_and(|config| {
            config.recognizer_definitions().iter().all(|recognizer| {
                bindings
                    .iter()
                    .any(|binding| &binding.definition == recognizer)
            })
        })
}
fn validate_delivery(
    config: &crate::config::Config,
    bindings: &[crate::lua::RecognizerBinding],
    fence: Fence,
    seat: &str,
    revision: u64,
    ticket: &Ticket,
    now: Duration,
) -> io::Result<()> {
    let declaration = bindings
        .iter()
        .find(|binding| binding.definition.name == ticket.recognizer)
        .ok_or_else(|| protocol::invalid("unapproved recognizer source binding"))?;
    ticket.validate_for(&declaration.definition, now)?;
    if ticket.fence != fence || ticket.input.source().seat != seat || ticket.revision != revision || !config.rules.iter().any(|rule| rule.enabled && config.mode_active(rule) && rule.id == ticket.rule && matches!(&rule.trigger, crate::config::Trigger::Custom { recognizer, .. } if recognizer == &declaration.definition)) {
        return Err(protocol::invalid("recognizer ticket differs from current source, seat or rules"));
    }
    Ok(())
}
fn validate_metadata(
    config: &crate::config::Config,
    bindings: &[crate::lua::RecognizerBinding],
    seat: &str,
    metadata: &crate::recognition::Metadata,
    now: Duration,
) -> io::Result<()> {
    let scope = crate::recognition::metadata_selectors(config);
    if metadata.source.seat != seat || !bindings_available(Some(config), bindings) {
        return Err(protocol::invalid(
            "unapproved recognizer metadata source or scope",
        ));
    }
    metadata.validate_for(config, &scope, now)
}
impl Connection {
    fn purge_recognitions(&mut self, candidate: Option<u64>) -> io::Result<()> {
        self.outgoing.retain(|reply| !matches!(reply, Reply::Recognition { ticket, .. } if candidate.is_none_or(|id| ticket.candidate == id)));
        if candidate.is_none() {
            self.outgoing
                .retain(|reply| !matches!(reply, Reply::RecognizerMetadata { .. }));
        }
        self.bytes = self
            .outgoing
            .iter()
            .map(reply_size)
            .sum::<io::Result<usize>>()?;
        Ok(())
    }
}
impl Broker {
    pub(super) fn recognizers_available(&self) -> bool {
        bindings_available(self.rule_baseline.as_deref(), &self.recognizer_bindings)
    }
    pub fn recognitions_wants_write(&self) -> bool {
        let now = monotonic().unwrap_or(Duration::MAX);
        self.connections
            .iter()
            .filter_map(|connection| connection.bound.as_ref())
            .any(|bound| {
                bound.recognizers.requested
                    && bound.recognizers.ready
                    && bound.recognizers.tickets.values().any(|pending| {
                        pending.proposal.is_some() && !pending.sent && now < pending.ticket.deadline
                    })
            })
    }
    pub(super) fn recognizer_ready(
        &mut self,
        connection: &mut Connection,
        fence: Fence,
        ready: bool,
        now: Duration,
    ) -> io::Result<()> {
        if ready {
            self.refresh_context(connection, now)?;
            if !self.guard_observation(connection, now)? {
                return Ok(());
            }
        }
        let bound = connection.bound.as_mut().unwrap();
        if bound.fence != fence {
            return Ok(());
        }
        if !bound.capabilities.contains(&Capability::CustomRecognizers)
            || ready
                && (!permitted(self.rule_baseline.as_deref(), bound, monotonic()?)
                    || self.reload_pending)
        {
            return Err(protocol::invalid(
                "recognizer readiness requires an approved owning desktop agent",
            ));
        }
        bound.recognizers.request(ready)?;
        if !ready {
            connection.purge_recognitions(None)?;
        }
        Ok(())
    }
    pub(super) fn prepare_recognition(
        &mut self,
        connection: &mut Connection,
        fence: Fence,
        proposal: Proposal,
        now: Duration,
    ) -> io::Result<()> {
        self.refresh_context(connection, now)?;
        if !self.guard_observation(connection, now)? {
            return Ok(());
        }
        let bound = connection.bound.as_mut().unwrap();
        if bound.fence != fence {
            return Ok(());
        }
        if !permitted(self.rule_baseline.as_deref(), bound, monotonic()?) || self.reload_pending {
            return Err(protocol::invalid(
                "recognizer proposal lacks current desktop authority",
            ));
        }
        bound.recognizers.prepare(proposal, monotonic()?)
    }
    pub(super) fn recognition_report(
        &mut self,
        connection: &mut Connection,
        report: Report,
        now: Duration,
    ) -> io::Result<()> {
        let bound = connection.bound.as_mut().unwrap();
        if report.subscription().fence != bound.fence || report.subscription().seat != bound.seat {
            return Ok(());
        }
        let fence = bound.fence;
        match report {
            Report::RecognizerReady { ready, .. } => {
                if !bound.capabilities.contains(&Capability::CustomRecognizers) {
                    return Err(protocol::invalid("unnegotiated recognizer readiness"));
                }
                let ready = bound.recognizers.acknowledge_ready(ready);
                if !ready {
                    connection.purge_recognitions(None)?;
                }
                connection.push(Reply::RecognizerReady { fence, ready })
            }
            Report::RecognizerMetadata { metadata, .. } => {
                if !permitted(self.rule_baseline.as_deref(), bound, now)
                    || !bound.recognizers.ready
                    || !bound.recognizers.requested
                {
                    return Ok(());
                }
                let config = self.rule_baseline.as_deref().ok_or_else(|| {
                    protocol::invalid("recognizer metadata lacks approved configuration")
                })?;
                validate_metadata(
                    config,
                    &self.recognizer_bindings,
                    &bound.seat,
                    &metadata,
                    now,
                )?;
                connection.push(Reply::RecognizerMetadata { fence, metadata })
            }
            Report::Recognition { ticket, .. } => {
                if !permitted(self.rule_baseline.as_deref(), bound, now)
                    || !bound.recognizers.ready
                    || !bound.recognizers.requested
                {
                    return Ok(());
                }
                let current = bound.rules.current().ok_or_else(|| {
                    protocol::invalid("recognizer has no approved rule configuration")
                })?;
                validate_delivery(
                    current,
                    &self.recognizer_bindings,
                    fence,
                    &bound.seat,
                    bound.rules.revision(),
                    &ticket,
                    now,
                )?;
                bound.recognizers.issue(ticket.clone())?;
                connection.push(Reply::Recognition { fence, ticket })
            }
            Report::RecognitionCancelled { cancellation, .. } => {
                if !bound.capabilities.contains(&Capability::CustomRecognizers)
                    || cancellation.fence != fence
                    || cancellation.source.seat != bound.seat
                {
                    return Err(protocol::invalid("foreign recognizer cancellation"));
                }
                let deliver = bound.recognizers.cancel(cancellation.clone(), now)?;
                connection.purge_recognitions(Some(cancellation.candidate))?;
                if deliver {
                    connection.push(Reply::RecognitionCancelled {
                        fence,
                        cancellation,
                    })?;
                }
                Ok(())
            }
            _ => Err(protocol::invalid("unexpected recognizer worker report")),
        }
    }
    pub fn recognition_command(
        &mut self,
        now: Duration,
    ) -> io::Result<Option<crate::configuration::Command>> {
        for index in 0..self.connections.len() {
            if self.connections[index]
                .bound
                .as_ref()
                .is_some_and(|bound| bound.recognizers.overdue(now))
            {
                let mut connection = self.connections.remove(index);
                self.advance_session(&mut connection, now)?;
                self.connections.insert(index, connection);
                return Ok(None);
            }
        }
        let selected = self.connections.iter().position(|connection| {
            connection.bound.as_ref().is_some_and(|bound| {
                bound.recognizers.requested
                    && bound.recognizers.ready
                    && bound
                        .recognizers
                        .tickets
                        .values()
                        .any(|pending| pending.proposal.is_some() && !pending.sent)
            })
        });
        let Some(index) = selected else {
            return Ok(None);
        };
        let mut connection = self.connections.remove(index);
        let result: io::Result<Option<crate::configuration::Command>> = (|| {
            self.refresh_context(&mut connection, now)?;
            if !self.guard_observation(&mut connection, now)? {
                return Ok(None);
            }
            let bound = connection.bound.as_ref().unwrap();
            if !permitted(self.rule_baseline.as_deref(), bound, monotonic()?)
                || !bound.recognizers.requested
                || !bound.recognizers.ready
                || self.reload_pending
            {
                return Ok(None);
            }
            self.verify_agent(connection.channel.peer().pid)?;
            let pending = bound.recognizers.tickets.values().find(|pending| {
                pending.proposal.is_some()
                    && !pending.sent
                    && monotonic().is_ok_and(|now| now < pending.ticket.deadline)
            });
            Ok(
                pending.map(|pending| crate::configuration::Command::Recognition {
                    seat: bound.seat.clone(),
                    fence: bound.fence,
                    proposal: pending.proposal.clone().unwrap(),
                }),
            )
        })();
        if result.is_ok() {
            self.connections.insert(index, connection);
        }
        match result {
            Ok(command) => Ok(command),
            Err(_) => Ok(None),
        }
    }
    pub fn recognition_command_sent(
        &mut self,
        seat: &str,
        fence: Fence,
        ticket: u64,
    ) -> io::Result<()> {
        let pending = self
            .connections
            .iter_mut()
            .filter_map(|connection| connection.bound.as_mut())
            .find(|bound| bound.seat == seat && bound.fence == fence)
            .and_then(|bound| bound.recognizers.tickets.get_mut(&ticket))
            .filter(|pending| pending.proposal.is_some() && !pending.sent)
            .ok_or_else(|| protocol::invalid("recognizer command identity retired"))?;
        pending.sent = true;
        Ok(())
    }
    pub fn recognition_receipt(
        &mut self,
        seat: &str,
        fence: Fence,
        ticket: u64,
        error: Option<String>,
        now: Duration,
    ) -> io::Result<()> {
        let Some(index) = self.connections.iter().position(|connection| {
            connection
                .bound
                .as_ref()
                .is_some_and(|bound| bound.seat == seat && bound.fence == fence)
        }) else {
            return Ok(());
        };
        let mut connection = self.connections.remove(index);
        let result: io::Result<()> = (|| {
            let bound = connection.bound.as_mut().unwrap();
            bound.registration.check(&self.logind, &bound.client)?;
            self.verify_agent(connection.channel.peer().pid)?;
            if bound.recognizers.overdue(now) {
                self.advance_session(&mut connection, now)?;
                return Ok(());
            }
            if bound.recognizers.receipt(ticket)? {
                connection.push(Reply::RecognitionResult {
                    fence,
                    ticket,
                    error,
                })?;
            }
            Ok(())
        })();
        if result.is_ok() {
            self.connections.insert(index, connection);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
