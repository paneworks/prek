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
#[cfg(test)]
mod inspection_tests;
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
    recognizers: Vec<crate::recognition::Definition>,
    next_id: u64,
    builders: Vec<Weak<RefCell<Builder>>>,
    /// A proposal without a builder was submitted by the host for user shortcuts.
    pending: Option<(RuleProposal, Option<Rc<RefCell<Builder>>>)>,
    outgoing: Option<RuleProposal>,
    baseline: Option<Config>,
    shortcuts: Shortcuts,
    shortcuts_next: Option<Shortcuts>,
    shortcut_outcome: Option<super::LogRecord>,
}
/// Rule IDs that the applied user shortcut file replaced, added or disabled.
#[derive(Clone, Default)]
struct Shortcuts {
    rules: std::collections::BTreeSet<String>,
    disabled: std::collections::BTreeSet<String>,
}
struct Builder {
    transaction: Transaction,
    state: &'static str,
    revision: Option<String>,
    error: Option<String>,
}
impl State {
    pub(super) fn initialize(&mut self, config: Config, handlers: Vec<HandlerRegistration>) {
        self.recognizers = config.recognizer_definitions();
        self.baseline = Some(config.clone());
        self.config = Some(config);
        self.handlers = handlers;
        self.revision = 1;
        self.shortcuts = Shortcuts::default();
    }
    pub(super) fn discard(&mut self, reason: &str) {
        self.outgoing = None;
        match self.pending.take() {
            Some((_, Some(builder))) => {
                let mut builder = builder.borrow_mut();
                builder.state = "rejected";
                builder.error = Some(reason.into());
                builder.transaction.changes.clear();
            }
            Some((_, None)) => {
                self.shortcuts_next = None;
                self.shortcut_outcome = Some(shortcut_log("shortcuts rejected", reason));
            }
            None => {}
        }
    }
    pub(super) fn pending_id(&self) -> Option<u64> {
        self.pending.as_ref().map(|(proposal, _)| proposal.id)
    }
    fn validate_handlers(&self, config: &Config) -> io::Result<()> {
        if config
            .recognizer_definitions()
            .iter()
            .any(|definition| !self.recognizers.contains(definition))
        {
            return Err(io::Error::other(
                "runtime rules cannot add unapproved custom recognizer declarations",
            ));
        }
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
pub(super) fn register_inspection<'gc>(
    ctx: &luna::Context<'gc>,
    module: Table<'gc>,
    host: Rc<RefCell<runtime::Host>>,
) -> Result<(), luna::Error<'gc>> {
    for namespace in ["config", "policy"] {
        let host = host.clone();
        let api: Table = module.get(*ctx, namespace)?;
        api.set(
            *ctx,
            "current",
            Callback::from_fn(ctx, move |ctx, _, mut stack| {
                let host = host.borrow();
                phase(&host)?;
                let Some(config) = &host.rules.config else {
                    stack.replace(ctx, Value::Nil);
                    return Ok(CallbackReturn::Return);
                };
                let view = Table::new(&ctx);
                view.set(
                    ctx,
                    "revision",
                    ctx.intern(host.rules.revision.to_string().as_bytes()),
                )?;
                let value = if namespace == "config" {
                    luna_util::serde::to_value(ctx, config).map_err(io::Error::other)?
                } else {
                    luna_util::serde::to_value(ctx, &config.policy).map_err(io::Error::other)?
                };
                view.set(ctx, namespace, value)?;
                stack.replace(ctx, view);
                Ok(CallbackReturn::Return)
            }),
        )?;
    }
    Ok(())
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
        for operation in ["enable", "disable", "replace", "remove", "activate_layer", "deactivate_layer", "reset_layer", "select_mode", "reset_mode", "commit", "cancel", "status"] {
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
                        host.rules.pending = Some((proposal, Some(retained)));
                        builder.state = "pending";
                        stack.replace(ctx, ());
                    }
                    _ => {
                        if builder.state != "open" { return Err(io::Error::other("rule transaction is closed").into()); }
                        let change = if operation == "reset_mode" {
                            Change::Mode { id: None }
                        } else if operation == "replace" {
                            let (_, value): (Value, Value) = stack.consume(ctx)?;
                            let rule = values::decode(ctx, value, 64, crate::rule_transactions::MAX_BYTES, 16 * crate::rule_transactions::MAX_BYTES)?;
                            Change::Replace { rule: Box::new(rule) }
                        } else {
                            let (_, id): (Value, luna::String) = stack.consume(ctx)?;
                            let id = id.to_str().map_err(io::Error::other)?;
                            if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
                                return Err(io::Error::other("invalid runtime rule identity").into());
                            }
                            let id = id.to_owned();
                            match operation {
                                "remove" => Change::Remove { id },
                                "select_mode" => Change::Mode { id: Some(id) },
                                "activate_layer" | "deactivate_layer" | "reset_layer" => Change::Layer {
                                    id, activation: match operation {
                                        "activate_layer" => crate::config::LayerActivation::Active,
                                        "deactivate_layer" => crate::config::LayerActivation::Inactive,
                                        _ => crate::config::LayerActivation::Physical,
                                    },
                                },
                                _ => Change::Enabled { id, enabled: operation == "enable" },
                            }
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
pub(super) fn register_layers<'gc>(
    ctx: &luna::Context<'gc>,
    layers: Table<'gc>,
    rules: Table<'gc>,
    host: Rc<RefCell<runtime::Host>>,
) -> Result<(), luna::Error<'gc>> {
    // One revision and one pending proposal cover mixed rule/layer batches.
    layers.set(
        *ctx,
        "transaction",
        rules.get::<_, Value>(*ctx, "transaction")?,
    )?;
    layers.set(*ctx, "revision", rules.get::<_, Value>(*ctx, "revision")?)?;
    for field in ["list", "get"] {
        let host = host.clone();
        layers.set(
            *ctx,
            field,
            Callback::from_fn(ctx, move |ctx, _, mut stack| {
                let host = host.borrow();
                phase(&host)?;
                let config = host
                    .rules
                    .config
                    .as_ref()
                    .ok_or_else(|| io::Error::other("runtime layer view unavailable"))?;
                let value = if field == "list" {
                    luna_util::serde::to_value(ctx, &config.layers).map_err(io::Error::other)?
                } else {
                    let id: luna::String = stack.consume(ctx)?;
                    let id = id.to_str().map_err(io::Error::other)?;
                    config
                        .layers
                        .iter()
                        .find(|layer| layer.id == id)
                        .map(|layer| luna_util::serde::to_value(ctx, layer))
                        .transpose()
                        .map_err(io::Error::other)?
                        .unwrap_or(Value::Nil)
                };
                stack.replace(ctx, value);
                Ok(CallbackReturn::Return)
            }),
        )?;
    }
    Ok(())
}

pub(super) fn register_modes<'gc>(
    ctx: &luna::Context<'gc>,
    modes: Table<'gc>,
    rules: Table<'gc>,
    host: Rc<RefCell<runtime::Host>>,
) -> Result<(), luna::Error<'gc>> {
    for field in ["transaction", "revision"] {
        modes.set(*ctx, field, rules.get::<_, Value>(*ctx, field)?)?;
    }
    for field in ["list", "get", "current"] {
        let host = host.clone();
        modes.set(
            *ctx,
            field,
            Callback::from_fn(ctx, move |ctx, _, mut stack| {
                let host = host.borrow();
                phase(&host)?;
                let config = host
                    .rules
                    .config
                    .as_ref()
                    .ok_or_else(|| io::Error::other("runtime mode view unavailable"))?;
                let value = if field == "list" {
                    luna_util::serde::to_value(ctx, &config.modes).map_err(io::Error::other)?
                } else {
                    let selected = if field == "current" {
                        config.modes.iter().find(|mode| mode.active)
                    } else {
                        let id: luna::String = stack.consume(ctx)?;
                        let id = id.to_str().map_err(io::Error::other)?;
                        config.modes.iter().find(|mode| mode.id == id)
                    };
                    selected
                        .map(|mode| luna_util::serde::to_value(ctx, mode))
                        .transpose()
                        .map_err(io::Error::other)?
                        .unwrap_or(Value::Nil)
                };
                stack.replace(ctx, value);
                Ok(CallbackReturn::Return)
            }),
        )?;
    }
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
        let submitted_by_host = builder.is_none();
        if let Some(builder) = builder {
            let mut builder = builder.borrow_mut();
            builder.state = if config.is_some() {
                "accepted"
            } else {
                "rejected"
            };
            builder.revision = Some(receipt.revision.to_string());
            builder.error = receipt.error.clone();
            builder.transaction.changes.clear();
        }
        let accepted = config.is_some();
        if let Some(config) = config {
            host.rules.config = Some(config);
            host.rules.revision = receipt.revision;
        } else if receipt.revision != host.rules.revision {
            host.rules.config = None;
        }
        host.rules.pending = None;
        if submitted_by_host {
            let next = host.rules.shortcuts_next.take().unwrap_or_default();
            host.rules.shortcut_outcome = Some(if accepted {
                let outcome = super::LogRecord {
                    message: "shortcuts applied".into(),
                    fields: serde_json::json!({"rules": next.rules.len(), "disabled": next.disabled.len()}),
                };
                host.rules.shortcuts = next;
                outcome
            } else {
                shortcut_log(
                    "shortcuts rejected",
                    receipt.error.as_deref().unwrap_or("rejected"),
                )
            });
        }
        Ok(())
    }
    /// Layer user shortcuts over the current rules as one host transaction:
    /// shortcuts dropped since the last file return to their system rule,
    /// and IDs no longer disabled regain their system state. Returns false
    /// when nothing changed.
    pub fn submit_shortcuts(
        &mut self,
        overlay: crate::config::Overlay,
    ) -> Result<bool, ConfigError> {
        let mut host = self.host.borrow_mut();
        let state = &mut host.rules;
        if !state.enabled {
            return Err(error("user shortcuts require runtime rule transactions"));
        }
        if state.pending.is_some() {
            return Err(error("another runtime rule proposal is pending"));
        }
        let current = state
            .config
            .as_ref()
            .ok_or_else(|| error("runtime rule view unavailable"))?;
        let baseline = state.baseline.as_ref().unwrap_or(current);
        let mut next = Shortcuts::default();
        for rule in &overlay.rules {
            if !next.rules.insert(rule.id.clone()) {
                return Err(error(format!("duplicate shortcut {}", rule.id)));
            }
        }
        next.disabled = overlay.disabled_rules.iter().cloned().collect();
        let system = |id: &str| baseline.rules.iter().find(|rule| rule.id == id);
        let mut changes = Vec::new();
        for id in state.shortcuts.rules.difference(&next.rules) {
            changes.push(match system(id) {
                Some(original) => Change::Replace {
                    rule: Box::new(original.clone()),
                },
                None => Change::Remove { id: id.clone() },
            });
        }
        for rule in &overlay.rules {
            if current.rules.iter().find(|old| old.id == rule.id) != Some(rule) {
                changes.push(Change::Replace {
                    rule: Box::new(rule.clone()),
                });
            }
        }
        for id in state.shortcuts.disabled.difference(&next.disabled) {
            if let Some(original) = system(id).filter(|_| !next.rules.contains(id)) {
                changes.push(Change::Enabled {
                    id: id.clone(),
                    enabled: original.enabled,
                });
            }
        }
        for id in &next.disabled {
            let enabled = current
                .rules
                .iter()
                .find(|rule| rule.id == *id)
                .is_none_or(|rule| rule.enabled);
            if enabled || next.rules.contains(id) {
                changes.push(Change::Enabled {
                    id: id.clone(),
                    enabled: false,
                });
            }
        }
        if changes.is_empty() {
            state.shortcuts = next;
            state.shortcut_outcome = Some(shortcut_log("shortcuts unchanged", ""));
            return Ok(false);
        }
        if changes.len() > crate::rule_transactions::MAX_OPERATIONS {
            return Err(error(format!(
                "shortcut changes exceed {} rule operations",
                crate::rule_transactions::MAX_OPERATIONS
            )));
        }
        let transaction = crate::rule_transactions::Transaction {
            revision: state.revision,
            changes,
        };
        let prepared = transaction.prepare(current, state.revision)?;
        state.validate_handlers(&prepared).map_err(error)?;
        let id = state
            .next_id
            .checked_add(1)
            .ok_or_else(|| error("rule proposal identity exhausted"))?;
        state.next_id = id;
        let proposal = RuleProposal { id, transaction };
        state.outgoing = Some(proposal.clone());
        state.pending = Some((proposal, None));
        state.shortcuts_next = Some(next);
        Ok(true)
    }
    /// The result of the last shortcut submission, once.
    pub fn take_shortcut_outcome(&mut self) -> Option<super::LogRecord> {
        self.host.borrow_mut().rules.shortcut_outcome.take()
    }
}

fn shortcut_log(message: &str, detail: &str) -> super::LogRecord {
    super::LogRecord {
        message: message.into(),
        fields: if detail.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::json!({ "detail": detail })
        },
    }
}

#[cfg(test)]
mod tests;
