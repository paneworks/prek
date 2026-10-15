//! Generation-local rule handles. The host, rather than Lua, owns proposal
//! identities, revisions and activation receipts.
use super::*;
use crate::rule_transactions::{Change, Transaction};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

const MAX_BUILDERS: usize = 64;
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuleProposal {
    pub id: u64,
    pub transaction: Transaction,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuleReceipt {
    pub id: u64,
    pub revision: u64,
    pub error: Option<String>,
}
#[derive(Default)]
pub(super) struct State {
    config: Option<Config>,
    revision: u64,
    enabled: bool,
    handlers: Vec<HandlerRegistration>,
    next_id: u64,
    builders: Vec<Weak<RefCell<Builder>>>,
    pending: Option<(RuleProposal, Rc<RefCell<Builder>>)>,
    outgoing: Option<RuleProposal>,
}
struct Builder {
    transaction: Transaction,
    state: &'static str,
    revision: Option<String>,
    error: Option<String>,
}
impl State {
    pub(super) fn initialize(&mut self, config: Config, handlers: Vec<HandlerRegistration>) {
        self.config = Some(config);
        self.handlers = handlers;
        self.revision = 1;
    }
    pub(super) fn discard(&mut self, reason: &str) {
        self.outgoing = None;
        if let Some((_, builder)) = self.pending.take() {
            let mut builder = builder.borrow_mut();
            builder.state = "rejected";
            builder.error = Some(reason.into());
            builder.transaction.changes.clear();
        }
    }
    pub(super) fn pending_id(&self) -> Option<u64> {
        self.pending.as_ref().map(|(proposal, _)| proposal.id)
    }
    fn validate_handlers(&self, config: &Config) -> io::Result<()> {
        for rule in &config.rules {
            if let crate::config::Action::Lua { handler, requires } = &rule.action
                && !self
                    .handlers
                    .iter()
                    .any(|approved| approved.name == *handler && approved.requires == *requires)
            {
                return Err(io::Error::other(
                    "runtime rule has no matching approved Lua handler",
                ));
            }
        }
        Ok(())
    }
}
fn phase(host: &runtime::Host) -> io::Result<()> {
    if host.configuring || !host.running {
        Err(io::Error::other(
            "rule queries and transactions require a runtime callback",
        ))
    } else {
        Ok(())
    }
}
pub(super) fn register<'gc>(
    ctx: &luna::Context<'gc>,
    rules: Table<'gc>,
    host: Rc<RefCell<runtime::Host>>,
) -> Result<(), luna::Error<'gc>> {
    for field in ["list", "get", "revision"] {
        let host = host.clone();
        rules.set(
            *ctx,
            field,
            Callback::from_fn(ctx, move |ctx, _, mut stack| {
                let host = host.borrow();
                phase(&host)?;
                let state = &host.rules;
                let config = state
                    .config
                    .as_ref()
                    .ok_or_else(|| io::Error::other("runtime rule view unavailable"))?;
                let value = match field {
                    "list" => {
                        luna_util::serde::to_value(ctx, &config.rules).map_err(io::Error::other)?
                    }
                    "get" => {
                        let id: luna::String = stack.consume(ctx)?;
                        let id = id.to_str().map_err(io::Error::other)?;
                        config
                            .rules
                            .iter()
                            .find(|rule| rule.id == id)
                            .map(|rule| luna_util::serde::to_value(ctx, rule))
                            .transpose()
                            .map_err(io::Error::other)?
                            .unwrap_or(Value::Nil)
                    }
                    _ => Value::String(ctx.intern(state.revision.to_string().as_bytes())),
                };
                stack.replace(ctx, value);
                Ok(CallbackReturn::Return)
            }),
        )?;
    }
    rules.set(*ctx, "transaction", Callback::from_fn(ctx, move |ctx, _, mut stack| {
        let mut state = host.borrow_mut();
        phase(&state)?;
        if !state.rules.enabled {
            return Err(io::Error::other("runtime rule mutation is unavailable for this session").into());
        }
        state.rules.builders.retain(|builder| builder.upgrade().is_some_and(|builder| builder.borrow().state == "open"));
        if state.rules.builders.len() >= MAX_BUILDERS {
            return Err(io::Error::other("runtime rule builder limit exceeded").into());
        }
        let builder = Rc::new(RefCell::new(Builder {
            transaction: Transaction { revision: state.rules.revision, changes: Vec::new() },
            state: "open", revision: None, error: None,
        }));
        state.rules.builders.push(Rc::downgrade(&builder));
        drop(state);
        let handle = Table::new(&ctx);
        for operation in ["enable", "disable", "replace", "remove", "commit", "cancel", "status"] {
            let host = host.clone();
            let builder = builder.clone();
            handle.set(ctx, operation, Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let mut host = host.borrow_mut();
                phase(&host)?;
                let retained = builder.clone();
                let mut builder = builder.borrow_mut();
                if !host.rules.enabled && !matches!(operation, "status" | "cancel") {
                    return Err(io::Error::other("runtime rule mutation transport retired").into());
                }
                match operation {
                    "status" => {
                        let value = serde_json::json!({"state":builder.state,"revision":builder.revision,"error":builder.error});
                        stack.replace(ctx, luna_util::serde::to_value(ctx, &value).map_err(io::Error::other)?);
                    }
                    "cancel" => {
                        if builder.state == "open" { builder.state = "cancelled"; builder.transaction.changes.clear(); }
                        stack.replace(ctx, ());
                    }
                    "commit" => {
                        if builder.state != "open" || host.rules.pending.is_some() {
                            return Err(io::Error::other("rule transaction closed or another proposal is pending").into());
                        }
                        let current = host.rules.config.as_ref().ok_or_else(|| io::Error::other("runtime rule view unavailable"))?;
                        let next = builder.transaction.prepare(current, host.rules.revision).map_err(io::Error::other)?;
                        host.rules.validate_handlers(&next)?;
                        let id = host.rules.next_id.checked_add(1).ok_or_else(|| io::Error::other("rule proposal identity exhausted"))?;
                        host.rules.next_id = id;
                        let proposal = RuleProposal { id, transaction: builder.transaction.clone() };
                        host.rules.outgoing = Some(proposal.clone());
                        host.rules.pending = Some((proposal, retained));
                        builder.state = "pending";
                        stack.replace(ctx, ());
                    }
                    _ => {
                        if builder.state != "open" { return Err(io::Error::other("rule transaction is closed").into()); }
                        let change = if operation == "replace" {
                            let (_, value): (Value, Value) = stack.consume(ctx)?;
                            let rule = luna_util::serde::from_value_with(ctx, value, luna_util::serde::DeOptions::default().max_depth(64)).map_err(io::Error::other)?;
                            Change::Replace { rule: Box::new(rule) }
                        } else {
                            let (_, id): (Value, luna::String) = stack.consume(ctx)?;
                            let id = id.to_str().map_err(io::Error::other)?.to_owned();
                            if operation == "remove" { Change::Remove { id } } else { Change::Enabled { id, enabled: operation == "enable" } }
                        };
                        let mut next = builder.transaction.clone();
                        next.changes.push(change);
                        next.validate().map_err(io::Error::other)?;
                        builder.transaction = next;
                        stack.replace(ctx, ());
                    }
                }
                Ok(CallbackReturn::Return)
            }))?;
        }
        stack.replace(ctx, handle);
        Ok(CallbackReturn::Return)
    }))?;
    Ok(())
}
impl Runtime {
    pub(super) fn rule_proposal_checkpoint(&self) -> Option<u64> {
        self.host.borrow().rules.pending_id()
    }
    pub(super) fn discard_rule_proposal_since(&mut self, checkpoint: Option<u64>, reason: &str) {
        let mut host = self.host.borrow_mut();
        if host.rules.pending_id() != checkpoint {
            host.rules.discard(reason);
        }
    }
    /// Only an embedding with a live, authorized mutation transport enables
    /// submission. Merely loading a Lua source grants no mutation authority.
    pub fn enable_rule_transactions(&mut self) {
        self.host.borrow_mut().rules.enabled = true;
    }
    pub fn disable_rule_transactions(&mut self) {
        let mut host = self.host.borrow_mut();
        host.rules.enabled = false;
        host.rules.discard("rule mutation transport retired");
    }
    pub fn take_rule_proposal(&mut self) -> Option<RuleProposal> {
        self.host.borrow_mut().rules.outgoing.take()
    }
    pub fn acknowledge_rules(&mut self, receipt: RuleReceipt) -> Result<(), ConfigError> {
        let mut host = self.host.borrow_mut();
        if receipt.id == 0
            || receipt
                .error
                .as_ref()
                .is_some_and(|error| error.len() > 512 || error.chars().any(char::is_control))
        {
            return Err(error("invalid runtime rule receipt"));
        }
        let (proposal, builder) = host
            .rules
            .pending
            .as_ref()
            .ok_or_else(|| error("unsolicited runtime rule receipt"))?;
        if proposal.id != receipt.id || host.rules.outgoing.is_some() {
            return Err(error("runtime rule receipt identity/order mismatch"));
        }
        let config = if receipt.error.is_none() {
            if receipt.revision
                != proposal
                    .transaction
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| error("runtime rule revision exhausted"))?
            {
                return Err(error("runtime rule receipt revision mismatch"));
            }
            Some(
                proposal
                    .transaction
                    .prepare(host.rules.config.as_ref().unwrap(), host.rules.revision)?,
            )
        } else {
            None
        };
        {
            let mut builder = builder.borrow_mut();
            builder.state = if config.is_some() {
                "accepted"
            } else {
                "rejected"
            };
            builder.revision = Some(receipt.revision.to_string());
            builder.error = receipt.error;
            builder.transaction.changes.clear();
        }
        if let Some(config) = config {
            host.rules.config = Some(config);
            host.rules.revision = receipt.revision;
        } else if receipt.revision != host.rules.revision {
            host.rules.config = None;
        }
        host.rules.pending = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
