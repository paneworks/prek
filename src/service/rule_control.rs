//! Broker-owned preparation and receipt correlation for seat-local rule edits.
use super::*;
use crate::{
    configuration::Command,
    lua::{RuleProposal, RuleReceipt},
};
use std::sync::Arc;

const RECEIPT_TIME: Duration = Duration::from_millis(250);
fn eligible(
    role: Role,
    capabilities: &BTreeSet<Capability>,
    observing: bool,
    fresh: bool,
    state: State,
    reloading: bool,
) -> bool {
    !reloading
        && role == Role::Agent
        && capabilities.contains(&Capability::RuntimeRules)
        && observing
        && fresh
        && state == State::Desktop
}
pub(super) struct Pending {
    proposal: RuleProposal,
    config: Arc<crate::config::Config>,
    expires: Duration,
    sent: bool,
}
pub(super) struct Rules {
    config: Option<Arc<crate::config::Config>>,
    revision: u64,
    last_id: u64,
    pending: Option<Pending>,
}
impl Rules {
    pub(super) fn new(config: Option<Arc<crate::config::Config>>) -> Self {
        Self {
            config,
            revision: 1,
            last_id: 0,
            pending: None,
        }
    }
    pub(super) fn busy(&self) -> bool {
        self.pending.is_some()
    }
    fn prepare(
        &mut self,
        proposal: RuleProposal,
        capabilities: &BTreeSet<Capability>,
        handlers: &[crate::lua::HandlerRegistration],
        now: Duration,
    ) -> io::Result<()> {
        if proposal.id == 0 || proposal.id <= self.last_id || self.pending.is_some() {
            return Err(protocol::invalid(
                "duplicate or overlapping runtime rule proposal",
            ));
        }
        self.last_id = proposal.id;
        let current = self
            .config
            .as_ref()
            .filter(|config| config.policy.allow_runtime_rules)
            .ok_or_else(|| protocol::invalid("system policy disables runtime rule edits"))?;
        let next = proposal
            .transaction
            .prepare(current, self.revision)
            .map_err(io::Error::other)?;
        for change in &proposal.transaction.changes {
            let edited = match change {
                crate::rule_transactions::Change::Replace { rule } => Some(rule.as_ref()),
                crate::rule_transactions::Change::Enabled { id, enabled: true } => {
                    next.rules.iter().find(|rule| rule.id == *id)
                }
                _ => None,
            };
            if let Some(rule) = edited {
                if !protocol::action_capabilities(&rule.action).is_subset(capabilities) {
                    return Err(protocol::invalid(
                        "runtime rule action exceeds negotiated capabilities",
                    ));
                }
                if let crate::config::Action::Lua { handler, requires } = &rule.action
                    && !handlers
                        .iter()
                        .any(|approved| approved.name == *handler && approved.requires == *requires)
                {
                    return Err(protocol::invalid(
                        "runtime rule has no matching approved Lua handler",
                    ));
                }
            }
        }
        self.pending = Some(Pending {
            proposal,
            config: Arc::new(next),
            expires: now + RECEIPT_TIME,
            sent: false,
        });
        Ok(())
    }
    fn receipt(&mut self, receipt: &RuleReceipt, now: Duration) -> io::Result<()> {
        let pending = self
            .pending
            .as_ref()
            .ok_or_else(|| protocol::invalid("unsolicited worker rule receipt"))?;
        if !pending.sent || receipt.id != pending.proposal.id || now >= pending.expires {
            return Err(protocol::invalid(
                "stale worker rule receipt identity/deadline",
            ));
        }
        let expected = self
            .revision
            .checked_add(1)
            .ok_or_else(|| protocol::invalid("runtime rule revision exhausted"))?;
        if receipt.error.is_none() && receipt.revision != expected
            || receipt.error.is_some() && receipt.revision != self.revision
        {
            return Err(protocol::invalid("worker and broker rule revisions differ"));
        }
        let pending = self.pending.take().unwrap();
        if receipt.error.is_none() {
            self.config = Some(pending.config);
            self.revision = expected;
        }
        Ok(())
    }
}

fn retire_rule_actions(
    actions: &mut ActionLedger,
    outgoing: &mut VecDeque<Reply>,
    fence: Fence,
) -> Vec<Reply> {
    let sources = actions
        .pending
        .values()
        .map(|offer| offer.source.clone())
        .collect::<BTreeSet<_>>();
    for source in sources {
        actions.cancel(&source, None);
    }
    outgoing.retain(|reply| !matches!(reply,
            Reply::Execute {..} | Reply::Dispatch {..})
            && !matches!(reply,Reply::Observation {event,..} if matches!(event.as_ref(),crate::rules::Event::Matched {..})));
    actions
        .pending
        .iter()
        .filter(|(_, offer)| offer.cancelled && offer.delivered)
        .map(|((id, ordinal), _)| Reply::ActionCancelled {
            fence,
            id: *id,
            ordinal: *ordinal,
        })
        .collect()
}
impl Connection {
    fn retire_rule_actions(&mut self) -> io::Result<()> {
        let bound = self.bound.as_mut().unwrap();
        let replies = retire_rule_actions(&mut bound.actions, &mut self.outgoing, bound.fence);
        self.bytes = self
            .outgoing
            .iter()
            .map(reply_size)
            .sum::<io::Result<usize>>()?;
        for reply in replies {
            self.push(reply)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
impl Broker {
    pub fn rules_busy(&self) -> bool {
        self.connections
            .iter()
            .filter_map(|connection| connection.bound.as_ref())
            .any(|bound| bound.rules.pending.is_some())
    }
    pub fn rules_wants_write(&self) -> bool {
        self.connections
            .iter()
            .filter_map(|connection| connection.bound.as_ref())
            .any(|bound| {
                bound
                    .rules
                    .pending
                    .as_ref()
                    .is_some_and(|pending| !pending.sent)
            })
    }
    pub(super) fn prepare_rules(
        &mut self,
        connection: &mut Connection,
        incoming: Fence,
        proposal: RuleProposal,
        now: Duration,
    ) -> io::Result<()> {
        self.refresh_context(connection, now)?;
        if !self.guard_observation(connection, now)? {
            return Ok(());
        }
        let bound = connection.bound.as_mut().unwrap();
        if bound.fence != incoming {
            return Ok(());
        }
        if bound.rules.busy() {
            return Err(protocol::invalid("overlapping runtime rule requests"));
        }
        let id = proposal.id;
        let result = if !eligible(
            bound.role,
            &bound.capabilities,
            bound.observing,
            bound.context.fresh(now),
            bound.context.state(now),
            self.reload_pending,
        ) {
            Err(protocol::invalid(
                "runtime rules require an active owning desktop registration and idle configuration",
            ))
        } else {
            bound
                .rules
                .prepare(proposal, &bound.capabilities, &self.rule_handlers, now)
        };
        if let Err(error) = result {
            let receipt = RuleReceipt {
                id,
                revision: bound.rules.revision,
                error: Some(
                    error
                        .to_string()
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(128)
                        .collect(),
                ),
            };
            let fence = bound.fence;
            connection.push(Reply::RulesChanged { fence, receipt })?;
        } else {
            connection.retire_rule_actions()?;
        }
        Ok(())
    }
    /// Keep a prepared command until the nonblocking worker channel accepts it.
    pub fn rule_command(&mut self, now: Duration) -> io::Result<Option<Command>> {
        let mut selected = None;
        for index in 0..self.connections.len() {
            let Some(bound) = self.connections[index].bound.as_ref() else {
                continue;
            };
            let Some(pending) = bound.rules.pending.as_ref() else {
                continue;
            };
            if now >= pending.expires {
                selected = Some((index, true));
                break;
            }
            if !pending.sent {
                selected = Some((index, false));
                break;
            }
        }
        let Some((index, expired)) = selected else {
            return Ok(None);
        };
        let mut connection = self.connections.remove(index);
        let result: io::Result<Option<Command>> = (|| {
            self.refresh_context(&mut connection, now)?;
            if !self.guard_observation(&mut connection, now)? {
                return Ok(None);
            }
            let bound = connection.bound.as_mut().unwrap();
            let checked = monotonic()?;
            if expired
                || !bound.context.fresh(checked)
                || bound.context.state(checked) != State::Desktop
                || bound
                    .rules
                    .pending
                    .as_ref()
                    .is_some_and(|pending| checked >= pending.expires)
            {
                self.advance_session(&mut connection, now)?;
                return Ok(None);
            }
            bound.registration.check(&self.logind, &bound.client)?;
            self.verify_agent(connection.channel.peer().pid)?;
            let Some(pending) = bound.rules.pending.as_ref() else {
                return Ok(None);
            };
            Ok(Some(Command::Rules {
                seat: bound.seat.clone(),
                fence: bound.fence,
                id: pending.proposal.id,
                transaction: pending.proposal.transaction.clone(),
            }))
        })();
        if result.is_ok() {
            self.connections.insert(index, connection);
        }
        match result {
            Ok(command) => Ok(command),
            Err(_) => Ok(None),
        }
    }
    pub fn rule_command_sent(&mut self, seat: &str, fence: Fence, id: u64) -> io::Result<()> {
        let pending = self
            .connections
            .iter_mut()
            .filter_map(|connection| connection.bound.as_mut())
            .find(|bound| bound.seat == seat && bound.fence == fence)
            .and_then(|bound| bound.rules.pending.as_mut())
            .filter(|pending| pending.proposal.id == id && !pending.sent)
            .ok_or_else(|| protocol::invalid("rule command owner/identity retired"))?;
        pending.sent = true;
        Ok(())
    }
    pub fn rule_receipt(
        &mut self,
        seat: &str,
        fence: Fence,
        receipt: RuleReceipt,
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
        let result = (|| {
            self.refresh_context(&mut connection, now)?;
            if !self.guard_observation(&mut connection, now)? {
                return Ok(());
            }
            let bound = connection.bound.as_mut().unwrap();
            if bound.fence != fence {
                return Ok(());
            }
            bound.registration.check(&self.logind, &bound.client)?;
            self.verify_agent(connection.channel.peer().pid)?;
            let checked = monotonic()?;
            if !bound.context.fresh(checked)
                || bound.context.state(checked) != State::Desktop
                || bound.rules.receipt(&receipt, checked).is_err()
            {
                self.advance_session(&mut connection, now)?;
                return Ok(());
            }
            connection.push(Reply::RulesChanged { fence, receipt })
        })();
        if result.is_ok() {
            self.connections.insert(index, connection);
        }
        match result {
            Ok(()) => Ok(()),
            Err(_) => Ok(()),
        }
    }
}
